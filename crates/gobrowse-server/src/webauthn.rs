//! WebAuthn passkey registration + login (M25a Batch 2).
//!
//! Uses `webauthn-rs` (attestation `none`, RP ID from the configured public
//! origin, resident-key passkeys). Ceremonies are held in memory keyed by
//! challenge id and expire after 5 minutes. Credentials are stored in
//! `auth_methods.method_data` as JSON: `{credential_id, passkey}`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::{
    AppState,
    auth::{audit, require_user},
    error::AppError,
};

/// Ceremony state held between the start and finish steps.
struct CeremonyStore {
    regs: HashMap<String, (PasskeyRegistration, Instant)>,
    auths: HashMap<String, (PasskeyAuthentication, Instant)>,
}

impl CeremonyStore {
    fn new() -> Self {
        Self {
            regs: HashMap::new(),
            auths: HashMap::new(),
        }
    }
}

/// WebAuthn manager wrapping the core verifier + ceremony state.
#[derive(Clone)]
pub struct WebauthnManager {
    core: Arc<Webauthn>,
    ceremonies: Arc<RwLock<CeremonyStore>>,
}

impl WebauthnManager {
    /// Build from a public origin (e.g. `https://host:port`).
    pub fn from_origin(rp_name: &str, public_origin: &str) -> Result<Self, AppError> {
        let origin = public_origin.trim_end_matches('/');
        let rp_origin = url::Url::parse(origin)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid public origin: {e}")))?;
        // IP origins (e.g. http://178.128.179.216:8080) have no `.domain()`;
        // fall back to the bare host (host_str handles both names and IPs).
        let rp_id = rp_origin
            .domain()
            .or_else(|| rp_origin.host_str())
            .unwrap_or(origin)
            .to_string();
        let core = WebauthnBuilder::new(&rp_id, &rp_origin)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn config: {e}")))?
            .rp_name(rp_name)
            .build()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn init: {e}")))?;
        Ok(Self {
            core: Arc::new(core),
            ceremonies: Arc::new(RwLock::new(CeremonyStore::new())),
        })
    }

    /// Build from app settings.
    ///
    /// If `auth.webauthn_rp_id` is set, use it as the RP ID (the origin
    /// still comes from `public_origin`).  Otherwise auto-derive from the
    /// origin domain (current behaviour).
    pub fn from_settings(settings: &crate::config::Settings) -> Result<Self, AppError> {
        let origin = settings.http.public_origin.as_str();
        if let Some(rp_id) = &settings.auth.webauthn_rp_id
            && rp_id_matches_origin(rp_id, origin)
        {
            Self::from_origin_with_rp_id("Gobrowse OS", origin, rp_id)
        } else {
            Self::from_origin("Gobrowse OS", origin)
        }
    }

    /// WebAuthn spec: the RP ID may differ from the effective origin host ONLY
    /// when the host is a subdomain of the RP ID (e.g. app.example.com under
    /// example.com). For IP origins or unrelated domains there is no valid
    /// pairing — the browser's `clientDataJSON.origin` would never match the
    /// verifier's expected origin, so every ceremony would fail.
    fn rp_id_matches_origin(rp_id: &str, public_origin: &str) -> bool {
        let Ok(parsed) = url::Url::parse(public_origin.trim_end_matches('/')) else {
            return false;
        };
        let Some(host) = parsed.host_str() else {
            return false;
        };
        let rp_id = rp_id.trim_end_matches('.');
        host.eq_ignore_ascii_case(rp_id)
            || host
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", rp_id.to_ascii_lowercase()))
    }

    /// Build with an explicit RP ID override.
    ///
    /// The `public_origin` supplies the scheme and port; the RP ID domain
    /// replaces the host so that `WebauthnBuilder` accepts the combination.
    /// In production the server must be reachable through that domain (e.g.
    /// via a reverse proxy) for the browser to complete the ceremony.
    pub fn from_origin_with_rp_id(
        rp_name: &str,
        public_origin: &str,
        rp_id: &str,
    ) -> Result<Self, AppError> {
        let origin = public_origin.trim_end_matches('/');
        let parsed = url::Url::parse(origin)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid public origin: {e}")))?;
        // Rebuild the origin URL using the RP ID as the host so the
        // builder sees a matching (rp_id, origin) pair.
        let mut rp_origin = parsed;
        rp_origin
            .set_host(Some(rp_id))
            .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid rp_id host: {e}")))?;
        let core = WebauthnBuilder::new(rp_id, &rp_origin)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn config: {e}")))?
            .rp_name(rp_name)
            .build()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn init: {e}")))?;
        Ok(Self {
            core: Arc::new(core),
            ceremonies: Arc::new(RwLock::new(CeremonyStore::new())),
        })
    }

