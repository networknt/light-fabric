use crate::inference::EmbeddingCapabilities;
use crate::inference::provider::ProviderProtocol;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusManifest {
    pub schema_version: String,
    pub corpus_version: String,
    pub valid_for_seconds: i64,
    pub refresh_before_seconds: u64,
    pub profiles: Vec<ProviderProfile>,
    pub fixtures: Vec<FixtureReference>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub provider: ProviderProtocol,
    pub physical_model: String,
    pub api_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_capabilities: Option<EmbeddingCapabilities>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureReference {
    pub path: String,
    pub sha256: String,
    pub provenance: FixtureProvenance,
    pub covers: BTreeSet<ConformanceCapability>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureProvenance {
    SyntheticSpecDerived,
    CapturedSanitized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceCapability {
    Generate,
    Embed,
    Text,
    Images,
    Tools,
    ParallelTools,
    StructuredJson,
    ReasoningUsage,
    Streaming,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FixtureKind {
    Request,
    Response,
    Stream,
    Error,
    Compatibility,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusFixture {
    pub schema_version: String,
    pub id: String,
    pub provider: ProviderProtocol,
    pub kind: FixtureKind,
    pub input: Value,
    pub expected: Value,
}
