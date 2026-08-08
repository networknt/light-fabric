use super::response::{NormalizedUsage, ProviderEvidence};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingRequest {
    pub model: String,
    pub inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingVector {
    pub index: u32,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EmbeddingResponse {
    pub vectors: Vec<EmbeddingVector>,
    pub usage: NormalizedUsage,
    #[serde(default)]
    pub evidence: ProviderEvidence,
}
