//! Image encoding with per-format quality control.
//!
//! The input is converted to 8-bit RGB before encoding (matching the original
//! service's behaviour of dropping alpha for compressed output). `into_rgb8()` is
//! used rather than `to_rgb8()` so the decoded buffer is consumed in place instead of
//! being copied — a meaningful saving on large images.

use ::image::codecs::jpeg::JpegEncoder;
use ::image::codecs::png::PngEncoder;
use ::image::{DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder, ImageFormat};

use crate::error::AppResult;

/// Encode `img` to `format` at `quality` (1..=100) into a fresh byte buffer.
///
/// Only the two formats we can honour exactly are supported here: JPEG (lossy, quality
/// driven) and PNG (lossless). The `image` crate's pure-Rust WebP encoder is
/// lossless-only, so WebP output is deliberately not offered — WebP input is
/// re-encoded to JPEG instead, which is strictly better for the "compress" use case.
pub fn encode_image(img: DynamicImage, format: ImageFormat, quality: u8) -> AppResult<Vec<u8>> {
    let rgb = img.into_rgb8();
    let (w, h) = rgb.dimensions();
    let raw = rgb.as_raw();
    let mut out: Vec<u8> = Vec::new();

    match format {
        ImageFormat::Png => {
            // PNG has no quality knob; the default encoder already uses the
            // adaptive filter + best-effort deflate.
            PngEncoder::new(&mut out).write_image(raw, w, h, ExtendedColorType::Rgb8)?;
        }
        // Everything else we offer is JPEG, whose encoder honours 1..=100 quality.
        _ => {
            JpegEncoder::new_with_quality(&mut out, quality)
                .write_image(raw, w, h, ExtendedColorType::Rgb8)?;
        }
    }
    Ok(out)
}
