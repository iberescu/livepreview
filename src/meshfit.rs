//! Recovers the exact Bezier mesh of a smart object's warp from the pixels Photoshop saved for
//! the layer.
//!
//! Photoshop renders every warp (presets included) as one bicubic Bezier patch over the
//! contents. A PSD stores the preset's name and bend but not that patch, and Photoshop's preset
//! formulas are undocumented. The layer's pixels, though, are Photoshop's render of the
//! *original* contents through that patch. So the 16 control points are fitted until PhotoCraft's
//! render of the original contents matches Photoshop's alpha. Replacement images then go
//! through the recovered mesh.
//!
//! The fit runs on an alpha-only, downscaled copy of the contents and a coarse triangle grid,
//! coarse to fine (blurred targets, shrinking steps), by coordinate descent. Two priors keep the
//! mesh plausible where the placeholder has no ink to constrain it: a penalty on third
//! differences of the control net (which leaves affine, quadratic and arc-like deformations
//! alone but suppresses wobbles) and a weak pull towards the identity.

use photocraft_algo::transform::{Homography, Interp};
use photocraft_algo::warp::warp_triangles;
use photocraft_color::PixelFormat;
use photocraft_doc::SmartObject;
use photocraft_geom::Rect;
use photocraft_geom::warp::BezierMesh;
use photocraft_raster::Surface;
use rayon::prelude::*;

/// Bump when the algorithm changes, so cached meshes are refitted.
pub const VERSION: u32 = 3;

/// Contents downscale for fitting.
const DOWN: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Weight of the third-difference (wobble) penalty.
    pub smooth: f64,
    /// Weight of the pull towards the identity mesh.
    pub identity: f64,
}

impl Options {
    /// Strong wobble suppression: right for symmetric presets (Arc, Arc Lower/Upper, Arch,
    /// Bulge, Shell, Fisheye, Inflate, Squeeze), where it both cleans the mesh and finds a better
    /// fit (calibrated on Photoshop renders: 2.9/255 vs 4.8 unregularised).
    pub const STRONG: Options = Options { smooth: 1e5, identity: 1e3 };
    /// Weak: for S-shaped presets (Flag, Wave, Fish, Rise, Twist) and perspective distortions,
    /// whose control nets legitimately have large third differences.
    pub const WEAK: Options = Options { smooth: 1e3, identity: 1e3 };

    /// `base` with `MESHFIT_SMOOTH` / `MESHFIT_IDENTITY` overrides (calibration runs).
    pub fn from_env(base: Options) -> Self {
        let get = |k: &str, d: f64| std::env::var(k).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(d);
        Options { smooth: get("MESHFIT_SMOOTH", base.smooth), identity: get("MESHFIT_IDENTITY", base.identity) }
    }
}

/// An alpha-only copy of `src` (contents, `w`×`h` at the origin), `DOWN`× smaller.
fn small_alpha(src: &Surface, w: u32, h: u32) -> (Surface, u32, u32) {
    let (sw, sh) = ((w as usize).div_ceil(DOWN), (h as usize).div_ceil(DOWN));
    let fmt = src.format();
    let n = fmt.channels();
    let rows: Vec<Vec<f32>> = (0..sh)
        .into_par_iter()
        .map(|by| {
            let y0 = (by * DOWN) as i32;
            let y1 = ((by + 1) * DOWN).min(h as usize) as i32;
            let band = src.read_region(Rect::new(0, y0, w as i32, y1));
            let bh = (y1 - y0) as usize;
            let mut out = vec![0.0f32; sw * 4];
            for bx in 0..sw {
                let (x0, x1) = (bx * DOWN, ((bx + 1) * DOWN).min(w as usize));
                let mut a = 0.0;
                for yy in 0..bh {
                    for xx in x0..x1 {
                        a += if fmt.alpha { band[(yy * w as usize + xx) * n + n - 1] } else { 1.0 };
                    }
                }
                out[bx * 4 + 3] = a / ((x1 - x0) * bh) as f32;
            }
            out
        })
        .collect();
    let mut s = Surface::new(PixelFormat::RGBA8);
    s.write_region(Rect::new(0, 0, sw as i32, sh as i32), &rows.concat());
    (s, sw as u32, sh as u32)
}

