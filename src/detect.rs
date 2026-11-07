//! Content-based file-type detection. We sniff magic bytes rather than trusting the
//! client-supplied extension or MIME type, which makes the service robust against
//! mislabelled uploads (e.g. a PDF renamed `.jpg`).

use image::ImageFormat;

/// The kind of file we detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Image(ImageFormat),
    Pdf,
    Unknown,
}

/// Detect a file kind from its leading bytes. Only a small header is needed: PDF is a
/// literal prefix and image formats are identified by their signatures.
pub fn detect(data: &[u8]) -> FileKind {
    if is_pdf(data) {
        return FileKind::Pdf;
    }
    if let Ok(fmt) = image::guess_format(data) {
        return FileKind::Image(fmt);
    }
    FileKind::Unknown
}

/// `true` when the bytes start with the `%PDF-` header.
pub fn is_pdf(data: &[u8]) -> bool {
    data.starts_with(b"%PDF-")
}

/// Cheap scan for a CMYK/YCCK JPEG (Adobe APP14 with colour transform 2).
///
/// Such files must not be embedded verbatim into a `/DeviceRGB` PDF image XObject, so
/// the caller falls back to a full decode + re-encode when this returns `true`. Only
/// the marker chain is walked (never the entropy-coded data), so this stays O(#markers)
/// and needs no JPEG decoder.
///
/// Layout of the APP14 segment we look for (`FF EE` `len` `"Adobe\0"` `ver` `f0` `f1`
/// `transform` …), where `len` counts the two length bytes themselves.
pub fn jpeg_is_cmyk(data: &[u8]) -> bool {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return false;
    }
    let mut i = 2usize;
    while i + 1 < data.len() {
        if data[i] != 0xFF {
            return false;
        }
        // Fill bytes (0xFF runs) are legal before a marker; skip them.
        let mut j = i + 1;
        while j < data.len() && data[j] == 0xFF {
            j += 1;
        }
        if j >= data.len() {
            return false;
        }
        let marker = data[j];
        // Standalone markers with no payload: TEM and RSTn..SOI/EOI.
        if (0xD0..=0xD9).contains(&marker) || marker == 0x01 {
            i = j + 1;
            continue;
        }
        if j + 3 > data.len() {
            return false;
        }
        let len = u16::from_be_bytes([data[j + 1], data[j + 2]]) as usize;
        if len < 2 {
            return false;
        }
        let payload = j + 3; // first byte after the length field
        let segment_end = j + 1 + len; // exclusive

        if marker == 0xEE && len >= 12 {
            // Adobe APP14: "Adobe" + terminator, then version/flags/transform.
            let end = payload + 10;
            if end <= data.len()
                && &data[payload..payload + 5] == b"Adobe"
                && data[payload + 5] == 0x00
                && data[payload + 9] == 2
            {
                return true; // transform 2 == YCCK
            }
        }
        if marker == 0xDA {
            return false; // start of scan — the rest is entropy-coded data
        }
        if segment_end <= j + 3 {
            return false; // malformed, avoid an infinite loop
        }
        i = segment_end;
    }
    false
}
