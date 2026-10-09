//! mockup-server: an HTTP API that puts an uploaded image into a named smart object of a PSD
//! from a folder of templates, and returns the rendered result as a web image.
//!
//! Rendering is PhotoCraft's engine (pure Rust, CPU, multithreaded); no UI or GPU is involved.

mod check;
mod displace;
mod error;
mod filters;
mod meshfit;
mod render;
mod scale;
mod templates;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use tokio::sync::Semaphore;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

use crate::error::ApiError;
use crate::templates::Store;

struct AppState {
    store: Arc<Store>,
    /// Bounds how many renders run at once (each one is itself multithreaded).
    renders: Arc<Semaphore>,
}

type Shared = Arc<AppState>;

fn env_or<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=info".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("check") {
        std::process::exit(check::run(&args[2..]));
    }
    serve();
}

#[tokio::main]
async fn serve() {

    let dir = PathBuf::from(std::env::var("PSD_DIR").unwrap_or_else(|_| "/data/psds".into()));
    let port: u16 = env_or("PORT", 8080);
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let max_renders: usize = env_or("MAX_CONCURRENT_RENDERS", (cores / 4).clamp(2, 8));
    let max_upload_mb: usize = env_or("MAX_UPLOAD_MB", 50);
    let preload: bool = env_or("PRELOAD", true);

    let store = Arc::new(Store::new(dir.clone()));
    let state = Arc::new(AppState { store: store.clone(), renders: Arc::new(Semaphore::new(max_renders)) });
    // The test page (web/index.html, compiled in) and the example designs it uses. The API is
    // open to any origin, with the custom headers readable, so the page also works from a file
    // or another site.
    let examples_dir = PathBuf::from(std::env::var("EXAMPLES_DIR").unwrap_or_else(|_| "/data/examples".into()));
    let cors = CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any).expose_headers([
        header::HeaderName::from_static("server-timing"),
        header::HeaderName::from_static("x-image-width"),
        header::HeaderName::from_static("x-image-height"),
        header::HeaderName::from_static("x-replaced-layers"),
        header::HeaderName::from_static("x-render-scale"),
    ]);
    let app = Router::new()
        .route("/", get(|| async { Html(include_str!("../web/index.html")) }))
        .nest_service("/examples", ServeDir::new(examples_dir))
        .route("/health", get(|| async { "ok" }))
        .route("/templates", get(list_templates))
        .route("/templates/{*id}", get(get_template))
        .route("/render", post(render_handler))
        .route("/preview", get(preview_handler))
        .layer(cors)
        .layer(DefaultBodyLimit::max(max_upload_mb * 1024 * 1024))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!(%addr, cores, max_renders, max_upload_mb, "listening");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    if preload {
        // Parse every template (and fit its warps) in the background: the service answers at
        // once, and a request for a template still loading simply waits for that one.
        let s = store.clone();
        tokio::spawn(async move {
            let t0 = Instant::now();
            let loaded = tokio::task::spawn_blocking(move || s.list()).await.unwrap_or_default();
            for e in &loaded {
                if let templates::ListEntry::Failed { id, error } = e {
                    tracing::warn!(template = id, error, "template failed to load");
                }
            }
            tracing::info!(templates = loaded.len(), ms = t0.elapsed().as_millis() as u64, dir = %dir.display(), "templates preloaded");
        });
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server");
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::internal(format!("worker failed: {e}")))?
}

/// GET /templates: every PSD in the folder with its smart objects (loads any new or changed file).
async fn list_templates(State(st): State<Shared>) -> Result<Response, ApiError> {
    let store = st.store.clone();
    let list = blocking(move || Ok(store.list())).await?;
    Ok(Json(list).into_response())
}

/// GET /templates/{id}
async fn get_template(State(st): State<Shared>, Path(id): Path<String>) -> Result<Response, ApiError> {
    let store = st.store.clone();
    let t = blocking(move || store.get(&id)).await?;
    Ok(Json(t.summary()).into_response())
}