    fn prune(&self, store: &mut CeremonyStore) {
        let cutoff = Instant::now() - Duration::from_secs(300);
        store.regs.retain(|_, (_, at)| *at > cutoff);
        store.auths.retain(|_, (_, at)| *at > cutoff);
    }
}

// ---- Registration (authenticated user adds a passkey) ----

#[derive(Deserialize)]
pub struct RegisterStartRequest {
    pub display_name: Option<String>,
}

#[derive(serde::Serialize)]
pub struct RegisterStartResponse {
    pub ceremony_id: String,
    pub creation_options: serde_json::Value,
}

pub async fn start_registration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(_input): Json<RegisterStartRequest>,
) -> Result<Json<RegisterStartResponse>, AppError> {
    let user = require_user(&state, &headers).await?;
    let manager = state
        .webauthn
        .as_ref()
        .ok_or_else(|| AppError::Conflict("WebAuthn is not configured"))?;

    // Exclude already-registered credentials for this user.
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT method_data->>'credential_id' FROM auth_methods \
         WHERE user_id = $1 AND method_type = 'webauthn'",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    let exclude: Vec<CredentialID> = existing
        .iter()
        .filter_map(|id| {
            base64::Engine::decode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                id.as_bytes(),
            )
            .ok()
            .map(CredentialID::from)
        })
        .collect();

    let (challenge, registration) = manager
        .core
        .start_passkey_registration(user.id, &user.email, &user.display_name, Some(exclude))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn start: {e}")))?;

    let ceremony_id = Uuid::now_v7().to_string();
    let mut store = manager.ceremonies.write().await;
    manager.prune(&mut store);
    store
        .regs
        .insert(ceremony_id.clone(), (registration, Instant::now()));
    drop(store);

    Ok(Json(RegisterStartResponse {
        ceremony_id,
        creation_options: serde_json::to_value(challenge)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize options: {e}")))?,
    }))
}

#[derive(Deserialize)]
pub struct RegisterCompleteRequest {
    pub ceremony_id: String,
    /// The browser's PublicKeyCredential JSON (with rawId/response).
    pub credential: serde_json::Value,
}

pub async fn complete_registration(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<RegisterCompleteRequest>,
) -> Result<Response, AppError> {
    let user = require_user(&state, &headers).await?;
    let manager = state
        .webauthn
        .as_ref()
        .ok_or_else(|| AppError::Conflict("WebAuthn is not configured"))?;

    let mut store = manager.ceremonies.write().await;
    let Some((registration, _)) = store.regs.remove(&input.ceremony_id) else {
        return Err(AppError::Conflict(
            "registration ceremony expired or unknown",
        ));
    };
    drop(store);

    let credential: RegisterPublicKeyCredential = serde_json::from_value(input.credential)
        .map_err(|e| AppError::Validation(format!("invalid credential JSON: {e}")))?;
    let passkey = manager
        .core
        .finish_passkey_registration(&credential, &registration)
        .map_err(|e| AppError::Validation(format!("passkey verification failed: {e}")))?;

    let credential_id = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        passkey.cred_id().as_ref(),
    );
    let passkey_json = serde_json::to_string(&passkey)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize passkey: {e}")))?;

    let mut tx = state.pool.begin().await?;
    let dup: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM auth_methods WHERE user_id = $1 AND method_type = 'webauthn' \
         AND method_data->>'credential_id' = $2)",
    )
    .bind(user.id)
    .bind(&credential_id)
    .fetch_one(&mut *tx)
    .await?;
    if dup {
        return Err(AppError::Conflict("this credential is already registered"));
    }
    let method_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth_methods (id, user_id, method_type, method_data, label, created_at) \
         VALUES ($1, $2, 'webauthn', $3, 'Passkey', now())",
    )
    .bind(method_id)
    .bind(user.id)
    .bind(serde_json::json!({ "credential_id": credential_id, "passkey": passkey_json }))
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "auth.webauthn_registered",
        "auth_method",
        Some(method_id.to_string()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---- Login (public, unauthenticated) ----

#[derive(Deserialize)]
pub struct LoginStartRequest {
    pub email: Option<String>,
}

#[derive(serde::Serialize)]
pub struct LoginStartResponse {
    pub ceremony_id: String,
    pub request_options: serde_json::Value,
}

pub async fn start_login(
    State(state): State<AppState>,
    Json(_input): Json<LoginStartRequest>,
) -> Result<Json<LoginStartResponse>, AppError> {
    let manager = state
        .webauthn
        .as_ref()
        .ok_or_else(|| AppError::Conflict("WebAuthn is not configured"))?;
    // Discoverable-credential (passkey) login: no allow-list needed; the
    // authenticator returns the credential and we look it up after verify.
    let (challenge, authentication) = manager
        .core
        .start_passkey_authentication(&[])
        .map_err(|e| AppError::Internal(anyhow::anyhow!("webauthn start: {e}")))?;
    let ceremony_id = Uuid::now_v7().to_string();
    let mut store = manager.ceremonies.write().await;
    manager.prune(&mut store);
    store
        .auths
        .insert(ceremony_id.clone(), (authentication, Instant::now()));
    drop(store);
    Ok(Json(LoginStartResponse {
        ceremony_id,
        request_options: serde_json::to_value(challenge)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("serialize options: {e}")))?,
    }))
}

