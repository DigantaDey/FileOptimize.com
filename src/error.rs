//! Unified error type that converts into HTTP responses.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Every fallible operation in the service maps to one of these variants, which
/// renders as a small JSON body `{ "error": "..." }` with an appropriate status.
#[derive(Debug)]
pub enum AppError {
    /// 400 — the request is malformed or semantically invalid.
    BadRequest(String),
    /// 404 — the route/resource does not exist.
    NotFound(String),
    /// 413 — the upload exceeds the configured size cap.
    PayloadTooLarge(String),
    /// 415 — the file type is not supported for the requested action.
    UnsupportedMediaType(String),
    /// 503 — an external tool (ghostscript / poppler) required for the operation is missing.
    ExternalToolMissing(String),
    /// 500 — an unexpected internal failure.
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            AppError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            AppError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            AppError::PayloadTooLarge(m) => (StatusCode::PAYLOAD_TOO_LARGE, m),
            AppError::UnsupportedMediaType(m) => (StatusCode::UNSUPPORTED_MEDIA_TYPE, m),
            AppError::ExternalToolMissing(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            AppError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        tracing::warn!(status = %status, error = %message, "request error");
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Convenience alias used across the crate.
pub type AppResult<T> = Result<T, AppError>;

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        AppError::Internal(format!("io error: {e}"))
    }
}

impl From<image::ImageError> for AppError {
    fn from(e: image::ImageError) -> Self {
        AppError::Internal(format!("image error: {e}"))
    }
}

impl From<axum::extract::multipart::MultipartError> for AppError {
    fn from(e: axum::extract::multipart::MultipartError) -> Self {
        AppError::BadRequest(format!("multipart error: {e}"))
    }
}

impl From<zip::result::ZipError> for AppError {
    fn from(e: zip::result::ZipError) -> Self {
        AppError::Internal(format!("zip error: {e}"))
    }
}