/// POST /render
///
/// Either `multipart/form-data` with an `image` file field plus text fields, or the raw image as
/// the request body with everything else in the query string. Parameters (field or query):
/// `template`, `smart_object`, and optionally `format`, `quality`, `width`, `height`, `fit`,
/// `background`, `lossless`, `instances`.
async fn render_handler(State(st): State<Shared>, Query(query): Query<HashMap<String, String>>, req: Request) -> Result<Response, ApiError> {
    let t0 = Instant::now();
    let mut params = query;
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().starts_with("multipart/form-data"));

    let image: Bytes = if is_multipart {
        let mut mp = Multipart::from_request(req, &()).await.map_err(|e| ApiError::bad_request(e.body_text()))?;
        let mut image = None;
        while let Some(field) = mp.next_field().await.map_err(|e| ApiError::bad_request(e.body_text()))? {
            let name = field.name().unwrap_or_default().to_string();
            if name == "image" || name == "file" {
                if let Some(f) = field.file_name() {
                    params.insert("filename".into(), f.to_string());
                }
                image = Some(field.bytes().await.map_err(|e| ApiError::bad_request(e.body_text()))?);
            } else {
                let text = field.text().await.map_err(|e| ApiError::bad_request(e.body_text()))?;
                params.insert(name, text);
            }
        }
        image.ok_or_else(|| ApiError::bad_request("missing the `image` file field"))?
    } else {
        Bytes::from_request(req, &()).await.map_err(|e| ApiError::bad_request(e.body_text()))?
    };
    if image.is_empty() {
        return Err(ApiError::bad_request("no image: send it as the `image` multipart field, or as the request body"));
    }

    let param = |k: &str| params.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let template = param("template").ok_or_else(|| ApiError::bad_request("missing `template` (a PSD id from GET /templates)"))?;
    let target = match (param("smart_object").or_else(|| param("layer")), param("layer_id")) {
        (_, Some(id)) => render::Target::LayerId(id.parse().map_err(|_| ApiError::bad_request("`layer_id` must be a number (Photoshop's layer id)"))?),
        (Some(name), None) => render::Target::Name(name),
        (None, None) => return Err(ApiError::bad_request("missing `smart_object` (the smart object layer's name) or `layer_id` (its Photoshop layer id)")),
    };
    let opts = render::Options::from_params(&params)?;
    let upload_name = param("filename").unwrap_or_else(|| "upload".into());

    let permit = st.renders.clone().acquire_owned().await.map_err(|_| ApiError::internal("shutting down"))?;
    let queued_ms = t0.elapsed().as_millis() as u64;
    let store = st.store.clone();
    let (tpl_id, out) = blocking(move || {
        let _permit = permit;
        let tpl = store.get(&template)?;
        let out = render::render(&tpl, &target, &image, &upload_name, &opts)?;
        Ok((tpl.id.clone(), out))
    })
    .await?;

    let total = t0.elapsed().as_millis() as u64;
    let timing = out.timings.iter().map(|(k, v)| format!("{k};dur={v}")).chain([format!("queue;dur={queued_ms}"), format!("total;dur={total}")]).collect::<Vec<_>>().join(", ");
    tracing::info!(template = tpl_id, replaced = out.replaced, width = out.width, height = out.height, bytes = out.bytes.len(), queued_ms, total_ms = total, "{timing}");

    Ok(image_response(out, &timing))
}

/// GET /preview?template=…&source=photoshop|photocraft (+ output options): the template as stored
/// by Photoshop (its saved composite) or as rendered by this service with nothing replaced.
async fn preview_handler(State(st): State<Shared>, Query(params): Query<HashMap<String, String>>) -> Result<Response, ApiError> {
    let template = params.get("template").cloned().ok_or_else(|| ApiError::bad_request("missing `template`"))?;
    let photoshop = match params.get("source").map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("photoshop") => true,
        Some("photocraft" | "service") => false,
        Some(s) => return Err(ApiError::bad_request(format!("unknown source \"{s}\" (use photoshop or photocraft)"))),
    };
    let opts = render::Options::from_params(&params)?;
    let permit = st.renders.clone().acquire_owned().await.map_err(|_| ApiError::internal("shutting down"))?;
    let store = st.store.clone();
    let out = blocking(move || {
        let _permit = permit;
        let tpl = store.get(&template)?;
        if photoshop { render::photoshop_composite(&tpl, &opts) } else { render::render_unchanged(&tpl, &opts) }
    })
    .await?;
    let timing = out.timings.iter().map(|(k, v)| format!("{k};dur={v}")).collect::<Vec<_>>().join(", ");
    Ok(image_response(out, &timing))
}

fn image_response(out: render::Output, timing: &str) -> Response {
    let mut res = Response::new(Body::from(out.bytes));
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(out.format.content_type()));
    if let Ok(v) = HeaderValue::from_str(timing) {
        h.insert("server-timing", v);
    }
    h.insert("x-image-width", out.width.into());
    h.insert("x-image-height", out.height.into());
    h.insert("x-replaced-layers", out.replaced.into());
    if let Ok(v) = HeaderValue::from_str(&format!("{}", out.render_scale)) {
        h.insert("x-render-scale", v);
    }
    res
}
