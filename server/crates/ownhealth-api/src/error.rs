//! Errors as FastAPI answered them: `{"detail": {"code": ..., "message": ..., ...}}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use ownhealth_core::Error;
use serde_json::{Map, Value, json};

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    /// Merged into `detail` after code and message (e.g. `allowed`).
    pub extra: Box<Map<String, Value>>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code: code.into(),
            message: message.into(),
            extra: Box::default(),
        }
    }

    pub fn with(mut self, key: &str, value: Value) -> ApiError {
        self.extra.insert(key.into(), value);
        self
    }

    pub fn unauthenticated() -> ApiError {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthenticated", "Log in first.")
    }

    pub fn not_found() -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "Not found.")
    }
}

impl From<Error> for ApiError {
    fn from(e: Error) -> ApiError {
        match e {
            Error::AccessDenied { code, message, allowed } => {
                let status = if code == "profile_not_granted" {
                    StatusCode::NOT_FOUND
                } else {
                    StatusCode::FORBIDDEN
                };
                ApiError::new(status, &code, message).with("allowed", json!(allowed))
            }
            Error::Review(m) | Error::Lifecycle(m) | Error::Provenance(m) => {
                ApiError::new(StatusCode::CONFLICT, "conflict", m)
            }
            Error::Upload(m) => ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_upload", m),
            Error::Invalid(m) => ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid", m),
            Error::NotFound(m) => ApiError::new(StatusCode::NOT_FOUND, "unknown", m),
            other => {
                tracing::error!(error = %other, "request failed");
                ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Internal error.")
            }
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> ApiError {
        Error::Db(e).into()
    }
}

impl From<ownhealth_auth::csrf::CsrfFailed> for ApiError {
    fn from(e: ownhealth_auth::csrf::CsrfFailed) -> ApiError {
        ApiError::new(StatusCode::FORBIDDEN, "csrf_failed", e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut detail = Map::new();
        detail.insert("code".into(), json!(self.code));
        detail.insert("message".into(), json!(self.message));
        detail.extend(*self.extra);
        (self.status, Json(json!({ "detail": detail }))).into_response()
    }
}
