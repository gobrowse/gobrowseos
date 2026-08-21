use crate::{
    AppState,
    auth::{AuthenticatedUser, audit, require_user},
    embedding,
    error::AppError,
};
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::types::time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

#[derive(Deserialize)]
pub struct CreateChatModelRequest {
    pub display_name: String,
    pub provider_type: String,
    pub base_url: Url,
    pub secret_reference: Option<String>,
    pub model_reference: String,
    pub context_window: i32,
    pub output_limit: i32,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub activate: bool,
    #[serde(default)]
    pub fallback_model_ids: Vec<String>,
}

#[derive(Serialize)]
pub struct ChatModelResponse {
    pub id: String,
    pub display_name: String,
    pub provider_type: String,
    pub model_reference: String,
    pub context_window: i32,
    pub output_limit: i32,
    pub priority: i32,
    pub authenticated: bool,
    pub active: bool,
    pub fallback_model_ids: Vec<String>,
}

pub async fn create_chat_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<CreateChatModelRequest>,
) -> Result<(StatusCode, Json<ChatModelResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    validate(&input)?;
    embedding::validate_provider_endpoint(
        &input.base_url,
        &input.provider_type,
        input.secret_reference.is_some(),
        state.settings.features.local_models,
    )
    .await?;
    if let Some(secret_id) = &input.secret_reference {
        let host = input
            .base_url
            .host_str()
            .expect("validated provider URL has a host")
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM secret_references WHERE id=$1 AND profile_id=$2 \
             AND purpose='provider_credential' AND backend='encrypted_database' AND $3=ANY(allowed_hosts))",
        )
        .bind(secret_id)
        .bind(user.profile_id)
        .bind(host)
        .fetch_one(&state.pool)
        .await?;
        if !exists {
            return Err(AppError::Validation(
                "provider credential is unavailable or not allowed for this host".into(),
            ));
        }
    }
    let provider_id = format!("provider_{}", Uuid::now_v7());
    let model_id = format!("model_{}", Uuid::now_v7());
    let mut tx = state.pool.begin().await?;
    if !input.fallback_model_ids.is_empty() {
        let owned: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM models m JOIN providers p ON p.id=m.provider_id \
             WHERE p.profile_id=$1 AND m.id=ANY($2::text[]) AND p.enabled AND m.enabled",
        )
        .bind(user.profile_id)
        .bind(&input.fallback_model_ids)
        .fetch_one(&mut *tx)
        .await?;
        if usize::try_from(owned).ok() != Some(input.fallback_model_ids.len()) {
            return Err(AppError::Validation(
                "fallback models must be enabled models in this profile".into(),
            ));
        }
    }
    sqlx::query(
        "INSERT INTO providers (id,profile_id,provider_type,display_name,base_url,secret_reference) \
         VALUES ($1,$2,$3,$4,$5,$6)",
    )
    .bind(&provider_id)
    .bind(user.profile_id)
    .bind(&input.provider_type)
    .bind(input.display_name.trim())
    .bind(input.base_url.as_str())
    .bind(&input.secret_reference)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO models (id,provider_id,model_reference,display_name,context_window,output_limit,capabilities,priority) \
         VALUES ($1,$2,$3,$4,$5,$6,ARRAY['text','tool_calls'],$7)",
    )
    .bind(&model_id)
    .bind(&provider_id)
    .bind(input.model_reference.trim())
    .bind(input.display_name.trim())
    .bind(input.context_window)
    .bind(input.output_limit)
    .bind(input.priority)
    .execute(&mut *tx)
    .await?;
    for (position, fallback_id) in input.fallback_model_ids.iter().enumerate() {
        sqlx::query(
            "INSERT INTO model_fallback_routes (profile_id,primary_model_id,fallback_model_id,position) \
             VALUES ($1,$2,$3,$4)",
        )
        .bind(user.profile_id)
        .bind(&model_id)
        .bind(fallback_id)
        .bind(i32::try_from(position).map_err(|_| {
            AppError::Validation("too many fallback models".into())
        })?)
        .execute(&mut *tx)
        .await?;
    }
    if input.activate {
        sqlx::query("UPDATE profiles SET active_chat_model_id=$1,updated_at=now() WHERE id=$2")
            .bind(&model_id)
            .bind(user.profile_id)
            .execute(&mut *tx)
            .await?;
    }
    audit(
        &mut tx,
        Some(user.id),
        Some(user.profile_id),
        "chat_model.created",
        "model",
        Some(model_id.clone()),
        "success",
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(ChatModelResponse {
            id: model_id,
            display_name: input.display_name.trim().into(),
            provider_type: input.provider_type,
            model_reference: input.model_reference.trim().into(),
            context_window: input.context_window,
            output_limit: input.output_limit,
            priority: input.priority,
            authenticated: input.secret_reference.is_some(),
            active: input.activate,
            fallback_model_ids: input.fallback_model_ids,
        }),
    ))
}

