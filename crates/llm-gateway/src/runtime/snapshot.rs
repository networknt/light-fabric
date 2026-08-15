use super::readiness::DeploymentReadiness;
use crate::audit::AuditTransportContext;
use crate::config::{
    AliasCapabilityRequirements, AuditMode, BedrockDeploymentPolicy, EmbeddingWorkloadLane,
    ReadinessPolicy, ReasoningMode, SamplingParameterPolicy,
};
use crate::error::LlmGatewayError;
use crate::pii::PiiProfile;
use crate::reasoning_seal::ReasoningSealer;
use crate::routing::{OwnedCircuitPermit, PassiveCircuit};
use crate::usage::{EmbeddingPrice, GenerationPrice, OperationPrice, UsageLedger};
use chrono::Utc;
use model_provider::conformance::{CapabilityRequirements, ConformanceResult, FixtureProvenance};
use model_provider::inference::{
    CompiledProvider, Operation, ProviderCapabilities, ProviderProtocol,
};
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

pub struct ProviderAccountRuntime {
    pub provider_account_id: String,
    pub quota_group_id: String,
    pub configured_concurrency: usize,
    pub permits: Arc<Semaphore>,
}

pub struct DeploymentRuntime {
    pub id: String,
    pub provider_endpoint_id: String,
    pub model: String,
    pub configured_concurrency: usize,
    pub provider: CompiledProvider,
    pub provider_digest: String,
    pub provider_client_generation: u64,
    pub provider_client_built_at: Instant,
    pub audit_transport: AuditTransportContext,
    pub capabilities: ProviderCapabilities,
    pub conformance_result: Option<ConformanceResult>,
    pub required_conformance_provenance: Option<FixtureProvenance>,
    pub readiness_policy: ReadinessPolicy,
    pub readiness: Arc<DeploymentReadiness>,
    pub cold_start_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub stream_setup_timeout_ms: u64,
    pub permits: Arc<Semaphore>,
    pub circuit: Arc<PassiveCircuit>,
    pub account: Arc<ProviderAccountRuntime>,
    pub prices: BTreeMap<Operation, OperationPrice>,
    pub bedrock_policy: Option<BedrockDeploymentPolicy>,
}

