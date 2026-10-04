//! Pure, opt-in cel-workflow-v2 library. No executor, admission writer or database wiring.
//! Limits validate returned outcomes; they do not meter CEL work or intermediate allocation.
//!
//! Public compiled objects contain opaque ID leases only. Compilation, auditing,
//! evaluation and final program disposal run on configured dedicated workers.
//! Cancellation observes safe boundaries; arbitrary CEL work is not interruptible.
//! General crash resistance and resource isolation remain unqualified.
mod compiler;
mod facade;
mod step;
mod worker;
pub use step::{StepError, StepField, StepRequest, escape, validate_output};
pub use worker::{Compilation, Engine, Handle, WorkerConfig, WorkerError};
mod error;
mod functions;
mod json;
mod retry_policy;
mod scanner;
mod uri;
mod validation;
pub use error::{Category, ExpressionError, Phase, ProfileError, ScanError};
pub use facade::{Bindings, evaluate, evaluate_json, evaluate_template};
pub use facade::{
    CompileCache, Compiled, CompiledJson, CompiledTemplate, compile, compile_with_limits,
};
pub use retry_policy::{
    FixedRetry, MAX_RETRY_DELAY_MS, resolve_retry_policy, validate_retry_policies,
};
pub use scanner::{Segment, scan};
use serde_json::Value;
pub use validation::{DefinitionValidation, Diagnostic, ValidationError};

/// Default worker stack. Configuration must remain strictly above 2 MiB.
pub const COMPILATION_STACK_BYTES: usize = 8 * 1024 * 1024;
pub use uri::encode_path_placeholders;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Profile {
    LegacyV1,
    CelWorkflowV2,
}
pub fn resolve_profile(raw: &Value) -> Result<Profile, ProfileError> {
    let Some(selector) = raw.pointer("/document/metadata/lightExpressionProfile") else {
        return Ok(Profile::LegacyV1);
    };
    if selector.as_str() != Some("cel-workflow-v2") {
        return Err(error::admission(Category::ProfileUnsupported));
    }
    let Some(evaluate) = raw.get("evaluate").and_then(Value::as_object) else {
        return Err(error::admission(Category::ProfileUnsupported));
    };
    if evaluate.get("language").and_then(Value::as_str) != Some("cel")
        || evaluate.contains_key("mode")
    {
        return Err(error::admission(Category::ProfileUnsupported));
    }
    Ok(Profile::CelWorkflowV2)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Json,
    String,
    Predicate,
    Export,
}
/// Every expression position in contract section 2.1; HTTP endpoints use the separate placeholder API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Set,
    HttpBody,
    HttpQuery,
    HttpHeader,
    IdempotencyKey,
    JsonRpcUri,
    JsonRpcParams,
    JsonRpcHeader,
    OpenRpcEndpoint,
    McpParams,
    McpResourceUri,
    A2aParameters,
    AgentInput,
    AgentMockOutput,
    AgentInstructions,
    AgentPrompt,
    AskCategory,
    AskReason,
    AskAssignee,
    AskRole,
    SwitchWhen,
    AssertValue,
    AssertEquals,
    AssertContains,
    AssertJsonEquals,
    AssertJsonContains,
    AssertJsonPredicate,
    Export,
    RunnerExport,
    WorkflowOutput,
}
impl Position {
    pub fn kind(self) -> Kind {
        match self {
            Self::HttpQuery
            | Self::HttpHeader
            | Self::IdempotencyKey
            | Self::JsonRpcUri
            | Self::OpenRpcEndpoint
            | Self::McpResourceUri
            | Self::AgentInstructions
            | Self::AgentPrompt
            | Self::AskCategory
            | Self::AskReason
            | Self::AskAssignee
            | Self::AskRole => Kind::String,
            Self::SwitchWhen | Self::AssertJsonPredicate => Kind::Predicate,
            Self::Export | Self::RunnerExport => Kind::Export,
            _ => Kind::Json,
        }
    }
    pub(crate) fn allows(self, name: &str) -> bool {
        matches!(name, "context" | "workflow")
            || (name == "output" && matches!(self, Self::Export | Self::RunnerExport))
            || (name == "value" && self == Self::AssertJsonPredicate)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub source_bytes: usize,
    pub ast_nodes: usize,
    pub ast_depth: usize,
    pub comprehension_depth: usize,
    pub input_bytes: usize,
    pub input_nodes: usize,
    pub input_depth: usize,
    pub output_bytes: usize,
    pub output_nodes: usize,
    pub output_depth: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            source_bytes: 16 * 1024,
            ast_nodes: 2048,
            ast_depth: 64,
            comprehension_depth: 2,
            input_bytes: 2 * 1024 * 1024,
            input_nodes: 100_000,
            input_depth: 64,
            output_bytes: 1024 * 1024,
            output_nodes: 100_000,
            output_depth: 64,
        }
    }
}
