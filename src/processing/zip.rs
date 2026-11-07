//! Streaming ZIP archive writer for the multi-page PDF→image result.
//!
//! Pages are added straight from their on-disk temp files via `io::copy`, so the whole
//! rendered set is never held in memory at once — only an 8 KiB copy buffer per entry.
//!
//! Entries are written with `Stored` rather than deflated: the payload is PNG, i.e.
//! already DEFLATE-compressed, so a second pass costs CPU and buys well under one
//! percent of size. That also lets us depend on the `zip` crate with no compression
//! backends compiled in at all.

use std::fs::File;
use std::path::Path;

use zip::write::{SimpleFileOptions, ZipWriter};
use zip::CompressionMethod;

use crate::error::AppResult;

/// Write a ZIP archive at `path` containing `files` (display name → source path).
pub fn write_zip_from_files(path: &Path, files: &[(String, std::path::PathBuf)]) -> AppResult<()> {
    let file = File::create(path)?;
    let mut zip = ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (name, src) in files {
        zip.start_file(name.clone(), options)?;
        let mut f = File::open(src)?;
        std::io::copy(&mut f, &mut zip)?;
    }
    zip.finish()?;
    Ok(())
}
