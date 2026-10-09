//! Rendering at a lower resolution.
//!
//! Smaller outputs should cost less, so a template keeps downscaled copies of its document
//! (level n = 1/2ⁿ), built once on first use, and a request composites on the smallest copy
//! that still covers its output. The copies keep every layer's Photoshop pixels (resampled),
//! as PhotoCraft's own Image Size does for pixels: masks, caches, fill caches; effects scale
//! their pixel sizes ("Scale Styles"); vector masks, shapes and smart object placements scale
//! their geometry. Warps are left alone: they live in the smart object's contents space, which
//! doesn't change. Nothing is re-rendered from source, so layers that aren't replaced look
//! exactly like Photoshop's render, just smaller.
//!
//! The second half is the source side: a placeholder's contents can be far larger than the
//! area they're placed in (a 12992-pixel-wide label placed at 7%). [`source_scale`] gives the
//! fraction of the contents size an upload needs to be fitted at, and [`for_source_scale`]
//! adjusts the placement so a source at that size renders where the full-size one would.

use photocraft_algo::resample::{Resample, resize_surface_in_canvas};
use photocraft_doc::{Document, Effect, Effects, FxPaint, Layer, LayerContent, Size, SmartFilter, SmartObject};
use photocraft_geom::{Affine, Rect};

/// Deepest level kept: 1/8 of the document.
pub const MAX_LEVEL: u8 = 3;

/// `doc` at 1/2^`level` of its size.
pub fn scale_document(doc: &Document, level: u8) -> Document {
    let s = 0.5f64.powi(i32::from(level));
    let (w, h) = (doc.size.width, doc.size.height);
    let (nw, nh) = (((f64::from(w) * s).round() as u32).max(1), ((f64::from(h) * s).round() as u32).max(1));
    let (sx, sy) = (f64::from(nw) / f64::from(w.max(1)), f64::from(nh) / f64::from(h.max(1)));
    let canvas = doc.bounds();
    let a = Affine { m: [sx, 0.0, 0.0, sy, 0.0, 0.0] };
    let mut d = doc.clone();
    scale_layers(&mut d.layers, sx, sy, canvas, &a);
    for ch in &mut d.channels {
        ch.surface = resize_surface_in_canvas(&ch.surface, sx, sy, Resample::Bilinear, canvas);
    }
    d.quick_mask = None;
    d.selection = None;
    d.size = Size::new(nw, nh);
    d
}

fn scale_layers(layers: &mut [Layer], sx: f64, sy: f64, canvas: Rect, a: &Affine) {
    let k = ((sx + sy) / 2.0) as f32;
    let pixels = |s: &photocraft_raster::Surface| resize_surface_in_canvas(s, sx, sy, Resample::Bicubic, canvas);
    let mask = |s: &photocraft_raster::Surface| resize_surface_in_canvas(s, sx, sy, Resample::Bilinear, canvas);
    for l in layers {
        if let Some(m) = &mut l.mask {
            m.surface = mask(&m.surface);
        }
        if let Some(vm) = &mut l.vector_mask {
            vm.path = vm.path.transform(a);
        }
        if let Some(fc) = &mut l.fill_cache {
            fc.surface = pixels(&fc.surface);
        }
        scale_effects(&mut l.effects, k);
        match &mut l.content {
            LayerContent::Raster(s) => *s = pixels(s),
            LayerContent::Text(t) => {
                if let Some(c) = &mut t.cache {
                    *c = pixels(c);
                }
            }
            LayerContent::Shape(sh) => {
                sh.path = sh.path.transform(a);
                if let Some(c) = &mut sh.cache {
                    *c = pixels(c);
                }
            }
            LayerContent::Smart(sm) => {
                if let Some(c) = &mut sm.cache {
                    *c = pixels(c);
                }
                if let Some(m) = &mut sm.filter_mask {
                    m.surface = mask(&m.surface);
                }
                // Source → document: the document scale goes on the outside.
                let [ma, mb, mc, md, me, mf] = sm.transform.m;
                sm.transform = Affine { m: [sx * ma, sy * mb, sx * mc, sy * md, sx * me, sy * mf] };
                if let Some(p) = &mut sm.perspective {
                    let [h0, h1, h2, h3, h4, h5, h6, h7, h8] = *p;
                    *p = [sx * h0, sx * h1, sx * h2, sy * h3, sy * h4, sy * h5, h6, h7, h8];
                }
                for f in &mut sm.smart_filters {
                    scale_filter(f, f64::from(k));
                }
            }
            LayerContent::Group(g) => scale_layers(&mut g.children, sx, sy, canvas, a),
            _ => {}
        }
    }
}

