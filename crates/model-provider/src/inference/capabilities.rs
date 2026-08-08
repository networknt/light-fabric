use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use super::provider::Operation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingEncoding {
    Float,
    Base64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentCapabilities {
    pub text: bool,
    pub images: bool,
    pub tools: bool,
    pub parallel_tools: bool,
    pub structured_json: bool,
    pub reasoning_usage: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationCapabilities {
    pub content: ContentCapabilities,
    pub streaming: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingCapabilities {
    pub max_batch_items: u32,
    pub max_input_tokens_per_item: u64,
    pub max_aggregate_input_tokens: u64,
    #[serde(default)]
    pub supported_dimensions: BTreeSet<u32>,
    #[serde(default)]
    pub supported_encodings: BTreeSet<EmbeddingEncoding>,
    pub max_response_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub operations: BTreeSet<Operation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<GenerationCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<EmbeddingCapabilities>,
}

impl ProviderCapabilities {
    pub fn supports(&self, operation: Operation) -> bool {
        self.operations.contains(&operation)
            && match operation {
                Operation::Generate => self.generation.is_some(),
                Operation::Embed => self.embedding.is_some(),
            }
    }
}
