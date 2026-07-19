use super::capabilities::ProviderCapabilities;
use super::error::InferenceError;
use super::request::InferenceRequest;
use super::response::InferenceResponse;
use super::stream::InferenceEvent;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientFormat {
    OpenAiChatCompletions,
    InternalCanonical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    ChatCompletions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFormat {
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TraceContext {
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderRequestContext {
    pub deadline: Instant,
    pub cancellation: CancellationToken,
    pub attempt_id: String,
    pub trace: TraceContext,
}

impl ProviderRequestContext {
    pub fn with_timeout(attempt_id: impl Into<String>, timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancellation: CancellationToken::new(),
            attempt_id: attempt_id.into(),
            trace: TraceContext::default(),
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.deadline.checked_duration_since(Instant::now())
    }

    pub fn check_active(&self) -> Result<(), InferenceError> {
        if self.cancellation.is_cancelled() {
            return Err(InferenceError::cancelled());
        }
        if self.remaining().is_none() {
            return Err(InferenceError::timeout_before_acceptance());
        }
        Ok(())
    }
}

pub type InferenceStream = BoxStream<'static, Result<InferenceEvent, InferenceError>>;

#[async_trait]
pub trait InferenceProvider: Send + Sync {
    fn format(&self) -> ProviderFormat;

    fn capabilities(&self) -> ProviderCapabilities;

    async fn infer(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError>;

    async fn stream(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::capabilities::ProviderCapabilities;
    use async_trait::async_trait;
    use futures_util::{StreamExt, stream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CancellationAwareMock {
        observed: Arc<AtomicBool>,
    }

    #[async_trait]
    impl InferenceProvider for CancellationAwareMock {
        fn format(&self) -> ProviderFormat {
            ProviderFormat::OpenAi
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::default()
        }

        async fn infer(
            &self,
            context: ProviderRequestContext,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, InferenceError> {
            context.cancellation.cancelled().await;
            self.observed.store(true, Ordering::SeqCst);
            Err(InferenceError::cancelled())
        }

        async fn stream(
            &self,
            context: ProviderRequestContext,
            _request: InferenceRequest,
        ) -> Result<InferenceStream, InferenceError> {
            let observed = Arc::clone(&self.observed);
            let cancellation = context.cancellation;
            Ok(Box::pin(stream::unfold(0_u8, move |state| {
                let cancellation = cancellation.clone();
                let observed = Arc::clone(&observed);
                async move {
                    match state {
                        0 => Some((
                            Ok(InferenceEvent::TextDelta {
                                text: "first".to_string(),
                            }),
                            1,
                        )),
                        1 => {
                            cancellation.cancelled().await;
                            observed.store(true, Ordering::SeqCst);
                            Some((Err(InferenceError::cancelled()), 2))
                        }
                        _ => None,
                    }
                }
            })))
        }
    }

    #[tokio::test]
    async fn cancellation_reaches_mock_before_acceptance() {
        let observed = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(CancellationAwareMock {
            observed: Arc::clone(&observed),
        });
        let context = ProviderRequestContext::with_timeout("cancel-before", Duration::from_secs(1));
        let cancellation = context.cancellation.clone();
        let task = tokio::spawn({
            let provider = Arc::clone(&provider);
            async move {
                provider
                    .infer(context, InferenceRequest::text("m", "hello"))
                    .await
            }
        });
        cancellation.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert_eq!(
            error.category,
            super::super::error::InferenceErrorCategory::Cancelled
        );
        assert!(observed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn cancellation_reaches_mock_after_first_output() {
        let observed = Arc::new(AtomicBool::new(false));
        let provider = CancellationAwareMock {
            observed: Arc::clone(&observed),
        };
        let context = ProviderRequestContext::with_timeout("cancel-after", Duration::from_secs(1));
        let cancellation = context.cancellation.clone();
        let mut output = provider
            .stream(context, InferenceRequest::text("m", "hello"))
            .await
            .unwrap();
        assert!(matches!(
            output.next().await,
            Some(Ok(InferenceEvent::TextDelta { .. }))
        ));
        cancellation.cancel();
        assert!(
            matches!(output.next().await, Some(Err(error)) if error.category == super::super::error::InferenceErrorCategory::Cancelled)
        );
        assert!(observed.load(Ordering::SeqCst));
    }
}
