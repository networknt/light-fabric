use super::capabilities::{EmbeddingCapabilities, GenerationCapabilities};
use super::embedding::{EmbeddingRequest, EmbeddingResponse};
use super::error::InferenceError;
use super::request::InferenceRequest;
use super::response::InferenceResponse;
use super::stream::InferenceEvent;
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ClientProtocol {
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_embeddings")]
    OpenAiEmbeddings,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "internal_canonical")]
    InternalCanonical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Generate,
    Embed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ProviderProtocol {
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    #[serde(rename = "openai_embeddings")]
    OpenAiEmbeddings,
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
    #[serde(rename = "bedrock_converse")]
    BedrockConverse,
}

impl ProviderProtocol {
    pub const fn operation(self) -> Operation {
        match self {
            Self::OpenAiChat
            | Self::OpenAiResponses
            | Self::AnthropicMessages
            | Self::BedrockConverse => Operation::Generate,
            Self::OpenAiEmbeddings => Operation::Embed,
        }
    }
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

pub type GenerationStream = BoxStream<'static, Result<InferenceEvent, InferenceError>>;

#[async_trait]
pub trait GenerationProvider: Send + Sync {
    fn protocol(&self) -> ProviderProtocol;

    fn capabilities(&self) -> GenerationCapabilities;

    async fn generate(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError>;

    async fn generate_stream(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError>;
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    fn protocol(&self) -> ProviderProtocol;

    fn capabilities(&self) -> EmbeddingCapabilities;

    async fn embed(
        &self,
        context: ProviderRequestContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError>;
}

#[derive(Clone)]
pub enum CompiledProvider {
    Generation(Arc<dyn GenerationProvider>),
    Embedding(Arc<dyn EmbeddingProvider>),
}

impl CompiledProvider {
    pub fn protocol(&self) -> ProviderProtocol {
        match self {
            Self::Generation(provider) => provider.protocol(),
            Self::Embedding(provider) => provider.protocol(),
        }
    }

    pub fn operation(&self) -> Operation {
        match self {
            Self::Generation(_) => Operation::Generate,
            Self::Embedding(_) => Operation::Embed,
        }
    }

    pub fn generation(&self) -> Option<&Arc<dyn GenerationProvider>> {
        match self {
            Self::Generation(provider) => Some(provider),
            Self::Embedding(_) => None,
        }
    }

    pub fn embedding(&self) -> Option<&Arc<dyn EmbeddingProvider>> {
        match self {
            Self::Embedding(provider) => Some(provider),
            Self::Generation(_) => None,
        }
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Generation(left), Self::Generation(right)) => Arc::ptr_eq(left, right),
            (Self::Embedding(left), Self::Embedding(right)) => Arc::ptr_eq(left, right),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures_util::{StreamExt, stream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct CancellationAwareMock {
        observed: Arc<AtomicBool>,
    }

    #[test]
    fn protocol_wire_values_are_exact_and_legacy_values_fail_closed() {
        assert_eq!(
            serde_json::to_string(&ProviderProtocol::OpenAiChat).unwrap(),
            "\"openai_chat\""
        );
        assert_eq!(
            serde_json::to_string(&ClientProtocol::OpenAiEmbeddings).unwrap(),
            "\"openai_embeddings\""
        );
        assert!(serde_json::from_str::<ProviderProtocol>("\"openai\"").is_err());
        assert!(serde_json::from_str::<Operation>("\"chat_completions\"").is_err());
    }

    #[async_trait]
    impl GenerationProvider for CancellationAwareMock {
        fn protocol(&self) -> ProviderProtocol {
            ProviderProtocol::OpenAiChat
        }

        fn capabilities(&self) -> GenerationCapabilities {
            GenerationCapabilities::default()
        }

        async fn generate(
            &self,
            context: ProviderRequestContext,
            _request: InferenceRequest,
        ) -> Result<InferenceResponse, InferenceError> {
            context.cancellation.cancelled().await;
            self.observed.store(true, Ordering::SeqCst);
            Err(InferenceError::cancelled())
        }

        async fn generate_stream(
            &self,
            context: ProviderRequestContext,
            _request: InferenceRequest,
        ) -> Result<GenerationStream, InferenceError> {
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
                    .generate(context, InferenceRequest::text("m", "hello"))
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
            .generate_stream(context, InferenceRequest::text("m", "hello"))
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
