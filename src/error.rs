//! Unified error type that converts into HTTP responses.

use axum::{
    http::{header, HeaderValue, StatusCode},
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
    /// 401 — the API is key-protected and the request carried no usable key.
    Unauthorized(String),
    /// 500 — an unexpected internal failure.
    Internal(String),
}

impl AppError {
    /// HTTP status this error renders as.
    pub fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            AppError::ExternalToolMissing(_) => StatusCode::SERVICE_UNAVAILABLE,
            AppError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Stable, machine-readable identifier. Clients should branch on this rather than
    /// on the human-readable message or even the status code.
    pub fn code(&self) -> &'static str {
        match self {
            AppError::BadRequest(_) => "bad_request",
            AppError::NotFound(_) => "not_found",
            AppError::PayloadTooLarge(_) => "payload_too_large",
            AppError::UnsupportedMediaType(_) => "unsupported_media_type",
            AppError::ExternalToolMissing(_) => "external_tool_missing",
            AppError::Unauthorized(_) => "unauthorized",
            AppError::Internal(_) => "internal_error",
        }
    }

    /// The human-readable explanation.
    pub fn message(&self) -> &str {
        match self {
            AppError::BadRequest(m)
            | AppError::NotFound(m)
            | AppError::PayloadTooLarge(m)
            | AppError::UnsupportedMediaType(m)
            | AppError::ExternalToolMissing(m)
            | AppError::Unauthorized(m)
            | AppError::Internal(m) => m,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let message = self.message().to_string();
        let code = self.code();
        // 5xx are worth an operator-level log; a client mistake is not.
        if status.is_server_error() {
            tracing::error!(status = %status, code, error = %message, "request failed");
        } else {
        let mut response =
            (status, Json(json!({ "error": message, "code": code }))).into_response();
        if status == StatusCode::UNAUTHORIZED {
            if let Ok(value) =
                HeaderValue::from_str("Bearer realm=\"fileoptimize\", error=\"invalid_key\"")
            {
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, value);
            }
        }
        response
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
