//! Photoshop's Filter › Distort › Displace, as a smart filter.
//!
//! PhotoCraft keeps filters it doesn't implement as `psd.unsupportedFilter` (the Photoshop
//! descriptor verbatim, hex in `params.psd`) and skips them on re-render. Displace matters for
//! mockups (it bends the design along fabric folds), so it's implemented here.
//!
//! Descriptor (`Fltr`, class `Dspl`): `HrzS`/`VrtS` scale in percent (100% = up to 128 px),
//! `DspM` `StrF` (stretch the map to fit) or `Tile`, `UndA` `RptE` (repeat edge pixels) or `WrpA`
//! (wrap around), `DspF` the map file alias and, when `EmbF` is set, `DspD`: the map itself,
//! `u32 LE height, u32 LE width, u16 LE (unknown)` then 8-bit planes of `width × height`. The
//! first plane drives horizontal displacement and the second (or the first, for a one-plane map)
//! vertical, as in Photoshop.

use photocraft_geom::Rect;
use photocraft_psd::descriptor::{Descriptor, Value};
use photocraft_raster::Surface;
use rayon::prelude::*;

/// Photoshop's filter id for Displace (`'Dspl'`).
pub const FILTER_ID: i64 = 0x4473_706c;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// The map is stretched/tiled over the document canvas (Photoshop's filter area for a layer
    /// without a selection).
    Canvas,
    /// …over the layer's content bounds.
    Layer,
}

/// Implementation choices Photoshop doesn't document; fixed by comparing with Photoshop's own
/// renders (`mockup-server check`).
#[derive(Clone, Copy, Debug)]
pub struct Variant {
    /// +1: a light map value samples the source further right/down (content moves left/up).
    pub sign: f64,
    pub area: Area,
    pub bilinear: bool,
    /// The map value with no displacement (128 or 127.5).
    pub neutral: f64,
}

pub const DEFAULT: Variant = Variant { sign: 1.0, area: Area::Canvas, bilinear: true, neutral: 128.0 };

#[derive(Debug)]
pub struct Displace {
    pub h_scale: f64,
    pub v_scale: f64,
    pub tile: bool,
    pub wrap: bool,
    pub map_w: usize,
    pub map_h: usize,
    /// Horizontal and vertical planes (may be the same data).
    pub h_map: std::sync::Arc<Vec<u8>>,
    pub v_map: std::sync::Arc<Vec<u8>>,
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if b.len() % 2 != 0 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    b.chunks_exact(2).map(|p| Some(nib(p[0])? << 4 | nib(p[1])?)).collect()
}

fn get<'a>(d: &'a Descriptor, key: &str) -> Option<&'a Value> {
    d.get(key)
}

fn int(d: &Descriptor, key: &str) -> Option<i64> {
    match get(d, key)? {
        Value::Integer(v) => Some(*v as i64),
        Value::LargeInteger(v) => Some(*v),
        Value::Double(v) => Some(*v as i64),
        Value::UnitFloat { value, .. } => Some(*value as i64),
        _ => None,
    }
}

fn enum_is(d: &Descriptor, key: &str, value: &str) -> bool {
    matches!(get(d, key), Some(Value::Enumerated { value: v, .. }) if v.is(value))
}

impl Displace {
    /// The Displace filter in a smart filter's params, if that's what it is (and its map is
    /// embedded). `Err` describes a Displace that can't be applied.
    pub fn from_params(params: &serde_json::Value) -> Option<Result<Displace, String>> {
        let id = params.get("filterID").and_then(|v| v.as_i64());
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        if id != Some(FILTER_ID) && !name.trim_end_matches('.').eq_ignore_ascii_case("displace") {
            return None;
        }
        Some(Self::parse(params))
    }

    fn parse(params: &serde_json::Value) -> Result<Displace, String> {
        let hex = params.get("psd").and_then(|v| v.as_str()).ok_or("no Photoshop data")?;
        let bytes = from_hex(hex).ok_or("bad Photoshop data")?;
        let item = Descriptor::from_bytes(&bytes).map_err(|e| format!("can't read the filter: {e}"))?;
        let Some(Value::Descriptor(f)) = item.get("Fltr") else { return Err("no filter settings".into()) };
        let Some(Value::RawData(map)) = f.get("DspD") else {
            return Err("the displacement map isn't embedded in the PSD (Photoshop linked it to a file)".into());
        };
        if map.len() < 10 {
            return Err("displacement map too short".into());
        }
        let map_h = u32::from_le_bytes(map[0..4].try_into().unwrap()) as usize;
        let map_w = u32::from_le_bytes(map[4..8].try_into().unwrap()) as usize;
        let plane = map_w.checked_mul(map_h).filter(|&p| p > 0).ok_or("empty displacement map")?;
        let body = &map[10..];
        let planes = body.len() / plane;
        if planes == 0 {
            return Err(format!("displacement map data too short for {map_w}×{map_h}"));
        }
        let h_map = std::sync::Arc::new(body[..plane].to_vec());
        let v_map = if planes >= 2 && body[plane..2 * plane] != body[..plane] { std::sync::Arc::new(body[plane..2 * plane].to_vec()) } else { h_map.clone() };
        Ok(Displace {
            h_scale: int(f, "HrzS").unwrap_or(10) as f64,
            v_scale: int(f, "VrtS").unwrap_or(10) as f64,
            tile: enum_is(f, "DspM", "Tile"),
            wrap: enum_is(f, "UndA", "WrpA"),
            map_w,
            map_h,
            h_map,
            v_map,
        })
    }

