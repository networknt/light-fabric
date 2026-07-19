use super::provider::Operation;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
pub struct ProviderCapabilities {
    pub operations: BTreeSet<Operation>,
    pub content: ContentCapabilities,
    pub streaming: bool,
}

impl ProviderCapabilities {
    pub fn supports(&self, operation: Operation) -> bool {
        self.operations.contains(&operation)
    }
}