/// Smart filter settings in document pixels (blur radii, motion distance) scale with it.
fn scale_filter(f: &mut SmartFilter, k: f64) {
    if !(f.command.starts_with("filter.blur.") || f.command.starts_with("filter.sharpen.")) {
        return;
    }
    for key in ["radius", "distance"] {
        if let Some(v) = f.params.get_mut(key)
            && let Some(x) = v.as_f64()
        {
            *v = serde_json::json!(x * k);
        }
    }
}

/// Photoshop's "Scale Styles" (after PhotoCraft's Image Size).
fn scale_effects(fx: &mut Effects, k: f32) {
    for e in &mut fx.items {
        match e {
            Effect::DropShadow(s) | Effect::InnerShadow(s) => {
                s.distance *= k;
                s.size *= k;
            }
            Effect::OuterGlow(g) | Effect::InnerGlow(g) => g.size *= k,
            Effect::Stroke(s) => s.size *= k,
            Effect::Satin(s) => {
                s.distance *= k;
                s.size *= k;
            }
            Effect::BevelEmboss(b) => {
                b.size *= k;
                b.soften *= k;
            }
            Effect::PatternOverlay { scale, .. } => *scale *= k,
            Effect::GradientOverlay { .. } | Effect::ColorOverlay { .. } => {}
        }
        if let Effect::Stroke(s) = e
            && let FxPaint::Pattern { scale, .. } = &mut s.paint
        {
            *scale *= k;
        }
    }
    if k != 1.0 {
        fx.psd_raw = None;
    }
}

/// The fraction of the contents size an upload is fitted at for `sm`: twice the placement's
/// largest scale factor (room for bicubic sampling, warps and perspective), at most 1.
pub fn source_scale(sm: &SmartObject) -> f64 {
    let [a, b, c, d, _, _] = sm.transform.m;
    let p = a * a + b * b + c * c + d * d;
    let q = (a * d - b * c).abs();
    let sigma = ((p + (p * p - 4.0 * q * q).max(0.0).sqrt()) / 2.0).sqrt();
    if !sigma.is_finite() || sigma <= 0.0 {
        return 1.0;
    }
    (2.0 * sigma).clamp(1.0 / 64.0, 1.0)
}

/// `sm` adjusted so that a source fitted at `k` × the contents size renders where the full-size
/// contents would: the placement maps source pixels, so it is divided by `k`; the warp, defined
/// over the contents box, is scaled to the smaller box.
pub fn for_source_scale(sm: &SmartObject, k: f64) -> SmartObject {
    let mut o = sm.clone();
    o.cache = None;
    if (k - 1.0).abs() < 1e-12 {
        return o;
    }
    let [a, b, c, d, e, f] = sm.transform.m;
    o.transform = Affine { m: [a / k, b / k, c / k, d / k, e, f] };
    if let Some(p) = &mut o.perspective {
        let [h0, h1, h2, h3, h4, h5, h6, h7, h8] = *p;
        *p = [h0 / k, h1 / k, h2, h3 / k, h4 / k, h5, h6 / k, h7 / k, h8];
    }
    if let Some(w) = &mut o.warp {
        w.bounds = [w.bounds[0] * k, w.bounds[1] * k, w.bounds[2] * k, w.bounds[3] * k];
        if let Some(m) = &mut w.mesh {
            for p in &mut m.points {
                p[0] *= k;
                p[1] *= k;
            }
        }
    }
    o
}
