use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use gobrowse_core::sandbox::ResponseEnvelope;
use tokio::sync::Notify;
use uuid::Uuid;

pub enum ReplayDecision {
    Execute,
    Cached(ResponseEnvelope),
    Wait(Arc<Notify>),
    Conflict,
    Full,
}

enum ReplayState {
    Pending(Arc<Notify>),
    Complete(ResponseEnvelope),
}

struct ReplayEntry {
    fingerprint: [u8; 32],
    state: ReplayState,
}

struct ReplayInner {
    entries: HashMap<Uuid, ReplayEntry>,
    order: VecDeque<Uuid>,
}

pub struct ReplayCache {
    capacity: usize,
    inner: Mutex<ReplayInner>,
}

impl ReplayCache {
    pub fn new(capacity: usize) -> Option<Self> {
        (capacity > 0).then(|| Self {
            capacity,
            inner: Mutex::new(ReplayInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
        })
    }

    pub fn begin(&self, request_id: Uuid, fingerprint: [u8; 32]) -> ReplayDecision {
        let mut inner = self.inner.lock().expect("replay cache mutex poisoned");
        if let Some(entry) = inner.entries.get(&request_id) {
            if entry.fingerprint != fingerprint {
                return ReplayDecision::Conflict;
            }
            return match &entry.state {
                ReplayState::Pending(notify) => ReplayDecision::Wait(Arc::clone(notify)),
                ReplayState::Complete(response) => ReplayDecision::Cached(response.clone()),
            };
        }

        if inner.entries.len() == self.capacity {
            let Some(index) = inner.order.iter().position(|candidate| {
                inner
                    .entries
                    .get(candidate)
                    .is_some_and(|entry| matches!(entry.state, ReplayState::Complete(_)))
            }) else {
                return ReplayDecision::Full;
            };
            let evicted = inner.order.remove(index).expect("replay index exists");
            inner.entries.remove(&evicted);
        }

        let notify = Arc::new(Notify::new());
        inner.entries.insert(
            request_id,
            ReplayEntry {
                fingerprint,
                state: ReplayState::Pending(notify),
            },
        );
        inner.order.push_back(request_id);
        ReplayDecision::Execute
    }

    pub fn complete(&self, request_id: Uuid, fingerprint: [u8; 32], response: ResponseEnvelope) {
        let notify = {
            let mut inner = self.inner.lock().expect("replay cache mutex poisoned");
            let Some(entry) = inner.entries.get_mut(&request_id) else {
                return;
            };
            if entry.fingerprint != fingerprint {
                return;
            }
            let ReplayState::Pending(notify) = &entry.state else {
                return;
            };
            let notify = Arc::clone(notify);
            entry.state = ReplayState::Complete(response);
            notify
        };
        notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use gobrowse_core::sandbox::{SANDBOX_PROTOCOL_VERSION, SandboxResult};

    use super::*;

    fn response(request_id: Uuid) -> ResponseEnvelope {
        ResponseEnvelope {
            version: SANDBOX_PROTOCOL_VERSION,
            request_id,
            result: Ok(SandboxResult::Terminated),
        }
    }

    #[test]
    fn cache_rejects_conflicts_and_evicts_only_completed_entries() {
        let cache = ReplayCache::new(1).unwrap();
        let first = Uuid::new_v4();
        assert!(matches!(
            cache.begin(first, [1; 32]),
            ReplayDecision::Execute
        ));
        assert!(matches!(
            cache.begin(first, [2; 32]),
            ReplayDecision::Conflict
        ));
        assert!(matches!(
            cache.begin(Uuid::new_v4(), [3; 32]),
            ReplayDecision::Full
        ));
        cache.complete(first, [1; 32], response(first));
        assert!(matches!(
            cache.begin(Uuid::new_v4(), [3; 32]),
            ReplayDecision::Execute
        ));
    }
}
