pub mod capabilities;
pub mod compatibility;
pub mod content;
pub mod embedding;
pub mod error;
pub mod provider;
pub mod request;
pub mod response;
pub mod stream;

pub use capabilities::{
    ContentCapabilities, EmbeddingCapabilities, EmbeddingDistanceMetric, EmbeddingEncoding,
    EmbeddingNormalization, EmbeddingSpaceContract, GenerationCapabilities, ProviderCapabilities,
};
pub use compatibility::{LegacyProviderAdapter, OpenAiCompatibilityProfile};
pub use content::{ContentBlock, ImageSource, Message, Role, ToolCall, ToolResult};
pub use embedding::{EmbeddingRequest, EmbeddingResponse, EmbeddingVector};
pub use error::{AcceptanceEvidence, InferenceError, InferenceErrorCategory, RetryDisposition};
pub use provider::{
    ClientProtocol, CompiledProvider, EmbeddingProvider, GenerationProvider, GenerationStream,
    Operation, ProviderProtocol, ProviderRequestContext, TraceContext,
};
pub use request::{
    InferenceRequest, ReasoningOptions, ResponseFormat, SamplingOptions, TokenLimits, ToolChoice,
    ToolDefinition,
};
pub use response::{
    FinishReason, GenerateOutputItem, InferenceResponse, ItemStatus, NormalizedUsage,
    ProviderEvidence, TerminalState,
};
pub use stream::{InferenceEvent, StreamDecoder, ToolCallDelta};
