//! The PSD "database": every `.psd` / `.psb` under a folder, parsed once and kept in memory.
//!
//! A template's id is its path relative to the folder, without the extension, with `/`
//! separators (`shirts/front.psd` → `shirts/front`). A file that changes on disk is re-parsed
//! the next time it's used; new files are picked up on the next lookup or listing.
//!
//! Loading also prepares each smart object for replacement: its Photoshop-only smart filters
//! are parsed (Displace), and a preset warp (Arc, Arc Lower, Flag…) is replaced by the mesh
//! recovered from Photoshop's own render of the layer (`meshfit`), cached on disk.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use photocraft_cms::{Builtin, ColorSpace, Intent, Profile, Transform};
use photocraft_color::ColorMode;
use photocraft_doc::{Document, LayerContent, SmartSource};
use photocraft_engine::smart_cmds;
use photocraft_geom::warp::{BezierMesh, Warp, WarpStyle};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::displace::Displace;
use crate::error::ApiError;
use crate::meshfit;

/// How a smart object's warp (Edit › Transform › Warp) is rendered.
#[derive(Clone, Debug, Serialize)]
pub struct WarpInfo {
    pub style: String,
    pub rendering: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fit: Option<FitInfo>,
}

/// Mean alpha difference (0..255) between a render of the original contents and Photoshop's
/// pixels for the layer: with PhotoCraft's own preset formula (`before`) and with the recovered
/// mesh (`after`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FitInfo {
    pub before: f64,
    pub after: f64,
    pub ms: u64,
    /// Which smoothness prior produced the kept mesh (`strong` or `weak`).
    #[serde(default)]
    pub prior: String,
}

/// A smart object layer of a template.
#[derive(Clone, Debug, Serialize)]
pub struct SmartInfo {
    pub name: String,
    /// Photoshop's layer id (`lyid`): what recorded actions and droplets target (`LyrI`).
    pub layer_id: Option<u32>,
    /// Pixel size of the smart object's contents: the replacement image is fitted to this.
    pub width: u32,
    pub height: u32,
    pub visible: bool,
    /// Layer opacity (0..1). Mockups often keep an opacity-0 "replace me" handle whose linked
    /// instances do the visible work; one with no instances changes nothing when replaced.
    pub opacity: f32,
    /// Mean opacity of the placeholder contents (0..1). Near 1 means the smart object is a
    /// full-surface texture (e.g. the whole shirt), so uploads should be opaque and match its
    /// aspect ratio (or pass `design_background`); low values mean a print-area placeholder.
    pub placeholder_coverage: f32,
    /// Other smart object layers sharing these contents (Photoshop instances): replacing one
    /// replaces them all, unless `instances=false`.
    pub instances: usize,
    /// The smart filters, in order. Ones marked "skipped" are Photoshop filters with no
    /// implementation: the replaced contents are rendered without them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warp: Option<WarpInfo>,
    /// Displace filters, parsed once (index in the filter stack, filter).
    #[serde(skip)]
    pub displace: Vec<(usize, Arc<Displace>)>,
    /// Path of indices from the document root down to the layer.
    #[serde(skip)]
    pub path: Vec<usize>,
    #[serde(skip)]
    pub source_key: u64,
}

pub struct Template {
    pub id: String,
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    pub doc: Arc<Document>,
    /// Downscaled copies of `doc` (level n = 1/2ⁿ), built on first use (`scale.rs`).
    scaled: RwLock<HashMap<u8, Arc<Document>>>,
    scale_lock: Mutex<()>,
    pub smart_objects: Vec<SmartInfo>,
    /// Smart objects whose contents aren't stored in the PSD (e.g. linked to a missing file).
    pub unavailable: Vec<String>,
    /// Document RGB profile → sRGB, for web output (`None` when the document is already sRGB, or
    /// isn't RGB: the compositor renders CMYK and Lab documents in sRGB).
    pub to_srgb: Option<Transform>,
    /// sRGB → document RGB profile, for the uploaded image.
    pub from_srgb: Option<Transform>,
    pub load_ms: u64,
}

#[derive(Serialize)]
pub struct TemplateSummary {
    pub id: String,
    pub file: String,
    pub width: u32,
    pub height: u32,
    pub mode: String,
    pub bits: u32,
    pub layers: usize,
    pub smart_objects: Vec<SmartInfo>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unavailable_smart_objects: Vec<String>,
    pub load_ms: u64,
}

