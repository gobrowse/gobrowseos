//! Strict MCP tools, resources, prompts, and content models.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    capabilities::{ClientCapabilities, ServerCapabilities},
    validation::{self, ValidateMcp, ValidationError},
    wire::RequestId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiscoverResult {
    pub supported_versions: Vec<String>,
    #[serde(default)]
    pub capabilities: ServerCapabilities,
    #[serde(default)]
    pub server_info: Option<ClientInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    pub server_info: ClientInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolCallResult {
    pub content: Vec<Content>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Content {
    Text { text: String },
    Image { data: String, mime_type: String },
    Audio { data: String, mime_type: String },
    Resource { resource: EmbeddedResource },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddedResource {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReadParams {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceReadResult {
    pub contents: Vec<ResourceContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceContent {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<PromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptGetParams {
    pub name: String,
    #[serde(default)]
    pub arguments: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptGetResult {
    #[serde(default)]
    pub description: Option<String>,
    pub messages: Vec<PromptMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptMessage {
    pub role: String,
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Paginated<T> {
    pub items: Vec<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ListParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelledParams {
    pub request_id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceSubscriptionParams {
    pub uri: String,
}

fn valid_name(value: &str) -> Result<(), ValidationError> {
    (!value.is_empty() && value.len() <= validation::MAX_URI_BYTES)
        .then_some(())
        .ok_or(ValidationError::InvalidValue)
}

impl ValidateMcp for InitializeParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.protocol_version)?;
        valid_name(&self.client_info.name)
    }
}
impl ValidateMcp for DiscoverResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_collection_len(self.supported_versions.len())?;
        if self.supported_versions.is_empty() {
            return Err(ValidationError::InvalidValue);
        }
        Ok(())
    }
}
impl ValidateMcp for Tool {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        self.input_schema.validate_mcp()?;
        if let Some(value) = &self.output_schema {
            value.validate_mcp()?;
        }
        Ok(())
    }
}
impl ValidateMcp for ToolCallParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        self.arguments.validate_mcp()
    }
}
impl ValidateMcp for Content {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        match self {
            Content::Text { text } => validation::validate_content_text(text),
            Content::Image { data, mime_type } | Content::Audio { data, mime_type } => {
                valid_name(mime_type)?;
                validation::validate_content_text(data)
            }
            Content::Resource { resource } => resource.validate_mcp(),
        }
    }
}
impl ValidateMcp for EmbeddedResource {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)?;
        if let Some(text) = &self.text {
            validation::validate_content_text(text)?;
        }
        if let Some(blob) = &self.blob {
            validation::validate_content_text(blob)?;
        }
        Ok(())
    }
}
impl ValidateMcp for Resource {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)?;
        valid_name(&self.name)
    }
}
impl ValidateMcp for ResourceReadParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)
    }
}
impl ValidateMcp for ResourceContent {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)?;
        if let Some(text) = &self.text {
            validation::validate_content_text(text)?;
        }
        Ok(())
    }
}
impl ValidateMcp for Prompt {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        validation::validate_collection_len(self.arguments.len())
    }
}
impl ValidateMcp for PromptGetParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        validation::validate_collection_len(self.arguments.len())
    }
}
impl ValidateMcp for PromptMessage {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        self.content.validate_mcp()
    }
}
impl<T: ValidateMcp> ValidateMcp for Paginated<T> {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_collection_len(self.items.len())?;
        validation::validate_cursor(self.next_cursor.as_deref())?;
        self.items.iter().try_for_each(ValidateMcp::validate_mcp)
    }
}
impl ValidateMcp for ListParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_cursor(self.cursor.as_deref())
    }
}
impl ValidateMcp for CancelledParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        self.request_id
            .validate()
            .map_err(|_| ValidationError::InvalidValue)
    }
}
impl ValidateMcp for ResourceSubscriptionParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUpdatedParams {
    pub uri: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgressParams {
    pub progress_token: RequestId,
    pub progress: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoggingMessageParams {
    pub level: String,
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logger: Option<String>,
}
impl ValidateMcp for ResourceUpdatedParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)
    }
}
impl ValidateMcp for ProgressParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        self.progress_token
            .validate()
            .map_err(|_| ValidationError::InvalidValue)
    }
}
impl ValidateMcp for LoggingMessageParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        self.data.validate_mcp()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uses_wire_camel_case_and_rejects_unknown_fields() {
        let params = InitializeParams {
            protocol_version: "2025-11-25".into(),
            capabilities: Default::default(),
            client_info: ClientInfo {
                name: "x".into(),
                version: "1".into(),
            },
        };
        let json = serde_json::to_value(&params).expect("serialize");
        assert!(json.get("protocolVersion").is_some());
        assert!(json.get("protocol_version").is_none());
        assert!(serde_json::from_value::<InitializeParams>(serde_json::json!({"protocolVersion":"x","capabilities":{},"clientInfo":{"name":"x","version":"1"},"extra":true})).is_err());
    }
}
