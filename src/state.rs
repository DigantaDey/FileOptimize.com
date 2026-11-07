//! Shared application state and the per-request scratch directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::Config;
use crate::engine::Engine;
use crate::error::AppResult;

/// Cheap to clone; the inner `Arc`s make it effectively a handle to shared state.
///
/// The HTTP layer owns *transport* concerns and delegates everything else to
/// [`Engine`], so an embedder can construct an `Engine` on its own and never touch a
/// router at all.
#[derive(Clone)]
pub struct AppState {
    /// Effective configuration, shared with the engine so neither can drift.
    pub config: Arc<Config>,
    /// The processing engine every handler runs its work through.
    pub engine: Arc<Engine>,
    pub tmp_dir: Arc<PathBuf>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        AppState::from_arc(Arc::new(config))
    }

    /// Build the state around an existing shared config (handy when the embedder keeps
    /// a handle to it as well).
    pub fn from_arc(config: Arc<Config>) -> Self {
        let tmp_dir = Arc::new(config.tmp_dir.clone());
        let engine = Arc::new(Engine::from_config(Arc::clone(&config)));
        AppState {
            config,
            engine,
            tmp_dir,
        }
    }

    /// Create a fresh, unique working directory for one request. The returned
    /// [`WorkDir`] removes itself on drop unless the caller keeps it alive for a
    /// streamed response.
    pub async fn new_workdir(&self) -> AppResult<WorkDir> {
        self.engine.new_workdir().await
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
    /// Wrap an existing directory. Prefer [`AppState::new_workdir`], which also makes
    /// sure the directory exists and is uniquely named.
    pub fn new(path: PathBuf) -> Self {
        WorkDir { path, keep: false }
    }

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
