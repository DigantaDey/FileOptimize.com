//! HTTP transport in front of [`crate::engine`].
//!
//! This module deliberately contains *no* processing policy: every limit, validation
//! and transform decision lives in the engine, so an HTTP client, a legacy client and
//! an embedded Rust caller all get identical behaviour. What is here is form parsing,
//! streaming, headers, status codes and API-key authentication.
//!
//! Routes come in two flavours:
//! - `/`, `/assets/*`, `/api/health`, `/api/info`, `/api/process`, `/upload` — the
//!   original (Flask-compatible) surface the bundled UI uses;
//! - `/api/v1/*` — the same engine behind a versioned, machine-oriented contract:
//!   optional API keys, a JSON descriptor instead of a bare download, results that can
//!   be fetched again, and a published OpenAPI document.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path as PathParam, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, MethodRouter},
    Json, Router,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::engine::{Action, Input, Job, JobId, JobOutcome};
use crate::error::{AppError, AppResult};
use crate::processing::Output;
use crate::sanitize::extension_of;
use crate::state::AppState;

/// Read size used when streaming a file off disk into the response body.
const STREAM_BUF_BYTES: usize = 128 * 1024;

/// Build the application router. Middleware (tracing, CORS, compression) is layered on
/// top by [`crate::create_app`].
pub fn router(state: AppState) -> Router {
    let body_limit = state.config.body_limit();
    // Every upload route gets the same explicit cap: the file size limit plus room for
    // the multipart framing and the small form fields riding with it.
    let limited = |route: MethodRouter<AppState>| route.layer(DefaultBodyLimit::max(body_limit));

    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/assets/style.css", get(style_css))
        .route("/assets/script.js", get(script_js))
        // Health and info stay open even when the API is key-protected: orchestrators
        // and load balancers must probe without a secret.
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        .route("/api/openapi.json", get(openapi))
        // `/upload` is kept as a legacy alias so existing clients keep working.
        .route("/upload", limited(post(process_legacy)))
        .route("/api/process", limited(post(process_legacy)))
        // Versioned surface for other applications: identical transforms, plus API-key
        // authentication and the option to retain the result.
        .route("/api/v1/health", get(health))
        .route("/api/v1/info", get(info))
        .route("/api/v1/openapi.json", get(openapi))
        .route("/api/v1/process", limited(post(process_v1)))
        .route("/api/v1/results/:id", get(get_result).delete(delete_result))
        .fallback(not_found)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Static / meta
// ---------------------------------------------------------------------------

const STATIC_HTML: &str = include_str!("../web/index.html");

/// The published API contract, embedded at build time from `web/openapi.json` — the
/// spec and the routes above ship in the same binary, so they cannot drift.
const OPENAPI_JSON: &str = include_str!("../web/openapi.json");

async fn index() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        STATIC_HTML,
    )
}

async fn style_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../web/style.css"),
    )
}

async fn script_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../web/script.js"),
    )
}

async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        OPENAPI_JSON,
    )
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    /// Transforms running right now (out of `max_concurrent`).
    running_jobs: usize,
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    Json(Health {
        status: "ok",
        service: "fileoptimize",
        version: env!("CARGO_PKG_VERSION"),
        running_jobs: state.engine.running_jobs(),
    })
}

