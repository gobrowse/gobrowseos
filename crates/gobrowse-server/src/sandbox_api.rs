//! Sandbox HTTP API: browser terminal + file-manager backend routes.
//!
//! All routes require an authenticated user, a workspace that belongs to the
//! user's profile, a role gate (write ops need EDITOR/OWNER/ADMIN, read ops
//! accept VIEWER+), `features.sandbox` enabled, and a configured sandbox
//! client. Workspace storage is provisioned (idempotently) on first use, and
//! every operation is audited as `sandbox.op`.
//!
//! All paths are workspace-relative and validated with
//! `gobrowse_core::sandbox::validate_workspace_path`; file content travels
//! base64-encoded per existing API conventions.

use axum::{
    Json,
    extract::{Path, State},
    http::HeaderMap,
};
use gobrowse_core::sandbox::{
    HARD_RESOURCE_LIMITS, TerminalStartRequest, validate_command, validate_workspace_path,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    error::AppError,
    sandbox_client::SandboxClient,
};

// ---------------------------------------------------------------------------
// Request/response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct WorkspacePathRequest {
    pub workspace_id: Uuid,
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecRequest {
    pub workspace_id: Uuid,
    pub command: Vec<String>,
    #[serde(default = "default_working_directory")]
    pub working_directory: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

fn default_working_directory() -> String {
    ".".into()
}

#[derive(Debug, Deserialize)]
pub struct WriteFileRequest {
    pub workspace_id: Uuid,
    pub path: String,
    pub data_base64: String,
}

#[derive(Debug, Deserialize)]
pub struct TerminalStartRequestDto {
    pub workspace_id: Uuid,
    pub command: Vec<String>,
    #[serde(default = "default_working_directory")]
    pub working_directory: String,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalWriteRequest {
    pub workspace_id: Uuid,
    pub data_base64: String,
}

#[derive(Debug, Deserialize)]
pub struct TerminalReadRequest {
    pub workspace_id: Uuid,
    #[serde(default)]
    pub after_cursor: u64,
    #[serde(default)]
    pub max_bytes: Option<u32>,
    #[serde(default)]
    pub wait_ms: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct TerminalResizeRequest {
    pub workspace_id: Uuid,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Deserialize)]
pub struct TerminalIdRequest {
    pub workspace_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ProcessesRequest {
    pub workspace_id: Uuid,
    pub terminal_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct KillRequest {
    pub workspace_id: Uuid,
    pub terminal_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct ExecResponse {
    pub terminal_id: Uuid,
    pub exit_code: Option<i32>,
    pub state: String,
    pub output_complete: bool,
    pub output: String,
}

#[derive(Debug, Serialize)]
pub struct ReadFileResponse {
    pub data_base64: String,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct WriteFileResponse {
    pub bytes: usize,
    pub sha256: String,
}

#[derive(Debug, Serialize)]
pub struct TerminalStartResponse {
    pub terminal_id: Uuid,
    pub network_policy: String,
}

#[derive(Debug, Serialize)]
pub struct TerminalWriteResponse {
    pub bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct TerminalReadResponse {
    pub data_base64: String,
    pub next_cursor: u64,
    pub state: String,
    pub output_complete: bool,
}

#[derive(Debug, Serialize)]
pub struct OkResponse {
    pub ok: bool,
}

// ---------------------------------------------------------------------------
// Route handlers
// ---------------------------------------------------------------------------

/// POST /sandbox/exec — run a command to completion (or timeout) and return
/// its combined output, exit code, and final terminal state.
pub async fn exec(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ExecRequest>,
) -> Result<Json<ExecResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    validate_command(&input.command)
        .map_err(|_| AppError::Validation("command must be 1-128 non-empty arguments".into()))?;
    validate_workspace_path(&input.working_directory).map_err(|_| {
        AppError::Validation("working_directory must be a workspace-relative path".into())
    })?;
    let timeout = input.timeout_seconds.unwrap_or(10).clamp(1, 10);
    let network_policy = workspace_network_policy(&state, input.workspace_id).await?;
    let outcome = run_sandbox_op(&state, &user, input.workspace_id, "sandbox.exec", async {
        let client = sandbox_client(&state)?;
        let terminal_id = Uuid::now_v7();
        client
            .terminal_start(
                terminal_id,
                TerminalStartRequest {
                    workspace_id: input.workspace_id,
                    command: input.command,
                    working_directory: input.working_directory,
                    cols: input.cols.unwrap_or(80),
                    rows: input.rows.unwrap_or(24),
                    network_policy,
                    limits: HARD_RESOURCE_LIMITS,
                },
            )
            .await
            .map_err(sandbox_error_to_app)?;
        drain_terminal(client, terminal_id, timeout).await
    })
    .await?;
    Ok(Json(outcome))
}

/// POST /sandbox/files/read
pub async fn read_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WorkspacePathRequest>,
) -> Result<Json<ReadFileResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, false).await?;
    let path = validated_path(&input.path)?;
    let (data, sha256) = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.read",
        async {
            sandbox_client(&state)?
                .fs_read(input.workspace_id, &path)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(ReadFileResponse {
        data_base64: base64_encode(&data),
        sha256,
    }))
}

/// POST /sandbox/files/write
pub async fn write_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WriteFileRequest>,
) -> Result<Json<WriteFileResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    let path = validated_path(&input.path)?;
    let data = base64_decode(&input.data_base64)?;
    let (bytes, sha256) = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.write",
        async {
            sandbox_client(&state)?
                .fs_write(input.workspace_id, &path, &data)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(WriteFileResponse { bytes, sha256 }))
}

/// POST /sandbox/files/list
pub async fn list_files(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WorkspacePathRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, false).await?;
    let path = validated_path(&input.path)?;
    let entries = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.list",
        async {
            sandbox_client(&state)?
                .fs_list(input.workspace_id, &path)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "entries": entries.iter().map(|entry| serde_json::json!({
            "name": entry.name,
            "kind": format!("{:?}", entry.kind),
            "size": entry.size,
        })).collect::<Vec<_>>()
    })))
}

