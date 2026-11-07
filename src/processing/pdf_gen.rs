//! Minimal, dependency-free PDF generation for the image→PDF action.
//!
//! The image is embedded as a single full-page image XObject using `/DCTDecode`
//! (i.e. the JPEG bytes are stored verbatim — no transcode), and the cross-reference
//! table is hand-written. This keeps the binary self-contained (no PDF library, no
//! native dependencies), gives us exact control over output size, and avoids the
//! multi-hundred-KB object model a generic PDF writer would allocate per page.

/// Build a one-page PDF embedding `jpeg` (DCTDecode).
///
/// `width`×`height` are pixels; the page box is derived using `dpi` so the document
/// prints at a sane physical size (`points = pixels * 72 / dpi`).
pub fn build_image_pdf(jpeg: &[u8], width: u32, height: u32, dpi: f64) -> Vec<u8> {
    let scale = 72.0 / dpi.max(1.0);
    let w = ((f64::from(width) * scale).round() as u32).max(1);
    let h = ((f64::from(height) * scale).round() as u32).max(1);

    // Rough header + trailer overhead; the image dominates the estimate.
    let mut out: Vec<u8> = Vec::with_capacity(jpeg.len() + 1024);
    let mut offsets: Vec<usize> = Vec::with_capacity(8);

    let push_obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, num: u32, body: &[u8]| {
        offsets.push(out.len());
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    };

    // Header + a binary comment line so tooling treats the file as binary.
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");

    // 1 catalog, 2 pages, 3 page, 4 image XObject, 5 content stream.
    push_obj(
        &mut out,
        &mut offsets,
        1,
        b"<</Type /Catalog /Pages 2 0 R>>",
    );
    push_obj(
        &mut out,
        &mut offsets,
        2,
        b"<</Type /Pages /Kids [3 0 R] /Count 1>>",
    );

    let page_body = format!(
        "<</Type /Page /Parent 2 0 R /MediaBox [0 0 {w} {h}] /Resources <</XObject <</Im0 4 0 R>>>> /Contents 5 0 R>>"
    );
    push_obj(&mut out, &mut offsets, 3, page_body.as_bytes());

    let header = format!(
        "<</Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /Length {len}>>\nstream\n",
        len = jpeg.len()
    );
    let mut img_body: Vec<u8> = Vec::with_capacity(header.len() + jpeg.len() + 16);
    img_body.extend_from_slice(header.as_bytes());
    img_body.extend_from_slice(jpeg);
    img_body.extend_from_slice(b"\nendstream");
    push_obj(&mut out, &mut offsets, 4, &img_body);

    // `q W 0 0 -H 0 H cm /Im0 Do Q` maps the unit-square image onto the whole page,
    // flipping Y so it renders upright instead of mirrored.
    let contents = format!("q {w} 0 0 {neg_h} 0 {h} cm /Im0 Do Q", neg_h = -(h as i64));
    let len = contents.len();
    let stream = format!("<</Length {len}>>\nstream\n{contents}\nendstream");
    push_obj(&mut out, &mut offsets, 5, stream.as_bytes());

    // Cross-reference table.
    let xref_pos = out.len();
    let size = offsets.len() + 1;
    out.extend_from_slice(format!("xref\n{size}\n").as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<</Size {size} /Root 1 0 R>>\nstartxref\n{xref_pos}\n%%EOF\n").as_bytes(),
    );

    out
}
