//! `mockup-server check <template> [out-dir]`: how faithfully this service reproduces Photoshop
//! for a template, measured against the pixels Photoshop saved in the PSD.
//!
//! 1. Each smart object is re-rendered from its own (unchanged) contents through its placement,
//!    warp and smart filters, and compared with the layer's pixels as Photoshop rendered them:
//!    this is the error a replacement image will see.
//! 2. The whole document is composited (layer effects, blend modes, masks) and compared with
//!    Photoshop's flattened image stored in the PSD.
//!
//! `CHECK_VARIANTS=1` also sweeps the Displace implementation choices; `CHECK_DEBUG=1` dumps
//! each smart object's placement descriptor.

use std::path::Path;

use photocraft_doc::LayerContent;
use photocraft_engine::smart_cmds;
use photocraft_geom::Rect;
use photocraft_raster::{Surface, to_rgba};

use crate::displace::{self, Area, Variant};
use crate::meshfit;
use crate::templates::Store;

struct Stats {
    /// Mean absolute difference of premultiplied RGBA (0..255).
    mean: f64,
    /// Share of pixels off by more than 8/255 (percent).
    off: f64,
    /// Mean absolute alpha difference (0..255): shape only.
    alpha: f64,
    /// Alpha-weighted mean colour (0..255) and mean coverage (0..1) of each side.
    ours: ([f64; 3], f64),
    ps: ([f64; 3], f64),
}

fn stats(ours: &Surface, ps: &Surface, r: Rect) -> Stats {
    let (fa, fb) = (ours.format(), ps.format());
    let (pa, pb) = (ours.read_region(r), ps.read_region(r));
    let (na, nb) = (fa.channels(), fb.channels());
    let (mut sum, mut bad, mut asum, mut n) = (0.0f64, 0usize, 0.0f64, 0usize);
    let (mut ca, mut cb, mut aa, mut ab) = ([0.0f64; 3], [0.0f64; 3], 0.0f64, 0.0f64);
    for (x, y) in pa.chunks_exact(na).zip(pb.chunks_exact(nb)) {
        let (x, y) = (to_rgba(&fa, x), to_rgba(&fb, y));
        if x[3] == 0.0 && y[3] == 0.0 {
            continue;
        }
        let mut m = (x[3] - y[3]).abs();
        for c in 0..3 {
            m = m.max((x[c] * x[3] - y[c] * y[3]).abs());
            ca[c] += (x[c] * x[3]) as f64;
            cb[c] += (y[c] * y[3]) as f64;
        }
        sum += m as f64;
        bad += (m > 8.0 / 255.0) as usize;
        asum += (x[3] - y[3]).abs() as f64;
        aa += x[3] as f64;
        ab += y[3] as f64;
        n += 1;
    }
    let nf = n.max(1) as f64;
    let ink = |c: [f64; 3], a: f64| if a > 0.0 { [c[0] / a * 255.0, c[1] / a * 255.0, c[2] / a * 255.0] } else { [0.0; 3] };
    Stats { mean: sum / nf * 255.0, off: bad as f64 / nf * 100.0, alpha: asum / nf * 255.0, ours: (ink(ca, aa), aa / nf), ps: (ink(cb, ab), ab / nf) }
}

impl Stats {
    fn line(&self) -> String {
        let c = |v: [f64; 3]| format!("({:.0},{:.0},{:.0})", v[0], v[1], v[2]);
        format!(
            "mean diff {:.2}/255, {:.1}% off by >8, shape {:.2}/255; ink ours {} ps {}; coverage ours {:.3} ps {:.3}",
            self.mean,
            self.off,
            self.alpha,
            c(self.ours.0),
            c(self.ps.0),
            self.ours.1,
            self.ps.1
        )
    }
}

