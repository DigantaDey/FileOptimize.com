//! End-to-end HTTP tests: real multipart requests through the real router, asserting on
//! status codes, headers, returned bytes and — importantly — on temp-directory leakage.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{header, HeaderName, HeaderValue, Request, StatusCode};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb};
use tower::ServiceExt;

use fileoptimize::config::Config;
use fileoptimize::processing::pdf::{gs_available, pdftoppm_available};
use fileoptimize::processing::pdf_gen::build_image_pdf;
use fileoptimize::routes;
use fileoptimize::state::AppState;

const BOUNDARY: &str = "----FileOptimizeTestBoundary7e1a";

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Distinct scratch dirs for the tool-facing tests, which run in parallel.
static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

/// Test config: an isolated temp root and small caps, so limit paths are cheap to hit.
fn test_config(max_upload: u64, max_pixels: u32) -> Config {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "fo-api-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("create test tmp root");

    Config {
        host: "127.0.0.1".into(),
        port: 0,
        tmp_dir: root,
        max_upload_size: max_upload,
        max_image_pixels: max_pixels,
        cleanup_ttl_secs: 3_600,
        max_concurrent: 2,
        compression_default_quality: 60,
        pdf_raster_dpi: 96,
        tool_timeout_secs: 60,
        // Generous, so only the dedicated memory test ever feels it.
        memory_budget_mb: 512,
        // Open by default: the auth tests set this explicitly.
        api_keys: Vec::new(),
    }
}

/// Build the router under test, plus the temp root it was configured with.
fn build_app(cfg: Config) -> (axum::Router, PathBuf) {
    let root = cfg.tmp_dir.clone();
    (routes::router(AppState::new(cfg)), root)
}

fn multipart(fields: &[(&str, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, value) in fields {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match filename {
            Some(f) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(value);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Request to an arbitrary path with optional extra headers — the v1 tests need to
/// target `/api/v1/*` and to attach API keys.
fn request(method: &str, path: &str, body: Vec<u8>, extra: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(path);
    if method == "POST" {
        builder = builder.header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={BOUNDARY}")).unwrap(),
        );
    }
    for (name, value) in extra {
        builder = builder.header(*name, HeaderValue::from_str(value).unwrap());
    }
    builder.body(Body::from(body)).unwrap()
}

fn post_process(body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/process")
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={BOUNDARY}")).unwrap(),
        )
        .body(Body::from(body))
        .unwrap()
}

type Reply = (StatusCode, axum::http::HeaderMap, Vec<u8>);

async fn send(app: &axum::Router, req: Request<Body>) -> Reply {
    let resp = app.clone().oneshot(req).await.expect("service call");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 16 * 1024 * 1024)
        .await
        .expect("read body")
        .to_vec();
    (status, headers, bytes)
}

fn hdr(h: &axum::http::HeaderMap, name: header::HeaderName) -> String {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string()
}

fn err_of(bytes: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_default()
}

fn gradient() -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    ImageBuffer::from_fn(24, 16, |x, y| Rgb([(x * 9) as u8, (y * 13) as u8, 128]))
}

