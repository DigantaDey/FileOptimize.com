//! The processing engine — the whole product, independent of any transport.
//!
//! [`Engine`] is the one place that knows how a file is validated, bounded,
//! transformed and cleaned up. The HTTP layer in [`crate::routes`] is a thin adapter in
//! front of it, and an embedder can skip HTTP entirely and call the engine directly:
//!
//! ```no_run
//! use fileoptimize::engine::{Action, Engine, Input, Job};
//!
//! # async fn demo() -> Result<(), fileoptimize::error::AppError> {
//! let engine = Engine::from_env();
//! let bytes = std::fs::read("holiday.jpg").unwrap_or_default();
//! let outcome = engine
//!     .run(Job::new(Action::Compress, "holiday.jpg", Input::Bytes(bytes)).with_quality(65))
//!     .await?;
//!
//! // Everything a client or another service needs in order to report the job:
//! let saved = outcome.original_size.saturating_sub(outcome.output.size());
//! let download_name = outcome.output.filename.clone(); // sanitised, header-safe
//!
//! // The payload is either in memory or on disk; `read_all` covers both.
//! let payload = outcome.output.read_all()?;
//! assert!(!payload.is_empty());
//! assert!(!download_name.is_empty() || saved > 0);
//! # Ok(())
//! # }
//! ```
//!
//! # Guarantees every caller gets
//!
//! * input size, image dimensions, action and quality are validated *before* any
//!  expensive work happens — the same checks the HTTP API applies;
//! * one [`tokio::sync::Semaphore`] bounds how many jobs run at once and a second,
//!   weighted one bounds the *bytes* they may hold at once
//!   (`Config::memory_budget_mb`), so peak RSS is
//!   `≈ min(max_concurrent, budget ÷ per-job estimate)` instead of unbounded;
//! * every scratch file lives in a UUID-named directory whose [`WorkDir`] guard removes
//!   it on drop — including on every error path;
//! * results a caller asks to retain ([`Job::stored`]) are addressed by an unguessable
//!   id, live under `TMP_DIR/results`, and are collected by the same TTL sweeper as
//!   everything else, so retention cannot leak disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::detect::{detect, jpeg_is_cmyk, FileKind};
use crate::error::{AppError, AppResult};
use crate::processing::image::encode_image;
use crate::processing::pdf::{compress_pdf, gs_available, pdf_to_images, pdftoppm_available};
use crate::processing::pdf_gen::build_image_pdf;
use crate::processing::zip::write_zip_from_files;
use crate::processing::{
    base_name, check_dimensions, content_type_of, decode_checked, ext_of, parse_quality,
    pick_compress_format, read_header, Output,
};
use crate::sanitize::sanitize_filename;
use crate::state::WorkDir;

/// DPI assumed when turning image pixels into PDF points (72 pt per inch).
const IMAGE_PDF_DPI: f64 = 100.0;
/// Quality used when a non-JPEG image must be re-encoded for embedding in a PDF.
const IMAGE_PDF_EMBED_QUALITY: u8 = 90;
/// Bytes of headroom charged to a job whose work happens inside an external tool
/// (those stream from disk, but they still touch page and object buffers).
const TOOL_JOB_BASE_BYTES: usize = 8 * 1024 * 1024;
const MIB: usize = 1024 * 1024;
/// Bytes read from the start of a file to identify it. Enough for every signature we
/// route on, and it never pulls a large upload into memory just to name it.
const SNIFF_BYTES: usize = 64;

/// The transforms this engine performs. The names are the wire format for both the HTTP
/// form field and the JSON API, so they are part of the public contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Re-encode an image at a given quality, or re-distil a PDF with Ghostscript.
    Compress,
    /// Wrap a single image in a one-page PDF.
    ImageToPdf,
    /// Rasterise every page of a PDF: one page → PNG, several → ZIP.
    PdfToImage,
}

impl Action {
    /// Parse a client-supplied action name. Hyphenated spellings are accepted because
    /// the original Python API's JavaScript used them.
    pub fn parse(raw: &str) -> AppResult<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "compress" => Ok(Action::Compress),
            "image_to_pdf" | "image-to-pdf" => Ok(Action::ImageToPdf),
            "pdf_to_image" | "pdf-to-image" => Ok(Action::PdfToImage),
            other => Err(AppError::BadRequest(format!(
                "invalid action '{other}': expected compress, image_to_pdf or pdf_to_image"
            ))),
        }
    }

    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Compress => "compress",
            Action::ImageToPdf => "image_to_pdf",
            Action::PdfToImage => "pdf_to_image",
        }
    }
}

