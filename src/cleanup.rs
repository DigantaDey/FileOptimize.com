//! Background janitor that removes stale temporary working directories.
//!
//! Outputs for PDF→image (and compressed PDFs) are streamed lazily off disk, so they
//! cannot be deleted the moment the response is built. This TTL sweeper collects them —
//! mirroring the original Python service's "delete after 30 minutes" behaviour, but
//! centralised, configurable and crash-safe.

use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::state::AppState;

/// Spawn the periodic cleaner as a detached tokio task.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let ttl = state.config.cleanup_ttl_secs.max(60);
        // Sweep every 10 minutes, but never less often than the TTL itself.
        let period = Duration::from_secs(ttl.min(600));
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tracing::info!(ttl_secs = ttl, ?period, "temp-file cleaner started");
        loop {
            ticker.tick().await;
            if let Err(e) = sweep(&state.tmp_dir, ttl).await {
                tracing::warn!("cleanup sweep error: {e}");
            }
        }
    });
}

/// One pass over the temp root (and over the retained-results store), so it can be
/// unit-tested directly. Returns how many directories were removed.
pub async fn sweep(tmp_dir: &Path, ttl: u64) -> std::io::Result<usize> {
    // The results directory is a container, not an artifact: it must survive even when
    // its own mtime is older than the TTL, while its entries expire like any work dir.
    let results = tmp_dir.join(crate::config::RESULTS_DIR_NAME);
    let mut removed = sweep_level(tmp_dir, ttl, Some(crate::config::RESULTS_DIR_NAME)).await?;
    removed += sweep_level(&results, ttl, None).await?;
    if removed > 0 {
        tracing::info!(removed, "cleanup sweep finished");
    }
    Ok(removed)
}

async fn sweep_level(dir: &Path, ttl: u64, skip: Option<&str>) -> std::io::Result<usize> {
    let mut rd = match tokio::fs::read_dir(dir).await {
        Ok(rd) => rd,
        // No temp dir yet is fine.
        Err(_) => return Ok(0),
    };
    let now = SystemTime::now();
    let mut removed = 0usize;
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        let meta = match entry.metadata().await {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_dir() {
            continue;
        }
        if let Some(keep) = skip {
            if entry.file_name().to_string_lossy() == keep {
                continue;
            }
        }
        let age = now
            .duration_since(meta.modified().unwrap_or(now))
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX);
        if age >= ttl {
            match tokio::fs::remove_dir_all(&path).await {
                Ok(()) => {
                    removed += 1;
                    tracing::debug!(path = %path.display(), "removed stale work dir");
                }
                Err(e) => tracing::warn!(path = %path.display(), "failed to remove stale dir: {e}"),
            }
        }
    }
    Ok(removed)
}
