//! Deterministic provider used by agent and integration tests without paid model calls.

use std::{collections::VecDeque, sync::Mutex};

use async_trait::async_trait;
use futures_util::stream;

use crate::model::{ModelEvent, ModelProvider, ModelRequest, ModelStream, ProviderError};

#[derive(Debug, Clone)]
pub enum FakeTurn {
    Reject(ProviderError),
    Events(Vec<Result<ModelEvent, ProviderError>>),
}

#[derive(Debug)]
pub struct FakeModelProvider {
    id: String,
    turns: Mutex<VecDeque<FakeTurn>>,
}

impl FakeModelProvider {
    pub fn new(id: impl Into<String>, turns: impl IntoIterator<Item = FakeTurn>) -> Self {
        Self {
            id: id.into(),
            turns: Mutex::new(turns.into_iter().collect()),
        }
    }

    pub fn remaining_turns(&self) -> usize {
        self.turns.lock().expect("fake model mutex poisoned").len()
    }
}

#[async_trait]
impl ModelProvider for FakeModelProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn stream(&self, _request: ModelRequest) -> Result<ModelStream, ProviderError> {
        match self
            .turns
            .lock()
            .expect("fake model mutex poisoned")
            .pop_front()
        {
            Some(FakeTurn::Reject(error)) => Err(error),
            Some(FakeTurn::Events(events)) => Ok(Box::pin(stream::iter(events))),
            None => Err(ProviderError::InvalidResponse),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, sync::Arc};

    use futures_util::StreamExt;

    use super::*;
    use crate::model::{ModelIdentity, ModelRoute, open_with_fallback};

    fn request() -> ModelRequest {
        ModelRequest {
            model: ModelIdentity {
                provider: "unused".into(),
                model: "unused".into(),
            },
            messages: vec![],
            tools: vec![],
            max_output_tokens: 100,
            required_capabilities: BTreeSet::new(),
        }
    }

    #[tokio::test]
    async fn deterministic_fallback_selects_second_provider() {
        let first = Arc::new(FakeModelProvider::new(
            "first",
            [FakeTurn::Reject(ProviderError::RateLimited {
                retry_after_seconds: Some(1),
            })],
        ));
        let second = Arc::new(FakeModelProvider::new(
            "second",
            [FakeTurn::Events(vec![
                Ok(ModelEvent::TextDelta {
                    text: "deterministic".into(),
                }),
                Ok(ModelEvent::Completed),
            ])],
        ));
        let routes = vec![
            ModelRoute {
                provider: first,
                identity: ModelIdentity {
                    provider: "first".into(),
                    model: "a".into(),
                },
            },
            ModelRoute {
                provider: second,
                identity: ModelIdentity {
                    provider: "second".into(),
                    model: "b".into(),
                },
            },
        ];
        let (selected, mut stream) = open_with_fallback(&routes, &request()).await.unwrap();
        assert_eq!(selected.provider, "second");
        assert!(matches!(
            stream.next().await,
            Some(Ok(ModelEvent::TextDelta { .. }))
        ));
    }

    #[tokio::test]
    async fn malformed_response_does_not_fallback() {
        let first = Arc::new(FakeModelProvider::new(
            "first",
            [FakeTurn::Reject(ProviderError::InvalidResponse)],
        ));
        let second = Arc::new(FakeModelProvider::new(
            "second",
            [FakeTurn::Events(vec![Ok(ModelEvent::Completed)])],
        ));
        let routes = vec![
            ModelRoute {
                provider: first,
                identity: ModelIdentity {
                    provider: "first".into(),
                    model: "a".into(),
                },
            },
            ModelRoute {
                provider: second.clone(),
                identity: ModelIdentity {
                    provider: "second".into(),
                    model: "b".into(),
                },
            },
        ];
        assert!(matches!(
            open_with_fallback(&routes, &request()).await,
            Err(ProviderError::InvalidResponse)
        ));
        assert_eq!(second.remaining_turns(), 1);
    }

    #[tokio::test]
    async fn empty_routes_return_temporary_unavailable() {
        let result = open_with_fallback(&[], &request()).await;
        assert!(matches!(result, Err(ProviderError::TemporaryUnavailable)));
    }

    #[tokio::test]
    async fn all_routes_fail_returns_last_error() {
        let first = Arc::new(FakeModelProvider::new(
            "first",
            [FakeTurn::Reject(ProviderError::RateLimited {
                retry_after_seconds: None,
            })],
        ));
        let second = Arc::new(FakeModelProvider::new(
            "second",
            [FakeTurn::Reject(ProviderError::Timeout)],
        ));
        let routes = vec![
            ModelRoute {
                provider: first,
                identity: ModelIdentity {
                    provider: "first".into(),
                    model: "a".into(),
                },
            },
            ModelRoute {
                provider: second,
                identity: ModelIdentity {
                    provider: "second".into(),
                    model: "b".into(),
                },
            },
        ];
        let result = open_with_fallback(&routes, &request()).await;
        // last_error is the Timeout from the second provider
        assert!(matches!(result, Err(ProviderError::Timeout)));
    }

    #[tokio::test]
    async fn first_route_success_skips_remaining() {
        let first = Arc::new(FakeModelProvider::new(
            "first",
            [FakeTurn::Events(vec![Ok(ModelEvent::Completed)])],
        ));
        let second = Arc::new(FakeModelProvider::new(
            "second",
            [FakeTurn::Events(vec![Ok(ModelEvent::Completed)])],
        ));
        let routes = vec![
            ModelRoute {
                provider: first,
                identity: ModelIdentity {
                    provider: "first".into(),
                    model: "a".into(),
                },
            },
            ModelRoute {
                provider: second.clone(),
                identity: ModelIdentity {
                    provider: "second".into(),
                    model: "b".into(),
                },
            },
        ];
        let (selected, _stream) = open_with_fallback(&routes, &request()).await.unwrap();
        assert_eq!(selected.provider, "first");
        // second provider was never consumed
        assert_eq!(second.remaining_turns(), 1);
    }
}
