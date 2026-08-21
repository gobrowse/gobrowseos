pub mod api;
pub mod auth;
pub mod autobiography_api;
pub mod capabilities_api;
pub mod chat;
pub mod config;
pub mod conversation_api;
pub mod csp;
pub mod db;
pub mod doctor;
pub mod embedding;
pub mod embedding_api;
pub mod error;
pub mod library_api;
pub mod mcp_api;
pub mod mcp_client;
pub mod model_api;
pub mod outbound_http;
pub mod plugin_api;
pub mod plugin_github;
pub mod realtime;
pub mod router;
pub mod run_api;
pub mod run_tools;
pub mod sandbox_api;
pub mod sandbox_client;
pub mod skills_api;
pub mod task_api;
pub mod ui_api;
pub mod usage_api;
pub mod vault;
pub mod vault_api;
pub mod webhook_scheduler;
pub mod webhooks;
pub mod worktree_api;

use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::{
    auth::PasswordRuntime,
    config::Settings,
    error::AppError,
    plugin_github::{GitHubMarketplace, GitHubReleaseSource},
    sandbox_client::{SandboxClient, SandboxClientError, SandboxConfig},
    vault::Vault,
};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
};
use sqlx::PgPool;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tower_http::{
    catch_panic::CatchPanicLayer,
    compression::CompressionLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    sensitive_headers::SetSensitiveRequestHeadersLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

/// Lazily-connected sandbox client.
///
/// The [`SandboxConfig`] is captured at startup, but no validation or
/// connection happens then, so a malformed or unreachable sandbox never fails
/// server startup. The validated [`SandboxClient`] is built on first use (via
/// [`SandboxHandle::get_or_connect`]) and cached on success; init failures are
/// reported gracefully and retried on the next call. The actual daemon socket
/// is opened lazily, per operation, by [`SandboxClient::send`].
#[derive(Clone)]
pub struct SandboxHandle {
    pub config: SandboxConfig,
    client: Arc<std::sync::OnceLock<SandboxClient>>,
}

impl SandboxHandle {
    /// Returns a connected client, building and caching it on first use.
    /// Propagates config validation errors without caching them.
    pub fn get_or_connect(&self) -> Result<SandboxClient, SandboxClientError> {
        if let Some(client) = self.client.get() {
            return Ok(client.clone());
        }
        let client = SandboxClient::new(self.config.clone())?;
        // Another caller may have initialized concurrently; ignore the
        // (unlikely) duplicate-set error and return our freshly built client.
        let _ = self.client.set(client.clone());
        Ok(client)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum UiKind {
    Theme,
    FullUi,
}

impl std::str::FromStr for UiKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "THEME" => Ok(Self::Theme),
            "FULL_UI" => Ok(Self::FullUi),
            other => Err(format!("unknown ui_kind {other}")),
        }
    }
}

impl std::fmt::Display for UiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Theme => write!(f, "THEME"),
            Self::FullUi => write!(f, "FULL_UI"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct UiAsset {
    pub file_path: String,
    pub content_type: String,
    pub sha256_hash: String,
}

#[derive(Clone, Debug)]
pub struct ActiveUiState {
    pub package_id: uuid::Uuid,
    pub ui_kind: UiKind,
    pub install_path: std::path::PathBuf,
    pub entry_point: String,
    pub assets: Vec<UiAsset>,
    pub theme_variables: Option<std::collections::HashMap<String, String>>,
}

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub settings: Arc<Settings>,
    pub passwords: PasswordRuntime,
    pub vault: Vault,
    pub run_cancellations: Arc<RwLock<HashMap<uuid::Uuid, (uuid::Uuid, CancellationToken)>>>,
    /// Configured sandbox handle. `None` when sandboxing is disabled or the
    /// daemon socket/token are not configured. The client connects lazily on
    /// first use, so a down or misconfigured daemon never fails startup.
    pub sandbox: Option<SandboxHandle>,
    /// GitHub release plugin source (honors test base-URL overrides).
    pub plugin_source: GitHubReleaseSource,
    /// GitHub marketplace adapter for `POST /plugins/search`.
    pub plugin_marketplace: GitHubMarketplace,
    /// Per-server MCP client pool (Lane E). Connections are created lazily on
    /// first `library_load` for an MCP book.
    pub mcp_clients: mcp_client::McpClientPool,
    /// Cached tool descriptors for the run loop. Computed at startup to avoid
    /// rebuilding per request.
    pub tool_descriptors: Arc<Vec<gobrowse_core::model::ToolDefinition>>,
    /// Active UI package state (M24b). Populated from DB on startup and updated on activation/rollback.
    pub active_ui: Arc<RwLock<Option<ActiveUiState>>>,
    /// Hashes of the built-in UI's inline scripts/styles, computed at
    /// startup so the strict CSP (no `unsafe-inline`) still allows the
    /// Trunk bootstrap and recovery page.
    pub builtin_csp_hashes: Arc<crate::csp::BuiltinCspHashes>,
}