fn png() -> Vec<u8> {
    let img = gradient();
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), 24, 16, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn jpeg() -> Vec<u8> {
    let img = gradient();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 92)
        .write_image(img.as_raw(), 24, 16, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A PDF built by our own writer — structurally valid, so Ghostscript/Poppler accept it.
fn valid_pdf() -> Vec<u8> {
    build_image_pdf(&jpeg(), 24, 16, 100.0)
}

/// Number of entries left in the temp root (each is one leaked work dir).
fn leftovers(root: &std::path::Path) -> usize {
    std::fs::read_dir(root).map(|rd| rd.count()).unwrap_or(0)
}

// ------------------------------------------------------------------ meta / static

#[tokio::test]
async fn health_and_info_report_the_limits() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let (status, _, body) = send(
        &app,
        Request::get("/api/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["status"].as_str(), Some("ok"));
    assert_eq!(v["service"].as_str(), Some("fileoptimize"));

    let (status, _, body) =
        send(&app, Request::get("/api/info").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["max_upload_size"].as_u64(), Some(1024 * 1024));
    assert_eq!(v["max_image_pixels"].as_u64(), Some(1_000_000));
    assert_eq!(v["max_concurrent"].as_u64(), Some(2));
    assert_eq!(v["default_quality"].as_u64(), Some(60));
    assert!(v["tools"]["ghostscript"].is_boolean());
    assert!(v["tools"]["poppler"].is_boolean());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn the_router_with_middleware_serves_the_page() {
    // Exercises the full layer stack (tracing → CORS → compression) via create_app.
    let cfg = test_config(1024 * 1024, 1_000_000);
    let root = cfg.tmp_dir.clone();
    let app = fileoptimize::create_app(AppState::new(cfg));
    let resp = app
        .oneshot(Request::get("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE].to_str().unwrap(),
        "text/html; charset=utf-8"
    );
    let body = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("<!DOCTYPE html>"));
    assert!(text.contains("FileOptimize"));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn serves_the_single_page_ui() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));

    for (uri, needle, ctype) in [
        ("/", "<!DOCTYPE html>", "text/html"),
        ("/index.html", "id=\"upload-form\"", "text/html"),
        ("/assets/style.css", "--bg:", "text/css"),
        (
            "/assets/script.js",
            "/api/process",
            "application/javascript",
        ),
    ] {
        let (status, headers, body) =
            send(&app, Request::get(uri).body(Body::empty()).unwrap()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains(needle), "{uri} missing {needle:?}");
        assert!(
            hdr(&headers, header::CONTENT_TYPE).starts_with(ctype),
            "{uri} content-type: {}",
            hdr(&headers, header::CONTENT_TYPE)
        );
    }

    // Unknown paths are JSON 404s, never a silent redirect to the page.
    let (status, _, body) = send(&app, Request::get("/nope").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_of(&body), "resource not found");

    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------ validation

#[tokio::test]
async fn rejects_requests_without_a_file() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[("action", None, b"compress")]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err_of(&bytes), "no file provided");
    assert_eq!(leftovers(&root), 0, "temp dirs leaked");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_empty_uploads() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("empty.png"), b""),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err_of(&bytes), "uploaded file is empty");
    assert_eq!(leftovers(&root), 0, "temp dirs leaked");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_unknown_actions() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[("action", None, b"rm_-rf"), ("file", Some("a.png"), &png())]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err_of(&bytes).contains("invalid action"),
        "{}",
        err_of(&bytes)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_out_of_range_quality() {
    for (q, rejected) in [
        ("0", true),
        ("101", true),
        ("ninety", true),
        ("-5", true),
        ("50", false),
    ] {
        let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
        let body = multipart(&[
            ("action", None, b"compress"),
            ("quality", None, q.as_bytes()),
            ("file", Some("a.png"), &png()),
        ]);
        let (status, _, bytes) = send(&app, post_process(body)).await;
        if rejected {
            assert_eq!(status, StatusCode::BAD_REQUEST, "quality {q} was accepted");
            assert!(err_of(&bytes).contains("quality"), "{}", err_of(&bytes));
        } else {
            assert_eq!(status, StatusCode::OK, "quality {q}: {}", err_of(&bytes));
        }
        assert_eq!(leftovers(&root), 0, "temp dirs leaked for quality {q}");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn rejects_non_image_for_image_to_pdf() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"image_to_pdf"),
        (
            "file",
            Some("notes.txt"),
            b"plain text, definitely not an image",
        ),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(err_of(&bytes).contains("image"), "{}", err_of(&bytes));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_pdf_for_image_to_pdf() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"image_to_pdf"),
        ("file", Some("doc.pdf"), &valid_pdf()),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        err_of(&bytes).contains("already a PDF"),
        "{}",
        err_of(&bytes)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_non_pdf_for_pdf_to_image() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"pdf_to_image"),
        ("file", Some("fake.png"), &png()),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(err_of(&bytes).contains("PDF"), "{}", err_of(&bytes));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn enforces_the_upload_size_cap_with_a_precise_error() {
    // Payload sits above the 4 KiB file cap but below the request-body guard, so the
    // service's own check is what fires — and it must leave nothing on disk.
    let (app, root) = build_app(test_config(4 * 1024, 1_000_000));
    let big = vec![0u8; 24 * 1024];
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("big.png"), &big),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        err_of(&bytes).contains("maximum upload size"),
        "unexpected error: {}",
        err_of(&bytes)
    );
    assert_eq!(leftovers(&root), 0, "temp dirs leaked after a 413");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn framework_body_limit_rejects_absurd_requests() {
    // Way past both the file cap and the body guard: still a clean 413, not a hang.
    let (app, root) = build_app(test_config(4 * 1024, 1_000_000));
    let huge = vec![7u8; 2 * 1024 * 1024];
    let body = multipart(&[("file", Some("huge.png"), &huge)]);
    let (status, _, _) = send(&app, post_process(body)).await;
    // Refused either by our in-flight cap (413 with a precise message) or by the outer
    // body guard, which aborts the stream mid-parse (400). Either way: a clean client
    // error, no hang, and nothing left on disk.
    assert!(
        matches!(
            status,
            StatusCode::PAYLOAD_TOO_LARGE | StatusCode::BAD_REQUEST
        ),
        "unexpected status {status}"
    );
    assert_eq!(
        leftovers(&root),
        0,
        "temp dirs leaked after an over-limit body"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_oversized_image_dimensions() {
    // A 24×16 PNG, but the pixel budget is 100 px → must be refused before decoding.
    let (app, root) = build_app(test_config(1024 * 1024, 100));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("a.png"), &png()),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(err_of(&bytes).contains("pixel limit"), "{}", err_of(&bytes));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_mislabelled_files() {
    // A text file named .png must not be treated as an image (magic-byte sniffing).
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("lies.png"), b"i am not a png at all"),
    ]);
    let (status, _, bytes) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(
        err_of(&bytes).contains("images and PDFs"),
        "{}",
        err_of(&bytes)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rejects_a_non_multipart_post() {
    let (app, root) = build_app(test_config(1024 * 1024, 1_000_000));
    let req = Request::post("/api/process")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    // A body that is not multipart at all is a malformed request (axum's own rejection,
    // 400). 415 is reserved for a *file* whose content the action cannot use.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn names_survive_sanitising_in_the_download_header() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("../../evil\" name\r\nX: 1.png"), &png()),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    let cd = hdr(&headers, header::CONTENT_DISPOSITION);
    // The embedded quote closes the multipart `filename` parameter early, so the name
    // the server is handed is just `evil`. What must hold is that nothing hostile
    // survives the round trip: no control characters, no traversal, no quote to break
    // out of the quoted string with.
    assert!(!cd.contains(".."), "{cd}");
    assert!(!cd.contains('\n') && !cd.contains('\r'), "{cd}");
    assert!(!cd.contains('/'), "{cd}");
    assert_eq!(cd.matches('"').count(), 2, "{cd}");
    assert!(cd.starts_with("attachment; filename=\""), "{cd}");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn rewrites_a_traversal_filename_in_the_download_header() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("quality", None, b"50"),
        ("file", Some("../../secret dir/photo.png"), &png()),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    // Only the last path component survives (directories are dropped, not rewritten),
    // then the action's suffix is appended — a name that cannot point anywhere else.
    assert_eq!(
        hdr(&headers, header::CONTENT_DISPOSITION),
        "attachment; filename=\"photo_optimized.jpg\""
    );
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------ happy paths

#[tokio::test]
async fn compresses_a_png_to_jpeg_and_reports_sizes() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let original = png();
    let body = multipart(&[
        ("action", None, b"compress"),
        ("quality", None, b"35"),
        ("file", Some("holiday.png"), &original),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));

    assert_eq!(hdr(&headers, header::CONTENT_TYPE), "image/jpeg");
    assert_eq!(
        hdr(&headers, header::CONTENT_DISPOSITION),
        "attachment; filename=\"holiday_optimized.jpg\""
    );
    assert_eq!(hdr(&headers, header::CONTENT_LENGTH), out.len().to_string());
    assert_eq!(
        hdr(&headers, HeaderName::from_static("x-original-size")),
        original.len().to_string()
    );
    assert_eq!(
        hdr(&headers, HeaderName::from_static("x-output-size")),
        out.len().to_string()
    );
    assert!(out.starts_with(&[0xFF, 0xD8]), "not a jpeg stream");

    // The result decodes and keeps its geometry.
    let img = image::load_from_memory(&out).expect("output decodes");
    assert_eq!((img.width(), img.height()), (24, 16));

    // In-memory results free their working directory immediately.
    assert_eq!(
        leftovers(&root),
        0,
        "temp dirs leaked after a successful compress"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn keeps_png_lossless_at_top_quality() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("quality", None, b"100"),
        ("file", Some("shot.png"), &png()),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    assert_eq!(hdr(&headers, header::CONTENT_TYPE), "image/png");
    assert_eq!(
        hdr(&headers, header::CONTENT_DISPOSITION),
        "attachment; filename=\"shot_optimized.png\""
    );
    assert_eq!(&out[1..4], b"PNG");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn uses_the_default_quality_when_none_is_sent() {
    let mut cfg = test_config(4 * 1024 * 1024, 1_000_000);
    cfg.compression_default_quality = 20;
    let (app, root) = build_app(cfg);
    // JPEG in, JPEG out at both ends, so quality is the only variable. (A PNG source
    // at quality >= 90 is kept losslessly as PNG — covered by another test.)
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("a.jpg"), &jpeg()),
    ]);
    let (status, _, low) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK);

    // A higher default must produce a strictly larger file for the same input.
    let mut cfg = test_config(4 * 1024 * 1024, 1_000_000);
    cfg.compression_default_quality = 95;
    let (app, root_hi) = build_app(cfg);
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("a.jpg"), &jpeg()),
    ]);
    let (status, _, high) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(low.len() < high.len(), "{} vs {}", low.len(), high.len());

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(root_hi);
}