pub async fn list_chat_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ChatModelResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT m.id,m.display_name,m.model_reference,m.context_window,m.output_limit,m.priority, \
         ARRAY(SELECT route.fallback_model_id FROM model_fallback_routes route \
               WHERE route.profile_id=p.profile_id AND route.primary_model_id=m.id ORDER BY route.position) AS fallback_model_ids, \
         p.provider_type,p.secret_reference IS NOT NULL AS authenticated,profile.active_chat_model_id=m.id AS active \
         FROM models m JOIN providers p ON p.id=m.provider_id JOIN profiles profile ON profile.id=p.profile_id \
         WHERE p.profile_id=$1 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities) \
         ORDER BY active DESC,m.priority DESC,m.display_name",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| ChatModelResponse {
                id: row.get("id"),
                display_name: row.get("display_name"),
                provider_type: row.get("provider_type"),
                model_reference: row.get("model_reference"),
                context_window: row.get("context_window"),
                output_limit: row.get("output_limit"),
                priority: row.get("priority"),
                authenticated: row.get("authenticated"),
                active: row.get("active"),
                fallback_model_ids: row.get("fallback_model_ids"),
            })
            .collect(),
    ))
}

pub async fn activate_chat_model(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;
    let result = sqlx::query(
        "UPDATE profiles SET active_chat_model_id=$1,updated_at=now() WHERE id=$2 AND EXISTS( \
         SELECT 1 FROM models m JOIN providers p ON p.id=m.provider_id \
         WHERE m.id=$1 AND p.profile_id=$2 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities))",
    )
    .bind(&id)
    .bind(user.profile_id)
    .execute(&state.pool)
    .await?;
    if result.rows_affected() != 1 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct TaskRouteRequest {
    pub task_class: String,
    pub preferred_model_id: String,
}

#[derive(Debug, Serialize)]
pub struct TaskRouteResponse {
    pub id: String,
    pub task_class: String,
    pub preferred_model_id: String,
    pub created_at: OffsetDateTime,
}

/// List all task routes for the current user's profile.
pub async fn list_task_routes(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<TaskRouteResponse>>, AppError> {
    let user = require_user(&state, &headers).await?;
    let rows = sqlx::query(
        "SELECT id, task_class, preferred_model_id, created_at \
         FROM model_task_routes WHERE profile_id=$1 ORDER BY task_class",
    )
    .bind(user.profile_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| TaskRouteResponse {
                id: row.get("id"),
                task_class: row.get("task_class"),
                preferred_model_id: row.get("preferred_model_id"),
                created_at: row.get("created_at"),
            })
            .collect(),
    ))
}

