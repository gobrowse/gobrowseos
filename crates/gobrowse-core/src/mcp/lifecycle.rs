//! Generation-fenced MCP lifecycle state and pending-request bookkeeping.

use std::collections::BTreeSet;

use super::{McpProtocolEra, capabilities::ServerCapabilities, wire::RequestId};

pub const MAX_PENDING_REQUESTS: usize = 64;
pub const MAX_RECONNECT_ATTEMPTS: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Disconnected,
    Connecting,
    Discovering,
    Initializing,
    Ready,
    Closing,
    Closed,
    Failed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    SafeDiscovery,
    SafePing,
    SafeList,
    SafeRead,
    Never,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("illegal MCP session lifecycle operation")]
    IllegalTransition,
    #[error("MCP session is not ready")]
    NotReady,
    #[error("MCP request capacity is exhausted")]
    PendingLimit,
    #[error("MCP request id is already pending")]
    DuplicateRequest,
    #[error("MCP response id is not pending for this generation")]
    UnknownResponse,
    #[error("MCP negotiation is not complete")]
    NegotiationRequired,
    #[error("MCP reconnect budget is exhausted")]
    ReconnectExhausted,
}

#[derive(Debug)]
pub struct SessionLifecycle {
    state: SessionState,
    generation: u64,
    era: Option<McpProtocolEra>,
    capabilities: Option<ServerCapabilities>,
    legacy_initialized: bool,
    pending: BTreeSet<(u64, RequestId)>,
    reconnect_attempts: u32,
    next_id: i64,
}
impl Default for SessionLifecycle {
    fn default() -> Self {
        Self::new()
    }
}
impl SessionLifecycle {
    pub fn new() -> Self {
        Self {
            state: SessionState::Disconnected,
            generation: 0,
            era: None,
            capabilities: None,
            legacy_initialized: false,
            pending: BTreeSet::new(),
            reconnect_attempts: 0,
            next_id: 0,
        }
    }
    pub const fn state(&self) -> SessionState {
        self.state
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn era(&self) -> Option<McpProtocolEra> {
        self.era
    }
    pub const fn capabilities(&self) -> Option<&ServerCapabilities> {
        self.capabilities.as_ref()
    }
    pub const fn reconnect_attempts(&self) -> u32 {
        self.reconnect_attempts
    }

    pub fn begin_connect(&mut self) -> Result<u64, LifecycleError> {
        if !matches!(
            self.state,
            SessionState::Disconnected | SessionState::Closed | SessionState::Failed
        ) {
            return Err(LifecycleError::IllegalTransition);
        }
        self.generation = self.generation.wrapping_add(1);
        self.state = SessionState::Connecting;
        self.era = None;
        self.capabilities = None;
        self.legacy_initialized = false;
        self.pending.clear();
        self.next_id = 0;
        Ok(self.generation)
    }
    pub fn begin_discovery(&mut self) -> Result<(), LifecycleError> {
        if self.state == SessionState::Connecting {
            self.state = SessionState::Discovering;
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition)
        }
    }
    pub fn begin_initialize(&mut self) -> Result<(), LifecycleError> {
        if self.state == SessionState::Connecting {
            self.state = SessionState::Initializing;
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition)
        }
    }
    pub fn bind_negotiation(
        &mut self,
        era: McpProtocolEra,
        capabilities: ServerCapabilities,
    ) -> Result<(), LifecycleError> {
        if !matches!(
            self.state,
            SessionState::Discovering | SessionState::Initializing
        ) || !super::SUPPORTED_PROTOCOL_ERAS.contains(&era)
        {
            return Err(LifecycleError::NegotiationRequired);
        }
        self.era = Some(era);
        self.capabilities = Some(capabilities);
        Ok(())
    }
    pub fn mark_legacy_initialized(&mut self) -> Result<(), LifecycleError> {
        if self.state == SessionState::Initializing
            && self.era == Some(McpProtocolEra::Legacy20251125)
            && self.capabilities.is_some()
            && !self.legacy_initialized
        {
            self.legacy_initialized = true;
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition)
        }
    }
    pub fn mark_ready(&mut self) -> Result<(), LifecycleError> {
        if self.era.is_none()
            || self.capabilities.is_none()
            || (self.era == Some(McpProtocolEra::Legacy20251125) && !self.legacy_initialized)
        {
            return Err(LifecycleError::NegotiationRequired);
        }
        if !matches!(
            self.state,
            SessionState::Discovering | SessionState::Initializing
        ) {
            return Err(LifecycleError::IllegalTransition);
        }
        self.state = SessionState::Ready;
        self.reconnect_attempts = 0;
        Ok(())
    }
    pub fn allocate_id(&mut self) -> Result<RequestId, LifecycleError> {
        if self.state != SessionState::Ready {
            return Err(LifecycleError::NotReady);
        }
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(LifecycleError::PendingLimit)?;
        Ok(RequestId::Number(self.next_id))
    }
    pub fn reserve(&mut self, id: RequestId) -> Result<(), LifecycleError> {
        if !matches!(self.state, SessionState::Ready | SessionState::Initializing) {
            return Err(LifecycleError::NotReady);
        }
        id.validate().map_err(|_| LifecycleError::UnknownResponse)?;
        let key = (self.generation, id);
        if self.pending.contains(&key) {
            return Err(LifecycleError::DuplicateRequest);
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(LifecycleError::PendingLimit);
        }
        self.pending.insert(key);
        Ok(())
    }
    pub fn complete(&mut self, id: &RequestId) -> Result<(), LifecycleError> {
        self.pending
            .remove(&(self.generation, id.clone()))
            .then_some(())
            .ok_or(LifecycleError::UnknownResponse)
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn begin_close(&mut self) -> Result<(), LifecycleError> {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            return Ok(());
        }
        if matches!(
            self.state,
            SessionState::Connecting
                | SessionState::Discovering
                | SessionState::Initializing
                | SessionState::Ready
                | SessionState::Failed
        ) {
            self.state = SessionState::Closing;
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition)
        }
    }
    pub fn finish_close(&mut self) -> Vec<RequestId> {
        self.state = SessionState::Closed;
        self.clear_session()
    }
    pub fn disconnect(&mut self) -> Vec<RequestId> {
        self.state = SessionState::Disconnected;
        self.clear_session()
    }
    pub fn fail(&mut self) -> Vec<RequestId> {
        self.state = SessionState::Failed;
        self.clear_session()
    }
    pub fn begin_reconnect(&mut self) -> Result<u64, LifecycleError> {
        if self.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
            self.state = SessionState::Failed;
            return Err(LifecycleError::ReconnectExhausted);
        }
        self.reconnect_attempts += 1;
        self.state = SessionState::Disconnected;
        self.clear_session();
        self.begin_connect()
    }
    fn clear_session(&mut self) -> Vec<RequestId> {
        self.era = None;
        self.capabilities = None;
        self.legacy_initialized = false;
        self.drain_pending()
    }
    fn drain_pending(&mut self) -> Vec<RequestId> {
        let ids = self.pending.iter().map(|(_, id)| id.clone()).collect();
        self.pending.clear();
        ids
    }
}

