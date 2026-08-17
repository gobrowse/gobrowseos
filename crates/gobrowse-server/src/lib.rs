pub mod api;
pub mod auth;
pub mod autobiography_api;
pub mod chat;
pub mod config;
pub mod conversation_api;
pub mod db;
pub mod doctor;
pub mod embedding;
pub mod embedding_api;
pub mod error;
pub mod library_api;
pub mod mcp_api;
pub mod model_api;
pub mod outbound_http;
pub mod realtime;
pub mod run_api;
pub mod skills_api;
pub mod task_api;
pub mod vault;
pub mod vault_api;
pub mod webhook_scheduler;
pub mod webhooks;
pub mod worktree_api;

use std::{collections::HashMap, sync::Arc, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::{get, patch, post},
};
use sqlx::PgPool;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tower_http::{
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    sensitive_headers::SetSensitiveRequestHeadersLayer,
    services::{ServeDir, ServeFile},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::{auth::PasswordRuntime, config::Settings, error::AppError, vault::Vault};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    pub passwords: PasswordRuntime,
    pub vault: Vault,
    pub run_cancellations: Arc<RwLock<HashMap<uuid::Uuid, (uuid::Uuid, CancellationToken)>>>,
}

impl AppState {
    pub async fn new(pool: PgPool, settings: Settings) -> Result<Self, AppError> {
        let passwords = PasswordRuntime::new(settings.auth.clone()).await?;
        let vault = Vault::from_settings(&settings.vault).await?;
        Ok(Self {
            pool,
            settings: Arc::new(settings),
            passwords,
            vault,
            run_cancellations: Arc::new(RwLock::new(HashMap::new())),
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
        .route("/auth/rotate", post(auth::rotate_sessions))
        .route("/version", get(api::version))
        .route(
            "/workspaces",
            get(api::list_workspaces).post(api::create_workspace),
        )
        .route(
            "/conversations",
            get(conversation_api::list_conversations).post(conversation_api::create_conversation),
        )
        .route(
            "/workspaces/{workspace_id}/worktrees",
            get(worktree_api::list_worktrees).post(worktree_api::create_worktree),
        )
        .route(
            "/workspaces/{workspace_id}/tasks",
            get(task_api::list_tasks).post(task_api::create_task),
        )
        .route(
            "/workspaces/{workspace_id}/activity",
            get(task_api::list_activity).post(task_api::create_activity),
        )
        .route(
            "/tasks/{id}",
            get(task_api::get_task).patch(task_api::update_task),
        )
        .route(
            "/worktrees/{id}",
            get(worktree_api::get_worktree)
                .patch(worktree_api::update_worktree)
                .delete(worktree_api::delete_worktree),
        )
        .route(
            "/conversations/search",
            get(conversation_api::search_conversations),
        )
        .route(
            "/conversations/{id}",
            get(conversation_api::get_conversation).delete(conversation_api::delete_conversation),
        )
        .route(
            "/conversations/{id}/messages",
            get(conversation_api::list_messages).post(conversation_api::append_message),
        )
        .route(
            "/conversations/{id}/fork",
            post(conversation_api::fork_conversation),
        )
        .route(
            "/library/books",
            get(library_api::list_books).post(library_api::create_book),
        )
        .route("/library/search", get(library_api::search_books))
        .route(
            "/embeddings/configurations",
            get(embedding_api::list_configurations).post(embedding_api::create_configuration),
        )
        .route(
            "/embeddings/configurations/{id}/activate",
            post(embedding_api::activate_configuration),
        )
        .route("/embeddings/jobs", get(embedding_api::list_jobs))
        .route(
            "/embeddings/jobs/{id}/retry",
            post(embedding_api::retry_job),
        )
        .route(
            "/models/auto-detect",
            get(model_api::auto_detect_providers),
        )
        .route(
            "/models/chat",
            get(model_api::list_chat_models).post(model_api::create_chat_model),
        )
        .route(
            "/models/chat/{id}/activate",
            post(model_api::activate_chat_model),
        )
        .route(
            "/conversations/{id}/runs",
            get(run_api::get_active_run).post(run_api::start_run),
        )
        .route("/conversations/{id}/turns", post(run_api::start_turn))
        .route("/runs/{id}", get(run_api::get_run))
        .route("/runs/{id}/events", get(run_api::list_run_events))
        .route("/runs/{id}/cancel", post(run_api::cancel_run))
        .route(
            "/vault/secrets",
            get(vault_api::list_secrets).post(vault_api::create_secret),
        )
        .route(
            "/vault/secrets/{id}",
            axum::routing::put(vault_api::replace_secret).delete(vault_api::delete_secret),
        )
        .route("/vault/rotate", post(vault_api::rotate_secrets))
        .route("/webhooks/{id}/deliver", post(webhooks::receive_webhook))
        .route(
            "/library/books/{id}",
            get(library_api::get_book).put(library_api::update_book),
        )
        .route(
            "/library/books/{id}/history",
            get(library_api::book_history),
        )
        .route(
            "/skills",
            get(skills_api::list_skills).post(skills_api::create_skill),
        )
        .route(
            "/skills/{skill_id}/revisions",
            get(skills_api::history).post(skills_api::create_revision),
        )
        .route("/skills/{skill_id}/history", get(skills_api::history))
        .route(
            "/skills/{skill_id}/propose",
            post(skills_api::propose_revision),
        )
        .route(
            "/skills/{skill_id}/revisions/{revision}/evaluate",
            post(skills_api::evaluate),
        )
        .route(
            "/skills/{skill_id}/evaluate",
            post(skills_api::evaluate_skill),
        )
        .route(
            "/skills/{skill_id}/revisions/{revision}/promote",
            post(skills_api::promote),
        )
        .route(
            "/skills/{skill_id}/promote",
            post(skills_api::promote_skill),
        )
        .route("/skills/{skill_id}/rollback", post(skills_api::rollback))
        .route(
            "/mcp/servers",
            get(mcp_api::list).post(mcp_api::create),
        )
        .route(
            "/mcp/servers/{id}",
            patch(mcp_api::update).delete(mcp_api::delete),
        )
        .route("/autobiography", get(autobiography_api::get_autobiography))
        .route(
            "/autobiography/policy",
            axum::routing::put(autobiography_api::update_policy),
        )
        .route(
            "/autobiography/manual",
            axum::routing::put(autobiography_api::manual_update),
        )
        .route(
            "/autobiography/proposals",
            get(autobiography_api::list_proposals).post(autobiography_api::create_proposal),
        )
        .route(
            "/autobiography/proposals/{id}/review",
            post(autobiography_api::review_proposal),
        )
        .route("/autobiography/rollback", post(autobiography_api::rollback))
        .route("/runs/{id}/realtime", get(realtime::upgrade));

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
        // Webhook deliveries use HMAC-signature auth, not cookies;
        // CSRF origin enforcement does not apply.
        if request.uri().path().starts_with("/api/v1/webhooks/") {
            return Ok(next.run(request).await);
        }
        // Sec-Fetch-Site: cross-site navigations and form submissions
        // from third-party origins must be rejected.
        if request
            .headers()
            .get("sec-fetch-site")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value == "cross-site")
        {
            return Err(AppError::Forbidden);
        }
        // Origin must be present and exactly match the configured
        // public_origin (scheme + host + port). A missing Origin on a
        // state-changing request is rejected; same-origin deployments
        // require this for CSRF protection.
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .ok_or(AppError::Forbidden)?;
        if origin.trim_end_matches('/')
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