impl AppState {
    pub async fn new(pool: PgPool, settings: Settings) -> Result<Self, AppError> {
        let passwords = PasswordRuntime::new(settings.auth.clone()).await?;
        let vault = Vault::from_settings(&settings.vault).await?;
        let sandbox = if settings.features.sandbox {
            match (
                &settings.features.sandbox_socket_path,
                &settings.features.sandbox_auth_token,
            ) {
                (Some(socket_path), Some(auth_token)) => Some(SandboxHandle {
                    config: SandboxConfig {
                        socket_path: socket_path.clone(),
                        auth_token: auth_token.clone(),
                        timeout: Duration::from_secs(
                            settings.features.sandbox_socket_timeout_seconds,
                        ),
                    },
                    client: Arc::new(std::sync::OnceLock::new()),
                }),
                _ => None,
            }
        } else {
            None
        };
        let plugin_source = GitHubReleaseSource::from_settings(&settings.features);
        let plugin_marketplace = GitHubMarketplace::from_settings(&settings.features);
        let tool_descriptors = Arc::new(crate::run_tools::tool_definitions(sandbox.is_some()));
        // Load active UI package from DB (M24b). Best-effort: missing table before migration 0024 yields None.
        let active_ui_state = load_active_ui_state(&pool).await.unwrap_or(None);
        let static_dir = settings.http.static_dir.clone();
        let builtin_index = tokio::fs::read_to_string(static_dir.join("index.html"))
            .await
            .unwrap_or_default();
        let mut script_hashes = crate::csp::extract_inline_hashes(&builtin_index, "script");
        let mut style_hashes = crate::csp::extract_inline_hashes(RECOVERY_HTML, "style");
        // The Trunk-built recovery app (if present) also carries an inline
        // bootstrap script and style block.
        for candidate in [
            std::path::PathBuf::from("/app/recovery/index.html"),
            std::path::PathBuf::from("./dist-recovery/index.html"),
            std::path::PathBuf::from("./crates/gobrowse-recovery/dist/index.html"),
            static_dir.join("recovery/index.html"),
        ] {
            if let Ok(html) = tokio::fs::read_to_string(&candidate).await {
                script_hashes.extend(crate::csp::extract_inline_hashes(&html, "script"));
                style_hashes.extend(crate::csp::extract_inline_hashes(&html, "style"));
            }
        }
        let builtin_csp_hashes = crate::csp::BuiltinCspHashes {
            script_hashes,
            style_hashes,
        };
        Ok(Self {
            pool,
            settings: Arc::new(settings),
            passwords,
            vault,
            run_cancellations: Arc::new(RwLock::new(HashMap::new())),
            sandbox,
            plugin_source,
            plugin_marketplace,
            mcp_clients: mcp_client::McpClientPool::new(),
            tool_descriptors,
            active_ui: Arc::new(RwLock::new(active_ui_state)),
            builtin_csp_hashes: Arc::new(builtin_csp_hashes),
        })
    }

    /// Returns the plugin source implementation for a `source_type` string,
    /// rejecting unsupported types with an actionable error.
    pub fn plugin_source_for(&self, source_type: &str) -> Result<&GitHubReleaseSource, AppError> {
        match source_type {
            "github_release" => Ok(&self.plugin_source),
            other => Err(AppError::Validation(format!(
                "unsupported plugin source type `{other}`; supported: github_release"
            ))),
        }
    }
}

