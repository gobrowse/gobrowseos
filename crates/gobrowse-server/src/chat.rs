use std::{
    collections::{BTreeSet, VecDeque},
    pin::Pin,
    sync::Arc,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt, stream};
use gobrowse_core::model::{
    ContentPart, MessageRole, ModelCapability, ModelEvent, ModelIdentity, ModelProvider,
    ModelRequest, ModelRoute, ModelStream, NeutralMessage, ProviderError, ToolDefinition,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use sqlx::Row;
use tracing::warn;
use url::Url;
use uuid::Uuid;

use crate::{AppState, embedding, error::AppError};

struct HttpChatProvider {
    id: String,
    provider_type: String,
    base_url: Url,
    token: Option<SecretString>,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDef>,
}

#[derive(Serialize)]
struct ToolDef {
    #[serde(rename = "type")]
    type_: String,
    function: ToolFunction,
}

#[derive(Serialize)]
struct ToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct OllamaOptions {
    num_predict: u32,
}

#[derive(Serialize)]
struct ChatMessage {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<AssistantToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize)]
struct AssistantToolCall {
    id: String,
    #[serde(rename = "type")]
    type_: String,
    function: ToolCallFunction,
}

#[derive(Serialize)]
struct ToolCallFunction {
    name: String,
    arguments: String,
}

struct ProviderStreamState {
    stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: Vec<u8>,
    pending: VecDeque<ModelEvent>,
    provider_type: String,
    sse_data: String,
    received_bytes: usize,
    completed: bool,
}

/// Maps neutral conversation messages onto provider wire messages. Tool
/// results fan out: one wire message per [`ContentPart::ToolResult`] so that
/// parallel tool calls in a single round each get a matching result message
/// (providers reject histories where an assistant tool_call id is missing
/// its result).
fn to_wire_messages(messages: &[NeutralMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .flat_map(|message| -> Vec<ChatMessage> {
            let role = role_name(message.role);
            match message.role {
                MessageRole::Tool => {
                    let mut wire_messages = Vec::new();
                    for part in &message.content {
                        if let ContentPart::ToolResult {
                            call_id, output, ..
                        } = part
                        {
                            wire_messages.push(ChatMessage {
                                role,
                                content: Some(output.to_string()),
                                tool_calls: None,
                                tool_call_id: Some(call_id.clone()),
                            });
                        }
                    }
                    if wire_messages.is_empty() {
                        wire_messages.push(ChatMessage {
                            role,
                            content: Some(String::new()),
                            tool_calls: None,
                            tool_call_id: None,
                        });
                    }
                    wire_messages
                }
                MessageRole::Assistant => {
                    let tool_calls: Vec<_> = message
                        .content
                        .iter()
                        .filter_map(|part| {
                            if let ContentPart::ToolCall { id, name, input } = part {
                                Some(AssistantToolCall {
                                    id: id.clone(),
                                    type_: "function".into(),
                                    function: ToolCallFunction {
                                        name: name.clone(),
                                        arguments: input.to_string(),
                                    },
                                })
                            } else {
                                None
                            }
                        })
                        .collect();
                    vec![ChatMessage {
                        role,
                        content: Some(content_text(&message.content)),
                        tool_calls: if tool_calls.is_empty() {
                            None
                        } else {
                            Some(tool_calls)
                        },
                        tool_call_id: None,
                    }]
                }
                _ => vec![ChatMessage {
                    role,
                    content: Some(content_text(&message.content)),
                    tool_calls: None,
                    tool_call_id: None,
                }],
            }
        })
        .collect()
}

#[async_trait]
impl ModelProvider for HttpChatProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, request: ModelRequest) -> Result<ModelStream, ProviderError> {
        let messages = to_wire_messages(&request.messages);
        let path = if self.provider_type == "ollama" {
            "api/chat"
        } else {
            "chat/completions"
        };
        let endpoint = endpoint(&self.base_url, path)?;
        let tools: Vec<ToolDef> = request
            .tools
            .iter()
            .map(|def| ToolDef {
                type_: "function".into(),
                function: ToolFunction {
                    name: def.id.clone(),
                    description: def.description.clone(),
                    parameters: def.input_schema.clone(),
                },
            })
            .collect();
        let body = ChatRequest {
            model: &request.model.model,
            messages: &messages,
            stream: true,
            max_tokens: (self.provider_type != "ollama").then_some(request.max_output_tokens),
            options: (self.provider_type == "ollama").then_some(OllamaOptions {
                num_predict: request.max_output_tokens,
            }),
            tools,
        };
        let mut outgoing = self.http.post(endpoint).json(&body);
        if let Some(token) = &self.token {
            outgoing = outgoing.bearer_auth(token.expose_secret());
        }
        let response = outgoing.send().await.map_err(map_transport_error)?;
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
        ) {
            return Err(ProviderError::InvalidCredentials);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_seconds = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse().ok());
            return Err(ProviderError::RateLimited {
                retry_after_seconds,
            });
        }
        if status.is_server_error() {
            return Err(ProviderError::TemporaryUnavailable);
        }
        if !status.is_success() {
            return Err(ProviderError::InvalidResponse);
        }
        let state = ProviderStreamState {
            stream: Box::pin(response.bytes_stream()),
            buffer: Vec::new(),
            pending: VecDeque::new(),
            provider_type: self.provider_type.clone(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        Ok(Box::pin(stream::unfold(state, next_provider_event)))
    }
}