/// Everything a client program needs in order to pre-validate a request: the limits,
/// the accepted actions, and whether this host has the PDF tools installed.
#[derive(Serialize)]
struct Info {
    service: &'static str,
    version: &'static str,
    actions: [&'static str; 3],
    quality_range: [u8; 2],
    max_upload_size: u64,
    max_image_pixels: u32,
    max_concurrent: usize,
    default_quality: u8,
    pdf_raster_dpi: u32,
    tool_timeout_secs: u64,
    /// Total working memory all concurrent jobs may hold at once.
    memory_budget_mb: usize,
    /// The part of that budget not currently reserved.
    memory_available_mb: usize,
    /// How long a retained result survives.
    result_ttl_secs: u64,
    /// Whether `/api/v1/*` demands an API key.
    auth_required: bool,
    tools: crate::engine::Tools,
}

async fn info(State(state): State<AppState>) -> impl IntoResponse {
    let cfg = state.engine.config();
    Json(Info {
        service: "fileoptimize",
        version: env!("CARGO_PKG_VERSION"),
        actions: [
            Action::Compress.as_str(),
            Action::ImageToPdf.as_str(),
            Action::PdfToImage.as_str(),
        ],
        quality_range: [1, 100],
        max_upload_size: cfg.max_upload_size,
        max_image_pixels: cfg.max_image_pixels,
        max_concurrent: cfg.max_concurrent,
        default_quality: cfg.compression_default_quality,
        pdf_raster_dpi: cfg.pdf_raster_dpi,
        tool_timeout_secs: cfg.tool_timeout_secs,
        memory_budget_mb: cfg.memory_budget_mb,
        memory_available_mb: state.engine.available_memory_mb(),
        result_ttl_secs: cfg.cleanup_ttl_secs,
        auth_required: cfg.requires_api_key(),
        tools: state.engine.tools().await,
    })
}

async fn not_found() -> AppError {
    AppError::NotFound("resource not found".to_string())
}

// ---------------------------------------------------------------------------
// Processing
// ---------------------------------------------------------------------------

/// `POST /api/process` and `POST /upload` — unchanged legacy contract: a file in, a
/// file out.
async fn process_legacy(
    State(state): State<AppState>,
    multipart: Multipart,
) -> AppResult<Response> {
    respond(run_upload(&state, multipart).await?)
}

/// `POST /api/v1/process` — the same form fields, additionally: API-key authentication
/// and `store=1` to receive a JSON descriptor with a re-fetchable id.
async fn process_v1(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    multipart: Multipart,
) -> AppResult<Response> {
    authorize(&state, &headers, &QueryMap::from(query))?;
    let outcome = run_upload(&state, multipart).await?;
    // `store=1` is answered with JSON; everything else with the file itself.
    if outcome.id.is_some() {
        return Ok(retained_json(outcome));
    }
    respond(outcome)
}

/// The reply for a retained job: an id, what happened to the file, and where to get it.
fn retained_json(outcome: JobOutcome) -> Response {
    let JobOutcome {
        output,
        original_name,
        original_size,
        quality,
        id,
        meta,
    } = outcome;
    let id = id
        .as_ref()
        .map(|i| i.as_str().to_string())
        .unwrap_or_default();
    let saved = output.filename.clone();
    let kind = output.content_type.clone();
    let output_size = output.size();
    let action = meta
        .as_ref()
        .map(|m| m.action.clone())
        .unwrap_or_else(|| Action::Compress.as_str().to_string());
    let max_age_secs = meta.map(|m| m.max_age_secs).unwrap_or(0);
    Json(Retained {
        ok: true,
        id: id.clone(),
        action,
        quality,
        original_name,
        original_size,
        filename: saved,
        content_type: kind,
        output_size,
        saved_bytes: original_size.saturating_sub(output_size),
        download_path: format!("/api/v1/results/{id}"),
        max_age_secs,
    })
    .into_response()
}

/// The reply for a normal (non-retained) job: the file itself.
fn respond(mut outcome: JobOutcome) -> AppResult<Response> {
    // Disjoint field borrows: the payload is mutable (the body takes it), the sizes are
    // read alongside it.
    build_download_response(
        &mut outcome.output,
        outcome.original_size,
        &outcome.original_name,
    )
}

/// The reply a client gets when it asked for the result to be retained.
#[derive(Serialize)]
struct Retained {
    ok: bool,
    id: String,
    action: String,
    quality: u8,
    original_name: String,
    original_size: u64,
    filename: String,
    content_type: String,
    output_size: u64,
    saved_bytes: u64,
    download_path: String,
    max_age_secs: u64,
}

/// Read the multipart form, hand the file to the engine, hand the result back.
///
/// The scratch dir is handed to the engine, which decides whether it survives the
/// request (a streamed payload must outlive the response; everything else is deleted
/// here, including on every error path).
async fn run_upload(state: &AppState, mut multipart: Multipart) -> AppResult<JobOutcome> {
    let work = state.engine.new_workdir().await?;
    let mut input_path: Option<PathBuf> = None;
    let mut action_raw = String::new();
    let mut quality_raw: Option<String> = None;
    let mut client_name = String::from("file");
    let mut store = false;

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                client_name = field.file_name().unwrap_or("file").to_string();
                let ext = extension_of(&client_name);
                let path = work.join(&format!("input.{ext}"));
                // The engine re-checks size and emptiness at its own boundary, so the
                // transport only has to stop a request that overshoots the cap mid-stream
                // — which is exactly what stream_to_disk does.
                stream_to_disk(field, &path, state.engine.config().max_upload_size).await?;
                input_path = Some(path);
            }
            "action" => action_raw = field.text().await?,
            "quality" => quality_raw = Some(field.text().await?),
            // `store` keeps the result addressable at `GET /api/v1/results/{id}` for the
            // configured TTL, which is how a client that cannot stream a download — a
            // queue consumer, a spreadsheet, a mobile app on a flaky link — uses us.
            "store" => {
                let raw = field.text().await?;
                store = matches!(
                    raw.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                );
            }
            // Unknown fields are drained and ignored rather than rejected, so stray
            // browser form extras cannot fail an otherwise valid job.
            _ => {
                let _ = field.bytes().await?;
            }
        }
    }

    let input_path =
        input_path.ok_or_else(|| AppError::BadRequest("no file provided".to_string()))?;

    let job = Job::from_fields(
        Some(action_raw.as_str()),
        quality_raw.as_deref(),
        Some(client_name.as_str()),
        Input::Uploaded {
            dir: work,
            path: input_path,
        },
        store,
        state.engine.config().compression_default_quality,
    )?;

    state.engine.run(job).await
}

