//! Plugin install server flow (Lane C): preview → approved install →
//! dormant, plus list/detail/patch, staged upgrade with permission diff,
//! activation, rollback, uninstall, and marketplace search.
//!
//! Security model:
//! * Preview never installs anything: it resolves an immutable revision,
//!   fetches + validates the manifest, and pins the artifact digest. The
//!   digest is echoed back to the operator and must be supplied on install,
//!   so the artifact cannot be swapped between preview and install (TOCTOU
//!   protection).
//! * Install requires explicit `approve: true` and re-verifies the digest.
//! * Plugins whose manifest declares executable components or a `self_test`
//!   require the sandbox (`features.sandbox` + socket + token); the self-test
//!   runs inside the sandbox in a plugin-specific workspace (id = plugin id)
//!   before any row is committed.
//! * The PLUGIN companion Book is created in the same transaction as the
//!   `plugins` row (amendment A1), carrying `kind='PLUGIN'`,
//!   `provenance='SYSTEM'`, `scope=WORKSPACE|PROFILE`, and metadata with the
//!   plugin id + component-name capabilities.
//! * Permission scopes stored in `plugin_permissions` are references only
//!   (paths, host:port pairs, vault secret reference ids) — never values.
//! * Install/upgrade/rollback serialize per plugin via `SELECT ... FOR UPDATE`
//!   on the `plugins` row.
//!
//! All mutations emit `audit_events` rows.

use std::{
    collections::HashSet,
    path::{Path as FsPath, PathBuf},
    time::Duration,
};

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use gobrowse_core::plugin::{
    MarketplaceError, PluginComponent, PluginComponentType, PluginIdentity, PluginManifest,
    PluginManifestV1, PluginMarketplace, PluginPermission, PluginSource, PluginSourceError,
};
use gobrowse_core::sandbox::{
    MAX_FILE_PAYLOAD_BYTES, MAX_TERMINAL_OUTPUT_READ_BYTES, NetworkPolicy, ResourceLimits,
    TerminalStartRequest, validate_workspace_path,
};
use serde::{Deserialize, Serialize};
// sha2 digest imports removed with self-asserted signature verification.
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    embedding,
    error::AppError,
    library_api::replace_book_snapshot,
    sandbox_client::SandboxClientError,
};

/// Bound on extracted archive size and entry count (zip-bomb protection).
const MAX_ARCHIVE_EXTRACTED_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ARCHIVE_ENTRIES: usize = 10_000;
/// Bound on the sandbox upload (files + total bytes).
const MAX_SANDBOX_UPLOAD_FILES: u64 = 10_000;
const MAX_SANDBOX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;
/// Permission domains accepted by the `plugin_permissions` CHECK constraint.
const PERMISSION_DOMAINS: [&str; 7] = [
    "filesystem_read",
    "filesystem_write",
    "network",
    "secrets",
    "subprocess",
    "admin",
    "system_info",
];

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub source_type: String,
    pub source_uri: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub source_type: String,
    pub source_uri: String,
    #[serde(default)]
    pub version: Option<String>,
    /// SHA-256 hex digest returned by a prior preview. Must match the
    /// re-resolved digest or the install is rejected.
    pub expected_digest: String,
    /// Explicit operator approval; install is rejected without it.
    pub approve: bool,
    #[serde(default)]
    pub workspace_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchPluginRequest {
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub trust: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpgradeRequest {
    pub version: String,
    #[serde(default)]
    pub source_uri: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
}

#[derive(Debug, Serialize)]
pub struct PluginSourceRef {
    pub source_type: String,
    pub source_uri: String,
    pub commit_sha: Option<String>,
    pub digest: String,
}

#[derive(Debug, Serialize)]
pub struct ComponentPreview {
    pub r#type: String,
    pub name: String,
    pub r#ref: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Permission preview: `domain` + reference `scope_value` (paths, host:port
/// pairs, vault secret reference ids — never secret values).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PermissionPreview {
    pub domain: String,
    pub scope_value: String,
}

impl From<&PluginPermission> for PermissionPreview {
    fn from(permission: &PluginPermission) -> Self {
        Self {
            domain: permission.domain.clone(),
            scope_value: permission.scope_value.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct SelfTestPreview {
    pub command: Vec<String>,
    pub timeout: u32,
}

#[derive(Debug, Serialize)]
pub struct PluginPreview {
    pub identity: PluginIdentity,
    pub name: String,
    pub version: String,
    pub publisher: String,
    pub description: String,
    pub source: PluginSourceRef,
    pub components: Vec<ComponentPreview>,
    /// Reference names only; secrets are listed as their vault reference ids.
    pub permissions: Vec<PermissionPreview>,
    pub network_policy: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<gobrowse_core::plugin::PluginResourceLimits>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub self_test: Option<SelfTestPreview>,
    /// Always `UNTRUSTED` until the operator approves installation.
    pub trust: String,
    pub update_policy: String,
}

#[derive(Debug, Serialize)]
pub struct InstallResponse {
    pub plugin_id: Uuid,
    pub book_id: Uuid,
    pub state: String,
    pub trust: String,
}

#[derive(Debug, Serialize)]
pub struct PluginListItem {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub version: String,
    pub publisher: Option<String>,
    pub source_type: String,
    pub source_uri: String,
    pub trust: String,
    pub state: String,
    pub verified: bool,
    pub workspace_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct ComponentRow {
    pub component_type: String,
    pub name: String,
    pub manifest_ref: String,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct InstallationRow {
    pub id: Uuid,
    pub version: String,
    pub artifact_digest: String,
    pub status: String,
    pub installed_by: Option<Uuid>,
    pub self_test_result: Option<serde_json::Value>,
    pub installed_at: Option<OffsetDateTime>,
    pub activated_at: Option<OffsetDateTime>,
    pub rolled_back_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct PluginDetail {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub version: String,
    pub publisher: Option<String>,
    pub source_type: String,
    pub source_uri: String,
    pub commit_sha: Option<String>,
    pub artifact_digest: Option<String>,
    pub signature: Option<serde_json::Value>,
    pub verified: bool,
    pub trust: String,
    pub state: String,
    pub install_path: Option<String>,
    pub manifest_version: i32,
    pub sandbox_policy: serde_json::Value,
    pub network_policy: String,
    pub resource_limits: Option<serde_json::Value>,
    pub workspace_id: Option<Uuid>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub components: Vec<ComponentRow>,
    pub permissions: Vec<PermissionPreview>,
    pub installations: Vec<InstallationRow>,
}

#[derive(Debug, Serialize)]
pub struct PermissionDiff {
    pub added: Vec<PermissionPreview>,
    pub removed: Vec<PermissionPreview>,
}

#[derive(Debug, Serialize)]
pub struct NameDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct UpgradeDiffResponse {
    pub installation_id: Uuid,
    pub current_version: String,
    pub new_version: String,
    pub artifact_digest: String,
    pub commit_sha: Option<String>,
    pub permissions: PermissionDiff,
    pub components: NameDiff,
    pub capabilities: NameDiff,
}

#[derive(Debug, Serialize)]
pub struct ActivationResponse {
    pub plugin_id: Uuid,
    pub version: String,
    pub state: String,
    pub installation_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct SearchResult {
    pub name: String,
    pub publisher: String,
    pub version: String,
    pub description: String,
    pub source: String,
    pub source_uri: String,
    pub trust: String,
    pub capabilities: Vec<String>,
    pub popularity: u64,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreviewRequest>,
) -> Result<Json<PluginPreview>, AppError> {
    let user = require_user(&state, &headers).await?;
    let source = state.plugin_source_for(&input.source_type)?;
    let identity = PluginIdentity {
        source_type: input.source_type.clone(),
        source_uri: validate_source_uri(&input.source_uri)?,
        commit_sha: None,
        version: input.version,
    };
    let resolved = source.resolve(&identity).await.map_err(map_source_error)?;
    let raw = source
        .fetch_manifest(&resolved)
        .await
        .map_err(map_source_error)?;
    let manifest = parse_and_validate_manifest(raw)?;
    let permissions: Vec<PermissionPreview> = Vec::<PluginPermission>::from(&manifest)
        .iter()
        .map(PermissionPreview::from)
        .collect();
    let components: Vec<ComponentPreview> = manifest
        .components
        .iter()
        .map(|component| ComponentPreview {
            r#type: component_type_db(component.r#type).to_owned(),
            name: component.name.clone(),
            r#ref: component.r#ref.clone(),
            description: component.description.clone(),
        })
        .collect();
    let (network_policy, _, _) = manifest_sandbox(&manifest);
    let self_test = manifest.self_test.as_ref().map(|test| SelfTestPreview {
        command: test.command.clone(),
        timeout: test.timeout_seconds,
    });
    let mut audit_tx = state.pool.begin().await?;
    audit(
        &mut audit_tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.previewed",
        "plugin",
        None,
        "success",
    )
    .await?;
    audit_tx.commit().await?;
    Ok(Json(PluginPreview {
        identity: resolved.identity.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        publisher: manifest
            .publisher
            .as_ref()
            .map(|publisher| publisher.name.clone())
            .unwrap_or_default(),
        description: manifest.description.clone(),
        source: PluginSourceRef {
            source_type: resolved.identity.source_type.clone(),
            source_uri: resolved.identity.source_uri.clone(),
            commit_sha: resolved.identity.commit_sha.clone(),
            digest: resolved.artifact_digest.clone(),
        },
        components,
        permissions,
        network_policy,
        resource_limits: manifest
            .sandbox
            .as_ref()
            .and_then(|sandbox| sandbox.resource_limits),
        self_test,
        trust: "UNTRUSTED".into(),
        update_policy: "manual".into(),
    }))
}

#[allow(clippy::too_many_lines)]
pub async fn install(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<InstallRequest>,
) -> Result<Json<InstallResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let source_type = input.source_type.trim().to_owned();
    if source_type != "github_release" {
        return Err(AppError::Validation(format!(
            "unsupported plugin source type `{source_type}`; supported: github_release"
        )));
    }
    let source_uri = validate_source_uri(&input.source_uri)?;
    if !is_sha256_hex(&input.expected_digest) {
        return Err(AppError::Validation(
            "expected_digest must be a 64-character lowercase hex SHA-256 digest".into(),
        ));
    }
    if !input.approve {
        return Err(AppError::Validation(
            "installation requires explicit approval (approve: true)".into(),
        ));
    }
    if let Some(workspace_id) = input.workspace_id {
        require_workspace_owner(&state, &user, workspace_id).await?;
    } else if !is_admin(&user) {
        return Err(AppError::Forbidden);
    }

    let source = state.plugin_source_for(&source_type)?;
    let identity = PluginIdentity {
        source_type,
        source_uri: source_uri.clone(),
        commit_sha: None,
        version: input.version,
    };
    let resolved = source.resolve(&identity).await.map_err(map_source_error)?;
    if !resolved
        .artifact_digest
        .eq_ignore_ascii_case(&input.expected_digest)
    {
        return Err(AppError::Conflict(
            "artifact digest does not match the digest from preview — the artifact changed between preview and install; run preview again and retry with the fresh digest",
        ));
    }
    let raw = source
        .fetch_manifest(&resolved)
        .await
        .map_err(map_source_error)?;
    let manifest = parse_and_validate_manifest(raw.clone())?;
    let permissions = Vec::<PluginPermission>::from(&manifest);
    validate_permissions(&permissions)?;
    validate_components(&manifest.components)?;

    let name_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM plugins WHERE profile_id = $1 AND name = $2)",
    )
    .bind(user.profile_id)
    .bind(&manifest.name)
    .fetch_one(&state.pool)
    .await?;
    if name_exists {
        return Err(AppError::Conflict(
            "a plugin with this name is already installed for this profile",
        ));
    }

    let plugin_id = Uuid::now_v7();
    let staging = state
        .settings
        .features
        .plugins_dir
        .join("staging")
        .join(plugin_id.to_string());
    let _guard = StagingGuard::new(staging.clone());
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("create staging dir: {error}")))?;
    let artifact_path = staging.join("artifact.bin");
    source
        .download_artifact(&resolved, &artifact_path)
        .await
        .map_err(map_source_error)?;
    let artifact_bytes = tokio::fs::read(&artifact_path)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("read staged artifact: {error}")))?;
    let extracted = staging.join("extracted");
    extract_staged(&artifact_path, &extracted).await?;

    // Static policy inspection is complete; now enforce the sandbox contract
    // for plugins that execute code.
    let has_executable = manifest
        .components
        .iter()
        .any(|component| component.r#type == PluginComponentType::Executable);
    let needs_sandbox = has_executable || manifest.self_test.is_some();
    let self_test_result = if needs_sandbox {
        require_sandbox(&state)?;
        if manifest.self_test.is_some() {
            Some(run_plugin_self_test(&state, plugin_id, &manifest, &extracted).await?)
        } else {
            None
        }
    } else {
        None
    };

    let signature = raw.get("signature").cloned();
    let verified = signature
        .as_ref()
        .is_some_and(|signature| verify_artifact_signature(signature, &artifact_bytes));
    let trust = if verified {
        "VERIFIED"
    } else {
        "USER_PROVIDED"
    };
    let (network_policy, sandbox_policy, resource_limits) = manifest_sandbox(&manifest);

    let mut tx = state.pool.begin().await?;
    insert_plugin_row(
        &mut tx,
        &user,
        &manifest,
        input.workspace_id,
        &source_uri,
        resolved.identity.commit_sha.as_deref(),
        &resolved.artifact_digest,
        signature.as_ref(),
        verified,
        trust,
        &network_policy,
        &sandbox_policy,
        resource_limits.as_ref(),
        plugin_id,
    )
    .await?;
    insert_components(&mut tx, plugin_id, &manifest.components).await?;
    insert_permissions(&mut tx, plugin_id, &permissions).await?;
    let installation_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO plugin_installations (id, plugin_id, version, artifact_digest, status, installed_by, self_test_result, installed_at, activated_at) \
         VALUES ($1,$2,$3,$4,'active',$5,$6,now(),now())",
    )
    .bind(installation_id)
    .bind(plugin_id)
    .bind(&manifest.version)
    .bind(&resolved.artifact_digest)
    .bind(user.id)
    .bind(&self_test_result)
    .execute(&mut *tx)
    .await?;
    let book_id = insert_plugin_book(
        &mut tx,
        &user,
        plugin_id,
        input.workspace_id,
        &manifest,
        trust,
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.installed",
        "plugin",
        Some(plugin_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await.map_err(|error| {
        if is_unique_violation(&error) {
            AppError::Conflict("a plugin with this name is already installed for this profile")
        } else {
            AppError::Database(error)
        }
    })?;
    Ok(Json(InstallResponse {
        plugin_id,
        book_id,
        state: "dormant".into(),
        trust: trust.into(),
    }))
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<PluginListItem>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, name, description, version, publisher, source_type, source_uri, trust, state, \
                verified, workspace_id, updated_at \
         FROM plugins p WHERE p.profile_id = $1 \
           AND ($2 IN ('OWNER','ADMIN') OR p.workspace_id IS NULL OR EXISTS( \
               SELECT 1 FROM workspace_memberships m WHERE m.workspace_id = p.workspace_id AND m.user_id = $3)) \
         ORDER BY updated_at DESC LIMIT 200",
    )
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| PluginListItem {
                id: row.get("id"),
                name: row.get("name"),
                description: row.get("description"),
                version: row.get("version"),
                publisher: row.get("publisher"),
                source_type: row.get("source_type"),
                source_uri: row.get("source_uri"),
                trust: row.get("trust"),
                state: row.get("state"),
                verified: row.get("verified"),
                workspace_id: row.get("workspace_id"),
                updated_at: row.get("updated_at"),
            })
            .collect(),
    ))
}

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<PluginDetail>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = fetch_visible_plugin(&state, &user, id).await?;
    Ok(Json(build_detail(&state, &row).await?))
}

