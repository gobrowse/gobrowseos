//! Strict, bounded JSON-RPC 2.0 envelopes for MCP.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::{self, Write};

use super::{
    model::*,
    validation::{self, ValidateMcp},
};

pub const JSONRPC_VERSION: &str = "2.0";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_METHOD_BYTES: usize = 128;
pub const MAX_ID_BYTES: usize = 128;
pub const METHOD_INITIALIZE: &str = "initialize";
pub const METHOD_INITIALIZED: &str = "notifications/initialized";
pub const METHOD_DISCOVER: &str = "server/discover";
pub const METHOD_PING: &str = "ping";
pub const METHOD_TOOLS_LIST: &str = "tools/list";
pub const METHOD_TOOLS_CALL: &str = "tools/call";
pub const METHOD_RESOURCES_LIST: &str = "resources/list";
pub const METHOD_RESOURCES_READ: &str = "resources/read";
pub const METHOD_RESOURCES_SUBSCRIBE: &str = "resources/subscribe";
pub const METHOD_RESOURCES_UNSUBSCRIBE: &str = "resources/unsubscribe";
pub const METHOD_PROMPTS_LIST: &str = "prompts/list";
pub const METHOD_PROMPTS_GET: &str = "prompts/get";
pub const METHOD_CANCELLED: &str = "notifications/cancelled";
pub const METHOD_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
pub const METHOD_RESOURCES_LIST_CHANGED: &str = "notifications/resources/list_changed";
pub const METHOD_RESOURCE_UPDATED: &str = "notifications/resources/updated";
pub const METHOD_PROGRESS: &str = "notifications/progress";
pub const METHOD_LOGGING_MESSAGE: &str = "notifications/message";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}
impl RequestId {
    pub fn validate(&self) -> Result<(), WireError> {
        if let Self::String(value) = self
            && (value.is_empty() || value.len() > MAX_ID_BYTES)
        {
            return Err(WireError::InvalidId);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(flatten)]
    pub result: ResponseBody,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Result { result: Value },
    Error { error: RpcError },
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SuccessEnvelope {
    jsonrpc: String,
    id: RequestId,
    result: Value,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ErrorEnvelope {
    jsonrpc: String,
    id: RequestId,
    error: RpcError,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedRequest(Request);
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedNotification(Notification);
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedResponse(Response);
#[derive(Debug, Clone, PartialEq)]
pub enum ValidatedMessage {
    Request(ValidatedRequest),
    Notification(ValidatedNotification),
    Response(ValidatedResponse),
}
impl ValidatedRequest {
    pub fn id(&self) -> &RequestId {
        &self.0.id
    }
    pub fn method(&self) -> &str {
        &self.0.method
    }
    pub fn params(&self) -> Option<&Value> {
        self.0.params.as_ref()
    }
}
impl ValidatedNotification {
    pub fn method(&self) -> &str {
        &self.0.method
    }
    pub fn params(&self) -> Option<&Value> {
        self.0.params.as_ref()
    }
}
impl ValidatedResponse {
    pub fn id(&self) -> &RequestId {
        &self.0.id
    }
    pub(crate) fn body(&self) -> &ResponseBody {
        &self.0.result
    }
    pub fn correlate(self, method: &str) -> Result<CorrelatedResponse, WireError> {
        if let ResponseBody::Result { result } = &self.0.result {
            validate_success(method, result)?;
        }
        Ok(CorrelatedResponse { response: self })
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct CorrelatedResponse {
    response: ValidatedResponse,
}
impl CorrelatedResponse {
    pub fn id(&self) -> &RequestId {
        self.response.id()
    }
    pub fn body(&self) -> &ResponseBody {
        self.response.body()
    }
}
impl ValidatedMessage {
    pub fn request(self) -> Option<ValidatedRequest> {
        if let Self::Request(value) = self {
            Some(value)
        } else {
            None
        }
    }
    pub fn into_notification(self) -> Option<ValidatedNotification> {
        if let Self::Notification(value) = self {
            Some(value)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("JSON-RPC frame exceeds the configured limit")]
    FrameTooLarge,
    #[error("malformed JSON-RPC message")]
    Malformed,
    #[error("JSON-RPC message must be an object")]
    NotObject,
    #[error("JSON-RPC version must be 2.0")]
    InvalidVersion,
    #[error("JSON-RPC method is empty or too long")]
    InvalidMethod,
    #[error("JSON-RPC id is invalid or too long")]
    InvalidId,
    #[error("JSON-RPC envelope shape is invalid")]
    InvalidShape,
    #[error("method-specific MCP value is invalid")]
    InvalidValue,
}

pub fn decode(bytes: &[u8]) -> Result<ValidatedMessage, WireError> {
    if bytes.is_empty() {
        return Err(WireError::Malformed);
    }
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge);
    }
    let value = validation::parse_bounded_json(bytes).map_err(|error| match error {
        validation::ValidationError::Malformed => WireError::Malformed,
        _ => WireError::InvalidValue,
    })?;
    validation::validate_json(&value).map_err(|_| WireError::InvalidValue)?;
    let object = value.as_object().ok_or(WireError::NotObject)?;
    classify(object)
}

fn classify(object: &Map<String, Value>) -> Result<ValidatedMessage, WireError> {
    let has = |key: &str| object.contains_key(key);
    if has("method") {
        if has("id") {
            if has("result") || has("error") {
                return Err(WireError::InvalidShape);
            }
            let request: Request = serde_json::from_value(Value::Object(object.clone()))
                .map_err(|_| WireError::InvalidShape)?;
            validate_request(request).map(ValidatedMessage::Request)
        } else {
            if has("result") || has("error") {
                return Err(WireError::InvalidShape);
            }
            let notification: Notification = serde_json::from_value(Value::Object(object.clone()))
                .map_err(|_| WireError::InvalidShape)?;
            validate_notification(notification).map(ValidatedMessage::Notification)
        }
    } else if has("result") ^ has("error") {
        let response = if has("result") {
            let envelope: SuccessEnvelope = serde_json::from_value(Value::Object(object.clone()))
                .map_err(|_| WireError::InvalidShape)?;
            Response {
                jsonrpc: envelope.jsonrpc,
                id: envelope.id,
                result: ResponseBody::Result {
                    result: envelope.result,
                },
            }
        } else {
            let envelope: ErrorEnvelope = serde_json::from_value(Value::Object(object.clone()))
                .map_err(|_| WireError::InvalidShape)?;
            Response {
                jsonrpc: envelope.jsonrpc,
                id: envelope.id,
                result: ResponseBody::Error {
                    error: envelope.error,
                },
            }
        };
        validate_response(response).map(ValidatedMessage::Response)
    } else {
        Err(WireError::InvalidShape)
    }
}

fn validate_request(request: Request) -> Result<ValidatedRequest, WireError> {
    validate_common(&request.jsonrpc, &request.method)?;
    request.id.validate()?;
    if let Some(params) = &request.params {
        validation::validate_json(params).map_err(|_| WireError::InvalidValue)?;
    }
    validate_method_params(&request.method, request.params.as_ref())?;
    Ok(ValidatedRequest(request))
}
fn validate_notification(notification: Notification) -> Result<ValidatedNotification, WireError> {
    validate_common(&notification.jsonrpc, &notification.method)?;
    if let Some(params) = &notification.params {
        validation::validate_json(params).map_err(|_| WireError::InvalidValue)?;
    }
    validate_method_params(&notification.method, notification.params.as_ref())?;
    Ok(ValidatedNotification(notification))
}
fn validate_response(response: Response) -> Result<ValidatedResponse, WireError> {
    if response.jsonrpc != JSONRPC_VERSION {
        return Err(WireError::InvalidVersion);
    }
    response.id.validate()?;
    match &response.result {
        ResponseBody::Result { result } => {
            validation::validate_json(result).map_err(|_| WireError::InvalidValue)?
        }
        ResponseBody::Error { error } => {
            if error.message.is_empty() || error.message.len() > MAX_METHOD_BYTES {
                return Err(WireError::InvalidValue);
            }
            if let Some(data) = &error.data {
                validation::validate_json(data).map_err(|_| WireError::InvalidValue)?;
            }
        }
    }
    preflight_size(&response)?;
    Ok(ValidatedResponse(response))
}
fn preflight_size(response: &Response) -> Result<(), WireError> {
    struct Capped(usize);
    impl Write for Capped {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "frame limit"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Capped(MAX_FRAME_BYTES), response).map_err(|error| {
        if error.io_error_kind().is_some() {
            WireError::FrameTooLarge
        } else {
            WireError::Malformed
        }
    })
}

pub(crate) fn safe_internal_error_response(id: RequestId) -> ValidatedResponse {
    ValidatedResponse(Response {
        jsonrpc: JSONRPC_VERSION.into(),
        id,
        result: ResponseBody::Error {
            error: RpcError {
                code: -32603,
                message: "internal MCP handler error".into(),
                data: None,
            },
        },
    })
}
fn validate_method_params(method: &str, params: Option<&Value>) -> Result<(), WireError> {
    let Some(params) = params else {
        if matches!(
            method,
            METHOD_INITIALIZE
                | METHOD_TOOLS_CALL
                | METHOD_RESOURCES_READ
                | METHOD_RESOURCES_SUBSCRIBE
                | METHOD_RESOURCES_UNSUBSCRIBE
                | METHOD_PROMPTS_GET
                | METHOD_CANCELLED
                | METHOD_RESOURCE_UPDATED
                | METHOD_PROGRESS
                | METHOD_LOGGING_MESSAGE
        ) {
            return Err(WireError::InvalidValue);
        }
        return Ok(());
    };
    if matches!(
        method,
        METHOD_INITIALIZED | METHOD_TOOLS_LIST_CHANGED | METHOD_RESOURCES_LIST_CHANGED
    ) {
        return Err(WireError::InvalidValue);
    }
    macro_rules! typed {
        ($ty:ty) => {{
            let value: $ty =
                serde_json::from_value(params.clone()).map_err(|_| WireError::InvalidValue)?;
            value.validate_mcp().map_err(|_| WireError::InvalidValue)?;
        }};
    }
    match method {
        METHOD_INITIALIZE => typed!(InitializeParams),
        METHOD_TOOLS_LIST | METHOD_RESOURCES_LIST | METHOD_PROMPTS_LIST => typed!(ListParams),
        METHOD_TOOLS_CALL => typed!(ToolCallParams),
        METHOD_RESOURCES_READ => typed!(ResourceReadParams),
        METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE => {
            typed!(ResourceSubscriptionParams)
        }
        METHOD_PROMPTS_GET => typed!(PromptGetParams),
        METHOD_CANCELLED => typed!(CancelledParams),
        METHOD_RESOURCE_UPDATED => typed!(ResourceUpdatedParams),
        METHOD_PROGRESS => typed!(ProgressParams),
        METHOD_LOGGING_MESSAGE => typed!(LoggingMessageParams),
        _ => {}
    }
    Ok(())
}

fn validate_common(version: &str, method: &str) -> Result<(), WireError> {
    if version != JSONRPC_VERSION {
        return Err(WireError::InvalidVersion);
    }
    if method.is_empty() || method.len() > MAX_METHOD_BYTES {
        return Err(WireError::InvalidMethod);
    }
    Ok(())
}

pub fn encode(message: &ValidatedMessage) -> Result<Vec<u8>, WireError> {
    let value = match message {
        ValidatedMessage::Request(value) => serde_json::to_value(&value.0),
        ValidatedMessage::Notification(value) => serde_json::to_value(&value.0),
        ValidatedMessage::Response(value) => serde_json::to_value(&value.0),
    }
    .map_err(|_| WireError::Malformed)?;
    let bytes = serde_json::to_vec(&value).map_err(|_| WireError::Malformed)?;
    if bytes.len() > MAX_FRAME_BYTES {
        Err(WireError::FrameTooLarge)
    } else {
        Ok(bytes)
    }
}

pub fn try_request(
    id: RequestId,
    method: impl Into<String>,
    params: Option<Value>,
) -> Result<ValidatedRequest, WireError> {
    validate_request(Request {
        jsonrpc: JSONRPC_VERSION.into(),
        id,
        method: method.into(),
        params,
    })
}
pub fn try_notification(
    method: impl Into<String>,
    params: Option<Value>,
) -> Result<ValidatedNotification, WireError> {
    validate_notification(Notification {
        jsonrpc: JSONRPC_VERSION.into(),
        method: method.into(),
        params,
    })
}
fn validate_success(method: &str, value: &Value) -> Result<(), WireError> {
    macro_rules! typed_result {
        ($ty:ty) => {{
            let parsed: $ty =
                serde_json::from_value(value.clone()).map_err(|_| WireError::InvalidValue)?;
            parsed.validate_mcp().map_err(|_| WireError::InvalidValue)?;
        }};
    }
    match method {
        METHOD_INITIALIZE => typed_result!(InitializeResult),
        METHOD_DISCOVER => typed_result!(DiscoverResult),
        METHOD_PING => {
            if !value.is_object() {
                return Err(WireError::InvalidValue);
            }
        }
        METHOD_TOOLS_LIST => typed_result!(Paginated<Tool>),
        METHOD_TOOLS_CALL => typed_result!(ToolCallResult),
        METHOD_RESOURCES_LIST => typed_result!(Paginated<Resource>),
        METHOD_RESOURCES_READ => typed_result!(ResourceReadResult),
        METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE => {
            value.validate_mcp().map_err(|_| WireError::InvalidValue)?
        }
        METHOD_PROMPTS_LIST => typed_result!(Paginated<Prompt>),
        METHOD_PROMPTS_GET => typed_result!(PromptGetResult),
        _ => return Err(WireError::InvalidValue),
    }
    Ok(())
}
pub fn validated_response_for_method(
    method: &str,
    id: RequestId,
    result: Result<Value, RpcError>,
) -> Result<ValidatedResponse, WireError> {
    if let Ok(value) = &result {
        validate_success(method, value)?;
    }
    response(id, result)
}
pub(crate) fn response(
    id: RequestId,
    result: Result<Value, RpcError>,
) -> Result<ValidatedResponse, WireError> {
    validate_response(Response {
        jsonrpc: JSONRPC_VERSION.into(),
        id,
        result: match result {
            Ok(result) => ResponseBody::Result { result },
            Err(error) => ResponseBody::Error { error },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_four_exclusive_shapes_and_rejects_hybrids() {
        let valid = [
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            r#"{"jsonrpc":"2.0","id":"x","result":{}}"#,
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":"bad"}}"#,
        ];
        for raw in valid {
            assert!(decode(raw.as_bytes()).is_ok(), "{raw}");
        }
        for raw in [
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","method":"x","id":null}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"extra":true}"#,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"}{"x":1}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"other"}"#,
        ] {
            assert!(decode(raw.as_bytes()).is_err(), "accepted {raw}");
        }
    }
    #[test]
    fn response_size_and_correlation_are_bounded() {
        let huge = serde_json::json!({"value": "x".repeat(MAX_FRAME_BYTES)});
        assert_eq!(
            validated_response_for_method(METHOD_PING, RequestId::Number(1), Ok(huge)),
            Err(WireError::FrameTooLarge)
        );
        let response = validated_response_for_method(
            METHOD_PING,
            RequestId::Number(2),
            Ok(serde_json::json!({})),
        )
        .unwrap();
        assert!(response.clone().correlate(METHOD_PING).is_ok());
        assert_eq!(
            response.clone().correlate(METHOD_TOOLS_LIST),
            Err(WireError::InvalidValue)
        );
        assert_eq!(
            response.correlate("unknown/method"),
            Err(WireError::InvalidValue)
        );
        let error = validated_response_for_method(
            METHOD_PING,
            RequestId::Number(3),
            Err(RpcError {
                code: -1,
                message: "x".repeat(MAX_METHOD_BYTES + 1),
                data: None,
            }),
        );
        assert_eq!(error, Err(WireError::InvalidValue));
    }
    #[test]
    fn rejects_deep_and_large_containers_before_dispatch() {
        let mut deep = Value::Null;
        for _ in 0..=validation::MAX_JSON_DEPTH {
            deep = Value::Array(vec![deep]);
        }
        assert_eq!(
            decode(serde_json::to_string(&deep).expect("json").as_bytes()),
            Err(WireError::InvalidValue)
        );
        let params = Value::Array((0..=validation::MAX_ITEMS).map(Value::from).collect());
        let raw = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"ping","params":params});
        assert_eq!(
            decode(serde_json::to_string(&raw).expect("json").as_bytes()),
            Err(WireError::InvalidValue)
        );
    }
}