async fn load_active_ui_state(pool: &PgPool) -> Result<Option<ActiveUiState>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, ui_kind, install_path, entry_point, manifest \
         FROM ui_packages WHERE state = 'active' LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    use sqlx::Row;
    let id: uuid::Uuid = row.get("id");
    let ui_kind_str: String = row.get("ui_kind");
    let ui_kind = ui_kind_str.parse().unwrap_or(UiKind::Theme);
    let install_path: Option<String> = row.get("install_path");
    let entry_point: Option<String> = row.get("entry_point");
    let manifest: serde_json::Value = row.get("manifest");
    let assets = sqlx::query(
        "SELECT file_path, content_type, sha256_hash FROM ui_package_assets WHERE ui_package_id = $1",
    )
    .bind(id)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|r| UiAsset {
        file_path: r.get("file_path"),
        content_type: r.get("content_type"),
        sha256_hash: r.get("sha256_hash"),
    })
    .collect::<Vec<_>>();
    let theme_variables = if ui_kind == UiKind::Theme {
        manifest
            .get("theme")
            .and_then(|t| t.get("variables"))
            .and_then(|v| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
            })
    } else {
        None
    };
    Ok(Some(ActiveUiState {
        package_id: id,
        ui_kind,
        install_path: install_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from("./data/ui-packages")),
        entry_point: entry_point.unwrap_or_else(|| "index.html".to_string()),
        assets,
        theme_variables,
    }))
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/setup", get(auth::setup_status))
        .route("/setup/owner", post(auth::create_owner))
        .route("/auth/login", post(auth::login))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/me", get(auth::me))
        .route("/auth/rotate", post(auth::rotate_sessions))
        .route("/version", get(api::version))
        .route("/capabilities", get(capabilities_api::get_capabilities))
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
        .route("/models/auto-detect", get(model_api::auto_detect_providers))
        .route(
            "/models/chat",
            get(model_api::list_chat_models).post(model_api::create_chat_model),
        )
        .route(
            "/models/chat/{id}/activate",
            post(model_api::activate_chat_model),
        )
        .route(
            "/models/task-routes",
            get(model_api::list_task_routes).put(model_api::upsert_task_route),
        )
        .route(
            "/models/task-routes/{task_class}",
            delete(model_api::delete_task_route),
        )
        .route("/providers/catalog", get(usage_api::list_provider_catalog))
        .route("/usage/summary", get(usage_api::usage_summary))
        .route(
            "/conversations/{id}/runs",
            get(run_api::get_active_run).post(run_api::start_run),
        )
        .route("/conversations/{id}/turns", post(run_api::start_turn))
        .route(
            "/conversations/{id}/pins",
            get(conversation_api::list_pinned_books),
        )
        .route(
            "/conversations/{id}/pins/{book_id}",
            put(conversation_api::pin_book).delete(conversation_api::unpin_book),
        )
        .route("/runs/{id}", get(run_api::get_run))
        .route("/runs/{id}/context", get(run_api::get_run_context))
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
        // --- Lane C: plugin install server flow -----------------------------
        // Preview never installs; install requires approve + matching digest.
        // Upgrade stages with a permission diff; activate/rollback serialize
        // per plugin via SELECT ... FOR UPDATE on the plugins row.
        .route("/plugins/preview", post(plugin_api::preview))
        .route("/plugins/install", post(plugin_api::install))
        .route("/plugins/search", post(plugin_api::search))
        .route("/plugins", get(plugin_api::list))
        .route(
            "/plugins/{id}",
            get(plugin_api::get)
                .patch(plugin_api::patch)
                .delete(plugin_api::delete_plugin),
        )
        .route("/plugins/{id}/upgrade", post(plugin_api::upgrade))
        .route(
            "/plugins/{id}/upgrade/{version}/activate",
            post(plugin_api::activate),
        )
        .route("/plugins/{id}/rollback", post(plugin_api::rollback))
        // --- M24b: UI packages ------------------------------------------------
        .route("/ui/preview", post(ui_api::preview))
        .route("/ui/install", post(ui_api::install))
        .route("/ui/packages", get(ui_api::list))
        .route(
            "/ui/packages/{id}",
            get(ui_api::get)
                .patch(ui_api::patch)
                .delete(ui_api::delete_package),
        )
        .route("/ui/packages/{id}/activate", post(ui_api::activate))
        .route("/ui/packages/{id}/approve", post(ui_api::approve))
        .route("/ui/packages/{id}/rollback", post(ui_api::rollback_by_id))
        .route("/ui/rollback", post(ui_api::rollback))
        .route("/ui/packages/{id}/theme.css", get(ui_api::theme_css))
        .route("/ui/active-theme.css", get(ui_api::active_theme_css))
        // --- End M24b --------------------------------------------------------
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
        .route("/mcp/servers", get(mcp_api::list).post(mcp_api::create))
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
        // ── Lane E: progressive loading + sandbox API ───────────────────
        // Library progressive loading (full body / skill revision / MCP
        // tools / plugin components; `?component=` resolves one schema).
        .route("/library/books/{id}/load", post(library_api::load_book))
        // Sandbox backend for the browser terminal + file manager.
        .route("/sandbox/exec", post(sandbox_api::exec))
        .route("/sandbox/files/read", post(sandbox_api::read_file))
        .route("/sandbox/files/write", post(sandbox_api::write_file))
        .route("/sandbox/files/list", post(sandbox_api::list_files))
        .route("/sandbox/files/stat", post(sandbox_api::stat_file))
        .route("/sandbox/files/mkdir", post(sandbox_api::mkdir))
        .route("/sandbox/files/remove", post(sandbox_api::remove))
        .route("/sandbox/terminal/start", post(sandbox_api::terminal_start))
        .route(
            "/sandbox/terminal/{id}/write",
            post(sandbox_api::terminal_write),
        )
        .route(
            "/sandbox/terminal/{id}/read",
            post(sandbox_api::terminal_read),
        )
        .route(
            "/sandbox/terminal/{id}/resize",
            post(sandbox_api::terminal_resize),
        )
        .route(
            "/sandbox/terminal/{id}/interrupt",
            post(sandbox_api::terminal_interrupt),
        )
        .route(
            "/sandbox/terminal/{id}/close",
            post(sandbox_api::terminal_close),
        )
        .route("/sandbox/processes", post(sandbox_api::processes))
        .route(
            "/sandbox/processes/{pid}/kill",
            post(sandbox_api::kill_process),
        )
        // ── end Lane E ───────────────────────────────────────────────────
        .route("/runs/{id}/realtime", get(realtime::upgrade));

    Router::new()
        .route("/health/live", get(api::live))
        .route("/health/ready", get(api::ready))
        .route("/recovery", get(recovery_handler))
        .route("/recovery/{*path}", get(recovery_asset_handler))
        .nest("/api/v1", api)
        .fallback(fallback_handler)
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
        .layer(middleware::from_fn_with_state(
            state.clone(),
            csp::csp_middleware_async,
        ))
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

