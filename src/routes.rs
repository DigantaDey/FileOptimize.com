//! HTTP route handlers.

use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Multipart, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;

use crate::detect::{detect, jpeg_is_cmyk, FileKind};
use crate::error::{AppError, AppResult};
use crate::processing::image::encode_image;
use crate::processing::pdf::{compress_pdf, gs_available, pdf_to_images, pdftoppm_available};
use crate::processing::pdf_gen::build_image_pdf;
use crate::processing::zip::write_zip_from_files;
use crate::processing::{
    base_name, check_dimensions, content_type_of, decode_checked, ext_of, parse_quality,
    pick_compress_format, read_header, Output,
};
use crate::sanitize::{extension_of, sanitize_filename};
use crate::state::{AppState, WorkDir};

/// DPI assumed when turning image pixels into PDF points (72 pt per inch).
const IMAGE_PDF_DPI: f64 = 100.0;
/// Quality used when a non-JPEG image must be re-encoded for embedding in a PDF.
const IMAGE_PDF_EMBED_QUALITY: u8 = 90;
/// Read size used when streaming a file off disk into the response body.
const STREAM_BUF_BYTES: usize = 128 * 1024;

/// Build the application router. Middleware (tracing, CORS, compression) is layered on
/// top by [`crate::create_app`].
pub fn router(state: AppState) -> Router {
    let body_limit = state.config.body_limit();
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/assets/style.css", get(style_css))
        .route("/assets/script.js", get(script_js))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        // `/upload` is kept as a legacy alias so existing clients keep working.
        .route(
            "/upload",
            post(process).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/api/process",
            post(process).layer(DefaultBodyLimit::max(body_limit)),
        )
        .fallback(not_found)
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Static / meta
// ---------------------------------------------------------------------------

const STATIC_HTML: &str = include_str!("../web/index.html");

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

#[derive(Serialize)]
struct Health {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health() -> impl IntoResponse {
    Json(Health {
        status: "ok",
        service: "fileoptimize",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Serialize)]
struct Tools {
    ghostscript: bool,
    poppler: bool,
}

#[derive(Serialize)]
struct Info {
    service: &'static str,
    version: &'static str,
    max_upload_size: u64,
    max_image_pixels: u32,
    max_concurrent: usize,
    default_quality: u8,
    pdf_raster_dpi: u32,
    tools: Tools,
}

async fn info(State(state): State<AppState>) -> impl IntoResponse {
    Json(Info {
        service: "fileoptimize",
        version: env!("CARGO_PKG_VERSION"),
        max_upload_size: state.config.max_upload_size,
        max_image_pixels: state.config.max_image_pixels,
        max_concurrent: state.config.max_concurrent,
        default_quality: state.config.compression_default_quality,
        pdf_raster_dpi: state.config.pdf_raster_dpi,
        tools: Tools {
            ghostscript: gs_available().await,
            poppler: pdftoppm_available().await,
        },
    })
}

async fn not_found() -> AppError {
    AppError::NotFound("resource not found".to_string())
}

// ---------------------------------------------------------------------------
// Main processing endpoint
// ---------------------------------------------------------------------------

/// The actions this service understands. Names are kept in sync with the UI and the
/// legacy Flask API so existing clients are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Compress,
    ImageToPdf,
    PdfToImage,
}

impl Action {
    fn parse(raw: &str) -> AppResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "compress" => Ok(Action::Compress),
            "image_to_pdf" | "image-to-pdf" => Ok(Action::ImageToPdf),
            "pdf_to_image" | "pdf-to-image" => Ok(Action::PdfToImage),
            other => Err(AppError::BadRequest(format!(
                "invalid action '{other}': expected compress, image_to_pdf or pdf_to_image"
            ))),
        }
    }
}

/// `POST /api/process` — `multipart/form-data` with fields:
/// `file` (binary), `action` (`compress` | `image_to_pdf` | `pdf_to_image`),
/// `quality` (1..100, optional — drives `compress`).
async fn process(State(state): State<AppState>, mut multipart: Multipart) -> AppResult<Response> {
    // Bound concurrency — the primary protection against memory pressure under load.
    // The permit is held for the whole transform and released on drop, including on
    // every error path; client download time is deliberately *not* counted.
    let permit = state
        .semaphore
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| AppError::Internal("processing gate closed".to_string()))?;

    let result = run_job(&state, &mut multipart).await;
    drop(permit);
    result
}

