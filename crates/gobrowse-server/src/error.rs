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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn extract(response: Response) -> (StatusCode, ApiError) {
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let api_error: ApiError = serde_json::from_slice(&body).unwrap();
        (status, api_error)
    }

    #[tokio::test]
    async fn every_app_error_variant_maps_to_correct_status_and_code() {
        let test_cases: Vec<(AppError, StatusCode, &str)> = vec![
            (
                AppError::Unauthorized,
                StatusCode::UNAUTHORIZED,
                "unauthorized",
            ),
            (AppError::Forbidden, StatusCode::FORBIDDEN, "forbidden"),
            (AppError::NotFound, StatusCode::NOT_FOUND, "not_found"),
            (
                AppError::Conflict("state conflict"),
                StatusCode::CONFLICT,
                "conflict",
            ),
            (
                AppError::Validation("field x is required".into()),
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation",
            ),
            (
                AppError::RateLimited,
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
            ),
        ];

        for (error, expected_status, expected_code) in test_cases {
            let (status, api_error) = extract(error.into_response()).await;
            assert_eq!(status, expected_status, "wrong status for {expected_code}");
            assert_eq!(
                api_error.code, expected_code,
                "wrong code for {expected_code}"
            );
            assert!(
                !api_error.correlation_id.is_nil(),
                "missing correlation_id for {expected_code}"
            );
        }
    }

    #[tokio::test]
    async fn database_error_returns_500_with_masked_message() {
        let db_err = AppError::Database(sqlx::Error::Configuration(Box::new(
            std::io::Error::other("config error"),
        )));
        let (status, api_error) = extract(db_err.into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_error.code, "internal");
        assert_eq!(
            api_error.message,
            "The operation failed. Use the correlation ID in server diagnostics."
        );
        assert!(!api_error.correlation_id.is_nil());
    }

    #[tokio::test]
    async fn internal_error_returns_500_with_masked_message() {
        let internal_err = AppError::Internal(anyhow::anyhow!("secret details"));
        let (status, api_error) = extract(internal_err.into_response()).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(api_error.code, "internal");
        assert_eq!(
            api_error.message,
            "The operation failed. Use the correlation ID in server diagnostics."
        );
        assert!(!api_error.correlation_id.is_nil());
    }
}
