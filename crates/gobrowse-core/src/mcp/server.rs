//! Transport-neutral MCP dispatcher with validated negotiation binding.

use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    McpProtocolEra,
    capabilities::{CapabilityNotification, ServerCapabilities, notification_allowed},
    model::*,
    validation::ValidateMcp,
    wire::*,
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

#[derive(Debug, Clone, PartialEq)]
enum NegotiationState {
    Pristine,
    LegacyStaged {
        era: McpProtocolEra,
        capabilities: ServerCapabilities,
    },
    Ready {
        era: McpProtocolEra,
        capabilities: ServerCapabilities,
    },
    Rejected,
}
pub struct McpServerDispatcher<H> {
    handler: H,
    negotiation: NegotiationState,
}
impl<H: McpServerHandler> McpServerDispatcher<H> {
    pub fn new(handler: H) -> Self {
        Self {
            handler,
            negotiation: NegotiationState::Pristine,
        }
    }
    pub fn capabilities(&self) -> Option<&ServerCapabilities> {
        match &self.negotiation {
            NegotiationState::Ready { capabilities, .. } => Some(capabilities),
            _ => None,
        }
    }
    pub fn era(&self) -> Option<McpProtocolEra> {
        match self.negotiation {
            NegotiationState::Ready { era, .. } => Some(era),
            _ => None,
        }
    }
    pub fn initialized(&self) -> bool {
        matches!(self.negotiation, NegotiationState::Ready { .. })
    }

    pub async fn dispatch(&mut self, request: ValidatedRequest) -> ValidatedResponse {
        let id = request.id().clone();
        let method = request.method().to_owned();
        let before = self.negotiation.clone();
        let result = self.dispatch_request(request).await;
        match validated_response_for_method(&method, id.clone(), result) {
            Ok(response) => response,
            Err(_) => {
                if !matches!(self.negotiation, NegotiationState::Rejected) {
                    self.negotiation = before;
                }
                safe_internal_error_response(id)
            }
        }
    }
    pub async fn notify(
        &mut self,
        notification: ValidatedNotification,
    ) -> Result<(), DispatchError> {
        match notification.method() {
            METHOD_INITIALIZED => {
                let NegotiationState::LegacyStaged { era, capabilities } = &self.negotiation else {
                    return Err(DispatchError::Negotiation);
                };
                self.negotiation = NegotiationState::Ready {
                    era: *era,
                    capabilities: capabilities.clone(),
                };
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
            METHOD_TOOLS_LIST_CHANGED => {
                self.notification_capability(CapabilityNotification::ToolsListChanged)
            }
            METHOD_RESOURCES_LIST_CHANGED => {
                self.notification_capability(CapabilityNotification::ResourcesListChanged)
            }
            METHOD_RESOURCE_UPDATED => {
                self.notification_capability(CapabilityNotification::ResourceUpdated)
            }
            METHOD_PROGRESS => Err(DispatchError::NotSupported),
            METHOD_LOGGING_MESSAGE => self.notification_capability(CapabilityNotification::Logging),
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
                if !matches!(self.negotiation, NegotiationState::Pristine) {
                    if matches!(self.negotiation, NegotiationState::LegacyStaged { .. }) {
                        self.negotiation = NegotiationState::Rejected;
                    }
                    return Err(negotiation_error());
                }
                let discovered = self.handler.discover().await?;
                discovered
                    .validate_mcp()
                    .map_err(|_| internal_handler_error())?;
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
                let value = serde_json::to_value(&discovered).map_err(internal_error)?;
                self.negotiation = NegotiationState::Ready {
                    era,
                    capabilities: discovered.capabilities.clone(),
                };
                Ok(value)
            }
            METHOD_INITIALIZE => {
                if !matches!(self.negotiation, NegotiationState::Pristine) {
                    if matches!(self.negotiation, NegotiationState::LegacyStaged { .. }) {
                        self.negotiation = NegotiationState::Rejected;
                    }
                    return Err(negotiation_error());
                }
                let params: InitializeParams = self.params(Some(&params))?;
                params.validate_mcp().map_err(|_| invalid_params())?;
                if params.protocol_version != "2025-11-25" {
                    return Err(negotiation_error());
                }
                let result = self.handler.initialize(params.clone()).await?;
                result
                    .validate_mcp()
                    .map_err(|_| internal_handler_error())?;
                if result.protocol_version != params.protocol_version
                    || super::McpProtocolEra::from_wire_version(&result.protocol_version)
                        != Some(McpProtocolEra::Legacy20251125)
                {
                    return Err(negotiation_error());
                }
                let value = serde_json::to_value(&result).map_err(internal_error)?;
                self.negotiation = NegotiationState::LegacyStaged {
                    era: McpProtocolEra::Legacy20251125,
                    capabilities: result.capabilities.clone(),
                };
                Ok(value)
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
    fn notification_capability(
        &self,
        notification: CapabilityNotification,
    ) -> Result<(), DispatchError> {
        let NegotiationState::Ready { capabilities, .. } = &self.negotiation else {
            return Err(DispatchError::NotInitialized);
        };
        notification_allowed(notification, capabilities)
            .then_some(())
            .ok_or(DispatchError::NotSupported)
    }
    fn params<T: serde::de::DeserializeOwned>(&self, value: Option<&Value>) -> Result<T, RpcError> {
        serde_json::from_value(value.cloned().unwrap_or(Value::Object(Default::default())))
            .map_err(|_| invalid_params())
    }
    fn require(&self, method: &str) -> Result<(), RpcError> {
        let caps = match &self.negotiation {
            NegotiationState::Ready { capabilities, .. } => capabilities,
            _ => {
                return Err(RpcError {
                    code: ERROR_NOT_INITIALIZED,
                    message: "server is not initialized".into(),
                    data: None,
                });
            }
        };
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
    value.validate_mcp().map_err(|_| internal_handler_error())?;
    serde_json::to_value(value).map_err(internal_error)
}

fn internal_handler_error() -> RpcError {
    RpcError {
        code: -32603,
        message: "internal MCP handler error".into(),
        data: None,
    }
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
        wire::{decode, try_notification, try_request},
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
    struct LoggingHandler;
    #[async_trait]
    impl McpServerHandler for LoggingHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Ok(DiscoverResult {
                supported_versions: vec!["2026-07-28".into()],
                capabilities: ServerCapabilities {
                    logging: Some(crate::mcp::capabilities::LoggingCapability {}),
                    ..Default::default()
                },
                server_info: None,
            })
        }
    }
    struct BadHandler;
    #[async_trait]
    impl McpServerHandler for BadHandler {
        async fn ping(&self) -> Result<Value, RpcError> {
            Err(RpcError {
                code: -1,
                message: "x".repeat(1024),
                data: Some(serde_json::json!({"wide": []})),
            })
        }
    }
    struct InvalidOutputHandler;
    #[async_trait]
    impl McpServerHandler for InvalidOutputHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Ok(DiscoverResult {
                supported_versions: vec!["2026-07-28".into()],
                capabilities: ServerCapabilities {
                    resources: Some(crate::mcp::capabilities::ResourcesCapability {
                        subscribe: Some(true),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                server_info: None,
            })
        }
        async fn ping(&self) -> Result<Value, RpcError> {
            Ok(serde_json::json!({"unexpected": true}))
        }
        async fn resources_subscribe(
            &self,
            _: ResourceSubscriptionParams,
        ) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
        async fn resources_unsubscribe(
            &self,
            _: ResourceSubscriptionParams,
        ) -> Result<Value, RpcError> {
            Ok(serde_json::json!(["unexpected"]))
        }
    }
    struct SemanticallyInvalidTypedHandler;
    #[async_trait]
    impl McpServerHandler for SemanticallyInvalidTypedHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Ok(DiscoverResult {
                supported_versions: vec!["2026-07-28".into()],
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability::default()),
                    ..Default::default()
                },
                server_info: None,
            })
        }

