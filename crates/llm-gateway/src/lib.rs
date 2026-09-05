//! Local LLM data plane. Configuration is compiled off the request path and
//! published as one immutable root so every request observes one generation.

pub mod admission;
pub mod audit;
pub mod config;
pub mod credentials;
pub mod error;
pub mod http;
pub mod pii;
pub mod provider;
pub mod reasoning_seal;
pub mod receipt;
pub mod routing;
pub mod runtime;
pub mod usage;

pub use error::LlmGatewayError;
pub use runtime::{LlmExecution, LlmRequestContext, LlmRuntime};
