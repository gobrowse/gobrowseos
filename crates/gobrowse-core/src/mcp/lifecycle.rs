//! Bounded MCP session lifecycle and pending-request fencing.

use std::collections::BTreeSet;

use super::{McpProtocolEra, wire::RequestId};

pub const MAX_PENDING_REQUESTS: usize = 64;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LifecycleError {
    #[error("illegal MCP session lifecycle transition")]
    IllegalTransition,
    #[error("MCP session is not ready")]
    NotReady,
    #[error("MCP request capacity is exhausted")]
    PendingLimit,
    #[error("MCP request id is already pending")]
    DuplicateRequest,
    #[error("MCP response id is not pending")]
    UnknownResponse,
}

#[derive(Debug)]
pub struct SessionLifecycle {
    state: SessionState,
    era: Option<McpProtocolEra>,
    pending: BTreeSet<RequestId>,
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
            era: None,
            pending: BTreeSet::new(),
        }
    }

    pub const fn state(&self) -> SessionState {
        self.state
    }
    pub const fn era(&self) -> Option<McpProtocolEra> {
        self.era
    }

    pub fn transition(&mut self, next: SessionState) -> Result<(), LifecycleError> {
        let legal = matches!(
            (self.state, next),
            (SessionState::Disconnected, SessionState::Connecting)
                | (SessionState::Connecting, SessionState::Discovering)
                | (SessionState::Connecting, SessionState::Initializing)
                | (SessionState::Discovering, SessionState::Ready)
                | (SessionState::Initializing, SessionState::Ready)
                | (SessionState::Ready, SessionState::Closing)
                | (SessionState::Ready, SessionState::Failed)
                | (SessionState::Ready, SessionState::Disconnected)
                | (SessionState::Connecting, SessionState::Failed)
                | (SessionState::Discovering, SessionState::Failed)
                | (SessionState::Initializing, SessionState::Failed)
                | (SessionState::Failed, SessionState::Disconnected)
                | (SessionState::Closing, SessionState::Closed)
                | (SessionState::Closed, SessionState::Disconnected)
        );
        if !legal {
            return Err(LifecycleError::IllegalTransition);
        }
        self.state = next;
        if matches!(
            next,
            SessionState::Closed | SessionState::Failed | SessionState::Disconnected
        ) {
            self.pending.clear();
        }
        Ok(())
    }

    pub fn set_era(&mut self, era: McpProtocolEra) -> Result<(), LifecycleError> {
        if !matches!(
            self.state,
            SessionState::Discovering | SessionState::Initializing
        ) {
            return Err(LifecycleError::IllegalTransition);
        }
        self.era = Some(era);
        Ok(())
    }

    pub fn reserve(&mut self, id: RequestId) -> Result<(), LifecycleError> {
        if self.state != SessionState::Ready && self.state != SessionState::Initializing {
            return Err(LifecycleError::NotReady);
        }
        if self.pending.len() >= MAX_PENDING_REQUESTS {
            return Err(LifecycleError::PendingLimit);
        }
        if !self.pending.insert(id) {
            return Err(LifecycleError::DuplicateRequest);
        }
        Ok(())
    }

    pub fn complete(&mut self, id: &RequestId) -> Result<(), LifecycleError> {
        self.pending
            .remove(id)
            .then_some(())
            .ok_or(LifecycleError::UnknownResponse)
    }

    pub fn cancel(&mut self, id: &RequestId) -> Result<(), LifecycleError> {
        self.complete(id)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::wire::RequestId;

    #[test]
    fn modern_lifecycle_and_pending_fencing_are_bounded() {
        let mut lifecycle = SessionLifecycle::new();
        assert!(lifecycle.transition(SessionState::Ready).is_err());
        lifecycle
            .transition(SessionState::Connecting)
            .expect("connect");
        lifecycle
            .transition(SessionState::Discovering)
            .expect("discover");
        lifecycle
            .set_era(McpProtocolEra::Modern20260728)
            .expect("era");
        lifecycle.transition(SessionState::Ready).expect("ready");
        let id = RequestId::Number(1);
        lifecycle.reserve(id.clone()).expect("reserve");
        assert_eq!(
            lifecycle.reserve(id.clone()),
            Err(LifecycleError::DuplicateRequest)
        );
        assert_eq!(
            lifecycle.complete(&RequestId::Number(2)),
            Err(LifecycleError::UnknownResponse)
        );
        lifecycle.cancel(&id).expect("cancel");
        lifecycle
            .transition(SessionState::Closing)
            .expect("closing");
        lifecycle.transition(SessionState::Closed).expect("closed");
        assert_eq!(lifecycle.pending_len(), 0);
    }
}
