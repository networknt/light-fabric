use super::content::{ContentBlock, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroize::Zeroizing;

#[derive(Clone, PartialEq, Eq)]
pub struct ProviderContinuationState {
    pub protocol: super::provider::ProviderProtocol,
    pub payload: Zeroizing<Vec<u8>>,
}

impl std::fmt::Debug for ProviderContinuationState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderContinuationState")
            .field("protocol", &self.protocol)
            .field("payload", &"*** Sensitive Data Redacted ***")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Cancelled,
    Error,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    Complete,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_finish_reason: Option<String>,
    #[serde(skip)]
    pub continuation: Option<ProviderContinuationState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    InProgress,
    Completed,
    Incomplete,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GenerateOutputItem {
    Message {
        id: String,
        role: Role,
        content: Vec<ContentBlock>,
        status: ItemStatus,
    },
    FunctionCall {
        id: String,
        call_id: String,
        name: String,
        arguments: Value,
        status: ItemStatus,
    },
    ReasoningSummary {
        id: String,
        summary: Vec<String>,
        status: ItemStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceResponse {
    /// Public content only. Provider reasoning or chain-of-thought content must not be included.
    pub output: Vec<GenerateOutputItem>,
    pub finish_reason: FinishReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<NormalizedUsage>,
    #[serde(default)]
    pub evidence: ProviderEvidence,
    pub terminal_state: TerminalState,
}