fn box_blur(px: &mut [f32], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0.0f32; px.len()];
    for y in 0..h {
        let row = &px[y * w..(y + 1) * w];
        let mut acc: f32 = row[..r.min(w)].iter().sum();
        for x in 0..w {
            if x + r < w {
                acc += row[x + r];
            }
            if x > r {
                acc -= row[x - r - 1];
            }
            let cnt = (x + r).min(w - 1) - x.saturating_sub(r) + 1;
            tmp[y * w + x] = acc / cnt as f32;
        }
    }
    for x in 0..w {
        let mut acc: f32 = (0..r.min(h)).map(|y| tmp[y * w + x]).sum();
        for y in 0..h {
            if y + r < h {
                acc += tmp[(y + r) * w + x];
            }
            if y > r {
                acc -= tmp[(y - r - 1) * w + x];
            }
            let cnt = (y + r).min(h - 1) - y.saturating_sub(r) + 1;
            px[y * w + x] = acc / cnt as f32;
        }
    }
}

fn alpha_of(s: &Surface, r: Rect) -> Vec<f32> {
    let fmt = s.format();
    let n = fmt.channels();
    s.read_region(r).chunks_exact(n).map(|p| if fmt.alpha { p[n - 1] } else { 1.0 }).collect()
}

/// Mean absolute alpha difference (0..255) over the pixels where either surface has content.
pub fn alpha_diff(a: &Surface, b: &Surface, canvas: Rect) -> f64 {
    let r = a.content_bounds().union(&b.content_bounds()).intersect(&canvas);
    if r.is_empty() {
        return 0.0;
    }
    let (pa, pb) = (alpha_of(a, r), alpha_of(b, r));
    let (mut sum, mut n) = (0.0f64, 0usize);
    for (x, y) in pa.iter().zip(&pb) {
        if *x == 0.0 && *y == 0.0 {
            continue;
        }
        sum += (x - y).abs() as f64;
        n += 1;
    }
    if n == 0 { 0.0 } else { sum / n as f64 * 255.0 }
}

/// Largest third difference of the control net (normalised to the box), a measure of wobble.
pub fn wobble(m: &BezierMesh, bounds: [f64; 4]) -> f64 {
    let (bw, bh) = ((bounds[2] - bounds[0]).max(1e-9), (bounds[3] - bounds[1]).max(1e-9));
    let p = |i: usize, j: usize| {
        let q = m.point(i, j);
        [q[0] / bw, q[1] / bh]
    };
    let mut worst: f64 = 0.0;
    for k in 0..4 {
        for c in 0..2 {
            let row = p(0, k)[c] - 3.0 * p(1, k)[c] + 3.0 * p(2, k)[c] - p(3, k)[c];
            let col = p(k, 0)[c] - 3.0 * p(k, 1)[c] + 3.0 * p(k, 2)[c] - p(k, 3)[c];
            worst = worst.max(row.abs()).max(col.abs());
        }
    }
    worst
}

fn penalty(m: &BezierMesh, id: &BezierMesh, bounds: [f64; 4], o: &Options) -> f64 {
    let (bw, bh) = ((bounds[2] - bounds[0]).max(1e-9), (bounds[3] - bounds[1]).max(1e-9));
    let p = |i: usize, j: usize| {
        let q = m.point(i, j);
        [q[0] / bw, q[1] / bh]
    };
    let mut third = 0.0;
    for k in 0..4 {
        for c in 0..2 {
            let row = p(0, k)[c] - 3.0 * p(1, k)[c] + 3.0 * p(2, k)[c] - p(3, k)[c];
            let col = p(k, 0)[c] - 3.0 * p(k, 1)[c] + 3.0 * p(k, 2)[c] - p(k, 3)[c];
            third += row * row + col * col;
        }
    }
    let mut ident = 0.0;
    for (a, b) in m.points.iter().zip(&id.points) {
        let (dx, dy) = ((a[0] - b[0]) / bw, (a[1] - b[1]) / bh);
        ident += dx * dx + dy * dy;
    }
    o.smooth * third + o.identity * ident
}

pub struct Fit {
    pub mesh: BezierMesh,
    /// Mean absolute alpha difference (0..255) at fit scale, before and after.
    pub before: f64,
    pub after: f64,
    pub evals: usize,
}