#[derive(Deserialize)]
pub struct LoginCompleteRequest {
    pub ceremony_id: String,
    /// The browser's PublicKeyCredential JSON (assertion).
    pub credential: serde_json::Value,
}

pub async fn complete_login(
    State(state): State<AppState>,
    Json(input): Json<LoginCompleteRequest>,
) -> Result<impl IntoResponse, AppError> {
    let manager = state
        .webauthn
        .as_ref()
        .ok_or_else(|| AppError::Conflict("WebAuthn is not configured"))?;
    let mut store = manager.ceremonies.write().await;
    let Some((authentication, _)) = store.auths.remove(&input.ceremony_id) else {
        return Err(AppError::Conflict("login ceremony expired or unknown"));
    };
    drop(store);

    let credential: PublicKeyCredential = serde_json::from_value(input.credential)
        .map_err(|e| AppError::Validation(format!("invalid credential JSON: {e}")))?;
    let result = manager
        .core
        .finish_passkey_authentication(&credential, &authentication)
        .map_err(|_| AppError::Unauthorized)?;

    // Find the user who owns this credential and create a session.
    let credential_id = base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        result.cred_id().as_ref(),
    );
    let user_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM auth_methods WHERE method_type = 'webauthn' \
         AND method_data->>'credential_id' = $1",
    )
    .bind(&credential_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(user_id) = user_id else {
        return Err(AppError::Unauthorized);
    };

    let mut tx = state.pool.begin().await?;
    let (cookie, user) = crate::auth::create_session(&state, &mut tx, user_id).await?;
    sqlx::query(
        "UPDATE auth_methods SET last_used_at = now() WHERE user_id = $1 AND method_type = 'webauthn'",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    audit(
        &mut tx,
        Some(user_id),
        Some(user.profile_id),
        "auth.login_webauthn",
        "auth_method",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok(([(axum::http::header::SET_COOKIE, cookie)], Json(user)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_origin_rejects_ip_addresses() {
        // WebAuthn spec: RP ID must be a registrable domain; IP origins are
        // invalid. `from_origin` must return Err (the app then degrades to
        // "WebAuthn unavailable" — password auth remains).
        let result = WebauthnManager::from_origin("Gobrowse OS", "http://178.128.179.216:8080");
        assert!(
            result.is_err(),
            "IP origins are not valid WebAuthn RP IDs and must be rejected"
        );
    }

    #[test]
    fn from_origin_accepts_hostnames() {
        let manager = WebauthnManager::from_origin("Gobrowse OS", "https://gobrowse.example.com")
            .expect("hostname origin should build");
        let _ = manager;
    }

    #[test]
    fn rp_id_override_requires_subdomain_or_equal() {
        // Spec-valid pairings: host is a subdomain of the RP ID (or equal).
        assert!(rp_id_matches_origin("example.com", "https://app.example.com:8080"));
        assert!(rp_id_matches_origin("example.com", "https://example.com"));
        // Invalid pairings: unrelated domain or IP origin.
        assert!(!rp_id_matches_origin("gobrowse.example.com", "http://178.128.179.216:8080"));
        assert!(!rp_id_matches_origin("other.com", "https://app.example.com"));
    }

    #[test]
    fn from_settings_degrades_on_invalid_rp_id_pairing() {
        use crate::config::Settings;
        let mut settings = Settings::default();
        settings.http.public_origin = url::Url::parse("http://178.128.179.216:8080").unwrap();
        // IP origin + unrelated RP ID: must fall through to from_origin and
        // fail (manager None -> friendly degradation), never fabricate an
        // origin the browser cannot match.
        settings.auth.webauthn_rp_id = Some("gobrowse.example.com".into());
        assert!(
            WebauthnManager::from_settings(&settings).is_err(),
            "IP origin with unrelated RP ID must degrade, not fabricate"
        );
    }
}