/// Built-in recovery page (no JS required): links to the API and the
/// recovery asset bundle. Kept as a constant so its inline style hash can be
/// computed once at startup and allowed by the strict CSP.
const RECOVERY_HTML: &str = r#"<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Gobrowse OS — Recovery</title><style>:root{--bg:#0f1419;--fg:#e6e8eb;--accent:#7ea0ff;--muted:#8a9099;--card:#1a232e;--border:#2a3441}*{box-sizing:border-box}body{margin:0;font-family:ui-sans-serif,system-ui,-apple-system,Arial;background:var(--bg);color:var(--fg);display:grid;place-items:center;min-height:100vh;padding:24px}a{color:var(--accent)}.card{background:var(--card);border:1px solid var(--border);border-radius:16px;padding:28px;max-width:560px;width:100%;box-shadow:0 10px 30px rgba(0,0,0,.35)}h1{margin:0 0 8px;font-size:22px;letter-spacing:-.02em}p{margin:8px 0;color:var(--muted);line-height:1.5}code{background:#0f1419;border:1px solid var(--border);padding:2px 6px;border-radius:6px}.btn{display:inline-block;margin-top:16px;background:var(--accent);color:#0f1419;padding:10px 14px;border-radius:10px;text-decoration:none;font-weight:600}</style></head><body><div class="card"><h1>Recovery</h1><p>Built-in recovery UI is always available. Use it to manage UI packages, rollback, or restore the built-in interface.</p><p>Endpoints: <code>GET /api/v1/capabilities</code> · <code>GET /api/v1/ui/packages</code></p><a class="btn" href="/api/v1/capabilities">View capabilities</a><p style="margin-top:16px"><a href="/">Back to app</a> · <a href="/recovery">Recovery</a></p><noscript><p>This page works without JavaScript. Use the API directly if needed.</p></noscript></div></body></html>"#;