pub async fn load_routes(
    state: &AppState,
    profile_id: Uuid,
    requested_model_id: Option<&str>,
) -> Result<Vec<ModelRoute>, AppError> {
    let rows = sqlx::query(
        "WITH primary_model AS ( \
             SELECT m.id FROM models m JOIN providers p ON p.id=m.provider_id \
             JOIN profiles profile ON profile.id=p.profile_id \
             WHERE p.profile_id=$1 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities) \
               AND ($2::text IS NULL OR m.id=$2) \
             ORDER BY (m.id=profile.active_chat_model_id) DESC,m.priority DESC,m.id LIMIT 1 \
         ), route_ids AS ( \
             SELECT id AS model_id,0 AS position FROM primary_model \
             UNION ALL \
             SELECT route.fallback_model_id,route.position+1 FROM model_fallback_routes route \
             JOIN primary_model ON primary_model.id=route.primary_model_id WHERE route.profile_id=$1 \
         ) \
         SELECT m.id,m.model_reference,m.capabilities,p.id AS provider_id,p.provider_type,p.base_url,p.secret_reference,m.cost_ranking \
         FROM route_ids route JOIN models m ON m.id=route.model_id JOIN providers p ON p.id=m.provider_id \
         WHERE p.profile_id=$1 AND p.enabled AND m.enabled AND 'text'=ANY(m.capabilities) ORDER BY (route.position = 0) DESC, m.cost_ranking ASC NULLS LAST, route.position",
    )
    .bind(profile_id)
    .bind(requested_model_id)
    .fetch_all(&state.pool)
    .await?;
    let mut routes = Vec::with_capacity(rows.len());
    for row in rows {
        let model_id: String = row.get("id");
        let provider_type: String = row.get("provider_type");
        // Catalog providers (opencode-go, openai-codex, openrouter, openai,
        // deepseek, mistral, xai, openai_compatible) all use the OpenAI chat
        // completions wire format; ollama uses its local /api/chat format.
        if provider_type != "ollama"
            && !matches!(
                provider_type.as_str(),
                "openai_compatible"
                    | "opencode-go"
                    | "openai-codex"
                    | "openrouter"
                    | "openai"
                    | "deepseek"
                    | "mistral"
                    | "xai"
            )
        {
            continue;
        }
        let base_url_value: Option<String> = row.try_get("base_url")?;
        let Some(base_url_value) = base_url_value else {
            warn!(model_id, "skipping chat route without base URL");
            continue;
        };
        let Ok(base_url) = Url::parse(&base_url_value) else {
            warn!(model_id, "skipping chat route with invalid base URL");
            continue;
        };
        let secret_reference: Option<String> = row.get("secret_reference");
        let token = match secret_reference {
            Some(secret_id) => {
                let Some(host) = base_url.host_str() else {
                    warn!(model_id, "skipping chat route without provider host");
                    continue;
                };
                match state
                    .vault
                    .resolve_for_provider(&state.pool, profile_id, &secret_id, host)
                    .await
                {
                    Ok(token) => Some(token),
                    Err(error) => {
                        warn!(model_id, error=%error, "skipping chat route with unavailable credential");
                        continue;
                    }
                }
            }
            None => None,
        };
        let http = match embedding::provider_http_client(
            &base_url,
            &provider_type,
            token.is_some(),
            state.settings.features.local_models,
            None,
        )
        .await
        {
            Ok(http) => http,
            Err(error) => {
                warn!(model_id, error=%error, "skipping chat route rejected by network policy");
                continue;
            }
        };
        let provider_id: String = row.get("provider_id");
        let model_reference: String = row.get("model_reference");
        let capabilities: Vec<String> = row.get("capabilities");
        let cost_ranking: f32 = row.get("cost_ranking");
        let supports_tools = capabilities.iter().any(|c| c == "tool_calls");
        routes.push(ModelRoute {
            supports_tools,
            cost_ranking,
            provider: Arc::new(HttpChatProvider {
                id: provider_id.clone(),
                provider_type,
                base_url,
                token,
                http,
            }),
            identity: ModelIdentity {
                provider: provider_id,
                model: model_reference,
            },
        });
    }
    Ok(routes)
}

