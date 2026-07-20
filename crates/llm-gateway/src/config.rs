use crate::pii::PiiProfile;
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
    #[serde(default = "default_global_stream_concurrency")]
    pub global_stream_concurrency: usize,
    #[serde(default = "default_stream_channel_capacity")]
    pub stream_channel_capacity: usize,
    #[serde(default = "default_stream_write_timeout_ms")]
    pub stream_write_timeout_ms: u64,
    #[serde(default = "default_stream_setup_timeout_ms")]
    pub stream_setup_timeout_ms: u64,
    #[serde(default = "default_stream_idle_timeout_ms")]
    pub stream_idle_timeout_ms: u64,
    #[serde(default = "default_stream_minimum_drain_rate")]
    pub stream_minimum_drain_bytes_per_second: u64,
    #[serde(default = "default_stream_drain_grace_ms")]
    pub stream_drain_grace_ms: u64,
    #[serde(default)]
    pub development_fixtures: bool,
    #[serde(default)]
    pub openai_extension_allowlist: BTreeSet<String>,
    #[serde(default)]
    pub production_projection: ProductionProjectionConfig,
    #[serde(default)]
    pub audit_runtime: AuditRuntimeConfig,
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
            global_stream_concurrency: default_global_stream_concurrency(),
            stream_channel_capacity: default_stream_channel_capacity(),
            stream_write_timeout_ms: default_stream_write_timeout_ms(),
            stream_setup_timeout_ms: default_stream_setup_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            stream_minimum_drain_bytes_per_second: default_stream_minimum_drain_rate(),
            stream_drain_grace_ms: default_stream_drain_grace_ms(),
            development_fixtures: false,
            openai_extension_allowlist: BTreeSet::new(),
            production_projection: ProductionProjectionConfig::default(),
            audit_runtime: AuditRuntimeConfig::default(),
            providers: BTreeMap::new(),
            deployments: BTreeMap::new(),
            aliases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditRuntimeConfig {
    #[serde(default = "default_audit_directory")]
    pub directory: String,
    #[serde(default = "default_gateway_instance")]
    pub gateway_instance: String,
    #[serde(default = "default_audit_host")]
    pub host_id: String,
    #[serde(default = "default_audit_record_bytes")]
    pub max_record_bytes: usize,
    #[serde(default = "default_audit_segment_bytes")]
    pub max_segment_bytes: u64,
    #[serde(default = "default_audit_spool_bytes")]
    pub max_spool_bytes: u64,
    #[serde(default = "default_audit_queue_records")]
    pub queue_records: usize,
    #[serde(default = "default_audit_batch_records")]
    pub batch_records: usize,
    #[serde(default = "default_audit_batch_bytes")]
    pub batch_bytes: usize,
    #[serde(default = "default_audit_commit_delay_ms")]
    pub commit_delay_ms: u64,
    #[serde(default)]
    pub terminal_commit_before_response: bool,
    #[serde(default)]
    pub persistent_volume: bool,
    /// Environment variable containing the separately credentialed audit
    /// PostgreSQL URL. The URL itself is never stored in this config.
    #[serde(default)]
    pub sink_database_url_env: Option<String>,
    #[serde(default = "default_audit_sink_batch_records")]
    pub sink_batch_records: usize,
    #[serde(default = "default_audit_sink_batch_bytes")]
    pub sink_batch_bytes: usize,
    #[serde(default = "default_audit_sink_poll_ms")]
    pub sink_poll_ms: u64,
    #[serde(default = "default_audit_sink_retry_max_ms")]
    pub sink_retry_max_ms: u64,
}

