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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingToken {
    generation: u64,
    id: RequestId,
}
impl PendingToken {
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn id(&self) -> &RequestId {
        &self.id
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectResult {
    pub generation: u64,
    pub drained: Vec<PendingToken>,
}
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("illegal MCP session lifecycle operation")]
    IllegalTransition,
    #[error("MCP session is not ready")]
    NotReady,
    #[error("MCP request capacity is exhausted")]
    PendingLimit,
    #[error("MCP request id is already pending")]
    DuplicateRequest,
    #[error("MCP response token is not pending for this generation")]
    UnknownResponse,
    #[error("MCP negotiation is not complete")]
    NegotiationRequired,
    #[error("MCP reconnect budget is exhausted")]
    ReconnectExhausted { drained: Vec<PendingToken> },
    #[error("MCP request or generation id is exhausted")]
    IdExhausted,
}

#[derive(Debug)]
pub struct SessionLifecycle {
    state: SessionState,
    generation: u64,
    era: Option<McpProtocolEra>,
    capabilities: Option<ServerCapabilities>,
    legacy_initialized: bool,
    pending: BTreeSet<PendingKey>,
    reconnect_attempts: u32,
    next_id: i64,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PendingKey {
    generation: u64,
    id: RequestId,
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
        ) || !self.pending.is_empty()
        {
            return Err(LifecycleError::IllegalTransition);
        }
        self.reconnect_attempts = 0;
        self.begin_connect_inner()
    }
    fn begin_connect_inner(&mut self) -> Result<u64, LifecycleError> {
        if !matches!(
            self.state,
            SessionState::Disconnected | SessionState::Closed | SessionState::Failed
        ) || !self.pending.is_empty()
        {
            return Err(LifecycleError::IllegalTransition);
        }
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or(LifecycleError::IdExhausted)?;
        self.state = SessionState::Connecting;
        self.era = None;
        self.capabilities = None;
        self.legacy_initialized = false;
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
        let path_ok = (era.is_modern() && self.state == SessionState::Discovering)
            || (!era.is_modern() && self.state == SessionState::Initializing);
        if !path_ok || !super::SUPPORTED_PROTOCOL_ERAS.contains(&era) {
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
    pub fn allocate_and_reserve(&mut self) -> Result<PendingToken, LifecycleError> {
        if self.state != SessionState::Ready {
            return Err(LifecycleError::NotReady);
        }
        let id = self
            .next_id
            .checked_add(1)
            .ok_or(LifecycleError::IdExhausted)?;
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(LifecycleError::PendingLimit);
        }
        self.next_id = id;
        let token = PendingToken {
            generation: self.generation,
            id: RequestId::Number(id),
        };
        self.pending.insert(PendingKey {
            generation: token.generation,
            id: token.id.clone(),
        });
        Ok(token)
    }
    pub fn reserve(&mut self, token: PendingToken) -> Result<(), LifecycleError> {
        if !matches!(self.state, SessionState::Ready | SessionState::Initializing) {
            return Err(LifecycleError::NotReady);
        }
        if token.generation != self.generation {
            return Err(LifecycleError::UnknownResponse);
        }
        token
            .id
            .validate()
            .map_err(|_| LifecycleError::UnknownResponse)?;
        if self
            .pending
            .iter()
            .any(|key| key.generation == token.generation && key.id == token.id)
        {
            return Err(LifecycleError::DuplicateRequest);
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(LifecycleError::PendingLimit);
        }
        self.pending.insert(PendingKey {
            generation: token.generation,
            id: token.id,
        });
        Ok(())
    }
    pub fn complete(&mut self, token: &PendingToken) -> Result<(), LifecycleError> {
        if self.state == SessionState::Closing
            || matches!(
                self.state,
                SessionState::Closed | SessionState::Failed | SessionState::Disconnected
            )
        {
            return Err(LifecycleError::UnknownResponse);
        }
        self.pending
            .remove(&PendingKey {
                generation: token.generation,
                id: token.id.clone(),
            })
            .then_some(())
            .ok_or(LifecycleError::UnknownResponse)
    }
    pub fn cancel(&mut self, token: &PendingToken) -> Result<(), LifecycleError> {
        self.complete(token)
    }
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
    pub fn begin_close(&mut self) -> Result<Vec<PendingToken>, LifecycleError> {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            return Ok(Vec::new());
        }
        if !matches!(
            self.state,
            SessionState::Connecting
                | SessionState::Discovering
                | SessionState::Initializing
                | SessionState::Ready
                | SessionState::Failed
        ) {
            return Err(LifecycleError::IllegalTransition);
        }
        self.state = SessionState::Closing;
        Ok(self.drain_pending())
    }
    pub fn finish_close(&mut self) -> Result<(), LifecycleError> {
        if self.state == SessionState::Closing {
            self.state = SessionState::Closed;
            self.clear_session();
            Ok(())
        } else if self.state == SessionState::Closed {
            Ok(())
        } else {
            Err(LifecycleError::IllegalTransition)
        }
    }
    pub fn disconnect(&mut self) -> Vec<PendingToken> {
        self.state = SessionState::Disconnected;
        self.clear_session();
        self.drain_pending()
    }
    pub fn fail(&mut self) -> Vec<PendingToken> {
        self.state = SessionState::Failed;
        self.clear_session();
        self.drain_pending()
    }
    pub fn begin_reconnect(&mut self) -> Result<ReconnectResult, LifecycleError> {
        if !matches!(
            self.state,
            SessionState::Ready
                | SessionState::Failed
                | SessionState::Disconnected
                | SessionState::Closed
        ) {
            return Err(LifecycleError::IllegalTransition);
        }
        if self.generation.checked_add(1).is_none() {
            return Err(LifecycleError::IdExhausted);
        }
        let drained = self.drain_pending();
        if self.reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
            self.state = SessionState::Failed;
            self.clear_session();
            return Err(LifecycleError::ReconnectExhausted { drained });
        }
        self.reconnect_attempts += 1;
        self.state = SessionState::Disconnected;
        self.clear_session();
        let generation = self.begin_connect_inner()?;
        Ok(ReconnectResult {
            generation,
            drained,
        })
    }
    fn clear_session(&mut self) {
        self.era = None;
        self.capabilities = None;
        self.legacy_initialized = false;
    }
    fn drain_pending(&mut self) -> Vec<PendingToken> {
        let tokens = self
            .pending
            .iter()
            .map(|key| PendingToken {
                generation: key.generation,
                id: key.id.clone(),
            })
            .collect();
        self.pending.clear();
        tokens
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
    fn modern_ready() -> SessionLifecycle {
        let mut life = SessionLifecycle::new();
        life.begin_connect().unwrap();
        life.begin_discovery().unwrap();
        life.bind_negotiation(
            McpProtocolEra::Modern20260728,
            ServerCapabilities::default(),
        )
        .unwrap();
        life.mark_ready().unwrap();
        life
    }
    #[test]
    fn generation_fences_and_reconnect_drains_without_resetting_ids() {
        let mut life = modern_ready();
        let first = life.allocate_and_reserve().unwrap();
        let old_generation = first.generation();
        let drained = life.disconnect();
        assert_eq!(drained, vec![first]);
        let reconnect = life.begin_reconnect().unwrap();
        assert!(reconnect.generation > old_generation);
        life.begin_discovery().unwrap();
        life.bind_negotiation(
            McpProtocolEra::Modern20260728,
            ServerCapabilities::default(),
        )
        .unwrap();
        life.mark_ready().unwrap();
        let second = life.allocate_and_reserve().unwrap();
        assert!(matches!(second.id(), RequestId::Number(value) if *value > 1));
        assert_eq!(
            life.complete(&PendingToken {
                generation: old_generation,
                id: RequestId::Number(1)
            }),
            Err(LifecycleError::UnknownResponse)
        );
    }
    #[test]
    fn close_drains_immediately_and_is_idempotent() {
        let mut life = modern_ready();
        let token = life.allocate_and_reserve().unwrap();
        let drained = life.begin_close().unwrap();
        assert_eq!(drained, vec![token]);
        assert_eq!(life.begin_close().unwrap(), Vec::new());
        life.finish_close().unwrap();
        assert_eq!(life.finish_close(), Ok(()));
    }
    #[test]
    fn legacy_path_and_id_exhaustion_are_boundaries() {
        let mut life = SessionLifecycle::new();
        life.begin_connect().unwrap();
        assert_eq!(
            life.bind_negotiation(
                McpProtocolEra::Legacy20251125,
                ServerCapabilities::default()
            ),
            Err(LifecycleError::NegotiationRequired)
        );
        life.begin_initialize().unwrap();
        life.bind_negotiation(
            McpProtocolEra::Legacy20251125,
            ServerCapabilities::default(),
        )
        .unwrap();
        assert_eq!(life.mark_ready(), Err(LifecycleError::NegotiationRequired));
        life.mark_legacy_initialized().unwrap();
        life.mark_ready().unwrap();
        assert_eq!(retry_class("tools/call"), RetryClass::Never);
    }
}
