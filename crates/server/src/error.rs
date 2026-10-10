//! The uniform error body: `{"error": "<code>", "message": "..."}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use openporch_store::StoreError;
use serde::Serialize;

/// Stable error codes.
pub mod code {
    /// The object does not exist or is not visible.
    pub const NOT_FOUND: &str = "API-001";
    /// An identifier does not match the ID rule.
    pub const INVALID_ID: &str = "API-002";
    /// The request is malformed or invalid.
    pub const INVALID_REQUEST: &str = "API-003";
    /// The request conflicts with current state.
    pub const CONFLICT: &str = "API-004";
    /// The method is not allowed on this path.
    pub const METHOD_NOT_ALLOWED: &str = "API-005";
    /// The request body is too large.
    pub const TOO_LARGE: &str = "API-006";
    /// No valid credential.
    pub const UNAUTHENTICATED: &str = "AUTH-001";
    /// The principal lacks the permission.
    pub const FORBIDDEN: &str = "AUTH-002";
    /// An unexpected server error.
    pub const INTERNAL: &str = "SRV-001";
    /// The server is not ready to accept changes.
    pub const NOT_READY: &str = "SRV-002";
    /// An external identity provider could not be reached.
    pub const PROVIDER_UNAVAILABLE: &str = "SRV-003";
}

/// An API error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    /// HTTP status.
    pub status: StatusCode,
    /// Stable code.
    pub code: &'static str,
    /// Human-readable message. Never contains secret values or tokens.
    pub message: String,
}

#[derive(Serialize)]
struct Body<'a> {
    error: &'a str,
    message: &'a str,
}

impl ApiError {
    /// An error with an explicit status and code.
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    /// `404`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code::NOT_FOUND, message)
    }

    /// `400` for a malformed or invalid request.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code::INVALID_REQUEST, message)
    }

    /// `400` for an identifier that breaks the ID rule.
    pub fn invalid_id(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code::INVALID_ID, message)
    }

    /// `409`.
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, code::CONFLICT, message)
    }

    /// `401`.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            code::UNAUTHENTICATED,
            "a valid bearer token is required",
        )
    }

    /// `403`.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code::FORBIDDEN, message)
    }

    /// `503`.
    pub fn not_ready(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code::NOT_READY, message)
    }

    /// `500`. The detail is logged, not returned.
    pub fn internal(detail: impl std::fmt::Display) -> Self {
        tracing::error!(error = %detail, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            code::INTERNAL,
            "internal server error",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::to_vec(&Body {
            error: self.code,
            message: &self.message,
        })
        .unwrap_or_default();
        (
            self.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            body,
        )
            .into_response()
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NotFound(_) => Self::not_found(e.to_string()),
            StoreError::AlreadyExists(_)
            | StoreError::Conflict(_)
            | StoreError::StaleGeneration { .. }
            | StoreError::Busy(_)
            | StoreError::RecoveryRequired(_)
            | StoreError::IdempotencyMismatch(_) => Self::conflict(e.to_string()),
            StoreError::Forbidden(_) => Self::forbidden(e.to_string()),
            other => Self::internal(other),
        }
    }
}

/// The result type of handlers.
pub type ApiResult<T> = Result<T, ApiError>;