/// Fits the warp mesh of `sm` (contents `w`×`h`, `src` at full size, the warp box `bounds` in
/// contents pixels) to Photoshop's pixels `ps`, starting from `init`.
pub fn fit(sm: &SmartObject, src: &Surface, w: u32, h: u32, ps: &Surface, canvas: Rect, bounds: [f64; 4], init: &BezierMesh, opts: &Options) -> Fit {
    let (small, sw, sh) = small_alpha(src, w, h);
    let (bx0, by0, bw, bh) = (bounds[0], bounds[1], bounds[2] - bounds[0], bounds[3] - bounds[1]);
    let to_doc = |u: f64, v: f64| -> (f64, f64) {
        match &sm.perspective {
            Some(p) => Homography(*p).apply(u, v),
            None => {
                let [a, b, c, d, e, f] = sm.transform.m;
                (a * u + c * v + e, b * u + d * v + f)
            }
        }
    };
    let region = ps.content_bounds().inflate(24).intersect(&canvas);
    let (rw, rh) = (region.width() as usize, region.height() as usize);
    let target0 = alpha_of(ps, region);
    let npx = target0.len().max(1) as f64;
    // Triangle grid over the warp box.
    let (nx, ny) = ((sw as usize / 4).clamp(8, 160), (sh as usize / 4).clamp(8, 160));
    let w1 = nx + 1;
    let mut tris = Vec::with_capacity(nx * ny * 2);
    for j in 0..ny {
        for i in 0..nx {
            let a = j * w1 + i;
            tris.push([a, a + 1, a + w1 + 1]);
            tris.push([a, a + w1 + 1, a + w1]);
        }
    }
    let render = |m: &BezierMesh| -> Vec<f32> {
        let mut verts = Vec::with_capacity(w1 * (ny + 1));
        for j in 0..=ny {
            for i in 0..=nx {
                let (s, t) = (i as f64 / nx as f64, j as f64 / ny as f64);
                let p = m.eval(s, t);
                let (dx, dy) = to_doc(p[0], p[1]);
                verts.push(([dx, dy], [(bx0 + s * bw) / DOWN as f64, (by0 + t * bh) / DOWN as f64]));
            }
        }
        let out = warp_triangles(&small, Rect::new(0, 0, sw as i32, sh as i32), &verts, &tris, Interp::Bilinear);
        alpha_of(&out, region)
    };
    // Data term: mean squared alpha difference, scaled so a converged fit scores ~10-100.
    let data = |a: &[f32], t: &[f32]| -> f64 { a.iter().zip(t).map(|(x, y)| ((x - y) * (x - y)) as f64).sum::<f64>() / npx * 1e5 };
    let mae = |a: &[f32], t: &[f32]| -> f64 { a.iter().zip(t).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / npx * 255.0 };

    let mut mesh = init.clone();
    let before = mae(&render(&mesh), &target0);
    let mut evals = 1;
    // Doc pixels per contents pixel, to size steps in contents units.
    let scale = {
        let (x0, y0) = to_doc(bx0, by0);
        let (x1, y1) = to_doc(bx0 + bw, by0 + bh);
        ((x1 - x0).hypot(y1 - y0) / bw.hypot(bh)).max(1e-6)
    };
    // (blur radius in doc px, step in doc px)
    for (blur, step0) in [(12usize, 24.0f64), (6, 12.0), (3, 6.0), (1, 3.0), (0, 1.5), (0, 0.5)] {
        let mut target = target0.clone();
        box_blur(&mut target, rw, rh, blur);
        let eval = |m: &BezierMesh| {
            let mut a = render(m);
            box_blur(&mut a, rw, rh, blur);
            data(&a, &target) + penalty(m, init, bounds, opts)
        };
        let mut best = eval(&mesh);
        evals += 1;
        let mut step = step0 / scale;
        for _ in 0..2 {
            for _round in 0..24 {
                // Every single-coordinate move of every control point, scored in parallel.
                let moves: Vec<(usize, usize, f64)> = (0..mesh.points.len()).flat_map(|i| [(i, 0, step), (i, 0, -step), (i, 1, step), (i, 1, -step)]).collect();
                let mut improving: Vec<(f64, (usize, usize, f64))> = moves
                    .par_iter()
                    .map(|&m| {
                        let mut t = mesh.clone();
                        t.points[m.0][m.1] += m.2;
                        (eval(&t), m)
                    })
                    .filter(|(sc, _)| *sc < best)
                    .collect();
                evals += moves.len();
                if improving.is_empty() {
                    break;
                }
                improving.sort_by(|a, b| a.0.total_cmp(&b.0));
                // The best single move, or every improving move stacked (one per coordinate)
                // when that scores better still.
                let (single_score, single) = improving[0];
                let mut combined = mesh.clone();
                let mut used = std::collections::HashSet::new();
                for (_, (i, axis, d)) in &improving {
                    if used.insert((*i, *axis)) {
                        combined.points[*i][*axis] += d;
                    }
                }
                let combined_score = if used.len() > 1 {
                    evals += 1;
                    eval(&combined)
                } else {
                    f64::INFINITY
                };
                if combined_score < single_score {
                    mesh = combined;
                    best = combined_score;
                } else {
                    mesh.points[single.0][single.1] += single.2;
                    best = single_score;
                }
            }
            step /= 2.0;
        }
    }
    let after = mae(&render(&mesh), &target0);
    Fit { mesh, before, after, evals }
}