pub async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<PatchPluginRequest>,
) -> Result<Json<PluginDetail>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let row = lock_plugin_for_write(&mut tx, &user, id).await?;
    let mut new_state = row.state.clone();
    let mut new_trust = row.trust.clone();
    if let Some(state) = &input.state {
        if !matches!(state.as_str(), "enabled" | "dormant") {
            return Err(AppError::Validation(
                "state must be `enabled` or `dormant`".into(),
            ));
        }
        new_state = state.clone();
    }
    if let Some(trust) = &input.trust {
        if !matches!(
            trust.as_str(),
            "USER_PROVIDED" | "AGENT_INFERRED" | "EXTERNAL" | "UNTRUSTED"
        ) {
            return Err(AppError::Validation(
                "trust may only be set to USER_PROVIDED, AGENT_INFERRED, EXTERNAL, or UNTRUSTED; \
                 VERIFIED requires artifact signature verification"
                    .into(),
            ));
        }
        new_trust = trust.clone();
    }
    sqlx::query("UPDATE plugins SET state = $1, trust = $2, updated_at = now() WHERE id = $3")
        .bind(&new_state)
        .bind(&new_trust)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE books SET trust = $1, updated_at = now() \
         WHERE kind = 'PLUGIN' AND metadata->>'plugin_id' = $2::text AND profile_id = $3",
    )
    .bind(&new_trust)
    .bind(id)
    .bind(user.profile_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.updated",
        "plugin",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    let row = fetch_visible_plugin(&state, &user, id).await?;
    Ok(Json(build_detail(&state, &row).await?))
}

