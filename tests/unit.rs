//! Unit tests for the deterministic parts of the service: validation, sniffing,
//! sanitisation, encoders, the hand-written PDF writer and the temp-file sweeper.

use std::io::Write;

use fileoptimize::detect::{detect, is_pdf, jpeg_is_cmyk, FileKind};
use fileoptimize::processing::image::encode_image;
use fileoptimize::processing::pdf_gen::build_image_pdf;
use fileoptimize::processing::{base_name, check_dimensions, parse_quality, pick_compress_format};
use fileoptimize::sanitize::{extension_of, sanitize_ext, sanitize_filename};

use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, ImageFormat, Rgb, RgbImage};

/// A deterministic 32×20 RGB gradient — small enough to keep the suite instant.
fn sample_image() -> RgbImage {
    ImageBuffer::from_fn(32, 20, |x, y| {
        Rgb([(x * 8) as u8, (y * 12) as u8, ((x + y) * 4) as u8])
    })
}

fn sample_png_bytes() -> Vec<u8> {
    let img = sample_image();
    let mut out = Vec::new();
    PngEncoder::new(&mut out)
        .write_image(img.as_raw(), 32, 20, ExtendedColorType::Rgb8)
        .expect("encode png");
    out
}

fn sample_jpeg_bytes() -> Vec<u8> {
    let img = sample_image();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .write_image(img.as_raw(), 32, 20, ExtendedColorType::Rgb8)
        .expect("encode jpeg");
    out
}

/// Lossless byte→char view of a binary buffer, so string offsets equal byte offsets.
fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|b| *b as char).collect()
}

// ---------------------------------------------------------------- sanitize

#[test]
fn strips_path_components_and_traversal() {
    assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_filename(r"C:\Windows\notice.txt"), "notice.txt");
    assert_eq!(sanitize_filename("a/../../b.pdf"), "b.pdf");
}

#[test]
fn neutralises_header_injection_in_filenames() {
    // Quotes, CR/LF and control bytes must never reach Content-Disposition.
    let evil = "x\"; filename=\"evil\r\nInjected: yes.txt";
    let safe = sanitize_filename(evil);
    assert!(!safe.contains('"'), "{safe}");
    assert!(!safe.contains('\n') && !safe.contains('\r'), "{safe}");
    assert!(!safe.contains(':'), "{safe}");
}

#[test]
fn falls_back_when_name_is_unusable() {
    assert_eq!(sanitize_filename(""), "file");
    assert_eq!(sanitize_filename("..."), "file");
    assert_eq!(sanitize_filename("///"), "file");
    assert_eq!(sanitize_filename("🐛🐛"), "file");
    assert_eq!(sanitize_filename(".hidden"), "hidden");
}

#[test]
fn truncates_pathologically_long_names() {
    let long = format!("{}.png", "a".repeat(5_000));
    assert!(sanitize_filename(&long).len() <= 136);
}

#[test]
fn extension_handling_is_conservative() {
    assert_eq!(extension_of("Report.PDF"), "pdf");
    assert_eq!(extension_of("no-extension"), "bin");
    assert_eq!(extension_of("archive.tar.gz"), "gz");
    assert_eq!(extension_of("file.toolongextension"), "bin");
    assert_eq!(extension_of("file.pn g"), "bin");
    assert_eq!(extension_of(""), "bin");
}

#[test]
fn sanitize_ext_normalises() {
    assert_eq!(sanitize_ext(".JPG"), "jpg");
    assert_eq!(sanitize_ext("pdf"), "pdf");
    assert_eq!(sanitize_ext(""), "bin");
    assert_eq!(sanitize_ext("p$df"), "bin");
}

// ---------------------------------------------------------------- detect