async fn run_job(state: &AppState, multipart: &mut Multipart) -> AppResult<Response> {
    let mut work = state.new_workdir().await?;
    let mut input_path: Option<PathBuf> = None;
    let mut action_raw = String::new();
    let mut quality_raw: Option<String> = None;
    let mut original_name = String::from("file");
    let mut original_size: u64 = 0;

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" => {
                original_name = sanitize_filename(field.file_name().unwrap_or("file"));
                let ext = extension_of(&original_name);
                let path = work.join(&format!("input.{ext}"));
                original_size = stream_to_disk(field, &path, state.config.max_upload_size).await?;
                if original_size == 0 {
                    return Err(AppError::BadRequest("uploaded file is empty".to_string()));
                }
                input_path = Some(path);
            }
            "action" => action_raw = field.text().await?,
            "quality" => quality_raw = Some(field.text().await?),
            // Unknown fields are drained and ignored rather than rejected, so stray
            // browser form extras can't fail an otherwise valid job.
            _ => {
                let _ = field.bytes().await?;
            }
        }
    }

    let input_path =
        input_path.ok_or_else(|| AppError::BadRequest("no file provided".to_string()))?;
    let action = Action::parse(&action_raw)?;
    let quality = parse_quality(
        quality_raw.as_deref(),
        state.config.compression_default_quality,
    )?;

    // Sniff only a header: enough to route the job, and it never loads a big PDF in full.
    let head = read_header(&input_path, 64).await?;
    let kind = detect(&head);

    let mut output = match action {
        Action::Compress => {
            compress(&input_path, &kind, quality, &original_name, &work, state).await?
        }
        Action::ImageToPdf => image_to_pdf(&input_path, &kind, &original_name, state).await?,
        Action::PdfToImage => {
            pdf_to_image(&input_path, &kind, &original_name, &work, state).await?
        }
    };

    // Must be sampled *before* the response consumes the output: a payload that is being
    // streamed off disk has to outlive this function, so its directory is handed to the
    // TTL sweeper instead of being deleted here. In-memory results are freed immediately.
    let streamed = output.is_streamed();
    let response = build_download_response(&mut output, original_size, &original_name)?;
    if streamed {
        work.keep();
    }
    drop(work);

    Ok(response)
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
// Actions
// ---------------------------------------------------------------------------

/// `compress` — images are re-encoded with a quality setting; PDFs go through Ghostscript.
async fn compress(
    input: &Path,
    kind: &FileKind,
    quality: u8,
    original_name: &str,
    work: &WorkDir,
    state: &AppState,
) -> AppResult<Output> {
    match kind {
        FileKind::Pdf => {
            if !gs_available().await {
                return Err(external_tool_missing("ghostscript", "PDF compression"));
            }
            let out_path = work.join("compressed.pdf");
            compress_pdf(input, &out_path, quality, tool_timeout(state)).await?;
            let base = base_name(original_name);
            Ok(Output::from_path(
                out_path,
                format!("{base}_compressed.pdf"),
                "application/pdf",
            ))
        }
        FileKind::Image(fmt) => {
            let data = tokio::fs::read(input).await?;
            let fmt = *fmt;
            let max_pixels = state.config.max_image_pixels;
            // Codecs are CPU-bound: run them on the blocking pool so the async runtime
            // keeps accepting and streaming other requests.
            let (bytes, target) = tokio::task::spawn_blocking(move || {
                let img = decode_checked(&data, fmt, max_pixels)?;
                let target = pick_compress_format(fmt, quality);
                Ok::<_, AppError>((encode_image(img, target, quality)?, target))
            })
            .await
            .map_err(join_err)??;

            let base = base_name(original_name);
            Ok(Output::in_memory(
                bytes,
                format!("{base}_optimized.{}", ext_of(target)),
                content_type_of(target),
            ))
        }
        FileKind::Unknown => Err(AppError::UnsupportedMediaType(
            "only images and PDFs can be compressed".to_string(),
        )),
    }
}