/// One file of a listing: its summary, or why it couldn't be opened.
#[derive(Serialize)]
#[serde(untagged)]
pub enum ListEntry {
    Ok(TemplateSummary),
    Failed { id: String, error: String },
}

impl Template {
    /// The document at 1/2^`level` of its size (level 0: the document itself).
    pub fn document_at(&self, level: u8) -> Arc<Document> {
        let level = level.min(crate::scale::MAX_LEVEL);
        if level == 0 {
            return self.doc.clone();
        }
        if let Some(d) = self.scaled.read().unwrap_or_else(|e| e.into_inner()).get(&level) {
            return d.clone();
        }
        let _guard = self.scale_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(d) = self.scaled.read().unwrap_or_else(|e| e.into_inner()).get(&level) {
            return d.clone();
        }
        let t = Instant::now();
        let d = Arc::new(crate::scale::scale_document(&self.doc, level));
        tracing::info!(template = %self.id, level, width = d.size.width, height = d.size.height, ms = t.elapsed().as_millis() as u64, "scaled copy built");
        self.scaled.write().unwrap_or_else(|e| e.into_inner()).insert(level, d.clone());
        d
    }

    pub fn summary(&self) -> TemplateSummary {
        TemplateSummary {
            id: self.id.clone(),
            file: self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            width: self.doc.size.width,
            height: self.doc.size.height,
            mode: format!("{:?}", self.doc.mode),
            bits: self.doc.pixel_format().sample.bytes() as u32 * 8,
            layers: self.doc.layer_count(),
            smart_objects: self.smart_objects.clone(),
            unavailable_smart_objects: self.unavailable.clone(),
            load_ms: self.load_ms,
        }
    }

    /// The smart object with Photoshop layer id `id`.
    pub fn find_id(&self, id: u32) -> Result<Vec<&SmartInfo>, ApiError> {
        let found: Vec<_> = self.smart_objects.iter().filter(|s| s.layer_id == Some(id)).collect();
        if found.is_empty() {
            let ids: Vec<_> = self.smart_objects.iter().filter_map(|s| s.layer_id.map(|i| format!("{i} (\"{}\")", s.name))).collect();
            return Err(ApiError::not_found(format!("template \"{}\" has no smart object with layer id {id} (available: {})", self.id, ids.join(", "))));
        }
        Ok(found)
    }

    /// The layers to replace for `name`: exact name match first, then case-insensitive.
    pub fn find(&self, name: &str) -> Result<Vec<&SmartInfo>, ApiError> {
        let exact: Vec<_> = self.smart_objects.iter().filter(|s| s.name == name).collect();
        if !exact.is_empty() {
            return Ok(exact);
        }
        let loose: Vec<_> = self.smart_objects.iter().filter(|s| s.name.trim().eq_ignore_ascii_case(name.trim())).collect();
        if !loose.is_empty() {
            return Ok(loose);
        }
        if self.unavailable.iter().any(|n| n.eq_ignore_ascii_case(name.trim())) {
            return Err(ApiError::unprocessable(format!(
                "smart object \"{name}\" in template \"{}\" has no embedded contents (linked to a file that isn't in the PSD), so its size is unknown; embed it in Photoshop (Layer › Smart Objects › Embed Linked)",
                self.id
            )));
        }
        let names: Vec<_> = self.smart_objects.iter().map(|s| format!("\"{}\"", s.name)).collect();
        Err(ApiError::not_found(format!("template \"{}\" has no smart object named \"{name}\" (available: {})", self.id, names.join(", "))))
    }
}

/// Mean alpha of `s` over `r`, sampled on up to 64 rows.
fn coverage(s: &photocraft_raster::Surface, r: photocraft_geom::Rect) -> f32 {
    let fmt = s.format();
    if !fmt.alpha || r.is_empty() {
        return 1.0;
    }
    let n = fmt.channels();
    let step = (r.height() / 64).max(1) as i32;
    let (mut sum, mut count) = (0.0f64, 0usize);
    let mut y = r.y0;
    while y < r.y1 {
        let row = s.read_region(photocraft_geom::Rect::new(r.x0, y, r.x1, y + 1));
        sum += row.chunks_exact(n).map(|p| p[n - 1] as f64).sum::<f64>();
        count += row.len() / n;
        y += step;
    }
    if count == 0 { 1.0 } else { (sum / count as f64) as f32 }
}

