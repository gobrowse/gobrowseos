use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use gobrowse_core::ApiError;
use thiserror::Error;
use tracing::error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("resource not found")]
    NotFound,
    #[error("request conflicts with current state: {0}")]
    Conflict(&'static str),
    #[error("invalid request: {0}")]
    Validation(String),
    #[error("too many requests")]
    RateLimited,
    #[error("database operation failed")]
    Database(#[from] sqlx::Error),
    #[error("internal operation failed")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let correlation_id = Uuid::now_v7();
        let (status, code, message) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", self.to_string()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden", self.to_string()),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", self.to_string()),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict", self.to_string()),
            Self::Validation(_) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation",
                self.to_string(),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                self.to_string(),
            ),
            Self::Database(_) | Self::Internal(_) => {
                error!(%correlation_id, error = %self, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "The operation failed. Use the correlation ID in server diagnostics.".into(),
                )
            }
        };
        (
            status,
            Json(ApiError {
                code: code.into(),
                message,
                correlation_id,
            }),
        )
            .into_response()
    }
}