#[tokio::test]
async fn wraps_an_image_in_a_pdf() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"image_to_pdf"),
        ("file", Some("scan.png"), &png()),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    assert_eq!(hdr(&headers, header::CONTENT_TYPE), "application/pdf");
    assert_eq!(
        hdr(&headers, header::CONTENT_DISPOSITION),
        "attachment; filename=\"scan.pdf\""
    );
    assert!(out.starts_with(b"%PDF-1.4"));
    assert!(out.windows(5).any(|w| w == b"%%EOF"));

    let text: String = out.iter().map(|b| *b as char).collect();
    assert!(
        text.contains("/Filter /DCTDecode"),
        "image not embedded as DCTDecode"
    );
    assert!(text.contains("/MediaBox [0 0 17 12]"), "{text}");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn embeds_an_existing_jpeg_without_a_second_transcode() {
    // For JPEG input the original scan data must be carried over verbatim — no
    // re-encode, so quality is preserved and no pixel buffer is allocated.
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let src = jpeg();
    let body = multipart(&[
        ("action", None, b"image_to_pdf"),
        ("file", Some("photo.jpg"), &src),
    ]);
    let (status, _, out) = send(&app, post_process(body)).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    assert!(
        out.windows(src.len()).any(|w| w == &src[..]),
        "the original JPEG bytes were not reused"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn legacy_upload_alias_still_works() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let body = multipart(&[
        ("action", None, b"compress"),
        ("file", Some("a.png"), &png()),
    ]);
    let req = Request::builder()
        .method("POST")
        .uri("/upload")
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={BOUNDARY}")).unwrap(),
        )
        .body(Body::from(body))
        .unwrap();
    let (status, _, _) = send(&app, req).await;
    assert_eq!(status, StatusCode::OK);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn pdf_tools_either_work_or_are_reported_as_missing() {
    // Ghostscript/Poppler availability is host-dependent. Both outcomes must be
    // explicit and correct — a missing dependency is 503 with a clear message, never a
    // 500, and never a silently corrupted file.
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let pdf = valid_pdf();

    // compress
    let body = multipart(&[
        ("action", None, b"compress"),
        ("quality", None, b"40"),
        ("file", Some("doc.pdf"), &pdf),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    match status {
        StatusCode::OK => {
            assert_eq!(hdr(&headers, header::CONTENT_TYPE), "application/pdf");
            assert_eq!(
                hdr(&headers, header::CONTENT_DISPOSITION),
                "attachment; filename=\"doc_compressed.pdf\""
            );
            assert!(out.starts_with(b"%PDF-"), "compressed output is not a PDF");
            assert_eq!(
                hdr(&headers, HeaderName::from_static("x-original-size")),
                pdf.len().to_string()
            );
            assert_eq!(
                hdr(&headers, HeaderName::from_static("x-output-size")),
                out.len().to_string()
            );
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            assert!(err_of(&out).contains("ghostscript"), "{}", err_of(&out));
        }
        other => panic!(
            "unexpected status from PDF compress: {other} — {}",
            err_of(&out)
        ),
    }

    // pdf_to_image
    let body = multipart(&[
        ("action", None, b"pdf_to_image"),
        ("file", Some("doc.pdf"), &pdf),
    ]);
    let (status, headers, out) = send(&app, post_process(body)).await;
    match status {
        StatusCode::OK => {
            let ct = hdr(&headers, header::CONTENT_TYPE);
            assert!(
                ct == "image/png" || ct == "application/zip",
                "unexpected type {ct}"
            );
            if ct == "image/png" {
                assert!(
                    out.starts_with(&[0x89, b'P', b'N', b'G']),
                    "not a PNG stream"
                );
            } else {
                assert!(out.starts_with(b"PK"), "not a ZIP stream");
            }
        }
        StatusCode::SERVICE_UNAVAILABLE => {
            assert!(err_of(&out).contains("poppler"), "{}", err_of(&out));
        }
        other => panic!(
            "unexpected status from PDF→image: {other} — {}",
            err_of(&out)
        ),
    }

    // A streamed result is intentionally left for the TTL sweeper.
    assert!(
        leftovers(&root) <= 2,
        "unexpected temp growth: {}",
        leftovers(&root)
    );
    let _ = std::fs::remove_dir_all(root);
}

/// Our PDFs are hand-built, so they must be checked against a strict mainstream parser
/// and against the exact command line the service uses — not only against Ghostscript,
/// which is lenient about missing stream dictionaries.
#[tokio::test]
async fn poppler_rasterises_the_pdfs_we_generate() {
    use std::process::Command as StdCommand;
    use std::process::Stdio;

    if !pdftoppm_available().await {
        eprintln!("skipped: poppler not installed");
        return;
    }
    let dir = scratch_dir("poppler");
    let pdf_path = dir.join("doc.pdf");
    std::fs::write(&pdf_path, valid_pdf()).expect("write pdf");

    let out = StdCommand::new("pdftoppm")
        .args(["-png", "-r", "150"])
        .arg(&pdf_path)
        .arg(dir.join("page"))
        .stdin(Stdio::null())
        .output()
        .expect("pdftoppm runnable");
    let pages: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("png"))
                .collect()
        })
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "pdftoppm rejected our PDF: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(pages.len(), 1, "expected exactly one rendered page");
    let img = image::open(&pages[0]).expect("rendered page is not a decodable PNG");
    assert!(
        img.width() >= 32 && img.height() >= 24,
        "unexpected raster size {}x{}",
        img.width(),
        img.height()
    );
}

