//! Provider catalog and model-cost aggregation.
//!
//! The catalog is a read-only reference list of popular model providers so the
//! operator UI can offer a dropdown instead of free-text provider entry, and
//! auto-fill context window / output limits / pricing instead of asking the
//! user to type them. The usage summary aggregates per-run token usage from
//! `messages.usage` and prices it with the embedded reference pricing table
//! (USD per 1M tokens).

use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use serde::{Deserialize, Serialize};

use crate::{AppState, auth::require_user, error::AppError};

#[derive(Debug, Clone, Serialize)]
pub struct CatalogModel {
    pub reference: String,
    pub context_window: i32,
    pub output_limit: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderCatalogEntry {
    pub provider_type: String,
    pub display_name: String,
    pub base_url: String,
    pub api_format: String,
    pub models: Vec<CatalogModel>,
}

/// Reference provider catalog. Base URLs are the documented public endpoints;
/// `opencode-go` and `openai-codex` use the same endpoints the Oh My Pi harness
/// uses. Context windows/output limits and prices are reference values and are
/// never authoritative billing data.
pub fn provider_catalog() -> Vec<ProviderCatalogEntry> {
    let m = |reference: &str, context_window: i32, output_limit: i32| CatalogModel {
        reference: reference.into(),
        context_window,
        output_limit,
    };
    vec![
        ProviderCatalogEntry {
            provider_type: "opencode-go".into(),
            display_name: "OpenCode Go".into(),
            base_url: "https://opencode.ai/zen/go/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("deepseek-v4-flash", 128_000, 8_192),
                m("mimo-v2.5", 131_072, 16_384),
                m("mimo-v2.5-pro", 131_072, 16_384),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "openai-codex".into(),
            display_name: "OpenAI Codex".into(),
            base_url: "https://chatgpt.com/backend-api".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("gpt-5.6-sol", 200_000, 16_384),
                m("gpt-5.6-luna", 200_000, 16_384),
                m("gpt-5.6-terra", 200_000, 16_384),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "openrouter".into(),
            display_name: "OpenRouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("deepseek/deepseek-v4-flash", 128_000, 8_192),
                m("anthropic/claude-sonnet-4", 200_000, 16_384),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "openai".into(),
            display_name: "OpenAI".into(),
            base_url: "https://api.openai.com/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("gpt-5.6", 200_000, 16_384),
                m("gpt-4.1", 1_000_000, 32_768),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "anthropic".into(),
            display_name: "Anthropic".into(),
            base_url: "https://api.anthropic.com/v1".into(),
            api_format: "anthropic".into(),
            models: vec![
                m("claude-sonnet-4-6", 200_000, 16_384),
                m("claude-opus-4", 200_000, 16_384),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "google".into(),
            display_name: "Google Gemini".into(),
            base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
            api_format: "google".into(),
            models: vec![
                m("gemini-3.7-flash", 1_000_000, 65_536),
                m("gemini-3.7-pro", 1_000_000, 65_536),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "deepseek".into(),
            display_name: "DeepSeek".into(),
            base_url: "https://api.deepseek.com/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("deepseek-v4-flash", 128_000, 8_192),
                m("deepseek-v4-pro", 128_000, 8_192),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "mistral".into(),
            display_name: "Mistral".into(),
            base_url: "https://api.mistral.ai".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("mistral-medium-3-5", 128_000, 8_192),
                m("mistral-large-3", 200_000, 16_384),
            ],
        },
        ProviderCatalogEntry {
            provider_type: "xai".into(),
            display_name: "xAI (Grok)".into(),
            base_url: "https://api.x.ai/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![m("grok-4", 256_000, 16_384)],
        },
        ProviderCatalogEntry {
            provider_type: "ollama".into(),
            display_name: "Ollama (local)".into(),
            base_url: "http://127.0.0.1:11434/v1".into(),
            api_format: "openai-compatible".into(),
            models: vec![
                m("llama3.2", 128_000, 4_096),
                m("nomic-embed-text", 8_192, 8_192),
            ],
        },
    ]
}

/// Look up catalog defaults for a provider+model, returning
/// (context_window, output_limit) or the catalog-wide defaults.
pub fn catalog_defaults(provider_type: &str, model: &str) -> (i32, i32) {
    for entry in provider_catalog() {
        if entry.provider_type == provider_type {
            for catalog_model in &entry.models {
                if catalog_model.reference == model {
                    return (catalog_model.context_window, catalog_model.output_limit);
                }
            }
            // Fall back to the first model's window for this provider.
            if let Some(first) = entry.models.first() {
                return (first.context_window, first.output_limit);
            }
        }
    }
    (128_000, 8_192)
}

/// Reference pricing in USD per 1M tokens, keyed by `provider_type:model_reference`.
/// Unknown models price at 0.0 and are counted separately.
pub fn reference_pricing(provider_type: &str, model: &str) -> (f64, f64) {
    use std::collections::HashMap;
    let mut table: HashMap<&str, (f64, f64)> = HashMap::new();
    table.insert("opencode-go:deepseek-v4-flash", (0.14, 0.28));
    table.insert("opencode-go:mimo-v2.5", (0.14, 0.28));
    table.insert("opencode-go:mimo-v2.5-pro", (0.435, 0.87));
    table.insert("openai-codex:gpt-5.6-sol", (2.50, 10.00));
    table.insert("openai-codex:gpt-5.6-luna", (1.25, 5.00));
    table.insert("openai-codex:gpt-5.6-terra", (2.00, 8.00));
    table.insert("openai:gpt-5.6", (1.25, 10.00));
    table.insert("openai:gpt-4.1", (2.00, 8.00));
    table.insert("anthropic:claude-sonnet-4-6", (3.00, 15.00));
    table.insert("anthropic:claude-opus-4", (15.00, 75.00));
    table.insert("google:gemini-3.7-flash", (0.10, 0.40));
    table.insert("google:gemini-3.7-pro", (1.25, 10.00));
    table.insert("deepseek:deepseek-v4-flash", (0.14, 0.28));
    table.insert("deepseek:deepseek-v4-pro", (0.55, 1.10));
    table.insert("mistral:mistral-medium-3-5", (0.60, 2.40));
    table.insert("mistral:mistral-large-3", (2.00, 6.00));
    table.insert("xai:grok-4", (3.00, 15.00));
    table.insert("ollama:llama3.2", (0.0, 0.0));
    table.insert("ollama:nomic-embed-text", (0.0, 0.0));
    if let Some(rest) = model.strip_prefix("deepseek/") {
        return reference_pricing("deepseek", rest);
    }
    if let Some(rest) = model.strip_prefix("anthropic/") {
        return reference_pricing("anthropic", rest);
    }
    if let Some(rest) = model.strip_prefix("openai/") {
        return reference_pricing("openai", rest);
    }
    let key = format!("{provider_type}:{model}");
    table.get(key.as_str()).copied().unwrap_or((0.0, 0.0))
}

#[derive(Debug, Deserialize)]
pub struct UsageWindow {
    #[serde(default = "default_window")]
    pub window: String,
}

fn default_window() -> String {
    "30d".into()
}

#[derive(Debug, Serialize)]
pub struct ModelCostRow {
    pub provider: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub runs: i64,
    pub spend: f64,
}

#[derive(Debug, Serialize)]
pub struct ProviderCostRow {
    pub provider: String,
    pub spend: f64,
    pub runs: i64,
}

#[derive(Debug, Serialize)]
pub struct DailyCostRow {
    pub day: String,
    pub spend: f64,
}

#[derive(Debug, Serialize)]
pub struct UsageSummary {
    pub total_spend: f64,
    pub total_input_tokens: i64,
    pub total_output_tokens: i64,
    pub total_runs: i64,
    pub unpriced_runs: i64,
    pub per_model: Vec<ModelCostRow>,
    pub per_provider: Vec<ProviderCostRow>,
    pub per_day: Vec<DailyCostRow>,
}

/// Aggregate model usage from `messages.usage` (authoritative per-message usage)
/// and price it with the reference table. Bounded: window in {7d,30d,all}.
pub async fn usage_summary(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<UsageWindow>,
) -> Result<Json<UsageSummary>, AppError> {
    let _user = require_user(&state, &headers).await?;
    let window_days = match params.window.as_str() {
        "7d" => Some(7_i64),
        "30d" => Some(30_i64),
        "all" => None,
        other => {
            return Err(AppError::Validation(format!(
                "window must be 7d, 30d, or all (got {other})"
            )));
        }
    };
    let since = window_days.map(|days| time::OffsetDateTime::now_utc() - time::Duration::days(days));

    let rows = sqlx::query_as::<_, (String, String, serde_json::Value, time::OffsetDateTime)>(
        "SELECT provider, model, usage, created_at \
         FROM messages \
         WHERE usage IS NOT NULL \
           AND provider IS NOT NULL AND model IS NOT NULL \
           AND ($1::timestamptz IS NULL OR created_at >= $1)",
    )
    .bind(since)
    .fetch_all(&state.pool)
    .await
    .map_err(AppError::Database)?;

    let mut per_model: std::collections::BTreeMap<String, ModelCostRow> = Default::default();
    let mut per_provider: std::collections::BTreeMap<String, ProviderCostRow> = Default::default();
    let mut per_day_map: std::collections::BTreeMap<String, f64> = Default::default();
    let mut total_spend = 0.0_f64;
    let mut total_input = 0_i64;
    let mut total_output = 0_i64;
    let mut unpriced = 0_i64;

    for (provider, model, usage, created_at) in rows {
        let input = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let output = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let (in_price, out_price) = reference_pricing(&provider, &model);
        let spend = input as f64 / 1_000_000.0 * in_price + output as f64 / 1_000_000.0 * out_price;
        if spend == 0.0 {
            unpriced += 1;
        }
        total_spend += spend;
        total_input += input;
        total_output += output;
        let key = format!("{provider}::{model}");
        let entry = per_model.entry(key.clone()).or_insert_with(|| ModelCostRow {
            provider: provider.clone(),
            model: model.clone(),
            input_tokens: 0,
            output_tokens: 0,
            runs: 0,
            spend: 0.0,
        });
        entry.input_tokens += input;
        entry.output_tokens += output;
        entry.runs += 1;
        entry.spend += spend;
        let pentry = per_provider.entry(provider.clone()).or_insert_with(|| ProviderCostRow {
            provider: provider.clone(),
            spend: 0.0,
            runs: 0,
        });
        pentry.spend += spend;
        pentry.runs += 1;
        let day = created_at
            .format(&time::format_description::well_known::Rfc3339)
            .map(|s| s[..10].to_string())
            .unwrap_or_else(|_| "unknown".into());
        *per_day_map.entry(day).or_insert(0.0) += spend;
    }

    let mut per_model_vec = per_model.into_values().collect::<Vec<_>>();
    per_model_vec.sort_by(|a, b| b.spend.partial_cmp(&a.spend).unwrap_or(std::cmp::Ordering::Equal));
    let mut per_provider_vec = per_provider.into_values().collect::<Vec<_>>();
    per_provider_vec.sort_by(|a, b| b.spend.partial_cmp(&a.spend).unwrap_or(std::cmp::Ordering::Equal));
    let per_day_vec = per_day_map.into_iter().map(|(day, spend)| DailyCostRow { day, spend }).collect();

    let total_runs = per_model_vec.iter().map(|r| r.runs).sum::<i64>();

    Ok(Json(UsageSummary {
        total_spend,
        total_input_tokens: total_input,
        total_output_tokens: total_output,
        total_runs,
        unpriced_runs: unpriced,
        per_model: per_model_vec,
        per_provider: per_provider_vec,
        per_day: per_day_vec,
    }))
}

/// Read-only provider catalog for the operator UI dropdown.
pub async fn list_provider_catalog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ProviderCatalogEntry>>, AppError> {
    require_user(&state, &headers).await?;
    Ok(Json(provider_catalog()))
}