pub fn retry_class(method: &str) -> RetryClass {
    match method {
        "server/discover" => RetryClass::SafeDiscovery,
        "ping" => RetryClass::SafePing,
        "tools/list" | "resources/list" | "prompts/list" => RetryClass::SafeList,
        "resources/read" => RetryClass::SafeRead,
        _ => RetryClass::Never,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn negotiation_is_required_and_reconnect_is_fresh_and_bounded() {
        let mut life = SessionLifecycle::new();
        life.begin_connect().expect("connect");
        life.begin_initialize().expect("initialize");
        assert_eq!(life.mark_ready(), Err(LifecycleError::NegotiationRequired));
        life.bind_negotiation(
            McpProtocolEra::Legacy20251125,
            ServerCapabilities::default(),
        )
        .expect("bind");
        life.mark_legacy_initialized().expect("initialized");
        life.mark_ready().expect("ready");
        let id = life.allocate_id().expect("id");
        life.reserve(id.clone()).expect("reserve");
        assert_eq!(life.reserve(id), Err(LifecycleError::DuplicateRequest));
        let old_generation = life.generation();
        let drained = life.disconnect();
        assert_eq!(drained.len(), 1);
        life.begin_reconnect().expect("reconnect");
        assert!(life.generation() > old_generation);
        assert_eq!(life.era(), None);
        assert_eq!(life.pending_len(), 0);
        assert_eq!(retry_class("tools/call"), RetryClass::Never);
    }
    #[test]
    fn close_is_idempotent_and_drains_pending() {
        let mut life = SessionLifecycle::new();
        life.begin_connect().unwrap();
        life.begin_discovery().unwrap();
        life.bind_negotiation(
            McpProtocolEra::Modern20260728,
            ServerCapabilities::default(),
        )
        .unwrap();
        life.mark_ready().unwrap();
        let id = life.allocate_id().unwrap();
        life.reserve(id).unwrap();
        life.begin_close().unwrap();
        life.begin_close().unwrap();
        assert_eq!(life.finish_close().len(), 1);
        assert_eq!(life.finish_close().len(), 0);
        assert_eq!(life.era(), None);
        assert_eq!(life.capabilities(), None);
    }
}