/// `image_to_pdf` — wrap a single image in a one-page PDF.
///
/// A baseline (non-CMYK) JPEG is embedded verbatim via `/DCTDecode`: no decode +
/// re-encode round-trip at all, which means zero pixel-buffer allocation and no second
/// lossy generation. Any other format is normalised to RGB JPEG first.
async fn image_to_pdf(
    input: &Path,
    kind: &FileKind,
    original_name: &str,
    state: &AppState,
) -> AppResult<Output> {
    let fmt = match kind {
        FileKind::Image(f) => *f,
        FileKind::Pdf => {
            return Err(AppError::BadRequest(
                "that is already a PDF — use compress instead".to_string(),
            ))
        }
        FileKind::Unknown => {
            return Err(AppError::UnsupportedMediaType(
                "image_to_pdf requires an image file".to_string(),
            ))
        }
    };

    let data = tokio::fs::read(input).await?;
    let max_pixels = state.config.max_image_pixels;
    let pdf = tokio::task::spawn_blocking(move || -> AppResult<Vec<u8>> {
        let (w, h) = ::image::ImageReader::with_format(std::io::Cursor::new(&data), fmt)
            .into_dimensions()?;
        check_dimensions(w, h, max_pixels)?;

        let jpeg: Vec<u8> = if fmt == ::image::ImageFormat::Jpeg && !jpeg_is_cmyk(&data) {
            data
        } else {
            let img = decode_checked(&data, fmt, max_pixels)?;
            encode_image(img, ::image::ImageFormat::Jpeg, IMAGE_PDF_EMBED_QUALITY)?
        };
        Ok(build_image_pdf(&jpeg, w, h, IMAGE_PDF_DPI))
    })
    .await
    .map_err(join_err)??;

    let base = base_name(original_name);
    Ok(Output::in_memory(
        pdf,
        format!("{base}.pdf"),
        "application/pdf",
    ))
}

/// `pdf_to_image` — rasterise pages. One page becomes a PNG, many become a streamed ZIP.
async fn pdf_to_image(
    input: &Path,
    kind: &FileKind,
    original_name: &str,
    work: &WorkDir,
    state: &AppState,
) -> AppResult<Output> {
    if !matches!(kind, FileKind::Pdf) {
        return Err(AppError::UnsupportedMediaType(
            "pdf_to_image requires a PDF file".to_string(),
        ));
    }
    if !pdftoppm_available().await {
        return Err(external_tool_missing(
            "poppler-utils",
            "PDF→image conversion",
        ));
    }

    let img_dir = work.join("pages");
    let files = pdf_to_images(
        input,
        &img_dir,
        state.config.pdf_raster_dpi,
        tool_timeout(state),
    )
    .await?;
    let base = base_name(original_name);

    if files.len() == 1 {
        let only = files.into_iter().next().expect("length checked above");
        let out_path = work.join(&format!("{base}.png"));
        // rename() can fail across devices; a copy is a safe fallback.
        if tokio::fs::rename(&only, &out_path).await.is_err() {
            tokio::fs::copy(&only, &out_path).await?;
            let _ = tokio::fs::remove_file(&only).await;
        }
        let _ = tokio::fs::remove_dir(&img_dir).await;
        return Ok(Output::from_path(
            out_path,
            format!("{base}.png"),
            "image/png",
        ));
    }

    let entries: Vec<(String, PathBuf)> = files
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("page_{:04}.png", i + 1), p.clone()))
        .collect();
    let zip_path = work.join(&format!("{base}_images.zip"));
    // Archiving is disk + CPU bound, so it also runs off the reactor.
    let (archive_path, entries) = (zip_path.clone(), entries);
    tokio::task::spawn_blocking(move || write_zip_from_files(&archive_path, &entries))
        .await
        .map_err(join_err)??;
    // The pages are inside the archive now; drop them so the temp dir holds one file.
    for f in &files {
        let _ = tokio::fs::remove_file(f).await;
    }
    let _ = tokio::fs::remove_dir(&img_dir).await;

    Ok(Output::from_path(
        zip_path,
        format!("{base}_images.zip"),
        "application/zip",
    ))
}

fn tool_timeout(state: &AppState) -> Duration {
    Duration::from_secs(state.config.tool_timeout_secs)
}

fn external_tool_missing(tool: &str, what: &str) -> AppError {
    AppError::ExternalToolMissing(format!(
        "{tool} is required for {what} but is not installed on this server"
    ))
}

fn join_err(e: tokio::task::JoinError) -> AppError {
    AppError::Internal(format!("worker task failed: {e}"))
}

// ---------------------------------------------------------------------------
// Response assembly
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

/// Insert a header, falling back to a known-good value if the payload can't be represented.
fn insert_header(headers: &mut HeaderMap, key: HeaderName, value: &str, fallback: &str) {
    let hv = HeaderValue::from_str(value)
        .or_else(|_| HeaderValue::from_str(fallback))
        .unwrap_or_else(|_| HeaderValue::from_static("0"));
    headers.insert(key, hv);
}