/// The compressor's own tool must accept what we generate, end to end, `--` and all.
#[tokio::test]
async fn ghostscript_rewrites_the_pdfs_we_generate() {
    use std::process::Command as StdCommand;
    use std::process::Stdio;

    if !gs_available().await {
        eprintln!("skipped: ghostscript not installed");
        return;
    }
    let dir = scratch_dir("gs");
    let pdf_path = dir.join("doc.pdf");
    std::fs::write(&pdf_path, valid_pdf()).expect("write pdf");
    let out_path = dir.join("out.pdf");

    let out = StdCommand::new("gs")
        .args([
            "-sDEVICE=pdfwrite",
            "-dCompatibilityLevel=1.4",
            "-dPDFSETTINGS=/ebook",
            "-dNOPAUSE",
            "-dBATCH",
            "-dQUIET",
            "-dSAFER",
        ])
        .arg(format!("-sOutputFile={}", out_path.display()))
        .arg("--")
        .arg(&pdf_path)
        .stdin(Stdio::null())
        .output()
        .expect("gs runnable");
    let produced = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        out.status.success(),
        "ghostscript rejected our PDF: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(produced > 0, "ghostscript wrote an empty file");
}

fn scratch_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fileoptimize-{tag}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[tokio::test]
async fn corrupt_pdf_yields_a_clean_error_not_a_panic() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let mut junk = b"%PDF-1.4\n".to_vec();
    junk.extend_from_slice(&[0u8; 512]);
    let body = multipart(&[
        ("action", None, b"pdf_to_image"),
        ("file", Some("junk.pdf"), &junk),
    ]);
    let (status, _, out) = send(&app, post_process(body)).await;
    assert!(
        matches!(
            status,
            StatusCode::OK | StatusCode::INTERNAL_SERVER_ERROR | StatusCode::SERVICE_UNAVAILABLE
        ),
        "unexpected {status}"
    );
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        assert!(!err_of(&out).is_empty(), "a 500 must still explain itself");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn concurrent_jobs_are_gated_but_all_complete() {
    // max_concurrent = 2 while 8 jobs are in flight: the gate must serialise, not drop.
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));

    let mut handles = Vec::new();
    for i in 0..8 {
        let app = app.clone();
        let png = png();
        handles.push(tokio::spawn(async move {
            let name = format!("pic-{i}.png");
            let body = multipart(&[
                ("action", None, b"compress"),
                ("quality", None, b"40"),
                ("file", Some(&name), &png),
            ]);
            let resp = app.oneshot(post_process(body)).await.expect("call");
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
                .await
                .unwrap();
            (status, bytes.len())
        }));
    }
    for h in handles {
        let (status, len) = h.await.expect("task");
        assert_eq!(status, StatusCode::OK);
        assert!(len > 100, "suspiciously small output: {len}");
    }
    assert_eq!(leftovers(&root), 0, "temp dirs leaked under concurrency");
    let _ = std::fs::remove_dir_all(root);
}

