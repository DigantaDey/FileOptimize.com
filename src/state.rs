//! Shared application state and the per-request scratch directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

use crate::config::Config;
use crate::error::AppResult;

/// Cheap to clone; the inner `Arc`s make it effectively a handle to shared state.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Limits concurrent processing jobs — the primary memory-bounding mechanism.
    pub semaphore: Arc<Semaphore>,
    pub tmp_dir: Arc<PathBuf>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        let tmp_dir = Arc::new(config.tmp_dir.clone());
        AppState {
            config: Arc::new(config),
            semaphore,
            tmp_dir,
        }
    }

    /// Create a fresh, unique working directory for one request. The returned
    /// [`WorkDir`] removes itself on drop unless the caller keeps it alive for a
    /// streamed response.
    pub async fn new_workdir(&self) -> AppResult<WorkDir> {
        let dir = self.tmp_dir.join(uuid::Uuid::new_v4().simple().to_string());
        tokio::fs::create_dir_all(&dir).await?;
        Ok(WorkDir {
            path: dir,
            keep: false,
        })
    }
}

/// RAII guard for a request-scoped temporary directory.
///
/// Deleting on drop means *every* error path (bad upload, unsupported type, tool
/// failure) still frees disk — the original service leaked files on error because its
/// cleanup lived after the response was already written.
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    keep: bool,
}

impl WorkDir {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path of a file inside this working directory.
    pub fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }

    /// Retain the directory after this guard is dropped (used when the response
    /// streams a file straight off disk). The background sweeper still collects it
    /// once it is older than `cleanup_ttl_secs`.
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
