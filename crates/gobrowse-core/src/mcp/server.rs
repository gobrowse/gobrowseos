//! Transport-neutral MCP dispatcher used by deterministic conformance servers.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    capabilities::{CapabilityMethod, ServerCapabilities},
    model::*,
    wire::*,
};

pub const ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERROR_INVALID_PARAMS: i64 = -32602;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DispatchError {
    #[error("MCP request requires initialization")]
    NotInitialized,
    #[error("MCP method is not available")]
    NotSupported,
}

#[async_trait]
pub trait McpServerHandler: Send + Sync {
    async fn discover(&self) -> Result<Value, RpcError> {
        Ok(json!({"supportedVersions": ["2026-07-28", "2025-11-25"]}))
    }
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult, RpcError> {
        Err(RpcError {
            code: ERROR_METHOD_NOT_FOUND,
            message: "initialize unavailable".into(),
            data: None,
        })
    }
    async fn tools_list(&self, _params: ListParams) -> Result<Paginated<Tool>, RpcError> {
        not_found()
    }
    async fn tools_call(&self, _params: ToolCallParams) -> Result<ToolCallResult, RpcError> {
        not_found()
    }
    async fn resources_list(&self, _params: ListParams) -> Result<Paginated<Resource>, RpcError> {
        not_found()
    }
    async fn resources_read(
        &self,
        _params: ResourceReadParams,
    ) -> Result<ResourceReadResult, RpcError> {
        not_found()
    }
    async fn prompts_list(&self, _params: ListParams) -> Result<Paginated<Prompt>, RpcError> {
        not_found()
    }
    async fn prompts_get(&self, _params: PromptGetParams) -> Result<PromptGetResult, RpcError> {
        not_found()
    }
    async fn ping(&self) -> Result<Value, RpcError> {
        Ok(json!({}))
    }
    async fn cancelled(&self, _params: CancelledParams) {}
}

fn not_found<T>() -> Result<T, RpcError> {
    Err(RpcError {
        code: ERROR_METHOD_NOT_FOUND,
        message: "method not found".into(),
        data: None,
    })
}

pub struct McpServerDispatcher<H> {
    handler: H,
    capabilities: ServerCapabilities,
    initialized: bool,
}

impl<H: McpServerHandler> McpServerDispatcher<H> {
    pub fn new(handler: H, capabilities: ServerCapabilities) -> Self {
        Self {
            handler,
            capabilities,
            initialized: false,
        }
    }

    pub const fn capabilities(&self) -> &ServerCapabilities {
        &self.capabilities
    }
    pub const fn initialized(&self) -> bool {
        self.initialized
    }

    pub async fn dispatch(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        let result = self.dispatch_request(request).await;
        Response {
            jsonrpc: JSONRPC_VERSION.to_owned(),
            id,
            result: match result {
                Ok(result) => ResponseBody::Result { result },
                Err(error) => ResponseBody::Error { error },
            },
        }
    }