// --------------------------------------------------------- versioned API for programs

#[tokio::test]
async fn v1_returns_a_descriptor_and_a_refetchable_result() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let original = png();
    let body = multipart(&[
        ("action", None, b"compress"),
        ("quality", None, b"50"),
        // `store` is how a client that cannot stream a download uses the service.
        ("store", None, b"1"),
        ("file", Some("keep.png"), &original),
    ]);
    let (status, headers, out) = send(&app, request("POST", "/api/v1/process", body, &[])).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));
    assert_eq!(hdr(&headers, header::CONTENT_TYPE), "application/json");

    let v: serde_json::Value = serde_json::from_slice(&out).expect("descriptor json");
    let id = v["id"].as_str().expect("id").to_string();
    assert_eq!(id.len(), 32, "unexpected id: {id}");
    assert_eq!(v["filename"].as_str(), Some("keep_optimized.jpg"));
    assert_eq!(v["action"].as_str(), Some("compress"));
    assert_eq!(v["quality"].as_u64(), Some(50));
    assert_eq!(v["original_size"].as_u64(), Some(original.len() as u64));
    assert!(v["saved_bytes"].as_u64().unwrap_or(0) > 0, "{v}");
    let path = v["download_path"].as_str().expect("download_path").to_string();
    assert_eq!(path, format!("/api/v1/results/{id}"));

    // Fetch it back: same bytes as the direct download would have produced.
    let (status, headers, again) = send(&app, request("GET", &path, Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hdr(&headers, header::CONTENT_TYPE), "image/jpeg");
    assert_eq!(
        hdr(&headers, header::CONTENT_DISPOSITION),
        "attachment; filename=\"keep_optimized.jpg\""
    );
    assert_eq!(again.len() as u64, v["output_size"].as_u64().expect("output_size"));
    assert_eq!(&again[0..2], &[0xFF, 0xD8]);

    // Discard it early, then it is gone for good.
    let (status, _, _) = send(&app, request("DELETE", &path, Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, out) = send(&app, request("GET", &path, Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let code: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(code["code"].as_str(), Some("not_found"));

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn an_id_that_is_not_32_hex_chars_is_refused_without_touching_disk() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    for bad in [
        "../../etc/passwd",
        "",
        "deadbeef",
        "DEADBEEFDEADBEEFDEADBEEFDEADBEEF",
        "deadbeefdeadbeefdeadbeefdeadbeef/",
    ] {
        let (status, _, out) = send(
            &app,
            request("GET", &format!("/api/v1/results/{bad}"), Vec::new(), &[]),
        )
        .await;
        assert!(
            matches!(status, StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND),
            "{bad}: {status} — {}",
            err_of(&out)
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn v1_demands_a_key_only_when_the_operator_configured_one() {
    let mut cfg = test_config(4 * 1024 * 1024, 1_000_000);
    cfg.api_keys = vec!["s3cret".to_string(), "other-key".to_string()];
    let (app, root) = build_app(cfg);

    let body = || {
        multipart(&[
            ("action", None, b"compress"),
            ("file", Some("a.png"), &png()),
        ])
    };

    // No key at all → 401, with a challenge and a machine-readable code.
    let (status, headers, out) = send(&app, request("POST", "/api/v1/process", body(), &[])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", err_of(&out));
    assert!(
        hdr(&headers, header::WWW_AUTHENTICATE).contains("Bearer"),
        "no challenge header"
    );
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["code"].as_str(), Some("unauthorized"));

    // Wrong key → still 401 (and a different message, so clients can tell the two apart).
    let (status, _, _) = send(
        &app,
        request("POST", "/api/v1/process", body(), &[("x-api-key", "nope")]),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Either accepted spelling of the key works, and so does the query fallback.
    for (name, value) in [
        ("x-api-key", "s3cret"),
        ("x-api-key", "other-key"),
        ("authorization", "Bearer s3cret"),
        ("authorization", "bearer other-key"),
    ] {
        let (status, _, out) = send(
            &app,
            request("POST", "/api/v1/process", body(), &[(name, value)]),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{name}: {}", err_of(&out));
    }
    let (status, _, out) = send(
        &app,
        request(
            "POST",
            "/api/v1/process?api_key=s3cret",
            body(),
            &[],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&out));

    // Probes and the legacy surface stay open, which is the whole point of splitting
    // them: a load balancer must not need a secret.
    let (status, _, _) = send(&app, request("GET", "/api/v1/health", Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, request("GET", "/api/v1/info", Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, post_process(body())).await;
    assert_eq!(status, StatusCode::OK);

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn info_advertises_the_new_engine_limits() {
    let (app, root) = build_app(test_config(1234 * 1024, 5_000_000));
    let (status, _, out) = send(&app, request("GET", "/api/v1/info", Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["max_upload_size"].as_u64(), Some(1234 * 1024));
    assert_eq!(v["max_image_pixels"].as_u64(), Some(5_000_000));
    assert_eq!(v["memory_budget_mb"].as_u64(), Some(512));
    assert_eq!(v["memory_available_mb"].as_u64(), Some(512));
    assert_eq!(v["result_ttl_secs"].as_u64(), Some(3_600));
    assert_eq!(v["auth_required"].as_bool(), Some(false));
    assert_eq!(v["quality_range"][0].as_u64(), Some(1));
    assert_eq!(v["quality_range"][1].as_u64(), Some(100));
    let actions: Vec<&str> = v["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a.as_str().expect("action str"))
        .collect();
    assert_eq!(actions, ["compress", "image_to_pdf", "pdf_to_image"]);
    assert!(v["tools"].get("ghostscript").is_some(), "{v}");

    // Health reports queue depth, which is what an autoscaler wants to see.
    let (status, _, out) = send(&app, request("GET", "/api/health", Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"].as_str(), Some("ok"));
    assert_eq!(v["running_jobs"].as_u64(), Some(0));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn the_shipped_openapi_document_describes_the_real_routes() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    for path in ["/api/openapi.json", "/api/v1/openapi.json"] {
        let (status, headers, out) = send(&app, request("GET", path, Vec::new(), &[])).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(hdr(&headers, header::CONTENT_TYPE).starts_with("application/json"));
        let v: serde_json::Value = serde_json::from_slice(&out).expect("valid json");
        assert_eq!(v["openapi"].as_str().unwrap().chars().next().unwrap(), '3');
        for route in [
            "/api/v1/process",
            "/api/v1/results/{id}",
            "/api/v1/info",
            "/api/process",
            "/upload",
        ] {
            assert!(v["paths"].get(route).is_some(), "missing {route} in {path}");
        }
        let codes = v["components"]["schemas"]["Error"]["properties"]["code"]["enum"]
            .as_array()
            .expect("error codes");
        for code in ["bad_request", "payload_too_large", "unauthorized"] {
            assert!(
                codes.iter().any(|c| c.as_str() == Some(code)),
                "missing error code {code}"
            );
        }
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn error_bodies_carry_a_stable_code() {
    let (app, root) = build_app(test_config(4 * 1024 * 1024, 1_000_000));
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "bad_request",
            multipart(&[
                ("action", None, b"shrink"),
                ("file", Some("a.png"), &png()),
            ]),
        ),
        (
            "unsupported_media_type",
            multipart(&[
                ("action", None, b"compress"),
                ("file", Some("notes.txt"), b"just some text, not a file we handle"),
            ]),
        ),
    ];
    for (code, body) in cases {
        let (status, _, out) = send(&app, post_process(body)).await;
        let v: serde_json::Value = serde_json::from_slice(&out).expect("json error");
        assert!(status.is_client_error(), "{code}: {status}");
        assert_eq!(v["code"].as_str(), Some(code), "{v}");
        assert!(v["error"].as_str().unwrap_or_default().len() > 10, "{v}");
    }

    // An unknown path is a JSON 404 with the same envelope.
    let (status, _, out) = send(&app, request("GET", "/nope", Vec::new(), &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["code"].as_str(), Some("not_found"));
    let _ = std::fs::remove_dir_all(root);
}