/// POST /sandbox/files/stat
pub async fn stat_file(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WorkspacePathRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, false).await?;
    let path = validated_path(&input.path)?;
    let metadata = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.stat",
        async {
            sandbox_client(&state)?
                .fs_stat(input.workspace_id, &path)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "kind": format!("{:?}", metadata.kind),
        "size": metadata.size,
        "mode": metadata.mode,
        "modified_unix_seconds": metadata.modified_unix_seconds,
    })))
}

/// POST /sandbox/files/mkdir
pub async fn mkdir(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WorkspacePathRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    let path = validated_path(&input.path)?;
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.mkdir",
        async {
            sandbox_client(&state)?
                .fs_mkdir(input.workspace_id, &path)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

/// POST /sandbox/files/remove
pub async fn remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WorkspacePathRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    let path = validated_path(&input.path)?;
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.files.remove",
        async {
            sandbox_client(&state)?
                .fs_delete(input.workspace_id, &path)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

/// POST /sandbox/terminal/start
pub async fn terminal_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<TerminalStartRequestDto>,
) -> Result<Json<TerminalStartResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    validate_command(&input.command)
        .map_err(|_| AppError::Validation("command must be 1-128 non-empty arguments".into()))?;
    validate_workspace_path(&input.working_directory).map_err(|_| {
        AppError::Validation("working_directory must be a workspace-relative path".into())
    })?;
    let cols = input.cols.unwrap_or(80);
    let rows = input.rows.unwrap_or(24);
    if !(20..=1_000).contains(&cols) || !(5..=500).contains(&rows) {
        return Err(AppError::Validation(
            "terminal dimensions are outside the supported range".into(),
        ));
    }
    let network_policy = workspace_network_policy(&state, input.workspace_id).await?;
    let response = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.start",
        async {
            let client = sandbox_client(&state)?;
            let terminal_id = Uuid::now_v7();
            let started = client
                .terminal_start(
                    terminal_id,
                    TerminalStartRequest {
                        workspace_id: input.workspace_id,
                        command: input.command,
                        working_directory: input.working_directory,
                        cols,
                        rows,
                        network_policy,
                        limits: HARD_RESOURCE_LIMITS,
                    },
                )
                .await
                .map_err(sandbox_error_to_app)?;
            Ok::<_, AppError>(TerminalStartResponse {
                terminal_id: started.terminal_id,
                network_policy: format!("{:?}", started.network_policy),
            })
        },
    )
    .await?;
    Ok(Json(response))
}

/// POST /sandbox/terminal/{id}/write
pub async fn terminal_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(terminal_id): Path<Uuid>,
    Json(input): Json<TerminalWriteRequest>,
) -> Result<Json<TerminalWriteResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    let data = base64_decode(&input.data_base64)?;
    let response = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.write",
        async {
            let accepted = sandbox_client(&state)?
                .terminal_input(terminal_id, Uuid::now_v7(), &data)
                .await
                .map_err(sandbox_error_to_app)?;
            Ok::<_, AppError>(TerminalWriteResponse {
                bytes: accepted.bytes,
            })
        },
    )
    .await?;
    Ok(Json(response))
}