pub async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<UpgradeRequest>,
) -> Result<Json<UpgradeDiffResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let version = input.version.trim().to_owned();
    if version.is_empty() || version.len() > 64 {
        return Err(AppError::Validation(
            "version must contain 1 to 64 characters".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = lock_plugin_for_write(&mut tx, &user, id).await?;
    if row.version == version {
        return Err(AppError::Conflict(
            "this version is already the installed version",
        ));
    }
    let source_uri = input
        .source_uri
        .clone()
        .unwrap_or_else(|| row.source_uri.clone());
    let identity = PluginIdentity {
        source_type: row.source_type.clone(),
        source_uri: validate_source_uri(&source_uri)?,
        commit_sha: None,
        version: Some(version.clone()),
    };
    let resolved = state
        .plugin_source
        .resolve(&identity)
        .await
        .map_err(map_source_error)?;
    let raw = state
        .plugin_source
        .fetch_manifest(&resolved)
        .await
        .map_err(map_source_error)?;
    let manifest = parse_and_validate_manifest(raw)?;
    if manifest.name != row.name {
        return Err(AppError::Validation(
            "plugin name cannot change between versions".into(),
        ));
    }
    // The installation row is keyed by the CANONICAL manifest version (e.g.
    // "1.1.0"), not the raw source tag (e.g. "v1.1.0"), so plugins.version and
    // plugin_installations.version always agree.
    let canonical_version = manifest.version.clone();
    let permissions = Vec::<PluginPermission>::from(&manifest);
    validate_permissions(&permissions)?;
    validate_components(&manifest.components)?;

    let current_permissions = query_permissions(&state, id).await?;
    let current_components = query_component_names(&state, id).await?;
    let next_permissions: Vec<PermissionPreview> =
        permissions.iter().map(PermissionPreview::from).collect();
    let next_components: Vec<String> = manifest
        .components
        .iter()
        .map(|component| component.name.clone())
        .collect();
    let (added_permissions, removed_permissions) =
        compute_permission_diff(&current_permissions, &next_permissions);
    let (added_components, removed_components) =
        compute_name_diff(&current_components, &next_components);

    // (Re)stage the installation row for the new version.
    let status: Option<String> = sqlx::query_scalar(
        "SELECT status FROM plugin_installations WHERE plugin_id = $1 AND version = $2",
    )
    .bind(id)
    .bind(&canonical_version)
    .fetch_optional(&mut *tx)
    .await?;
    let installation_id = match status.as_deref() {
        Some("staged") => {
            return Err(AppError::Conflict(
                "an upgrade to this version is already staged for this plugin",
            ));
        }
        Some("active") => {
            return Err(AppError::Conflict(
                "this version is already the installed version",
            ));
        }
        Some("failed") | Some("rolled_back") => {
            let existing_id: Uuid = sqlx::query_scalar(
                "SELECT id FROM plugin_installations WHERE plugin_id = $1 AND version = $2",
            )
            .bind(id)
            .bind(&canonical_version)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE plugin_installations SET status = 'staged', artifact_digest = $3, self_test_result = NULL, created_at = now() \
                 WHERE plugin_id = $1 AND version = $2",
            )
            .bind(id)
            .bind(&canonical_version)
            .bind(&resolved.artifact_digest)
            .execute(&mut *tx)
            .await?;
            existing_id
        }
        None => {
            let installation_id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO plugin_installations (id, plugin_id, version, artifact_digest, status) \
                 VALUES ($1,$2,$3,$4,'staged')",
            )
            .bind(installation_id)
            .bind(id)
            .bind(&canonical_version)
            .bind(&resolved.artifact_digest)
            .execute(&mut *tx)
            .await?;
            installation_id
        }
        _ => unreachable!("plugin_installations.status CHECK limits values"),
    };
    sqlx::query("UPDATE plugins SET state = 'staged', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.upgrade_staged",
        "plugin",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(UpgradeDiffResponse {
        installation_id,
        current_version: row.version.clone(),
        new_version: canonical_version,
        artifact_digest: resolved.artifact_digest,
        commit_sha: resolved.identity.commit_sha,
        permissions: PermissionDiff {
            added: added_permissions,
            removed: removed_permissions,
        },
        components: NameDiff {
            added: added_components.clone(),
            removed: removed_components.clone(),
        },
        capabilities: NameDiff {
            added: added_components,
            removed: removed_components,
        },
    }))
}

pub async fn activate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(Uuid, String)>,
) -> Result<Json<ActivationResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    if version.is_empty() || version.len() > 64 {
        return Err(AppError::Validation(
            "version must contain 1 to 64 characters".into(),
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = lock_plugin_for_write(&mut tx, &user, id).await?;
    let installation = sqlx::query(
        "SELECT id, version, artifact_digest, status FROM plugin_installations \
         WHERE plugin_id = $1 AND version = $2",
    )
    .bind(id)
    .bind(&version)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let installation_id: Uuid = installation.get("id");
    let staged_digest: String = installation.get("artifact_digest");
    let status: String = installation.get("status");
    if status != "staged" {
        return Err(AppError::Conflict(
            "this version is not staged for activation; run upgrade first",
        ));
    }

    let staged = stage_and_verify(&state, &row, &version, &staged_digest).await?;
    let manifest = &staged.manifest;
    let permissions = Vec::<PluginPermission>::from(manifest);
    let (network_policy, sandbox_policy, resource_limits) = manifest_sandbox(manifest);
    let self_test_result = if staged.needs_sandbox {
        require_sandbox(&state)?;
        if manifest.self_test.is_some() {
            Some(run_plugin_self_test(&state, id, manifest, &staged.extracted_dir).await?)
        } else {
            None
        }
    } else {
        None
    };

    let signature = staged.raw.get("signature").cloned();
    let verified = match (&signature, &staged.artifact_bytes) {
        (Some(signature), Some(bytes)) => verify_artifact_signature(signature, bytes),
        _ => false,
    };
    let new_trust = if verified {
        "VERIFIED".to_owned()
    } else if row.trust != "UNTRUSTED" {
        row.trust.clone()
    } else {
        "USER_PROVIDED".to_owned()
    };

    sqlx::query("DELETE FROM plugin_components WHERE plugin_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_components(&mut tx, id, &manifest.components).await?;
    sqlx::query("DELETE FROM plugin_permissions WHERE plugin_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_permissions(&mut tx, id, &permissions).await?;

    sqlx::query(
        "UPDATE plugins SET version = $1, description = $2, artifact_digest = $3, commit_sha = $4, \
                publisher = $5, signature = $6, verified = $7, trust = $8, state = 'dormant', \
                manifest_version = $9, sandbox_policy = $10, network_policy = $11, resource_limits = $12, \
                updated_at = now() \
         WHERE id = $13",
    )
    .bind(&manifest.version)
    .bind(&manifest.description)
    .bind(&staged_digest)
    .bind(staged.commit_sha.as_deref())
    .bind(manifest.publisher.as_ref().map(|publisher| publisher.name.clone()))
    .bind(&signature)
    .bind(verified)
    .bind(&new_trust)
    .bind(manifest.manifest_version as i32)
    .bind(&sandbox_policy)
    .bind(&network_policy)
    .bind(&resource_limits)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE plugin_installations SET status = 'active', installed_at = now(), activated_at = now(), \
                self_test_result = $1, installed_by = $2 \
         WHERE id = $3",
    )
    .bind(&self_test_result)
    .bind(user.id)
    .bind(installation_id)
    .execute(&mut *tx)
    .await?;

    update_plugin_book(
        &mut tx,
        &user,
        id,
        manifest,
        &new_trust,
        format!("Plugin upgraded to version {}", manifest.version),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.upgrade_activated",
        "plugin",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ActivationResponse {
        plugin_id: id,
        version: manifest.version.clone(),
        state: "dormant".into(),
        installation_id,
    }))
}

pub async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ActivationResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let row = lock_plugin_for_write(&mut tx, &user, id).await?;
    let current: Uuid = sqlx::query_scalar(
        "SELECT id FROM plugin_installations WHERE plugin_id = $1 AND version = $2 AND status = 'active'",
    )
    .bind(id)
    .bind(&row.version)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(AppError::Conflict("current installation record is missing"))?;
    let previous = sqlx::query(
        "SELECT id, version, artifact_digest FROM plugin_installations \
         WHERE plugin_id = $1 AND status = 'active' AND version <> $2 \
         ORDER BY activated_at DESC NULLS LAST LIMIT 1",
    )
    .bind(id)
    .bind(&row.version)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(previous) = previous else {
        return Err(AppError::Conflict(
            "no previous version is available to roll back to",
        ));
    };
    let previous_id: Uuid = previous.get("id");
    let previous_version: String = previous.get("version");
    let previous_digest: String = previous.get("artifact_digest");

    let staged = stage_and_verify(&state, &row, &previous_version, &previous_digest).await?;
    let manifest = &staged.manifest;
    let permissions = Vec::<PluginPermission>::from(manifest);
    let (network_policy, sandbox_policy, resource_limits) = manifest_sandbox(manifest);
    let signature = staged.raw.get("signature").cloned();
    let verified = match (&signature, &staged.artifact_bytes) {
        (Some(signature), Some(bytes)) => verify_artifact_signature(signature, bytes),
        _ => false,
    };
    let new_trust = if verified {
        "VERIFIED".to_owned()
    } else if row.trust != "UNTRUSTED" {
        row.trust.clone()
    } else {
        "USER_PROVIDED".to_owned()
    };

    sqlx::query("DELETE FROM plugin_components WHERE plugin_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_components(&mut tx, id, &manifest.components).await?;
    sqlx::query("DELETE FROM plugin_permissions WHERE plugin_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    insert_permissions(&mut tx, id, &permissions).await?;

    sqlx::query(
        "UPDATE plugins SET version = $1, description = $2, artifact_digest = $3, commit_sha = $4, \
                publisher = $5, signature = $6, verified = $7, trust = $8, state = 'dormant', \
                manifest_version = $9, sandbox_policy = $10, network_policy = $11, resource_limits = $12, \
                updated_at = now() \
         WHERE id = $13",
    )
    .bind(&manifest.version)
    .bind(&manifest.description)
    .bind(&previous_digest)
    .bind(staged.commit_sha.as_deref())
    .bind(manifest.publisher.as_ref().map(|publisher| publisher.name.clone()))
    .bind(&signature)
    .bind(verified)
    .bind(&new_trust)
    .bind(manifest.manifest_version as i32)
    .bind(&sandbox_policy)
    .bind(&network_policy)
    .bind(&resource_limits)
    .bind(id)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE plugin_installations SET status = 'rolled_back', rolled_back_at = now() WHERE id = $1",
    )
    .bind(current)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE plugin_installations SET status = 'active', activated_at = now(), installed_by = $1 WHERE id = $2",
    )
    .bind(user.id)
    .bind(previous_id)
    .execute(&mut *tx)
    .await?;

    update_plugin_book(
        &mut tx,
        &user,
        id,
        manifest,
        &new_trust,
        format!("Plugin rolled back to version {}", manifest.version),
    )
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.rolled_back",
        "plugin",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(ActivationResponse {
        plugin_id: id,
        version: manifest.version.clone(),
        state: "dormant".into(),
        installation_id: previous_id,
    }))
}