/// Where the input comes from.
#[derive(Debug)]
pub enum Input {
    /// Caller-held bytes. Checked against `Config::max_upload_size` before anything is
    /// done with them; kept in memory for image work and written to scratch only when a
    /// PDF tool genuinely needs a file.
    Bytes(Vec<u8>),
    /// A file the caller owns. It is opened read-only, never moved or deleted, and its
    /// size is checked against the cap.
    Path(PathBuf),
    /// A file already inside a [`WorkDir`] — the HTTP layer uses this so the engine can
    /// write its outputs next to the upload and the same guard cleans up both.
    Uploaded { dir: WorkDir, path: PathBuf },
}

/// A unit of work for [`Engine::run`].
#[derive(Debug)]
pub struct Job {
    /// What to do.
    pub action: Action,
    /// 1–100, meaningful for [`Action::Compress`]. `0` means "use the server default".
    pub quality: u8,
    /// The client's filename. Sanitised by the engine — pass it as received.
    pub filename: String,
    /// The file itself.
    pub input: Input,
    /// Retain the result under a [`JobId`] for later retrieval.
    pub store: bool,
}

impl Job {
    /// Job with the configured defaults for everything but the file.
    pub fn new(action: Action, filename: impl Into<String>, input: Input) -> Self {
        Job {
            action,
            quality: 0,
            filename: filename.into(),
            input,
            store: false,
        }
    }

    /// Build a job from raw, untrusted field values — the path an HTTP or RPC adapter
    /// uses. `None` quality means "not supplied", which is resolved later by the engine
    /// so the configured default applies.
    pub fn from_fields(
        action: Option<&str>,
        quality: Option<&str>,
        filename: Option<&str>,
        input: Input,
        store: bool,
        default_quality: u8,
    ) -> AppResult<Self> {
        let action = Action::parse(action.unwrap_or(""))?;
        let quality = parse_quality(quality, default_quality)?;
        Ok(Job {
            action,
            quality,
            filename: filename.unwrap_or("file").to_string(),
            input,
            store,
        })
    }

    /// `0` means "use the server default", resolved inside [`Engine::run`] so that
    /// library callers and HTTP clients behave identically.
    pub fn with_quality(mut self, quality: u8) -> Self {
        self.quality = quality;
        self
    }

    /// Keep the result addressable after the response.
    pub fn stored(mut self) -> Self {
        self.store = true;
        self
    }
}

/// Id of a retained result: 32 lowercase hex characters, and nothing else. The strict
/// parse is what makes it safe to use a client-supplied id as a directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobId(String);

impl JobId {
    /// New random id. UUIDv4 gives 122 bits of entropy, so ids are unguessable.
    pub fn random() -> Self {
        JobId(uuid::Uuid::new_v4().simple().to_string())
    }