fn source_key(src: &SmartSource) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match src {
        SmartSource::Linked { path } => (0u8, path).hash(&mut h),
        SmartSource::Embedded { bytes, .. } => (1u8, blake3::hash(bytes).as_bytes()).hash(&mut h),
    }
    h.finish()
}

/// Describes the smart filters and parses the Photoshop-only ones this service renders.
fn parse_filters(sm: &photocraft_doc::SmartObject) -> (Vec<String>, Vec<(usize, Arc<Displace>)>) {
    let mut filters = Vec::new();
    let mut displace = Vec::new();
    for (i, f) in sm.smart_filters.iter().enumerate() {
        if f.command != smart_cmds::UNSUPPORTED_FILTER {
            filters.push(f.command.strip_prefix("filter.").unwrap_or(&f.command).to_string());
            continue;
        }
        let name = f.params.get("name").and_then(|v| v.as_str()).unwrap_or("?").trim_end_matches('.').to_string();
        match Displace::from_params(&f.params) {
            Some(Ok(d)) => {
                filters.push(format!("{name} (Photoshop; implemented by this service)"));
                displace.push((i, Arc::new(d)));
            }
            Some(Err(e)) => filters.push(format!("{name} (Photoshop; skipped: {e})")),
            None => filters.push(format!("{name} (Photoshop; skipped: not implemented)")),
        }
    }
    (filters, displace)
}

// ---------- warp fitting + cache ----------

struct FileKey {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Serialize, Deserialize)]
struct WarpCache {
    version: u32,
    mesh: BezierMesh,
    fit: FitInfo,
}

fn cache_dir() -> Option<PathBuf> {
    let dir = std::env::var("CACHE_DIR").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/data/cache"));
    match std::fs::create_dir_all(&dir) {
        Ok(()) => Some(dir),
        Err(e) => {
            tracing::warn!(dir = %dir.display(), "no cache directory ({e}); fitted warps won't be kept between runs");
            None
        }
    }
}

fn cache_key(file: &FileKey, s: &SmartInfo, w: &Warp) -> String {
    let mut h = blake3::Hasher::new();
    h.update(file.path.to_string_lossy().as_bytes());
    h.update(&file.len.to_le_bytes());
    if let Some(m) = file.modified.and_then(|m| m.duration_since(UNIX_EPOCH).ok()) {
        h.update(&m.as_nanos().to_le_bytes());
    }
    let (a, b) = (meshfit::Options::from_env(meshfit::Options::STRONG), meshfit::Options::from_env(meshfit::Options::WEAK));
    h.update(
        format!("{:?}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}|{}|v{}", s.layer_id, s.path, w.bounds, w.style.id(), w.bend, w.h_distort, w.v_distort, w.vertical, a.smooth, a.identity, b.smooth, b.identity, meshfit::VERSION)
            .as_bytes(),
    );
    h.finalize().to_hex()[..24].to_string()
}

fn read_cache(key: &str) -> Option<WarpCache> {
    let p = cache_dir()?.join(format!("warp-{key}.json"));
    let c: WarpCache = serde_json::from_slice(&std::fs::read(p).ok()?).ok()?;
    (c.version == meshfit::VERSION && c.mesh.is_valid()).then_some(c)
}

fn write_cache(key: &str, c: &WarpCache) {
    let Some(dir) = cache_dir() else { return };
    let p = dir.join(format!("warp-{key}.json"));
    if let Err(e) = serde_json::to_vec(c).map_err(|e| e.to_string()).and_then(|b| std::fs::write(&p, b).map_err(|e| e.to_string())) {
        tracing::warn!(path = %p.display(), "can't write the warp cache: {e}");
    }
}

/// Photoshop's transform quad (`Trnf`) of a warped placed layer is where the bounding box of
/// the warp's control net lands, not where the original box would: verified on seven
/// custom-warped layers (0.2–0.9/255 against Photoshop's pixels, versus 16–190 when the box is
/// mapped). PhotoCraft maps the box, so the control net is normalised onto the box here.
/// Returns false when the mesh already spans the box (nothing to do) or isn't a single patch.
fn normalise_custom_mesh(w: &mut Warp) -> bool {
    let [b0, b1, b2, b3] = w.bounds;
    let (bw, bh) = (b2 - b0, b3 - b1);
    let Some(m) = &mut w.mesh else { return false };
    if m.us.len() != 2 || m.vs.len() != 2 || !(bw > 0.0 && bh > 0.0) {
        return false;
    }
    let e = m.points.iter().fold([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY], |a, p| [a[0].min(p[0]), a[1].min(p[1]), a[2].max(p[0]), a[3].max(p[1])]);
    let (ew, eh) = (e[2] - e[0], e[3] - e[1]);
    if !(ew > 0.0 && eh > 0.0) || e.iter().zip(&w.bounds).all(|(x, y)| (x - y).abs() < 1e-6) {
        return false;
    }
    for p in &mut m.points {
        *p = [b0 + (p[0] - e[0]) * bw / ew, b1 + (p[1] - e[1]) * bh / eh];
    }
    true
}