pub async fn delete_plugin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    lock_plugin_for_write(&mut tx, &user, id).await?;
    let book_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM books WHERE kind = 'PLUGIN' AND metadata->>'plugin_id' = $1::text AND profile_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(book_id) = book_id {
        sqlx::query("DELETE FROM books WHERE id = $1")
            .bind(book_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("DELETE FROM plugins WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.deleted",
        "plugin",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<SearchRequest>,
) -> Result<Json<Vec<SearchResult>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let query = input.query.trim().to_owned();
    if query.is_empty() || query.chars().count() > 200 {
        return Err(AppError::Validation(
            "search query must contain 1 to 200 characters".into(),
        ));
    }
    let entries = state
        .plugin_marketplace
        .search(&query)
        .await
        .map_err(map_marketplace_error)?;
    let mut audit_tx = state.pool.begin().await?;
    audit(
        &mut audit_tx,
        Some(user.id),
        Some(user.profile_id),
        "plugin.searched",
        "plugin",
        None,
        "success",
    )
    .await?;
    audit_tx.commit().await?;
    Ok(Json(
        entries
            .into_iter()
            .map(|entry| SearchResult {
                name: entry.name,
                publisher: entry.publisher,
                version: entry.latest_version,
                description: entry.description,
                source: format!("github_release:{}", entry.id),
                source_uri: format!("https://github.com/{}", entry.id),
                trust: "UNTRUSTED".into(),
                capabilities: Vec::new(),
                popularity: entry.download_count,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PluginRow {
    id: Uuid,
    workspace_id: Option<Uuid>,
    name: String,
    description: String,
    version: String,
    source_type: String,
    source_uri: String,
    commit_sha: Option<String>,
    artifact_digest: Option<String>,
    publisher: Option<String>,
    signature: Option<serde_json::Value>,
    verified: bool,
    trust: String,
    state: String,
    install_path: Option<String>,
    manifest_version: i32,
    sandbox_policy: serde_json::Value,
    network_policy: String,
    resource_limits: Option<serde_json::Value>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

fn plugin_row_from(row: &sqlx::postgres::PgRow) -> PluginRow {
    PluginRow {
        id: row.get("id"),
        workspace_id: row.get("workspace_id"),
        name: row.get("name"),
        description: row.get("description"),
        version: row.get("version"),
        source_type: row.get("source_type"),
        source_uri: row.get("source_uri"),
        commit_sha: row.get("commit_sha"),
        artifact_digest: row.get("artifact_digest"),
        publisher: row.get("publisher"),
        signature: row.get("signature"),
        verified: row.get("verified"),
        trust: row.get("trust"),
        state: row.get("state"),
        install_path: row.get("install_path"),
        manifest_version: row.get("manifest_version"),
        sandbox_policy: row.get("sandbox_policy"),
        network_policy: row.get("network_policy"),
        resource_limits: row.get("resource_limits"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

const PLUGIN_COLUMNS: &str = "id, profile_id, workspace_id, name, description, version, source_type, \
     source_uri, commit_sha, artifact_digest, publisher, signature, verified, trust, state, \
     install_path, manifest_version, sandbox_policy, network_policy, resource_limits, created_at, updated_at";

/// Lock the plugin row for a write operation. Profile-level plugins require a
/// profile OWNER/ADMIN; workspace-scoped plugins require workspace OWNER
/// membership (matching the install gate).
async fn lock_plugin_for_write(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    plugin_id: Uuid,
) -> Result<PluginRow, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {PLUGIN_COLUMNS} FROM plugins p WHERE p.id = $1 AND p.profile_id = $2 \
           AND ($3 IN ('OWNER','ADMIN') OR (p.workspace_id IS NOT NULL AND EXISTS( \
               SELECT 1 FROM workspace_memberships m WHERE m.workspace_id = p.workspace_id AND m.user_id = $4 \
               AND m.access IN ('OWNER')))) \
         FOR UPDATE"
    ))
    .bind(plugin_id)
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(plugin_row_from(&row))
}

async fn fetch_visible_plugin(
    state: &AppState,
    user: &AuthenticatedUser,
    plugin_id: Uuid,
) -> Result<PluginRow, AppError> {
    let row = sqlx::query(&format!(
        "SELECT {PLUGIN_COLUMNS} FROM plugins p WHERE p.id = $1 AND p.profile_id = $2 \
           AND ($3 IN ('OWNER','ADMIN') OR p.workspace_id IS NULL OR EXISTS( \
               SELECT 1 FROM workspace_memberships m WHERE m.workspace_id = p.workspace_id AND m.user_id = $4))"
    ))
    .bind(plugin_id)
    .bind(user.profile_id)
    .bind(&user.role)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or(AppError::NotFound)?;
    Ok(plugin_row_from(&row))
}

async fn build_detail(state: &AppState, row: &PluginRow) -> Result<PluginDetail, AppError> {
    let components = query_components(state, row.id).await?;
    let permissions = query_permissions(state, row.id).await?;
    let installations = query_installations(state, row.id).await?;
    Ok(PluginDetail {
        id: row.id,
        name: row.name.clone(),
        description: row.description.clone(),
        version: row.version.clone(),
        publisher: row.publisher.clone(),
        source_type: row.source_type.clone(),
        source_uri: row.source_uri.clone(),
        commit_sha: row.commit_sha.clone(),
        artifact_digest: row.artifact_digest.clone(),
        signature: row.signature.clone(),
        verified: row.verified,
        trust: row.trust.clone(),
        state: row.state.clone(),
        install_path: row.install_path.clone(),
        manifest_version: row.manifest_version,
        sandbox_policy: row.sandbox_policy.clone(),
        network_policy: row.network_policy.clone(),
        resource_limits: row.resource_limits.clone(),
        workspace_id: row.workspace_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
        components,
        permissions,
        installations,
    })
}

async fn query_components(
    state: &AppState,
    plugin_id: Uuid,
) -> Result<Vec<ComponentRow>, AppError> {
    let rows = sqlx::query(
        "SELECT component_type, name, manifest_ref, metadata FROM plugin_components \
         WHERE plugin_id = $1 ORDER BY name",
    )
    .bind(plugin_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| ComponentRow {
            component_type: row.get("component_type"),
            name: row.get("name"),
            manifest_ref: row.get("manifest_ref"),
            metadata: row.get("metadata"),
        })
        .collect())
}

async fn query_component_names(state: &AppState, plugin_id: Uuid) -> Result<Vec<String>, AppError> {
    let rows = sqlx::query("SELECT name FROM plugin_components WHERE plugin_id = $1 ORDER BY name")
        .bind(plugin_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(rows.into_iter().map(|row| row.get("name")).collect())
}

async fn query_permissions(
    state: &AppState,
    plugin_id: Uuid,
) -> Result<Vec<PermissionPreview>, AppError> {
    let rows = sqlx::query(
        "SELECT permission_domain, scope_value FROM plugin_permissions \
         WHERE plugin_id = $1 ORDER BY permission_domain, scope_value",
    )
    .bind(plugin_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| PermissionPreview {
            domain: row.get("permission_domain"),
            scope_value: row.get("scope_value"),
        })
        .collect())
}

async fn query_installations(
    state: &AppState,
    plugin_id: Uuid,
) -> Result<Vec<InstallationRow>, AppError> {
    let rows = sqlx::query(
        "SELECT id, version, artifact_digest, status, installed_by, self_test_result, installed_at, \
                activated_at, rolled_back_at, created_at \
         FROM plugin_installations WHERE plugin_id = $1 ORDER BY created_at DESC",
    )
    .bind(plugin_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| InstallationRow {
            id: row.get("id"),
            version: row.get("version"),
            artifact_digest: row.get("artifact_digest"),
            status: row.get("status"),
            installed_by: row.get("installed_by"),
            self_test_result: row.get("self_test_result"),
            installed_at: row.get("installed_at"),
            activated_at: row.get("activated_at"),
            rolled_back_at: row.get("rolled_back_at"),
            created_at: row.get("created_at"),
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
async fn insert_plugin_row(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    manifest: &PluginManifestV1,
    workspace_id: Option<Uuid>,
    source_uri: &str,
    commit_sha: Option<&str>,
    artifact_digest: &str,
    signature: Option<&serde_json::Value>,
    verified: bool,
    trust: &str,
    network_policy: &str,
    sandbox_policy: &serde_json::Value,
    resource_limits: Option<&serde_json::Value>,
    plugin_id: Uuid,
) -> Result<(), AppError> {
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO plugins (id, profile_id, workspace_id, name, description, version, source_type, \
                source_uri, commit_sha, artifact_digest, publisher, signature, verified, trust, state, \
                install_path, manifest_version, sandbox_policy, network_policy, resource_limits, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'dormant',NULL,$15,$16,$17,$18,$19,$19)",
    )
    .bind(plugin_id)
    .bind(user.profile_id)
    .bind(workspace_id)
    .bind(&manifest.name)
    .bind(&manifest.description)
    .bind(&manifest.version)
    .bind("github_release")
    .bind(source_uri)
    .bind(commit_sha)
    .bind(artifact_digest)
    .bind(manifest.publisher.as_ref().map(|publisher| publisher.name.clone()))
    .bind(signature)
    .bind(verified)
    .bind(trust)
    .bind(manifest.manifest_version as i32)
    .bind(sandbox_policy)
    .bind(network_policy)
    .bind(resource_limits)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_components(
    tx: &mut Transaction<'_, Postgres>,
    plugin_id: Uuid,
    components: &[PluginComponent],
) -> Result<(), AppError> {
    for component in components {
        sqlx::query(
            "INSERT INTO plugin_components (id, plugin_id, component_type, name, manifest_ref, metadata) \
             VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(Uuid::now_v7())
        .bind(plugin_id)
        .bind(component_type_db(component.r#type))
        .bind(&component.name)
        .bind(&component.r#ref)
        .bind(component.metadata.clone().unwrap_or_else(|| serde_json::json!({})))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn insert_permissions(
    tx: &mut Transaction<'_, Postgres>,
    plugin_id: Uuid,
    permissions: &[PluginPermission],
) -> Result<(), AppError> {
    for permission in permissions {
        sqlx::query(
            "INSERT INTO plugin_permissions (id, plugin_id, permission_domain, scope_value) \
             VALUES ($1,$2,$3,$4)",
        )
        .bind(Uuid::now_v7())
        .bind(plugin_id)
        .bind(&permission.domain)
        .bind(&permission.scope_value)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Creates the PLUGIN companion Book in the same transaction as the plugin
/// row (amendment A1): `book_type='INSTRUCTION'`, `kind='PLUGIN'`,
/// `provenance='SYSTEM'`, `trust` mirroring the plugin row,
/// `scope=WORKSPACE|PROFILE`, `security_classification='INTERNAL'`,
/// `author='system'`, metadata `{plugin_id, capabilities, version}`.
async fn insert_plugin_book(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    plugin_id: Uuid,
    workspace_id: Option<Uuid>,
    manifest: &PluginManifestV1,
    trust: &str,
) -> Result<Uuid, AppError> {
    let book_id = Uuid::now_v7();
    let capabilities: Vec<String> = manifest
        .components
        .iter()
        .map(|component| component.name.clone())
        .collect();
    let metadata = serde_json::json!({
        "plugin_id": plugin_id,
        "capabilities": capabilities,
        "version": manifest.version,
    });
    let scope = if workspace_id.is_some() {
        "WORKSPACE"
    } else {
        "PROFILE"
    };
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust, \
                source, author, workspace_id, security_classification, metadata, kind, owner_user_id, \
                created_by_user_id, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,'INSTRUCTION',$5,'{}'::text[],'SYSTEM',$6,'{}'::jsonb,'system',$7,'INTERNAL',$8,'PLUGIN',$9,$10,$11,$11)",
    )
    .bind(book_id)
    .bind(user.profile_id)
    .bind(&manifest.name)
    .bind(&manifest.description)
    .bind(scope)
    .bind(trust)
    .bind(workspace_id)
    .bind(&metadata)
    .bind(Some(user.id))
    .bind(user.id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, changed_by, change_reason) \
         VALUES ($1,$2,1,$3,$4,'{}'::text[],$5,$6,'Installed plugin')",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(&manifest.name)
    .bind(&manifest.description)
    .bind(&metadata)
    .bind(user.id)
    .execute(&mut **tx)
    .await?;
    embedding::enqueue_book(tx, user.profile_id, book_id, 1).await?;
    Ok(book_id)
}

/// Updates the PLUGIN companion Book body/metadata after activation or
/// rollback (new description, new capabilities, new version).
async fn update_plugin_book(
    tx: &mut Transaction<'_, Postgres>,
    user: &AuthenticatedUser,
    plugin_id: Uuid,
    manifest: &PluginManifestV1,
    trust: &str,
    reason: String,
) -> Result<(), AppError> {
    let Some(book_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM books WHERE kind = 'PLUGIN' AND metadata->>'plugin_id' = $1::text AND profile_id = $2 FOR UPDATE",
    )
    .bind(plugin_id)
    .bind(user.profile_id)
    .fetch_optional(&mut **tx)
    .await?
    else {
        return Ok(());
    };
    let capabilities: Vec<String> = manifest
        .components
        .iter()
        .map(|component| component.name.clone())
        .collect();
    let metadata = serde_json::json!({
        "plugin_id": plugin_id,
        "capabilities": capabilities,
        "version": manifest.version,
    });
    replace_book_snapshot(
        tx,
        book_id,
        &manifest.name,
        &manifest.description,
        &[],
        &metadata,
        Some(user.id),
        &reason,
    )
    .await?;
    sqlx::query("UPDATE books SET trust = $1 WHERE id = $2")
        .bind(trust)
        .bind(book_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn is_admin(user: &AuthenticatedUser) -> bool {
    matches!(user.role.as_str(), "OWNER" | "ADMIN")
}

fn validate_source_uri(source_uri: &str) -> Result<String, AppError> {
    let source_uri = source_uri.trim().to_owned();
    if source_uri.is_empty() || source_uri.chars().count() > 2_048 {
        return Err(AppError::Validation(
            "source_uri must contain 1 to 2048 characters".into(),
        ));
    }
    Ok(source_uri)
}

async fn require_workspace_owner(
    state: &AppState,
    user: &AuthenticatedUser,
    workspace_id: Uuid,
) -> Result<(), AppError> {
    let allowed: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workspaces w WHERE id = $1 AND profile_id = $2 AND ( \
         $4 IN ('OWNER','ADMIN') OR EXISTS(SELECT 1 FROM workspace_memberships m \
         WHERE m.workspace_id = w.id AND m.user_id = $3 AND m.access IN ('OWNER'))))",
    )
    .bind(workspace_id)
    .bind(user.profile_id)
    .bind(user.id)
    .bind(&user.role)
    .fetch_one(&state.pool)
    .await?;
    if !allowed {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_unique_violation(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(database) if database.is_unique_violation())
}

fn parse_and_validate_manifest(raw: serde_json::Value) -> Result<PluginManifestV1, AppError> {
    let manifest: PluginManifest = serde_json::from_value(raw)
        .map_err(|error| AppError::Validation(format!("invalid plugin manifest: {error}")))?;
    manifest
        .validate()
        .map_err(|error| AppError::Validation(format!("invalid plugin manifest: {error}")))?;
    Ok(manifest
        .as_v1()
        .expect("a validated manifest is version 1")
        .clone())
}

/// Static policy inspection: rejects unknown permission domains and oversized
/// scope values. Scope strings are references (paths, host:port, vault secret
/// reference ids), never secret values.
fn validate_permissions(permissions: &[PluginPermission]) -> Result<(), AppError> {
    for permission in permissions {
        if !PERMISSION_DOMAINS.contains(&permission.domain.as_str()) {
            return Err(AppError::Validation(format!(
                "plugin requests an unknown permission domain `{}`",
                permission.domain
            )));
        }
        if permission.scope_value.is_empty() || permission.scope_value.chars().count() > 512 {
            return Err(AppError::Validation(format!(
                "permission scope for domain `{}` must contain 1 to 512 characters",
                permission.domain
            )));
        }
    }
    Ok(())
}

fn validate_components(components: &[PluginComponent]) -> Result<(), AppError> {
    for component in components {
        if component.name.chars().count() > 200 || component.r#ref.len() > 2_048 {
            return Err(AppError::Validation(
                "component names must be at most 200 characters and refs at most 2048 bytes".into(),
            ));
        }
    }
    Ok(())
}

fn component_type_db(component_type: PluginComponentType) -> &'static str {
    match component_type {
        PluginComponentType::Skill => "skill",
        PluginComponentType::McpServer => "mcp_server",
        PluginComponentType::SourceBook => "source_book",
        PluginComponentType::Executable => "executable",
        PluginComponentType::Asset => "asset",
        PluginComponentType::Schema => "schema",
    }
}

/// Derives the DB `network_policy` string, the `sandbox_policy` jsonb payload,
/// and the optional `resource_limits` jsonb from a manifest.
fn manifest_sandbox(
    manifest: &PluginManifestV1,
) -> (String, serde_json::Value, Option<serde_json::Value>) {
    let sandbox = manifest.sandbox.as_ref();
    let network_policy = sandbox
        .and_then(|sandbox| sandbox.network)
        .unwrap_or(NetworkPolicy::Restricted);
    let network_policy_db = match network_policy {
        NetworkPolicy::None => "NONE",
        NetworkPolicy::Restricted => "RESTRICTED",
        NetworkPolicy::Full => "FULL",
    }
    .to_owned();
    let sandbox_policy = sandbox
        .map(|sandbox| serde_json::to_value(sandbox).unwrap_or_else(|_| serde_json::json!({})))
        .unwrap_or_else(|| serde_json::json!({}));
    let resource_limits = sandbox
        .and_then(|sandbox| sandbox.resource_limits)
        .map(|limits| serde_json::to_value(limits).unwrap_or_else(|_| serde_json::json!({})));
    (network_policy_db, sandbox_policy, resource_limits)
}

fn compute_permission_diff(
    current: &[PermissionPreview],
    next: &[PermissionPreview],
) -> (Vec<PermissionPreview>, Vec<PermissionPreview>) {
    let added = next
        .iter()
        .filter(|permission| !current.contains(permission))
        .cloned()
        .collect();
    let removed = current
        .iter()
        .filter(|permission| !next.contains(permission))
        .cloned()
        .collect();
    (added, removed)
}

fn compute_name_diff(current: &[String], next: &[String]) -> (Vec<String>, Vec<String>) {
    let added = next
        .iter()
        .filter(|name| !current.contains(name))
        .cloned()
        .collect();
    let removed = current
        .iter()
        .filter(|name| !next.contains(name))
        .cloned()
        .collect();
    (added, removed)
}

/// Accepts a manifest `signature` only when it is a valid hex SHA-256 (64
/// chars) or SHA-512 (128 chars) of the artifact bytes. Anything else is
/// rejected (returns false), keeping the plugin UNTRUSTED/USER_PROVIDED.
fn verify_artifact_signature(signature: &serde_json::Value, _artifact_bytes: &[u8]) -> bool {
    // A manifest-declared digest of the artifact is NOT a signature: the
    // publisher controls both values, so trusting it would be self-asserted.
    // VERIFIED is reserved for signatures verified against a known publisher
    // key, which no key registry exists for yet — always return false so
    // installs stay UNTRUSTED/USER_PROVIDED until real key infrastructure lands.
    let _ = signature;
    false
}

// ---------------------------------------------------------------------------
// Staging + artifact extraction
// ---------------------------------------------------------------------------

/// Removes the staging directory when dropped (success or failure).
struct StagingGuard {
    dir: PathBuf,
}

impl StagingGuard {
    fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn extract_staged(artifact_path: &FsPath, extracted_dir: &FsPath) -> Result<(), AppError> {
    tokio::fs::create_dir_all(extracted_dir)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("create extraction dir: {error}")))?;
    let artifact_path = artifact_path.to_owned();
    let extracted_dir = extracted_dir.to_owned();
    tokio::task::spawn_blocking(move || extract_archive(&artifact_path, &extracted_dir))
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("extraction task failed: {error}")))?
        .map_err(AppError::Validation)?;
    Ok(())
}

/// Extracts a zip or tar.gz artifact into `dest_dir`, sanitizing entry paths
/// (rejecting traversal, absolute paths, symlinks, hard links) and bounding
/// total size / entry count. Returns the number of entries extracted.
fn extract_archive(archive_path: &FsPath, dest_dir: &FsPath) -> Result<usize, String> {
    let mut head = [0_u8; 4];
    let mut file =
        std::fs::File::open(archive_path).map_err(|error| format!("open artifact: {error}"))?;
    use std::io::Read;
    let read = file
        .read(&mut head)
        .map_err(|error| format!("read artifact: {error}"))?;
    if read >= 4 && head[..4] == [0x50, 0x4b, 0x03, 0x04] {
        extract_zip(archive_path, dest_dir)
    } else if read >= 2 && head[..2] == [0x1f, 0x8b] {
        extract_tar_gz(archive_path, dest_dir)
    } else {
        Err("artifact is neither a zip archive nor a gzip-compressed tarball".into())
    }
}

fn extract_zip(archive_path: &FsPath, dest_dir: &FsPath) -> Result<usize, String> {
    let file =
        std::fs::File::open(archive_path).map_err(|error| format!("open zip artifact: {error}"))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|error| format!("invalid zip archive: {error}"))?;
    let mut total_bytes: u64 = 0;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("read zip entry: {error}"))?;
        let name = entry.name().to_owned();
        let is_symlink = entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000);
        if is_symlink {
            return Err(format!(
                "archive entry `{name}` is a symlink, which is not allowed in plugin artifacts"
            ));
        }
        let sanitized = sanitize_archive_path(&name)?;
        if entry.is_dir() {
            std::fs::create_dir_all(dest_dir.join(&sanitized))
                .map_err(|error| format!("create zip directory `{name}`: {error}"))?;
            continue;
        }
        total_bytes = total_bytes
            .checked_add(entry.size())
            .filter(|total| *total <= MAX_ARCHIVE_EXTRACTED_BYTES)
            .ok_or_else(|| "archive exceeds the 512 MiB extraction bound".to_owned())?;
        let full = dest_dir.join(&sanitized);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create zip parent dir for `{name}`: {error}"))?;
        }
        let mut out = std::fs::File::create(&full)
            .map_err(|error| format!("create zip output `{name}`: {error}"))?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|error| format!("write zip entry `{name}`: {error}"))?;
        if index >= MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "archive exceeds the {MAX_ARCHIVE_ENTRIES} entry bound"
            ));
        }
    }
    Ok(archive.len())
}

fn extract_tar_gz(archive_path: &FsPath, dest_dir: &FsPath) -> Result<usize, String> {
    let file = std::fs::File::open(archive_path)
        .map_err(|error| format!("open tarball artifact: {error}"))?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut total_bytes: u64 = 0;
    let mut count = 0usize;
    let entries = archive
        .entries()
        .map_err(|error| format!("invalid tarball: {error}"))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| format!("read tarball entry: {error}"))?;
        let header = entry.header();
        let entry_type = header.entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(
                "tarball contains a link entry, which is not allowed in plugin artifacts".into(),
            );
        }
        let raw_name = entry
            .path()
            .map_err(|error| format!("read tarball entry path: {error}"))?
            .to_string_lossy()
            .into_owned();
        let sanitized = sanitize_archive_path(&raw_name)?;
        if entry_type.is_dir() {
            std::fs::create_dir_all(dest_dir.join(&sanitized))
                .map_err(|error| format!("create tarball directory `{raw_name}`: {error}"))?;
        } else if entry_type.is_file() {
            total_bytes = total_bytes
                .checked_add(entry.size())
                .filter(|total| *total <= MAX_ARCHIVE_EXTRACTED_BYTES)
                .ok_or_else(|| "archive exceeds the 512 MiB extraction bound".to_owned())?;
            let full = dest_dir.join(&sanitized);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).map_err(|error| {
                    format!("create tarball parent dir for `{raw_name}`: {error}")
                })?;
            }
            let mut out = std::fs::File::create(&full)
                .map_err(|error| format!("create tarball output `{raw_name}`: {error}"))?;
            std::io::copy(&mut entry, &mut out)
                .map_err(|error| format!("write tarball entry `{raw_name}`: {error}"))?;
        }
        count += 1;
        if count > MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "archive exceeds the {MAX_ARCHIVE_ENTRIES} entry bound"
            ));
        }
    }
    Ok(count)
}