fn save_png(s: &Surface, r: Rect, path: &Path) {
    let fmt = s.format();
    let n = fmt.channels();
    let px: Vec<u8> = s.read_region(r).chunks_exact(n).flat_map(|p| to_rgba(&fmt, p).map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect();
    save_rgba(&px, r.width() as u32, r.height() as u32, path);
}

fn save_rgba(px: &[u8], w: u32, h: u32, path: &Path) {
    if let Err(e) = image::save_buffer(path, px, w, h, image::ExtendedColorType::Rgba8) {
        eprintln!("can't write {}: {e}", path.display());
    }
}

fn stem(name: &str, id: Option<u32>) -> String {
    format!("{}-{}", name.trim().replace(|c: char| !c.is_ascii_alphanumeric(), "_"), id.map_or("x".to_string(), |i| i.to_string()))
}

fn profile_desc(icc: Option<&[u8]>) -> String {
    match icc {
        None => "no profile (sRGB assumed)".into(),
        Some(b) => match photocraft_cms::Profile::parse(b) {
            Ok(p) => format!("{:?} profile, {} bytes{}", p.color_space, b.len(), if p.same_colors(photocraft_cms::Builtin::Srgb.profile()) { " (sRGB)" } else { "" }),
            Err(_) => format!("unparseable profile, {} bytes", b.len()),
        },
    }
}

pub fn run(args: &[String]) -> i32 {
    let Some(id) = args.first() else {
        eprintln!("usage: mockup-server check <template-id> [out-dir]");
        return 2;
    };
    let out = args.get(1).map(std::path::PathBuf::from);
    let dir = std::path::PathBuf::from(std::env::var("PSD_DIR").unwrap_or_else(|_| "/data/psds".into()));
    let tpl = match Store::new(dir).get(id) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("{}", e.message);
            return 1;
        }
    };
    let doc = &tpl.doc;
    let canvas = doc.bounds();
    let fmt = doc.pixel_format();
    println!(
        "{}: {}x{} {:?} {}-bit, {}; loaded in {} ms",
        tpl.id,
        doc.size.width,
        doc.size.height,
        doc.mode,
        fmt.sample.bytes() * 8,
        profile_desc(doc.icc_profile.as_deref().map(|v| v.as_slice())),
        tpl.load_ms
    );

    for s in &tpl.smart_objects {
        let Some(layer) = doc.layer_at(&s.path) else { continue };
        let LayerContent::Smart(sm) = &layer.content else { continue };
        println!("\n\"{}\" (layer id {:?}) contents {}x{}", s.name, s.layer_id, s.width, s.height);
        if !s.filters.is_empty() {
            println!("  filters: {}", s.filters.join(" → "));
        }
        if let Some(w) = &s.warp {
            print!("  warp: {} — {}", w.style, w.rendering);
            if let Some(f) = &w.fit {
                print!(" (shape diff {:.2} → {:.2}/255, {} ms)", f.before, f.after, f.ms);
            }
            println!();
            if let Some(m) = sm.warp.as_ref().and_then(|w| w.mesh.as_ref()) {
                let b = sm.warp.as_ref().map(|w| w.bounds).unwrap_or([0.0, 0.0, 1.0, 1.0]);
                println!("  mesh (normalised to the warp box; wobble {:.4}):", meshfit::wobble(m, b));
                for j in 0..4 {
                    let row: Vec<String> = (0..4)
                        .map(|i| {
                            let p = m.point(i, j);
                            format!("({:+.4},{:+.4})", (p[0] - b[0]) / (b[2] - b[0]), (p[1] - b[1]) / (b[3] - b[1]))
                        })
                        .collect();
                    println!("    {}", row.join(" "));
                }
            }
        }
        let Some(ps) = sm.cache.as_ref() else {
            println!("  (Photoshop saved no pixels for this layer)");
            continue;
        };
        let Some((file, bytes)) = smart_cmds::source_bytes(&doc.metadata, &sm.source) else { continue };
        if let Ok(src_doc) = smart_cmds::decode_source(&file, &bytes) {
            println!(
                "  contents file \"{file}\": {:?} {}-bit, {}",
                src_doc.mode,
                src_doc.pixel_format().sample.bytes() * 8,
                profile_desc(src_doc.icc_profile.as_deref().map(|v| v.as_slice()))
            );
        }
        if std::env::var("CHECK_DEBUG").is_ok() {
            println!("  transform {:?}\n  perspective {:?}", sm.transform, sm.perspective);
            for (k, data) in &layer.psd_blocks {
                if (k == b"SoLd" || k == b"SoLE") && data.len() > 8 {
                    if let Ok((d, _)) = photocraft_psd::VersionedDescriptor::parse_prefix(&data[8..]) {
                        dump(&d.descriptor, 4);
                    }
                }
            }
        }
        let Ok(src) = smart_cmds::source_image(&file, &bytes, fmt) else { continue };
        let placed = crate::render::place(sm, &src.surface, s.width, s.height);
        let region = ps.content_bounds().union(&placed.content_bounds()).intersect(&canvas);
        let extra = |i: usize| s.displace.iter().find(|(j, _)| *j == i).map(|(_, d)| &**d);
        let ours = crate::filters::apply_smart_filters(&placed, sm, canvas, &extra, displace::DEFAULT);
        println!("  layer pixels vs Photoshop's: {}", stats(&ours, ps, region).line());
        // CHECK_MESH=1: how Photoshop's stored 4x4 custom mesh should be read. Each reading is
        // rendered and scored against Photoshop's pixels (alpha only, so the point ordering is
        // invisible for opaque contents; the direction and the control/surface question aren't).
        if std::env::var("CHECK_MESH").is_ok()
            && let Some(w) = &sm.warp
            && let Some(m) = &w.mesh
            && m.us.len() == 2
            && m.vs.len() == 2
        {
            use photocraft_geom::warp::{BezierMesh, Warp};
            let b = w.bounds;
            let (bw, bh) = (b[2] - b[0], b[3] - b[1]);
            let score = |mesh: BezierMesh| {
                let mut sm2 = sm.clone();
                sm2.warp = Some(Warp::custom(mesh, b));
                let r = crate::render::place(&sm2, &src.surface, s.width, s.height);
                meshfit::alpha_diff(&r, ps, canvas)
            };
            // Points taken as the surface at (i/3, j/3) instead of as control points.
            let through = |pts: &BezierMesh| {
                BezierMesh::fit(
                    &|u, v| {
                        let (i, j) = ((u * 3.0).round() as usize, (v * 3.0).round() as usize);
                        pts.point(i.min(3), j.min(3))
                    },
                    vec![0.0, 1.0],
                    vec![0.0, 1.0],
                )
            };
            let ident = BezierMesh::identity(b, 1, 1);
            let mut reflected = m.clone();
            for (p, i) in reflected.points.iter_mut().zip(&ident.points) {
                *p = [2.0 * i[0] - p[0], 2.0 * i[1] - p[1]];
            }
            // The numerical inverse of the mesh (where a destination grid point samples from).
            let inverse = BezierMesh::fit(
                &|u, v| {
                    let (s2, t2) = m.param_at([b[0] + u * bw, b[1] + v * bh]);
                    [b[0] + s2 * bw, b[1] + t2 * bh]
                },
                vec![0.0, 1.0],
                vec![0.0, 1.0],
            );
            println!("    mesh as control points: {:.2}/255; as surface points: {:.2}/255", score(m.clone()), score(through(m)));
            // The transform may describe where the warped shape's bounding box goes, not the
            // original box: normalise the mesh's extent onto the contents box first.
            let bbox = |pts: &[[f64; 2]]| {
                pts.iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |a, p| [a[0].min(p[0]), a[1].min(p[1]), a[2].max(p[0]), a[3].max(p[1])])
            };
            let normalise = |pts: &BezierMesh, e: [f64; 4]| {
                let mut n = pts.clone();
                for p in &mut n.points {
                    *p = [b[0] + (p[0] - e[0]) * bw / (e[2] - e[0]), b[1] + (p[1] - e[1]) * bh / (e[3] - e[1])];
                }
                n
            };
            let control_extent = bbox(&m.points);
            let mut samples = Vec::new();
            for j in 0..=32 {
                for i in 0..=32 {
                    samples.push(m.eval(i as f64 / 32.0, j as f64 / 32.0));
                }
            }
            let surface_extent = bbox(&samples);
            println!(
                "    control-net extent {:?} / box {:?}; surface extent {:?}",
                control_extent.map(|v| v.round()),
                b.map(|v| v.round()),
                surface_extent.map(|v| v.round())
            );
            println!(
                "    normalised to the control-net extent: {:.2}/255; to the surface extent: {:.2}/255",
                score(normalise(m, control_extent)),
                score(normalise(m, surface_extent))
            );
            let t = through(m);
            let mut ts = Vec::new();
            for j in 0..=32 {
                for i in 0..=32 {
                    ts.push(t.eval(i as f64 / 32.0, j as f64 / 32.0));
                }
            }
            println!("    as surface points, normalised to its surface extent: {:.2}/255", score(normalise(&t, bbox(&ts))));
            println!("    reflected (first-order inverse) as control points: {:.2}/255; as surface points: {:.2}/255", score(reflected.clone()), score(through(&reflected)));
            println!("    numerical inverse mesh: {:.2}/255", score(inverse));
            let mut sm2 = sm.clone();
            sm2.warp = None;
            let r = crate::render::place(&sm2, &src.surface, s.width, s.height);
            println!("    no warp at all: {:.2}/255", meshfit::alpha_diff(&r, ps, canvas));
        }
        if !s.displace.is_empty() {
            let none = |_: usize| None;
            let without = crate::filters::apply_smart_filters(&placed, sm, canvas, &none, displace::DEFAULT);
            println!("  (without Displace it would be: {})", stats(&without, ps, region).line());
        }
        if let Some(o) = &out {
            let st = stem(&s.name, s.layer_id);
            save_png(ps, region, &o.join(format!("{st}.photoshop.png")));
            save_png(&ours, region, &o.join(format!("{st}.ours.png")));
        }
        if !s.displace.is_empty() && std::env::var("CHECK_VARIANTS").is_ok() {
            for sign in [1.0, -1.0] {
                for area in [Area::Canvas, Area::Layer] {
                    for bilinear in [true, false] {
                        for neutral in [128.0, 127.5] {
                            let var = Variant { sign, area, bilinear, neutral };
                            let r = crate::filters::apply_smart_filters(&placed, sm, canvas, &extra, var);
                            println!("  displace {var:?}: {}", stats(&r, ps, region).line());
                        }
                    }
                }
            }
        }
    }

    // The whole document against Photoshop's flattened image (both in the document's own colour
    // space; the composite tests blending, effects and masks, since every layer keeps Photoshop's
    // pixels here).
    println!("\ncomposite vs Photoshop's flattened image:");
    match std::fs::read(&tpl.path).map_err(|e| e.to_string()).and_then(|b| photocraft_psd::PsdFile::from_bytes(&b).map_err(|e| e.to_string())).and_then(|f| f.composite_rgba8().map_err(|e| e.to_string())) {
        Err(e) => println!("  unavailable: {e}"),
        Ok(img) => {
            let ours = crate::render::composite(doc, None, None);
            if (img.width, img.height) != (ours.w, ours.h) || img.left != 0 || img.top != 0 {
                println!("  size mismatch: Photoshop {}x{} at ({},{}), ours {}x{}", img.width, img.height, img.left, img.top, ours.w, ours.h);
            } else {
                let (mut sum, mut bad, mut n, mut alpha_mismatch) = (0.0f64, 0usize, 0usize, 0usize);
                let mut diff = vec![0u8; ours.px.len()];
                for ((a, b), d) in ours.px.chunks_exact(4).zip(img.data.chunks_exact(4)).zip(diff.chunks_exact_mut(4)) {
                    if a[3] != b[3] {
                        alpha_mismatch += 1;
                    }
                    let m = (0..3).map(|c| (a[c] as i32 - b[c] as i32).unsigned_abs()).max().unwrap_or(0);
                    sum += m as f64;
                    bad += (m > 8) as usize;
                    n += 1;
                    let v = (m * 4).min(255) as u8;
                    d.copy_from_slice(&[v, v, v, 255]);
                }
                println!("  mean diff {:.2}/255, {:.2}% of pixels off by >8, {} alpha mismatches ({}x{})", sum / n.max(1) as f64, bad as f64 / n.max(1) as f64 * 100.0, alpha_mismatch, ours.w, ours.h);
                if let Some(o) = &out {
                    save_rgba(&ours.px, ours.w, ours.h, &o.join("composite.ours.png"));
                    save_rgba(&img.data, img.width, img.height, &o.join("composite.photoshop.png"));
                    save_rgba(&diff, ours.w, ours.h, &o.join("composite.diff-x4.png"));
                }
            }
        }
    }
    0
}