/// POST /sandbox/terminal/{id}/read
pub async fn terminal_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(terminal_id): Path<Uuid>,
    Json(input): Json<TerminalReadRequest>,
) -> Result<Json<TerminalReadResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, false).await?;
    let max_bytes = input.max_bytes.unwrap_or(32 * 1024).clamp(1, 256 * 1024);
    let wait_ms = input.wait_ms.unwrap_or(0).clamp(0, 30_000);
    let response = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.read",
        async {
            let output = sandbox_client(&state)?
                .terminal_read_output(terminal_id, input.after_cursor, max_bytes, wait_ms)
                .await
                .map_err(sandbox_error_to_app)?;
            Ok::<_, AppError>(TerminalReadResponse {
                data_base64: base64_encode(&output.data),
                next_cursor: output.next_cursor,
                state: format!("{:?}", output.state),
                output_complete: output.output_complete,
            })
        },
    )
    .await?;
    Ok(Json(response))
}

/// POST /sandbox/terminal/{id}/resize
pub async fn terminal_resize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(terminal_id): Path<Uuid>,
    Json(input): Json<TerminalResizeRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    if !(20..=1_000).contains(&input.cols) || !(5..=500).contains(&input.rows) {
        return Err(AppError::Validation(
            "terminal dimensions are outside the supported range".into(),
        ));
    }
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.resize",
        async {
            sandbox_client(&state)?
                .terminal_resize(terminal_id, input.cols, input.rows)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

/// POST /sandbox/terminal/{id}/interrupt
pub async fn terminal_interrupt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(terminal_id): Path<Uuid>,
    Json(input): Json<TerminalIdRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.interrupt",
        async {
            sandbox_client(&state)?
                .terminal_interrupt(terminal_id)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

/// POST /sandbox/terminal/{id}/close
pub async fn terminal_close(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(terminal_id): Path<Uuid>,
    Json(input): Json<TerminalIdRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.terminal.close",
        async {
            sandbox_client(&state)?
                .terminal_terminate(terminal_id)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

/// POST /sandbox/processes
pub async fn processes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<ProcessesRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, false).await?;
    let process_list = run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.processes.list",
        async {
            sandbox_client(&state)?
                .terminal_processes(input.terminal_id)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(serde_json::json!({
        "processes": process_list.iter().map(|process| serde_json::json!({
            "pid": process.pid,
            "command": process.command,
        })).collect::<Vec<_>>()
    })))
}

/// POST /sandbox/processes/{pid}/kill
pub async fn kill_process(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<u32>,
    Json(input): Json<KillRequest>,
) -> Result<Json<OkResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    authorize_sandbox_workspace(&state, &user, input.workspace_id, true).await?;
    run_sandbox_op(
        &state,
        &user,
        input.workspace_id,
        "sandbox.processes.kill",
        async {
            sandbox_client(&state)?
                .terminal_kill(input.terminal_id, pid)
                .await
                .map_err(sandbox_error_to_app)
        },
    )
    .await?;
    Ok(Json(OkResponse { ok: true }))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn sandbox_client(state: &AppState) -> Result<&SandboxClient, AppError> {
    state.sandbox.as_ref().ok_or_else(|| {
        AppError::ServiceUnavailable("sandbox unavailable — daemon not configured or not reachable")
    })
}

/// Workspace lookup + role gate: write ops need EDITOR/OWNER/ADMIN (or a
/// profile OWNER/ADMIN role); read ops accept any membership.
async fn authorize_sandbox_workspace(
    state: &AppState,
    user: &AuthenticatedUser,
    workspace_id: Uuid,
    require_write: bool,
) -> Result<(), AppError> {
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspaces workspace WHERE id=$1 AND profile_id=$2 AND ( \
         $4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships member \
         WHERE member.workspace_id=workspace.id AND member.user_id=$3 \
         AND (NOT $5::boolean OR member.access IN ('OWNER','EDITOR')))))",
    )
    .bind(workspace_id)
    .bind(user.profile_id)
    .bind(user.id)
    .bind(&user.role)
    .bind(require_write)
    .fetch_one(&state.pool)
    .await?;
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// Loads the workspace's configured network policy (server-side authoritative).
async fn workspace_network_policy(
    state: &AppState,
    workspace_id: Uuid,
) -> Result<gobrowse_core::sandbox::NetworkPolicy, AppError> {
    let policy: Option<String> =
        sqlx::query_scalar("SELECT network_policy FROM workspaces WHERE id=$1")
            .bind(workspace_id)
            .fetch_one(&state.pool)
            .await
            .map_err(AppError::Database)?;
    Ok(match policy.as_deref() {
        Some("NONE") => gobrowse_core::sandbox::NetworkPolicy::None,
        Some("FULL") => gobrowse_core::sandbox::NetworkPolicy::Full,
        _ => gobrowse_core::sandbox::NetworkPolicy::Restricted,
    })
}

