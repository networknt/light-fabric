//! Explicit trusted deployment-profile composition; no default selector or runner.
//! Construction neither opens a worker nor initializes the precision engine.
use light_lambda_runtime::{
    bundle_snapshot::Pin,
    init::{InitInput, PreparationGate},
    init_error::InitError,
    required_validator_profile::RequiredValidatorProfile,
    validator_failure_policy::ValidatorFailureDisposition,
    validator_ipc::{
        Failure, SupervisedStaticEngine,
        accounted_profile::{AccountedCatalogueEngine, Limits},
        bundle_admission::{CatalogueEngine, CataloguePin},
        ownership::{Drain, Snapshot},
        supervision::Policy,
    },
    validator_policy::ValidatorPolicyProvider,
};
use serverless_core::HandlerId;
use std::{path::Component, sync::Arc, time::Duration};
use tokio::time::Instant;

const PROFILE_ERROR: &str = "validator: trusted deployment profile unavailable";
fn invalid() -> InitError {
    InitError::InvalidConfiguration(PROFILE_ERROR)
}

/// Expected identities come from reviewed startup code, never runtime config.
/// No Default/Deserialize, executable fallback, numeric defaults or activation.
#[must_use]
pub struct DeploymentProfile {
    diagnostic: Option<Arc<light_lambda_runtime::validator_init_diagnostic::InitDiagnostic>>,
    engine: Arc<AccountedCatalogueEngine>,
    supervisor: Arc<SupervisedStaticEngine>,
    required: Arc<RequiredValidatorProfile>,
    timeout: Duration,
    observation: Option<ProfileObservation>,
}
/// Source-selected qualification observation; never a production trust token.
pub struct ProfileObservation {
    pub lifecycle: Arc<light_lambda_runtime::validator_ipc::lifecycle::Lifecycle>,
    pub drain_receipt: std::path::PathBuf,
}

