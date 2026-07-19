use super::error::InferenceError;
use super::response::{FinishReason, NormalizedUsage, ProviderEvidence, TerminalState};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallDelta {
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments_fragment: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum InferenceEvent {
    MessageStart {
        evidence: ProviderEvidence,
    },
    TextDelta {
        text: String,
    },
    ToolCallDelta {
        delta: ToolCallDelta,
    },
    Usage {
        usage: NormalizedUsage,
    },
    MessageEnd {
        finish_reason: FinishReason,
        terminal_state: TerminalState,
    },
}

pub trait StreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<InferenceEvent>, InferenceError>;
    fn finish(&mut self) -> Result<Vec<InferenceEvent>, InferenceError>;
}
