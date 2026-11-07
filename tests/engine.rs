//! The engine used as a library: no router, no sockets, no multipart framing.
//!
//! These tests are the contract for embedders — another service that vendors this crate
//! and calls [`Engine::run`] must get exactly the guarantees the HTTP API advertises, so
//! they assert the same bounds (size, pixels, type, memory) at the engine boundary.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgb};

use fileoptimize::config::Config;
use fileoptimize::engine::{Action, Engine, Input, Job, JobId};
use fileoptimize::error::AppError;

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Isolated engine + its temp root, so leaks are measurable per test.
fn setup() -> (Engine, PathBuf) {
    let cfg = test_config(1_000_000, 25_000_000);
    let root = cfg.tmp_dir.clone();
    (Engine::new(cfg), root)
}

fn test_config(max_upload: u64, max_pixels: u32) -> Config {
    let root: PathBuf = std::env::temp_dir().join(format!(
        "fo-engine-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("create engine test tmp root");
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
        memory_budget_mb: 512,
        api_keys: Vec::new(),
    }
}

fn gradient(w: u32, h: u32) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    ImageBuffer::from_fn(w, h, |x, y| Rgb([(x * 9) as u8, (y * 13) as u8, 128]))
}

/// Textured, so PNG has to spend real bytes on it while JPEG gets to throw them away:
/// the only honest way to assert that compression shrank something, or that it scales
/// with quality. At thumbnail sizes the format tables alone outweigh the pixels.
fn photo(w: u32, h: u32) -> ImageBuffer<Rgb<u8>, Vec<u8>> {
    ImageBuffer::from_fn(w, h, |x, y| {
        let n = (x as u64).wrapping_mul(73856093) ^ (y as u64).wrapping_mul(19349663);
        Rgb([
            (n % 251) as u8,
            ((n / 7) % 249) as u8,
            ((n / 13) % 245) as u8,
        ])
    })
}