pub fn request(
    messages: Vec<gobrowse_core::model::NeutralMessage>,
    tools: Vec<ToolDefinition>,
    max_output_tokens: u32,
) -> ModelRequest {
    let required_capabilities = if tools.is_empty() {
        BTreeSet::new()
    } else {
        BTreeSet::from([ModelCapability::ToolCalls])
    };
    ModelRequest {
        model: ModelIdentity {
            provider: String::new(),
            model: String::new(),
        },
        messages,
        tools,
        max_output_tokens,
        required_capabilities,
    }
}

fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::System => "system",
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::Tool => "tool",
    }
}

fn content_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.clone()),
            ContentPart::ToolResult { output, .. } => Some(output.to_string()),
            ContentPart::ImageReference { .. } | ContentPart::ToolCall { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn endpoint(base: &Url, path: &str) -> Result<Url, ProviderError> {
    let mut base = base.clone();
    if !base.path().ends_with('/') {
        base.set_path(&format!("{}/", base.path()));
    }
    base.join(path).map_err(|_| ProviderError::InvalidResponse)
}

fn map_transport_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::TemporaryUnavailable
    }
}

async fn next_provider_event(
    mut state: ProviderStreamState,
) -> Option<(Result<ModelEvent, ProviderError>, ProviderStreamState)> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Some((Ok(event), state));
        }
        if state.completed {
            return None;
        }
        if let Some(newline) = state.buffer.iter().position(|byte| *byte == b'\n') {
            let line = state.buffer.drain(..=newline).collect::<Vec<_>>();
            match parse_provider_line(&mut state, &line) {
                Ok(Some(event)) => return Some((Ok(event), state)),
                Ok(None) => continue,
                Err(error) => {
                    state.completed = true;
                    return Some((Err(error), state));
                }
            }
        }
        match state.stream.next().await {
            Some(Ok(chunk)) => {
                state.received_bytes = state.received_bytes.saturating_add(chunk.len());
                if state.received_bytes > 4 * 1024 * 1024 {
                    state.completed = true;
                    return Some((Err(ProviderError::InvalidResponse), state));
                }
                state.buffer.extend_from_slice(&chunk);
            }
            Some(Err(error)) => {
                state.completed = true;
                return Some((Err(map_transport_error(error)), state));
            }
            None if !state.buffer.is_empty() => {
                let line = std::mem::take(&mut state.buffer);
                match parse_provider_line(&mut state, &line) {
                    Ok(Some(event)) => return Some((Ok(event), state)),
                    Ok(None) if state.completed => continue,
                    Ok(None) | Err(_) => {
                        state.completed = true;
                        return Some((Err(ProviderError::InvalidResponse), state));
                    }
                }
            }
            None if !state.sse_data.is_empty() => match parse_provider_line(&mut state, b"") {
                Ok(Some(event)) => return Some((Ok(event), state)),
                Ok(None) if state.completed => continue,
                Ok(None) | Err(_) => {
                    state.completed = true;
                    return Some((Err(ProviderError::InvalidResponse), state));
                }
            },
            None if state.completed => return None,
            None => {
                state.completed = true;
                return Some((Err(ProviderError::InvalidResponse), state));
            }
        }
    }
}

