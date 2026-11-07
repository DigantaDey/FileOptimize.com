//! PDF operations backed by industry-standard external tools:
//! - [`compress_pdf`] shells out to Ghostscript (`gs`) for high-quality PDF re-compression.
//! - [`pdf_to_images`] shells out to Poppler (`pdftoppm`) to rasterise pages.
//!
//! Both run as real async child processes with a wall-clock timeout, so a pathological
//! PDF can never pin a worker or hold a concurrency permit forever. Tool presence is
//! probed at startup and surfaced via `/api/info`; a missing tool yields a clear
//! [`AppError::ExternalToolMissing`] rather than a panic or a corrupt output.

use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{AppError, AppResult};

/// `true` if the Ghostscript binary is on PATH.
pub async fn gs_available() -> bool {
    probe("gs", &["--version"]).await
}

/// `true` if the Poppler `pdftoppm` binary is on PATH.
pub async fn pdftoppm_available() -> bool {
    probe("pdftoppm", &["-v"]).await
}

async fn probe(bin: &str, args: &[&str]) -> bool {
    Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .is_ok()
}

/// Spawn `cmd`, wait for it under `timeout`, and return its status plus captured stderr.
async fn wait_capped(
    mut child: tokio::process::Child,
    tool: &str,
    timeout: Duration,
) -> AppResult<(ExitStatus, String)> {
    // Drain stderr concurrently so a chatty child can never deadlock on a full pipe.
    let stderr = child.stderr.take().map(drain_to_string);

    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Err(_) => {
            let _ = child.start_kill();
            tracing::error!(tool, secs = timeout.as_secs(), "external tool timed out");
            return Err(AppError::Internal(format!(
                "{tool} timed out after {}s and was killed",
                timeout.as_secs()
            )));
        }
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(AppError::Internal(format!("failed to run {tool}: {e}"))),
    };

    let text = match stderr {
        Some(fut) => fut.await.unwrap_or_default(),
        None => String::new(),
    };
    Ok((status, text))
}

async fn drain_to_string(mut r: impl AsyncReadExt + Unpin) -> String {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

/// Map a 1..=100 quality to a Ghostscript `/PDFSETTINGS` preset.
/// Lower quality ⇒ smaller file; the mapping matches the original Python service.
fn gs_preset(quality: u8) -> &'static str {
    if quality <= 25 {
        "/screen"
    } else if quality <= 50 {
        "/ebook"
    } else if quality <= 75 {
        "/printer"
    } else {
        "/prepress"
    }
}

/// Compress `input` into `output` with Ghostscript.
///
/// The file is streamed by the tool itself — it never passes through our address space.
pub async fn compress_pdf(
    input: &Path,
    output: &Path,
    quality: u8,
    timeout: Duration,
) -> AppResult<()> {
    let preset = gs_preset(quality);
    let child = Command::new("gs")
        .arg("-sDEVICE=pdfwrite")
        .arg("-dCompatibilityLevel=1.4")
        .arg(format!("-dPDFSETTINGS={preset}"))
        .arg("-dNOPAUSE")
        .arg("-dBATCH")
        .arg("-dQUIET")
        .arg("-dSAFER")
        .arg("-dDetectDuplicateImages=true")
        .arg(format!("-dColorImageResolution={}", 72 + u32::from(quality) * 3))
        .arg(format!("-sOutputFile={}", output.display()))
        .arg("--")
        .arg(input)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            tracing::warn!(?e, "ghostscript not runnable");
            AppError::ExternalToolMissing(
                "ghostscript (`gs`) is required for PDF compression but is not installed".to_string(),
            )
        })?;

    let (status, stderr) = wait_capped(child, "ghostscript", timeout).await?;
    if !status.success() {
        tracing::warn!(code = ?status.code(), stderr = %stderr.trim(), "ghostscript failed");
        return Err(AppError::Internal(
            "ghostscript could not process this PDF (it may be damaged or encrypted)".to_string(),
        ));
    }
    let out_len = tokio::fs::metadata(output).await.map(|m| m.len()).unwrap_or(0);
    if out_len == 0 {
        return Err(AppError::Internal(
            "ghostscript produced no output for this PDF".to_string(),
        ));
    }
    Ok(())
}

/// Rasterise every page of `input` into `out_dir` as PNGs at `dpi`.
/// Returns the produced file paths sorted in page order.
pub async fn pdf_to_images(
    input: &Path,
    out_dir: &Path,
    dpi: u32,
    timeout: Duration,
) -> AppResult<Vec<PathBuf>> {
    tokio::fs::create_dir_all(out_dir).await?;
    let prefix = out_dir.join("page");

    let child = Command::new("pdftoppm")
        .arg("-png")
        .arg(format!("-r{dpi}"))
        .arg("--")
        .arg(input)
        .arg(&prefix)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            tracing::warn!(?e, "pdftoppm not runnable");
            AppError::ExternalToolMissing(
                "poppler-utils (`pdftoppm`) is required for PDF→image but is not installed".to_string(),
            )
        })?;

    let (status, stderr) = wait_capped(child, "pdftoppm", timeout).await?;
    if !status.success() {
        tracing::warn!(code = ?status.code(), stderr = %stderr.trim(), "pdftoppm failed");
        return Err(AppError::Internal(
            "the PDF could not be rendered (it may be damaged or password-protected)".to_string(),
        ));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut rd = tokio::fs::read_dir(out_dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            files.push(path);
        }
    }
    // pdftoppm zero-pads page numbers, so a lexical sort is also a correct page sort.
    files.sort();
    if files.is_empty() {
        return Err(AppError::Internal(
            "no pages could be rendered from the PDF".to_string(),
        ));
    }
    Ok(files)
}
