//! File transformation pipeline. Each action produces an [`Output`] that the route
//! layer turns into a downloadable response.
//!
//! Memory strategy:
//! - uploads are streamed to disk as they arrive (never fully buffered in RAM),
//! - images are dimension-checked *before* full decode to reject decompression bombs,
//! - PDF work happens on disk via external tools and is streamed back from disk,
//! - a semaphore in [`crate::state::AppState`] caps concurrent jobs so peak memory
//!   stays roughly `max_concurrent * per_job_peak`.

pub mod image;
pub mod pdf;
pub mod pdf_gen;
pub mod zip;

use std::path::PathBuf;

use ::image::{DynamicImage, ImageFormat};

use crate::error::{AppError, AppResult};

/// A processed result ready to be streamed back to the client.
///
/// Exactly one of `data` / `path` is populated. Small or inherently in-memory results
/// (image compression, image→PDF) use `data`; large lazily-produced results
/// (compressed PDFs, rendered pages, ZIP archives) use `path` so they are copied
/// straight from disk into the response socket.
pub struct Output {
    pub data: Option<Vec<u8>>,
    pub path: Option<PathBuf>,
    pub filename: String,
    pub content_type: String,
}

/// Deliberately terse: a payload can be hundreds of megabytes, and `{:?}` on one would
/// be a wall of bytes in the middle of a log line.
impl std::fmt::Debug for Output {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Output")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("bytes", &self.size())
            .field("streamed", &self.is_streamed())
            .finish()
    }
}

impl Output {
    pub fn in_memory(data: Vec<u8>, filename: String, content_type: &str) -> Self {
        Output {
            data: Some(data),
            path: None,
            filename,
            content_type: content_type.to_string(),
        }
    }

    pub fn from_path(path: PathBuf, filename: String, content_type: &str) -> Self {
        Output {
            data: None,
            path: Some(path),
            filename,
            content_type: content_type.to_string(),
        }
    }

    /// `true` when the payload lives on disk and must therefore not be deleted before
    /// the response has been streamed.
    pub fn is_streamed(&self) -> bool {
        self.path.is_some()
    }

    /// Take the in-memory payload, if there is one (`None` for a streamed result).
    pub fn take_data(&mut self) -> Option<Vec<u8>> {
        self.data.take()
    }

    /// The whole payload as bytes — from memory when it is already there, otherwise read
    /// from disk. Convenient for library callers that would rather not stream.
    pub fn read_all(&self) -> crate::error::AppResult<Vec<u8>> {
        if let Some(data) = &self.data {
            return Ok(data.clone());
        }
        match &self.path {
            Some(p) => Ok(std::fs::read(p)?),
            None => Ok(Vec::new()),
        }
    }

    /// Open the on-disk payload, so a caller can copy or hash it without buffering it.
    pub fn open_file(&self) -> crate::error::AppResult<Option<std::fs::File>> {
        match &self.path {
            Some(p) => Ok(Some(std::fs::File::open(p)?)),
            None => Ok(None),
        }
    }

    /// Best-effort size in bytes (in-memory or on-disk).
    pub fn size(&self) -> u64 {
        if let Some(d) = &self.data {
            d.len() as u64
        } else if let Some(p) = &self.path {
            std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
        } else {
            0
        }
    }
}

/// Read up to `n` bytes from the start of a file — enough for magic-byte sniffing
/// without pulling a potentially huge upload into memory.
pub async fn read_header(path: &std::path::Path, n: usize) -> AppResult<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut f = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; n];
    let mut total = 0usize;
    while total < n {
        match f.read(&mut buf[total..]).await? {
            0 => break,
            k => total += k,
        }
    }
    buf.truncate(total);
    Ok(buf)
}

/// Decode an image, but only after confirming its dimensions stay within `max_pixels`.
/// This is the key defence against decompression-bomb style inputs: we never allocate
/// the full pixel buffer for an image whose declared dimensions exceed the cap.
pub fn decode_checked(data: &[u8], fmt: ImageFormat, max_pixels: u32) -> AppResult<DynamicImage> {
    let (w, h) =
        ::image::ImageReader::with_format(std::io::Cursor::new(data), fmt).into_dimensions()?;
    check_dimensions(w, h, max_pixels)?;
    Ok(::image::load_from_memory_with_format(data, fmt)?)
}

/// Reject images whose declared pixel count exceeds the configured cap.
pub fn check_dimensions(w: u32, h: u32, max_pixels: u32) -> AppResult<()> {
    if w == 0 || h == 0 {
        return Err(AppError::BadRequest(format!(
            "invalid image dimensions {w}x{h}"
        )));
    }
    if u64::from(w) * u64::from(h) > u64::from(max_pixels) {
        return Err(AppError::BadRequest(format!(
            "image dimensions {w}x{h} ({} px) exceed the {max_pixels} pixel limit",
            u64::from(w) * u64::from(h)
        )));
    }
    Ok(())
}

/// Extension (no dot) for an image format.
pub fn ext_of(fmt: ImageFormat) -> &'static str {
    match fmt {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Png => "png",
        ImageFormat::WebP => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        _ => "img",
    }
}

/// MIME type for an image format.
pub fn content_type_of(fmt: ImageFormat) -> &'static str {
    match fmt {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Gif => "image/gif",
        ImageFormat::Bmp => "image/bmp",
        ImageFormat::Tiff => "image/tiff",
        _ => "application/octet-stream",
    }
}

/// Choose the encoding used when compressing.
///
/// PNG stays PNG only when the user asks for near-lossless output (quality ≥ 90),
/// because PNG has no quality knob — anything lower is far better served by JPEG.
/// Everything else (JPEG, WebP, GIF, BMP, TIFF) is encoded as JPEG so the quality
/// slider is always meaningful. Mirrors and improves the original Python behaviour.
pub fn pick_compress_format(input: ImageFormat, quality: u8) -> ImageFormat {
    match input {
        ImageFormat::Png if quality >= 90 => ImageFormat::Png,
        ImageFormat::Jpeg => ImageFormat::Jpeg,
        _ => ImageFormat::Jpeg,
    }
}

/// Strip the extension from a sanitised base name.
pub fn base_name(original: &str) -> String {
    let safe = crate::sanitize::sanitize_filename(original);
    match safe.rsplit_once('.') {
        Some((base, _)) if !base.is_empty() => base.to_string(),
        _ => safe,
    }
}

/// Validate a client-supplied quality value. Accepts `None`/empty (→ `default`),
/// rejects non-numeric input and anything outside 1..=100.
pub fn parse_quality(raw: Option<&str>, default: u8) -> AppResult<u8> {
    let Some(s) = raw else {
        return Ok(default);
    };
    let s = s.trim();
    if s.is_empty() {
        return Ok(default);
    }
    let v: i64 = s.parse().map_err(|_| {
        AppError::BadRequest(format!(
            "invalid quality '{s}': must be an integer between 1 and 100"
        ))
    })?;
    if !(1..=100).contains(&v) {
        return Err(AppError::BadRequest(format!(
            "quality {v} out of range: must be between 1 and 100"
        )));
    }
    Ok(v as u8)
}