    /// The filter for a document scaled by `k`: a stretched map follows the canvas by itself;
    /// the displacement, in document pixels, scales with it.
    pub fn scaled(&self, k: f64) -> Displace {
        Displace {
            h_scale: self.h_scale * k,
            v_scale: self.v_scale * k,
            tile: self.tile,
            wrap: self.wrap,
            map_w: self.map_w,
            map_h: self.map_h,
            h_map: self.h_map.clone(),
            v_map: self.v_map.clone(),
        }
    }

    /// Bilinear map value at (u, v) in map pixels (centre-sampled).
    fn map_at(&self, plane: &[u8], u: f64, v: f64) -> f64 {
        let (w, h) = (self.map_w as isize, self.map_h as isize);
        let (x, y) = (u - 0.5, v - 0.5);
        let (x0, y0) = (x.floor(), y.floor());
        let (fx, fy) = (x - x0, y - y0);
        let at = |xi: isize, yi: isize| -> f64 {
            let (xi, yi) = if self.tile { (xi.rem_euclid(w), yi.rem_euclid(h)) } else { (xi.clamp(0, w - 1), yi.clamp(0, h - 1)) };
            plane[yi as usize * self.map_w + xi as usize] as f64
        };
        let (x0, y0) = (x0 as isize, y0 as isize);
        let top = at(x0, y0) * (1.0 - fx) + at(x0 + 1, y0) * fx;
        let bot = at(x0, y0 + 1) * (1.0 - fx) + at(x0 + 1, y0 + 1) * fx;
        top * (1.0 - fy) + bot * fy
    }

    /// Displaces `src` (document coordinates) over `canvas`, as Photoshop does for a layer.
    pub fn apply(&self, src: &Surface, canvas: Rect, var: Variant) -> Surface {
        let fmt = src.format();
        let n = fmt.channels();
        let alpha = fmt.alpha;
        let area = match var.area {
            Area::Canvas => canvas,
            Area::Layer => src.content_bounds(),
        };
        let mut out = Surface::new(fmt);
        if area.is_empty() {
            return out;
        }
        let (aw, ah) = (area.width() as usize, area.height() as usize);
        // Source pixels over the filter area, premultiplied so transparent edges don't bleed.
        let mut px = src.read_region(area);
        if alpha {
            for p in px.chunks_exact_mut(n) {
                let a = p[n - 1];
                for c in &mut p[..n - 1] {
                    *c *= a;
                }
            }
        }
        // Map pixels per filter-area pixel.
        let (sx, sy) = if self.tile { (1.0, 1.0) } else { (self.map_w as f64 / aw as f64, self.map_h as f64 / ah as f64) };
        let (kh, kv) = (var.sign * self.h_scale / 100.0, var.sign * self.v_scale / 100.0);
        let fetch = |xi: isize, yi: isize, acc: &mut [f32], wgt: f32| {
            let (xi, yi) = if self.wrap {
                (xi.rem_euclid(aw as isize), yi.rem_euclid(ah as isize))
            } else {
                (xi.clamp(0, aw as isize - 1), yi.clamp(0, ah as isize - 1))
            };
            let o = (yi as usize * aw + xi as usize) * n;
            for c in 0..n {
                acc[c] += px[o + c] * wgt;
            }
        };
        let mut data = vec![0.0f32; aw * ah * n];
        data.par_chunks_mut(aw * n).enumerate().for_each(|(y, row)| {
            let mut acc = vec![0.0f32; n];
            for x in 0..aw {
                let (u, v) = ((x as f64 + 0.5) * sx, (y as f64 + 0.5) * sy);
                let dx = (self.map_at(&self.h_map, u, v) - var.neutral) * kh;
                let dy = (self.map_at(&self.v_map, u, v) - var.neutral) * kv;
                let (fx, fy) = (x as f64 + dx, y as f64 + dy);
                acc.iter_mut().for_each(|a| *a = 0.0);
                if var.bilinear {
                    let (x0, y0) = (fx.floor(), fy.floor());
                    let (tx, ty) = ((fx - x0) as f32, (fy - y0) as f32);
                    let (x0, y0) = (x0 as isize, y0 as isize);
                    fetch(x0, y0, &mut acc, (1.0 - tx) * (1.0 - ty));
                    fetch(x0 + 1, y0, &mut acc, tx * (1.0 - ty));
                    fetch(x0, y0 + 1, &mut acc, (1.0 - tx) * ty);
                    fetch(x0 + 1, y0 + 1, &mut acc, tx * ty);
                } else {
                    fetch(fx.round() as isize, fy.round() as isize, &mut acc, 1.0);
                }
                let o = &mut row[x * n..x * n + n];
                if alpha {
                    let a = acc[n - 1];
                    for c in 0..n - 1 {
                        o[c] = if a > 1e-6 { (acc[c] / a).clamp(0.0, 1.0) } else { 0.0 };
                    }
                    o[n - 1] = a.clamp(0.0, 1.0);
                } else {
                    o.copy_from_slice(&acc);
                }
            }
        });
        out.write_region(area, &data);
        out.prune();
        out
    }
}
