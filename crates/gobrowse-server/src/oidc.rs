//! OIDC login via Authorization Code Flow with PKCE (M25a Batch 3).
//!
//! Provider discovery, state/nonce ceremony held in memory (5 min TTL),
//! ID token verification (iss/aud/exp/nonce), account linking by email.
//! New users from OIDC are created as MEMBER; the OWNER must be established
//! through password setup (no silent privilege escalation).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Redirect},
};
use openidconnect::{
    AuthenticationFlow, AuthorizationCode, ClientId, ClientSecret, CsrfToken, IssuerUrl, Nonce,
    PkceCodeChallenge, PkceCodeVerifier, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{
    AppState,
    auth::{audit, create_session, require_user},
    config::OidcProviderConfig,
    error::AppError,
};

/// In-memory OIDC ceremony state.
struct OidcCeremony {
    provider_label: String,
    csrf: CsrfToken,
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    created: Instant,
}

#[derive(Clone)]
pub struct OidcManager {
    ceremonies: Arc<RwLock<HashMap<String, OidcCeremony>>>,
}

impl Default for OidcManager {
    fn default() -> Self {
        Self::new()
    }
}

impl OidcManager {
    pub fn new() -> Self {
        Self {
            ceremonies: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn prune(&self, store: &mut HashMap<String, OidcCeremony>) {
        let cutoff = Instant::now() - Duration::from_secs(300);
        store.retain(|_, c| c.created > cutoff);
    }
}

/// Start an OIDC login for a configured provider: returns the redirect URL.
#[derive(Deserialize)]
pub struct OidcStartRequest {
    pub provider: String,
}

#[derive(serde::Serialize)]
pub struct OidcStartResponse {
    pub redirect_url: String,
}

pub async fn start_oidc(
    State(state): State<AppState>,
    Json(input): Json<OidcStartRequest>,
) -> Result<Json<OidcStartResponse>, AppError> {
    let provider = state
        .settings
        .auth
        .oidc_providers
        .iter()
        .find(|p| p.label == input.provider)
        .ok_or_else(|| AppError::Validation("unknown OIDC provider".into()))?;
    let client = build_client(&state, provider).await?;
    let (auth_url, csrf, nonce, verifier) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(PkceCodeChallenge::new_random_sha256().0)
        .url();
    // Persist ceremony.
    let mut store = state.oidc.ceremonies.write().await;
    state.oidc.prune(&mut store);
    store.insert(
        csrf.secret().to_string(),
        OidcCeremony {
            provider_label: provider.label.clone(),
            csrf,
            nonce,
            verifier,
            created: Instant::now(),
        },
    );
    drop(store);
    Ok(Json(OidcStartResponse {
        redirect_url: auth_url.to_string(),
    }))
}

/// OIDC callback: exchange code, verify ID token, link or create user,
/// create a session.
#[derive(Deserialize)]
pub struct OidcCallbackParams {
    pub code: String,
    pub state: String,
}

pub async fn oidc_callback(
    State(state): State<AppState>,
    Query(params): Query<OidcCallbackParams>,
) -> Result<impl IntoResponse, AppError> {
    let mut store = state.oidc.ceremonies.write().await;
    let Some(ceremony) = store.remove(&params.state) else {
        return Err(AppError::Validation("OIDC state invalid or expired"));
    };
    drop(store);

    let provider = state
        .settings
        .auth
        .oidc_providers
        .iter()
        .find(|p| p.label == ceremony.provider_label)
        .ok_or_else(|| AppError::Validation("OIDC provider no longer configured".into()))?;
    let client = build_client(&state, provider).await?;

    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("http client: {e}")))?;
    let token_response = client
        .exchange_code(AuthorizationCode::new(params.code))
        .set_pkce_verifier(ceremony.verifier)
        .request_async(&http_client)
        .await
        .map_err(|e| AppError::Validation(format!("OIDC token exchange failed: {e}")))?;

    let id_token = token_response
        .extra_fields()
        .id_token()
        .ok_or_else(|| AppError::Validation("OIDC provider returned no id_token".into()))?;
    let claims = id_token
        .claims(&client.id_token_verifier(), &ceremony.nonce)
        .map_err(|e| AppError::Validation(format!("OIDC id_token verification failed: {e}")))?;
    let subject = claims.subject().to_string();
    let email = claims.email().map(|e| e.to_string()).unwrap_or_default();
    if email.is_empty() {
        return Err(AppError::Validation(
            "OIDC provider did not return an email claim".into(),
        ));
    }

    let mut tx = state.pool.begin().await?;
    // Existing user by email -> link; else create as MEMBER.
    let user_id: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE lower(email) = lower($1)")
            .bind(&email)
            .fetch_optional(&mut *tx)
            .await?;

    let (user_id, is_new) = match user_id {
        Some(id) => (id, false),
        None => {
            let id = Uuid::now_v7();
            let profile_id = Uuid::now_v7();
            let display = claims
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| email.split('@').next().unwrap_or("User").to_string());
            sqlx::query("INSERT INTO profiles (id, name) VALUES ($1, $2)")
                .bind(profile_id)
                .bind(format!("{display}'s profile"))
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO users (id, primary_profile_id, email, display_name, role, password_hash) \
                 VALUES ($1, $2, $3, $4, 'MEMBER', '')",
            )
            .bind(id)
            .bind(profile_id)
            .bind(&email)
            .bind(&display)
            .execute(&mut *tx)
            .await?;
            (id, true)
        }
    };

    // Link the OIDC method (idempotent).
    let method_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO auth_methods (id, user_id, method_type, method_data, label, created_at) \
         VALUES ($1, $2, 'oidc', $3, $4, now()) \
         ON CONFLICT (user_id, method_type, label) DO NOTHING",
    )
    .bind(method_id)
    .bind(user_id)
    .bind(serde_json::json!({ "subject": subject, "provider": provider.label }))
    .bind(&provider.label)
    .execute(&mut *tx)
    .await?;

    let (cookie, user) = create_session(&state, &mut tx, user_id).await?;
    audit(
        &mut tx,
        Some(user_id),
        Some(user.profile_id),
        if is_new {
            "auth.oidc_user_created"
        } else {
            "auth.login_oidc"
        },
        "auth_method",
        None,
        "success",
    )
    .await?;
    tx.commit().await?;

    // Redirect back to the app with the session cookie set.
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