/// Sanitizes an archive entry path: rejects absolute paths, parent traversal,
/// NUL bytes, and empty paths; normalizes separators.
fn sanitize_archive_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.as_bytes().contains(&0) || name.len() > 4 * 1024 {
        return Err(format!("archive entry `{name}` has an invalid path"));
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(format!(
            "archive entry `{name}` uses an absolute path, which is not allowed"
        ));
    }
    let mut parts = Vec::new();
    for part in name.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                return Err(format!(
                    "archive entry `{name}` escapes the extraction directory"
                ));
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err("archive entry has an empty path".into());
    }
    Ok(PathBuf::from(parts.join("/")))
}

// ---------------------------------------------------------------------------
// Sandbox self-test
// ---------------------------------------------------------------------------

fn require_sandbox(state: &AppState) -> Result<&crate::sandbox_client::SandboxClient, AppError> {
    let sandbox = state.sandbox.as_ref().ok_or_else(|| {
        AppError::Conflict(
            "this plugin declares executable components or a self_test, so installation requires \
             the sandbox, but features.sandbox with sandbox_socket_path and sandbox_auth_token are \
             not configured",
        )
    })?;
    Ok(sandbox)
}

fn map_sandbox_error(error: SandboxClientError) -> AppError {
    match error {
        SandboxClientError::Disconnected => {
            AppError::Conflict("sandbox unavailable — daemon not reachable or not provisioned")
        }
        SandboxClientError::Unauthorized => AppError::Conflict("sandbox authentication failed"),
        SandboxClientError::NotFound => AppError::Conflict("sandbox workspace was not provisioned"),
        SandboxClientError::PolicyDenied => {
            AppError::Conflict("sandbox policy denied the operation")
        }
        SandboxClientError::LimitExceeded => AppError::Conflict("sandbox resource limit exceeded"),
        SandboxClientError::Timeout => AppError::Conflict("sandbox operation timed out"),
        SandboxClientError::ProtocolViolation(_) => {
            AppError::Conflict("sandbox protocol violation")
        }
        SandboxClientError::Internal(_) => AppError::Conflict("sandbox internal error"),
    }
}