async fn recovery_handler() -> impl IntoResponse {
    axum::response::Html(RECOVERY_HTML)
}

async fn recovery_asset_handler(
    State(state): State<AppState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    // Try to serve from recovery dir on disk if present; otherwise return recovery HTML.
    let recovery_dirs: Vec<std::path::PathBuf> = vec![
        std::path::PathBuf::from("/app/recovery"),
        std::path::PathBuf::from("./dist-recovery"),
        std::path::PathBuf::from("./crates/gobrowse-recovery/dist"),
        state.settings.http.static_dir.join("recovery"),
    ];
    for base in &recovery_dirs {
        let candidate = base.join(&path);
        if candidate.is_file()
            && let Ok(bytes) = tokio::fs::read(&candidate).await
        {
            let ct = if path.ends_with(".wasm") {
                "application/wasm"
            } else if path.ends_with(".js") {
                "application/javascript"
            } else if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".html") {
                "text/html"
            } else {
                "application/octet-stream"
            };
            let mut resp = bytes.into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(ct));
            return resp;
        }
    }
    recovery_handler().await.into_response()
}

async fn fallback_handler(State(state): State<AppState>, request: Request<Body>) -> Response {
    let path = request.uri().path().to_owned();
    // Bypass API and health (already matched) — serve SPA.
    // If active UI is FULL_UI, try to serve from its install_path first.
    let active = state.active_ui.read().await.clone();
    if let Some(active) = active
        && active.ui_kind == UiKind::FullUi
    {
        let rel = path.trim_start_matches('/');
        let candidate = if rel.is_empty() || rel == "/" {
            active.install_path.join(&active.entry_point)
        } else {
            active.install_path.join(rel)
        };
        if candidate.is_file()
            && let Ok(bytes) = tokio::fs::read(&candidate).await
        {
            let ct = if candidate.extension().and_then(|e| e.to_str()) == Some("wasm") {
                "application/wasm"
            } else if candidate.extension().and_then(|e| e.to_str()) == Some("js") {
                "application/javascript"
            } else if candidate.extension().and_then(|e| e.to_str()) == Some("css") {
                "text/css"
            } else {
                "text/html"
            };
            let mut resp = bytes.into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(ct));
            return resp;
        }
        // If entry point missing, redirect to recovery
        let entry_exists = active.install_path.join(&active.entry_point).is_file();
        if !entry_exists {
            return axum::response::Redirect::temporary("/recovery").into_response();
        }
        // Fall back to entry point for SPA routing
        if let Ok(bytes) = tokio::fs::read(active.install_path.join(&active.entry_point)).await {
            let mut resp = bytes.into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/html"),
            );
            return resp;
        }
    }
    // Default: serve from built-in static_dir
    let static_dir = state.settings.http.static_dir.clone();
    let rel = path.trim_start_matches('/');
    if !rel.is_empty() && !rel.contains("..") {
        let candidate = static_dir.join(rel);
        if candidate.is_file()
            && let Ok(bytes) = tokio::fs::read(&candidate).await
        {
            let ct = if rel.ends_with(".wasm") {
                "application/wasm"
            } else if rel.ends_with(".js") {
                "application/javascript"
            } else if rel.ends_with(".css") {
                "text/css"
            } else if rel.ends_with(".html") {
                "text/html"
            } else {
                "application/octet-stream"
            };
            let mut resp = bytes.into_response();
            resp.headers_mut()
                .insert(header::CONTENT_TYPE, header::HeaderValue::from_static(ct));
            return resp;
        }
    }
    // SPA fallback to index.html
    let index = static_dir.join("index.html");
    if let Ok(bytes) = tokio::fs::read(&index).await {
        let mut resp = bytes.into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/html"),
        );
        return resp;
    }
    // If static index missing (e.g., in tests), return minimal HTML
    let html = r#"<!doctype html><html><head><meta charset="utf-8"><title>Gobrowse OS</title></head><body><div id="app">Gobrowse OS</div><script type="module">import init from "/pkg/gobrowse_web.js"; init();</script></body></html>"#;
    axum::response::Html(html).into_response()
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
