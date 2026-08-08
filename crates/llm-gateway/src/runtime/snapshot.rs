use crate::config::{AliasCapabilityRequirements, AuditMode};
use crate::pii::PiiProfile;
use crate::routing::PassiveCircuit;
use crate::usage::{EmbeddingPrice, GenerationPrice, OperationPrice, UsageLedger};
use chrono::Utc;
use model_provider::conformance::{CapabilityRequirements, ConformanceResult, FixtureProvenance};
use model_provider::inference::{
    CompiledProvider, Operation, ProviderCapabilities, ProviderProtocol,
};
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct ProviderAccountRuntime {
    pub provider_account_id: String,
    pub quota_group_id: String,
    pub configured_concurrency: usize,
    pub permits: Arc<Semaphore>,
}

pub struct DeploymentRuntime {
    pub id: String,
    pub model: String,
    pub configured_concurrency: usize,
    pub provider: CompiledProvider,
    pub provider_digest: String,
    pub capabilities: ProviderCapabilities,
    pub conformance_result: Option<ConformanceResult>,
    pub required_conformance_provenance: Option<FixtureProvenance>,
    pub permits: Arc<Semaphore>,
    pub circuit: Arc<PassiveCircuit>,
    pub account: Arc<ProviderAccountRuntime>,
    pub prices: BTreeMap<Operation, OperationPrice>,
}

impl DeploymentRuntime {
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
    pub fn supports(&self, required: &CapabilityRequirements) -> bool {
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
    pub aliases: BTreeMap<String, Arc<AliasPlan>>,
    pub deployments: BTreeMap<String, Arc<DeploymentRuntime>>,
    pub principal_permits: Arc<PrincipalPermitStripes>,
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