fn dump(d: &photocraft_psd::Descriptor, ind: usize) {
    use photocraft_psd::descriptor::Value;
    for (k, v) in &d.items {
        let key = String::from_utf8_lossy(k.as_bytes()).into_owned();
        let pad = " ".repeat(ind);
        match v {
            Value::Descriptor(c) | Value::GlobalObject(c) => {
                println!("{pad}{key}: {{{}}}", String::from_utf8_lossy(c.class_id.as_bytes()));
                dump(c, ind + 2);
            }
            Value::RawData(b) | Value::Alias(b) | Value::Path(b) => println!("{pad}{key}: <{} bytes>", b.len()),
            Value::List(l) if l.len() > 40 => println!("{pad}{key}: [{} items] {:?}", l.len(), &l[..8]),
            Value::List(l) => {
                println!("{pad}{key}: [");
                for x in l {
                    match x {
                        Value::Descriptor(c) => dump(c, ind + 2),
                        other => println!("{pad}  {other:?}"),
                    }
                }
                println!("{pad}]");
            }
            Value::Text(t) => println!("{pad}{key}: {:?}", t.to_string_lossy()),
            Value::Enumerated { type_id, value } => println!("{pad}{key}: {}.{}", String::from_utf8_lossy(type_id.as_bytes()), String::from_utf8_lossy(value.as_bytes())),
            Value::ObjectArray(a) => println!("{pad}{key}: ObAr {:?}", a),
            other => println!("{pad}{key}: {other:?}"),
        }
    }
}
