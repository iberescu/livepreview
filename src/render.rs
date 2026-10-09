//! Replace a smart object's contents and render the document to a web image.
//!
//! ```text
//! upload ─decode (EXIF-oriented)─► fit to the smart object's contents size (cover/contain/fill)
//!        ─► surface in the document's pixel format ─► through the smart object's own transform,
//!           warp/perspective and smart filters (PhotoCraft's engine) ─► the layer's new pixels
//! template.clone() with those pixels ─composite (PhotoCraft, multithreaded)─► sRGB RGBA8
//!        ─► optional downscale ─► JPEG / WebP / PNG
//! ```
//!
//! The template is never modified: each request works on a copy-on-write clone of the parsed
//! document, so any number of renders of the same template run side by side.

use std::collections::HashMap;
use std::time::Instant;

use fast_image_resize as fr;
use photocraft_algo::transform::Homography;
use photocraft_algo::warp::{place_source, place_source_projective};
use photocraft_cms::Transform;
use photocraft_color::PixelFormat;
use photocraft_doc::LayerContent;
use photocraft_geom::Rect;
use photocraft_raster::Surface;
use rayon::prelude::*;

use crate::error::ApiError;
use crate::templates::{SmartInfo, Template};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Jpeg,
    Webp,
    Png,
}

impl Format {
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Jpeg => "image/jpeg",
            Format::Webp => "image/webp",
            Format::Png => "image/png",
        }
    }
}

/// How the uploaded image fills the smart object's contents area.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Scale to cover the whole area, cropping the overflow (centred).
    Cover,
    /// Scale to fit inside the area, leaving the rest transparent (the default: nothing of the
    /// design is lost).
    Contain,
    /// Stretch to the area exactly (aspect ratio not kept).
    Fill,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub format: Format,
    pub quality: u8,
    pub lossless: bool,
    pub max_width: Option<u32>,
    pub max_height: Option<u32>,
    pub fit: Fit,
    /// Flatten transparency onto this colour (always done for JPEG, white by default).
    pub background: Option<[f32; 3]>,
    /// Output size as a fraction of the document (Save for Web's percent); overrides the caps.
    pub scale: Option<f64>,
    /// Fill the transparent parts of the fitted design (its own transparency, and the bands a
    /// `contain` fit leaves) with this colour, so the whole smart object area is covered.
    pub design_background: Option<[f32; 3]>,
    /// Also replace the other instances sharing the smart object's contents.
    pub instances: bool,
}

impl Options {
    /// Reads options from request parameters (query string and multipart text fields).
    pub fn from_params(p: &HashMap<String, String>) -> Result<Self, ApiError> {
        let get = |k: &str| p.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let num = |k: &str| -> Result<Option<u32>, ApiError> {
            get(k).map(|v| v.parse::<u32>().map_err(|_| ApiError::bad_request(format!("`{k}` must be a positive integer")))).transpose()
        };
        let flag = |k: &str, default: bool| -> Result<bool, ApiError> {
            match get(k).map(|v| v.to_ascii_lowercase()) {
                None => Ok(default),
                Some(v) if matches!(v.as_str(), "1" | "true" | "yes" | "on") => Ok(true),
                Some(v) if matches!(v.as_str(), "0" | "false" | "no" | "off") => Ok(false),
                Some(_) => Err(ApiError::bad_request(format!("`{k}` must be true or false"))),
            }
        };
        let format = match get("format").map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("webp") => Format::Webp,
            Some("jpeg" | "jpg") => Format::Jpeg,
            Some("png") => Format::Png,
            Some(f) => return Err(ApiError::bad_request(format!("unknown format \"{f}\" (use webp, jpeg or png)"))),
        };
        let fit = match get("fit").map(|v| v.to_ascii_lowercase()).as_deref() {
            None | Some("contain") => Fit::Contain,
            Some("cover") => Fit::Cover,
            Some("fill" | "stretch") => Fit::Fill,
            Some(f) => return Err(ApiError::bad_request(format!("unknown fit \"{f}\" (use cover, contain or fill)"))),
        };
        let quality = num("quality")?.unwrap_or(85);
        if !(1..=100).contains(&quality) {
            return Err(ApiError::bad_request("`quality` must be 1..100"));
        }
        let background = get("background").map(parse_hex).transpose()?;
        let scale = match get("scale") {
            None => None,
            Some(v) => {
                let v: f64 = v.trim_end_matches('%').trim().parse().map_err(|_| ApiError::bad_request("`scale` must be a number: a percentage like 50, or a fraction like 0.5"))?;
                let k = if v > 1.0 { v / 100.0 } else { v };
                if !(k > 0.0 && k <= 1.0) {
                    return Err(ApiError::bad_request("`scale` must be between 1 and 100 percent"));
                }
                Some(k)
            }
        };
        let design_background = get("design_background").map(parse_hex).transpose()?;
        let nonzero = |v: Option<u32>, k: &str| match v {
            Some(0) => Err(ApiError::bad_request(format!("`{k}` must be at least 1"))),
            v => Ok(v),
        };
        Ok(Options {
            format,
            quality: quality as u8,
            lossless: flag("lossless", false)?,
            max_width: nonzero(num("width")?, "width")?,
            max_height: nonzero(num("height")?, "height")?,
            fit,
            background,
            scale,
            design_background,
            instances: flag("instances", true)?,
        })
    }
}