    /// Validate an id from an untrusted source (a URL path segment).
    pub fn parse(raw: &str) -> AppResult<Self> {
        // Lowercase hex only: exactly what `Uuid::simple` produces, and it makes `..`,
        // separators and case tricks impossible before the id reaches the filesystem.
        let ok = raw.len() == 32
            && raw
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        if ok {
            Ok(JobId(raw.to_string()))
        } else {
            Err(AppError::BadRequest(
                "result id must be 32 hexadecimal characters".to_string(),
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Metadata written next to a retained result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMeta {
    /// The id the result is addressable by.
    pub id: String,
    /// Action that produced it.
    pub action: String,
    /// Download filename (sanitised).
    pub filename: String,
    /// Media type of the payload.
    pub content_type: String,
    /// The client's filename, sanitised.
    pub original_name: String,
    /// Bytes in.
    pub original_size: u64,
    /// Bytes out.
    pub output_size: u64,
    /// Seconds since the Unix epoch when it was stored.
    pub created_unix: u64,
    /// Retention window advertised to clients; the sweeper collects it after this.
    pub max_age_secs: u64,
}

/// The result of [`Engine::run`].
#[derive(Debug)]
pub struct JobOutcome {
    /// The transformed file, in memory or on disk.
    pub output: Output,
    /// Sanitised name of the input, safe to echo in a header.
    pub original_name: String,
    /// Size of the input in bytes.
    pub original_size: u64,
    /// Quality actually used (the caller's, or the configured default).
    pub quality: u8,
    /// Set when the job asked for retention.
    pub id: Option<JobId>,
    /// Metadata for a retained result.
    pub meta: Option<ResultMeta>,
}

impl JobOutcome {
    /// Bytes saved by the transform (may be negative in spirit, so saturating).
    pub fn bytes_saved(&self) -> u64 {
        self.original_size.saturating_sub(self.output.size())
    }

    /// `true` when the payload is a file that must outlive this call.
    pub fn is_streamed(&self) -> bool {
        self.output.is_streamed()
    }
}

/// Which external tools this host has. Serialized into `/api/info`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Tools {
    /// Ghostscript, needed to compress PDFs.
    pub ghostscript: bool,
    /// Poppler's `pdftoppm`, needed to rasterise PDF pages.
    pub poppler: bool,
}

impl Tools {
    /// `true` when every PDF action can run.
    pub fn all_present(&self) -> bool {
        self.ghostscript && self.poppler
    }
}

/// The engine handle: cheap to clone (it is `Arc`-internal), safe to share across
/// tasks, and the only object needed to embed FileOptimize in another application.
pub struct Engine {
    config: Arc<Config>,
    /// Bounds how many transforms run at once.
    gate: Arc<Semaphore>,
    /// Weighted in the unit of 1 MiB: bounds how many bytes all running transforms may
    /// hold at once. This is the knob that protects against memory pressure specifically.
    memory: Arc<Semaphore>,
    results: PathBuf,
    tools: tokio::sync::OnceCell<Tools>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("max_concurrent", &self.config.max_concurrent)
            .field("memory_budget_mb", &self.config.memory_budget_mb)
            .field("tmp_dir", &self.config.tmp_dir)
            .finish_non_exhaustive()
    }
}

impl Engine {
    /// Engine with an owned config.
    pub fn new(config: Config) -> Self {
        Engine::from_config(Arc::new(config))
    }

    /// Engine sharing an existing `Arc<Config>` — used by [`crate::state::AppState`] so
    /// the HTTP layer and the engine cannot disagree about a limit.
    pub fn from_config(config: Arc<Config>) -> Self {
        let gate = Arc::new(Semaphore::new(config.max_concurrent.max(1)));
        let memory = Arc::new(Semaphore::new(config.memory_budget_mb.max(1)));
        let results = config.results_dir();
        Engine {
            config,
            gate,
            memory,
            results,
            tools: tokio::sync::OnceCell::new(),
        }
    }

    /// Engine configured from the process environment.
    pub fn from_env() -> Self {
        Engine::new(Config::from_env())
    }

    /// Effective configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Shared handle to the configuration.
    pub fn config_arc(&self) -> Arc<Config> {
        Arc::clone(&self.config)
    }

    /// Root directory for scratch and retained results.
    pub fn tmp_dir(&self) -> &Path {
        &self.config.tmp_dir
    }

    /// Directory holding retained results.
    pub fn results_dir(&self) -> &Path {
        &self.results
    }

    /// MiB of the memory budget currently free.
    pub fn available_memory_mb(&self) -> usize {
        self.memory.available_permits()
    }

    /// Jobs currently running (0..=`Config::max_concurrent`).
    pub fn running_jobs(&self) -> usize {
        self.config
            .max_concurrent
            .saturating_sub(self.gate.available_permits())
    }

    /// Which external tools are installed. Probed once and cached for the process
    /// lifetime: presence cannot change while we run, and `/api/info` would otherwise
    /// `fork` two children per call.
    pub async fn tools(&self) -> Tools {
        *self
            .tools
            .get_or_init(|| async {
                let (ghostscript, poppler) = tokio::join!(gs_available(), pdftoppm_available());
                Tools {
                    ghostscript,
                    poppler,
                }
            })
            .await
    }

    /// A fresh, unique working directory whose guard deletes it on drop.
    pub async fn new_workdir(&self) -> AppResult<WorkDir> {
        let dir = self
            .config
            .tmp_dir
            .join(uuid::Uuid::new_v4().simple().to_string());
        tokio::fs::create_dir_all(&dir).await?;
        Ok(WorkDir::new(dir))
    }

    /// Run one transform, start to finish, with all limits enforced.
    pub async fn run(&self, job: Job) -> AppResult<JobOutcome> {
        let Job {
            action,
            quality,
            filename,
            input,
            store,
        } = job;

        let quality = if quality == 0 {
            self.config.compression_default_quality
        } else {
            quality
        };
        let original_name = sanitize_filename(&filename);

        let mut prepared = self.accept(input).await?;
        let head = prepared.header().await?;
        let kind = detect(&head);

        // Two independent bounds, taken in this order on purpose: the cheap concurrency
        // gate first (it never holds payload bytes), then the memory budget for the job
        // that is actually about to allocate. Waiting on memory while holding a slot is
        // the lesser evil to holding decoded pixels while waiting for a slot.
        let _gate = self
            .gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| AppError::Internal("processing gate closed".to_string()))?;
        let estimate = self.estimate(&kind, &prepared)?;
        let _memory = self.reserve_memory(estimate).await?;

        let mut output = match action {
            Action::Compress => {
                self.compress(&mut prepared, &kind, quality, &original_name)
                    .await?
            }
            Action::ImageToPdf => {
                self.image_to_pdf(&mut prepared, &kind, &original_name)
                    .await?
            }
            Action::PdfToImage => {
                self.pdf_to_image(&mut prepared, &kind, &original_name)
                    .await?
            }
        };

        let original_size = prepared.size;
        let mut work = prepared.work;

        let (id, meta) = if store {
            let meta = self
                .retain(&mut output, action, &original_name, original_size)
                .await?;
            (Some(JobId(meta.id.clone())), Some(meta))
        } else {
            (None, None)
        };

        // A streamed payload still on disk has to outlive this call: hand its directory
        // to the sweeper instead of deleting it here. A retained result already lives in
        // the results store, so its scratch dir can go.
        if output.is_streamed() && id.is_none() {
            work.keep();
        }

        Ok(JobOutcome {
            output,
            original_name,
            original_size,
            quality,
            id,
            meta,
        })
    }

    /// Fetch a retained result: a streamable [`Output`] plus its metadata.
    pub async fn result(&self, id: &JobId) -> AppResult<(Output, ResultMeta)> {
        let dir = self.results.join(id.as_str());
        let raw = tokio::fs::read(dir.join("meta.json"))
            .await
            .map_err(|_| AppError::NotFound("no retained result with that id".to_string()))?;
        let meta: ResultMeta = serde_json::from_slice(&raw)
            .map_err(|e| AppError::Internal(format!("unreadable result metadata: {e}")))?;
        let path = dir.join(&meta.filename);
        if !path.exists() {
            return Err(AppError::NotFound(
                "the retained result has expired".to_string(),
            ));
        }
        Ok((
            Output::from_path(path, meta.filename.clone(), &meta.content_type),
            meta,
        ))
    }

    /// Drop a retained result early. `false` when there was nothing to remove.
    pub async fn discard(&self, id: &JobId) -> AppResult<bool> {
        let dir = self.results.join(id.as_str());
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    // ------------------------------------------------------------------ internals

    /// Materialise the input inside a scratch directory and check its size.
    async fn accept(&self, input: Input) -> AppResult<Prepared> {
        match input {
            Input::Bytes(bytes) => {
                let size = bytes.len() as u64;
                self.check_size(size)?;
                let work = self.new_workdir().await?;
                Ok(Prepared {
                    work,
                    input: None,
                    inline: Some(bytes),
                    size,
                })
            }
            Input::Path(path) => {
                let size = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .map_err(|_| {
                        AppError::BadRequest(format!("cannot read the file at {}", path.display()))
                    })?;
                self.check_size(size)?;
                let work = self.new_workdir().await?;
                Ok(Prepared {
                    work,
                    input: Some(path),
                    inline: None,
                    size,
                })
            }
            Input::Uploaded { dir, path } => {
                let size = tokio::fs::metadata(&path)
                    .await
                    .map(|m| m.len())
                    .unwrap_or(0);
                self.check_size(size)?;
                Ok(Prepared {
                    work: dir,
                    input: Some(path),
                    inline: None,
                    size,
                })
            }
        }
    }

    fn check_size(&self, size: u64) -> AppResult<()> {
        if size == 0 {
            return Err(AppError::BadRequest("uploaded file is empty".to_string()));
        }
        if size > self.config.max_upload_size {
            return Err(AppError::PayloadTooLarge(format!(
                "file exceeds the maximum upload size of {} bytes",
                self.config.max_upload_size
            )));
        }
        Ok(())
    }

    /// Peak bytes this job may allocate. For images this is measured from the header
    /// (never a guess) so a decompression bomb is refused *before* it is decoded.
    fn estimate(&self, kind: &FileKind, prepared: &Prepared) -> AppResult<usize> {
        let needs_pixels = matches!(kind, FileKind::Image(_));
        if !needs_pixels {
            let base = TOOL_JOB_BASE_BYTES.saturating_add(prepared.size as usize / 4);
            return Ok(base.max(MIB));
        }
        let (w, h) = prepared.dimensions(kind)?;
        check_dimensions(w, h, self.config.max_image_pixels)?;
        // A decode allocates the RGBA buffer (4 B/px) and the RGB8 buffer we encode from
        // (3 B/px); the encoder and the raw file are charged on top. Round to 8 B/px so
        // the estimate stays honest without depending on codec internals.
        let pixels = w as usize * h as usize;
        let decode = pixels.saturating_mul(8);
        Ok(decode.saturating_add(prepared.size as usize).max(MIB))
    }

    async fn reserve_memory(&self, bytes: usize) -> AppResult<OwnedSemaphorePermit> {
        let need_mib = bytes.div_ceil(MIB).max(1);
        if need_mib > self.config.memory_budget_mb {
            return Err(AppError::PayloadTooLarge(format!(
                "this file needs an estimated {need_mib} MB of working memory, above the \
                 {budget} MB in-flight budget: lower MAX_IMAGE_PIXELS or raise \
                 MEMORY_BUDGET_MB",
                budget = self.config.memory_budget_mb
            )));
        }
        // Waiting for memory is normal backpressure; a closed semaphore means the
        // process is shutting down.
        self.memory
            .clone()
            .acquire_many_owned(need_mib as u32)
            .await
            .map_err(|_| AppError::Internal("memory budget closed".to_string()))
    }

    /// `compress` — images are re-encoded with a quality setting; PDFs go through
    /// Ghostscript (which streams the file itself, so it never passes through our
    /// address space).
    async fn compress(
        &self,
        prepared: &mut Prepared,
        kind: &FileKind,
        quality: u8,
        original_name: &str,
    ) -> AppResult<Output> {
        match kind {
            FileKind::Pdf => {
                if !self.tools().await.ghostscript {
                    return Err(tool_missing("ghostscript", "PDF compression"));
                }
                let input = prepared.materialise("input.pdf").await?;
                let out_path = prepared.work.join("compressed.pdf");
                compress_pdf(
                    &input,
                    &out_path,
                    quality,
                    Duration::from_secs(self.config.tool_timeout_secs),
                )
                .await?;
                let base = base_name(original_name);
                Ok(Output::from_path(
                    out_path,
                    format!("{base}_compressed.pdf"),
                    "application/pdf",
                ))
            }
            FileKind::Image(fmt) => {
                let data = prepared.take_bytes().await?;
                let fmt = *fmt;
                let max_pixels = self.config.max_image_pixels;
                // Codecs are CPU-bound: run them on the blocking pool so the async
                // runtime keeps accepting and streaming other requests.
                let (bytes, target) = tokio::task::spawn_blocking(move || {
                    let img = decode_checked(&data, fmt, max_pixels)?;
                    let target = pick_compress_format(fmt, quality);
                    Ok::<_, AppError>((encode_image(img, target, quality)?, target))
                })
                .await
                .map_err(join_err)??;

                let base = base_name(original_name);
                Ok(Output::in_memory(
                    bytes,
                    format!("{base}_optimized.{}", ext_of(target)),
                    content_type_of(target),
                ))
            }
            FileKind::Unknown => Err(AppError::UnsupportedMediaType(
                "only images and PDFs can be compressed".to_string(),
            )),
        }
    }

    /// `image_to_pdf` — wrap a single image in a one-page PDF.
    ///
    /// A baseline (non-CMYK) JPEG is embedded verbatim via `/DCTDecode`: no decode +
    /// re-encode round-trip at all, which means zero pixel-buffer allocation and no
    /// second lossy generation. Any other format is normalised to RGB JPEG first.
    async fn image_to_pdf(
        &self,
        prepared: &mut Prepared,
        kind: &FileKind,
        original_name: &str,
    ) -> AppResult<Output> {
        let fmt = match kind {
            FileKind::Image(f) => *f,
            FileKind::Pdf => {
                return Err(AppError::BadRequest(
                    "that is already a PDF — use compress instead".to_string(),
                ))
            }
            FileKind::Unknown => {
                return Err(AppError::UnsupportedMediaType(
                    "image_to_pdf requires an image file".to_string(),
                ))
            }
        };

        let data = prepared.take_bytes().await?;
        let max_pixels = self.config.max_image_pixels;
        let pdf = tokio::task::spawn_blocking(move || -> AppResult<Vec<u8>> {
            let (w, h) = ::image::ImageReader::with_format(std::io::Cursor::new(&data), fmt)
                .into_dimensions()?;
            check_dimensions(w, h, max_pixels)?;

            let jpeg: Vec<u8> = if fmt == ::image::ImageFormat::Jpeg && !jpeg_is_cmyk(&data) {
                data
            } else {
                let img = decode_checked(&data, fmt, max_pixels)?;
                encode_image(img, ::image::ImageFormat::Jpeg, IMAGE_PDF_EMBED_QUALITY)?
            };
            Ok(build_image_pdf(&jpeg, w, h, IMAGE_PDF_DPI))
        })
        .await
        .map_err(join_err)??;

        let base = base_name(original_name);
        Ok(Output::in_memory(
            pdf,
            format!("{base}.pdf"),
            "application/pdf",
        ))
    }

    /// `pdf_to_image` — rasterise pages. One page becomes a PNG, many become a ZIP.
    async fn pdf_to_image(
        &self,
        prepared: &Prepared,
        kind: &FileKind,
        original_name: &str,
    ) -> AppResult<Output> {
        if !matches!(kind, FileKind::Pdf) {
            return Err(AppError::UnsupportedMediaType(
                "pdf_to_image requires a PDF file".to_string(),
            ));
        }
        if !self.tools().await.poppler {
            return Err(tool_missing("poppler-utils", "PDF→image conversion"));
        }

        let input = prepared.materialise("input.pdf").await?;
        let img_dir = prepared.work.join("pages");
        let files = pdf_to_images(
            &input,
            &img_dir,
            self.config.pdf_raster_dpi,
            Duration::from_secs(self.config.tool_timeout_secs),
        )
        .await?;
        let base = base_name(original_name);

        if files.len() == 1 {
            let only = files.into_iter().next().expect("length checked above");
            let out_path = prepared.work.join(&format!("{base}.png"));
            // rename() can fail across devices; a copy is a safe fallback.
            if tokio::fs::rename(&only, &out_path).await.is_err() {
                tokio::fs::copy(&only, &out_path).await?;
                let _ = tokio::fs::remove_file(&only).await;
            }
            let _ = tokio::fs::remove_dir(&img_dir).await;
            return Ok(Output::from_path(
                out_path,
                format!("{base}.png"),
                "image/png",
            ));
        }

        let entries: Vec<(String, PathBuf)> = files
            .iter()
            .enumerate()
            .map(|(i, p)| (format!("page_{:04}.png", i + 1), p.clone()))
            .collect();
        let zip_path = prepared.work.join(&format!("{base}_images.zip"));
        // Archiving is disk + CPU bound, so it also runs off the reactor.
        let archive = zip_path.clone();
        tokio::task::spawn_blocking(move || write_zip_from_files(&archive, &entries))
            .await
            .map_err(join_err)??;
        // The pages are inside the archive now; drop them so the temp dir holds one file.
        for f in &files {
            let _ = tokio::fs::remove_file(f).await;
        }
        let _ = tokio::fs::remove_dir(&img_dir).await;

        Ok(Output::from_path(
            zip_path,
            format!("{base}_images.zip"),
            "application/zip",
        ))
    }

    /// Move or copy the finished payload into the results store and record its metadata.
    async fn retain(
        &self,
        output: &mut Output,
        action: Action,
        original_name: &str,
        original_size: u64,
    ) -> AppResult<ResultMeta> {
        let id = JobId::random();
        let dir = self.results.join(id.as_str());
        tokio::fs::create_dir_all(&dir).await?;
        let dest = dir.join(&output.filename);

        if let Some(data) = output.data.as_ref() {
            let mut file = tokio::fs::File::create(&dest).await?;
            file.write_all(data).await?;
            file.flush().await?;
        } else if let Some(src) = output.path.take() {
            // Prefer a rename: a multi-hundred-MB archive should not be copied. If the
            // results store is on another device, fall back to copy + unlink.
            if tokio::fs::rename(&src, &dest).await.is_err() {
                tokio::fs::copy(&src, &dest).await?;
                let _ = tokio::fs::remove_file(&src).await;
            }
            output.path = Some(dest);
        }

        let created_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let meta = ResultMeta {
            id: id.as_str().to_string(),
            action: action.as_str().to_string(),
            filename: std::mem::take(&mut output.filename),
            content_type: std::mem::take(&mut output.content_type),
            original_name: original_name.to_string(),
            original_size,
            output_size: output.size(),
            created_unix,
            max_age_secs: self.config.cleanup_ttl_secs,
        };
        // Put the names back for the caller's response.
        output.filename = meta.filename.clone();
        output.content_type = meta.content_type.clone();

        let json = serde_json::to_vec_pretty(&meta)
            .map_err(|e| AppError::Internal(format!("cannot serialise result metadata: {e}")))?;
        let mut file = tokio::fs::File::create(dir.join("meta.json")).await?;
        file.write_all(&json).await?;
        file.flush().await?;
        // The payload no longer lives in the scratch dir, so it can be reclaimed by its
        // guard as soon as the job returns — the retained copy is in the results store.
        Ok(meta)
    }
}

/// A file we accepted for processing, plus the scratch dir that owns it.
#[derive(Debug)]
struct Prepared {
    work: WorkDir,
    input: Option<PathBuf>,
    inline: Option<Vec<u8>>,
    size: u64,
}

impl Prepared {
    /// First bytes of the file, for magic sniffing only.
    async fn header(&self) -> AppResult<Vec<u8>> {
        if let Some(bytes) = &self.inline {
            let n = bytes.len().min(SNIFF_BYTES);
            return Ok(bytes[..n].to_vec());
        }
        read_header(self.input_file()?, SNIFF_BYTES).await
    }

    /// Path of the file on disk, when the input is a file rather than bytes.
    fn input_file(&self) -> AppResult<&Path> {
        self.input
            .as_deref()
            .ok_or_else(|| AppError::Internal("no input file for this job".to_string()))
    }

    /// Image dimensions from the file header, without decoding pixels.
    fn dimensions(&self, kind: &FileKind) -> AppResult<(u32, u32)> {
        let fmt = match kind {
            FileKind::Image(f) => *f,
            _ => {
                return Err(AppError::Internal(
                    "dimensions requested for a non-image".to_string(),
                ))
            }
        };
        let (w, h) = match &self.inline {
            Some(bytes) => ::image::ImageReader::with_format(std::io::Cursor::new(bytes), fmt)
                .into_dimensions()?,
            None => {
                // BufReader<File>, not Cursor<File>: a file is seekable, while
                // Cursor<File> satisfies neither BufRead nor Seek.
                let path = self.input_file()?;
                let file = std::io::BufReader::new(std::fs::File::open(path)?);
                let reader = ::image::ImageReader::with_format(file, fmt);
                reader.into_dimensions()?
            }
        };
        Ok((w, h))
    }

    /// The whole file. When the caller handed us bytes they are *moved* out — no copy
    /// of a potentially large payload — so this can only be called once.
    async fn take_bytes(&mut self) -> AppResult<Vec<u8>> {
        if let Some(bytes) = self.inline.take() {
            return Ok(bytes);
        }
        Ok(tokio::fs::read(self.input_file()?).await?)
    }

    /// Ensure the payload exists as a file inside the scratch dir (external tools need
    /// a path) and return it.
    async fn materialise(&self, name: &str) -> AppResult<PathBuf> {
        if let Some(path) = &self.input {
            return Ok(path.clone());
        }
        let data = self
            .inline
            .as_ref()
            .ok_or_else(|| AppError::Internal("no input to write".to_string()))?;
        let dest = self.work.join(name);
        let mut file = tokio::fs::File::create(&dest).await?;
        file.write_all(data).await?;
        file.flush().await?;
        Ok(dest)
    }
}

fn tool_missing(tool: &str, what: &str) -> AppError {
    AppError::ExternalToolMissing(format!(
        "{tool} is required for {what} but is not installed on this server"
    ))
}

fn join_err(e: tokio::task::JoinError) -> AppError {
    if e.is_panic() {
        tracing::error!("a blocking transform panicked: {e}");
        AppError::Internal("the transform panicked; the request was not completed".to_string())
    } else {
        AppError::Internal("the transform was cancelled".to_string())
    }
}