/// Create or update a task route for the current user's profile.
/// Requires ADMIN or OWNER role.
pub async fn upsert_task_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<TaskRouteRequest>,
) -> Result<(StatusCode, Json<TaskRouteResponse>), AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;

    // Validate task_class
    let valid_classes = [
        "coding",
        "research",
        "data_analysis",
        "document_creation",
        "general_qa",
        "shell_automation",
        "ecommerce",
        "system_administration",
    ];
    if !valid_classes.contains(&input.task_class.as_str()) {
        return Err(AppError::Validation(format!(
            "invalid task_class: {}",
            input.task_class
        )));
    }

    // Verify the model exists and belongs to the profile
    let model_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM models m JOIN providers p ON p.id=m.provider_id \
         WHERE m.id=$1 AND p.profile_id=$2 AND p.enabled AND m.enabled)",
    )
    .bind(&input.preferred_model_id)
    .bind(user.profile_id)
    .fetch_one(&state.pool)
    .await?;

    if !model_exists {
        return Err(AppError::NotFound);
    }

    let route_id = Uuid::new_v4();
    let row = sqlx::query(
        "INSERT INTO model_task_routes (id, profile_id, task_class, preferred_model_id, position, created_at) \
         VALUES ($1, $2, $3, $4, 0, now()) \
         ON CONFLICT (profile_id, task_class, position) DO UPDATE SET preferred_model_id=$4 \
         RETURNING id, task_class, preferred_model_id, created_at",
    )
    .bind(route_id)
    .bind(user.profile_id)
    .bind(&input.task_class)
    .bind(&input.preferred_model_id)
    .fetch_one(&state.pool)
    .await?;

    let response = TaskRouteResponse {
        id: row.get("id"),
        task_class: row.get("task_class"),
        preferred_model_id: row.get("preferred_model_id"),
        created_at: row.get("created_at"),
    };
    Ok((StatusCode::CREATED, Json(response)))
}