impl DeploymentRuntime {
    pub fn prepare_generate_request(
        &self,
        request: &mut model_provider::inference::InferenceRequest,
        client_protocol: model_provider::inference::ClientProtocol,
    ) -> Result<(), LlmGatewayError> {
        if self.provider.protocol() != ProviderProtocol::BedrockConverse {
            return Ok(());
        }
        let policy = self.bedrock_policy.as_ref().ok_or_else(|| {
            LlmGatewayError::UnsupportedCapability(
                "Bedrock deployment has no typed request policy".to_string(),
            )
        })?;
        validate_sampling_parameter(
            "temperature",
            request.sampling.temperature,
            &policy.sampling.temperature,
        )?;
        validate_sampling_parameter("top_p", request.sampling.top_p, &policy.sampling.top_p)?;
        if request.sampling.temperature.is_some()
            && request.sampling.top_p.is_some()
            && !policy.sampling.allow_temperature_and_top_p
        {
            return Err(LlmGatewayError::UnsupportedCapability(
                "selected deployment does not allow temperature and top_p together".to_string(),
            ));
        }
        match policy.reasoning.mode {
            ReasoningMode::Unsupported
                if request.reasoning.is_some() || request.provider_continuation.is_some() =>
            {
                return Err(LlmGatewayError::UnsupportedCapability(
                    "selected deployment does not support reasoning continuation".to_string(),
                ));
            }
            ReasoningMode::AlwaysOnAdaptive
                if client_protocol == model_provider::inference::ClientProtocol::OpenAiChat =>
            {
                return Err(LlmGatewayError::UnsupportedCapability(
                    "always-on reasoning is not available through Chat Completions".to_string(),
                ));
            }
            ReasoningMode::AlwaysOnAdaptive if request.reasoning.is_none() => {
                request.reasoning = Some(model_provider::inference::ReasoningOptions::default());
            }
            _ => {}
        }
        if let Some(effort) = request
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.effort.as_ref())
            && !policy.reasoning.supported_efforts.contains(effort)
        {
            return Err(LlmGatewayError::UnsupportedCapability(format!(
                "reasoning effort `{effort}` is not supported by the selected deployment"
            )));
        }
        Ok(())
    }

    pub fn readiness_policy(&self) -> ReadinessPolicy {
        self.readiness_policy
    }

    pub fn generation_price(&self) -> Option<GenerationPrice> {
        match self.prices.get(&Operation::Generate) {
            Some(OperationPrice::Generate(price)) => Some(*price),
            _ => None,
        }
    }
    pub fn embedding_price(&self) -> Option<EmbeddingPrice> {
        match self.prices.get(&Operation::Embed) {
            Some(OperationPrice::Embed(price)) => Some(*price),
            _ => None,
        }
    }

    /// Rechecks the selection-time readiness decision and atomically acquires
    /// the dispatch-time circuit permit. Callers must not count an attempt
    /// until this permit and the required capacity permits have been acquired.
    pub fn acquire_dispatch_health(
        self: &Arc<Self>,
        now: Instant,
    ) -> Result<OwnedCircuitPermit, LlmGatewayError> {
        if !self.readiness.is_ready() {
            return Err(LlmGatewayError::NoReadyDeployment);
        }
        self.circuit.acquire_owned(now)
    }

    pub fn supports(&self, required: &CapabilityRequirements) -> bool {
        if !self.readiness.is_ready() {
            return false;
        }
        let mut required = required.clone();
        required.required_provenance = self.required_conformance_provenance;
        if let Some(result) = &self.conformance_result {
            return result.satisfies(&required, Utc::now());
        }
        let generation = self.capabilities.generation.as_ref();
        self.required_conformance_provenance.is_none()
            && self.capabilities.supports(required.operation)
            && (required.operation != Operation::Generate || generation.is_some())
            && (!required.images || generation.is_some_and(|value| value.content.images))
            && (!required.tools || generation.is_some_and(|value| value.content.tools))
            && (!required.parallel_tools
                || generation.is_some_and(|value| value.content.parallel_tools))
            && (!required.structured_json
                || generation.is_some_and(|value| value.content.structured_json))
            && (!required.reasoning
                || (self.provider.protocol() == ProviderProtocol::OpenAiResponses
                    && generation.is_some_and(|value| value.content.reasoning_usage)))
            && (!required.streaming || generation.is_some_and(|value| value.streaming))
    }

    pub fn supports_static(&self, required: &CapabilityRequirements) -> bool {
        let generation = self.capabilities.generation.as_ref();
        self.capabilities.supports(required.operation)
            && self.prices.contains_key(&required.operation)
            && self.provider.operation() == required.operation
            && (!required.images || generation.is_some_and(|value| value.content.images))
            && (!required.tools || generation.is_some_and(|value| value.content.tools))
            && (!required.parallel_tools
                || generation.is_some_and(|value| value.content.parallel_tools))
            && (!required.structured_json
                || generation.is_some_and(|value| value.content.structured_json))
            && (!required.reasoning
                || (self.provider.protocol() == ProviderProtocol::OpenAiResponses
                    && generation.is_some_and(|value| value.content.reasoning_usage)))
            && (!required.streaming || generation.is_some_and(|value| value.streaming))
    }
}

fn validate_sampling_parameter(
    name: &str,
    supplied: Option<f64>,
    policy: &SamplingParameterPolicy,
) -> Result<(), LlmGatewayError> {
    let Some(value) = supplied else {
        return Ok(());
    };
    let valid = match policy {
        SamplingParameterPolicy::Unsupported => false,
        SamplingParameterPolicy::Range { minimum, maximum } => {
            value.is_finite() && value >= *minimum && value <= *maximum
        }
        SamplingParameterPolicy::Fixed { value: fixed } => {
            value.is_finite() && (value - fixed).abs() <= f64::EPSILON
        }
    };
    if valid {
        Ok(())
    } else {
        Err(LlmGatewayError::UnsupportedCapability(format!(
            "{name} is not accepted by the selected deployment"
        )))
    }
}