fn parse_hex(s: &str) -> Result<[f32; 3], ApiError> {
    let h = s.trim_start_matches('#');
    let bad = || ApiError::bad_request("`background` must be a hex colour like #ffffff");
    let v = match h.len() {
        6 => u32::from_str_radix(h, 16).map_err(|_| bad())?,
        3 => {
            let v = u32::from_str_radix(h, 16).map_err(|_| bad())?;
            let (r, g, b) = ((v >> 8) & 15, (v >> 4) & 15, v & 15);
            (r * 17) << 16 | (g * 17) << 8 | (b * 17)
        }
        _ => return Err(bad()),
    };
    Ok([((v >> 16) & 255) as f32 / 255.0, ((v >> 8) & 255) as f32 / 255.0, (v & 255) as f32 / 255.0])
}

pub struct Output {
    pub bytes: Vec<u8>,
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub replaced: usize,
    pub timings: Vec<(&'static str, u64)>,
    /// The fraction of the document's size the composite was rendered at (1, 1/2, 1/4, 1/8).
    pub render_scale: f64,
}

pub struct Rgba8 {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// Decodes an upload to straight-alpha sRGB RGBA8 with PhotoCraft's decoders: any format the
/// editor opens (PNG, JPEG, WebP, TIFF, GIF, BMP, HEIC, AVIF, EXR, SVG, PSD/PSB, camera raw…),
/// turned upright by its EXIF orientation, 8/16/32-bit, and colour-managed: an embedded RGB or
/// gray profile is converted to sRGB; CMYK and Lab go through their profile.
fn decode_upload(name: &str, bytes: &[u8]) -> Result<Rgba8, ApiError> {
    use photocraft_cms::{Builtin, ColorSpace, Intent, Profile, Transform};
    use photocraft_color::ColorMode;
    let bad = |e: &dyn std::fmt::Display| ApiError::unprocessable(format!("can't decode the uploaded image: {e}"));
    let doc = photocraft_io::import(name, bytes).map_err(|e| bad(&e))?.document;
    let (w, h) = (doc.size.width, doc.size.height);
    if w == 0 || h == 0 {
        return Err(ApiError::unprocessable("the uploaded image is empty"));
    }
    let srgb = Builtin::Srgb.profile();
    let profile = doc.icc_profile.as_ref().and_then(|b| Profile::parse(b).ok());
    // The compositor leaves RGB and gray values in the file's own profile (gray replicated to
    // RGB), so those are converted here; it already renders CMYK and Lab in sRGB.
    let to_srgb = match (doc.mode, profile) {
        (ColorMode::Rgb, Some(p)) if p.color_space == ColorSpace::Rgb && !p.same_colors(srgb) => Transform::new(&p, srgb, Intent::RelativeColorimetric, true).ok(),
        (ColorMode::Grayscale | ColorMode::Duotone | ColorMode::Bitmap, Some(p)) if p.color_space == ColorSpace::Gray => Transform::new(&p, srgb, Intent::RelativeColorimetric, true).ok(),
        _ => None,
    };
    let mut out: Vec<u8> = Vec::with_capacity(w as usize * h as usize * 4);
    let _ = photocraft_compose::render_bands(&doc, doc.bounds(), 0, |mut band| -> Result<(), ()> {
        let px = band.px.as_flattened_mut();
        if let Some(t) = &to_srgb {
            // In place over the 4-float pixels: an RGB transform reads and writes channels 0..3;
            // a gray one reads channel 0 (the replicated gray) and writes RGB over 0..3.
            px.par_chunks_mut(4 * 4096).for_each(|c| t.apply(c, 4));
        }
        let start = out.len();
        out.resize(start + px.len(), 0);
        out[start..].par_chunks_mut(4 * 4096).zip(px.par_chunks(4 * 4096)).for_each(|(o, p)| {
            for (o, p) in o.iter_mut().zip(p) {
                *o = (p.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
            }
        });
        Ok(())
    });
    Ok(Rgba8 { w, h, px: out })
}

/// Fills the transparent parts of `img` with `bg` (straight alpha), making it opaque.
fn fill_transparent(img: &mut Rgba8, bg: [f32; 3]) {
    img.px.par_chunks_mut(4 * 4096).for_each(|c| {
        for p in c.chunks_exact_mut(4) {
            let a = p[3] as f32 / 255.0;
            if a < 1.0 {
                for i in 0..3 {
                    p[i] = (p[i] as f32 * a + bg[i] * 255.0 * (1.0 - a) + 0.5) as u8;
                }
                p[3] = 255;
            }
        }
    });
}

fn resize_err(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(format!("resize failed: {e}"))
}

fn resize(src: &Rgba8, w: u32, h: u32, crop_to_fill: bool) -> Result<Rgba8, ApiError> {
    if src.w == w && src.h == h {
        return Ok(Rgba8 { w, h, px: src.px.clone() });
    }
    let from = fr::images::ImageRef::new(src.w, src.h, &src.px, fr::PixelType::U8x4).map_err(resize_err)?;
    let mut to = fr::images::Image::new(w, h, fr::PixelType::U8x4);
    let mut opts = fr::ResizeOptions::new().resize_alg(fr::ResizeAlg::Convolution(fr::FilterType::Lanczos3));
    if crop_to_fill {
        opts = opts.fit_into_destination(Some((0.5, 0.5)));
    }
    fr::Resizer::new().resize(&from, &mut to, &opts).map_err(resize_err)?;
    Ok(Rgba8 { w, h, px: to.into_vec() })
}

/// The upload fitted to `w`×`h` (the smart object's contents size).
fn fit_upload(src: &Rgba8, w: u32, h: u32, fit: Fit) -> Result<Rgba8, ApiError> {
    match fit {
        Fit::Fill => resize(src, w, h, false),
        Fit::Cover => resize(src, w, h, true),
        Fit::Contain => {
            let k = (w as f64 / src.w as f64).min(h as f64 / src.h as f64);
            let (iw, ih) = (((src.w as f64 * k).round() as u32).clamp(1, w), ((src.h as f64 * k).round() as u32).clamp(1, h));
            let inner = resize(src, iw, ih, false)?;
            let mut px = vec![0u8; w as usize * h as usize * 4];
            let (ox, oy) = ((w - iw) / 2, (h - ih) / 2);
            for y in 0..ih as usize {
                let d = ((oy as usize + y) * w as usize + ox as usize) * 4;
                let s = y * iw as usize * 4;
                px[d..d + iw as usize * 4].copy_from_slice(&inner.px[s..s + iw as usize * 4]);
            }
            Ok(Rgba8 { w, h, px })
        }
    }
}

/// An sRGB RGBA8 image as a surface in the document's pixel format, at the origin (the smart
/// object's source space).
fn to_surface(img: &Rgba8, fmt: PixelFormat, from_srgb: Option<&Transform>) -> Surface {
    const BAND: u32 = 64;
    const BANDS_AT_ONCE: usize = 32;
    let n = fmt.channels();
    let w = img.w as usize;
    let mut s = Surface::new(fmt);
    let starts: Vec<u32> = (0..img.h).step_by(BAND as usize).collect();
    for group in starts.chunks(BANDS_AT_ONCE) {
        let bands: Vec<(Rect, Vec<f32>)> = group
            .par_iter()
            .map(|&y0| {
                let y1 = (y0 + BAND).min(img.h);
                let px = &img.px[y0 as usize * w * 4..y1 as usize * w * 4];
                let mut f: Vec<f32> = px.iter().map(|&v| v as f32 * (1.0 / 255.0)).collect();
                if let Some(t) = from_srgb {
                    t.apply(&mut f, 4);
                }
                let mut out = vec![0.0f32; f.len() / 4 * n];
                for (p, o) in f.chunks_exact(4).zip(out.chunks_exact_mut(n)) {
                    photocraft_raster::from_rgba_into(&fmt, [p[0], p[1], p[2], p[3]], o);
                }
                (Rect::new(0, y0 as i32, img.w as i32, y1 as i32), out)
            })
            .collect();
        for (r, data) in bands {
            s.write_region(r, &data);
        }
    }
    s.prune();
    s
}

/// Composites the document to straight-alpha sRGB RGBA8 (flattened onto `matte` when given).
pub fn composite(doc: &photocraft_doc::Document, to_srgb: Option<&Transform>, matte: Option<[f32; 3]>) -> Rgba8 {
    let (w, h) = (doc.size.width, doc.size.height);
    let mut out: Vec<u8> = Vec::with_capacity(w as usize * h as usize * 4);
    let _ = photocraft_compose::render_bands(doc, doc.bounds(), 0, |mut band| -> Result<(), ()> {
        let px = band.px.as_flattened_mut();
        if let Some(t) = to_srgb {
            px.par_chunks_mut(4 * 4096).for_each(|c| t.apply(c, 4));
        }
        let start = out.len();
        out.resize(start + px.len(), 0);
        out[start..].par_chunks_mut(4 * 4096).zip(px.par_chunks(4 * 4096)).for_each(|(o, p)| {
            for (o, p) in o.chunks_exact_mut(4).zip(p.chunks_exact(4)) {
                let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                let a = p[3].clamp(0.0, 1.0);
                match matte {
                    Some(bg) => {
                        for i in 0..3 {
                            o[i] = q(p[i] * a + bg[i] * (1.0 - a));
                        }
                        o[3] = 255;
                    }
                    None => {
                        for i in 0..3 {
                            o[i] = q(p[i]);
                        }
                        o[3] = q(a);
                    }
                }
            }
        });
        Ok(())
    });
    Rgba8 { w, h, px: out }
}

fn output_size(w: u32, h: u32, mw: Option<u32>, mh: Option<u32>) -> (u32, u32) {
    let k = [mw.map(|m| m as f64 / w as f64), mh.map(|m| m as f64 / h as f64), Some(1.0)].into_iter().flatten().fold(f64::INFINITY, f64::min);
    (((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1))
}

fn encode(img: &Rgba8, o: &Options) -> Result<Vec<u8>, ApiError> {
    let opaque = img.px.par_chunks(4 * 4096).all(|c| c.chunks_exact(4).all(|p| p[3] == 255));
    let rgb = || -> Vec<u8> { img.px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect() };
    match o.format {
        Format::Jpeg => {
            if img.w > u16::MAX as u32 || img.h > u16::MAX as u32 {
                return Err(ApiError::unprocessable("JPEG is limited to 65535 pixels per side; pass width/height or use another format"));
            }
            let mut out = Vec::new();
            let enc = jpeg_encoder::Encoder::new(&mut out, o.quality);
            enc.encode(&rgb(), img.w as u16, img.h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| ApiError::internal(format!("JPEG encode failed: {e}")))?;
            Ok(out)
        }
        Format::Webp => {
            if img.w > 16383 || img.h > 16383 {
                return Err(ApiError::unprocessable("WebP is limited to 16383 pixels per side; pass width/height or use another format"));
            }
            let data = if opaque { rgb() } else { img.px.clone() };
            let enc = if opaque { webp::Encoder::from_rgb(&data, img.w, img.h) } else { webp::Encoder::from_rgba(&data, img.w, img.h) };
            let mut cfg = webp::WebPConfig::new().map_err(|_| ApiError::internal("WebP config failed"))?;
            cfg.lossless = o.lossless as i32;
            cfg.quality = if o.lossless { 25.0 } else { o.quality as f32 };
            // Effort 0 (fast) ..= 6 (small). 4 is libwebp's default; 2 is ~2x faster for ~2% larger files.
            cfg.method = if o.lossless { 1 } else { 2 };
            cfg.thread_level = 1;
            let mem = enc.encode_advanced(&cfg).map_err(|e| ApiError::internal(format!("WebP encode failed: {e:?}")))?;
            Ok(mem.to_vec())
        }
        Format::Png => {
            use image::ImageEncoder;
            use image::codecs::png::{CompressionType, FilterType, PngEncoder};
            let mut out = Vec::new();
            let enc = PngEncoder::new_with_quality(&mut out, CompressionType::Fast, FilterType::Adaptive);
            let r = if opaque {
                enc.write_image(&rgb(), img.w, img.h, image::ExtendedColorType::Rgb8)
            } else {
                enc.write_image(&img.px, img.w, img.h, image::ExtendedColorType::Rgba8)
            };
            r.map_err(|e| ApiError::internal(format!("PNG encode failed: {e}")))?;
            Ok(out)
        }
    }
}

/// `source` (the contents, `w`×`h` at the origin) through the smart object's warp, perspective
/// or transform, into document pixels: everything but the smart filters.
pub fn place(sm: &photocraft_doc::SmartObject, source: &Surface, w: u32, h: u32) -> Surface {
    let area = Rect::new(0, 0, w as i32, h as i32);
    match &sm.perspective {
        Some(p) => place_source_projective(source, area, &Homography(*p), sm.warp.as_ref()),
        None => place_source(source, area, &sm.transform, sm.warp.as_ref()),
    }
}

/// Which smart object to replace.
pub enum Target {
    Name(String),
    /// Photoshop layer id, as recorded in actions and droplets.
    LayerId(u32),
}

/// The output size for a request: `scale` (a fraction of the document) wins over the
/// `width`/`height` caps; never larger than the document.
fn requested_size(doc_w: u32, doc_h: u32, o: &Options) -> (u32, u32) {
    match o.scale {
        Some(k) => (((f64::from(doc_w) * k).round() as u32).max(1), ((f64::from(doc_h) * k).round() as u32).max(1)),
        None => output_size(doc_w, doc_h, o.max_width, o.max_height),
    }
}

/// The pyramid level (0 = full size, n = 1/2ⁿ) to composite at for an `ow`×`oh` output: the
/// smallest copy of the document that is still at least as large as the output.
fn level_for(doc_w: u32, doc_h: u32, ow: u32, oh: u32) -> u8 {
    let mut level = 0u8;
    while level < crate::scale::MAX_LEVEL {
        let f = 0.5f64.powi(i32::from(level) + 1);
        if (f64::from(doc_w) * f).round() >= f64::from(ow) && (f64::from(doc_h) * f).round() >= f64::from(oh) {
            level += 1;
        } else {
            break;
        }
    }
    level
}

/// Renders the template unchanged (PhotoCraft's composite), for comparison with Photoshop's.
pub fn render_unchanged(tpl: &Template, o: &Options) -> Result<Output, ApiError> {
    let t = Instant::now();
    let (ow, oh) = requested_size(tpl.doc.size.width, tpl.doc.size.height, o);
    let level = level_for(tpl.doc.size.width, tpl.doc.size.height, ow, oh);
    let doc = tpl.document_at(level);
    let matte = o.background.or((o.format == Format::Jpeg).then_some([1.0, 1.0, 1.0]));
    let full = composite(&doc, tpl.to_srgb.as_ref(), matte);
    finish(full, (ow, oh), o, vec![("composite", t.elapsed().as_millis() as u64)], 0, level)
}

/// Photoshop's own flattened image stored in the PSD ("Maximize Compatibility").
pub fn photoshop_composite(tpl: &Template, o: &Options) -> Result<Output, ApiError> {
    let t = Instant::now();
    let bytes = std::fs::read(&tpl.path).map_err(|e| ApiError::internal(format!("can't read the PSD: {e}")))?;
    let file = photocraft_psd::PsdFile::from_bytes(&bytes).map_err(|e| ApiError::unprocessable(format!("can't parse the PSD: {e}")))?;
    let img = file.composite_rgba8().map_err(|e| ApiError::unprocessable(format!("the PSD has no usable composite image: {e}")))?;
    let mut px = img.data;
    if let Some(bg) = o.background.or((o.format == Format::Jpeg).then_some([1.0, 1.0, 1.0])) {
        for p in px.chunks_exact_mut(4) {
            let a = p[3] as f32 / 255.0;
            for i in 0..3 {
                p[i] = (p[i] as f32 * a + bg[i] * 255.0 * (1.0 - a) + 0.5) as u8;
            }
            p[3] = 255;
        }
    }
    let size = requested_size(img.width, img.height, o);
    finish(Rgba8 { w: img.width, h: img.height, px }, size, o, vec![("read", t.elapsed().as_millis() as u64)], 0, 0)
}

fn finish(full: Rgba8, (w, h): (u32, u32), o: &Options, mut timings: Vec<(&'static str, u64)>, replaced: usize, level: u8) -> Result<Output, ApiError> {
    let t = Instant::now();
    let out = if (w, h) == (full.w, full.h) { full } else { resize(&full, w, h, false)? };
    let bytes = encode(&out, o)?;
    timings.push(("encode", t.elapsed().as_millis() as u64));
    Ok(Output { bytes, format: o.format, width: out.w, height: out.h, replaced, timings, render_scale: 0.5f64.powi(i32::from(level)) })
}

/// Replaces the contents of the `target` smart object in `tpl` with `upload` and renders the result.
pub fn render(tpl: &Template, target: &Target, upload: &[u8], upload_name: &str, o: &Options) -> Result<Output, ApiError> {
    let mut timings = Vec::new();
    let mut t = Instant::now();
    let mut lap = |label: &'static str, t: &mut Instant| {
        timings.push((label, t.elapsed().as_millis() as u64));
        *t = Instant::now();
    };

    let mut targets = match target {
        Target::Name(name) => tpl.find(name)?,
        Target::LayerId(id) => tpl.find_id(*id)?,
    };
    if o.instances {
        let keys: Vec<u64> = targets.iter().map(|s| s.source_key).collect();
        targets = tpl.smart_objects.iter().filter(|s| keys.contains(&s.source_key)).collect();
    }

    let src = decode_upload(upload_name, upload)?;
    lap("decode", &mut t);

    // Composite on the smallest copy of the document that still covers the output.
    let (dw, dh) = (tpl.doc.size.width, tpl.doc.size.height);
    let (ow, oh) = requested_size(dw, dh, o);
    let level = level_for(dw, dh, ow, oh);
    let base = tpl.document_at(level);
    let doc_scale = 0.5f64.powi(i32::from(level));
    let fmt = base.pixel_format();
    let smart_of = |path: &[usize]| -> Result<&photocraft_doc::SmartObject, ApiError> {
        match base.layer_at(path).map(|l| &l.content) {
            Some(LayerContent::Smart(sm)) => Ok(sm),
            _ => Err(ApiError::internal("smart object layer moved")),
        }
    };

    // Each target's source resolution: the fraction of the contents size its placement needs
    // (a 35-megapixel placeholder placed at 7% is fitted at 14%). One fitted surface per size.
    let mut plan: Vec<(&SmartInfo, f64, (u32, u32))> = Vec::new();
    for s in &targets {
        let k = crate::scale::source_scale(smart_of(&s.path)?);
        let size = (((f64::from(s.width) * k).round() as u32).max(1), ((f64::from(s.height) * k).round() as u32).max(1));
        plan.push((s, k, size));
    }
    let mut fitted: HashMap<(u32, u32), Surface> = HashMap::new();
    for (_, _, size) in &plan {
        if !fitted.contains_key(size) {
            let mut img = fit_upload(&src, size.0, size.1, o.fit)?;
            if let Some(bg) = o.design_background {
                fill_transparent(&mut img, bg);
            }
            fitted.insert(*size, to_surface(&img, fmt, tpl.from_srgb.as_ref()));
        }
    }
    lap("fit", &mut t);

    // The document is copy-on-write: cloning shares every layer's pixel tiles.
    let mut doc = photocraft_doc::Document::clone(&base);
    let canvas = doc.bounds();
    let rendered: Vec<(Vec<usize>, Surface)> = plan
        .par_iter()
        .map(|(s, k, size)| {
            let sm = smart_of(&s.path)?;
            let placed = place(&crate::scale::for_source_scale(sm, *k), &fitted[size], size.0, size.1);
            let displace: Vec<(usize, crate::displace::Displace)> = s.displace.iter().map(|(i, d)| (*i, d.scaled(doc_scale))).collect();
            let extra = |i: usize| displace.iter().find(|(j, _)| *j == i).map(|(_, d)| d);
            Ok((s.path.clone(), crate::filters::apply_smart_filters(&placed, sm, canvas, &extra, crate::displace::DEFAULT)))
        })
        .collect::<Result<_, _>>()?;
    for (path, px) in rendered {
        if let Some(l) = doc.layer_at_mut(&path)
            && let LayerContent::Smart(sm) = &mut l.content
        {
            sm.cache = Some(px);
        }
    }
    lap("place", &mut t);

    let matte = o.background.or((o.format == Format::Jpeg).then_some([1.0, 1.0, 1.0]));
    let full = composite(&doc, tpl.to_srgb.as_ref(), matte);
    drop(doc);
    lap("composite", &mut t);

    let out = if (ow, oh) == (full.w, full.h) { full } else { resize(&full, ow, oh, false)? };
    lap("resize", &mut t);

    let bytes = encode(&out, o)?;
    lap("encode", &mut t);

    Ok(Output { bytes, format: o.format, width: out.w, height: out.h, replaced: targets.len(), timings, render_scale: doc_scale })
}