/// Copy one multipart field to disk in chunks, enforcing the size cap as it goes.
/// The upload is never fully buffered in memory.
async fn stream_to_disk(
    mut field: axum::extract::multipart::Field<'_>,
    path: &Path,
    max_bytes: u64,
) -> AppResult<u64> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut written: u64 = 0;
    while let Some(chunk) = field.chunk().await? {
        let n = chunk.len() as u64;
        if written.saturating_add(n) > max_bytes {
            let _ = file.shutdown().await;
            return Err(AppError::PayloadTooLarge(format!(
                "file exceeds the maximum upload size of {max_bytes} bytes"
            )));
        }
        file.write_all(&chunk).await?;
        written += n;
    }
    file.flush().await?;
    Ok(written)
}

// ---------------------------------------------------------------------------
// Retained results
// ---------------------------------------------------------------------------

/// `GET /api/v1/results/{id}` — stream back a result the client asked to keep.
async fn get_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    PathParam(raw): PathParam<String>,
) -> AppResult<Response> {
    authorize(&state, &headers, &QueryMap::from(query))?;
    let id = JobId::parse(&raw)?;
    let (mut output, meta) = state.engine.result(&id).await?;
    build_download_response(&mut output, meta.original_size, &meta.original_name)
}

/// `DELETE /api/v1/results/{id}` — drop it early. A second delete is a 404, not an
/// error, because the first one already succeeded.
async fn delete_result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    PathParam(raw): PathParam<String>,
) -> AppResult<StatusCode> {
    authorize(&state, &headers, &QueryMap::from(query))?;
    let id = JobId::parse(&raw)?;
    if state.engine.discard(&id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(
            "no retained result with that id".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Authentication
// ---------------------------------------------------------------------------

/// API-key gate for the versioned surface. A no-op unless `API_KEYS` is configured, so
/// a single-tenant deployment (or anything behind a gateway that authenticates) is
/// unaffected by the code path.
fn authorize(state: &AppState, headers: &HeaderMap, query: &QueryMap) -> AppResult<()> {
    let cfg = state.engine.config();
    if !cfg.requires_api_key() {
        return Ok(());
    }
    let presented = key_from_headers(headers).or_else(|| query.api_key.clone());
    match presented {
        Some(key) if cfg.accepts_api_key(&key) => Ok(()),
        Some(_) => Err(AppError::Unauthorized(
            "the presented API key is not recognised".to_string(),
        )),
        None => Err(AppError::Unauthorized(
            "an API key is required: send `x-api-key: <key>` or `Authorization: Bearer <key>`"
                .to_string(),
        )),
    }
}

fn key_from_headers(headers: &HeaderMap) -> Option<String> {
    let api_key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    api_key.or_else(|| {
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    })
}

/// `?api_key=` is accepted for clients that cannot set a header (server-rendered links,
/// simple form posts). Documented as the less preferred option, since query strings end
/// up in access logs.
#[derive(Default)]
struct QueryMap {
    api_key: Option<String>,
}

impl From<HashMap<String, String>> for QueryMap {
    fn from(map: HashMap<String, String>) -> Self {
        QueryMap {
            api_key: map.get("api_key").cloned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Response building
// ---------------------------------------------------------------------------

fn build_download_response(
    out: &mut Output,
    original_size: u64,
    original_name: &str,
) -> AppResult<Response> {
    let output_size = out.size();

    let mut headers = HeaderMap::new();
    insert_header(
        &mut headers,
        header::CONTENT_TYPE,
        &out.content_type,
        "application/octet-stream",
    );
    insert_header(
        &mut headers,
        header::CONTENT_DISPOSITION,
        &format!("attachment; filename=\"{}\"", out.filename),
        "attachment",
    );
    insert_header(
        &mut headers,
        HeaderName::from_static("x-original-size"),
        &original_size.to_string(),
        "0",
    );
    insert_header(
        &mut headers,
        HeaderName::from_static("x-output-size"),
        &output_size.to_string(),
        "0",
    );
    insert_header(
        &mut headers,
        HeaderName::from_static("x-original-filename"),
        original_name,
        "file",
    );
    insert_header(
        &mut headers,
        header::CONTENT_LENGTH,
        &output_size.to_string(),
        "0",
    );
    insert_header(
        &mut headers,
        header::ACCESS_CONTROL_EXPOSE_HEADERS,
        "content-disposition, x-original-size, x-output-size, x-original-filename",
        "content-disposition",
    );

    let body = if let Some(bytes) = out.data.take() {
        Body::from(bytes)
    } else if let Some(path) = out.path.take() {
        // Stream the file off disk in fixed-size slices instead of buffering it.
        let file = std::fs::File::open(&path)?;
        Body::from_stream(tokio_util::io::ReaderStream::with_capacity(
            tokio::fs::File::from_std(file),
            STREAM_BUF_BYTES,
        ))
    } else {
        Body::empty()
    };

    Ok((StatusCode::OK, headers, body).into_response())
}

fn insert_header(
    headers: &mut HeaderMap,
    name: HeaderName,
    value: &str,
    fallback: &'static str,
) {
    let value = if value.is_empty() { fallback } else { value };
    match HeaderValue::from_str(value) {
        Ok(v) => {
            headers.insert(name, v);
        }
        Err(e) => {
            // A value we built ourselves should always be header-safe; if it is not, the
            // ASCII fallback keeps the response well-formed instead of failing it.
            tracing::warn!(error = %e, "refused to emit header value");
            headers.insert(name, HeaderValue::from_static(fallback));
        }
    }
}
