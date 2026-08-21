use axum::{Json, extract::State};
use serde::Serialize;

use crate::AppState;

/// `GET /api/v1/capabilities` — unauthenticated, always available.
pub async fn get_capabilities(
    State(state): State<AppState>,
) -> Result<Json<CapabilitiesResponse>, crate::error::AppError> {
    // Schema version from DB; fall back to 24 on query failure (tests with no DB).
    let schema_version: i64 =
        sqlx::query_scalar("SELECT schema_version FROM schema_metadata WHERE singleton")
            .fetch_one(&state.pool)
            .await
            .unwrap_or(24);

    let setup_required: bool =
        sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM users WHERE role = 'OWNER')")
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
            .unwrap_or(false);

    // Determine active UI package.
    let ui = {
        let guard = state.active_ui.read().await;
        if let Some(active) = guard.as_ref() {
            UiCapabilities {
                active_package_id: Some(active.package_id),
                active_package_kind: match active.ui_kind {
                    crate::UiKind::Theme => "THEME".to_string(),
                    crate::UiKind::FullUi => "FULL_UI".to_string(),
                },
                permission_level: "ASK".to_string(),
            }
        } else {
            UiCapabilities {
                active_package_id: None,
                active_package_kind: "BUILT_IN".to_string(),
                permission_level: "ASK".to_string(),
            }
        }
    };

    let tool_kinds = state
        .tool_descriptors
        .iter()
        .map(|d| {
            let kind = if d.id.starts_with("sandbox_") || d.id.starts_with("terminal_") {
                "sandbox"
            } else {
                "library"
            };
            ToolCapability {
                name: d.id.clone(),
                kind: kind.to_string(),
            }
        })
        .collect::<Vec<_>>();

    let resp = CapabilitiesResponse {
        api_version: "v1".to_string(),
        schema_version,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        auth: AuthCapabilities {
            methods: vec!["session_cookie".to_string()],
            setup_required,
        },
        features: FeatureCapabilities {
            sandbox: state.settings.features.sandbox,
            browser: state.settings.features.browser,
            messaging: state.settings.features.messaging,
            plugins: true,
            ui_packages: true,
            webhooks: state.settings.features.webhook_scheduler_enabled,
        },
        tools: tool_kinds,
        endpoints: EndpointsCapabilities {
            conversations: EndpointRef {
                base: "/api/v1/conversations".to_string(),
            },
            library: EndpointRef {
                base: "/api/v1/library".to_string(),
            },
        },
        ui,
    };
    Ok(Json(resp))
}

#[derive(Debug, Serialize)]
pub struct CapabilitiesResponse {
    pub api_version: String,
    pub schema_version: i64,
    pub server_version: String,
    pub auth: AuthCapabilities,
    pub features: FeatureCapabilities,
    pub tools: Vec<ToolCapability>,
    pub endpoints: EndpointsCapabilities,
    pub ui: UiCapabilities,
}

#[derive(Debug, Serialize)]
pub struct AuthCapabilities {
    pub methods: Vec<String>,
    pub setup_required: bool,
}

#[derive(Debug, Serialize)]
pub struct FeatureCapabilities {
    pub sandbox: bool,
    pub browser: bool,
    pub messaging: bool,
    pub plugins: bool,
    pub ui_packages: bool,
    pub webhooks: bool,
}

#[derive(Debug, Serialize)]
pub struct ToolCapability {
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct EndpointsCapabilities {
    pub conversations: EndpointRef,
    pub library: EndpointRef,
}

#[derive(Debug, Serialize)]
pub struct EndpointRef {
    pub base: String,
}

#[derive(Debug, Serialize)]
pub struct UiCapabilities {
    pub active_package_id: Option<uuid::Uuid>,
    pub active_package_kind: String,
    pub permission_level: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_response_serializes() {
        let resp = CapabilitiesResponse {
            api_version: "v1".into(),
            schema_version: 24,
            server_version: "0.1.0".into(),
            auth: AuthCapabilities {
                methods: vec!["session_cookie".into()],
                setup_required: false,
            },
            features: FeatureCapabilities {
                sandbox: false,
                browser: false,
                messaging: false,
                plugins: true,
                ui_packages: true,
                webhooks: true,
            },
            tools: vec![ToolCapability {
                name: "library_search".into(),
                kind: "library".into(),
            }],
            endpoints: EndpointsCapabilities {
                conversations: EndpointRef {
                    base: "/api/v1/conversations".into(),
                },
                library: EndpointRef {
                    base: "/api/v1/library".into(),
                },
            },
            ui: UiCapabilities {
                active_package_id: None,
                active_package_kind: "BUILT_IN".into(),
                permission_level: "ASK".into(),
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["api_version"], "v1");
        assert_eq!(json["ui"]["active_package_kind"], "BUILT_IN");
    }
}
