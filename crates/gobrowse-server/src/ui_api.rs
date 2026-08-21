//! UI package lifecycle: preview → install → activate/rollback/delete.
//! Simplified from Plugin pattern but with THEME/FULL_UI distinction.

use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::{
    ActiveUiState, AppState, UiAsset, UiKind,
    auth::{audit, require_user, require_user_or_run},
    error::AppError,
};

// ---------------------------------------------------------------------------
// Manifest validation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct UiManifest {
    pub kind: String,
    pub manifest_version: i32,
    pub name: String,
    pub version: String,
    pub ui_kind: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
    #[serde(default)]
    pub min_api_version: Option<String>,
    #[serde(default)]
    pub entry: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    #[serde(default)]
    pub theme: Option<UiTheme>,
    #[serde(default)]
    pub publisher: Option<Value>,
    #[serde(default)]
    pub license: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UiTheme {
    pub variables: std::collections::HashMap<String, String>,
}

fn validate_manifest(raw: Value) -> Result<UiManifest, AppError> {
    let m: UiManifest = serde_json::from_value(raw.clone())
        .map_err(|e| AppError::Validation(format!("manifest parse error: {e}")))?;
    if m.kind != "gobrowse-ui" {
        return Err(AppError::Validation(
            "manifest kind must be \"gobrowse-ui\"".into(),
        ));
    }
    if m.manifest_version != 1 {
        return Err(AppError::Validation("manifest_version must be 1".into()));
    }
    let name = m.name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(AppError::Validation("name must be 1-200 chars".into()));
    }
    let ver = m.version.trim();
    if ver.is_empty() || ver.len() > 64 {
        return Err(AppError::Validation("version must be 1-64 chars".into()));
    }
    if m.ui_kind != "THEME" && m.ui_kind != "FULL_UI" {
        return Err(AppError::Validation(
            "ui_kind must be THEME or FULL_UI".into(),
        ));
    }
    if m.ui_kind == "FULL_UI"
        && m.entry
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    {
        return Err(AppError::Validation("FULL_UI requires entry".into()));
    }
    if let Some(theme) = &m.theme {
        if theme.variables.is_empty() {
            return Err(AppError::Validation(
                "theme.variables must not be empty".into(),
            ));
        }
        for (k, v) in &theme.variables {
            if !k.starts_with("--") {
                return Err(AppError::Validation(format!(
                    "theme variable {k} must start with --"
                )));
            }
            if v.trim().is_empty() || v.len() > 200 {
                return Err(AppError::Validation(format!(
                    "theme variable {k} value invalid"
                )));
            }
        }
    }
    Ok(m)
}