impl DeploymentProfile {
    /// Trusted qualification selection only. Does not open or execute the image.
    #[allow(clippy::too_many_arguments)]
    pub fn new_readonly_observed(
        worker: Pin,
        catalogue: CataloguePin,
        required: RequiredValidatorProfile,
        policy: Policy,
        limits: Option<Limits>,
        timeout: Duration,
        observation: ProfileObservation,
        readonly: light_lambda_runtime::validator_ipc::ReadonlyProfile,
    ) -> Result<Self, InitError> {
        readonly.validate_pin(&worker).map_err(|_| invalid())?;
        Self::build_selected(
            worker,
            catalogue,
            required,
            policy,
            limits,
            timeout,
            Some(observation),
            Some(readonly),
        )
    }
    pub fn new(
        worker: Pin,
        catalogue: CataloguePin,
        required: RequiredValidatorProfile,
        policy: Policy,
        limits: Option<Limits>,
        timeout: Duration,
    ) -> Result<Self, InitError> {
        Self::build(worker, catalogue, required, policy, limits, timeout, None)
    }
    pub fn new_observed(
        worker: Pin,
        catalogue: CataloguePin,
        required: RequiredValidatorProfile,
        policy: Policy,
        limits: Option<Limits>,
        timeout: Duration,
        observation: ProfileObservation,
    ) -> Result<Self, InitError> {
        Self::build(
            worker,
            catalogue,
            required,
            policy,
            limits,
            timeout,
            Some(observation),
        )
    }
    fn build(
        worker: Pin,
        catalogue: CataloguePin,
        required: RequiredValidatorProfile,
        policy: Policy,
        limits: Option<Limits>,
        timeout: Duration,
        observation: Option<ProfileObservation>,
    ) -> Result<Self, InitError> {
        Self::build_selected(
            worker,
            catalogue,
            required,
            policy,
            limits,
            timeout,
            observation,
            None,
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn build_selected(
        worker: Pin,
        catalogue: CataloguePin,
        required: RequiredValidatorProfile,
        policy: Policy,
        limits: Option<Limits>,
        timeout: Duration,
        observation: Option<ProfileObservation>,
        readonly: Option<light_lambda_runtime::validator_ipc::ReadonlyProfile>,
    ) -> Result<Self, InitError> {
        if !worker.path.is_absolute()
            || worker
                .path
                .components()
                .any(|p| matches!(p, Component::ParentDir))
            || worker.bytes == 0
            || worker.sha256 == [0; 32]
            || catalogue.source_bytes == 0
            || catalogue.source_sha256 == [0; 32]
            || required.handler_sha256 == [0; 32]
            || timeout.is_zero()
            || Instant::now().checked_add(timeout).is_none()
            || !required.active_handlers.contains(&HandlerId::Specification)
            || !required.active_handlers.contains(&HandlerId::Validator)
            || required.active_handlers.contains(&HandlerId::Proxy)
            || required
                .active_handlers
                .iter()
                .enumerate()
                .any(|(i, id)| required.active_handlers[..i].contains(id))
        {
            return Err(invalid());
        }
        let limits = limits.ok_or_else(invalid)?;
        if catalogue.source_bytes > limits.max_source_bytes {
            return Err(invalid());
        }
        let supervisor = Arc::new(SupervisedStaticEngine {
            executable: worker,
            policy,
        });
        let catalogue = Arc::new(CatalogueEngine {
            engine: supervisor.clone(),
            catalogue,
        });
        let engine=match (readonly, &observation) {
            (Some(profile), Some(observed)) => AccountedCatalogueEngine::new_readonly_observed(
                Arc::new(light_lambda_runtime::validator_ipc::bundle_admission::ReadonlyCatalogueEngine::new_observed(
                    catalogue, profile, observed.lifecycle.clone()).map_err(|_| invalid())?), Some(limits)),
            (Some(_), None) => return Err(invalid()),
            (None, Some(observed))=>AccountedCatalogueEngine::new_observed(
                Arc::new(light_lambda_runtime::validator_ipc::lifecycle::CatalogueObservedProfileEngine {
                    engine:catalogue,lifecycle:observed.lifecycle.clone(),
                }),Some(limits)),
            (None, None)=>AccountedCatalogueEngine::new(catalogue,Some(limits)),
        }.map_err(|_|invalid())?;
        Ok(Self {
            diagnostic: None,
            engine: Arc::new(engine),
            supervisor,
            required: Arc::new(required),
            timeout,
            observation,
        })
    }

    /// Source-selected observation only; never enabled by configuration.
    pub fn with_init_diagnostic(
        mut self,
        diagnostic: Arc<light_lambda_runtime::validator_init_diagnostic::InitDiagnostic>,
    ) -> Self {
        self.diagnostic = Some(diagnostic);
        self
    }
    /// Attach only to an explicitly supplied InitInput. Actual captured source,
    /// settings and active chain admission remain the existing Init/provider gate.
    /// This function does not initialize or prove the profile is deployable.
    pub fn attach(self, mut input: InitInput) -> Result<ConfiguredProfile, InitError> {
        for id in [HandlerId::Specification, HandlerId::Validator] {
            if input.handlers.iter().filter(|r| r.id == id).count() != 1 {
                return Err(invalid());
            }
        }
        let provider = ValidatorPolicyProvider::with_engine(
            self.engine,
            Arc::new(ValidatorFailureDisposition),
            self.timeout,
        )
        .with_required_profile(self.required);
        let provider = Arc::new(match self.diagnostic {
            Some(diagnostic) => provider.with_init_diagnostic(diagnostic),
            None => provider,
        });
        for registration in &mut input.handlers {
            if matches!(
                registration.id,
                HandlerId::Specification | HandlerId::Validator
            ) {
                registration.factory = Some(provider.clone());
            }
        }
        Ok(ConfiguredProfile {
            input,
            shutdown: ProfileShutdown {
                supervisor: self.supervisor,
                observation: self.observation,
            },
        })
    }
}

/// Explicit composition output; no Run method or ordinary-bootstrap adoption.
#[must_use]
pub struct ConfiguredProfile {
    input: InitInput,
    shutdown: ProfileShutdown,
}
impl ConfiguredProfile {
    /// The future lifecycle runner must retain the shutdown handle through Init
    /// failure and service completion. Dropping it is NOT cleanup/reap evidence.
    pub fn into_parts(self) -> (InitInput, ProfileShutdown) {
        (self.input, self.shutdown)
    }
}

/// Existing process-wide ownership, not a new independent scope or Drop cleanup.
/// No production runner is introduced until its lifecycle contract is qualified.
#[must_use]
pub struct ProfileShutdown {
    supervisor: Arc<SupervisedStaticEngine>,
    observation: Option<ProfileObservation>,
}
impl ProfileShutdown {
    pub(crate) fn request_completion(&self, kind: light_lambda_runtime::owned_runtime::ExitKind) {
        use light_lambda_runtime::{
            owned_runtime::ExitKind, validator_ipc::lifecycle::CompletionCause,
        };
        if let Some(observed) = &self.observation {
            observed.lifecycle.request_shutdown(match kind {
                ExitKind::Completed | ExitKind::ShutdownRequested => CompletionCause::Success,
                _ => CompletionCause::RuntimeTerminated,
            });
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        Drain::process().snapshot()
    }
    pub async fn shutdown(&self, deadline: Instant) -> Result<Snapshot, Failure> {
        // Attempt both existing drains under the SAME deadline even if the first
        // fails. Never report cleanup success without both outcomes succeeding.
        let precision = self.supervisor.shutdown(deadline).await;
        let preparation = PreparationGate::process().settled(deadline).await;
        let snapshot = precision?;
        preparation.map_err(|_| Failure::AdapterInvariant)?;
        if let Some(observed) = &self.observation {
            let state = observed
                .lifecycle
                .wait_idle(deadline)
                .await
                .map_err(|_| Failure::AdapterInvariant)?;
            if !state.clean() {
                return Err(Failure::AdapterInvariant);
            }
            let evidence = serde_json::json!({"evidenceOrigin":"qualificationEntryOwner",
                "lifecycleClean":state.clean(),"observedChildren":state.children.len(),
                "precisionDrained":true,"preparationSettled":true,"resourceBoundsProved":false});
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&observed.drain_receipt)
                .map_err(|_| Failure::AdapterInvariant)?;
            serde_json::to_writer(file, &evidence).map_err(|_| Failure::AdapterInvariant)?;
        }

        Ok(snapshot)
    }
}