/// Runs the plugin self-test inside the sandbox in a plugin-specific
/// workspace (id = plugin id): provisions the workspace, uploads the
/// extracted artifact, starts the `self_test` command, collects bounded
/// output, and verifies the exit code against `expected_exit` (default 0).
async fn run_plugin_self_test(
    state: &AppState,
    plugin_id: Uuid,
    manifest: &PluginManifestV1,
    artifact_dir: &FsPath,
) -> Result<serde_json::Value, AppError> {
    let sandbox = require_sandbox(state)?;
    sandbox.health().await.map_err(|error| {
        AppError::Validation(format!(
            "sandbox unavailable — daemon not reachable or not provisioned: {error}"
        ))
    })?;
    sandbox
        .provision_workspace(plugin_id)
        .await
        .map_err(map_sandbox_error)?;
    upload_artifact_to_sandbox(sandbox, plugin_id, artifact_dir).await?;

    let Some(self_test) = &manifest.self_test else {
        return Ok(serde_json::Value::Null);
    };
    let timeout_seconds = u64::from(self_test.timeout_seconds.max(1));
    let limits = self_test_limits(manifest, timeout_seconds)?;
    let network_policy = manifest
        .sandbox
        .as_ref()
        .and_then(|sandbox| sandbox.network)
        .unwrap_or(NetworkPolicy::Restricted);
    let request = TerminalStartRequest {
        workspace_id: plugin_id,
        command: self_test.command.clone(),
        working_directory: ".".into(),
        cols: 80,
        rows: 24,
        network_policy,
        limits,
    };
    request
        .validate()
        .map_err(|error| AppError::Validation(format!("plugin self_test is invalid: {error}")))?;
    let terminal_id = Uuid::now_v7();
    sandbox
        .terminal_start(terminal_id, request)
        .await
        .map_err(map_sandbox_error)?;

    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.saturating_add(10));
    let mut output = Vec::new();
    let mut cursor = 0_u64;
    match tokio::time::timeout_at(deadline, async {
        loop {
            let read = sandbox
                .terminal_read_output(
                    terminal_id,
                    cursor,
                    MAX_TERMINAL_OUTPUT_READ_BYTES as u32,
                    500,
                )
                .await
                .map_err(map_sandbox_error)?;
            cursor = read.next_cursor;
            output.extend_from_slice(&read.data);
            if output.len() > 64 * 1024 {
                output.truncate(64 * 1024);
            }
            if read.output_complete {
                break;
            }
        }
        Ok::<(), AppError>(())
    })
    .await
    {
        Err(_) => {
            let _ = sandbox.terminal_terminate(terminal_id).await;
            return Err(AppError::Conflict("plugin self-test timed out"));
        }
        Ok(Err(error)) => {
            let _ = sandbox.terminal_terminate(terminal_id).await;
            return Err(error);
        }
        Ok(Ok(())) => {}
    }
    let info = sandbox
        .terminal_reconnect(terminal_id)
        .await
        .map_err(map_sandbox_error)?;
    let _ = sandbox.terminal_terminate(terminal_id).await;
    let expected = self_test.expected_exit.unwrap_or(0);
    if info.exit_code != Some(expected) {
        let tail = String::from_utf8_lossy(&output);
        let tail: String = tail.chars().take(2_000).collect();
        return Err(AppError::Validation(format!(
            "plugin self-test failed: expected exit code {expected}, got {:?}. Output: {tail}",
            info.exit_code
        )));
    }
    Ok(serde_json::json!({
        "exit_code": info.exit_code,
        "output": String::from_utf8_lossy(&output),
    }))
}

