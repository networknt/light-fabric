use a2a_protocol::{A2aOperation, ProtocolProfile};
use agent_core::{PolicySnapshot, sha256_digest};
use agent_runtime_protocol::canonical_digest;
use chrono::{DateTime, Utc};
use knowledge_core::RetrievalFilters;
use serde::de::{DeserializeOwned, Error as DeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};
use url::Url;
use uuid::Uuid;

pub const AGENT_CONFIG_FILE: &str = "agent.yml";
pub const AGENT_CONFIG_MODULE_ID: &str = "light-agent/agent";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfig {
    pub operational_store: OperationalStoreProjection,
    pub runtime_policy: RuntimePolicyEnvelope,
    pub portal_association: PortalAssociationEvidence,
    pub agent_policy: AgentPolicy,
    #[serde(default)]
    pub a2a_policy: NativeA2aPolicy,
    #[serde(default)]
    pub a2a_outbound: OutboundA2aPolicy,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboundA2aPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub authorization_context_key_file: String,
    #[serde(default)]
    pub bindings: Vec<OutboundA2aBinding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboundA2aBinding {
    pub agent_ref: String,
    pub display_name: String,
    pub description: String,
    pub catalog_tool_id: Uuid,
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: String,
    pub gateway_uri: String,
    pub protocol_version: String,
    pub data_boundary_digest: String,
    pub maximum_delegation_depth: u16,
    pub maximum_budget_units: u64,
    #[serde(default)]
    pub allowed_skill_ids: Vec<String>,
}

/// Portal relationship evidence used by operational records and audit only.
/// It is deliberately outside runtimePolicy and is never workload identity.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortalAssociationEvidence {
    pub runtime_instance_id: Uuid,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeA2aPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub agent_ref: String,
    #[serde(default, deserialize_with = "deserialize_optional_uuid")]
    pub binding_id: Option<Uuid>,
    #[serde(default, deserialize_with = "deserialize_optional_uuid")]
    pub publication_id: Option<Uuid>,
    #[serde(default)]
    pub policy_digest: String,
    #[serde(default)]
    pub content_digest: String,
    #[serde(default)]
    pub authorization_context_key_file: String,
    #[serde(default)]
    pub protocol_profile: Option<ProtocolProfile>,
    #[serde(default)]
    pub allowed_operations: std::collections::BTreeSet<A2aOperation>,
    #[serde(default)]
    pub allowed_principal_prefixes: Vec<String>,
    #[serde(default)]
    pub public_url: String,
    #[serde(default)]
    pub agent_card: Option<Value>,
    #[serde(default)]
    pub artifact_retention: Option<A2aArtifactRetentionPolicy>,
    #[serde(default)]
    pub artifact_root_directory: std::path::PathBuf,
    #[serde(default)]
    pub trusted_signing_profile: Option<a2a_protocol::TrustedCardSigningProfile>,
    #[serde(default)]
    pub public_skills: Vec<A2aPublicSkillMapping>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aPublicSkillMapping {
    pub publication_alias: String,
    pub skill_id: Uuid,
    pub skill_version: String,
    pub skill_digest: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aArtifactRetentionPolicy {
    pub profile_id: String,
    pub task_retention_days: u32,
    pub artifact_retention_days: u32,
    pub maximum_artifact_bytes: u64,
    pub access_policy_ref: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationalStoreProjection {
    pub contract_version: u32,
    pub binding_id: Uuid,
    pub binding_digest: String,
    pub profile_id: String,
    pub deployment_profile: String,
    pub scope_kind: String,
    pub scope_id: Uuid,
    pub host_id: Uuid,
    pub environment: String,
    pub server_host: String,
    pub port: u16,
    pub tls_mode: String,
    pub service_owner: String,
    pub schema: String,
    pub minimum_schema_version: i64,
    pub expected_database: String,
    pub database_url_file: String,
    pub credential_generation: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyEnvelope {
    pub publication_id: Uuid,
    pub release_version: u64,
    pub policy_snapshot_id: Uuid,
    pub policy_version: u64,
    pub policy_digest: String,
    pub content_digest: String,
    pub audience: String,
    pub host: String,
    pub service_id: String,
    pub env_tag: String,
    pub source_event_sequence: i64,
    pub schema_version: u32,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub refresh_after: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revocation_epoch: u64,
    pub compatibility_generation: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPolicy {
    pub agent_def_id: Uuid,
    pub definition_version: i64,
    pub prompt: AgentPromptPolicy,
    pub model: AgentModelPolicy,
    #[serde(default)]
    pub skills: Vec<AgentSkillPolicy>,
    pub policy_snapshot: PolicySnapshot,
    pub execution: AgentExecutionPolicy,
    pub catalog: AgentCatalogPolicy,
    pub memory: AgentMemoryPolicy,
    pub knowledge: AgentKnowledgePolicy,
    pub channel: Value,
    pub data_boundary: Value,
    pub gateway_delegation: Value,
    pub session: AgentSessionPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPromptPolicy {
    pub system: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkillPolicy {
    pub skill_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub content_markdown: String,
    pub version: String,
    pub aggregate_version: i64,
    pub digest: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelPolicy {
    pub provider: String,
    pub alias: String,
    pub temperature: f64,
    pub maximum_tokens: u64,
    pub gateway: LlmGatewayClientPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmGatewayClientPolicy {
    pub name: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExecutionPolicy {
    pub maximum_turn_seconds: u64,
    pub maximum_model_calls: usize,
    pub maximum_action_calls: usize,
    pub maximum_user_message_bytes: usize,
    pub maximum_tool_argument_bytes: usize,
    pub maximum_tool_output_bytes: usize,
    pub maximum_gateway_response_bytes: usize,
    pub maximum_response_bytes: usize,
    pub maximum_output_depth: usize,
    pub maximum_output_items: usize,
    pub maximum_turn_tokens: u64,
    pub execution_api_url: String,
    #[serde(default)]
    pub quota_policies: Vec<AgentQuotaPolicy>,
    #[serde(default)]
    pub model_rates: Vec<AgentModelRatePolicy>,
    #[serde(default)]
    pub service_pools: Vec<AgentServicePoolPolicy>,
    #[serde(default)]
    pub edge_runner_bindings: Vec<AgentEdgeRunnerBindingPolicy>,
    #[serde(default)]
    pub approval_rules: Vec<Value>,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
    pub coding_profile: Option<CodingProfilePolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelRatePolicy {
    pub rate_id: Uuid,
    pub provider: String,
    pub model: String,
    pub input_cost_micros_per_million: i64,
    pub output_cost_micros_per_million: i64,
    pub effective_at: DateTime<Utc>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    pub aggregate_version: i64,
    pub digest: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentEdgeRunnerBindingPolicy {
    pub edge_binding_id: Uuid,
    pub principal_id: String,
    pub runner_id: String,
    pub backend_id: String,
    pub compatibility_digest: String,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default)]
    pub allowed_actions: Vec<String>,
    pub action_policies: Value,
    pub expires_at: DateTime<Utc>,
    pub revocation_epoch: u64,
    pub aggregate_version: i64,
    pub digest: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentQuotaPolicy {
    pub quota_id: Uuid,
    pub policy_version: i64,
    pub policy_digest: String,
    pub scope_kind: String,
    pub scope_key: String,
    #[serde(default)]
    pub maximum_active_sessions: Option<i32>,
    #[serde(default)]
    pub maximum_queued_turns: Option<i32>,
    #[serde(default)]
    pub maximum_running_turns: Option<i32>,
    #[serde(default)]
    pub token_budget_per_window: Option<i64>,
    #[serde(default)]
    pub cost_budget_micros_per_window: Option<i64>,
    pub window_seconds: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentServicePoolPolicy {
    pub pool_id: Uuid,
    pub compatibility_dimensions: Value,
    pub compatibility_digest: String,
    pub maximum_concurrency: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingProfilePolicy {
    pub schema_version: u16,
    pub product_profile_digest: String,
    pub repository_uri_prefix: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub adapter_protocol_version: String,
    pub action_kind: String,
    pub compatibility_digest: String,
    pub image_digest: String,
    pub capability_digest: String,
    pub template_id: String,
    pub template_version: u32,
    pub template_digest: String,
    pub executable: String,
    pub schema_digest: String,
    pub required_features: BTreeSet<String>,
    pub binary_digest: String,
    pub qualification: coding_agent_runtime::CodingAdapterQualification,
    pub model: String,
    pub review_model: String,
    pub authentication_profile: coding_agent_runtime::CodingAuthenticationProfile,
    #[serde(default)]
    pub enterprise_gateway: Option<CodingGatewayPolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingGatewayPolicy {
    pub base_url: String,
    pub credential_target: String,
    pub audience: String,
    pub route_digest: String,
    pub budget_policy_id: String,
    pub maximum_requests: u32,
    pub maximum_tokens: u64,
    pub maximum_cost_micros: u64,
    pub maximum_response_bytes: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCatalogPolicy {
    pub cache_ttl_seconds: u64,
    pub stale_on_error_seconds: u64,
    #[serde(default)]
    pub effective_catalog: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentMemoryPolicy {
    pub write_mode: String,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_string")]
    pub personal_profile_digest: Option<String>,
    #[serde(default)]
    pub rules: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentKnowledgePolicy {
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_string")]
    pub endpoint: Option<String>,
    pub allow_private_plaintext: bool,
    #[serde(default)]
    pub bindings: Vec<Value>,
    pub retrieval: AgentKnowledgeRetrievalPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentKnowledgeRetrievalPolicy {
    pub top_k: usize,
    pub token_budget: usize,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
    pub filters: Option<RetrievalFilters>,
}

fn deserialize_optional_non_empty_map<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    let value = match value {
        Value::Null => return Ok(None),
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            serde_yaml::from_str::<Value>(value).map_err(D::Error::custom)?
        }
        value => value,
    };

    match &value {
        Value::Null => Ok(None),
        Value::Object(entries) if entries.is_empty() => Ok(None),
        Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(D::Error::custom),
        _ => Err(D::Error::custom(
            "expected null, an empty value, or a JSON/YAML map",
        )),
    }
}

fn deserialize_optional_uuid<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(None),
        Value::String(value) if value.trim().is_empty() => Ok(None),
        Value::String(value) => Uuid::parse_str(value.trim())
            .map(Some)
            .map_err(D::Error::custom),
        _ => Err(D::Error::custom("expected an optional UUID string")),
    }
}

fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Value::deserialize(deserializer)?;
    match value {
        Value::Null => Ok(None),
        Value::String(value)
            if value.trim().is_empty() || value.trim().eq_ignore_ascii_case("null") =>
        {
            Ok(None)
        }
        Value::String(value) => Ok(Some(value)),
        _ => Err(D::Error::custom("expected an optional string")),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionPolicy {
    pub idle_seconds: u64,
    pub maximum_seconds: u64,
    pub maximum_active_sessions: u64,
    pub maximum_queued_turns: u64,
}

impl AgentConfig {
    pub fn validate(
        &self,
        host: &str,
        service_id: &str,
        env_tag: &str,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let envelope = &self.runtime_policy;
        let policy = &self.agent_policy;
        let store = &self.operational_store;
        if store.contract_version != 2
            || store.deployment_profile != "CUSTOMER_MANAGED"
            || store.scope_kind != "HOST"
            || store.scope_id != store.host_id
            || store.environment != envelope.env_tag
            || store.service_owner != "light-agent"
            || store.schema != agent_store::EXPECTED_SCHEMA
            || store.minimum_schema_version < 1
            || store.credential_generation < 1
            || !operational_store::runtime::postgres_identifier(&store.expected_database)
            || store.profile_id.trim().is_empty()
            || !store.database_url_file.starts_with('/')
            || !is_sha256_digest(&store.binding_digest)
        {
            return Err(
                "operationalStore does not match the Agent Host/environment authority".to_string(),
            );
        }
        if envelope.schema_version != 1 {
            return Err(format!(
                "unsupported Agent policy schema version {}",
                envelope.schema_version
            ));
        }
        if envelope.audience != "agent" {
            return Err("runtimePolicy.audience must be agent".to_string());
        }
        a2a_core::RuntimeIdentity {
            host: envelope.host.clone(),
            service_id: envelope.service_id.clone(),
            env_tag: envelope.env_tag.clone(),
        }
        .validate_against(host, service_id, env_tag)
        .map_err(|_| {
            "runtimePolicy host, serviceId, and envTag do not match the running service".to_string()
        })?;
        if envelope.policy_snapshot_id != policy.policy_snapshot.snapshot_id {
            return Err(
                "runtimePolicy.policySnapshotId does not match agentPolicy.policySnapshot"
                    .to_string(),
            );
        }
        if now < envelope.valid_from {
            return Err("Agent policy is not valid yet".to_string());
        }
        if now >= envelope.expires_at {
            return Err("Agent policy has expired".to_string());
        }
        if envelope.created_at > envelope.valid_from
            || envelope.valid_from >= envelope.refresh_after
            || envelope.refresh_after >= envelope.expires_at
        {
            return Err("Agent policy validity window is invalid".to_string());
        }
        if policy.definition_version <= 0 {
            return Err("agentPolicy.definitionVersion must be positive".to_string());
        }
        if policy.prompt.system.trim().is_empty() {
            return Err("agentPolicy.prompt.system is required".to_string());
        }
        for skill in &policy.skills {
            if skill.name.trim().is_empty()
                || skill.content_markdown.trim().is_empty()
                || skill.version.trim().is_empty()
                || skill.aggregate_version <= 0
                || !skill.digest.starts_with("sha256:")
            {
                return Err("agentPolicy.skills contains an invalid skill projection".to_string());
            }
        }
        if policy.model.provider != "gateway" {
            return Err("agentPolicy.model.provider must be gateway".to_string());
        }
        if policy.model.alias.trim().is_empty() {
            return Err("agentPolicy.model.alias is required".to_string());
        }
        if policy.model.gateway.base_url.trim().is_empty() {
            return Err("agentPolicy.model.gateway.baseUrl is required".to_string());
        }
        if !policy.model.temperature.is_finite() || !(0.0..=2.0).contains(&policy.model.temperature)
        {
            return Err("agentPolicy.model.temperature must be between 0 and 2".to_string());
        }
        if policy.model.maximum_tokens == 0 || policy.model.maximum_tokens > u64::from(u32::MAX) {
            return Err(
                "agentPolicy.model.maximumTokens must be between 1 and 4294967295".to_string(),
            );
        }
        if policy.model.maximum_tokens > policy.execution.maximum_turn_tokens {
            return Err(
                "agentPolicy.model.maximumTokens cannot exceed execution.maximumTurnTokens"
                    .to_string(),
            );
        }
        let mut pool_ids = HashSet::new();
        let mut pool_digests = HashSet::new();
        for pool in &policy.execution.service_pools {
            if !pool_ids.insert(pool.pool_id)
                || !pool_digests.insert(pool.compatibility_digest.as_str())
            {
                return Err("agentPolicy.execution.servicePools contains a duplicate".to_string());
            }
            if pool.maximum_concurrency <= 0 {
                return Err(
                    "agentPolicy.execution.servicePools.maximumConcurrency must be positive"
                        .to_string(),
                );
            }
            let dimensions = pool.compatibility_dimensions.as_object().ok_or_else(|| {
                "agentPolicy.execution.servicePools.compatibilityDimensions must be an object"
                    .to_string()
            })?;
            for required in [
                "tenant",
                "identity",
                "modelCredential",
                "region",
                "dataBoundary",
                "network",
                "retention",
                "profile",
            ] {
                if dimensions
                    .get(required)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(format!(
                        "agentPolicy.execution.servicePools compatibility dimension {required} is missing"
                    ));
                }
            }
            let computed =
                execution_runner_protocol::canonical_sha256(&pool.compatibility_dimensions)
                    .map_err(|error| {
                        format!("failed to digest service-pool dimensions: {error}")
                    })?;
            if pool.compatibility_digest != computed {
                return Err(
                    "agentPolicy.execution.servicePools compatibility digest is stale".to_string(),
                );
            }
        }
        let mut quota_ids = HashSet::new();
        for quota in &policy.execution.quota_policies {
            if !quota_ids.insert(quota.quota_id)
                || quota.policy_version <= 0
                || !is_sha256_digest(&quota.policy_digest)
                || !matches!(
                    quota.scope_kind.as_str(),
                    "HOST" | "PRINCIPAL" | "AGENT" | "PROFILE" | "PROVIDER" | "POOL"
                )
                || quota.scope_key.trim().is_empty()
                || !(1..=86_400).contains(&quota.window_seconds)
                || [
                    quota.maximum_active_sessions.map(i64::from),
                    quota.maximum_queued_turns.map(i64::from),
                    quota.maximum_running_turns.map(i64::from),
                    quota.token_budget_per_window,
                    quota.cost_budget_micros_per_window,
                ]
                .into_iter()
                .flatten()
                .any(|limit| limit < 0)
            {
                return Err(
                    "agentPolicy.execution.quotaPolicies contains an invalid pinned policy"
                        .to_string(),
                );
            }
        }
        let mut rate_ids = HashSet::new();
        for rate in &policy.execution.model_rates {
            if !rate_ids.insert(rate.rate_id)
                || rate.provider.trim().is_empty()
                || rate.model.trim().is_empty()
                || rate.input_cost_micros_per_million < 0
                || rate.output_cost_micros_per_million < 0
                || rate.aggregate_version <= 0
                || !is_sha256_digest(&rate.digest)
                || rate
                    .expires_at
                    .is_some_and(|expires| expires <= rate.effective_at)
            {
                return Err(
                    "agentPolicy.execution.modelRates contains invalid pinned evidence".to_string(),
                );
            }
        }
        let mut edge_binding_ids = HashSet::new();
        for binding in &policy.execution.edge_runner_bindings {
            let action_policies = binding.action_policies.as_object();
            if !edge_binding_ids.insert(binding.edge_binding_id)
                || binding.principal_id.trim().is_empty()
                || binding.runner_id.trim().is_empty()
                || binding.backend_id.trim().is_empty()
                || !is_sha256_digest(&binding.compatibility_digest)
                || binding.allowed_actions.is_empty()
                || action_policies.is_none()
                || !binding.allowed_actions.iter().all(|action| {
                    action_policies.is_some_and(|policies| policies.contains_key(action))
                })
                || binding.expires_at <= now
                || binding.aggregate_version <= 0
                || !is_sha256_digest(&binding.digest)
            {
                return Err(
                    "agentPolicy.execution.edgeRunnerBindings contains invalid pinned evidence"
                        .to_string(),
                );
            }
        }
        if policy.session.idle_seconds == 0
            || policy.session.maximum_seconds == 0
            || policy.session.idle_seconds > policy.session.maximum_seconds
            || policy.session.maximum_active_sessions == 0
            || policy.session.maximum_queued_turns == 0
            || policy.session.idle_seconds > i64::MAX as u64
            || policy.session.maximum_seconds > i64::MAX as u64
        {
            return Err("agentPolicy.session limits are invalid".to_string());
        }
        if policy.knowledge.retrieval.top_k == 0
            || policy.knowledge.retrieval.top_k > 100
            || policy.knowledge.retrieval.token_budget == 0
        {
            return Err("agentPolicy.knowledge.retrieval limits are invalid".to_string());
        }
        if self.a2a_policy.enabled
            && (self.a2a_policy.agent_ref.trim().is_empty()
                || self.a2a_policy.binding_id.is_none()
                || self.a2a_policy.publication_id.is_none()
                || !is_sha256_digest(&self.a2a_policy.policy_digest)
                || !is_sha256_digest(&self.a2a_policy.content_digest)
                || self.a2a_policy.protocol_profile.is_none()
                || self.a2a_policy.allowed_operations.is_empty()
                || self.a2a_policy.allowed_principal_prefixes.is_empty()
                || self
                    .a2a_policy
                    .allowed_principal_prefixes
                    .iter()
                    .any(|prefix| prefix.is_empty())
                || self.a2a_policy.public_url.trim().is_empty()
                || self.a2a_policy.agent_card.is_none()
                || self.a2a_policy.artifact_retention.is_none()
                || !self.a2a_policy.artifact_root_directory.is_absolute()
                || !self
                    .a2a_policy
                    .authorization_context_key_file
                    .starts_with('/'))
        {
            return Err(
                "a2aPolicy must bind this native Agent publication, policy, and key file"
                    .to_string(),
            );
        }
        if self.a2a_policy.enabled {
            let mut aliases = HashSet::new();
            let mut skill_ids = HashSet::new();
            for mapping in &self.a2a_policy.public_skills {
                let skill = policy
                    .skills
                    .iter()
                    .find(|skill| skill.skill_id == mapping.skill_id)
                    .ok_or_else(|| {
                        "a2aPolicy.publicSkills references an unassigned Agent skill".to_string()
                    })?;
                if mapping.publication_alias.is_empty()
                    || mapping.publication_alias.len() > 128
                    || !mapping
                        .publication_alias
                        .bytes()
                        .next()
                        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
                    || !mapping.publication_alias.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'.' | b'_' | b'-')
                    })
                    || !aliases.insert(mapping.publication_alias.clone())
                    || !skill_ids.insert(mapping.skill_id)
                    || mapping.skill_version != skill.version
                    || mapping.skill_digest != skill.digest
                {
                    return Err(
                        "a2aPolicy.publicSkills does not match the immutable Agent skill projection"
                            .into(),
                    );
                }
            }
            let card_skill_ids = self
                .a2a_policy
                .agent_card
                .as_ref()
                .and_then(|card| card.get("skills"))
                .and_then(Value::as_array)
                .ok_or_else(|| "native Agent Card skills are required".to_string())?
                .iter()
                .map(|skill| {
                    skill
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| "native Agent Card contains an invalid skill ID".to_string())
                })
                .collect::<Result<HashSet<_>, _>>()?;
            if card_skill_ids != aliases {
                return Err("native Agent Card skills do not match a2aPolicy.publicSkills".into());
            }
            let mut digest_value = serde_json::to_value(&self.a2a_policy)
                .map_err(|error| format!("cannot canonicalize a2aPolicy: {error}"))?;
            digest_value
                .as_object_mut()
                .expect("a2aPolicy serializes as an object")
                .remove("contentDigest");
            validate_digest(
                "a2aPolicy.contentDigest",
                &self.a2a_policy.content_digest,
                &canonical_digest(&digest_value)
                    .map_err(|error| format!("failed to digest a2aPolicy: {error}"))?,
            )?;
        }
        if let Some(retention) = self.a2a_policy.artifact_retention.as_ref()
            && (retention.profile_id.trim().is_empty()
                || retention.task_retention_days == 0
                || retention.task_retention_days > 3650
                || retention.artifact_retention_days == 0
                || retention.artifact_retention_days > 3650
                || retention.maximum_artifact_bytes == 0
                || retention.maximum_artifact_bytes > 1_099_511_627_776
                || retention.access_policy_ref.trim().is_empty())
        {
            return Err("native A2A artifact retention policy is invalid".into());
        }
        if let Some(profile) = self.a2a_policy.protocol_profile.as_ref() {
            profile
                .validate()
                .map_err(|error| format!("invalid native A2A protocol profile: {error}"))?;
            if !profile.advertised_extensions.is_empty()
                || !profile.allowed_inbound_extensions.is_empty()
                || !profile.required_extensions.is_empty()
            {
                return Err("initial native A2A profile must not activate extensions".into());
            }
            a2a_protocol::rewrite_agent_card_url(
                self.a2a_policy.agent_card.as_ref().expect("checked above"),
                &self.a2a_policy.public_url,
            )
            .map_err(|error| format!("invalid native Agent Card: {error}"))?;
            match (
                self.a2a_policy
                    .agent_card
                    .as_ref()
                    .and_then(|card| card.get("signatures")),
                self.a2a_policy.trusted_signing_profile.as_ref(),
            ) {
                (Some(_), Some(profile)) => a2a_protocol::verify_signed_agent_card(
                    self.a2a_policy.agent_card.as_ref().expect("checked above"),
                    profile,
                )
                .map_err(|error| format!("invalid native Agent Card signature: {error}"))?,
                _ => {
                    return Err(
                        "native Agent Card and trusted signing profile must be projected together"
                            .into(),
                    );
                }
            }
        }
        if self.a2a_outbound.enabled {
            if self.a2a_outbound.bindings.is_empty()
                || !self
                    .a2a_outbound
                    .authorization_context_key_file
                    .starts_with('/')
            {
                return Err("a2aOutbound requires bindings and a server-owned key file".into());
            }
            let mut aliases = HashSet::new();
            let mut tool_ids = HashSet::new();
            for binding in &self.a2a_outbound.bindings {
                let endpoint = Url::parse(&binding.gateway_uri)
                    .map_err(|_| "a2aOutbound gatewayUri is invalid".to_string())?;
                if binding.agent_ref.trim().is_empty()
                    || binding.display_name.trim().is_empty()
                    || binding.description.trim().is_empty()
                    || !aliases.insert(binding.agent_ref.clone())
                    || !tool_ids.insert(binding.catalog_tool_id)
                    || !is_sha256_digest(&binding.policy_digest)
                    || !is_sha256_digest(&binding.data_boundary_digest)
                    || endpoint.scheme() != "https"
                    || endpoint.host_str().is_none()
                    || !endpoint.username().is_empty()
                    || endpoint.password().is_some()
                    || endpoint.query().is_some()
                    || endpoint.fragment().is_some()
                    || !matches!(binding.protocol_version.as_str(), "0.3" | "1.0")
                    || binding.maximum_delegation_depth == 0
                    || binding.maximum_budget_units == 0
                    || binding
                        .allowed_skill_ids
                        .iter()
                        .any(|value| value.trim().is_empty())
                {
                    return Err("a2aOutbound contains an invalid immutable binding".into());
                }
            }
        } else if !self.a2a_outbound.bindings.is_empty() {
            return Err("a2aOutbound bindings cannot be projected while disabled".into());
        }
        validate_digest(
            "runtimePolicy.policyDigest",
            &envelope.policy_digest,
            &persisted_policy_digest(&policy.policy_snapshot)
                .map_err(|error| format!("failed to digest Agent policy: {error}"))?,
        )?;
        validate_digest(
            "runtimePolicy.contentDigest",
            &envelope.content_digest,
            &canonical_digest(policy)
                .map_err(|error| format!("failed to digest Agent projection: {error}"))?,
        )?;
        Ok(())
    }

    pub fn compiled_system_prompt(&self) -> String {
        let mut prompt = self.agent_policy.prompt.system.trim().to_string();
        for skill in &self.agent_policy.skills {
            prompt.push_str("\n\n## Skill: ");
            prompt.push_str(skill.name.trim());
            if let Some(description) = skill.description.as_deref() {
                let description = description.trim();
                if !description.is_empty() {
                    prompt.push_str("\n");
                    prompt.push_str(description);
                }
            }
            prompt.push_str("\n\n");
            prompt.push_str(skill.content_markdown.trim());
        }
        prompt
    }

    pub fn a2a_skill_mapping_digest(&self) -> Result<String, String> {
        canonical_digest(&self.a2a_policy.public_skills)
            .map_err(|error| format!("failed to digest native A2A skill mapping: {error}"))
    }
}

fn is_sha256_digest(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn persisted_policy_digest(policy: &PolicySnapshot) -> Result<String, serde_json::Error> {
    // Keep the deployed Portal/Agent digest contract until a coordinated
    // canonical-digest migration and database backfill are available.
    Ok(sha256_digest(&serde_json::to_vec(policy)?))
}

fn validate_digest(name: &str, configured: &str, actual: &str) -> Result<(), String> {
    if configured != actual {
        return Err(format!(
            "{name} failed digest verification: configured={configured} actual={actual}"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::sha256_digest;
    use chrono::Duration;
    use std::collections::BTreeMap;

    fn digest(value: &str) -> String {
        sha256_digest(value.as_bytes())
    }

    #[test]
    fn java_portal_flattened_empty_values_preserve_digest_contract() {
        let fixture = r#"{"agentDefId":"019d82bf-ab5e-791a-885c-d08aafa2b614","policySnapshot":{"snapshotId":"01a05d30-0000-7000-8000-000000000002","definitionDigest":"sha256:be35b6d853200d676a398e2f3ce8f58dc0f464a7c124ef0051c45d43753e65a1","productProfileDigest":"sha256:21be0163c81abd92121c99dddc95d0ac7d78647fe8f78172c1655a76e143bc31","modelDigest":"sha256:bd845e5a18e7c20041c25c6539c497f41ce27533de12e02f3b22000bf2d23cb2","catalogDigest":"sha256:0335a42d3d5fb9fd161ae19413e00e4421d8cca3e8e9f0be6978860f9e98677a","memoryDigest":"sha256:f03b2d6941c243aa01e80e685fef205d54961742ae5735579c0b2287077a383a","executionDigest":"sha256:13e1f4b064bd36177be2126f08278d08170f7f9887800dc5f68e2df9116a990c","channelDigest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","dataBoundaryDigest":"sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a","tools":{}},"session":{"idleSeconds":3600,"maximumSeconds":86400,"maximumActiveSessions":1,"maximumQueuedTurns":100},"memory":{"writeMode":"operational","personalProfileDigest":"","rules":{}},"model":{"provider":"gateway","alias":"assistant-dev","temperature":0.7,"maximumTokens":1000000,"gateway":{"name":"llm-gateway","baseUrl":"https://llm-gateway:8443/v1"}},"definitionVersion":2,"dataBoundary":{},"knowledge":{"endpoint":"","allowPrivatePlaintext":false,"bindings":[],"retrieval":{"topK":5,"tokenBudget":2000,"filters":{}}},"skills":[],"gatewayDelegation":{},"execution":{"maximumTurnSeconds":120,"maximumModelCalls":10,"maximumActionCalls":20,"maximumUserMessageBytes":65536,"maximumToolArgumentBytes":65536,"maximumToolOutputBytes":65536,"maximumGatewayResponseBytes":1048576,"maximumResponseBytes":65536,"maximumOutputDepth":16,"maximumOutputItems":1024,"maximumTurnTokens":1000000,"executionApiUrl":"https://controller:8438/","quotaPolicies":[],"modelRates":[],"servicePools":[],"edgeRunnerBindings":[],"approvalRules":[],"codingProfile":{}},"prompt":{"system":"Account Agent"},"channel":{},"catalog":{"cacheTtlSeconds":60,"staleOnErrorSeconds":300,"effectiveCatalog":{}}}"#;
        let policy: AgentPolicy =
            serde_json::from_str(fixture).expect("Java Portal Agent policy fixture");

        assert_eq!(
            persisted_policy_digest(&policy.policy_snapshot).unwrap(),
            "sha256:614a33b62286877becb54b64fc72743760b9b5f34c73cff25d8dd77d54f0ae77"
        );
        assert_eq!(
            canonical_digest(&policy).unwrap(),
            "sha256:30701db286777609bd56f31ce07e745799333e1347b68a0d81462157b8109e99"
        );
    }

    #[test]
    fn portal_null_string_optionals_do_not_change_policy_content() {
        let memory: AgentMemoryPolicy = serde_json::from_value(serde_json::json!({
            "writeMode": "operational",
            "personalProfileDigest": "null",
            "rules": {}
        }))
        .expect("Portal memory projection");
        let knowledge: AgentKnowledgePolicy = serde_json::from_value(serde_json::json!({
            "endpoint": "null",
            "allowPrivatePlaintext": false,
            "bindings": [],
            "retrieval": {"topK": 5, "tokenBudget": 2000, "filters": {}}
        }))
        .expect("Portal knowledge projection");

        assert_eq!(None, memory.personal_profile_digest);
        assert_eq!(None, knowledge.endpoint);
        assert!(knowledge.retrieval.filters.is_none());
    }

    #[test]
    fn portal_a2a_projection_shapes_deserialize_without_runtime_drift() {
        let retention: A2aArtifactRetentionPolicy = serde_json::from_value(serde_json::json!({
            "profileId":"01964b05-552a-7c4b-9184-6857e7f3dc5f",
            "taskRetentionDays":30,
            "artifactRetentionDays":60,
            "maximumArtifactBytes":1048576,
            "accessPolicyRef":"account-agent-artifacts"
        }))
        .expect("Portal artifact retention projection");
        assert_eq!(retention.profile_id, "01964b05-552a-7c4b-9184-6857e7f3dc5f");

        let binding: OutboundA2aBinding = serde_json::from_value(serde_json::json!({
            "agentRef":"account.agent",
            "displayName":"Account agent",
            "description":"Governed account operations",
            "catalogToolId":"01964b05-552a-7c4b-9184-6857e7f3dc60",
            "bindingId":"01964b05-552a-7c4b-9184-6857e7f3dc61",
            "publicationId":"01964b05-552a-7c4b-9184-6857e7f3dc62",
            "policyDigest":format!("sha256:{}","a".repeat(64)),
            "gatewayUri":"https://gateway.example/internal/a2a/outbound/account.agent",
            "protocolVersion":"1.0",
            "dataBoundaryDigest":format!("sha256:{}","b".repeat(64)),
            "maximumDelegationDepth":65535,
            "maximumBudgetUnits":65536,
            "allowedSkillIds":["account.lookup"]
        }))
        .expect("Portal outbound A2A binding projection");
        assert_eq!(binding.maximum_delegation_depth, u16::MAX);
        assert!(serde_json::from_value::<OutboundA2aBinding>(serde_json::json!({
            "agentRef":"account.agent","displayName":"Account agent","description":"description",
            "catalogToolId":"a2a.account.agent","bindingId":Uuid::now_v7(),
            "publicationId":Uuid::now_v7(),"policyDigest":format!("sha256:{}","a".repeat(64)),
            "gatewayUri":"https://gateway.example/a2a","protocolVersion":"1.0",
            "dataBoundaryDigest":format!("sha256:{}","b".repeat(64)),
            "maximumDelegationDepth":4,"maximumBudgetUnits":1,"allowedSkillIds":[]
        })).is_err());
    }

    fn config(now: DateTime<Utc>) -> AgentConfig {
        let snapshot_id = Uuid::now_v7();
        let policy_snapshot = PolicySnapshot {
            snapshot_id,
            definition_digest: digest("definition"),
            product_profile_digest: digest("profile"),
            model_digest: digest("model"),
            catalog_digest: digest("catalog"),
            memory_digest: digest("memory"),
            execution_digest: digest("execution"),
            channel_digest: digest("channel"),
            data_boundary_digest: digest("boundary"),
            tools: BTreeMap::new(),
        };
        let agent_policy = AgentPolicy {
            agent_def_id: Uuid::now_v7(),
            definition_version: 3,
            prompt: AgentPromptPolicy {
                system: "help safely".into(),
            },
            model: AgentModelPolicy {
                provider: "gateway".into(),
                alias: "support-chat".into(),
                temperature: 0.3,
                maximum_tokens: 4096,
                gateway: LlmGatewayClientPolicy {
                    name: "llm-gateway".into(),
                    base_url: "https://llm-gateway:8443/v1".into(),
                },
            },
            skills: vec![AgentSkillPolicy {
                skill_id: Uuid::now_v7(),
                name: "Support".into(),
                description: Some("Answer product questions".into()),
                content_markdown: "Use only verified product facts.".into(),
                version: "1.0.0".into(),
                aggregate_version: 1,
                digest: digest("support-skill"),
            }],
            policy_snapshot,
            execution: AgentExecutionPolicy {
                maximum_turn_seconds: 120,
                maximum_model_calls: 10,
                maximum_action_calls: 20,
                maximum_user_message_bytes: 65_536,
                maximum_tool_argument_bytes: 65_536,
                maximum_tool_output_bytes: 65_536,
                maximum_gateway_response_bytes: 1_048_576,
                maximum_response_bytes: 65_536,
                maximum_output_depth: 16,
                maximum_output_items: 1024,
                maximum_turn_tokens: 65_536,
                execution_api_url: "https://controller:8438/".into(),
                quota_policies: vec![],
                model_rates: vec![],
                service_pools: vec![],
                edge_runner_bindings: vec![],
                approval_rules: vec![],
                coding_profile: None,
            },
            catalog: AgentCatalogPolicy {
                cache_ttl_seconds: 60,
                stale_on_error_seconds: 300,
                effective_catalog: None,
            },
            memory: AgentMemoryPolicy {
                write_mode: "operational".into(),
                personal_profile_digest: None,
                rules: serde_json::json!({}),
            },
            knowledge: AgentKnowledgePolicy {
                endpoint: None,
                allow_private_plaintext: false,
                bindings: vec![],
                retrieval: AgentKnowledgeRetrievalPolicy {
                    top_k: 5,
                    token_budget: 2_000,
                    filters: None,
                },
            },
            channel: serde_json::json!({}),
            data_boundary: serde_json::json!({}),
            gateway_delegation: serde_json::json!({}),
            session: AgentSessionPolicy {
                idle_seconds: 3600,
                maximum_seconds: 86_400,
                maximum_active_sessions: 10,
                maximum_queued_turns: 100,
            },
        };
        let policy_digest = persisted_policy_digest(&agent_policy.policy_snapshot).unwrap();
        let content_digest = canonical_digest(&agent_policy).unwrap();
        let host_id = Uuid::now_v7();
        AgentConfig {
            operational_store: OperationalStoreProjection {
                contract_version: 2,
                binding_id: Uuid::now_v7(),
                binding_digest: digest("binding"),
                profile_id: "dev-dedicated".into(),
                deployment_profile: "CUSTOMER_MANAGED".into(),
                scope_kind: "HOST".into(),
                scope_id: host_id,
                host_id,
                environment: "dev".into(),
                server_host: "postgres".into(),
                port: 5432,
                tls_mode: "DISABLE".into(),
                service_owner: "light-agent".into(),
                schema: "agent_ops".into(),
                minimum_schema_version: 2,
                expected_database: "operations".into(),
                database_url_file: "/run/secrets/operational-database-url".into(),
                credential_generation: 1,
            },
            runtime_policy: RuntimePolicyEnvelope {
                publication_id: Uuid::now_v7(),
                release_version: 4,
                policy_snapshot_id: snapshot_id,
                policy_version: 3,
                policy_digest,
                content_digest,
                audience: "agent".into(),
                host: "agent.dev.lightapi.net".into(),
                service_id: "com.networknt.agent.support-1.0.0".into(),
                env_tag: "dev".into(),
                source_event_sequence: 42,
                schema_version: 1,
                created_at: now - Duration::minutes(1),
                valid_from: now - Duration::seconds(30),
                refresh_after: now + Duration::minutes(5),
                expires_at: now + Duration::minutes(15),
                revocation_epoch: 1,
                compatibility_generation: 1,
            },
            portal_association: PortalAssociationEvidence {
                runtime_instance_id: Uuid::now_v7(),
            },
            agent_policy,
            a2a_policy: NativeA2aPolicy::default(),
            a2a_outbound: OutboundA2aPolicy::default(),
        }
    }

    #[test]
    fn validates_bound_immutable_agent_projection() {
        let now = Utc::now();
        let config = config(now);
        assert!(
            config
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .is_ok()
        );
        assert_eq!(
            config.compiled_system_prompt(),
            "help safely\n\n## Skill: Support\nAnswer product questions\n\nUse only verified product facts."
        );
    }

    #[test]
    fn rejects_direct_provider_and_tampered_content() {
        let now = Utc::now();
        let mut direct = config(now);
        direct.agent_policy.model.provider = "openai".into();
        assert!(
            direct
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("must be gateway")
        );

        let mut tampered = config(now);
        tampered.agent_policy.model.alias = "unpublished-alias".into();
        assert!(
            tampered
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("contentDigest")
        );
    }

    #[test]
    fn rejects_unbounded_session_model_and_retrieval_limits() {
        let now = Utc::now();

        let mut session = config(now);
        session.agent_policy.session.maximum_seconds = i64::MAX as u64 + 1;
        assert!(
            session
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("session limits")
        );

        let mut model = config(now);
        model.agent_policy.model.maximum_tokens = u64::from(u32::MAX) + 1;
        assert!(
            model
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("maximumTokens")
        );

        let mut retrieval = config(now);
        retrieval.agent_policy.knowledge.retrieval.top_k = 0;
        assert!(
            retrieval
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("retrieval limits")
        );
    }

    #[test]
    fn rejects_stale_pool_and_invalid_quota_projection_evidence() {
        let now = Utc::now();
        let mut pool = config(now);
        pool.agent_policy.execution.service_pools = vec![AgentServicePoolPolicy {
            pool_id: Uuid::now_v7(),
            compatibility_dimensions: serde_json::json!({
                "tenant": pool.operational_store.host_id,
                "identity": "isolated",
                "modelCredential": "gateway",
                "region": "ca-central",
                "dataBoundary": pool.agent_policy.policy_snapshot.data_boundary_digest,
                "network": "private",
                "retention": "standard",
                "profile": pool.agent_policy.policy_snapshot.product_profile_digest
            }),
            compatibility_digest: "stale".into(),
            maximum_concurrency: 4,
            enabled: true,
        }];
        assert!(
            pool.validate(
                "agent.dev.lightapi.net",
                "com.networknt.agent.support-1.0.0",
                "dev",
                now
            )
            .unwrap_err()
            .contains("compatibility digest is stale")
        );

        let mut quota = config(now);
        quota.agent_policy.execution.quota_policies = vec![AgentQuotaPolicy {
            quota_id: Uuid::now_v7(),
            policy_version: 1,
            policy_digest: "not-a-digest".into(),
            scope_kind: "HOST".into(),
            scope_key: quota.operational_store.host_id.to_string(),
            maximum_active_sessions: Some(10),
            maximum_queued_turns: None,
            maximum_running_turns: None,
            token_budget_per_window: None,
            cost_budget_micros_per_window: None,
            window_seconds: 60,
            enabled: true,
        }];
        assert!(
            quota
                .validate(
                    "agent.dev.lightapi.net",
                    "com.networknt.agent.support-1.0.0",
                    "dev",
                    now
                )
                .unwrap_err()
                .contains("invalid pinned policy")
        );
    }

    #[test]
    fn optional_structured_policy_maps_accept_disabled_and_populated_forms() {
        #[derive(Deserialize)]
        struct CodingProfileValue {
            #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
            value: Option<CodingProfilePolicy>,
        }

        #[derive(Deserialize, Serialize)]
        struct CatalogValue {
            #[serde(default)]
            value: Option<Value>,
        }

        #[derive(Deserialize)]
        struct FiltersValue {
            #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
            value: Option<RetrievalFilters>,
        }

        for yaml in ["{}", "value: null", "value: ''", "value: {}"] {
            let parsed: CodingProfileValue = serde_yaml::from_str(yaml).unwrap();
            assert!(parsed.value.is_none(), "{yaml}");
        }

        let coding: CodingProfileValue = serde_yaml::from_str(
            r#"
value:
  schemaVersion: 1
  productProfileDigest: sha256:profile
  repositoryUriPrefix: file:///var/lib/light-agent/repositories/
  adapterId: codex-app-server-v1
  adapterVersion: 0.153.2
  adapterProtocolVersion: codex-app-server-v2
  actionKind: coding.codex-app-server-v1
  compatibilityDigest: sha256:compatibility
  imageDigest: sha256:image
  capabilityDigest: sha256:capability
  templateId: coding-codex-app-server-v1
  templateVersion: 1
  templateDigest: sha256:template
  executable: /usr/local/bin/codex
  schemaDigest: sha256:schema
  requiredFeatures: [deny-all-egress, immutable-repository-upload, canonical-patch-output, codex-app-server-v1]
  binaryDigest: sha256:binary
  qualification:
    schemaVersion: 1
    adapterId: codex-app-server-v1
    adapterVersion: 0.153.2
    status: qualified
    evaluatedDimensions: [protocol-lifecycle, approval-mediation, streaming-events, usage-accounting, cancellation, resumability, canonical-patch, review-isolation, authentication-profiles, workspace-isolation, panic-containment, dependency-compatibility, license-compatibility]
    contractDigest: sha256:contract
    evidenceDigest: sha256:evidence
  model: coding-implementer
  reviewModel: coding-reviewer
  authenticationProfile: personal-subscription
"#,
        )
        .unwrap();
        let coding = coding.value.unwrap();
        assert_eq!(coding.model, "coding-implementer");
        assert_eq!(coding.review_model, "coding-reviewer");
        assert_eq!(
            coding.authentication_profile,
            coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription
        );

        let missing_catalog: CatalogValue = serde_yaml::from_str("{}").unwrap();
        assert!(missing_catalog.value.is_none());
        let catalog: CatalogValue = serde_yaml::from_str("value: {}").unwrap();
        assert_eq!(catalog.value, Some(serde_json::json!({})));
        assert_eq!(
            serde_json::to_value(catalog).unwrap(),
            serde_json::json!({"value": {}})
        );

        let filters: FiltersValue =
            serde_yaml::from_str("value: {languages: [en], sourceIds: []}").unwrap();
        let filters = filters.value.unwrap();
        assert_eq!(filters.languages, ["en"]);
        assert!(filters.source_ids.is_empty());

        let quoted_filters: FiltersValue =
            serde_yaml::from_str("value: '{languages: [en], sourceIds: []}'").unwrap();
        assert_eq!(quoted_filters.value.unwrap().languages, ["en"]);

        let quoted_null: FiltersValue = serde_yaml::from_str("value: 'null'").unwrap();
        assert!(quoted_null.value.is_none());

        assert!(serde_yaml::from_str::<CodingProfileValue>("value: []").is_err());
        assert!(serde_yaml::from_str::<CodingProfileValue>("value: enabled").is_err());
        assert!(
            serde_yaml::from_str::<CodingProfileValue>("value: {model: coding-model}").is_err()
        );
    }
}