pub struct AliasPlan {
    pub public_name: String,
    pub deployments: Vec<Arc<DeploymentRuntime>>,
    pub operations: std::collections::BTreeSet<Operation>,
    pub max_attempts: usize,
    pub configured_concurrency: usize,
    pub permits: Arc<Semaphore>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
    pub internal: bool,
    pub bound_principal: Option<String>,
    pub audit: AuditMode,
    pub pii: PiiProfile,
    pub required_capabilities: AliasCapabilityRequirements,
    pub require_expected_embedding_space: bool,
    pub embedding_workload_lane: EmbeddingWorkloadLane,
    pub ledger: Arc<UsageLedger>,
}

impl AliasPlan {
    pub fn merge_requirements(
        &self,
        mut required: CapabilityRequirements,
    ) -> CapabilityRequirements {
        required.images |= self.required_capabilities.images;
        required.tools |= self.required_capabilities.tools;
        required.parallel_tools |= self.required_capabilities.parallel_tools;
        required.structured_json |= self.required_capabilities.structured_json;
        required.streaming |= self.required_capabilities.streaming;
        required
    }
}

pub struct PrincipalPermitStripes {
    stripes: Vec<Arc<Semaphore>>,
}

impl PrincipalPermitStripes {
    pub fn new(stripes: usize, permits_per_stripe: usize) -> Self {
        Self {
            stripes: (0..stripes.max(1))
                .map(|_| Arc::new(Semaphore::new(permits_per_stripe.max(1))))
                .collect(),
        }
    }

    pub fn permits_for(&self, principal: &str) -> Arc<Semaphore> {
        let mut hash = DefaultHasher::new();
        principal.hash(&mut hash);
        Arc::clone(&self.stripes[hash.finish() as usize % self.stripes.len()])
    }
}

pub struct LlmPublishedSnapshot {
    pub generation: u64,
    /// Public, secret-free publication digest.
    pub digest: String,
    pub global_concurrency: usize,
    pub global_stream_concurrency: usize,
    pub stream_channel_capacity: usize,
    pub max_stream_response_bytes: usize,
    pub stream_write_timeout_ms: u64,
    pub stream_setup_timeout_ms: u64,
    pub stream_idle_timeout_ms: u64,
    pub stream_minimum_drain_bytes_per_second: u64,
    pub stream_drain_grace_ms: u64,
    pub max_replay_bytes: usize,
    pub embedding_memory: EmbeddingMemoryBounds,
    pub embedding_memory_permits: Arc<Semaphore>,
    pub embedding_workload_lane: EmbeddingWorkloadLane,
    pub aliases: BTreeMap<String, Arc<AliasPlan>>,
    pub deployments: BTreeMap<String, Arc<DeploymentRuntime>>,
    pub principal_permits: Arc<PrincipalPermitStripes>,
    pub reasoning_sealer: Arc<ReasoningSealer>,
    pub reasoning_key_set_generation: u64,
    pub reasoning_key_set_digest: String,
    pub anthropic_messages_enabled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbeddingMemoryBounds {
    pub admission_slots: usize,
    pub per_slot_peak_bytes: usize,
    pub aggregate_peak_bytes: usize,
    pub max_memory_bytes: usize,
    pub max_request_body_bytes: usize,
    pub max_replay_bytes: usize,
    pub max_replay_resident_bytes: usize,
    pub max_canonical_vector_bytes: usize,
    pub max_rendered_response_bytes: usize,
    pub overlapping_provider_response_bytes: usize,
    pub ingress_concurrency: usize,
    pub max_ingress_resident_bytes: usize,
    pub aggregate_ingress_bytes: usize,
    pub max_ingress_memory_bytes: usize,
    pub items_per_permit: usize,
    pub write_timeout_ms: u64,
    pub minimum_drain_bytes_per_second: u64,
    pub max_input_bytes_per_item: usize,
    pub max_total_input_bytes: usize,
    pub body_read_timeout_ms: u64,
    pub minimum_receive_bytes_per_second: u64,
    pub authorization_timeout_ms: u64,
}