    pub async fn notify(&mut self, notification: Notification) -> Result<(), DispatchError> {
        match notification.method.as_str() {
            METHOD_INITIALIZED => {
                self.initialized = true;
                Ok(())
            }
            METHOD_CANCELLED => {
                let params = parse_params(notification.params)?;
                self.handler.cancelled(params).await;
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn dispatch_request(&mut self, request: Request) -> Result<Value, RpcError> {
        let params = request.params.unwrap_or(Value::Object(Default::default()));
        match request.method.as_str() {
            METHOD_DISCOVER => self.handler.discover().await,
            METHOD_INITIALIZE => {
                let params: InitializeParams = parse_json(params)?;
                let result = self.handler.initialize(params).await?;
                Ok(serde_json::to_value(result).map_err(internal_error)?)
            }
            METHOD_PING => serde_json::to_value(self.handler.ping().await?).map_err(internal_error),
            METHOD_TOOLS_LIST => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::ToolsList)?;
                let params: ListParams = parse_json(params)?;
                serde_json::to_value(self.handler.tools_list(params).await?).map_err(internal_error)
            }
            METHOD_TOOLS_CALL => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::ToolsCall)?;
                let params: ToolCallParams = parse_json(params)?;
                serde_json::to_value(self.handler.tools_call(params).await?).map_err(internal_error)
            }
            METHOD_RESOURCES_LIST => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::ResourcesList)?;
                let params: ListParams = parse_json(params)?;
                serde_json::to_value(self.handler.resources_list(params).await?)
                    .map_err(internal_error)
            }
            METHOD_RESOURCES_READ => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::ResourcesRead)?;
                let params: ResourceReadParams = parse_json(params)?;
                serde_json::to_value(self.handler.resources_read(params).await?)
                    .map_err(internal_error)
            }
            METHOD_PROMPTS_LIST => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::PromptsList)?;
                let params: ListParams = parse_json(params)?;
                serde_json::to_value(self.handler.prompts_list(params).await?)
                    .map_err(internal_error)
            }
            METHOD_PROMPTS_GET => {
                self.require_initialized()?;
                self.require_capability(CapabilityMethod::PromptsGet)?;
                let params: PromptGetParams = parse_json(params)?;
                serde_json::to_value(self.handler.prompts_get(params).await?)
                    .map_err(internal_error)
            }
            _ => Err(RpcError {
                code: ERROR_METHOD_NOT_FOUND,
                message: "method not found".into(),
                data: None,
            }),
        }
    }

    fn require_capability(&self, method: CapabilityMethod) -> Result<(), RpcError> {
        method.require(&self.capabilities).map_err(|_| RpcError {
            code: -32001,
            message: "method unavailable in negotiated capabilities".into(),
            data: None,
        })
    }

    fn require_initialized(&self) -> Result<(), RpcError> {
        self.initialized.then_some(()).ok_or(RpcError {
            code: -32002,
            message: "server is not initialized".into(),
            data: None,
        })
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, RpcError> {
    serde_json::from_value(value).map_err(|_| RpcError {
        code: ERROR_INVALID_PARAMS,
        message: "invalid parameters".into(),
        data: None,
    })
}

fn parse_params<T: serde::de::DeserializeOwned>(value: Option<Value>) -> Result<T, DispatchError> {
    serde_json::from_value(value.unwrap_or(Value::Null)).map_err(|_| DispatchError::NotSupported)
}

fn internal_error(_: serde_json::Error) -> RpcError {
    RpcError {
        code: -32603,
        message: "internal MCP response error".into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::capabilities::{ResourcesCapability, ToolsCapability};

    struct Fixture;
    #[async_trait]
    impl McpServerHandler for Fixture {
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult {
                protocol_version: params.protocol_version,
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    resources: Some(ResourcesCapability::default()),
                    ..Default::default()
                },
                server_info: ClientInfo {
                    name: "fixture".into(),
                    version: "1".into(),
                },
                instructions: None,
            })
        }
        async fn tools_list(&self, _: ListParams) -> Result<Paginated<Tool>, RpcError> {
            Ok(Paginated {
                items: vec![],
                next_cursor: None,
            })
        }
    }

    #[tokio::test]
    async fn dispatches_initialize_ping_and_capability_methods() {
        let mut dispatcher = McpServerDispatcher::new(Fixture, ServerCapabilities::default());
        let response = dispatcher
            .dispatch(request(RequestId::Number(1), METHOD_TOOLS_LIST, None))
            .await;
        assert!(matches!(
            response.result,
            ResponseBody::Error {
                error: RpcError { code: -32002, .. }
            }
        ));
        let init = InitializeParams {
            protocol_version: "2025-11-25".into(),
            capabilities: Default::default(),
            client_info: ClientInfo {
                name: "test".into(),
                version: "1".into(),
            },
        };
        let response = dispatcher
            .dispatch(request(
                RequestId::Number(2),
                METHOD_INITIALIZE,
                Some(serde_json::to_value(init).expect("params")),
            ))
            .await;
        assert!(matches!(response.result, ResponseBody::Result { .. }));
        assert!(!dispatcher.initialized());
        dispatcher
            .notify(notification(METHOD_INITIALIZED, None))
            .await
            .expect("initialized notification");
        let response = dispatcher
            .dispatch(request(RequestId::Number(3), METHOD_PING, None))
            .await;
        assert!(matches!(response.result, ResponseBody::Result { .. }));
    }
}