#[test]
fn detects_by_magic_bytes_not_extension() {
    assert!(matches!(
        detect(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n..."),
        FileKind::Pdf
    ));
    assert!(matches!(
        detect(&sample_png_bytes()),
        FileKind::Image(ImageFormat::Png)
    ));
    assert!(matches!(
        detect(&sample_jpeg_bytes()),
        FileKind::Image(ImageFormat::Jpeg)
    ));
    assert!(matches!(detect(b"just some text"), FileKind::Unknown));
    assert!(matches!(detect(&[]), FileKind::Unknown));
    assert!(is_pdf(b"%PDF-1.4"));
    assert!(!is_pdf(b"  %PDF-1.4"));
}

#[test]
fn ycck_jpegs_are_flagged_for_reencode() {
    // Hand-built APP14 with colour transform 2 (YCCK).
    let mut evil: Vec<u8> = vec![0xFF, 0xD8, 0xFF, 0xEE, 0x00, 0x0E];
    evil.extend_from_slice(b"Adobe\x00");
    evil.extend_from_slice(&[0x01, 0x02, 0x00, 0x02]); // ver, flags0, flags1, transform
    evil.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
    assert!(jpeg_is_cmyk(&evil));

    // A real baseline JPEG must not be flagged.
    assert!(!jpeg_is_cmyk(&sample_jpeg_bytes()));
    // A truncated marker chain must not panic or loop.
    assert!(!jpeg_is_cmyk(b"\xFF\xD8\xFF"));
    assert!(!jpeg_is_cmyk(&[]));
    assert!(!jpeg_is_cmyk(b"\xFF\xD8\xFF\xEE\x00\x00zz"));
    assert!(!jpeg_is_cmyk(&sample_png_bytes()));
}

// ---------------------------------------------------------------- bounds

#[test]
fn quality_bounds_are_enforced() {
    assert_eq!(parse_quality(None, 60).unwrap(), 60);
    assert_eq!(parse_quality(Some(""), 60).unwrap(), 60);
    assert_eq!(parse_quality(Some("  100  "), 60).unwrap(), 100);
    assert_eq!(parse_quality(Some("1"), 60).unwrap(), 1);
    assert_eq!(parse_quality(Some("25"), 60).unwrap(), 25);
    for bad in [
        "0", "-1", "101", "255", "abc", "50.5", "1e2", "٩٩", "+5", "50%",
    ] {
        assert!(
            parse_quality(Some(bad), 60).is_err(),
            "quality {bad:?} should be rejected"
        );
    }
}

#[test]
fn blank_quality_falls_back_to_the_default() {
    assert_eq!(parse_quality(Some("   "), 42).unwrap(), 42);
}

#[test]
fn pixel_budget_rejects_oversized_images() {
    assert!(check_dimensions(100, 100, 25_000_000).is_ok());
    assert!(check_dimensions(6000, 6000, 25_000_000).is_err());
    // Zero/absurd dimensions are never acceptable.
    assert!(check_dimensions(0, 100, 25_000_000).is_err());
    assert!(check_dimensions(1, 1, 1).is_ok());
    // And the multiplication must not overflow u32.
    assert!(check_dimensions(u32::MAX, u32::MAX, 1_000).is_err());
}

#[test]
fn format_selection_stays_lossless_only_at_high_quality() {
    assert_eq!(
        pick_compress_format(ImageFormat::Jpeg, 10),
        ImageFormat::Jpeg
    );
    assert_eq!(
        pick_compress_format(ImageFormat::Png, 100),
        ImageFormat::Png
    );
    assert_eq!(
        pick_compress_format(ImageFormat::Png, 89),
        ImageFormat::Jpeg
    );
    assert_eq!(
        pick_compress_format(ImageFormat::WebP, 80),
        ImageFormat::Jpeg
    );
    assert_eq!(
        pick_compress_format(ImageFormat::Gif, 99),
        ImageFormat::Jpeg
    );
}

// ---------------------------------------------------------------- encoders

#[test]
fn jpeg_output_honours_quality_and_stays_smaller() {
    let img = image::load_from_memory(&sample_png_bytes()).expect("decode");
    let low = encode_image(img.clone(), ImageFormat::Jpeg, 15).expect("low");
    let high = encode_image(img.clone(), ImageFormat::Jpeg, 95).expect("high");
    assert!(low.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    assert!(high.starts_with(&[0xFF, 0xD8]));
    assert!(
        low.len() < high.len(),
        "quality 15 ({}) should beat quality 95 ({})",
        low.len(),
        high.len()
    );
    // And it must still decode, with the original geometry intact.
    let back = image::load_from_memory(&low).expect("re-decode");
    assert_eq!((back.width(), back.height()), (32, 20));
}

#[test]
fn png_output_is_lossless() {
    let img = image::load_from_memory(&sample_png_bytes()).expect("decode");
    let png = encode_image(img, ImageFormat::Png, 50).expect("encode png");
    assert_eq!(&png[1..4], b"PNG");
    let back = image::load_from_memory(&png).expect("re-decode");
    assert_eq!(back.to_rgb8(), sample_image());
}

// ---------------------------------------------------------------- pdf writer

#[test]
fn image_pdf_has_valid_structure_and_xref_offsets() {
    let jpeg = sample_jpeg_bytes();
    let pdf = build_image_pdf(&jpeg, 32, 20, 100.0);

    assert!(pdf.starts_with(b"%PDF-1.4"));
    assert!(pdf.windows(5).any(|w| w == b"%%EOF"));
    let text = latin1(&pdf);

    // The JPEG is embedded verbatim — no transcode, no second lossy generation.
    assert!(
        pdf.windows(jpeg.len()).any(|w| w == jpeg),
        "jpeg not embedded raw"
    );

    // /Length must match the stream payload exactly or viewers reject the file.
    let at = text.find("/Length ").expect("length key");
    // `>>` is a delimiter, so writers may glue it to the number — read the leading
    // digits rather than a whitespace-delimited token.
    let digits: String = text[at + 8..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    assert_eq!(
        digits.parse::<usize>().expect("len int"),
        jpeg.len(),
        "/Length does not match the embedded stream"
    );

    // startxref must point at the real xref table.
    let start = text.rfind("startxref").expect("startxref");
    let offset: usize = text[start + 9..]
        .split_whitespace()
        .next()
        .expect("xref offset")
        .parse()
        .expect("xref offset int");
    assert_eq!(
        &pdf[offset..offset + 4],
        b"xref",
        "startxref does not point at xref"
    );

    // The content stream needs its own exact /Length too — poppler refuses pages
    // without it (ghostscript is lenient, which is how this slipped before).
    let obj5 = text.rfind("5 0 obj").expect("object 5");
    let rest = &text[obj5..];
    let l_at = rest.find("/Length ").expect("content length");
    let declared: usize = rest[l_at + 8..]
        .split_whitespace()
        .next()
        .and_then(|v| v.trim_end_matches('>').parse().ok())
        .expect("content length int");
    let s_start = rest.find("\nstream\n").expect("content stream") + 8;
    let s_end = rest.find("\nendstream").expect("endstream");
    assert_eq!(declared, s_end - s_start, "content stream /Length mismatch");
    assert!(rest[s_start..s_end].starts_with("q "), "{rest}");

    // Every recorded offset must land exactly on its object header.
    let table = &text[offset..];
    let mut lines = table.lines().skip(3); // "xref", size, free entry
    for num in 1..=5u32 {
        let line = lines.next().expect("xref entry");
        let off: usize = line[..10].parse().expect("offset digits");
        let expect = format!("{num} 0 obj");
        assert_eq!(
            &text[off..off + expect.len()],
            expect,
            "bad offset for object {num}"
        );
    }
    assert!(lines.next().is_none(), "unexpected extra xref entries");
}

#[test]
fn image_pdf_page_box_uses_dpi_scaling() {
    // 32×20 px at 100 DPI → 23×14 pt.
    let pdf = build_image_pdf(&sample_jpeg_bytes(), 32, 20, 100.0);
    let text = latin1(&pdf);
    assert!(text.contains("/MediaBox [0 0 23 14]"), "{text}");

    // A 1×1 image still yields a legal (non-zero) page box.
    let tiny = latin1(&build_image_pdf(&sample_jpeg_bytes(), 1, 1, 100.0));
    assert!(tiny.contains("/MediaBox [0 0 1 1]"), "{tiny}");

    // The content stream flips Y so the image is not mirrored.
    assert!(text.contains("23 0 0 -14 0 14 cm /Im0 Do Q"), "{text}");
}

#[test]
fn base_name_drops_extension_but_keeps_words() {
    assert_eq!(base_name("holiday.photo.jpg"), "holiday.photo");
    assert_eq!(base_name("noext"), "noext");
    assert_eq!(base_name("../../etc/passwd"), "passwd");
}

// ---------------------------------------------------------------- zip writer

#[test]
fn zip_writer_roundtrips_every_page() {
    let dir = std::env::temp_dir().join(format!("fo-zip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let pages: Vec<std::path::PathBuf> = (1..=3)
        .map(|i| {
            let p = dir.join(format!("page-{i}.png"));
            let mut f = std::fs::File::create(&p).expect("create page");
            f.write_all(&sample_png_bytes()).expect("write page");
            f.flush().expect("flush");
            p
        })
        .collect();

    let entries: Vec<(String, std::path::PathBuf)> = pages
        .iter()
        .enumerate()
        .map(|(i, p)| (format!("page_{:04}.png", i + 1), p.clone()))
        .collect();

    let archive = dir.join("out.zip");
    fileoptimize::processing::zip::write_zip_from_files(&archive, &entries).expect("write zip");

    let file = std::fs::File::open(&archive).expect("open zip");
    let mut za = zip::ZipArchive::new(file).expect("read zip");
    assert_eq!(za.len(), 3);
    for i in 0..3 {
        let mut zf = za.by_index(i).expect("entry");
        assert_eq!(zf.name(), format!("page_{:04}.png", i + 1));
        let mut got = Vec::new();
        std::io::Read::read_to_end(&mut zf, &mut got).expect("read entry");
        assert_eq!(got, sample_png_bytes(), "entry {i} corrupted");
    }
    drop(za);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------- cleanup

#[tokio::test]
async fn sweeper_removes_only_expired_dirs() {
    let root = std::env::temp_dir().join(format!("fo-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    let old = root.join("old");
    let fresh = root.join("fresh");
    std::fs::create_dir_all(&old).expect("old");
    std::fs::create_dir_all(&fresh).expect("fresh");
    std::fs::write(old.join("x.png"), b"stale").expect("file in old");

    // Sweeping a missing/empty dir is a no-op, never an error.
    assert_eq!(fileoptimize::cleanup::sweep(&fresh, 0).await.unwrap(), 0);
    assert_eq!(
        fileoptimize::cleanup::sweep(&root.join("nope"), 0)
            .await
            .unwrap(),
        0
    );

    // ttl 0 ⇒ everything is expired.
    let removed = fileoptimize::cleanup::sweep(&root, 0).await.unwrap();
    assert_eq!(removed, 2, "both work dirs should be collected");
    assert!(!old.exists());
    assert!(!fresh.exists());

    // A generous TTL must leave recent dirs alone.
    std::fs::create_dir_all(&fresh).expect("recreate");
    assert_eq!(fileoptimize::cleanup::sweep(&root, 3_600).await.unwrap(), 0);
    assert!(fresh.exists(), "fresh dir must survive");

    // Loose files in the root are ignored — we only own our subdirectories.
    std::fs::write(root.join("stray.txt"), b"x").expect("stray");
    assert_eq!(fileoptimize::cleanup::sweep(&root, 0).await.unwrap(), 1);
    assert!(root.join("stray.txt").exists());

    let _ = std::fs::remove_dir_all(&root);
}