fn photo_png(w: u32, h: u32) -> Vec<u8> {
    let img = photo(w, h);
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn photo_jpeg(w: u32, h: u32, quality: u8) -> Vec<u8> {
    let img = photo(w, h);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = gradient(w, h);
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

/// A large *flat* image: big in pixels, tiny on disk, so a refusal can only come from
/// the memory the decode would need — never from the upload-size cap.
fn png_flat(w: u32, h: u32) -> Vec<u8> {
    // Annotated: without this the colour literals default to i32 and the encoder, which
    // takes `&[u8]`, will not accept the buffer.
    let img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgb([10, 200, 40]));
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn jpeg_bytes() -> Vec<u8> {
    let img = gradient(24, 16);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 92)
        .write_image(img.as_raw(), 24, 16, ExtendedColorType::Rgb8)
        .unwrap();
    out
}

fn entries(root: &std::path::Path) -> usize {
    std::fs::read_dir(root).map(|rd| rd.count()).unwrap_or(0)
}

// ------------------------------------------------------------------ the happy path

#[tokio::test]
async fn compresses_png_bytes_into_a_smaller_jpeg() {
    let (engine, root) = setup();
    let original = photo_png(320, 240);
    let outcome = engine
        .run(
            Job::new(
                Action::Compress,
                "../../etc/holiday.png",
                Input::Bytes(original.clone()),
            )
            .with_quality(50),
        )
        .await
        .expect("compress");

    // The name comes back sanitised, so a caller can echo it in a header verbatim.
    assert_eq!(outcome.original_name, "holiday.png");
    assert_eq!(outcome.output.filename, "holiday_optimized.jpg");
    assert_eq!(outcome.output.content_type, "image/jpeg");
    assert_eq!(outcome.original_size, original.len() as u64);
    assert!(
        outcome.output.size() < original.len() as u64,
        "compression made it bigger"
    );
    let payload = outcome.output.read_all().expect("payload");
    assert_eq!(&payload[0..2], &[0xFF, 0xD8], "not a JPEG stream");
    assert_eq!(
        outcome.bytes_saved(),
        original.len() as u64 - payload.len() as u64
    );
    // An in-memory result leaves no scratch behind.
    assert_eq!(entries(&root), 0, "scratch leaked");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn quality_is_respected_and_the_default_applies_when_omitted() {
    let mut cfg = test_config(1_000_000, 25_000_000);
    cfg.compression_default_quality = 20;
    let engine = Engine::new(cfg.clone());

    // JPEG in, JPEG out: size must follow quality monotonically.
    let small = engine
        .run(Job::new(
            Action::Compress,
            "a.jpg",
            Input::Bytes(photo_jpeg(320, 240, 92)),
        ))
        .await
        .expect("low quality");
    let omitted = engine
        .run(Job::new(
            Action::Compress,
            "a.jpg",
            Input::Bytes(photo_jpeg(320, 240, 92)),
        ))
        .await
        .expect("default quality");
    cfg.compression_default_quality = 95;
    let big_engine = Engine::new(cfg);
    let big = big_engine
        .run(Job::new(
            Action::Compress,
            "a.jpg",
            Input::Bytes(photo_jpeg(320, 240, 92)),
        ))
        .await
        .expect("high quality");

    let a = small.output.size();
    let b = omitted.output.size();
    let c = big.output.size();
    assert!(a <= b, "{a} vs {b}");
    assert!(b < c, "{b} vs {c}");
    // The omitted case really did use the configured default of 20.
    assert_eq!(omitted.quality, 20);
    let _ = std::fs::remove_dir_all(&engine.config().tmp_dir);
    let _ = std::fs::remove_dir_all(&big_engine.config().tmp_dir);
}

#[tokio::test]
async fn wraps_an_image_in_a_pdf_the_caller_can_read_back() {
    let (engine, root) = setup();
    let outcome = engine
        .run(Job::new(
            Action::ImageToPdf,
            "scan.png",
            Input::Bytes(png_bytes(24, 16)),
        ))
        .await
        .expect("image_to_pdf");
    let pdf = outcome.output.read_all().expect("pdf bytes");
    assert!(pdf.starts_with(b"%PDF-"), "no PDF header");
    assert!(
        pdf.windows(b"startxref".len()).any(|w| w == b"startxref"),
        "no xref"
    );
    assert!(pdf.windows(5).any(|w| w == b"%%EOF"), "no EOF marker");
    assert_eq!(outcome.output.filename, "scan.pdf");
    assert_eq!(outcome.output.content_type, "application/pdf");
    assert_eq!(entries(&root), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn an_image_that_cannot_be_made_smaller_comes_back_as_it_was() {
    // A 24x16 gradient PNG is 218 bytes. The smallest honest JPEG of the same pixels costs
    // several times that because the format's tables do not amortise over six blocks, so a
    // naive compressor would "optimise" the file into something bigger than it started as.
    let (engine, root) = setup();
    let original = png_bytes(24, 16);
    let outcome = engine
        .run(
            Job::new(Action::Compress, "dot.png", Input::Bytes(original.clone()))
                .with_quality(25),
        )
        .await
        .expect("compress");

    assert_eq!(outcome.output.content_type, "image/png");
    assert_eq!(outcome.output.filename, "dot_optimized.png");
    assert_eq!(outcome.output.size(), original.len() as u64);
    assert_eq!(outcome.bytes_saved(), 0, "the engine claimed a saving");
    assert_eq!(outcome.output.read_all().expect("payload"), original);
    assert_eq!(entries(&root), 0, "scratch left behind");
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------ the bounds

#[tokio::test]
async fn rejects_empty_input_and_input_beyond_the_cap() {
    let (engine, root) = setup();

    let err = engine
        .run(Job::new(Action::Compress, "e.png", Input::Bytes(vec![])))
        .await
        .expect_err("empty input must not be accepted");
    assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");
    assert_eq!(err.code(), "bad_request");

    let fat = vec![0u8; (engine.config().max_upload_size as usize) + 1];
    let err = engine
        .run(Job::new(Action::Compress, "f.png", Input::Bytes(fat)))
        .await
        .expect_err("oversized input must be refused");
    assert!(matches!(err, AppError::PayloadTooLarge(_)), "{err:?}");
    assert_eq!(err.code(), "payload_too_large");

    // Refusals happen before any work, so nothing is left behind either way.
    assert_eq!(entries(&root), 0, "scratch leaked on a rejected job");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn refuses_input_it_cannot_use() {
    let (engine, root) = setup();
    let err = engine
        .run(Job::new(
            Action::Compress,
            "notes.txt",
            Input::Bytes(b"prose, not pixels".to_vec()),
        ))
        .await
        .expect_err("text must not be compressed as an image");
    assert!(matches!(err, AppError::UnsupportedMediaType(_)), "{err:?}");
    assert_eq!(err.code(), "unsupported_media_type");
    assert_eq!(entries(&root), 0);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn validates_action_and_quality_from_raw_field_values() {
    // No engine is needed: the field parser is the thing under test.
    let (_engine, root) = setup();
    let input = || Input::Bytes(png_bytes(24, 16));

    let err = Job::from_fields(Some("shrink"), None, Some("a.png"), input(), false, 60)
        .expect_err("unknown action");
    assert!(matches!(err, AppError::BadRequest(_)), "{err:?}");

    // An empty quality means "use the server default" (asserted below), so only real
    // out-of-range or non-numeric values are refused here.
    for bad in ["0", "101", "-4", "abc", "60.5"] {
        let err = Job::from_fields(
            Some("compress"),
            Some(bad),
            Some("a.png"),
            input(),
            false,
            60,
        )
        .expect_err("quality must be 1..=100");
        assert!(matches!(err, AppError::BadRequest(_)), "{bad}: {err:?}");
    }

    // Empty action means the default; empty quality means the server default.
    let job = Job::from_fields(Some(""), Some(""), Some("a.png"), input(), false, 60).expect("ok");
    assert_eq!(job.action, Action::Compress);
    assert_eq!(job.quality, 60);
    // Hyphenated spellings stay accepted (the original client used them).
    let job = Job::from_fields(
        Some("image-to-pdf"),
        Some("75"),
        Some("a.png"),
        input(),
        false,
        60,
    )
    .expect("ok");
    assert_eq!(job.action, Action::ImageToPdf);
    assert_eq!(job.quality, 75);
    assert!(!job.store);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_decode_bigger_than_the_memory_budget_is_refused_not_attempted() {
    // 4 MP needs ~32 MB of working memory; a 16 MB budget must refuse it up front
    // instead of letting the allocator find out the hard way.
    let mut tight = test_config(8_000_000, 25_000_000);
    tight.memory_budget_mb = 16;
    let engine = Engine::new(tight);
    let big = png_flat(2000, 2000);
    assert!(
        big.len() < 512 * 1024,
        "test input must stay small so the budget is what refuses it: {} bytes",
        big.len()
    );
    let err = engine
        .run(Job::new(Action::Compress, "big.png", Input::Bytes(big.clone())).with_quality(50))
        .await
        .expect_err("over-budget decode");
    assert!(
        matches!(err, AppError::PayloadTooLarge(_)),
        "expected a size/budget refusal, got {err:?}"
    );
    assert!(err.message().contains("MEMORY_BUDGET_MB"), "{err:?}");
    // Nothing was reserved permanently.
    assert_eq!(engine.available_memory_mb(), 16);
    let _ = std::fs::remove_dir_all(&engine.config().tmp_dir);

    // With room to spare, the same file is accepted.
    let ok = test_config(8_000_000, 25_000_000);
    let ok_root = ok.tmp_dir.clone();
    let roomy = Engine::new(ok);
    let outcome = roomy
        .run(Job::new(Action::Compress, "big.png", Input::Bytes(big)).with_quality(50))
        .await
        .expect("within budget");
    assert!(outcome.output.size() > 0);
    // The reservation is returned when the job ends.
    assert_eq!(roomy.available_memory_mb(), 512);
    assert_eq!(roomy.running_jobs(), 0);
    let _ = std::fs::remove_dir_all(ok_root);
}

#[tokio::test]
async fn pdf_actions_say_503_when_the_tool_is_absent() {
    let (engine, root) = setup();
    let pdf = fileoptimize::processing::pdf_gen::build_image_pdf(&jpeg_bytes(), 24, 16, 100.0);
    let tools = engine.tools().await;
    let outcome = engine
        .run(Job::new(Action::PdfToImage, "doc.pdf", Input::Bytes(pdf)))
        .await;
    match outcome {
        Ok(got) => {
            assert!(tools.poppler, "succeeded without poppler?");
            let bytes = got.output.read_all().expect("bytes");
            let is_png = bytes.starts_with(&[0x89, b'P', b'N', b'G']);
            let is_zip = bytes.starts_with(b"PK");
            assert!(is_png || is_zip, "unexpected payload");
        }
        Err(err) => {
            assert!(matches!(err, AppError::ExternalToolMissing(_)), "{err:?}");
            assert_eq!(err.code(), "external_tool_missing");
        }
    }
    // A rendered page is streamed off disk, so its directory is deliberately handed to
    // the sweeper (one entry); a refusal leaves nothing at all.
    assert!(
        entries(&root) <= 1,
        "scratch leaked: {root:?} has {} entries",
        entries(&root)
    );
    let _ = std::fs::remove_dir_all(root);
}

// ------------------------------------------------------------------ ownership

#[tokio::test]
async fn a_file_input_is_left_where_it_was() {
    let (engine, root) = setup();
    let outside = std::env::temp_dir().join(format!(
        "fo-engine-own-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&outside).expect("caller dir");
    let mine = outside.join("holiday.png");
    let bytes = png_bytes(24, 16);
    std::fs::write(&mine, &bytes).expect("write");

    let outcome = engine
        .run(Job::new(Action::Compress, "holiday.png", Input::Path(mine.clone())).with_quality(40))
        .await
        .expect("compress a caller-owned path");

    assert_eq!(outcome.original_size, bytes.len() as u64);
    assert!(mine.exists(), "the engine deleted the caller's file");
    assert_eq!(
        std::fs::read(&mine).expect("still readable").len(),
        bytes.len()
    );
    // The engine's own scratch is gone; the caller's directory is not ours to touch.
    assert_eq!(entries(&root), 0);
    let _ = std::fs::remove_dir_all(outside);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn a_retained_result_can_be_read_back_and_dropped() {
    let (engine, root) = setup();
    let outcome = engine
        .run(
            Job::new(
                Action::Compress,
                "keep.png",
                Input::Bytes(photo_png(320, 240)),
            )
            .with_quality(45)
            .stored(),
        )
        .await
        .expect("stored run");

    let id = outcome.id.clone().expect("id");
    let meta = outcome.meta.as_ref().expect("meta");
    assert_eq!(meta.action, "compress");
    assert_eq!(meta.filename, "keep_optimized.jpg");
    assert_eq!(meta.original_name, "keep.png");
    assert_eq!(meta.max_age_secs, 3_600);
    assert!(meta.created_unix > 1_700_000_000, "{meta:?}");
    let stored = engine.results_dir().join(id.as_str());
    assert!(stored.join("meta.json").exists(), "no metadata written");
    assert!(stored.join(&meta.filename).exists(), "no payload written");

    let (fetched, again) = engine.result(&id).await.expect("fetch");
    assert_eq!(again.output_size, meta.output_size);
    assert_eq!(fetched.filename, "keep_optimized.jpg");
    assert_eq!(fetched.content_type, "image/jpeg");
    let payload = fetched.read_all().expect("payload");
    assert_eq!(payload.len() as u64, meta.output_size);

    assert!(engine.discard(&id).await.expect("discard"));
    let err = engine.result(&id).await.expect_err("gone");
    assert!(matches!(err, AppError::NotFound(_)), "{err:?}");
    assert!(
        !engine.discard(&id).await.expect("second discard"),
        "double delete"
    );

    // The scratch dirs are transient; only the retained copy existed, and it is now
    // removed as well.
    assert_eq!(entries(&root), 1, "expected only the results container");
    assert!(
        engine.results_dir().exists(),
        "container must survive sweeps"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn result_ids_are_strict() {
    // Anything that could escape the results directory must be rejected at the door.
    for bad in [
        "",
        "..",
        "../../etc/passwd",
        "DEADBEEFDEADBEEFDEADBEEFDEADBEEF",
        "deadbeefdeadbeefdeadbeefdeadbeefx",
        "deadbeef-deadf-beef-dead-beefdeadbeef0",
    ] {
        let err = JobId::parse(bad).expect_err("must reject");
        assert!(matches!(err, AppError::BadRequest(_)), "{bad}: {err:?}");
    }
    let id = JobId::random();
    assert_eq!(JobId::parse(id.as_str()).expect("round trip"), id);
    assert_eq!(id.as_str().len(), 32);
}