/// Prepares each smart object's warp: a custom mesh is normalised onto its box (see
/// [`normalise_custom_mesh`]), a preset is replaced by the mesh recovered from Photoshop's
/// render of the layer.
fn fit_warps(id: &str, doc: &mut Document, smart_objects: &mut [SmartInfo], file: &FileKey) {
    let canvas = doc.bounds();
    let fmt = doc.pixel_format();
    for s in smart_objects.iter_mut() {
        let Some(layer) = doc.layer_at(&s.path) else { continue };
        let LayerContent::Smart(sm) = &layer.content else { continue };
        let Some(w) = sm.warp.clone() else { continue };
        let style = format!("{:?}", w.style);
        if w.style == WarpStyle::Custom {
            let mut fixed = w.clone();
            if !normalise_custom_mesh(&mut fixed) {
                s.warp = Some(WarpInfo { style, rendering: "Photoshop's own mesh, stored in the PSD".into(), fit: None });
                continue;
            }
            // Keep whichever reading matches Photoshop's render, when the PSD holds one.
            let t = Instant::now();
            let verdict = sm
                .cache
                .as_ref()
                .filter(|c| !c.content_bounds().is_empty())
                .and_then(|ps| {
                    let (file_name, bytes) = smart_cmds::source_bytes(&doc.metadata, &sm.source)?;
                    let src = smart_cmds::source_image(&file_name, &bytes, fmt).ok()?;
                    let before = meshfit::alpha_diff(&crate::render::place(sm, &src.surface, s.width, s.height), ps, canvas);
                    let mut fixed_sm = sm.clone();
                    fixed_sm.warp = Some(fixed.clone());
                    let after = meshfit::alpha_diff(&crate::render::place(&fixed_sm, &src.surface, s.width, s.height), ps, canvas);
                    Some(FitInfo { before, after, ms: t.elapsed().as_millis() as u64, prior: "control-net extent".into() })
                });
            let apply = verdict.as_ref().is_none_or(|v| v.after <= v.before);
            if let Some(v) = &verdict {
                tracing::info!(template = id, layer = %s.name, before = v.before, after = v.after, applied = apply, "custom warp mesh normalised to its extent");
            }
            if apply
                && let Some(l) = doc.layer_at_mut(&s.path)
                && let LayerContent::Smart(sm) = &mut l.content
            {
                sm.warp = Some(fixed);
            }
            s.warp = Some(WarpInfo {
                style,
                rendering: if apply { "Photoshop's own mesh, stored in the PSD; its control net mapped onto the transform box".into() } else { "Photoshop's own mesh, stored in the PSD".into() },
                fit: verdict,
            });
            continue;
        }
        let sm = sm.clone();
        let approx = |why: &str| WarpInfo { style: style.clone(), rendering: format!("PhotoCraft's approximation of the preset ({why})"), fit: None };
        let Some(ps) = sm.cache.as_ref().filter(|c| !c.content_bounds().is_empty()) else {
            s.warp = Some(approx("the PSD holds no render of the layer to fit to"));
            continue;
        };
        let Some((file_name, bytes)) = smart_cmds::source_bytes(&doc.metadata, &sm.source) else {
            s.warp = Some(approx("the contents are unavailable"));
            continue;
        };
        let Ok(src) = smart_cmds::source_image(&file_name, &bytes, fmt) else {
            s.warp = Some(approx("the contents can't be decoded"));
            continue;
        };
        let key = cache_key(file, s, &w);
        let cached = read_cache(&key);
        let from_cache = cached.is_some();
        let WarpCache { mesh, fit, .. } = cached.unwrap_or_else(|| {
            let t = Instant::now();
            let init = BezierMesh::identity(w.bounds, 1, 1);
            let before = meshfit::alpha_diff(&crate::render::place(&sm, &src.surface, s.width, s.height), ps, canvas);
            let run = |opts: meshfit::Options| {
                let f = meshfit::fit(&sm, &src.surface, s.width, s.height, ps, canvas, w.bounds, &init, &opts);
                let mut fitted = sm.clone();
                fitted.warp = Some(Warp::custom(f.mesh.clone(), w.bounds));
                let after = meshfit::alpha_diff(&crate::render::place(&fitted, &src.surface, s.width, s.height), ps, canvas);
                tracing::debug!(template = id, layer = %s.name, ?opts, fit_scale_before = f.before, fit_scale_after = f.after, evals = f.evals, after, "warp fit pass");
                (f.mesh, after)
            };
            let (mut mesh, mut after) = run(meshfit::Options::from_env(meshfit::Options::STRONG));
            let mut prior = "strong";
            // S-shaped presets and distortions need the weak prior; so does any poor fit.
            let asymmetric = matches!(w.style, WarpStyle::Flag | WarpStyle::Wave | WarpStyle::Fish | WarpStyle::Rise | WarpStyle::Twist) || w.h_distort != 0.0 || w.v_distort != 0.0;
            if asymmetric || after > 6.0 {
                let (m2, a2) = run(meshfit::Options::from_env(meshfit::Options::WEAK));
                if a2 < after {
                    (mesh, after, prior) = (m2, a2, "weak");
                }
            }
            let c = WarpCache { version: meshfit::VERSION, mesh, fit: FitInfo { before, after, ms: t.elapsed().as_millis() as u64, prior: prior.into() } };
            write_cache(&key, &c);
            c
        });
        tracing::info!(template = id, layer = %s.name, style = %style, before = fit.before, after = fit.after, prior = %fit.prior, ms = fit.ms, cached = from_cache, "warp mesh fitted");
        if fit.after < fit.before {
            if let Some(l) = doc.layer_at_mut(&s.path)
                && let LayerContent::Smart(sm) = &mut l.content
            {
                sm.warp = Some(Warp::custom(mesh, w.bounds));
            }
            s.warp = Some(WarpInfo { style, rendering: "mesh recovered from Photoshop's render of the original contents".into(), fit: Some(fit) });
        } else {
            s.warp = Some(WarpInfo { style, rendering: "PhotoCraft's approximation of the preset (the fit didn't improve on it)".into(), fit: Some(fit) });
        }
    }
}

