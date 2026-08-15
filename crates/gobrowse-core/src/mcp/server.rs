//! Transport-neutral MCP dispatcher with validated negotiation binding.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    McpProtocolEra, capabilities::ServerCapabilities, model::*, validation::ValidateMcp, wire::*,
};

pub const ERROR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERROR_INVALID_PARAMS: i64 = -32602;
pub const ERROR_NOT_INITIALIZED: i64 = -32002;
pub const ERROR_CAPABILITY: i64 = -32001;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DispatchError {
    #[error("MCP request requires initialization")]
    NotInitialized,
    #[error("MCP method is not available")]
    NotSupported,
    #[error("MCP negotiation is invalid")]
    Negotiation,
}

#[async_trait]
pub trait McpServerHandler: Send + Sync {
    async fn discover(&self) -> Result<DiscoverResult, RpcError> {
        Ok(DiscoverResult {
            supported_versions: vec!["2026-07-28".into(), "2025-11-25".into()],
            capabilities: ServerCapabilities::default(),
            server_info: None,
        })
    }
    async fn initialize(&self, _params: InitializeParams) -> Result<InitializeResult, RpcError> {
        not_found()
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
    async fn resources_subscribe(
        &self,
        _params: ResourceSubscriptionParams,
    ) -> Result<Value, RpcError> {
        not_found()
    }
    async fn resources_unsubscribe(
        &self,
        _params: ResourceSubscriptionParams,
    ) -> Result<Value, RpcError> {
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
    era: Option<McpProtocolEra>,
    capabilities: Option<ServerCapabilities>,
    pending_legacy: Option<(McpProtocolEra, ServerCapabilities)>,
    initialize_seen: bool,
    initialized_seen: bool,
}
impl<H: McpServerHandler> McpServerDispatcher<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            era: None,
            capabilities: None,
            pending_legacy: None,
            initialize_seen: false,
            initialized_seen: false,
        }
    }
    pub const fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }
    pub const fn era(&self) -> Option<McpProtocolEra> {
        self.era
    }
    pub const fn initialized(&self) -> bool {
        self.initialized_seen
    }

    pub async fn dispatch(&mut self, request: ValidatedRequest) -> ValidatedResponse {
        let id = request.id().clone();
        let method = request.method().to_owned();
        let result = self.dispatch_request(request).await;
        validated_response_for_method(&method, id, result)
            .expect("dispatcher creates valid response")
    }
    pub async fn notify(
        &mut self,
        notification: ValidatedNotification,
    ) -> Result<(), DispatchError> {
        match notification.method() {
            METHOD_INITIALIZED => {
                if self.pending_legacy.is_none() || !self.initialize_seen || self.initialized_seen {
                    return Err(DispatchError::Negotiation);
                }
                let (era, capabilities) = self
                    .pending_legacy
                    .take()
                    .ok_or(DispatchError::Negotiation)?;
                self.era = Some(era);
                self.capabilities = Some(capabilities);
                self.initialized_seen = true;
                Ok(())
            }
            METHOD_CANCELLED => {
                let params: CancelledParams = self
                    .params(notification.params())
                    .map_err(|_| DispatchError::NotSupported)?;
                params
                    .validate_mcp()
                    .map_err(|_| DispatchError::NotSupported)?;
                self.handler.cancelled(params).await;
                Ok(())
            }
            METHOD_TOOLS_LIST_CHANGED => self.notification_capability(true, false),
            METHOD_RESOURCES_LIST_CHANGED | METHOD_RESOURCE_UPDATED => {
                self.notification_capability(false, true)
            }
            METHOD_PROGRESS | METHOD_LOGGING_MESSAGE => Err(DispatchError::NotSupported),
            _ => Ok(()),
        }
    }
    async fn dispatch_request(&mut self, request: ValidatedRequest) -> Result<Value, RpcError> {
        let method = request.method().to_owned();
        let params = request
            .params()
            .cloned()
            .unwrap_or_else(|| Value::Object(Default::default()));
        match method.as_str() {
            METHOD_DISCOVER => {
                if self.era.is_some() {
                    return Err(negotiation_error());
                }
                let discovered = self.handler.discover().await?;
                discovered.validate_mcp().map_err(|_| invalid_params())?;
                let mut versions = std::collections::BTreeSet::new();
                if discovered.supported_versions.iter().any(|version| {
                    !versions.insert(version)
                        || super::McpProtocolEra::from_wire_version(version).is_none()
                }) {
                    return Err(negotiation_error());
                }
                let era = super::select_protocol_version(&discovered.supported_versions)
                    .map_err(|_| negotiation_error())?;
                if !era.is_modern() {
                    return Err(negotiation_error());
                }
                self.era = Some(era);
                self.capabilities = Some(discovered.capabilities.clone());
                self.initialized_seen = true;
                serde_json::to_value(discovered).map_err(internal_error)
            }
            METHOD_INITIALIZE => {
                if self.initialize_seen || self.era.is_some() {
                    return Err(negotiation_error());
                }
                let params: InitializeParams = self.params(Some(&params))?;
                params.validate_mcp().map_err(|_| invalid_params())?;
                if params.protocol_version != "2025-11-25" {
                    return Err(negotiation_error());
                }
                let result = self.handler.initialize(params.clone()).await?;
                result.validate_mcp().map_err(|_| invalid_params())?;
                if result.protocol_version != params.protocol_version
                    || super::McpProtocolEra::from_wire_version(&result.protocol_version)
                        != Some(McpProtocolEra::Legacy20251125)
                {
                    return Err(negotiation_error());
                }
                self.pending_legacy =
                    Some((McpProtocolEra::Legacy20251125, result.capabilities.clone()));
                self.initialize_seen = true;
                serde_json::to_value(result).map_err(internal_error)
            }
            METHOD_PING => validated_result(self.handler.ping().await),
            METHOD_TOOLS_LIST => {
                self.require(METHOD_TOOLS_LIST)?;
                let value: ListParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.tools_list(value).await)
            }
            METHOD_TOOLS_CALL => {
                self.require(METHOD_TOOLS_CALL)?;
                let value: ToolCallParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.tools_call(value).await)
            }
            METHOD_RESOURCES_LIST => {
                self.require(METHOD_RESOURCES_LIST)?;
                let value: ListParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.resources_list(value).await)
            }
            METHOD_RESOURCES_READ => {
                self.require(METHOD_RESOURCES_READ)?;
                let value: ResourceReadParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.resources_read(value).await)
            }
            METHOD_RESOURCES_SUBSCRIBE => {
                self.require(METHOD_RESOURCES_SUBSCRIBE)?;
                let value: ResourceSubscriptionParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.resources_subscribe(value).await)
            }
            METHOD_RESOURCES_UNSUBSCRIBE => {
                self.require(METHOD_RESOURCES_UNSUBSCRIBE)?;
                let value: ResourceSubscriptionParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.resources_unsubscribe(value).await)
            }
            METHOD_PROMPTS_LIST => {
                self.require(METHOD_PROMPTS_LIST)?;
                let value: ListParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.prompts_list(value).await)
            }
            METHOD_PROMPTS_GET => {
                self.require(METHOD_PROMPTS_GET)?;
                let value: PromptGetParams = self.params(Some(&params))?;
                value.validate_mcp().map_err(|_| invalid_params())?;
                validated_result(self.handler.prompts_get(value).await)
            }
            _ => Err(RpcError {
                code: ERROR_METHOD_NOT_FOUND,
                message: "method not found".into(),
                data: None,
            }),
        }
    }
    fn notification_capability(&self, tools: bool, resources: bool) -> Result<(), DispatchError> {
        let Some(capabilities) = &self.capabilities else {
            return Err(DispatchError::NotInitialized);
        };
        let allowed = (tools
            && capabilities
                .tools
                .as_ref()
                .and_then(|c| c.list_changed)
                .unwrap_or(false))
            || (resources
                && capabilities
                    .resources
                    .as_ref()
                    .and_then(|c| c.list_changed)
                    .unwrap_or(false));
        allowed.then_some(()).ok_or(DispatchError::NotSupported)
    }
    fn params<T: serde::de::DeserializeOwned>(&self, value: Option<&Value>) -> Result<T, RpcError> {
        serde_json::from_value(value.cloned().unwrap_or(Value::Object(Default::default())))
            .map_err(|_| invalid_params())
    }
    fn require(&self, method: &str) -> Result<(), RpcError> {
        if !self.initialized_seen {
            return Err(RpcError {
                code: ERROR_NOT_INITIALIZED,
                message: "server is not initialized".into(),
                data: None,
            });
        }
        let caps = self.capabilities.as_ref().ok_or_else(negotiation_error)?;
        let allowed = match method {
            METHOD_TOOLS_LIST | METHOD_TOOLS_CALL => caps.tools.is_some(),
            METHOD_RESOURCES_LIST | METHOD_RESOURCES_READ => caps.resources.is_some(),
            METHOD_RESOURCES_SUBSCRIBE | METHOD_RESOURCES_UNSUBSCRIBE => caps
                .resources
                .as_ref()
                .and_then(|c| c.subscribe)
                .unwrap_or(false),
            METHOD_PROMPTS_LIST | METHOD_PROMPTS_GET => caps.prompts.is_some(),
            _ => false,
        };
        allowed.then_some(()).ok_or(RpcError {
            code: ERROR_CAPABILITY,
            message: "method unavailable in negotiated capabilities".into(),
            data: None,
        })
    }
}
fn validated_result<T: serde::Serialize + ValidateMcp>(
    result: Result<T, RpcError>,
) -> Result<Value, RpcError> {
    let value = result?;
    value.validate_mcp().map_err(|_| invalid_params())?;
    serde_json::to_value(value).map_err(internal_error)
}
fn invalid_params() -> RpcError {
    RpcError {
        code: ERROR_INVALID_PARAMS,
        message: "invalid parameters".into(),
        data: None,
    }
}
fn negotiation_error() -> RpcError {
    RpcError {
        code: -32003,
        message: "invalid MCP negotiation".into(),
        data: None,
    }
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
    use crate::mcp::{
        capabilities::ToolsCapability,
        wire::{decode, try_request},
    };
    struct Fixture;
    #[async_trait]
    impl McpServerHandler for Fixture {
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult {
                protocol_version: params.protocol_version,
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
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
    async fn binds_capabilities_only_after_valid_legacy_handshake() {
        let mut dispatcher = McpServerDispatcher::new(Fixture);
        let before = try_request(RequestId::Number(1), METHOD_TOOLS_LIST, None).unwrap();
        let result = dispatcher.dispatch(before).await;
        assert!(matches!(
            result.body(),
            ResponseBody::Error {
                error: RpcError {
                    code: ERROR_NOT_INITIALIZED,
                    ..
                }
            }
        ));
        let init = serde_json::json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}});
        let result = dispatcher
            .dispatch(try_request(RequestId::Number(2), METHOD_INITIALIZE, Some(init)).unwrap())
            .await;
        assert!(matches!(result.body(), ResponseBody::Result { .. }));
        assert!(!dispatcher.initialized());
        assert_eq!(dispatcher.era(), None);
        assert_eq!(dispatcher.capabilities(), None);
        assert!(
            dispatcher
                .notify(
                    decode(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                        .unwrap()
                        .into_notification()
                        .unwrap()
                )
                .await
                .is_ok()
        );
        let result = dispatcher
            .dispatch(try_request(RequestId::Number(3), METHOD_TOOLS_LIST, None).unwrap())
            .await;
        assert!(matches!(result.body(), ResponseBody::Result { .. }));
        assert!(
            dispatcher
                .notify(
                    decode(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                        .unwrap()
                        .into_notification()
                        .unwrap()
                )
                .await
                .is_err()
        );
    }
}