fn validated_path(path: &str) -> Result<String, AppError> {
    validate_workspace_path(path)
        .map(|_| path.to_owned())
        .map_err(|_| {
            AppError::Validation(
                "path must be workspace-relative and cannot traverse parents".into(),
            )
        })
}

fn base64_decode(value: &str) -> Result<Vec<u8>, AppError> {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    BASE64
        .decode(value)
        .map_err(|_| AppError::Validation("data must be valid base64".into()))
}

fn base64_encode(data: &[u8]) -> String {
    use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
    BASE64.encode(data)
}

/// Runs a sandbox operation with provisioning + audit. The operation runs
/// first (provisioning is idempotent), then a `sandbox.op` audit row records
/// the outcome.
async fn run_sandbox_op<F, T>(
    state: &AppState,
    user: &AuthenticatedUser,
    workspace_id: Uuid,
    action: &str,
    operation: F,
) -> Result<T, AppError>
where
    F: std::future::Future<Output = Result<T, AppError>>,
{
    // Idempotent provisioning on first use per workspace.
    sandbox_client(state)?
        .provision_workspace(workspace_id)
        .await
        .map_err(|_| {
            AppError::ServiceUnavailable(
                "sandbox unavailable — daemon not reachable or not provisioned",
            )
        })?;
    let result = operation.await;
    let outcome = if result.is_ok() { "success" } else { "failure" };
    let mut tx = state.pool.begin().await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        action,
        "sandbox",
        Some(workspace_id.to_string()),
        outcome,
    )
    .await?;
    tx.commit().await?;
    result
}

async fn drain_terminal(
    client: &SandboxClient,
    terminal_id: Uuid,
    timeout_seconds: u64,
) -> Result<ExecResponse, AppError> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
    let mut output: Vec<u8> = Vec::new();
    let mut cursor = 0_u64;
    let mut state = gobrowse_core::sandbox::TerminalState::Running;
    let mut output_complete = false;
    while tokio::time::Instant::now() < deadline && output.len() < 60 * 1024 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let wait_ms = (u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX)).clamp(1, 500);
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.terminal_read_output(terminal_id, cursor, 32 * 1024, wait_ms),
        )
        .await
        .map_err(|_| AppError::ServiceUnavailable("sandbox operation timed out"))?
        .map_err(sandbox_error_to_app)?;
        cursor = read.next_cursor;
        output.extend_from_slice(&read.data);
        state = read.state;
        output_complete = read.output_complete;
        if output_complete
            || matches!(
                state,
                gobrowse_core::sandbox::TerminalState::Exited
                    | gobrowse_core::sandbox::TerminalState::Terminated
                    | gobrowse_core::sandbox::TerminalState::Unrecoverable
            )
        {
            break;
        }
    }
    let inspect = client.terminal_inspect(terminal_id).await.ok();
    let exit_code = inspect.as_ref().and_then(|info| info.exit_code);
    let state = inspect.as_ref().map(|info| info.state).unwrap_or(state);
    let _ = client.terminal_terminate(terminal_id).await;
    let text = String::from_utf8_lossy(&output[..output.len().min(60 * 1024)]).into_owned();
    Ok(ExecResponse {
        terminal_id,
        exit_code,
        state: format!("{state:?}"),
        output_complete,
        output: text,
    })
}

/// Maps a sandbox client error to an actionable HTTP error.
fn sandbox_error_to_app(error: crate::sandbox_client::SandboxClientError) -> AppError {
    use crate::sandbox_client::SandboxClientError;
    match error {
        SandboxClientError::Disconnected => AppError::ServiceUnavailable(
            "sandbox unavailable — daemon not reachable or not provisioned",
        ),
        SandboxClientError::Unauthorized => {
            AppError::Conflict("sandbox authentication failed — check sandbox_auth_token")
        }
        SandboxClientError::NotFound => AppError::NotFound,
        SandboxClientError::PolicyDenied => AppError::Forbidden,
        SandboxClientError::LimitExceeded => AppError::Conflict("sandbox resource limit exceeded"),
        SandboxClientError::Timeout => AppError::ServiceUnavailable("sandbox operation timed out"),
        SandboxClientError::ProtocolViolation(message) => {
            AppError::Internal(anyhow::anyhow!("sandbox protocol violation: {message}"))
        }
        SandboxClientError::Internal(message) => {
            AppError::Internal(anyhow::anyhow!("sandbox internal error: {message}"))
        }
    }
}
