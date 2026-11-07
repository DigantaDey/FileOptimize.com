//! FileOptimize — a fast, memory-efficient file compression & conversion engine, with
//! an HTTP service on top of it.
//!
//! # Two layers, one set of rules
//!
//! - [`engine`] — **the product.** [`engine::Engine`] turns bytes into bytes: validation,
//!   limits, concurrency and memory accounting, scratch-space lifecycle, retained
//!   results. It knows nothing about HTTP. Embed it in another Rust service (a queue
//!   worker, a CLI, a batch importer) and you get exactly the guarantees the public API
//!   advertises, because the API is a thin adapter in front of it.
//! - [`routes`] — the transport: multipart parsing, headers, status codes, API keys,
//!   `/api/v1/*` for machines and `/api/process` + `/upload` for the bundled UI and
//!   legacy clients.
//! - [`processing`] — the codecs and tool invocations the engine drives (images, PDFs,
//!   archives).
//! - [`state`] / [`config`] — shared handles and tunable limits.
//! - [`cleanup`] — TTL sweeper for temporary and retained artifacts.
//! - [`detect`] / [`sanitize`] — input validation primitives.
//!
//! # Embedding
//!
//! ```no_run
//! use fileoptimize::engine::{Action, Engine, Input, Job};
//!
//! # async fn demo() -> Result<(), fileoptimize::error::AppError> {
//! let engine = Engine::from_env();
//! let job = Job::new(Action::ImageToPdf, "scan.png", Input::Bytes(vec![]));
//! let outcome = engine.run(job).await?;
//! let pdf = outcome.output.read_all()?;
//! assert!(pdf.starts_with(b"%PDF-"));
//! # Ok(())
//! # }
//! ```
//!
//! [`create_app`] builds the configured [`axum::Router`] from an [`state::AppState`];
//! `src/main.rs` binds it to a listener with graceful shutdown.

pub mod cleanup;
pub mod config;
pub mod detect;
pub mod engine;
pub mod error;
pub mod processing;
pub mod routes;
pub mod sanitize;
pub mod state;

use axum::Router;
use tower_http::compression::predicate::{NotForContentType, Predicate, SizeAbove};
use tower_http::trace::TraceLayer;

/// Build the fully-layered application router, ready to be served.
///
/// Middleware order (outermost first): request tracing → CORS → selective response
/// compression. Compression deliberately skips images *and* the already-deflated
/// PDF/ZIP payloads, so no CPU is burned re-compressing incompressible bytes.
pub fn create_app(state: state::AppState) -> Router {
    let should_compress = SizeAbove::new(512)
        .and(NotForContentType::IMAGES)
        .and(NotForContentType::const_new("application/pdf"))
        .and(NotForContentType::const_new("application/zip"));

    Router::new()
        .merge(routes::router(state))
        .layer(tower_http::compression::CompressionLayer::new().compress_when(should_compress))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}
