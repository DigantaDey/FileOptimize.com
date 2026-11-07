//! Runtime configuration, sourced from environment variables with sensible,
//! defensive defaults. Every value is clamped into a safe range so a typo in
//! production env config degrades gracefully instead of wedging the service.

use std::path::PathBuf;

/// Immutable configuration for the server. Cloned into [`crate::state::AppState`].
#[derive(Debug, Clone)]
pub struct Config {
    /// Bind host. Defaults to `0.0.0.0` so the service is reachable in containers.
    pub host: String,
    /// Bind port. Defaults to `8080`.
    pub port: u16,
    /// Directory used for temporary upload/output artifacts.
    pub tmp_dir: PathBuf,
    /// Hard cap on the size (bytes) of a single uploaded file.
    pub max_upload_size: u64,
    /// Hard cap on image pixel count (width × height). Prevents memory exhaustion on
    /// decompression-bomb style inputs.
    pub max_image_pixels: u32,
    /// How long (seconds) temporary artifacts live before the background cleaner removes them.
    pub cleanup_ttl_secs: u64,
    /// Maximum number of concurrent processing jobs (the memory-bounding knob).
    pub max_concurrent: usize,
    /// Default JPEG quality (1-100) when the client omits `quality`.
    pub compression_default_quality: u8,
    /// DPI used when rasterising PDF pages to images.
    pub pdf_raster_dpi: u32,
    /// Wall-clock limit for external tools (ghostscript / pdftoppm).
    pub tool_timeout_secs: u64,
    /// Total memory (MB) the service may keep *in flight* across all concurrent jobs.
    /// Each job reserves an estimate of its peak (decoded pixels + buffers) from this
    /// budget before it decodes anything, so a burst of large images queues instead of
    /// swapping the process to death. `max_concurrent` bounds how many jobs run;
    /// `memory_budget_mb` bounds how much they may hold at once.
    pub memory_budget_mb: usize,
    /// API keys accepted on the versioned, machine-facing API (`/api/v1/*`).
    /// Empty (the default) means the v1 API is open, which is the right choice for a
    /// single-tenant deployment behind a gateway that already authenticates.
    pub api_keys: Vec<String>,
}

/// Name of the subdirectory of `TMP_DIR` that holds results clients asked to retain.
/// Kept out of the top-level sweep so it is never deleted wholesale.
pub const RESULTS_DIR_NAME: &str = "results";

/// Generous but safe ceilings for `MAX_IMAGE_PIXELS` (25 MP ≈ 750 MB decoded RGB).
const MIN_PIXELS: u32 = 100 * 100;
const MAX_PIXELS: u32 = 100 * 1_000_000;

impl Config {
    /// Build a `Config` from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let max_upload_mb = env_parsed::<u64>("MAX_UPLOAD_SIZE_MB", 50).clamp(1, 500);

        let cfg = Config {
            host: env_or("HOST", "0.0.0.0"),
            port: env_parsed("PORT", 8080),
            tmp_dir: env_path("TMP_DIR", PathBuf::from("./tmp")),
            max_upload_size: max_upload_mb * 1024 * 1024,
            max_image_pixels: env_parsed("MAX_IMAGE_PIXELS", 25_000_000)
                .clamp(MIN_PIXELS, MAX_PIXELS),
            cleanup_ttl_secs: env_parsed("CLEANUP_TTL_SECS", 1800).clamp(60, 86_400),
            max_concurrent: env_parsed("MAX_CONCURRENT", 4).clamp(1, 64),
            compression_default_quality: env_parsed("DEFAULT_QUALITY", 60u8).clamp(1, 100),
            pdf_raster_dpi: env_parsed("PDF_RASTER_DPI", 150u32).clamp(36, 600),
            tool_timeout_secs: env_parsed("TOOL_TIMEOUT_SECS", 120u64).clamp(5, 1800),
            memory_budget_mb: env_parsed::<usize>("MEMORY_BUDGET_MB", 1024).clamp(16, 65_000),
            api_keys: env_list("API_KEYS"),
        };
        cfg.validate();
        cfg
    }

    /// Fail fast on obviously unusable config (called at startup, not per request).
    fn validate(&self) {
        assert!(self.max_concurrent >= 1, "MAX_CONCURRENT must be >= 1");
        assert!(self.max_upload_size > 0, "MAX_UPLOAD_SIZE must be > 0");
    }

    /// `true` when the v1 API must present one of [`Config::api_keys`].
    pub fn requires_api_key(&self) -> bool {
        !self.api_keys.is_empty()
    }

    /// Accept `key` if the API is open, or if it matches a configured key exactly.
    pub fn accepts_api_key(&self, key: &str) -> bool {
        let key = key.trim();
        !key.is_empty() && self.api_keys.iter().any(|k| k == key)
    }

    /// Directory that retained results live in, relative to [`Config::tmp_dir`].
    pub fn results_dir(&self) -> PathBuf {
        self.tmp_dir.join(RESULTS_DIR_NAME)
    }

    /// `DefaultBodyLimit` headroom: the multipart framing, part headers and the
    /// `action`/`quality` fields ride along with the file, so the body limit is the
    /// file cap plus a small allowance.
    pub fn body_limit(&self) -> usize {
        self.max_upload_size
            .saturating_add(64 * 1024)
            .try_into()
            .unwrap_or(usize::MAX)
    }
}

fn env_or(key: &str, default: &str) -> String {
    let raw = std::env::var(key).unwrap_or_default();
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn env_parsed<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .unwrap_or(default)
}

/// Parse a comma/space separated list, dropping empty entries.
fn env_list(key: &str) -> Vec<String> {
    std::env::var(key)
        .ok()
        .map(|raw| {
            raw.split([',', ' ', '\n', '\t'])
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn env_path(key: &str, default: PathBuf) -> PathBuf {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or(default)
}