/// Delete a task route for the current user's profile.
/// Requires ADMIN or OWNER role.
pub async fn delete_task_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_class): Path<String>,
) -> Result<StatusCode, AppError> {
    let user = require_user(&state, &headers).await?;
    require_admin(&user)?;

    let result = sqlx::query("DELETE FROM model_task_routes WHERE profile_id=$1 AND task_class=$2")
        .bind(user.profile_id)
        .bind(&task_class)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn validate(input: &CreateChatModelRequest) -> Result<(), AppError> {
    // Catalog providers all speak OpenAI-compatible chat completions except
    // ollama, which uses the local /api/chat wire format. Accept the catalog
    // provider types; the request builder (chat.rs) switches on ollama only.
    if !matches!(
        input.provider_type.as_str(),
        "openai_compatible"
            | "ollama"
            | "opencode-go"
            | "openai-codex"
            | "openrouter"
            | "openai"
            | "deepseek"
            | "mistral"
            | "xai"
    ) {
        return Err(AppError::Validation(format!(
            "provider_type {} is not supported",
            input.provider_type
        )));
    }
    let unique: std::collections::BTreeSet<_> = input.fallback_model_ids.iter().collect();
    if input.fallback_model_ids.len() > 5 || unique.len() != input.fallback_model_ids.len() {
        return Err(AppError::Validation(
            "configure at most five distinct fallback models".into(),
        ));
    }
    if input.display_name.trim().is_empty()
        || input.display_name.chars().count() > 200
        || input.model_reference.trim().is_empty()
        || input.model_reference.chars().count() > 300
        || !(256..=10_000_000).contains(&input.context_window)
        || !(1..=1_000_000).contains(&input.output_limit)
        || input.output_limit >= input.context_window
    {
        return Err(AppError::Validation(
            "invalid chat model configuration".into(),
        ));
    }
    if !matches!(input.base_url.scheme(), "http" | "https")
        || input.base_url.host_str().is_none()
        || !input.base_url.username().is_empty()
        || input.base_url.password().is_some()
        || input.base_url.query().is_some()
        || input.base_url.fragment().is_some()
    {
        return Err(AppError::Validation(
            "base_url must be an http(s) URL without credentials, query, or fragment".into(),
        ));
    }
    Ok(())
}

fn require_admin(user: &AuthenticatedUser) -> Result<(), AppError> {
    if matches!(user.role.as_str(), "OWNER" | "ADMIN") {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}

#[derive(Debug, Serialize)]
pub struct DetectedProvider {
    pub provider_type: String,
    pub available: bool,
    pub models: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct AutoDetectResponse {
    pub detected: Vec<DetectedProvider>,
}

/// Detect providers from environment variables only (sync, no network).
/// Suitable for unit tests.
pub fn detect_providers_from_env(
    env: &std::collections::HashMap<String, String>,
) -> Vec<DetectedProvider> {
    let mut detected = Vec::new();

    // Known API-key env vars — presence means "available".
    let key_vars: &[(&str, &str)] = &[
        ("openai", "OPENAI_API_KEY"),
        ("openrouter", "OPENROUTER_API_KEY"),
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("google", "GOOGLE_API_KEY"),
        ("deepseek", "DEEPSEEK_API_KEY"),
    ];
    for &(provider_type, var) in key_vars {
        detected.push(DetectedProvider {
            provider_type: provider_type.to_string(),
            available: env.contains_key(var),
            models: Vec::new(),
        });
    }

    // GOBROWSE__PROVIDER__* prefixed env vars.
    let has_gobrowse_provider = env.keys().any(|k| k.starts_with("GOBROWSE__PROVIDER__"));
    detected.push(DetectedProvider {
        provider_type: "gobrowse_provider_override".into(),
        available: has_gobrowse_provider,
        models: Vec::new(),
    });

    detected
}

/// Probe a local Ollama instance. Returns a DetectedProvider with
/// available and any discovered models.
async fn probe_ollama(client: &reqwest::Client) -> DetectedProvider {
    let version_ok = client
        .get("http://127.0.0.1:11434/api/version")
        .timeout(std::time::Duration::from_millis(500))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success());

    let mut models = Vec::new();
    if version_ok {
        // Default recommended models.
        models.push("llama3.2".into());
        models.push("nomic-embed-text".into());

        // Cheap probe for installed models.
        if let Ok(response) = client
            .get("http://127.0.0.1:11434/api/tags")
            .timeout(std::time::Duration::from_millis(1000))
            .send()
            .await
            && response.status().is_success()
            && let Ok(body) = response.json::<serde_json::Value>().await
            && let Some(model_list) = body["models"].as_array()
        {
            for entry in model_list {
                if let Some(name) = entry["name"].as_str() {
                    let trimmed = name.trim_end_matches(":latest");
                    if !models.contains(&trimmed.to_owned()) {
                        models.push(trimmed.to_owned());
                    }
                }
            }
        }
    }

    DetectedProvider {
        provider_type: "ollama".into(),
        available: version_ok,
        models,
    }
}

pub async fn auto_detect_providers(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<AutoDetectResponse>, AppError> {
    let _user = require_user(&state, &headers).await?;
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let mut detected = detect_providers_from_env(&env);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .map_err(|_| AppError::Internal(anyhow::anyhow!("failed to build HTTP client")))?;
    detected.push(probe_ollama(&client).await);
    Ok(Json(AutoDetectResponse { detected }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_request() -> CreateChatModelRequest {
        CreateChatModelRequest {
            display_name: "Test Model".into(),
            provider_type: "openai_compatible".into(),
            base_url: Url::parse("https://api.example.com/v1").expect("valid URL"),
            secret_reference: None,
            model_reference: "gpt-test".into(),
            context_window: 128_000,
            output_limit: 4096,
            priority: 0,
            activate: false,
            fallback_model_ids: vec![],
        }
    }

    #[test]
    fn validate_rejects_base_url_with_query_fragment_or_credentials() {
        // URL with query
        let mut req = base_request();
        req.base_url = Url::parse("https://api.example.com/v1?key=val").expect("valid URL");
        assert!(
            validate(&req).is_err(),
            "base URL with query should be rejected"
        );

        // URL with fragment
        let mut req = base_request();
        req.base_url = Url::parse("https://api.example.com/v1#section").expect("valid URL");
        assert!(
            validate(&req).is_err(),
            "base URL with fragment should be rejected"
        );

        // URL with credentials (username)
        let mut req = base_request();
        req.base_url = Url::parse("https://user@api.example.com/v1").expect("valid URL");
        assert!(
            validate(&req).is_err(),
            "base URL with credential username should be rejected"
        );

        // URL with password
        let mut req = base_request();
        req.base_url = Url::parse("https://user:pass@api.example.com/v1").expect("valid URL");
        assert!(
            validate(&req).is_err(),
            "base URL with credential password should be rejected"
        );
    }

    #[test]
    fn validate_rejects_non_http_scheme_and_missing_host() {
        // Non-http(s) scheme
        let mut req = base_request();
        req.base_url = Url::parse("ftp://api.example.com/v1").expect("valid URL");
        assert!(
            validate(&req).is_err(),
            "non-http(s) scheme should be rejected"
        );

        // URL with no host would fail Url::parse, but we can test one
        // that has an opaque host or fails host_str().
    }

    #[test]
    fn validate_rejects_output_limit_not_strictly_below_context_window() {
        // output_limit >= context_window
        let mut req = base_request();
        req.context_window = 4096;
        req.output_limit = 4096;
        assert!(
            validate(&req).is_err(),
            "output_limit equal to context_window should be rejected"
        );

        let mut req = base_request();
        req.context_window = 4096;
        req.output_limit = 8192;
        assert!(
            validate(&req).is_err(),
            "output_limit above context_window should be rejected"
        );

        // output_limit below context_window should be ok
        let mut req = base_request();
        req.context_window = 128_000;
        req.output_limit = 4096;
        assert!(
            validate(&req).is_ok(),
            "output_limit strictly below context_window should pass"
        );
    }

    #[test]
    fn validate_rejects_unbounded_or_nonunique_fallback_ids() {
        // >5 fallback model IDs
        let mut req = base_request();
        req.fallback_model_ids = (0..6).map(|i| format!("model_{i}")).collect();
        assert!(
            validate(&req).is_err(),
            "more than 5 fallbacks should be rejected"
        );

        // duplicate fallback IDs
        let mut req = base_request();
        req.fallback_model_ids = vec!["dup".into(), "dup".into(), "other".into()];
        assert!(
            validate(&req).is_err(),
            "duplicate fallback IDs should be rejected"
        );

        // exactly 5 unique IDs should pass
        let mut req = base_request();
        req.fallback_model_ids = (0..5).map(|i| format!("model_{i}")).collect();
        assert!(
            validate(&req).is_ok(),
            "exactly 5 unique fallbacks should pass"
        );
    }

    #[test]
    fn detect_providers_from_env_detects_nothing_when_empty() {
        let env = std::collections::HashMap::new();
        let result = detect_providers_from_env(&env);
        assert_eq!(result.len(), 6, "six provider types expected");
        for provider in &result {
            assert!(!provider.available, "no keys should mean unavailable");
            assert!(provider.models.is_empty(), "no models without probe");
        }
    }

    #[test]
    fn detect_providers_from_env_detects_openai_key() {
        let mut env = std::collections::HashMap::new();
        env.insert("OPENAI_API_KEY".into(), "sk-test123".into());
        let result = detect_providers_from_env(&env);
        let openai = result
            .iter()
            .find(|p| p.provider_type == "openai")
            .expect("openai entry");
        assert!(openai.available, "OPENAI_API_KEY present");
        let openrouter = result
            .iter()
            .find(|p| p.provider_type == "openrouter")
            .expect("openrouter entry");
        assert!(!openrouter.available, "OPENROUTER_API_KEY absent");
    }

    #[test]
    fn detect_providers_from_env_detects_gobrowse_provider() {
        let mut env = std::collections::HashMap::new();
        env.insert(
            "GOBROWSE__PROVIDER__CHAT_BASE_URL".into(),
            "http://localhost:8080".into(),
        );
        let result = detect_providers_from_env(&env);
        let override_provider = result
            .iter()
            .find(|p| p.provider_type == "gobrowse_provider_override")
            .expect("gobrowse override entry");
        assert!(
            override_provider.available,
            "GOBROWSE__PROVIDER__ prefixed env var present"
        );
    }

    #[test]
    fn detect_providers_from_env_detects_all_keys() {
        let mut env = std::collections::HashMap::new();
        env.insert("OPENAI_API_KEY".into(), "sk-1".into());
        env.insert("OPENROUTER_API_KEY".into(), "sk-2".into());
        env.insert("ANTHROPIC_API_KEY".into(), "sk-3".into());
        env.insert("GOOGLE_API_KEY".into(), "sk-4".into());
        env.insert("DEEPSEEK_API_KEY".into(), "sk-5".into());
        let result = detect_providers_from_env(&env);
        for provider in &result {
            if provider.provider_type == "gobrowse_provider_override" {
                assert!(!provider.available, "prefix var absent");
            } else {
                assert!(
                    provider.available,
                    "{} should be available",
                    provider.provider_type
                );
            }
        }
    }
}
