use crate::config::AuditMode;
use crate::routing::PassiveCircuit;
use crate::usage::{Price, UsageLedger};
use model_provider::inference::{InferenceProvider, ProviderCapabilities};
use std::collections::{BTreeMap, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub struct ProviderAccountRuntime {
    pub provider_account_id: String,
    pub quota_group_id: String,
}

pub struct DeploymentRuntime {
    pub id: String,
    pub model: String,
    pub provider: Arc<dyn InferenceProvider>,
    pub provider_digest: String,
    pub capabilities: ProviderCapabilities,
    pub permits: Arc<Semaphore>,
    pub circuit: Arc<PassiveCircuit>,
    pub account: Arc<ProviderAccountRuntime>,
    pub price: Price,
}

impl DeploymentRuntime {
    pub fn supports(&self, required: (bool, bool, bool)) -> bool {
        (!required.0 || self.capabilities.content.images)
            && (!required.1 || self.capabilities.content.tools)
            && (!required.2 || self.capabilities.content.structured_json)
    }
}

pub struct AliasPlan {
    pub public_name: String,
    pub deployments: Vec<Arc<DeploymentRuntime>>,
    pub max_attempts: usize,
    pub permits: Arc<Semaphore>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub max_cost_micros: Option<u64>,
    pub internal: bool,
    pub bound_principal: Option<String>,
    pub audit: AuditMode,
    pub ledger: Arc<UsageLedger>,
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
    pub digest: String,
    pub global_concurrency: usize,
    pub max_replay_bytes: usize,
    pub aliases: BTreeMap<String, Arc<AliasPlan>>,
    pub deployments: BTreeMap<String, Arc<DeploymentRuntime>>,
    pub principal_permits: PrincipalPermitStripes,
}