fn parse_provider_line(
    state: &mut ProviderStreamState,
    line: &[u8],
) -> Result<Option<ModelEvent>, ProviderError> {
    let line = std::str::from_utf8(line)
        .map_err(|_| ProviderError::InvalidResponse)?
        .trim();
    let payload = if state.provider_type == "ollama" {
        if line.is_empty() {
            return Ok(None);
        }
        line.to_owned()
    } else {
        if let Some(data) = line.strip_prefix("data:").map(str::trim_start) {
            if !state.sse_data.is_empty() {
                state.sse_data.push('\n');
            }
            state.sse_data.push_str(data);
            return Ok(None);
        }
        if !line.is_empty() {
            return Ok(None);
        }
        let payload = std::mem::take(&mut state.sse_data);
        if payload.is_empty() {
            return Ok(None);
        }
        if payload == "[DONE]" {
            state.completed = true;
            return Ok(Some(ModelEvent::Completed));
        }
        payload
    };
    let value: serde_json::Value =
        serde_json::from_str(&payload).map_err(|_| ProviderError::InvalidResponse)?;
    if state.provider_type == "ollama" {
        let text = value["message"]["content"].as_str().unwrap_or_default();
        if value["done"].as_bool().unwrap_or(false) {
            state.pending.push_back(ModelEvent::Usage {
                input_tokens: value["prompt_eval_count"].as_u64().unwrap_or(0),
                output_tokens: value["eval_count"].as_u64().unwrap_or(0),
                cached_tokens: 0,
            });
            state.pending.push_back(ModelEvent::Completed);
            state.completed = true;
        }
        return if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(ModelEvent::TextDelta { text: text.into() }))
        };
    }
    if let Some(usage) = value.get("usage") {
        state.pending.push_back(ModelEvent::Usage {
            input_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
            cached_tokens: usage["prompt_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
        });
    }

    // Check for tool_calls in the delta
    if let Some(tool_calls) = value["choices"][0]["delta"]["tool_calls"].as_array() {
        for tc in tool_calls {
            if let (Some(id), Some(name)) = (tc["id"].as_str(), tc["function"]["name"].as_str()) {
                let input: serde_json::Value = tc["function"]["arguments"]
                    .as_str()
                    .and_then(|s| serde_json::from_str(s).ok())
                    .unwrap_or(serde_json::Value::Null);
                state.pending.push_back(ModelEvent::ToolCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    input,
                });
            }
        }
    }

    let text = value["choices"][0]["delta"]["content"]
        .as_str()
        .unwrap_or_default();
    if value["choices"][0]["finish_reason"].is_string() {
        state.pending.push_back(ModelEvent::Completed);
        state.completed = true;
    }
    if text.is_empty() && state.pending.is_empty() {
        Ok(None)
    } else if !text.is_empty() {
        Ok(Some(ModelEvent::TextDelta { text: text.into() }))
    } else {
        // We enqueued tool_calls and/or usage; let the next event loop return them.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_lines_preserve_unicode() {
        let mut state = ProviderStreamState {
            stream: Box::pin(stream::empty()),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "ollama".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let event = parse_provider_line(
            &mut state,
            r#"{"message":{"content":"🦀"},"done":false}"#.as_bytes(),
        )
        .unwrap();
        assert!(matches!(event, Some(ModelEvent::TextDelta { text }) if text == "🦀"));
    }

    #[test]
    fn openai_sse_dispatches_only_at_event_boundary() {
        let mut state = ProviderStreamState {
            stream: Box::pin(stream::empty()),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "openai_compatible".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        assert!(
            parse_provider_line(
                &mut state,
                br#"data: {"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#
            )
            .unwrap()
            .is_none()
        );
        assert!(
            matches!(parse_provider_line(&mut state, b"").unwrap(), Some(ModelEvent::TextDelta { text }) if text == "hi")
        );
    }

    #[test]
    fn tool_results_fan_out_to_one_wire_message_per_call() {
        use gobrowse_core::model::{ContentPart, MessageRole, NeutralMessage};
        let messages = vec![
            NeutralMessage {
                role: MessageRole::Assistant,
                content: vec![
                    ContentPart::ToolCall {
                        id: "call-a".into(),
                        name: "library_search".into(),
                        input: serde_json::json!({"q": "one"}),
                    },
                    ContentPart::ToolCall {
                        id: "call-b".into(),
                        name: "library_search".into(),
                        input: serde_json::json!({"q": "two"}),
                    },
                ],
                provider_provenance: None,
            },
            NeutralMessage {
                role: MessageRole::Tool,
                content: vec![
                    ContentPart::ToolResult {
                        call_id: "call-a".into(),
                        output: serde_json::json!({"books": []}),
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        call_id: "call-b".into(),
                        output: serde_json::json!({"books": [1]}),
                        is_error: false,
                    },
                ],
                provider_provenance: None,
            },
        ];
        let wire = to_wire_messages(&messages);
        assert_eq!(wire.len(), 3);
        assert_eq!(wire[0].tool_calls.as_ref().unwrap().len(), 2);
        let tool_messages: Vec<_> = wire
            .iter()
            .filter(|message| message.role == "tool")
            .collect();
        assert_eq!(tool_messages.len(), 2, "one wire message per tool result");
        assert_eq!(tool_messages[0].tool_call_id.as_deref(), Some("call-a"));
        assert_eq!(tool_messages[1].tool_call_id.as_deref(), Some("call-b"));
        assert_eq!(tool_messages[0].content.as_deref(), Some("{\"books\":[]}"));
    }

    #[tokio::test]
    async fn final_ollama_record_without_newline_drains_terminal_events() {
        let state = ProviderStreamState {
            stream: Box::pin(stream::iter([Ok(Bytes::from_static(
                br#"{"message":{"content":""},"done":true,"prompt_eval_count":2,"eval_count":1}"#,
            ))])),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "ollama".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let events = stream::unfold(state, next_provider_event)
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.as_slice(),
            [Ok(ModelEvent::Usage { .. }), Ok(ModelEvent::Completed)]
        ));
    }

    #[tokio::test]
    async fn oversized_event_aborts_after_four_megabyte_cap() {
        // Two 3 MiB chunks: first is under the 4 MiB cumulative cap,
        // second pushes received_bytes over cap → InvalidResponse.
        let chunk_a = Bytes::from(vec![b'A'; 3 * 1024 * 1024]);
        let chunk_b = Bytes::from(vec![b'B'; 3 * 1024 * 1024]);
        let state = ProviderStreamState {
            stream: Box::pin(stream::iter([Ok(chunk_a), Ok(chunk_b)])),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "openai_compatible".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let results = stream::unfold(state, next_provider_event)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(ProviderError::InvalidResponse)));
    }

    #[tokio::test]
    async fn malformed_json_line_returns_invalid_response() {
        let state = ProviderStreamState {
            stream: Box::pin(stream::iter([Ok(Bytes::from_static(
                b"data: {not-json}\n\n",
            ))])),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "openai_compatible".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let results = stream::unfold(state, next_provider_event)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(ProviderError::InvalidResponse)));
    }

    #[tokio::test]
    async fn openai_done_sentinel_emits_completed_exactly_once() {
        let state = ProviderStreamState {
            stream: Box::pin(stream::iter([Ok(Bytes::from_static(b"data: [DONE]\n\n"))])),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "openai_compatible".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let results = stream::unfold(state, next_provider_event)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(
            results.len(),
            1,
            "expected exactly one Completed event, got {results:?}"
        );
        assert!(matches!(results[0], Ok(ModelEvent::Completed)));
    }

    #[tokio::test]
    async fn midstream_disconnect_with_pending_sse_drains_or_errors() {
        // Stream delivers a partial SSE data line (no terminating newline)
        // then ends.  The pending incomplete JSON must trigger an error.
        let state = ProviderStreamState {
            stream: Box::pin(stream::iter([Ok(Bytes::from_static(
                br#"data: {"choices":[{"delta":{"content":"partial"#,
            ))])),
            buffer: vec![],
            pending: VecDeque::new(),
            provider_type: "openai_compatible".into(),
            sse_data: String::new(),
            received_bytes: 0,
            completed: false,
        };
        let results = stream::unfold(state, next_provider_event)
            .collect::<Vec<_>>()
            .await;
        assert_eq!(results.len(), 1);
        assert!(matches!(results[0], Err(ProviderError::InvalidResponse)));
    }
}