/// List configured OIDC providers for the login screen.
pub async fn list_oidc_providers(
    State(state): State<AppState>,
    _headers: HeaderMap,
) -> Result<Json<serde_json::Value>, AppError> {
    let providers: Vec<serde_json::Value> = state
        .settings
        .auth
        .oidc_providers
        .iter()
        .map(|p| serde_json::json!({ "label": p.label, "scopes": p.scopes }))
        .collect();
    Ok(Json(serde_json::json!({ "providers": providers })))
}

/// Build a CoreClient from provider config via discovery.
async fn build_client(
    state: &AppState,
    provider: &OidcProviderConfig,
) -> Result<CoreClient, AppError> {
    let issuer = IssuerUrl::new(provider.issuer.clone())
        .map_err(|e| AppError::Validation(format!("invalid issuer: {e}")))?;
    let secret = resolve_secret(state, &provider.client_secret_ref).await?;
    let http_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("http client: {e}")))?;
    let metadata = CoreProviderMetadata::discover_async(issuer, &http_client)
        .await
        .map_err(|e| AppError::Validation(format!("OIDC discovery failed: {e}")))?;
    let redirect = openidconnect::RedirectUrl::new(format!(
        "{}/api/v1/auth/oidc/callback",
        state
            .settings
            .http
            .public_origin
            .as_str()
            .trim_end_matches('/')
    ))
    .map_err(|e| AppError::Validation(format!("invalid redirect URL: {e}")))?;
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(provider.client_id.clone()),
        Some(ClientSecret::new(secret)),
    )
    .set_redirect_uri(redirect))
}

/// Resolve a client secret from the vault (secret reference) or plain text.
async fn resolve_secret(state: &AppState, secret_ref: &str) -> Result<String, AppError> {
    let trimmed = secret_ref.trim();
    if trimmed.starts_with("secret_") || trimmed.starts_with("vault:") {
        let key = trimmed.trim_start_matches("vault:");
        if let Ok(value) = state.vault.resolve_reference(key).await {
            return Ok(value);
        }
        return Err(AppError::Validation(
            "OIDC client secret reference could not be resolved".into(),
        ));
    }
    Ok(trimmed.to_string())
}
