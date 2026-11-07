//! Filename sanitisation helpers. Uploaded files are always stored under a random
//! UUID inside a per-request working directory, so a client-supplied name is never used
//! for path construction. It is still sanitised before it is echoed back in
//! `Content-Disposition`, where injection (quotes, newlines, control chars) matters.

use uuid::Uuid;

/// Longest base name we keep, so a hostile 10 KiB filename can't bloat headers.
const MAX_BASE_LEN: usize = 128;

/// Reduce an arbitrary filename to a safe, printable base name (no path components,
/// no control/unsafe characters, no leading/trailing dots that could hide traversal).
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut out = String::with_capacity(base.len().min(MAX_BASE_LEN + 8));
    for c in base.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
            out.push(c);
        } else {
            out.push('-');
        }
        if out.len() >= MAX_BASE_LEN {
            break;
        }
    }
    let trimmed = out.trim().trim_matches('.').trim();
    // Nothing but separators left (e.g. an all-emoji or all-dots name) is not a usable
    // filename, so fall back to a neutral placeholder.
    if trimmed.is_empty() || trimmed.chars().all(|c| matches!(c, '-' | '.' | ' ')) {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Extract a lowercased, validated extension (no dot) from a filename. Falls back
/// to `"bin"` when absent or implausible.
pub fn extension_of(name: &str) -> String {
    let safe = sanitize_filename(name);
    safe.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| is_valid_ext(ext))
        .unwrap_or_else(|| "bin".to_string())
}

/// Sanitise an arbitrary extension string for use in an output filename.
pub fn sanitize_ext(ext: &str) -> String {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    if is_valid_ext(&e) {
        e
    } else {
        "bin".to_string()
    }
}

fn is_valid_ext(ext: &str) -> bool {
    !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Produce a collision-free storage name `{uuid}.{ext}`.
pub fn stored_filename(ext: &str) -> String {
    format!("{}.{}", Uuid::new_v4().simple(), sanitize_ext(ext))
}