fn load(id: &str, path: &Path) -> Result<Template, String> {
    let t0 = Instant::now();
    let meta = std::fs::metadata(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let modified = meta.modified().ok();
    let bytes = std::fs::read(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "template.psd".into());
    let mut doc = photocraft_io::import(&name, &bytes).map_err(|e| format!("can't open {name}: {e}"))?.document;
    drop(bytes);

    let fmt = doc.pixel_format();
    let mut smart_objects = Vec::new();
    let mut unavailable = Vec::new();
    for (path, _, layer) in doc.walk() {
        let LayerContent::Smart(sm) = &layer.content else { continue };
        let source = smart_cmds::source_bytes(&doc.metadata, &sm.source)
            .and_then(|(file, bytes)| smart_cmds::source_image(&file, &bytes, fmt).ok())
            .filter(|img| img.bounds.width() > 0 && img.bounds.height() > 0);
        let (filters, displace) = parse_filters(sm);
        match source {
            Some(img) => smart_objects.push(SmartInfo {
                name: layer.name.clone(),
                layer_id: layer.psd_id,
                width: img.bounds.width() as u32,
                height: img.bounds.height() as u32,
                visible: layer.visible,
                opacity: layer.opacity,
                placeholder_coverage: coverage(&img.surface, img.bounds),
                instances: 0,
                filters,
                warp: None,
                displace,
                path,
                source_key: source_key(&sm.source),
            }),
            None => unavailable.push(layer.name.clone()),
        }
    }
    let counts = smart_objects.iter().fold(HashMap::<u64, usize>::new(), |mut m, s| {
        *m.entry(s.source_key).or_default() += 1;
        m
    });
    for s in &mut smart_objects {
        s.instances = counts[&s.source_key] - 1;
    }

    fit_warps(id, &mut doc, &mut smart_objects, &FileKey { path: path.to_path_buf(), len: meta.len(), modified });

    // Photoshop converts between profiles with relative colorimetric intent (its default).
    let (to_srgb, from_srgb) = match (doc.mode, doc.icc_profile.as_ref().and_then(|b| Profile::parse(b).ok())) {
        (ColorMode::Rgb, Some(p)) if p.color_space == ColorSpace::Rgb && !p.same_colors(Builtin::Srgb.profile()) => (
            Transform::new(&p, Builtin::Srgb.profile(), Intent::RelativeColorimetric, true).ok(),
            Transform::new(Builtin::Srgb.profile(), &p, Intent::RelativeColorimetric, true).ok(),
        ),
        _ => (None, None),
    };

    Ok(Template {
        id: id.to_string(),
        path: path.to_path_buf(),
        modified,
        doc: Arc::new(doc),
        scaled: RwLock::new(HashMap::new()),
        scale_lock: Mutex::new(()),
        smart_objects,
        unavailable,
        to_srgb,
        from_srgb,
        load_ms: t0.elapsed().as_millis() as u64,
    })
}

pub struct Store {
    dir: PathBuf,
    loaded: RwLock<HashMap<String, Arc<Template>>>,
    /// One lock per template id, so concurrent first requests don't parse the same file twice
    /// while different files still load in parallel.
    load_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir, loaded: RwLock::new(HashMap::new()), load_locks: Mutex::new(HashMap::new()) }
    }

    /// Every PSD/PSB under the folder, by id.
    pub fn scan(&self) -> Vec<(String, PathBuf)> {
        fn rec(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    rec(root, &p, out);
                    continue;
                }
                let ext = p.extension().map(|x| x.to_string_lossy().to_ascii_lowercase());
                if !matches!(ext.as_deref(), Some("psd" | "psb")) {
                    continue;
                }
                let Ok(rel) = p.strip_prefix(root) else { continue };
                let id = rel.with_extension("").components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
                out.push((id, p));
            }
        }
        let mut out = Vec::new();
        rec(&self.dir, &self.dir, &mut out);
        out.sort();
        out
    }

    fn cached_fresh(&self, id: &str) -> Option<Arc<Template>> {
        let t = self.loaded.read().ok()?.get(id).cloned()?;
        let modified = std::fs::metadata(&t.path).and_then(|m| m.modified()).ok();
        (modified.is_some() && modified == t.modified).then_some(t)
    }

    fn load_into_cache(&self, id: &str, path: &Path) -> Result<Arc<Template>, ApiError> {
        let lock = self.load_locks.lock().unwrap_or_else(|e| e.into_inner()).entry(id.to_string()).or_default().clone();
        let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = self.cached_fresh(id) {
            return Ok(t);
        }
        let t = Arc::new(load(id, path).map_err(ApiError::unprocessable)?);
        tracing::info!(template = id, ms = t.load_ms, smart_objects = t.smart_objects.len(), "template loaded");
        self.loaded.write().unwrap_or_else(|e| e.into_inner()).insert(id.to_string(), t.clone());
        Ok(t)
    }

    /// The template `id` (blocking: parses the PSD on first use or after it changed on disk).
    pub fn get(&self, id: &str) -> Result<Arc<Template>, ApiError> {
        let id = id.trim().trim_matches('/');
        let id = id.strip_suffix(".psd").or_else(|| id.strip_suffix(".psb")).unwrap_or(id);
        if let Some(t) = self.cached_fresh(id) {
            return Ok(t);
        }
        // Resolve through a directory scan, never by joining the id onto the folder path.
        let Some((_, path)) = self.scan().into_iter().find(|(i, _)| i == id) else {
            self.loaded.write().unwrap_or_else(|e| e.into_inner()).remove(id);
            return Err(ApiError::not_found(format!("no template \"{id}\" in {}", self.dir.display())));
        };
        self.load_into_cache(id, &path)
    }

    /// Loads (in parallel) every template, dropping ones whose file is gone.
    pub fn list(&self) -> Vec<ListEntry> {
        let files = self.scan();
        {
            let mut loaded = self.loaded.write().unwrap_or_else(|e| e.into_inner());
            loaded.retain(|id, _| files.iter().any(|(i, _)| i == id));
        }
        files
            .par_iter()
            .map(|(id, _)| match self.get(id) {
                Ok(t) => ListEntry::Ok(t.summary()),
                Err(e) => ListEntry::Failed { id: id.clone(), error: e.message },
            })
            .collect()
    }
}