        async fn tools_list(&self, _: ListParams) -> Result<Paginated<Tool>, RpcError> {
            let duplicate = Tool {
                name: "duplicate".into(),
                title: None,
                description: None,
                input_schema: json!({"type": "object"}),
                output_schema: None,
            };
            Ok(Paginated {
                items: vec![duplicate.clone(), duplicate],
                next_cursor: None,
            })
        }
    }
    struct InvalidDiscoverThenValidHandler {
        returned_invalid: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl McpServerHandler for InvalidDiscoverThenValidHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            if !self
                .returned_invalid
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(DiscoverResult {
                    supported_versions: vec!["2026-07-28".into(), "2026-07-28".into()],
                    capabilities: ServerCapabilities::default(),
                    server_info: None,
                });
            }
            Ok(DiscoverResult {
                supported_versions: vec!["2026-07-28".into()],
                capabilities: ServerCapabilities::default(),
                server_info: None,
            })
        }
    }
    struct InvalidInitializeThenValidHandler {
        returned_invalid: std::sync::atomic::AtomicBool,
    }
    #[async_trait]
    impl McpServerHandler for InvalidInitializeThenValidHandler {
        async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult, RpcError> {
            if !self
                .returned_invalid
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return Ok(InitializeResult {
                    protocol_version: params.protocol_version,
                    capabilities: ServerCapabilities::default(),
                    server_info: ClientInfo {
                        name: String::new(),
                        version: "1".into(),
                    },
                    instructions: None,
                });
            }
            Ok(InitializeResult {
                protocol_version: params.protocol_version,
                capabilities: ServerCapabilities::default(),
                server_info: ClientInfo {
                    name: "fixture".into(),
                    version: "1".into(),
                },
                instructions: None,
            })
        }
    }
    struct IntentionalNegotiationErrorHandler;
    #[async_trait]
    impl McpServerHandler for IntentionalNegotiationErrorHandler {
        async fn discover(&self) -> Result<DiscoverResult, RpcError> {
            Err(intentional_negotiation_error())
        }
        async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult, RpcError> {
            Err(intentional_negotiation_error())
        }
    }
    struct ProtocolMismatchHandler;
    #[async_trait]
    impl McpServerHandler for ProtocolMismatchHandler {
        async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult, RpcError> {
            Ok(InitializeResult {
                protocol_version: "2026-07-28".into(),
                capabilities: ServerCapabilities::default(),
                server_info: ClientInfo {
                    name: "fixture".into(),
                    version: "1".into(),
                },
                instructions: None,
            })
        }
    }
    fn intentional_negotiation_error() -> RpcError {
        RpcError {
            code: -32099,
            message: "intentional negotiation error".into(),
            data: Some(json!({"reason": "intentional"})),
        }
    }
    fn legacy_initialize_params() -> Value {
        json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "test", "version": "1"},
        })
    }
    #[tokio::test]
    async fn logging_notification_requires_negotiated_logging_capability() {
        let mut dispatcher = McpServerDispatcher::new(LoggingHandler);
        dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_DISCOVER, None).unwrap())
            .await;
        let notification = try_notification(
            METHOD_LOGGING_MESSAGE,
            Some(serde_json::json!({"level":"info","data":{}})),
        )
        .unwrap();
        assert_eq!(dispatcher.notify(notification).await, Ok(()));
    }
    #[tokio::test]
    async fn invalid_handler_errors_use_a_safe_non_panicking_fallback() {
        let mut dispatcher = McpServerDispatcher::new(BadHandler);
        let response = dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_PING, None).unwrap())
            .await;
        assert!(
            matches!(response.body(), ResponseBody::Error { error: RpcError { code: -32603, message, data: None } } if message == "internal MCP handler error")
        );
    }
    #[tokio::test]
    async fn invalid_ping_and_subscription_outputs_use_the_same_safe_fallback() {
        let mut dispatcher = McpServerDispatcher::new(InvalidOutputHandler);
        let discover = dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_DISCOVER, None).unwrap())
            .await;
        assert!(matches!(discover.body(), ResponseBody::Result { .. }));
        assert_eq!(
            dispatcher.era(),
            Some(crate::mcp::McpProtocolEra::Modern20260728)
        );

        for (id, method) in [
            (2, METHOD_PING),
            (3, METHOD_RESOURCES_SUBSCRIBE),
            (4, METHOD_RESOURCES_UNSUBSCRIBE),
        ] {
            let params = (method != METHOD_PING).then(|| serde_json::json!({"uri":"urn:test"}));
            let response = dispatcher
                .dispatch(try_request(RequestId::Number(id), method, params).unwrap())
                .await;
            assert!(matches!(
                response.body(),
                ResponseBody::Error { error: RpcError { code: -32603, message, data: None } }
                    if message == "internal MCP handler error"
            ));
            assert_eq!(response.id(), &RequestId::Number(id));
            let encoded = encode(&ValidatedMessage::Response(response)).expect("fallback frame");
            assert!(encoded.len() < MAX_FRAME_BYTES);
        }
    }

    #[tokio::test]
    async fn semantically_invalid_typed_handler_result_uses_internal_fallback() {
        let mut dispatcher = McpServerDispatcher::new(SemanticallyInvalidTypedHandler);
        let discover = dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_DISCOVER, None).unwrap())
            .await;
        assert!(matches!(discover.body(), ResponseBody::Result { .. }));

        let response = dispatcher
            .dispatch(
                try_request(RequestId::Number(2), METHOD_TOOLS_LIST, Some(json!({}))).unwrap(),
            )
            .await;
        assert!(matches!(
            response.body(),
            ResponseBody::Error { error: RpcError { code: -32603, message, data: None } }
                if message == "internal MCP handler error"
        ));
        assert_eq!(response.id(), &RequestId::Number(2));
        assert!(
            encode(&ValidatedMessage::Response(response))
                .expect("fallback frame")
                .len()
                < MAX_FRAME_BYTES
        );
    }

    #[tokio::test]
    async fn invalid_discover_result_uses_redacted_fallback_and_rolls_back() {
        let mut dispatcher = McpServerDispatcher::new(InvalidDiscoverThenValidHandler {
            returned_invalid: std::sync::atomic::AtomicBool::new(false),
        });

        let response = dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_DISCOVER, None).unwrap())
            .await;
        assert!(matches!(
            response.body(),
            ResponseBody::Error { error: RpcError { code: -32603, message, data: None } }
                if message == "internal MCP handler error"
        ));
        assert_eq!(response.id(), &RequestId::Number(1));
        assert!(
            encode(&ValidatedMessage::Response(response))
                .expect("fallback frame")
                .len()
                < MAX_FRAME_BYTES
        );
        assert!(!dispatcher.initialized());
        assert_eq!(dispatcher.era(), None);
        assert_eq!(dispatcher.capabilities(), None);

        let valid = dispatcher
            .dispatch(try_request(RequestId::Number(2), METHOD_DISCOVER, None).unwrap())
            .await;
        assert!(matches!(valid.body(), ResponseBody::Result { .. }));
        assert!(dispatcher.initialized());
        assert_eq!(
            dispatcher.era(),
            Some(crate::mcp::McpProtocolEra::Modern20260728)
        );
    }

    #[tokio::test]
    async fn invalid_initialize_result_uses_redacted_fallback_and_rolls_back() {
        let mut dispatcher = McpServerDispatcher::new(InvalidInitializeThenValidHandler {
            returned_invalid: std::sync::atomic::AtomicBool::new(false),
        });

        let response = dispatcher
            .dispatch(
                try_request(
                    RequestId::Number(1),
                    METHOD_INITIALIZE,
                    Some(legacy_initialize_params()),
                )
                .unwrap(),
            )
            .await;
        assert!(matches!(
            response.body(),
            ResponseBody::Error { error: RpcError { code: -32603, message, data: None } }
                if message == "internal MCP handler error"
        ));
        assert_eq!(response.id(), &RequestId::Number(1));
        assert!(
            encode(&ValidatedMessage::Response(response))
                .expect("fallback frame")
                .len()
                < MAX_FRAME_BYTES
        );
        assert!(!dispatcher.initialized());
        assert_eq!(dispatcher.era(), None);
        assert_eq!(dispatcher.capabilities(), None);

        let valid = dispatcher
            .dispatch(
                try_request(
                    RequestId::Number(2),
                    METHOD_INITIALIZE,
                    Some(legacy_initialize_params()),
                )
                .unwrap(),
            )
            .await;
        assert!(matches!(valid.body(), ResponseBody::Result { .. }));
        assert!(!dispatcher.initialized());
        assert_eq!(dispatcher.era(), None);
        assert_eq!(dispatcher.capabilities(), None);
    }

    #[tokio::test]
    async fn intentional_negotiation_handler_errors_are_preserved() {
        let mut dispatcher = McpServerDispatcher::new(IntentionalNegotiationErrorHandler);

        let discover = dispatcher
            .dispatch(try_request(RequestId::Number(1), METHOD_DISCOVER, None).unwrap())
            .await;
        assert_eq!(discover.id(), &RequestId::Number(1));
        assert_eq!(
            discover.body(),
            &ResponseBody::Error {
                error: intentional_negotiation_error(),
            }
        );

        let initialize = dispatcher
            .dispatch(
                try_request(
                    RequestId::Number(2),
                    METHOD_INITIALIZE,
                    Some(legacy_initialize_params()),
                )
                .unwrap(),
            )
            .await;
        assert_eq!(initialize.id(), &RequestId::Number(2));
        assert_eq!(
            initialize.body(),
            &ResponseBody::Error {
                error: intentional_negotiation_error(),
            }
        );
    }

    #[tokio::test]
    async fn protocol_version_mismatch_remains_negotiation_error_without_binding() {
        let mut dispatcher = McpServerDispatcher::new(ProtocolMismatchHandler);

        let response = dispatcher
            .dispatch(
                try_request(
                    RequestId::Number(1),
                    METHOD_INITIALIZE,
                    Some(legacy_initialize_params()),
                )
                .unwrap(),
            )
            .await;
        assert!(matches!(
            response.body(),
            ResponseBody::Error {
                error: RpcError { code: -32003, .. }
            }
        ));
        assert_eq!(response.id(), &RequestId::Number(1));
        assert!(!dispatcher.initialized());
        assert_eq!(dispatcher.era(), None);
        assert_eq!(dispatcher.capabilities(), None);
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
        let mixed = dispatcher
            .dispatch(try_request(RequestId::Number(22), METHOD_DISCOVER, None).unwrap())
            .await;
        assert!(matches!(
            mixed.body(),
            ResponseBody::Error {
                error: RpcError { code: -32003, .. }
            }
        ));
        assert_eq!(dispatcher.era(), None);
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
        let result = dispatcher
            .dispatch(try_request(RequestId::Number(3), METHOD_TOOLS_LIST, None).unwrap())
            .await;
        assert!(matches!(result.body(), ResponseBody::Error { .. }));
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
