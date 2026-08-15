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
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyResult {}
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
        METHOD_PING | METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE => {
            let Some(object) = value.as_object() else {
                return Err(WireError::InvalidValue);
            };
            if !object.is_empty() {
                return Err(WireError::InvalidValue);
            }
            serde_json::from_value::<EmptyResult>(value.clone())
                .map_err(|_| WireError::InvalidValue)?;
        }
        METHOD_TOOLS_LIST => typed_result!(Paginated<Tool>),
        METHOD_TOOLS_CALL => typed_result!(ToolCallResult),
        METHOD_RESOURCES_LIST => typed_result!(Paginated<Resource>),
        METHOD_RESOURCES_READ => typed_result!(ResourceReadResult),
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
            validated_response_for_method(
                METHOD_PING,
                RequestId::Number(1),
                Err(RpcError {
                    code: -1,
                    message: "x".into(),
                    data: Some(huge)
                }),
            ),
            Err(WireError::FrameTooLarge)
        );
        let validated = validated_response_for_method(
            METHOD_PING,
            RequestId::Number(2),
            Ok(serde_json::json!({})),
        )
        .unwrap();
        assert!(validated.clone().correlate(METHOD_PING).is_ok());
        assert_eq!(
            validated.clone().correlate(METHOD_TOOLS_LIST),
            Err(WireError::InvalidValue)
        );
        assert_eq!(
            validated.correlate("unknown/method"),
            Err(WireError::InvalidValue)
        );
        for method in [
            METHOD_PING,
            METHOD_RESOURCES_SUBSCRIBE,
            METHOD_RESOURCES_UNSUBSCRIBE,
        ] {
            assert!(
                validated_response_for_method(
                    method,
                    RequestId::Number(4),
                    Ok(serde_json::json!({}))
                )
                .is_ok()
            );
            for invalid in [
                serde_json::Value::Null,
                serde_json::json!([]),
                serde_json::json!(""),
                serde_json::json!(0),
                serde_json::json!(false),
                serde_json::json!({"extra": null}),
                serde_json::json!({"nested": {"x": 1}}),
            ] {
                assert_eq!(
                    validated_response_for_method(
                        method,
                        RequestId::Number(5),
                        Ok(invalid.clone())
                    ),
                    Err(WireError::InvalidValue)
                );
                let generic = super::response(RequestId::Number(6), Ok(invalid)).unwrap();
                assert_eq!(generic.correlate(method), Err(WireError::InvalidValue));
            }
        }
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

    #[test]
    fn envelope_and_id_matrix_is_exclusive_and_fail_closed() {
        for raw in [
            r#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#,
            r#"{"jsonrpc":2,"id":1,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":"","method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx","method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"method":"ping"}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{},"unknown":true}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","method":"other"}"#,
            r#"{"jsonrpc":"2.0","id":1,"method":"ping"} {"jsonrpc":"2.0"}"#,
            r#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#,
        ] {
            assert!(decode(raw.as_bytes()).is_err(), "accepted {raw}");
        }
    }

    #[test]
    fn known_method_parameter_matrix_distinguishes_missing_empty_and_aliases() {
        let initialize = serde_json::json!({
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"client","version":"1"}
        });
        let calls = [
            (METHOD_DISCOVER, None),
            (METHOD_INITIALIZE, Some(initialize)),
            (METHOD_PING, None),
            (METHOD_TOOLS_LIST, None),
            (METHOD_RESOURCES_LIST, Some(serde_json::json!({}))),
            (
                METHOD_PROMPTS_LIST,
                Some(serde_json::json!({"cursor":"next"})),
            ),
            (
                METHOD_TOOLS_CALL,
                Some(serde_json::json!({"name":"tool","arguments":{}})),
            ),
            (
                METHOD_RESOURCES_READ,
                Some(serde_json::json!({"uri":"urn:test"})),
            ),
            (
                METHOD_RESOURCES_SUBSCRIBE,
                Some(serde_json::json!({"uri":"urn:test"})),
            ),
            (
                METHOD_RESOURCES_UNSUBSCRIBE,
                Some(serde_json::json!({"uri":"urn:test"})),
            ),
            (
                METHOD_PROMPTS_GET,
                Some(serde_json::json!({"name":"prompt","arguments":{}})),
            ),
        ];
        for (method, params) in calls {
            assert!(
                try_request(RequestId::Number(1), method, params).is_ok(),
                "{method}"
            );
        }
        for (method, params) in [
            (METHOD_INITIALIZE, None),
            (METHOD_TOOLS_CALL, None),
            (METHOD_RESOURCES_READ, None),
            (METHOD_RESOURCES_SUBSCRIBE, None),
            (METHOD_RESOURCES_UNSUBSCRIBE, None),
            (METHOD_PROMPTS_GET, None),
        ] {
            assert_eq!(
                try_request(RequestId::Number(1), method, params),
                Err(WireError::InvalidValue)
            );
        }
        assert!(
            try_request(
                RequestId::Number(1),
                METHOD_TOOLS_LIST,
                Some(serde_json::json!({"cursor":""}))
            )
            .is_err()
        );
        assert!(
            try_request(
                RequestId::Number(1),
                METHOD_RESOURCES_READ,
                Some(serde_json::json!({"uri":"urn:test","uri_text":"alias"}))
            )
            .is_err()
        );
        assert!(try_notification(METHOD_INITIALIZED, Some(serde_json::json!({}))).is_err());
        assert!(try_notification(METHOD_TOOLS_LIST_CHANGED, Some(serde_json::json!({}))).is_err());
        assert!(
            try_notification(METHOD_CANCELLED, Some(serde_json::json!({"requestId":1}))).is_ok()
        );
    }

    #[test]
    fn exact_frame_limit_and_response_fallback_size_are_deterministic() {
        fn error_with_padding(padding: String) -> Result<ValidatedResponse, WireError> {
            response(
                RequestId::Number(7),
                Err(RpcError {
                    code: -1,
                    message: "e".into(),
                    data: Some(serde_json::json!({"padding": padding})),
                }),
            )
        }
        let base = error_with_padding(String::new()).expect("base");
        let base_len = serde_json::to_vec(&base.0).expect("base bytes").len();
        let exact =
            error_with_padding("x".repeat(MAX_FRAME_BYTES - base_len)).expect("exact frame");
        assert_eq!(
            serde_json::to_vec(&exact.0).expect("exact bytes").len(),
            MAX_FRAME_BYTES
        );
        assert_eq!(
            error_with_padding("x".repeat(MAX_FRAME_BYTES - base_len + 1)),
            Err(WireError::FrameTooLarge)
        );
        assert_eq!(
            decode(&vec![b' '; MAX_FRAME_BYTES + 1]),
            Err(WireError::FrameTooLarge)
        );
        let fallback = safe_internal_error_response(RequestId::String("request".into()));
        assert!(
            encode(&ValidatedMessage::Response(fallback))
                .expect("fallback")
                .len()
                < MAX_FRAME_BYTES
        );
    }

    #[test]
    fn all_valid_success_results_reject_cross_method_correlation() {
        let results = [
            (
                METHOD_DISCOVER,
                serde_json::json!({"supportedVersions":["2026-07-28"],"capabilities":{}}),
            ),
            (METHOD_PING, serde_json::json!({})),
            (METHOD_RESOURCES_SUBSCRIBE, serde_json::json!({})),
            (METHOD_RESOURCES_UNSUBSCRIBE, serde_json::json!({})),
        ];
        for (method, value) in results {
            let response = validated_response_for_method(method, RequestId::Number(1), Ok(value))
                .expect("valid result");
            assert!(response.clone().correlate(method).is_ok());
            assert_eq!(
                response.clone().correlate(METHOD_TOOLS_LIST),
                Err(WireError::InvalidValue)
            );
            assert!(response.correlate(method).is_ok());
        }
    }

    #[test]
    fn exhaustive_response_correlation_matrix_round_trips_all_supported_methods() {
        let supported = [
            (
                METHOD_DISCOVER,
                serde_json::json!({
                    "supportedVersions":["2026-07-28"],
                    "capabilities":{}
                }),
            ),
            (
                METHOD_INITIALIZE,
                serde_json::json!({
                    "protocolVersion":"2025-11-25",
                    "capabilities":{},
                    "serverInfo":{"name":"server","version":"1"}
                }),
            ),
            (METHOD_PING, serde_json::json!({})),
            (METHOD_TOOLS_LIST, serde_json::json!({"items":[]})),
            (METHOD_TOOLS_CALL, serde_json::json!({"content":[]})),
            (METHOD_RESOURCES_LIST, serde_json::json!({"items":[]})),
            (METHOD_RESOURCES_READ, serde_json::json!({"contents":[]})),
            (METHOD_RESOURCES_SUBSCRIBE, serde_json::json!({})),
            (METHOD_RESOURCES_UNSUBSCRIBE, serde_json::json!({})),
            (METHOD_PROMPTS_LIST, serde_json::json!({"items":[]})),
            (METHOD_PROMPTS_GET, serde_json::json!({"messages":[]})),
        ];

        for (index, (method, result)) in supported.iter().enumerate() {
            let id = RequestId::Number(index as i64 + 1);
            let response = validated_response_for_method(method, id.clone(), Ok(result.clone()))
                .expect("valid bounded success result");
            let encoded = encode(&ValidatedMessage::Response(response)).expect("encode response");
            let response = match decode(&encoded).expect("decode response") {
                ValidatedMessage::Response(response) => response,
                _ => panic!("decoded success response as a non-response"),
            };

            assert_eq!(response.id(), &id, "{method}");
            assert!(response.clone().correlate(method).is_ok(), "{method}");
            for (other_method, _) in &supported {
                let compatible = method == other_method
                    || (matches!(
                        *method,
                        METHOD_PING | METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE
                    ) && matches!(
                        *other_method,
                        METHOD_PING | METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE
                    ))
                    || (matches!(
                        *method,
                        METHOD_TOOLS_LIST | METHOD_RESOURCES_LIST | METHOD_PROMPTS_LIST
                    ) && matches!(
                        *other_method,
                        METHOD_TOOLS_LIST | METHOD_RESOURCES_LIST | METHOD_PROMPTS_LIST
                    ));
                let correlation = response.clone().correlate(other_method);
                if compatible {
                    assert!(
                        correlation.is_ok(),
                        "{method} rejected under {other_method}"
                    );
                } else {
                    assert_eq!(
                        correlation,
                        Err(WireError::InvalidValue),
                        "{method} accepted under {other_method}"
                    );
                }
            }
        }

        let id = RequestId::String("error-matrix".into());
        let error = RpcError {
            code: -32001,
            message: "bounded error".into(),
            data: Some(serde_json::json!({"detail":"bounded"})),
        };
        let response =
            validated_response_for_method(supported[0].0, id.clone(), Err(error.clone()))
                .expect("valid bounded error response");
        let encoded = encode(&ValidatedMessage::Response(response)).expect("encode error response");
        let response = match decode(&encoded).expect("decode error response") {
            ValidatedMessage::Response(response) => response,
            _ => panic!("decoded error response as a non-response"),
        };

        assert_eq!(response.id(), &id);
        for (method, _) in &supported {
            let correlated = response
                .clone()
                .correlate(method)
                .expect("error response correlates for every known method");
            assert_eq!(correlated.id(), &id, "{method}");
            match correlated.body() {
                ResponseBody::Error { error: actual } => assert_eq!(actual, &error, "{method}"),
                ResponseBody::Result { .. } => panic!("decoded error response as a success result"),
            }
        }
    }

    #[test]
    fn resource_result_rejects_simultaneous_text_and_blob() {
        assert_eq!(
            validated_response_for_method(
                METHOD_RESOURCES_READ,
                RequestId::Number(1),
                Ok(serde_json::json!({
                    "contents":[{
                        "uri":"urn:test",
                        "text":"plain text",
                        "blob":"c2VjcmV0"
                    }]
                })),
            ),
            Err(WireError::InvalidValue)
        );
    }

    #[test]
    fn tool_call_result_rejects_simultaneous_embedded_resource_text_and_blob() {
        assert_eq!(
            validated_response_for_method(
                METHOD_TOOLS_CALL,
                RequestId::Number(1),
                Ok(serde_json::json!({
                    "content":[{
                        "type":"resource",
                        "resource":{
                            "uri":"urn:test",
                            "text":"plain text",
                            "blob":"c2VjcmV0"
                        }
                    }]
                })),
            ),
            Err(WireError::InvalidValue)
        );
    }

    #[test]
    fn prompt_get_result_rejects_simultaneous_embedded_resource_text_and_blob() {
        assert_eq!(
            validated_response_for_method(
                METHOD_PROMPTS_GET,
                RequestId::Number(1),
                Ok(serde_json::json!({
                    "messages":[{
                        "role":"user",
                        "content":{
                            "type":"resource",
                            "resource":{
                                "uri":"urn:test",
                                "text":"plain text",
                                "blob":"c2VjcmV0"
                            }
                        }
                    }]
                })),
            ),
            Err(WireError::InvalidValue)
        );
    }
}
