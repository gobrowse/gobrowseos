pub mod api;
pub mod auth;
pub mod config;
pub mod db;
pub mod doctor;
pub mod error;
pub mod library_api;
pub mod realtime;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
};
use sqlx::PgPool;
use tower_http::{
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    sensitive_headers::SetSensitiveRequestHeadersLayer,
    services::{ServeDir, ServeFile},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{auth::PasswordRuntime, config::Settings, error::AppError};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    pub passwords: PasswordRuntime,
}

impl AppState {
    pub async fn new(pool: PgPool, settings: Settings) -> Result<Self, AppError> {
        let passwords = PasswordRuntime::new(settings.auth.clone()).await?;
        Ok(Self {
            pool,
            settings: Arc::new(settings),
            passwords,
        })
    }
}

pub fn router(state: AppState) -> Router {
    let static_dir = state.settings.http.static_dir.clone();
    let index_file = static_dir.join("index.html");
    let api = Router::new()
        .route("/setup", get(auth::setup_status))
        .route("/setup/owner", post(auth::create_owner))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/version", get(api::version))
        .route(
            "/workspaces",
            get(api::list_workspaces).post(api::create_workspace),
        )
        .route(
            "/library/books",
            get(library_api::list_books).post(library_api::create_book),
        )
        .route("/library/search", get(library_api::search_books))
        .route(
            "/library/books/{id}",
            get(library_api::get_book).put(library_api::update_book),
        )
        .route(
            "/library/books/{id}/history",
            get(library_api::book_history),
        )
        .route("/realtime", get(realtime::upgrade));

    Router::new()
        .route("/health/live", get(api::live))
        .route("/health/ready", get(api::ready))
        .nest("/api/v1", api)
        .fallback_service(ServeDir::new(static_dir).fallback(ServeFile::new(index_file)))
        .layer(DefaultBodyLimit::max(
            state.settings.http.request_body_limit_bytes,
        ))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(CompressionLayer::new())
        .layer(SetSensitiveRequestHeadersLayer::new(std::iter::once(
            header::AUTHORIZATION,
        )))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::new(
            header::HeaderName::from_static("x-request-id"),
            MakeRequestUuid,
        ))
        .layer(TraceLayer::new_for_http())
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn_with_state(state.clone(), origin_guard))
        .with_state(state)
}

async fn origin_guard(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    if matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        if request
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "cross-site")
        {
            return Err(AppError::Forbidden);
        }
        if let Some(origin) = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            && origin.trim_end_matches('/')
                != state
                    .settings
                    .http
                    .public_origin
                    .as_str()
                    .trim_end_matches('/')
        {
            return Err(AppError::Forbidden);
        }
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_methods_are_enumerated() {
        assert!(matches!(
            Method::POST,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ));
        assert!(!matches!(
            Method::GET,
            Method::POST | Method::PUT | Method::PATCH | Method::DELETE
        ));
    }
}
