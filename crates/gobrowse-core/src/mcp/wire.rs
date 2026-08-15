//! Bounded JSON-RPC 2.0 wire envelopes used by MCP transports.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Message {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("JSON-RPC frame exceeds the configured limit")]
    FrameTooLarge,
    #[error("malformed JSON-RPC message")]
    Malformed,
    #[error("JSON-RPC version must be 2.0")]
    InvalidVersion,
    #[error("JSON-RPC method is empty or too long")]
    InvalidMethod,
    #[error("JSON-RPC id is invalid or too long")]
    InvalidId,
    #[error("JSON-RPC response id is missing or invalid")]
    InvalidResponse,
}

pub fn encode(message: &Message) -> Result<Vec<u8>, WireError> {
    let bytes = serde_json::to_vec(message).map_err(|_| WireError::Malformed)?;
    validate_bytes(&bytes)?;
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<Message, WireError> {
    validate_bytes(bytes)?;
    let message: Message = serde_json::from_slice(bytes).map_err(|_| WireError::Malformed)?;
    validate_message(&message)?;
    Ok(message)
}

pub fn validate_bytes(bytes: &[u8]) -> Result<(), WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        Err(WireError::FrameTooLarge)
    } else {
        Ok(())
    }
}

pub fn validate_message(message: &Message) -> Result<(), WireError> {
    match message {
        Message::Request(request) => {
            validate_common(&request.jsonrpc, &request.method)?;
            request.id.validate()
        }
        Message::Notification(notification) => {
            validate_common(&notification.jsonrpc, &notification.method)
        }
        Message::Response(response) => {
            if response.jsonrpc != JSONRPC_VERSION {
                return Err(WireError::InvalidVersion);
            }
            response
                .id
                .validate()
                .map_err(|_| WireError::InvalidResponse)
        }
    }
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

pub fn request(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Request {
    Request {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        id,
        method: method.into(),
        params,
    }
}

pub fn notification(method: impl Into<String>, params: Option<Value>) -> Notification {
    Notification {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        method: method.into(),
        params,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_integer_and_string_ids_without_coercion() {
        for id in [RequestId::Number(7), RequestId::String("seven".into())] {
            let bytes =
                encode(&Message::Request(request(id.clone(), METHOD_PING, None))).expect("encode");
            let decoded = decode(&bytes).expect("decode");
            assert_eq!(decoded, Message::Request(request(id, METHOD_PING, None)));
        }
    }

    #[test]
    fn notifications_have_no_id_and_oversized_frames_are_rejected() {
        let message = Message::Notification(notification(METHOD_CANCELLED, None));
        assert!(decode(&encode(&message).expect("encode")).is_ok());
        assert_eq!(
            decode(&vec![b'x'; MAX_FRAME_BYTES + 1]),
            Err(WireError::FrameTooLarge)
        );
    }
}