/// Uploads every file of the extracted artifact into the sandbox workspace
/// (creating directories as needed), bounding file count and total bytes.
async fn upload_artifact_to_sandbox(
    sandbox: &crate::sandbox_client::SandboxClient,
    workspace_id: Uuid,
    artifact_dir: &FsPath,
) -> Result<(), AppError> {
    let mut created_dirs = HashSet::new();
    let mut pending = vec![PathBuf::new()];
    let mut total_bytes: u64 = 0;
    let mut file_count: u64 = 0;
    while let Some(relative) = pending.pop() {
        let full = artifact_dir.join(&relative);
        let mut entries = tokio::fs::read_dir(&full)
            .await
            .map_err(|error| AppError::Internal(anyhow::anyhow!("read staging dir: {error}")))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| AppError::Internal(anyhow::anyhow!("read staging entry: {error}")))?
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_relative = if relative.as_os_str().is_empty() {
                PathBuf::from(name)
            } else {
                relative.join(name)
            };
            let child_str = child_relative.to_string_lossy().replace('\\', "/");
            validate_workspace_path(&child_str).map_err(|error| {
                AppError::Validation(format!(
                    "plugin artifact contains an invalid workspace path `{child_str}`: {error}"
                ))
            })?;
            let file_type = entry.file_type().await.map_err(|error| {
                AppError::Internal(anyhow::anyhow!("stat staging entry: {error}"))
            })?;
            if file_type.is_dir() {
                if created_dirs.insert(child_str.clone()) {
                    sandbox
                        .fs_mkdir(workspace_id, &child_str)
                        .await
                        .map_err(map_sandbox_error)?;
                }
                pending.push(child_relative);
            } else if file_type.is_file() {
                let data = tokio::fs::read(entry.path()).await.map_err(|error| {
                    AppError::Internal(anyhow::anyhow!("read artifact file: {error}"))
                })?;
                if data.len() > MAX_FILE_PAYLOAD_BYTES {
                    return Err(AppError::Validation(format!(
                        "plugin file `{child_str}` exceeds the sandbox file size limit (256 KiB)"
                    )));
                }
                if let Some(parent) = FsPath::new(&child_str).parent() {
                    let parent_str = parent.to_string_lossy().replace('\\', "/");
                    if !parent_str.is_empty() && created_dirs.insert(parent_str.clone()) {
                        sandbox
                            .fs_mkdir(workspace_id, &parent_str)
                            .await
                            .map_err(map_sandbox_error)?;
                    }
                }
                sandbox
                    .fs_write(workspace_id, &child_str, &data)
                    .await
                    .map_err(map_sandbox_error)?;
                total_bytes = total_bytes.saturating_add(data.len() as u64);
                file_count = file_count.saturating_add(1);
            } else {
                return Err(AppError::Validation(format!(
                    "plugin artifact contains an unsupported file type at `{child_str}`"
                )));
            }
            if file_count > MAX_SANDBOX_UPLOAD_FILES || total_bytes > MAX_SANDBOX_UPLOAD_BYTES {
                return Err(AppError::Validation(
                    "plugin artifact is too large to upload into the sandbox (10000 files / 256 MiB max)"
                        .into(),
                ));
            }
        }
    }
    Ok(())
}

fn self_test_limits(
    manifest: &PluginManifestV1,
    timeout_seconds: u64,
) -> Result<ResourceLimits, AppError> {
    let requested = manifest
        .sandbox
        .as_ref()
        .and_then(|sandbox| sandbox.resource_limits);
    let limits = ResourceLimits {
        cpu_millis: requested
            .and_then(|limits| limits.cpu_millis)
            .unwrap_or(1_000),
        memory_bytes: requested
            .and_then(|limits| limits.memory_bytes)
            .unwrap_or(1024 * 1024 * 1024),
        writable_storage_bytes: requested
            .and_then(|limits| limits.writable_storage_bytes)
            .unwrap_or(1024 * 1024 * 1024),
        pids: requested.and_then(|limits| limits.pids).unwrap_or(256),
        execution_seconds: requested
            .and_then(|limits| limits.execution_seconds)
            .unwrap_or(timeout_seconds.saturating_mul(2).max(60)),
    };
    limits.validate_hard_ceiling().map_err(|error| {
        AppError::Validation(format!("plugin resource limits are invalid: {error}"))
    })?;
    Ok(limits)
}

// ---------------------------------------------------------------------------
// Upgrade/rollback shared pipeline
// ---------------------------------------------------------------------------

/// A freshly staged and verified version of a plugin: manifest, raw manifest
/// JSON (for the optional `signature` field), artifact bytes (read only when a
/// signature is present, for verification), the extracted payload directory
/// (used by the sandbox self-test), and the resolved commit SHA. The staging
/// directory is removed when this value drops.
struct StagedArtifact {
    manifest: PluginManifestV1,
    raw: serde_json::Value,
    artifact_bytes: Option<Vec<u8>>,
    extracted_dir: PathBuf,
    commit_sha: Option<String>,
    needs_sandbox: bool,
    _guard: StagingGuard,
}

