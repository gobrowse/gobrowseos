//! Strict capability negotiation and method authorization for the MCP core surface.

use super::validation::{ValidateMcp, ValidationError};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<RootsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<SamplingCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourcesCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<PromptsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logging: Option<LoggingCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RootsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamplingCapability {}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourcesCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscribe: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptsCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list_changed: Option<bool>,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingCapability {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CapabilityError {
    #[error("MCP method is not supported by negotiated server capabilities")]
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityMethod {
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourcesRead,
    ResourcesSubscribe,
    ResourcesUnsubscribe,
    PromptsList,
    PromptsGet,
    Ping,
}
impl CapabilityMethod {
    pub fn allowed(self, capabilities: &ServerCapabilities) -> bool {
        match self {
            Self::ToolsList | Self::ToolsCall => capabilities.tools.is_some(),
            Self::ResourcesList | Self::ResourcesRead => capabilities.resources.is_some(),
            Self::ResourcesSubscribe | Self::ResourcesUnsubscribe => capabilities
                .resources
                .as_ref()
                .and_then(|value| value.subscribe)
                .unwrap_or(false),
            Self::PromptsList | Self::PromptsGet => capabilities.prompts.is_some(),
            Self::Ping => true,
        }
    }
    pub fn require(self, capabilities: &ServerCapabilities) -> Result<(), CapabilityError> {
        self.allowed(capabilities)
            .then_some(())
            .ok_or(CapabilityError::Unsupported)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityNotification {
    ToolsListChanged,
    ResourcesListChanged,
    ResourceUpdated,
    Progress,
    Logging,
}
impl ValidateMcp for ClientCapabilities {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        if let Some(value) = &self.experimental {
            value.validate_mcp()?;
        }
        Ok(())
    }
}
impl ValidateMcp for ServerCapabilities {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        if let Some(value) = &self.experimental {
            value.validate_mcp()?;
        }
        Ok(())
    }
}

pub fn notification_allowed(
    notification: CapabilityNotification,
    capabilities: &ServerCapabilities,
) -> bool {
    match notification {
        CapabilityNotification::ToolsListChanged => capabilities
            .tools
            .as_ref()
            .and_then(|c| c.list_changed)
            .unwrap_or(false),
        CapabilityNotification::ResourcesListChanged => capabilities
            .resources
            .as_ref()
            .and_then(|c| c.list_changed)
            .unwrap_or(false),
        CapabilityNotification::ResourceUpdated => capabilities
            .resources
            .as_ref()
            .and_then(|c| c.subscribe)
            .unwrap_or(false),
        CapabilityNotification::Progress => false,
        CapabilityNotification::Logging => capabilities.logging.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gates_methods_and_serializes_list_changed() {
        let capabilities = ServerCapabilities {
            tools: Some(ToolsCapability::default()),
            resources: Some(ResourcesCapability {
                subscribe: Some(false),
                list_changed: Some(true),
            }),
            ..Default::default()
        };
        assert!(CapabilityMethod::ToolsCall.allowed(&capabilities));
        assert!(!CapabilityMethod::PromptsList.allowed(&capabilities));
        assert!(!CapabilityMethod::ResourcesSubscribe.allowed(&capabilities));
        assert_eq!(
            serde_json::to_value(capabilities.resources.expect("resources")).expect("json")["listChanged"],
            true
        );
    }
}
