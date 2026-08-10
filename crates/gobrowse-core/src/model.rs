use std::{collections::BTreeSet, pin::Pin, sync::Arc};

use async_trait::async_trait;
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Text,
    Vision,
    Audio,
    Files,
    ToolCalls,
    Reasoning,
    StructuredOutput,
    ParallelTools,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelIdentity {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisteredModel {
    pub id: String,
    pub identity: ModelIdentity,
    pub display_name: String,
    pub context_window: u32,
    pub output_limit: u32,
    pub capabilities: BTreeSet<ModelCapability>,
    pub enabled: bool,
    pub priority: i32,
}

impl RegisteredModel {
    pub fn supports(&self, required: &BTreeSet<ModelCapability>) -> bool {
        required.is_subset(&self.capabilities)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeutralMessage {
    pub role: MessageRole,
    pub content: Vec<ContentPart>,
    pub provider_provenance: Option<ModelIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageReference {
        artifact_id: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        call_id: String,
        output: serde_json::Value,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: ModelIdentity,
    pub messages: Vec<NeutralMessage>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
    pub required_capabilities: BTreeSet<ModelCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub id: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    TextDelta {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cached_tokens: u64,
    },
    Completed,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProviderError {
    #[error("provider authentication failed")]
    InvalidCredentials,
    #[error("provider rate limit")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("provider temporarily unavailable")]
    TemporaryUnavailable,
    #[error("model does not support required capability")]
    UnsupportedCapability,
    #[error("request exceeds model context window")]
    ContextLimit,
    #[error("provider request timed out")]
    Timeout,
    #[error("provider returned an invalid response")]
    InvalidResponse,
    #[error("request was canceled")]
    Canceled,
}

impl ProviderError {
    pub fn allows_fallback(&self) -> bool {
        matches!(
            self,
            Self::InvalidCredentials
                | Self::RateLimited { .. }
                | Self::TemporaryUnavailable
                | Self::UnsupportedCapability
                | Self::ContextLimit
                | Self::Timeout
        )
    }
}

pub type ModelStream = Pin<Box<dyn Stream<Item = Result<ModelEvent, ProviderError>> + Send>>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn id(&self) -> &str;
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError>;
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn id(&self) -> &str;
    fn dimensions(&self) -> usize;
    async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>, ProviderError>;
}

#[async_trait]
pub trait RerankProvider: Send + Sync {
    async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<f32>, ProviderError>;
}

#[async_trait]
pub trait ImageProvider: Send + Sync {
    async fn generate(&self, prompt: &str) -> Result<Vec<u8>, ProviderError>;
}

#[async_trait]
pub trait SpeechProvider: Send + Sync {
    async fn transcribe(&self, audio: &[u8]) -> Result<String, ProviderError>;
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>, ProviderError>;
}

#[derive(Clone)]
pub struct ModelRoute {
    pub provider: Arc<dyn ModelProvider>,
    pub identity: ModelIdentity,
}

/// Opens a model stream using the first route that accepts the request.
///
/// Fallback applies only before a stream is returned. A mid-stream failure is surfaced because
/// the provider may already have emitted content or committed a tool call.
pub async fn open_with_fallback(
    routes: &[ModelRoute],
    request: &ModelRequest,
) -> Result<(ModelIdentity, ModelStream), ProviderError> {
    let mut last_error = None;
    for route in routes {
        let mut routed_request = request.clone();
        routed_request.model = route.identity.clone();
        match route.provider.stream(routed_request).await {
            Ok(stream) => return Ok((route.identity.clone(), stream)),
            Err(error) if error.allows_fallback() => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }
    Err(last_error.unwrap_or(ProviderError::TemporaryUnavailable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_check_is_explicit() {
        let model = RegisteredModel {
            id: "local/test".into(),
            identity: ModelIdentity {
                provider: "local".into(),
                model: "test".into(),
            },
            display_name: "Test".into(),
            context_window: 8192,
            output_limit: 1024,
            capabilities: BTreeSet::from([ModelCapability::Text]),
            enabled: true,
            priority: 0,
        };
        assert!(model.supports(&BTreeSet::from([ModelCapability::Text])));
        assert!(!model.supports(&BTreeSet::from([ModelCapability::ToolCalls])));
    }

    #[test]
    fn invalid_responses_are_not_retried_as_fallbacks() {
        assert!(!ProviderError::InvalidResponse.allows_fallback());
        assert!(ProviderError::TemporaryUnavailable.allows_fallback());
    }
}