/// Re-resolves + re-verifies a version of the plugin source, downloads and
/// extracts the artifact into a fresh staging dir, and performs static policy
/// inspection. Returns the validated manifest plus the staged payload so the
/// caller can run the sandbox self-test and verify an artifact signature.
async fn stage_and_verify(
    state: &AppState,
    row: &PluginRow,
    version: &str,
    expected_digest: &str,
) -> Result<StagedArtifact, AppError> {
    let identity = PluginIdentity {
        source_type: row.source_type.clone(),
        source_uri: row.source_uri.clone(),
        commit_sha: None,
        version: Some(version.to_owned()),
    };
    let resolved = state
        .plugin_source
        .resolve(&identity)
        .await
        .map_err(map_source_error)?;
    if !resolved
        .artifact_digest
        .eq_ignore_ascii_case(expected_digest)
    {
        return Err(AppError::Conflict(
            "artifact digest changed since it was staged; run upgrade again to re-stage",
        ));
    }
    let raw = state
        .plugin_source
        .fetch_manifest(&resolved)
        .await
        .map_err(map_source_error)?;
    let manifest = parse_and_validate_manifest(raw.clone())?;
    if manifest.name != row.name {
        return Err(AppError::Validation(
            "plugin name cannot change between versions".into(),
        ));
    }
    let permissions = Vec::<PluginPermission>::from(&manifest);
    validate_permissions(&permissions)?;
    validate_components(&manifest.components)?;
    let staging = state
        .settings
        .features
        .plugins_dir
        .join("staging")
        .join(Uuid::now_v7().to_string());
    let guard = StagingGuard::new(staging.clone());
    tokio::fs::create_dir_all(&staging)
        .await
        .map_err(|error| AppError::Internal(anyhow::anyhow!("create staging dir: {error}")))?;
    let artifact_path = staging.join("artifact.bin");
    state
        .plugin_source
        .download_artifact(&resolved, &artifact_path)
        .await
        .map_err(map_source_error)?;
    let extracted_dir = staging.join("extracted");
    extract_staged(&artifact_path, &extracted_dir).await?;
    // Read the artifact bytes only when a signature needs verifying.
    let artifact_bytes = if raw.get("signature").is_some() {
        Some(tokio::fs::read(&artifact_path).await.map_err(|error| {
            AppError::Internal(anyhow::anyhow!("read staged artifact: {error}"))
        })?)
    } else {
        None
    };
    let needs_sandbox = manifest
        .components
        .iter()
        .any(|component| component.r#type == PluginComponentType::Executable)
        || manifest.self_test.is_some();
    Ok(StagedArtifact {
        manifest,
        raw,
        artifact_bytes,
        extracted_dir,
        commit_sha: resolved.identity.commit_sha,
        needs_sandbox,
        _guard: guard,
    })
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

fn map_source_error(error: PluginSourceError) -> AppError {
    match error {
        PluginSourceError::NotFound => AppError::NotFound,
        PluginSourceError::RateLimited { .. } => AppError::RateLimited,
        PluginSourceError::Timeout | PluginSourceError::Unavailable => {
            AppError::ServiceUnavailable("plugin source is unavailable or timed out")
        }
        PluginSourceError::InvalidResponse => {
            AppError::Validation("plugin source returned an invalid response".into())
        }
        PluginSourceError::ManifestInvalid(message) => {
            AppError::Validation(format!("invalid plugin manifest: {message}"))
        }
        PluginSourceError::DigestMismatch => AppError::Conflict(
            "artifact digest mismatch — the artifact changed since it was resolved; run preview again",
        ),
        PluginSourceError::Download(_) => {
            AppError::ServiceUnavailable("plugin artifact download failed")
        }
        PluginSourceError::Io(error) => AppError::Internal(anyhow::anyhow!(error)),
    }
}

fn map_marketplace_error(error: MarketplaceError) -> AppError {
    match error {
        MarketplaceError::NotFound => AppError::NotFound,
        MarketplaceError::RateLimited { .. } => AppError::RateLimited,
        MarketplaceError::Unavailable | MarketplaceError::Timeout => {
            AppError::ServiceUnavailable("plugin marketplace is unavailable or timed out")
        }
        MarketplaceError::InvalidResponse => {
            AppError::Validation("plugin marketplace returned an invalid response".into())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn manifest_v1() -> PluginManifestV1 {
        serde_json::from_value(serde_json::json!({
            "manifest_version": 1,
            "name": "demo",
            "version": "1.0.0",
            "description": "Demo plugin",
            "publisher": { "name": "demo-labs" },
            "permissions": {
                "filesystem_read": ["/workspace/**"],
                "secrets": ["GITHUB_TOKEN"]
            },
            "components": [
                { "type": "skill", "name": "run-demo", "ref": "skills/run-demo.md" }
            ]
        }))
        .unwrap()
    }

    fn permission(domain: &str, scope_value: &str) -> PermissionPreview {
        PermissionPreview {
            domain: domain.into(),
            scope_value: scope_value.into(),
        }
    }

    #[test]
    fn permission_diff_reports_added_and_removed() {
        let current = vec![
            permission("filesystem_read", "/workspace/**"),
            permission("secrets", "GITHUB_TOKEN"),
        ];
        let next = vec![
            permission("filesystem_read", "/workspace/**"),
            permission("network", "api.example.com:443"),
        ];
        let (added, removed) = compute_permission_diff(&current, &next);
        assert_eq!(added, vec![permission("network", "api.example.com:443")]);
        assert_eq!(removed, vec![permission("secrets", "GITHUB_TOKEN")]);
    }

    #[test]
    fn name_diff_reports_added_and_removed() {
        let current = vec!["a".to_owned(), "b".to_owned()];
        let next = vec!["b".to_owned(), "c".to_owned()];
        let (added, removed) = compute_name_diff(&current, &next);
        assert_eq!(added, vec!["c".to_owned()]);
        assert_eq!(removed, vec!["a".to_owned()]);
    }

    #[test]
    fn signature_is_never_self_asserted() {
        // A manifest-declared digest of the artifact is NOT a signature: the
        // publisher controls both values. VERIFIED is reserved for signatures
        // verified against a known publisher key (none exist yet), so the
        // verification always returns false and installs stay UNTRUSTED or
        // USER_PROVIDED.
        let artifact = b"artifact-bytes";
        assert!(!verify_artifact_signature(
            &serde_json::json!("deadbeef"),
            artifact
        ));
        assert!(!verify_artifact_signature(
            &serde_json::Value::Null,
            artifact
        ));
        assert!(!verify_artifact_signature(
            &serde_json::json!("not-hex"),
            artifact
        ));
    }
    #[test]
    fn permission_validation_rejects_unknown_domains_and_oversized_scopes() {
        let known = PluginPermission {
            domain: "filesystem_read".into(),
            scope_value: "/workspace/**".into(),
        };
        assert!(validate_permissions(&[known]).is_ok());
        let unknown = PluginPermission {
            domain: "everything".into(),
            scope_value: "/".into(),
        };
        assert!(validate_permissions(&[unknown]).is_err());
        let oversized = PluginPermission {
            domain: "network".into(),
            scope_value: "x".repeat(513),
        };
        assert!(validate_permissions(&[oversized]).is_err());
        let empty = PluginPermission {
            domain: "network".into(),
            scope_value: String::new(),
        };
        assert!(validate_permissions(&[empty]).is_err());
    }

    #[test]
    fn component_type_maps_to_db_values() {
        assert_eq!(component_type_db(PluginComponentType::Skill), "skill");
        assert_eq!(
            component_type_db(PluginComponentType::McpServer),
            "mcp_server"
        );
        assert_eq!(
            component_type_db(PluginComponentType::SourceBook),
            "source_book"
        );
        assert_eq!(
            component_type_db(PluginComponentType::Executable),
            "executable"
        );
        assert_eq!(component_type_db(PluginComponentType::Asset), "asset");
        assert_eq!(component_type_db(PluginComponentType::Schema), "schema");
    }

    #[test]
    fn sha256_hex_validation_accepts_only_full_hex() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"a".repeat(63)));
        assert!(!is_sha256_hex(&"g".repeat(64)));
        assert!(!is_sha256_hex(""));
    }

    #[test]
    fn manifest_sandbox_defaults_to_restricted_network() {
        let (network_policy, _, resource_limits) = manifest_sandbox(&manifest_v1());
        assert_eq!(network_policy, "RESTRICTED");
        assert!(resource_limits.is_none());
    }

    #[test]
    fn sanitize_archive_path_normalizes_and_rejects_traversal() {
        assert_eq!(
            sanitize_archive_path("skills/run.md").unwrap(),
            PathBuf::from("skills/run.md")
        );
        assert_eq!(
            sanitize_archive_path("a\\b.txt").unwrap(),
            PathBuf::from("a/b.txt")
        );
        assert!(sanitize_archive_path("../evil").is_err());
        assert!(sanitize_archive_path("/etc/passwd").is_err());
        assert!(sanitize_archive_path("").is_err());
        assert!(sanitize_archive_path("a\0b").is_err());
    }

    #[test]
    fn extract_zip_round_trip_and_traversal_rejection() {
        let dir = std::env::temp_dir().join(format!("gobrowse-plugin-api-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("plugin.zip");
        let dest = dir.join("out");
        let file = std::fs::File::create(&archive).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.start_file("skills/run.md", options).unwrap();
        writer.write_all(b"# run").unwrap();
        writer.start_file("docs/readme.md", options).unwrap();
        writer.write_all(b"readme").unwrap();
        writer.finish().unwrap();
        let count = extract_archive(&archive, &dest).unwrap();
        assert_eq!(count, 2);
        assert_eq!(
            std::fs::read_to_string(dest.join("skills/run.md")).unwrap(),
            "# run"
        );
        assert_eq!(
            std::fs::read_to_string(dest.join("docs/readme.md")).unwrap(),
            "readme"
        );

        // Traversal entries must be rejected.
        let evil = dir.join("evil.zip");
        let file = std::fs::File::create(&evil).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer.start_file("../evil.txt", options).unwrap();
        writer.write_all(b"pwn").unwrap();
        writer.finish().unwrap();
        assert!(extract_archive(&evil, &dest).is_err());
        assert!(!dir.join("..").join("evil.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_tar_gz_round_trip_and_rejects_links() {
        let dir = std::env::temp_dir().join(format!("gobrowse-plugin-api-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive = dir.join("plugin.tar.gz");
        let dest = dir.join("out");
        let file = std::fs::File::create(&archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let contents = b"payload";
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "bin/run", &contents[..])
            .unwrap();
        builder.finish().unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        let count = extract_archive(&archive, &dest).unwrap();
        assert_eq!(count, 1);
        assert_eq!(std::fs::read(dest.join("bin/run")).unwrap(), contents);

        // A symlink entry must be rejected.
        let link_archive = dir.join("link.tar.gz");
        let file = std::fs::File::create(&link_archive).unwrap();
        let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o777);
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_cksum();
        builder.append_data(&mut header, "link", &b""[..]).unwrap();
        builder.finish().unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        assert!(extract_archive(&link_archive, &dest).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn map_source_error_maps_taxonomy() {
        assert!(matches!(
            map_source_error(PluginSourceError::NotFound),
            AppError::NotFound
        ));
        assert!(matches!(
            map_source_error(PluginSourceError::RateLimited {
                retry_after_seconds: None
            }),
            AppError::RateLimited
        ));
        assert!(matches!(
            map_source_error(PluginSourceError::Timeout),
            AppError::ServiceUnavailable(_)
        ));
        assert!(matches!(
            map_source_error(PluginSourceError::ManifestInvalid("bad".into())),
            AppError::Validation(_)
        ));
        assert!(matches!(
            map_source_error(PluginSourceError::DigestMismatch),
            AppError::Conflict(_)
        ));
    }
}
