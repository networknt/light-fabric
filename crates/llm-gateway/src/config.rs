use crate::pii::PiiProfile;
use crate::usage::OperationPrice;
use model_provider::conformance::{ConformanceResult, FixtureProvenance};
use model_provider::inference::{
    EmbeddingCapabilities, EmbeddingSpaceContract, ProviderCapabilities,
};
use model_provider::inference::{Operation, ProviderProtocol};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const LLM_ROUTER_FILE: &str = "llm-router.yml";
pub const LLM_ROUTER_MODULE_ID: &str = "light-pingora/llm-router";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingWorkloadLane {
    #[default]
    Standard,
    KbQuery,
    KbIndex,
}

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
    #[serde(default)]
    pub embedding_memory: EmbeddingMemoryConfig,
    #[serde(default)]
    pub embedding_workload_lane: EmbeddingWorkloadLane,
    #[serde(default = "default_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_global_concurrency")]
    pub global_concurrency: usize,
    #[serde(default = "default_global_stream_concurrency")]
    pub global_stream_concurrency: usize,
    #[serde(default = "default_stream_channel_capacity")]
    pub stream_channel_capacity: usize,
    #[serde(default = "default_max_stream_response_bytes")]
    pub max_stream_response_bytes: usize,
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
    /// Protected rollout switch for explicitly enabled local provider profiles.
    #[serde(default)]
    pub local_transport_enabled: bool,
    #[serde(default)]
    pub openai_extension_allowlist: BTreeSet<String>,
    #[serde(default)]
    pub client_compatibility: ClientCompatibilityConfig,
    #[serde(default)]
    pub runtime_material: RuntimeMaterialConfig,
    #[serde(default)]
    pub audit_runtime: AuditRuntimeConfig,
    #[serde(default)]
    pub network_zones: BTreeMap<String, NetworkZone>,
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
            embedding_memory: EmbeddingMemoryConfig::default(),
            embedding_workload_lane: EmbeddingWorkloadLane::Standard,
            request_timeout_ms: default_timeout_ms(),
            global_concurrency: default_global_concurrency(),
            global_stream_concurrency: default_global_stream_concurrency(),
            stream_channel_capacity: default_stream_channel_capacity(),
            max_stream_response_bytes: default_max_stream_response_bytes(),
            stream_write_timeout_ms: default_stream_write_timeout_ms(),
            stream_setup_timeout_ms: default_stream_setup_timeout_ms(),
            stream_idle_timeout_ms: default_stream_idle_timeout_ms(),
            stream_minimum_drain_bytes_per_second: default_stream_minimum_drain_rate(),
            stream_drain_grace_ms: default_stream_drain_grace_ms(),
            development_fixtures: false,
            local_transport_enabled: false,
            openai_extension_allowlist: BTreeSet::new(),
            client_compatibility: ClientCompatibilityConfig::default(),
            runtime_material: RuntimeMaterialConfig::default(),
            audit_runtime: AuditRuntimeConfig::default(),
            network_zones: BTreeMap::new(),
            providers: BTreeMap::new(),
            deployments: BTreeMap::new(),
            aliases: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClientCompatibilityConfig {
    #[serde(default)]
    pub anthropic_messages: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingMemoryConfig {
    #[serde(default = "default_embedding_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_embedding_replay_bytes")]
    pub max_replay_bytes: usize,
    #[serde(default = "default_embedding_memory_bytes")]
    pub max_memory_bytes: usize,
    #[serde(default = "default_embedding_ingress_concurrency")]
    pub ingress_concurrency: usize,
    #[serde(default = "default_embedding_ingress_memory_bytes")]
    pub max_ingress_memory_bytes: usize,
    #[serde(default = "default_embedding_ingress_overhead_bytes")]
    pub ingress_overhead_bytes: usize,
    #[serde(default = "default_embedding_items_per_permit")]
    pub items_per_permit: usize,
    #[serde(default = "default_embedding_input_bytes_per_item")]
    pub max_input_bytes_per_item: usize,
    #[serde(default = "default_embedding_total_input_bytes")]
    pub max_total_input_bytes: usize,
    #[serde(default = "default_embedding_body_read_timeout_ms")]
    pub body_read_timeout_ms: u64,
    #[serde(default = "default_embedding_minimum_receive_rate")]
    pub minimum_receive_bytes_per_second: u64,
    #[serde(default = "default_embedding_authorization_timeout_ms")]
    pub authorization_timeout_ms: u64,
    #[serde(default = "default_embedding_write_timeout_ms")]
    pub write_timeout_ms: u64,
    #[serde(default = "default_embedding_minimum_drain_rate")]
    pub minimum_drain_bytes_per_second: u64,
}

impl Default for EmbeddingMemoryConfig {
    fn default() -> Self {
        Self {
            max_request_body_bytes: default_embedding_body_bytes(),
            max_replay_bytes: default_embedding_replay_bytes(),
            max_memory_bytes: default_embedding_memory_bytes(),
            ingress_concurrency: default_embedding_ingress_concurrency(),
            max_ingress_memory_bytes: default_embedding_ingress_memory_bytes(),
            ingress_overhead_bytes: default_embedding_ingress_overhead_bytes(),
            items_per_permit: default_embedding_items_per_permit(),
            max_input_bytes_per_item: default_embedding_input_bytes_per_item(),
            max_total_input_bytes: default_embedding_total_input_bytes(),
            body_read_timeout_ms: default_embedding_body_read_timeout_ms(),
            minimum_receive_bytes_per_second: default_embedding_minimum_receive_rate(),
            authorization_timeout_ms: default_embedding_authorization_timeout_ms(),
            write_timeout_ms: default_embedding_write_timeout_ms(),
            minimum_drain_bytes_per_second: default_embedding_minimum_drain_rate(),
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
pub struct RuntimeMaterialConfig {
    /// Minimum evidence provenance accepted for production routing. Production
    /// defaults to sanitized provider captures; synthetic fixtures remain
    /// available only to explicitly marked development configurations.
    #[serde(default = "default_conformance_provenance")]
    pub required_conformance_provenance: FixtureProvenance,
    /// Maps opaque `credential://` references to application-owned environment
    /// variable names. Values are names, never secret material.
    #[serde(default)]
    pub credential_environment: BTreeMap<String, String>,
    /// Maps approved `config://` trust-bundle references to protected local
    /// PEM paths. Configuration carries only the reference and digest.
    #[serde(default)]
    pub trust_bundle_files: BTreeMap<String, String>,
    /// Protected runner public keys, encoded as base64 raw Ed25519 keys.
    #[serde(default)]
    pub evidence_public_keys: BTreeMap<String, String>,
    #[serde(default)]
    pub evidence_key_set_version: String,
    #[serde(default)]
    pub evidence_key_set_digest: String,
    #[serde(default)]
    pub reasoning_seal: ReasoningSealConfig,
}

impl Default for RuntimeMaterialConfig {
    fn default() -> Self {
        Self {
            required_conformance_provenance: default_conformance_provenance(),
            credential_environment: BTreeMap::new(),
            trust_bundle_files: BTreeMap::new(),
            evidence_public_keys: BTreeMap::new(),
            evidence_key_set_version: String::new(),
            evidence_key_set_digest: String::new(),
            reasoning_seal: ReasoningSealConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSealState {
    #[default]
    Disabled,
    Prepared,
    Active,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningSealKeyReference {
    pub key_id: String,
    pub credential_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningStateLimits {
    #[serde(default = "default_reasoning_encoded_item_bytes")]
    pub max_encoded_item_bytes: usize,
    #[serde(default = "default_reasoning_decoded_state_bytes")]
    pub max_decoded_provider_state_bytes: usize,
    #[serde(default = "default_reasoning_item_count")]
    pub max_items_per_request: usize,
    #[serde(default = "default_reasoning_cumulative_bytes")]
    pub max_cumulative_encoded_bytes: usize,
    #[serde(default = "default_reasoning_cumulative_decoded_bytes")]
    pub max_cumulative_decoded_bytes: usize,
}

impl Default for ReasoningStateLimits {
    fn default() -> Self {
        Self {
            max_encoded_item_bytes: default_reasoning_encoded_item_bytes(),
            max_decoded_provider_state_bytes: default_reasoning_decoded_state_bytes(),
            max_items_per_request: default_reasoning_item_count(),
            max_cumulative_encoded_bytes: default_reasoning_cumulative_bytes(),
            max_cumulative_decoded_bytes: default_reasoning_cumulative_decoded_bytes(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningSealConfig {
    #[serde(default)]
    pub state: ReasoningSealState,
    #[serde(default)]
    pub key_set_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ReasoningSealKeyReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<ReasoningSealKeyReference>,
    #[serde(default)]
    pub limits: ReasoningStateLimits,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndpointApiKeyHeader {
    #[default]
    XApiKey,
    Authorization,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderProfileType {
    OpenAi,
    Anthropic,
    AwsBedrock,
    Xai,
    GoogleGemini,
    GoogleVertex,
    #[default]
    Compatible,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AwsCredentialSource {
    #[default]
    DefaultChain,
}

impl EndpointApiKeyHeader {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::XApiKey => "x-api-key",
            Self::Authorization => "authorization",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum EndpointAuth {
    None,
    Bearer {
        credential_ref: String,
    },
    ApiKey {
        credential_ref: String,
        header: EndpointApiKeyHeader,
    },
    BedrockApiKey {
        credential_ref: String,
    },
    AwsSigV4 {
        #[serde(default)]
        credential_source: AwsCredentialSource,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeAuth {
    None,
    Bearer {
        credential_ref: String,
    },
    ApiKey {
        credential_ref: String,
        header: EndpointApiKeyHeader,
    },
}

impl Default for RuntimeAuth {
    fn default() -> Self {
        Self::None
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProfileMode {
    #[default]
    PublicTls,
    PrivateTls,
    PrivatePlaintext,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkTermination {
    #[default]
    Native,
    LightGatewaySidecar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustBundleReference {
    pub trust_bundle_ref: String,
    pub trust_bundle_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderConnectionPolicy {
    #[serde(default = "default_pool_idle_timeout_ms")]
    pub pool_idle_timeout_ms: u64,
    #[serde(default = "default_client_refresh_interval_ms")]
    pub client_refresh_interval_ms: u64,
}

impl Default for ProviderConnectionPolicy {
    fn default() -> Self {
        Self {
            pool_idle_timeout_ms: default_pool_idle_timeout_ms(),
            client_refresh_interval_ms: default_client_refresh_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkProfile {
    #[serde(default)]
    pub mode: NetworkProfileMode,
    #[serde(default)]
    pub termination: NetworkTermination,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_zone_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TrustBundleReference>,
    #[serde(default)]
    pub connection: ProviderConnectionPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkZone {
    pub id: String,
    #[serde(default)]
    pub dns_names: BTreeSet<String>,
    #[serde(default)]
    pub cidrs: BTreeSet<String>,
    #[serde(default)]
    pub ports: BTreeSet<u16>,
    #[serde(default)]
    pub allow_private_tls: bool,
    #[serde(default)]
    pub allow_private_plaintext: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessPolicy {
    #[default]
    Immediate,
    WarmBeforeEligible,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeCapacity {
    pub physical_runtime_id: String,
    pub capacity_domain_id: String,
    pub max_parallel_requests: usize,
    pub max_queued_requests: usize,
    #[serde(default)]
    pub readiness_policy: ReadinessPolicy,
    pub cold_start_timeout_ms: u64,
    pub stream_setup_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SidecarExpectation {
    pub profile_version: String,
    pub config_sha256: String,
    #[serde(default)]
    pub runtime_auth: RuntimeAuth,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingBasis {
    #[default]
    ExternalProvider,
    ZeroMarginal,
    AmortizedInternal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderConfig {
    #[serde(default)]
    pub provider_account_id: String,
    #[serde(default)]
    pub provider_type: ProviderProfileType,
    pub provider_protocol: ProviderProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_region: Option<String>,
    pub material_generation: u64,
    pub base_url: String,
    pub endpoint_auth: EndpointAuth,
    #[serde(default)]
    pub network_profile: NetworkProfile,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub quota_group_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentConfig {
    pub provider: String,
    #[serde(default)]
    pub deployment_revision_id: String,
    pub model: String,
    #[serde(default = "default_deployment_concurrency")]
    pub concurrency: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_capacity: Option<RuntimeCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidecar: Option<SidecarExpectation>,
    #[serde(default)]
    pub pricing_basis: PricingBasis,
    pub prices: BTreeMap<Operation, OperationPrice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bedrock_policy: Option<BedrockDeploymentPolicy>,
    /// Complete static provider contract supplied by the control plane through
    /// the values-backed deployment map, without flattening capability fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_capabilities: Option<ProviderCapabilities>,
    #[serde(default)]
    pub embedding_capabilities: Option<EmbeddingCapabilities>,
    #[serde(default)]
    pub conformance_digest: String,
    /// Legacy complete, self-digested deployment evidence. New control-plane
    /// publications use `declaredCapabilities`; this remains readable for
    /// compatibility with manually supplied or older configuration.
    #[serde(default)]
    pub conformance_result: Option<ConformanceResult>,
    #[serde(default = "default_true")]
    pub text: bool,
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub structured_json: bool,
    /// Legacy flattened streaming capability. Defaults to true so local
    /// fixtures retain SSE parity when no complete declared contract exists.
    #[serde(default = "default_true")]
    pub streaming: bool,
    /// Legacy development-fixture placeholder-preservation percentage.
    #[serde(default)]
    pub pii_placeholder_preservation_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BedrockDeploymentPolicy {
    #[serde(default)]
    pub sampling: SamplingCapabilities,
    #[serde(default)]
    pub reasoning: ReasoningCapabilities,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SamplingCapabilities {
    #[serde(default)]
    pub temperature: SamplingParameterPolicy,
    #[serde(default)]
    pub top_p: SamplingParameterPolicy,
    #[serde(default)]
    pub allow_temperature_and_top_p: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SamplingParameterPolicy {
    #[default]
    Unsupported,
    Range {
        minimum: f64,
        maximum: f64,
    },
    Fixed {
        value: f64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReasoningCapabilities {
    #[serde(default)]
    pub mode: ReasoningMode,
    #[serde(default)]
    pub supported_efforts: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningMode {
    #[default]
    Unsupported,
    OptionalAdaptive,
    AlwaysOnAdaptive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AliasConfig {
    pub operations: BTreeSet<Operation>,
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
    #[serde(default)]
    pub required_capabilities: AliasCapabilityRequirements,
    #[serde(default)]
    pub require_expected_embedding_space: bool,
    #[serde(default)]
    pub embedding_workload_lane: EmbeddingWorkloadLane,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AliasCapabilityRequirements {
    #[serde(default)]
    pub images: bool,
    #[serde(default)]
    pub tools: bool,
    #[serde(default)]
    pub parallel_tools: bool,
    #[serde(default)]
    pub structured_json: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_space: Option<EmbeddingSpaceContract>,
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
fn default_embedding_body_bytes() -> usize {
    4 * 1024 * 1024
}
fn default_embedding_replay_bytes() -> usize {
    4 * 1024 * 1024
}
fn default_embedding_memory_bytes() -> usize {
    16 * 1024 * 1024 * 1024
}
fn default_embedding_ingress_concurrency() -> usize {
    32
}
fn default_embedding_ingress_memory_bytes() -> usize {
    3 * 1024 * 1024 * 1024
}
fn default_embedding_ingress_overhead_bytes() -> usize {
    (4 * 1024 * 1024 * 20) + (64 * 1024)
}
fn default_embedding_items_per_permit() -> usize {
    256
}
fn default_embedding_input_bytes_per_item() -> usize {
    1024 * 1024
}
fn default_embedding_total_input_bytes() -> usize {
    4 * 1024 * 1024
}
fn default_embedding_body_read_timeout_ms() -> u64 {
    30_000
}
fn default_embedding_minimum_receive_rate() -> u64 {
    1024
}
fn default_embedding_authorization_timeout_ms() -> u64 {
    10_000
}
fn default_embedding_write_timeout_ms() -> u64 {
    30_000
}
fn default_embedding_minimum_drain_rate() -> u64 {
    1024
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
fn default_max_stream_response_bytes() -> usize {
    16 * 1024 * 1024
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
fn default_gateway_instance() -> String {
    "gateway-local".to_string()
}

fn default_reasoning_encoded_item_bytes() -> usize {
    128 * 1024
}

fn default_reasoning_decoded_state_bytes() -> usize {
    96 * 1024
}

fn default_reasoning_item_count() -> usize {
    8
}

fn default_reasoning_cumulative_bytes() -> usize {
    256 * 1024
}

fn default_reasoning_cumulative_decoded_bytes() -> usize {
    192 * 1024
}

fn default_pool_idle_timeout_ms() -> u64 {
    30_000
}

fn default_client_refresh_interval_ms() -> u64 {
    300_000
}

fn default_conformance_provenance() -> FixtureProvenance {
    FixtureProvenance::CapturedSanitized
}