fn compute_digest(value: &Value) -> String {
    let canonical = serde_json::to_string(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let result = hasher.finalize();
    result.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn generate_theme_css(vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::from(":root {\n");
    let mut keys: Vec<_> = vars.keys().collect();
    keys.sort();
    for k in keys {
        let v = &vars[k];
        out.push_str(&format!("  {k}: {v};\n"));
    }
    out.push_str("}\n");
    out
}

fn asset_content_type(path: &str) -> &'static str {
    if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".html") {
        "text/html"
    } else {
        "application/octet-stream"
    }
}

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreviewRequest {
    pub source_type: String,
    pub source_uri: String,
    #[serde(default)]
    pub manifest: Option<Value>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewResponse {
    pub digest: String,
    pub manifest: Value,
    pub ui_kind: String,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    pub source_type: String,
    pub source_uri: String,
    pub expected_digest: String,
    pub approve: bool,
    #[serde(default)]
    pub manifest: Option<Value>,
    #[serde(default)]
    pub workspace_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct InstallResponse {
    pub id: Uuid,
    pub state: String,
    pub name: String,
    pub version: String,
}

#[derive(Debug, Serialize)]
pub struct UiPackageListItem {
    pub id: Uuid,
    pub name: String,
    pub version: String,
    pub ui_kind: String,
    pub state: String,
    pub trust: String,
    pub source_type: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Serialize)]
pub struct UiPackageDetail {
    pub id: Uuid,
    pub profile_id: Uuid,
    pub name: String,
    pub version: String,
    pub ui_kind: String,
    pub description: String,
    pub source_type: String,
    pub source_uri: String,
    pub state: String,
    pub trust: String,
    pub manifest: Value,
    pub artifact_digest: Option<String>,
    pub install_path: Option<String>,
    pub entry_point: Option<String>,
    pub assets: Vec<AssetRow>,
    pub capabilities: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AssetRow {
    pub file_path: String,
    pub content_type: String,
    pub sha256_hash: String,
    pub file_size: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    pub trust: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ActivateResponse {
    pub id: Uuid,
    pub state: String,
    pub previous_id: Option<Uuid>,
}

// ---------------------------------------------------------------------------
// Helpers — manifest extraction
// ---------------------------------------------------------------------------

fn extract_manifest_from_request(req: &PreviewRequest) -> Result<Value, AppError> {
    if let Some(m) = &req.manifest {
        return Ok(m.clone());
    }
    let uri = req.source_uri.trim();
    if uri.starts_with('{') {
        let v: Value = serde_json::from_str(uri)
            .map_err(|e| AppError::Validation(format!("source_uri JSON parse: {e}")))?;
        return Ok(v);
    }
    if let Some(rest) = uri.strip_prefix("inline:") {
        let v: Value = serde_json::from_str(rest)
            .map_err(|e| AppError::Validation(format!("inline manifest parse: {e}")))?;
        return Ok(v);
    }
    if std::path::Path::new(uri).exists() {
        let content = std::fs::read_to_string(uri)
            .map_err(|e| AppError::Validation(format!("read source_uri file: {e}")))?;
        let v: Value = serde_json::from_str(&content)
            .map_err(|e| AppError::Validation(format!("file JSON parse: {e}")))?;
        return Ok(v);
    }
    let candidate = std::path::Path::new(uri).join("manifest.json");
    if candidate.exists() {
        let content = std::fs::read_to_string(&candidate)
            .map_err(|e| AppError::Validation(format!("read manifest.json: {e}")))?;
        let v: Value = serde_json::from_str(&content)
            .map_err(|e| AppError::Validation(format!("manifest.json parse: {e}")))?;
        return Ok(v);
    }
    Err(AppError::Validation(
        "no manifest provided and source_uri is not readable JSON or file".into(),
    ))
}

fn resolve_manifest_for_install(req: &InstallRequest) -> Result<Value, AppError> {
    if let Some(m) = &req.manifest {
        return Ok(m.clone());
    }
    let uri = req.source_uri.trim();
    if uri.starts_with('{') {
        let v: Value = serde_json::from_str(uri)
            .map_err(|e| AppError::Validation(format!("source_uri JSON parse: {e}")))?;
        return Ok(v);
    }
    if let Some(rest) = uri.strip_prefix("inline:") {
        let v: Value = serde_json::from_str(rest)
            .map_err(|e| AppError::Validation(format!("inline manifest parse: {e}")))?;
        return Ok(v);
    }
    if std::path::Path::new(uri).exists() {
        let candidate = std::path::Path::new(uri).join("manifest.json");
        let path = if candidate.exists() {
            candidate
        } else {
            std::path::PathBuf::from(uri)
        };
        let content = std::fs::read_to_string(&path)
            .map_err(|e| AppError::Validation(format!("read file: {e}")))?;
        let v: Value = serde_json::from_str(&content)
            .map_err(|e| AppError::Validation(format!("file JSON parse: {e}")))?;
        return Ok(v);
    }
    Err(AppError::Validation(
        "no manifest provided for install".into(),
    ))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<PreviewRequest>,
) -> Result<Json<PreviewResponse>, AppError> {
    let _user = require_user(&state, &headers).await?;
    if input.source_type.trim().is_empty() {
        return Err(AppError::Validation("source_type required".into()));
    }
    if !matches!(
        input.source_type.as_str(),
        "github_release" | "local_package" | "marketplace"
    ) {
        return Err(AppError::Validation(format!(
            "unsupported source_type {}",
            input.source_type
        )));
    }
    let raw = extract_manifest_from_request(&input)?;
    let manifest = validate_manifest(raw.clone())?;
    let digest = compute_digest(&raw);
    let mut tx = state.pool.begin().await?;
    audit(
        &mut tx,
        Some(_user.id),
        Some(_user.profile_id),
        "ui_preview",
        "ui_package",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(PreviewResponse {
        digest,
        manifest: raw,
        ui_kind: manifest.ui_kind,
        capabilities: manifest.capabilities.unwrap_or_default(),
    }))
}

pub async fn install(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<InstallRequest>,
) -> Result<Json<InstallResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    if !is_sha256_hex(&input.expected_digest) {
        return Err(AppError::Validation(
            "expected_digest must be 64 hex chars".into(),
        ));
    }
    if !input.approve {
        return Err(AppError::Validation(
            "installation requires explicit approval (approve: true)".into(),
        ));
    }
    if !matches!(
        input.source_type.as_str(),
        "github_release" | "local_package" | "marketplace"
    ) {
        return Err(AppError::Validation(format!(
            "unsupported source_type {}",
            input.source_type
        )));
    }
    if let Some(ws) = input.workspace_id {
        let member: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workspace_memberships WHERE workspace_id=$1 AND user_id=$2)",
        )
        .bind(ws)
        .bind(user.id)
        .fetch_one(&state.pool)
        .await?;
        if !member && !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
            return Err(AppError::Forbidden);
        }
    }
    let raw = resolve_manifest_for_install(&input)?;
    let computed = compute_digest(&raw);
    if computed.to_lowercase() != input.expected_digest.to_lowercase() {
        return Err(AppError::Conflict(
            "artifact digest does not match the digest from preview — the artifact changed between preview and install; run preview again and retry with the fresh digest",
        ));
    }
    let manifest = validate_manifest(raw.clone())?;
    if let Some(min) = &manifest.min_api_version
        && min != "v1"
    {
        return Err(AppError::Validation(format!(
            "unsupported min_api_version {min}"
        )));
    }
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM ui_packages WHERE profile_id=$1 AND name=$2)",
    )
    .bind(user.profile_id)
    .bind(manifest.name.trim())
    .fetch_one(&state.pool)
    .await?;
    if exists {
        return Err(AppError::Conflict(
            "a UI package with this name is already installed for this profile",
        ));
    }
    let pkg_id = Uuid::now_v7();
    let ui_kind = manifest.ui_kind.clone();
    let name = manifest.name.trim().to_string();
    let version = manifest.version.trim().to_string();
    let description = manifest.description.clone().unwrap_or_default();
    let source_uri = input.source_uri.clone();
    let api_version = manifest
        .api_version
        .clone()
        .unwrap_or_else(|| "v1".to_string());
    let install_path = state
        .settings
        .features
        .ui_packages_dir
        .join(pkg_id.to_string());
    tokio::fs::create_dir_all(&install_path)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("create install dir: {e}")))?;
    let mut assets: Vec<(String, String, String)> = Vec::new();
    if ui_kind == "THEME" {
        let vars = manifest
            .theme
            .as_ref()
            .map(|t| t.variables.clone())
            .unwrap_or_default();
        let css = generate_theme_css(&vars);
        let hash = hex_encode(Sha256::digest(css.as_bytes()).as_slice());
        let file_path = "theme.css".to_string();
        tokio::fs::write(install_path.join(&file_path), css.as_bytes())
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("write theme.css: {e}")))?;
        assets.push((
            file_path.clone(),
            asset_content_type(&file_path).to_string(),
            hash,
        ));
        tokio::fs::write(
            install_path.join("manifest.json"),
            serde_json::to_string_pretty(&raw).unwrap().as_bytes(),
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write manifest: {e}")))?;
    } else {
        let entry = manifest
            .entry
            .clone()
            .unwrap_or_else(|| "index.html".to_string());
        let html_content = format!(
            "<!doctype html><html><head><title>{}</title></head><body><h1>{} v{}</h1><script src=\"app.js\"></script></body></html>",
            name, name, version
        );
        tokio::fs::write(install_path.join(&entry), html_content.as_bytes())
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("write entry: {e}")))?;
        let html_hash = hex_encode(Sha256::digest(html_content.as_bytes()).as_slice());
        assets.push((entry.clone(), "text/html".to_string(), html_hash));
        let js_content = format!("// {} v{} ui package", name, version);
        tokio::fs::write(install_path.join("app.js"), js_content.as_bytes())
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("write app.js: {e}")))?;
        let js_hash = hex_encode(Sha256::digest(js_content.as_bytes()).as_slice());
        assets.push((
            "app.js".to_string(),
            "application/javascript".to_string(),
            js_hash,
        ));
        tokio::fs::write(
            install_path.join("manifest.json"),
            serde_json::to_string_pretty(&raw).unwrap().as_bytes(),
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("write manifest: {e}")))?;
    }
    let entry_point = if ui_kind == "THEME" {
        None
    } else {
        manifest.entry.clone()
    };
    let mut tx = state.pool.begin().await?;
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO ui_packages (id, profile_id, workspace_id, name, version, ui_kind, description, source_type, source_uri, manifest, artifact_digest, install_path, entry_point, api_version, state, trust, created_by, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'candidate','UNTRUSTED',$15,$16,$16)",
    )
    .bind(pkg_id)
    .bind(user.profile_id)
    .bind(input.workspace_id)
    .bind(&name)
    .bind(&version)
    .bind(&ui_kind)
    .bind(&description)
    .bind(&input.source_type)
    .bind(&source_uri)
    .bind(&raw)
    .bind(&computed)
    .bind(install_path.to_string_lossy().to_string())
    .bind(&entry_point)
    .bind(&api_version)
    .bind(user.id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    for (file_path, content_type, hash) in &assets {
        let size: i64 = tokio::fs::metadata(install_path.join(file_path))
            .await
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        sqlx::query("INSERT INTO ui_package_assets (id, ui_package_id, file_path, content_type, sha256_hash, file_size) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(Uuid::now_v7()).bind(pkg_id).bind(file_path).bind(content_type).bind(hash).bind(size)
            .execute(&mut *tx).await?;
    }
    let caps = manifest.capabilities.clone().unwrap_or_default();
    for cap in &caps {
        sqlx::query("INSERT INTO ui_package_capabilities (id, ui_package_id, capability_name) VALUES ($1,$2,$3)")
            .bind(Uuid::now_v7()).bind(pkg_id).bind(cap)
            .execute(&mut *tx).await?;
    }
    insert_ui_book(&mut tx, &user, pkg_id, input.workspace_id, &manifest).await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "ui_install",
        "ui_package",
        Some(pkg_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(Json(InstallResponse {
        id: pkg_id,
        state: "candidate".to_string(),
        name,
        version,
    }))
}

async fn insert_ui_book(
    tx: &mut Transaction<'_, Postgres>,
    user: &crate::auth::AuthenticatedUser,
    pkg_id: Uuid,
    workspace_id: Option<Uuid>,
    manifest: &UiManifest,
) -> Result<Uuid, AppError> {
    let book_id = Uuid::now_v7();
    let caps = manifest.capabilities.clone().unwrap_or_default();
    let metadata = serde_json::json!({
        "ui_package_id": pkg_id,
        "capabilities": caps,
        "version": manifest.version,
        "ui_kind": manifest.ui_kind,
    });
    let scope = if workspace_id.is_some() {
        "WORKSPACE"
    } else {
        "PROFILE"
    };
    let now = OffsetDateTime::now_utc();
    sqlx::query(
        "INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust, source, author, workspace_id, security_classification, metadata, kind, owner_user_id, created_by_user_id, created_at, updated_at) \
         VALUES ($1,$2,$3,$4,'INSTRUCTION',$5,'{}'::text[],'SYSTEM','UNTRUSTED','{}'::jsonb,'system',$6,'INTERNAL',$7,'GOBROWSE_UI',$8,$9,$10,$10)",
    )
    .bind(book_id)
    .bind(user.profile_id)
    .bind(&manifest.name)
    .bind(manifest.description.clone().unwrap_or_else(|| format!("UI package {} v{}", manifest.name, manifest.version)))
    .bind(scope)
    .bind(workspace_id)
    .bind(&metadata)
    .bind(Some(user.id))
    .bind(user.id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO book_revisions (id, book_id, revision, title, body, tags, metadata, changed_by, change_reason) VALUES ($1,$2,1,$3,$4,'{}'::text[],$5,$6,'Installed UI package')",
    )
    .bind(Uuid::now_v7())
    .bind(book_id)
    .bind(&manifest.name)
    .bind(manifest.description.clone().unwrap_or_default())
    .bind(&metadata)
    .bind(user.id)
    .execute(&mut **tx)
    .await?;
    crate::embedding::enqueue_book(tx, user.profile_id, book_id, 1).await?;
    Ok(book_id)
}

pub async fn list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<UiPackageListItem>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query("SELECT id, name, version, ui_kind, state, trust, source_type, created_at FROM ui_packages WHERE profile_id=$1 ORDER BY updated_at DESC")
        .bind(user.profile_id).fetch_all(&state.pool).await?;
    let items = rows
        .into_iter()
        .map(|r| UiPackageListItem {
            id: r.get("id"),
            name: r.get("name"),
            version: r.get("version"),
            ui_kind: r.get("ui_kind"),
            state: r.get("state"),
            trust: r.get("trust"),
            source_type: r.get("source_type"),
            created_at: r.get("created_at"),
        })
        .collect();
    Ok(Json(items))
}

pub async fn get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<UiPackageDetail>, AppError> {
    let user = require_user(&state, &headers).await?;
    let row = sqlx::query("SELECT id, profile_id, name, version, ui_kind, description, source_type, source_uri, state, trust, manifest, artifact_digest, install_path, entry_point FROM ui_packages WHERE id=$1 AND profile_id=$2")
        .bind(id).bind(user.profile_id).fetch_optional(&state.pool).await?
        .ok_or(AppError::NotFound)?;
    let assets_rows = sqlx::query("SELECT file_path, content_type, sha256_hash, file_size FROM ui_package_assets WHERE ui_package_id=$1")
        .bind(id).fetch_all(&state.pool).await?;
    let assets = assets_rows
        .into_iter()
        .map(|r| AssetRow {
            file_path: r.get("file_path"),
            content_type: r.get("content_type"),
            sha256_hash: r.get("sha256_hash"),
            file_size: r.get("file_size"),
        })
        .collect::<Vec<_>>();
    let caps_rows =
        sqlx::query("SELECT capability_name FROM ui_package_capabilities WHERE ui_package_id=$1")
            .bind(id)
            .fetch_all(&state.pool)
            .await?;
    let caps = caps_rows
        .into_iter()
        .map(|r| r.get::<String, _>("capability_name"))
        .collect::<Vec<_>>();
    Ok(Json(UiPackageDetail {
        id: row.get("id"),
        profile_id: row.get("profile_id"),
        name: row.get("name"),
        version: row.get("version"),
        ui_kind: row.get("ui_kind"),
        description: row.get("description"),
        source_type: row.get("source_type"),
        source_uri: row.get("source_uri"),
        state: row.get("state"),
        trust: row.get("trust"),
        manifest: row.get("manifest"),
        artifact_digest: row.get("artifact_digest"),
        install_path: row.get("install_path"),
        entry_point: row.get("entry_point"),
        assets,
        capabilities: caps,
    }))
}

pub async fn patch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(input): Json<PatchRequest>,
) -> Result<Json<UiPackageDetail>, AppError> {
    let user = require_user(&state, &headers).await?;
    if let Some(trust) = &input.trust
        && !matches!(
            trust.as_str(),
            "VERIFIED" | "USER_PROVIDED" | "AGENT_INFERRED" | "EXTERNAL" | "UNTRUSTED"
        )
    {
        return Err(AppError::Validation("invalid trust value".into()));
    }
    let mut tx = state.pool.begin().await?;
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM ui_packages WHERE id=$1 AND profile_id=$2 FOR UPDATE")
            .bind(id)
            .bind(user.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
    if exists.is_none() {
        return Err(AppError::NotFound);
    }
    if let Some(trust) = input.trust {
        sqlx::query("UPDATE ui_packages SET trust=$1, updated_at=now() WHERE id=$2")
            .bind(trust)
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    get(State(state), headers, Path(id)).await
}

pub async fn activate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ActivateResponse>, AppError> {
    let user = require_user_or_run(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(user.profile_id.to_string())
        .execute(&mut *tx)
        .await?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT state, ui_kind FROM ui_packages WHERE id=$1 AND profile_id=$2 FOR UPDATE",
    )
    .bind(id)
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (state_str, _ui_kind) = row.ok_or(AppError::NotFound)?;
    if !matches!(
        state_str.as_str(),
        "candidate" | "validated" | "staged" | "previous" | "failed"
    ) {
        return Err(AppError::Validation(format!(
            "cannot activate from state {state_str}"
        )));
    }
    let perm: Option<String> =
        sqlx::query_scalar("SELECT ui_permission_level FROM profiles WHERE id=$1")
            .bind(user.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
    let perm = perm.unwrap_or_else(|| "ASK".to_string());
    if perm == "DENY" && !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        return Err(AppError::Forbidden);
    }
    let current_active: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ui_packages WHERE profile_id=$1 AND state='active' FOR UPDATE",
    )
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    // Sweep stale rollback anchors into rolled_back BEFORE demoting the
    // current active, so the just-demoted package remains the single
    // 'previous' rollback anchor. (Sweeping after demotion would destroy
    // the anchor and make rollback unfindable.)
    sqlx::query("UPDATE ui_packages SET state='rolled_back', updated_at=now() WHERE profile_id=$1 AND state='previous'")
        .bind(user.profile_id)
        .execute(&mut *tx)
        .await?;
    if let Some(cur) = current_active {
        if cur == id {
            return Err(AppError::Validation("already active".into()));
        }
        sqlx::query("UPDATE ui_packages SET state='previous', updated_at=now() WHERE id=$1")
            .bind(cur)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE ui_packages SET state='active', activated_at=now(), activated_by=$1, updated_at=now() WHERE id=$2")
        .bind(user.id)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let manifest: Value = sqlx::query_scalar("SELECT manifest FROM ui_packages WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let ui_kind_str: String = sqlx::query_scalar("SELECT ui_kind FROM ui_packages WHERE id=$1")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    let ui_kind = ui_kind_str.parse().unwrap_or(UiKind::Theme);
    let install_path: Option<String> =
        sqlx::query_scalar("SELECT install_path FROM ui_packages WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let entry_point: Option<String> =
        sqlx::query_scalar("SELECT entry_point FROM ui_packages WHERE id=$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let assets_rows = sqlx::query(
        "SELECT file_path, content_type, sha256_hash FROM ui_package_assets WHERE ui_package_id=$1",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    let assets = assets_rows
        .into_iter()
        .map(|r| UiAsset {
            file_path: r.get("file_path"),
            content_type: r.get("content_type"),
            sha256_hash: r.get("sha256_hash"),
        })
        .collect::<Vec<_>>();
    let theme_vars = if ui_kind == UiKind::Theme {
        manifest
            .get("theme")
            .and_then(|t| t.get("variables"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    } else {
        None
    };
    let new_active = ActiveUiState {
        package_id: id,
        ui_kind,
        install_path: install_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| state.settings.features.ui_packages_dir.join(id.to_string())),
        entry_point: entry_point.unwrap_or_else(|| "index.html".to_string()),
        assets,
        theme_variables: theme_vars,
    };
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "ui_activate",
        "ui_package",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    *state.active_ui.write().await = Some(new_active);
    Ok(Json(ActivateResponse {
        id,
        state: "active".to_string(),
        previous_id: current_active,
    }))
}

pub async fn approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ActivateResponse>, AppError> {
    activate(State(state), headers, Path(id)).await
}

pub async fn rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ActivateResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(user.profile_id.to_string())
        .execute(&mut *tx)
        .await?;
    let previous: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ui_packages WHERE profile_id=$1 AND state='previous' FOR UPDATE",
    )
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    let prev_id = previous.ok_or(AppError::NotFound)?;
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM ui_packages WHERE profile_id=$1 AND state='active' FOR UPDATE",
    )
    .bind(user.profile_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(cur) = current {
        sqlx::query("UPDATE ui_packages SET state='rolled_back', updated_at=now() WHERE id=$1")
            .bind(cur)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("UPDATE ui_packages SET state='active', activated_at=now(), activated_by=$1, updated_at=now() WHERE id=$2")
        .bind(user.id)
        .bind(prev_id)
        .execute(&mut *tx)
        .await?;
    let ui_kind_str: String = sqlx::query_scalar("SELECT ui_kind FROM ui_packages WHERE id=$1")
        .bind(prev_id)
        .fetch_one(&mut *tx)
        .await?;
    let ui_kind = ui_kind_str.parse().unwrap_or(UiKind::Theme);
    let manifest: Value = sqlx::query_scalar("SELECT manifest FROM ui_packages WHERE id=$1")
        .bind(prev_id)
        .fetch_one(&mut *tx)
        .await?;
    let install_path: Option<String> =
        sqlx::query_scalar("SELECT install_path FROM ui_packages WHERE id=$1")
            .bind(prev_id)
            .fetch_one(&mut *tx)
            .await?;
    let entry_point: Option<String> =
        sqlx::query_scalar("SELECT entry_point FROM ui_packages WHERE id=$1")
            .bind(prev_id)
            .fetch_one(&mut *tx)
            .await?;
    let assets_rows = sqlx::query(
        "SELECT file_path, content_type, sha256_hash FROM ui_package_assets WHERE ui_package_id=$1",
    )
    .bind(prev_id)
    .fetch_all(&mut *tx)
    .await?;
    let assets = assets_rows
        .into_iter()
        .map(|r| UiAsset {
            file_path: r.get("file_path"),
            content_type: r.get("content_type"),
            sha256_hash: r.get("sha256_hash"),
        })
        .collect::<Vec<_>>();
    let theme_vars = if ui_kind == UiKind::Theme {
        manifest
            .get("theme")
            .and_then(|t| t.get("variables"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
    } else {
        None
    };
    let new_active = ActiveUiState {
        package_id: prev_id,
        ui_kind,
        install_path: install_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                state
                    .settings
                    .features
                    .ui_packages_dir
                    .join(prev_id.to_string())
            }),
        entry_point: entry_point.unwrap_or_else(|| "index.html".to_string()),
        assets,
        theme_variables: theme_vars,
    };
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "ui_rollback",
        "ui_package",
        Some(prev_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    *state.active_ui.write().await = Some(new_active);
    Ok(Json(ActivateResponse {
        id: prev_id,
        state: "active".to_string(),
        previous_id: current,
    }))
}

pub async fn rollback_by_id(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<ActivateResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1::text))")
        .bind(user.profile_id.to_string())
        .execute(&mut *tx)
        .await?;
    let state_str: Option<String> =
        sqlx::query_scalar("SELECT state FROM ui_packages WHERE id=$1 AND profile_id=$2")
            .bind(id)
            .bind(user.profile_id)
            .fetch_optional(&mut *tx)
            .await?;
    let s = state_str.ok_or(AppError::NotFound)?;
    drop(tx);
    if s == "active" {
        return rollback(State(state), headers).await;
    } else if s == "previous" {
        return activate(State(state), headers, Path(id)).await;
    }
    Err(AppError::Validation(format!(
        "cannot rollback package in state {s}"
    )))
}

pub async fn delete_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let row: Option<(String, Uuid)> =
        sqlx::query_as("SELECT state, profile_id FROM ui_packages WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let (state_str, profile_id) = row.ok_or(AppError::NotFound)?;
    if profile_id != user.profile_id && !matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        return Err(AppError::Forbidden);
    }
    if state_str == "active" {
        return Err(AppError::Validation(
            "cannot delete active UI package — rollback or activate another first".into(),
        ));
    }
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ui_packages WHERE profile_id=$1 AND id <> $2 AND state IN ('active','previous','candidate','validated','staged')",
    )
    .bind(user.profile_id)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if remaining == 0 {
        let total_remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ui_packages WHERE profile_id=$1 AND id <> $2")
                .bind(user.profile_id)
                .bind(id)
                .fetch_one(&mut *tx)
                .await?;
        if total_remaining == 0 {
            return Err(AppError::Validation(
                "cannot delete the last UI package — at least one must remain as last-known-good"
                    .into(),
            ));
        }
    }
    sqlx::query(
        "DELETE FROM books WHERE kind='GOBROWSE_UI' AND metadata->>'ui_package_id' = $1::text AND profile_id=$2",
    )
    .bind(id)
    .bind(user.profile_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM ui_packages WHERE id=$1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "ui_delete",
        "ui_package",
        Some(id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    let dir = state.settings.features.ui_packages_dir.join(id.to_string());
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn theme_css(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Response, AppError> {
    let row = sqlx::query("SELECT manifest, ui_kind FROM ui_packages WHERE id=$1")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    let manifest: Value = row.get("manifest");
    let ui_kind: String = row.get("ui_kind");
    if ui_kind != "THEME" {
        return Err(AppError::Validation("package is not a THEME".into()));
    }
    let vars: Option<std::collections::HashMap<String, String>> = manifest
        .get("theme")
        .and_then(|t| t.get("variables"))
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let css = if let Some(vars) = vars {
        generate_theme_css(&vars)
    } else {
        ":root {}\n".to_string()
    };
    let mut resp = css.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/css"),
    );
    Ok(resp)
}

pub async fn active_theme_css(State(state): State<AppState>) -> Result<Response, AppError> {
    let guard = state.active_ui.read().await;
    let Some(active) = guard.as_ref() else {
        let css = ":root {}\n";
        let mut resp = css.into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/css"),
        );
        return Ok(resp);
    };
    if active.ui_kind != UiKind::Theme {
        let css = ":root {}\n";
        let mut resp = css.into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/css"),
        );
        return Ok(resp);
    }
    let vars = active.theme_variables.clone().unwrap_or_default();
    let css = generate_theme_css(&vars);
    let mut resp = css.into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/css"),
    );
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_validation_accepts_theme() {
        let raw = serde_json::json!({
            "kind": "gobrowse-ui",
            "manifest_version": 1,
            "name": "dark-theme",
            "version": "1.0.0",
            "ui_kind": "THEME",
            "description": "dark",
            "theme": { "variables": { "--ink": "#fff" } }
        });
        assert!(validate_manifest(raw).is_ok());
    }

    #[test]
    fn manifest_validation_rejects_bad_kind() {
        let raw = serde_json::json!({
            "kind": "bad",
            "manifest_version": 1,
            "name": "x",
            "version": "1.0.0",
            "ui_kind": "THEME"
        });
        assert!(validate_manifest(raw).is_err());
    }

    #[test]
    fn digest_is_deterministic() {
        let v = serde_json::json!({"a": 1});
        assert_eq!(compute_digest(&v), compute_digest(&v));
    }

    #[test]
    fn theme_css_generates() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("--ink".to_string(), "#fff".to_string());
        let css = generate_theme_css(&vars);
        assert!(css.contains("--ink"));
    }
}
