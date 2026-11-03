use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("{0}")]
    Unprocessable(String),
    #[error("payload too large: {0}")]
    PayloadTooLarge(String),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<aaf_core::Error> for ApiError {
    fn from(e: aaf_core::Error) -> Self {
        match e {
            aaf_core::Error::Denied(m) => ApiError::Forbidden(m),
            other => ApiError::Internal(other.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, kind) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::Unprocessable(_) => (StatusCode::UNPROCESSABLE_ENTITY, "blocked"),
            ApiError::PayloadTooLarge(_) => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            ApiError::Internal(m) => {
                tracing::error!(error = %m, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };
        // 內部錯誤細節不回給用戶端
        let message = if matches!(self, ApiError::Internal(_)) {
            "internal error".to_string()
        } else {
            self.to_string()
        };
        (
            code,
            Json(serde_json::json!({"error": kind, "message": message})),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