impl Default for AuditRuntimeConfig {
    fn default() -> Self {
        Self {
            directory: default_audit_directory(),
            gateway_instance: default_gateway_instance(),
            host_id: default_audit_host(),
            max_record_bytes: default_audit_record_bytes(),
            max_segment_bytes: default_audit_segment_bytes(),
            max_spool_bytes: default_audit_spool_bytes(),
            queue_records: default_audit_queue_records(),
            batch_records: default_audit_batch_records(),
            batch_bytes: default_audit_batch_bytes(),
            commit_delay_ms: default_audit_commit_delay_ms(),
            terminal_commit_before_response: false,
            persistent_volume: false,
            sink_database_url_env: None,
            sink_batch_records: default_audit_sink_batch_records(),
            sink_batch_bytes: default_audit_sink_batch_bytes(),
            sink_poll_ms: default_audit_sink_poll_ms(),
            sink_retry_max_ms: default_audit_sink_retry_max_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductionProjectionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_projection_root")]
    pub root_directory: String,
    #[serde(default = "default_projection_checkpoint")]
    pub checkpoint_path: String,
    #[serde(default = "default_projection_acknowledgements")]
    pub acknowledgement_directory: String,
    #[serde(default = "default_gateway_instance")]
    pub gateway_instance: String,
    #[serde(default = "default_projection_poll_ms")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_projection_artifact_bytes")]
    pub max_artifact_bytes: usize,
    /// Maps opaque `credential://` references to application-owned environment
    /// variable names. Values are names, never secret material.
    #[serde(default)]
    pub credential_environment: BTreeMap<String, String>,
}

impl Default for ProductionProjectionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            root_directory: default_projection_root(),
            checkpoint_path: default_projection_checkpoint(),
            acknowledgement_directory: default_projection_acknowledgements(),
            gateway_instance: default_gateway_instance(),
            poll_interval_ms: default_projection_poll_ms(),
            max_artifact_bytes: default_projection_artifact_bytes(),
            credential_environment: BTreeMap::new(),
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
    /// Exact placeholder-preservation percentage from the versioned
    /// deployment conformance corpus. Zero means no reversible-PII evidence.
    #[serde(default)]
    pub pii_placeholder_preservation_percent: u8,
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
    #[serde(default)]
    pub pii: PiiProfile,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    #[default]
    Disabled,
    BestEffort,
    BoundedAsync,
    LocalDurable,
    RemoteDurable,
    /// Backward-compatible spelling for required bounded-async audit.
    Required,
    /// Backward-compatible spelling for required local-durable audit.
    Durable,
}

impl AuditMode {
    pub fn is_local_durable(self) -> bool {
        matches!(self, Self::LocalDurable | Self::Durable)
    }
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
fn default_global_stream_concurrency() -> usize {
    64
}
fn default_stream_channel_capacity() -> usize {
    8
}
fn default_stream_write_timeout_ms() -> u64 {
    5_000
}
fn default_stream_setup_timeout_ms() -> u64 {
    10_000
}
fn default_stream_idle_timeout_ms() -> u64 {
    15_000
}
fn default_stream_minimum_drain_rate() -> u64 {
    128
}
fn default_stream_drain_grace_ms() -> u64 {
    1_000
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
fn default_audit_directory() -> String {
    "data/llm-audit".to_string()
}
fn default_audit_host() -> String {
    "host-local".to_string()
}
fn default_audit_record_bytes() -> usize {
    4 * 1024
}
fn default_audit_segment_bytes() -> u64 {
    64 * 1024 * 1024
}
fn default_audit_spool_bytes() -> u64 {
    1024 * 1024 * 1024
}
fn default_audit_queue_records() -> usize {
    8_192
}
fn default_audit_batch_records() -> usize {
    64
}
fn default_audit_batch_bytes() -> usize {
    256 * 1024
}
fn default_audit_commit_delay_ms() -> u64 {
    5
}
fn default_audit_sink_batch_records() -> usize {
    256
}
fn default_audit_sink_batch_bytes() -> usize {
    1024 * 1024
}
fn default_audit_sink_poll_ms() -> u64 {
    100
}
fn default_audit_sink_retry_max_ms() -> u64 {
    10_000
}
fn default_projection_root() -> String {
    "config-cache/llm-projection".to_string()
}
fn default_projection_checkpoint() -> String {
    "data/llm-projection/checkpoint.json".to_string()
}
fn default_projection_acknowledgements() -> String {
    "data/llm-projection/acknowledgements".to_string()
}
fn default_gateway_instance() -> String {
    "gateway-local".to_string()
}
fn default_projection_poll_ms() -> u64 {
    1_000
}
fn default_projection_artifact_bytes() -> usize {
    4 * 1024 * 1024
}
