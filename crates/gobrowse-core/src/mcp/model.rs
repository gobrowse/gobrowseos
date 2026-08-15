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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptRole {
    User,
    Assistant,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptMessage {
    pub role: PromptRole,
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
            .map_err(|_| ValidationError::InvalidValue)?;
        if let Some(reason) = &self.reason {
            valid_text(reason)?;
        }
        Ok(())
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoggingLevel {
    Debug,
    Info,
    Notice,
    Warning,
    Error,
    Critical,
    Alert,
    Emergency,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LoggingMessageParams {
    pub level: LoggingLevel,
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
        let cancelled = |reason: Option<String>| CancelledParams {
            request_id: RequestId::Number(1),
            reason,
        };
        assert!(cancelled(None).validate_mcp().is_ok());
        assert!(
            cancelled(Some("x".repeat(validation::MAX_METADATA_TEXT_BYTES)))
                .validate_mcp()
                .is_ok()
        );
        assert!(
            cancelled(Some("x".repeat(validation::MAX_METADATA_TEXT_BYTES + 1)))
                .validate_mcp()
                .is_err()
        );
        assert!(cancelled(Some(String::new())).validate_mcp().is_err());
        assert_eq!(
            serde_json::from_str::<PromptRole>("\"user\"").unwrap(),
            PromptRole::User
        );
        assert!(serde_json::from_str::<PromptRole>("\"system\"").is_err());
        assert_eq!(
            serde_json::from_str::<LoggingLevel>("\"emergency\"").unwrap(),
            LoggingLevel::Emergency
        );
        assert!(serde_json::from_str::<LoggingLevel>("\"trace\"").is_err());
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

    #[test]
    fn modern_discover_and_legacy_initialize_fixtures_have_exact_wire_keys() {
        let discover = DiscoverResult {
            supported_versions: vec!["2026-07-28".into(), "2025-11-25".into()],
            capabilities: ServerCapabilities::default(),
            server_info: Some(ClientInfo {
                name: "server".into(),
                version: "1".into(),
            }),
        };
        let discover_json = serde_json::to_value(&discover).expect("discover");
        assert_eq!(
            discover_json["supportedVersions"],
            serde_json::json!(["2026-07-28", "2025-11-25"])
        );
        assert!(discover_json.get("supported_versions").is_none());
        assert!(
            serde_json::from_value::<DiscoverResult>(serde_json::json!({
                "supported_versions":["2026-07-28"], "capabilities":{}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<DiscoverResult>(serde_json::json!({
                "supportedVersions":["2026-07-28"], "capabilities":{}, "unknown":true
            }))
            .is_err()
        );
        assert!(discover.validate_mcp().is_ok());

        let initialize = InitializeResult {
            protocol_version: "2025-11-25".into(),
            capabilities: ServerCapabilities::default(),
            server_info: ClientInfo {
                name: "server".into(),
                version: "1".into(),
            },
            instructions: Some("read this".into()),
        };
        let initialize_json = serde_json::to_value(&initialize).expect("initialize");
        for key in ["protocolVersion", "serverInfo"] {
            assert!(initialize_json.get(key).is_some(), "{key}");
        }
        assert!(initialize_json.get("protocol_version").is_none());
        assert!(
            serde_json::from_value::<InitializeResult>(serde_json::json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "serverInfo":{"name":"server","version":"1"}, "server_info":{}
            }))
            .is_err()
        );
        assert!(initialize.validate_mcp().is_ok());
    }

    #[test]
    fn model_limits_vocabularies_and_page_identity_matrix_is_bounded() {
        assert!(valid_name(&"x".repeat(validation::MAX_IDENTIFIER_BYTES)).is_ok());
        assert!(valid_name(&"x".repeat(validation::MAX_IDENTIFIER_BYTES + 1)).is_err());
        assert!(valid_text(&"x".repeat(validation::MAX_METADATA_TEXT_BYTES)).is_ok());
        assert!(valid_text(&"x".repeat(validation::MAX_METADATA_TEXT_BYTES + 1)).is_err());
        assert!(valid_mime(&"x".repeat(validation::MAX_MIME_BYTES)).is_ok());
        assert!(valid_mime(&"x".repeat(validation::MAX_MIME_BYTES + 1)).is_err());
        assert!(
            validation::validate_cursor(Some(&"x".repeat(validation::MAX_CURSOR_BYTES))).is_ok()
        );
        assert!(
            validation::validate_cursor(Some(&"x".repeat(validation::MAX_CURSOR_BYTES + 1)))
                .is_err()
        );
        assert!(validation::validate_uri(&"x".repeat(validation::MAX_URI_BYTES)).is_ok());
        assert!(validation::validate_uri(&"x".repeat(validation::MAX_URI_BYTES + 1)).is_err());

        let valid_schema = serde_json::json!({"type":"object"});
        let tool = |name: &str| Tool {
            name: name.into(),
            title: None,
            description: None,
            input_schema: valid_schema.clone(),
            output_schema: None,
        };
        let exact_tools = (0..validation::MAX_ITEMS)
            .map(|index| tool(&format!("tool{index}")))
            .collect();
        assert!(
            Paginated {
                items: exact_tools,
                next_cursor: None
            }
            .validate_mcp()
            .is_ok()
        );
        let over_tools = (0..=validation::MAX_ITEMS)
            .map(|index| tool(&format!("tool{index}")))
            .collect();
        assert!(
            Paginated {
                items: over_tools,
                next_cursor: None
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            Paginated {
                items: vec![tool("a"), tool("a")],
                next_cursor: None
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            Paginated {
                items: vec![tool("a")],
                next_cursor: Some("".into())
            }
            .validate_mcp()
            .is_err()
        );

        let exact_blob = STANDARD.encode(vec![b'x'; validation::MAX_CONTENT_BYTES]);
        assert!(
            Content::Image {
                data: exact_blob,
                mime_type: "image/png".into()
            }
            .validate_mcp()
            .is_ok()
        );
        let oversized_blob = STANDARD.encode(vec![b'x'; validation::MAX_CONTENT_BYTES + 1]);
        assert!(
            Content::Image {
                data: oversized_blob,
                mime_type: "image/png".into()
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            Content::Text {
                text: "x".repeat(validation::MAX_CONTENT_BYTES)
            }
            .validate_mcp()
            .is_ok()
        );
        assert!(
            Content::Text {
                text: "x".repeat(validation::MAX_CONTENT_BYTES + 1)
            }
            .validate_mcp()
            .is_err()
        );

        for role in ["user", "assistant"] {
            assert!(serde_json::from_str::<PromptRole>(&format!("\"{role}\"")).is_ok());
            assert!(
                serde_json::from_str::<PromptRole>(&format!("\"{}\"", role.to_uppercase()))
                    .is_err()
            );
        }
        for level in [
            "debug",
            "info",
            "notice",
            "warning",
            "error",
            "critical",
            "alert",
            "emergency",
        ] {
            assert!(serde_json::from_str::<LoggingLevel>(&format!("\"{level}\"")).is_ok());
            assert!(
                serde_json::from_str::<LoggingLevel>(&format!("\"{}\"", level.to_uppercase()))
                    .is_err()
            );
        }
        for invalid in [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!("unknown"),
        ] {
            assert!(serde_json::from_value::<PromptRole>(invalid.clone()).is_err());
            assert!(serde_json::from_value::<LoggingLevel>(invalid).is_err());
        }
    }

    #[test]
    fn content_progress_and_schema_matrix_rejects_invalid_shapes() {
        assert!(
            serde_json::from_value::<Content>(serde_json::json!({
                "type":"text", "text":"ok", "extra":true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<Content>(serde_json::json!({
                "type":"image", "data":"aA==", "mimeType":"image/png", "mime_type":"x"
            }))
            .is_err()
        );
        assert!(
            Content::Image {
                data: "not base64".into(),
                mime_type: "image/png".into()
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            ResourceReadResult {
                contents: vec![
                    ResourceContent {
                        uri: "urn:a".into(),
                        mime_type: None,
                        text: Some("a".into()),
                        blob: None
                    },
                    ResourceContent {
                        uri: "urn:a".into(),
                        mime_type: None,
                        text: Some("b".into()),
                        blob: None
                    },
                ]
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            ProgressParams {
                progress_token: RequestId::String("".into()),
                progress: 1,
                total: None
            }
            .validate_mcp()
            .is_err()
        );
        assert!(
            LoggingMessageParams {
                level: LoggingLevel::Info,
                data: serde_json::Value::Null,
                logger: Some("".into())
            }
            .validate_mcp()
            .is_err()
        );
        assert_eq!(
            super::validate_schema(
                "tool",
                &serde_json::json!({"$ref":"https://example.invalid/schema"})
            ),
            Err(ValidationError::InvalidValue)
        );
    }
}
