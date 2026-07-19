use model_provider::inference::ProviderFormat;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const LLM_ROUTER_FILE: &str = "llm-router.yml";
pub const LLM_ROUTER_MODULE_ID: &str = "light-pingora/llm-router";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRouterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_path_prefix")]
    pub path_prefix: String,
    #[serde(default = "default_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_json_depth")]
    pub max_json_depth: usize,
    #[serde(default = "default_replay_bytes")]
    pub max_replay_bytes: usize,
    #[serde(default = "default_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_global_concurrency")]
    pub global_concurrency: usize,
    #[serde(default)]
    pub development_fixtures: bool,
    #[serde(default)]
    pub openai_extension_allowlist: BTreeSet<String>,
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub deployments: BTreeMap<String, DeploymentConfig>,
    #[serde(default)]
    pub aliases: BTreeMap<String, AliasConfig>,
}

impl Default for LlmRouterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path_prefix: default_path_prefix(),
            max_request_body_bytes: default_body_bytes(),
            max_json_depth: default_json_depth(),
            max_replay_bytes: default_replay_bytes(),
            request_timeout_ms: default_timeout_ms(),
            global_concurrency: default_global_concurrency(),
            development_fixtures: false,
            openai_extension_allowlist: BTreeSet::new(),
            providers: BTreeMap::new(),
            deployments: BTreeMap::new(),
            aliases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    pub format: ProviderFormat,
    pub base_url: String,
    pub secret_ref: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub quota_group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentConfig {
    pub provider: String,
    pub model: String,
    #[serde(default = "default_deployment_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub input_micros_per_million: Option<u64>,
    #[serde(default)]
    pub output_micros_per_million: Option<u64>,
    #[serde(default)]
    pub conformance_digest: String,
    #[serde(default = "default_true")]
    pub text: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub structured_json: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasConfig {
    pub deployments: Vec<String>,
    #[serde(default = "default_attempts")]
    pub max_attempts: usize,
    #[serde(default = "default_alias_concurrency")]
    pub concurrency: usize,
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default)]
    pub max_cost_micros: Option<u64>,
    #[serde(default)]
    pub internal: bool,
    #[serde(default)]
    pub bound_principal: Option<String>,
    #[serde(default)]
    pub audit: AuditMode,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    #[default]
    Disabled,
    Required,
    Durable,
}

fn default_path_prefix() -> String {
    "/v1".to_string()
}
fn default_body_bytes() -> usize {
    1024 * 1024
}
fn default_json_depth() -> usize {
    64
}
fn default_replay_bytes() -> usize {
    1024 * 1024
}
fn default_timeout_ms() -> u64 {
    30_000
}
fn default_global_concurrency() -> usize {
    256
}
fn default_deployment_concurrency() -> usize {
    32
}
fn default_alias_concurrency() -> usize {
    64
}
fn default_attempts() -> usize {
    1
}
fn default_true() -> bool {
    true
}
