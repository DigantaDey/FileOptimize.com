//! FileOptimize — a fast, memory-efficient file compression & conversion backend.
//!
//! Architecture:
//! - [`routes`] — HTTP surface (`/`, `/api/process`, `/api/health`, `/api/info`).
//! - [`processing`] — the transformation pipeline (images, PDFs, archives).
//! - [`state`] / [`config`] — shared handles and tunable limits.
//! - [`cleanup`] — TTL sweeper for temporary artifacts.
//! - [`detect`] / [`sanitize`] — input validation primitives.
//!
//! [`create_app`] builds the configured [`axum::Router`]; `src/main.rs` binds it to a
//! listener with graceful shutdown.

pub mod cleanup;
pub mod config;
pub mod detect;
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
