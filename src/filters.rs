//! The smart filter stack, as `photocraft_engine::smart_cmds::apply_smart_filters` runs it, plus
//! Photoshop filters PhotoCraft keeps but doesn't render (Displace, see `displace.rs`).
//! `blend_surfaces` and `mask_mix` follow the engine's private helpers of the same name
//! (PhotoCraft, MIT OR Apache-2.0).

use photocraft_color::BlendMode;
use photocraft_doc::{LayerMask, SmartObject};
use photocraft_geom::Rect;
use photocraft_raster::Surface;

use crate::displace::{Displace, Variant};

/// Parsed Photoshop filters of one smart object, by index in its filter stack.
pub type Extra<'a> = &'a dyn Fn(usize) -> Option<&'a Displace>;

pub fn apply_smart_filters(placed: &Surface, sm: &SmartObject, canvas: Rect, extra: Extra<'_>, var: Variant) -> Surface {
    if !sm.filters_enabled || !sm.smart_filters.iter().any(|f| f.visible) {
        return placed.clone();
    }
    let mut cur = placed.clone();
    let extent = canvas.union(&placed.content_bounds());
    for (i, f) in sm.smart_filters.iter().enumerate().filter(|(_, f)| f.visible) {
        let out = match extra(i) {
            Some(d) => Some(d.apply(&cur, canvas, var)),
            None => photocraft_engine::filters::apply_filter_to_surface(&f.command, &f.params, &cur, canvas, None, extent),
        };
        // Unknown ids (Photoshop filters nobody implements) leave the pixels alone.
        let Some(out) = out else { continue };
        cur = if f.blend == BlendMode::Normal && f.opacity >= 1.0 { out } else { blend_surfaces(&cur, &out, f.blend, f.opacity.clamp(0.0, 1.0)) };
    }
    match sm.filter_mask.as_ref().filter(|m| m.enabled) {
        Some(m) => mask_mix(placed, &cur, m),
        None => cur,
    }
}

/// `top` blended over `base` with `mode` at `opacity` (a smart filter's blending options).
fn blend_surfaces(base: &Surface, top: &Surface, mode: BlendMode, opacity: f32) -> Surface {
    let fmt = base.format();
    let area = base.content_bounds().union(&top.content_bounds());
    let mut out = base.clone();
    if area.is_empty() {
        return out;
    }
    let n = fmt.channels();
    let (b, t) = (base.read_region(area), top.read_region(area));
    let mut o = vec![0.0f32; b.len()];
    for ((bp, tp), op) in b.chunks_exact(n).zip(t.chunks_exact(n)).zip(o.chunks_exact_mut(n)) {
        let rgba = photocraft_color::blend::composite(mode, photocraft_raster::to_rgba(&fmt, bp), photocraft_raster::to_rgba(&fmt, tp), opacity);
        photocraft_raster::from_rgba_into(&fmt, rgba, op);
    }
    out.write_region(area, &o);
    out.prune();
    out
}

/// Mixes `filtered` over `base` through the filter mask (premultiplied, any channel layout).
fn mask_mix(base: &Surface, filtered: &Surface, mask: &LayerMask) -> Surface {
    let fmt = base.format();
    let area = base.content_bounds().union(&filtered.content_bounds());
    let mut out = filtered.clone();
    if area.is_empty() {
        return out;
    }
    let n = fmt.channels();
    let (b, f) = (base.read_region(area), filtered.read_region(area));
    let mut k = Vec::new();
    mask.values_into(area, &mut k);
    let mut o = vec![0.0f32; b.len()];
    for (i, ((bp, fp), op)) in b.chunks_exact(n).zip(f.chunks_exact(n)).zip(o.chunks_exact_mut(n)).enumerate() {
        let m = k.get(i).copied().unwrap_or(1.0).clamp(0.0, 1.0);
        if !fmt.alpha {
            for c in 0..n {
                op[c] = bp[c] + (fp[c] - bp[c]) * m;
            }
            continue;
        }
        let a = n - 1;
        let oa = bp[a] + (fp[a] - bp[a]) * m;
        op[a] = oa;
        for c in 0..a {
            let pm = bp[c] * bp[a] + (fp[c] * fp[a] - bp[c] * bp[a]) * m;
            op[c] = if oa > 0.0 { (pm / oa).clamp(0.0, 1.0) } else { 0.0 };
        }
    }
    out.write_region(area, &o);
    out.prune();
    out
}
