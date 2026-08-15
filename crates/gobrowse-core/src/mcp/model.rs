//! Strict MCP tools, resources, prompts, and content models.

use base64::{Engine as _, engine::general_purpose::STANDARD};
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
    Text {
        text: String,
    },
    Image {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Audio {
        data: String,
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    Resource {
        resource: EmbeddedResource,
    },
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
    validation::validate_identifier(value)
}
fn valid_text(value: &str) -> Result<(), ValidationError> {
    validation::validate_metadata_text(value)
}
fn valid_mime(value: &str) -> Result<(), ValidationError> {
    validation::validate_mime(value)
}

impl ValidateMcp for InitializeParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.protocol_version)?;
        self.capabilities.validate_mcp()?;
        valid_name(&self.client_info.name)?;
        valid_name(&self.client_info.version)
    }
}
impl ValidateMcp for DiscoverResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_collection_len(self.supported_versions.len())?;
        if self.supported_versions.is_empty()
            || self.supported_versions.iter().any(|v| v.is_empty())
        {
            return Err(ValidationError::InvalidValue);
        }
        let mut seen = std::collections::BTreeSet::new();
        if self.supported_versions.iter().any(|version| {
            !seen.insert(version) || super::McpProtocolEra::from_wire_version(version).is_none()
        }) {
            return Err(ValidationError::InvalidValue);
        }
        self.capabilities.validate_mcp()?;
        if let Some(info) = &self.server_info {
            valid_name(&info.name)?;
            valid_name(&info.version)?;
        }
        Ok(())
    }
}
impl ValidateMcp for InitializeResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.protocol_version)?;
        self.capabilities.validate_mcp()?;
        if let Some(instructions) = &self.instructions {
            valid_text(instructions)?;
        }
        valid_name(&self.server_info.name)?;
        valid_name(&self.server_info.version)
    }
}
fn validate_schema(name: &str, value: &Value) -> Result<(), ValidationError> {
    let check = super::validate_tool_schema(name, value);
    if check.status == super::DiagnosticStatus::Fail {
        Err(ValidationError::InvalidValue)
    } else {
        value.validate_mcp()
    }
}
fn validate_blob(value: &str) -> Result<(), ValidationError> {
    if value.len() > validation::MAX_CONTENT_BYTES.saturating_mul(4) / 3 + 4 {
        return Err(ValidationError::ContentTooLarge);
    }
    let decoded = STANDARD
        .decode(value)
        .map_err(|_| ValidationError::InvalidValue)?;
    (decoded.len() <= validation::MAX_CONTENT_BYTES)
        .then_some(())
        .ok_or(ValidationError::ContentTooLarge)
}
impl ValidateMcp for Tool {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        if let Some(title) = &self.title {
            valid_text(title)?;
        }
        if let Some(description) = &self.description {
            valid_text(description)?;
        }
        validate_schema(&self.name, &self.input_schema)?;
        if let Some(value) = &self.output_schema {
            validate_schema(&self.name, value)?;
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
                valid_mime(mime_type)?;
                validate_blob(data)
            }
            Content::Resource { resource } => resource.validate_mcp(),
        }
    }
}
impl ValidateMcp for EmbeddedResource {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)?;
        if let Some(mime_type) = &self.mime_type {
            valid_mime(mime_type)?;
        }
        if let Some(text) = &self.text {
            validation::validate_content_text(text)?;
        }
        if let Some(blob) = &self.blob {
            validate_blob(blob)?;
        }
        Ok(())
    }
}
impl ValidateMcp for Resource {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_uri(&self.uri)?;
        valid_name(&self.name)?;
        if let Some(description) = &self.description {
            valid_text(description)?;
        }
        if let Some(mime_type) = &self.mime_type {
            valid_mime(mime_type)?;
        }
        Ok(())
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
        if let Some(mime_type) = &self.mime_type {
            valid_mime(mime_type)?;
        }
        if let Some(text) = &self.text {
            validation::validate_content_text(text)?;
        }
        if let Some(blob) = &self.blob {
            validate_blob(blob)?;
        }
        Ok(())
    }
}
impl ValidateMcp for Prompt {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        if let Some(description) = &self.description {
            valid_text(description)?;
        }
        validation::validate_collection_len(self.arguments.len())?;
        for argument in &self.arguments {
            valid_name(&argument.name)?;
            if let Some(description) = &argument.description {
                valid_text(description)?;
            }
        }
        Ok(())
    }
}
impl ValidateMcp for PromptGetParams {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.name)?;
        validation::validate_collection_len(self.arguments.len())?;
        for (name, value) in &self.arguments {
            valid_name(name)?;
            valid_text(value)?;
        }
        Ok(())
    }
}
impl ValidateMcp for PromptMessage {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        valid_name(&self.role)?;
        self.content.validate_mcp()
    }
}
impl ValidateMcp for ToolCallResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_collection_len(self.content.len())?;
        self.content
            .iter()
            .try_for_each(ValidateMcp::validate_mcp)?;
        if let Some(value) = &self.structured_content {
            value.validate_mcp()?;
        }
        Ok(())
    }
}
impl ValidateMcp for ResourceReadResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validation::validate_collection_len(self.contents.len())?;
        let mut seen = std::collections::BTreeSet::new();
        for content in &self.contents {
            if !seen.insert(&content.uri) {
                return Err(ValidationError::InvalidValue);
            }
            content.validate_mcp()?;
        }
        Ok(())
    }
}
impl ValidateMcp for PromptGetResult {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        if let Some(description) = &self.description {
            valid_text(description)?;
        }
        validation::validate_collection_len(self.messages.len())?;
        self.messages.iter().try_for_each(ValidateMcp::validate_mcp)
    }
}
fn validate_page_bounds<T: ValidateMcp>(page: &Paginated<T>) -> Result<(), ValidationError> {
    validation::validate_collection_len(page.items.len())?;
    validation::validate_cursor(page.next_cursor.as_deref())?;
    page.items.iter().try_for_each(ValidateMcp::validate_mcp)
}
impl ValidateMcp for Paginated<Tool> {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validate_page_bounds(self)?;
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            if !seen.insert(&item.name) {
                return Err(ValidationError::InvalidValue);
            }
        }
        Ok(())
    }
}
impl ValidateMcp for Paginated<Resource> {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validate_page_bounds(self)?;
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            if !seen.insert(&item.uri) {
                return Err(ValidationError::InvalidValue);
            }
        }
        Ok(())
    }
}
impl ValidateMcp for Paginated<Prompt> {
    fn validate_mcp(&self) -> Result<(), ValidationError> {
        validate_page_bounds(self)?;
        let mut seen = std::collections::BTreeSet::new();
        for item in &self.items {
            if !seen.insert(&item.name) {
                return Err(ValidationError::InvalidValue);
            }
        }
        Ok(())
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
#[serde(rename_all = "camelCase", deny_unknown_fields)]
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
        valid_name(&self.level)?;
        if let Some(logger) = &self.logger {
            valid_name(logger)?;
        }
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
        let image = serde_json::to_value(Content::Image {
            data: "aGVsbG8=".into(),
            mime_type: "image/png".into(),
        })
        .expect("image");
        assert_eq!(image["mimeType"], "image/png");
        assert!(image.get("mime_type").is_none());
        let progress = serde_json::to_value(ProgressParams {
            progress_token: RequestId::String("p".into()),
            progress: 1,
            total: Some(2),
        })
        .expect("progress");
        assert_eq!(progress["progressToken"], "p");
        assert!(progress.get("progress_token").is_none());
        assert!(
            serde_json::from_value::<ProgressParams>(
                serde_json::json!({"progress_token":"p","progress":1})
            )
            .is_err()
        );
        assert!(
            Content::Image {
                data: "not-base64".into(),
                mime_type: "image/png".into()
            }
            .validate_mcp()
            .is_err()
        );
        let duplicate_resources = ResourceReadResult {
            contents: vec![
                ResourceContent {
                    uri: "urn:test".into(),
                    mime_type: None,
                    text: Some("a".into()),
                    blob: None,
                },
                ResourceContent {
                    uri: "urn:test".into(),
                    mime_type: None,
                    text: Some("b".into()),
                    blob: None,
                },
            ],
        };
        assert!(duplicate_resources.validate_mcp().is_err());
        let duplicate_versions = DiscoverResult {
            supported_versions: vec!["2026-07-28".into(), "2026-07-28".into()],
            capabilities: Default::default(),
            server_info: None,
        };
        assert!(duplicate_versions.validate_mcp().is_err());
    }
}
