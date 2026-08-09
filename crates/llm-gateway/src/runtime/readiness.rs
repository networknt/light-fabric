use super::store::LlmSnapshotStore;
use model_provider::inference::{EmbeddingRequest, InferenceRequest, ProviderRequestContext};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DeploymentReadinessState {
    Unqualified = 0,
    Warming = 1,
    Ready = 2,
    NotReady = 3,
    Failed = 4,
}

/// Request-path readers observe one atomic value. Health checks, warmup and
/// model-load calls are deliberately owned by an off-path controller.
#[derive(Debug)]
pub struct DeploymentReadiness {
    state: AtomicU8,
    probe_in_flight: AtomicBool,
}

impl DeploymentReadiness {
    pub fn new(state: DeploymentReadinessState) -> Self {
        Self {
            state: AtomicU8::new(state as u8),
            probe_in_flight: AtomicBool::new(false),
        }
    }

    pub fn state(&self) -> DeploymentReadinessState {
        match self.state.load(Ordering::Acquire) {
            1 => DeploymentReadinessState::Warming,
            2 => DeploymentReadinessState::Ready,
            3 => DeploymentReadinessState::NotReady,
            4 => DeploymentReadinessState::Failed,
            _ => DeploymentReadinessState::Unqualified,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.state() == DeploymentReadinessState::Ready
    }

    pub fn begin_warmup(&self) -> bool {
        self.state
            .compare_exchange(
                DeploymentReadinessState::Unqualified as u8,
                DeploymentReadinessState::Warming as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .or_else(|current| {
                if current == DeploymentReadinessState::NotReady as u8 {
                    self.state.compare_exchange(
                        current,
                        DeploymentReadinessState::Warming as u8,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                } else {
                    Err(current)
                }
            })
            .is_ok()
    }

    pub fn warmup_succeeded(&self) -> bool {
        self.state
            .compare_exchange(
                DeploymentReadinessState::Warming as u8,
                DeploymentReadinessState::Ready as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    pub fn mark_not_ready(&self) {
        self.state
            .store(DeploymentReadinessState::NotReady as u8, Ordering::Release);
    }

    pub fn warmup_failed(&self) {
        self.state
            .store(DeploymentReadinessState::NotReady as u8, Ordering::Release);
    }

    pub fn security_failed(&self) {
        self.state
            .store(DeploymentReadinessState::Failed as u8, Ordering::Release);
    }

    pub fn try_begin_probe(&self) -> bool {
        self.probe_in_flight
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub fn finish_probe(&self) {
        self.probe_in_flight.store(false, Ordering::Release);
    }
}

pub struct ReadinessControllerTask {
    handle: tokio::task::JoinHandle<()>,
}

impl ReadinessControllerTask {
    pub fn stop(&self) {
        self.handle.abort();
    }
}

impl Drop for ReadinessControllerTask {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn start_readiness_controller(
    store: Arc<LlmSnapshotStore>,
    interval: Duration,
) -> Arc<ReadinessControllerTask> {
    let handle = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(250)));
        let ready_probe_interval = Duration::from_secs(30);
        let mut last_ready_probe = BTreeMap::<String, Instant>::new();
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let snapshot = store.load();
            for deployment in snapshot.deployments.values() {
                if deployment.readiness_policy != crate::config::ReadinessPolicy::WarmBeforeEligible
                {
                    continue;
                }
                let was_ready = deployment.readiness.is_ready();
                if was_ready {
                    let due = last_ready_probe
                        .get(&deployment.id)
                        .is_none_or(|last| last.elapsed() >= ready_probe_interval);
                    if !due || !deployment.readiness.try_begin_probe() {
                        continue;
                    }
                    last_ready_probe.insert(deployment.id.clone(), Instant::now());
                } else {
                    // Reserve the single-flight slot before changing state. If
                    // another probe is still publishing its result, leave that
                    // result untouched and retry on the next controller tick.
                    if !deployment.readiness.try_begin_probe() {
                        continue;
                    }
                    if !deployment.readiness.begin_warmup() {
                        deployment.readiness.finish_probe();
                        continue;
                    }
                }
                let deployment = Arc::clone(deployment);
                tokio::spawn(async move {
                    // Warmup traffic consumes the same signed deployment and
                    // account capacity as user traffic. A busy runtime is not
                    // probed beyond its qualified concurrency envelope.
                    let Ok(_account_permit) =
                        Arc::clone(&deployment.account.permits).try_acquire_owned()
                    else {
                        if !was_ready {
                            deployment.readiness.mark_not_ready();
                        }
                        deployment.readiness.finish_probe();
                        return;
                    };
                    let Ok(_deployment_permit) =
                        Arc::clone(&deployment.permits).try_acquire_owned()
                    else {
                        if !was_ready {
                            deployment.readiness.mark_not_ready();
                        }
                        deployment.readiness.finish_probe();
                        return;
                    };
                    let timeout = Duration::from_millis(deployment.cold_start_timeout_ms.max(1));
                    let context = ProviderRequestContext::with_timeout(
                        format!("warmup:{}", deployment.id),
                        timeout,
                    );
                    let warmup = match &deployment.provider {
                        model_provider::inference::CompiledProvider::Generation(provider) => {
                            tokio::time::timeout(
                                timeout,
                                provider.generate(
                                    context,
                                    InferenceRequest::text(&deployment.model, "warmup"),
                                ),
                            )
                            .await
                            .map_err(|_| {
                                model_provider::inference::InferenceError::timeout_before_acceptance()
                            })
                            .and_then(|result| result.map(|_| ()))
                        }
                        model_provider::inference::CompiledProvider::Embedding(provider) => {
                            tokio::time::timeout(
                                timeout,
                                provider.embed(
                                    context,
                                    EmbeddingRequest {
                                        model: deployment.model.clone(),
                                        inputs: vec!["warmup".to_string()],
                                        dimensions: None,
                                    },
                                ),
                            )
                            .await
                            .map_err(|_| {
                                model_provider::inference::InferenceError::timeout_before_acceptance()
                            })
                            .and_then(|result| result.map(|_| ()))
                        }
                    };
                    if warmup.is_ok() {
                        if !was_ready {
                            deployment.readiness.warmup_succeeded();
                        }
                    } else if warmup.as_ref().is_err_and(|error| {
                        matches!(
                            error.category,
                            model_provider::inference::InferenceErrorCategory::SecurityInvariant
                                | model_provider::inference::InferenceErrorCategory::PermissionDenied
                        )
                    }) {
                        deployment.readiness.security_failed();
                    } else {
                        deployment.readiness.mark_not_ready();
                    }
                    deployment.readiness.finish_probe();
                });
            }
        }
    });
    Arc::new(ReadinessControllerTask { handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warm_before_eligible_never_admits_traffic_early() {
        let readiness = DeploymentReadiness::new(DeploymentReadinessState::Unqualified);
        assert!(!readiness.is_ready());
        assert!(readiness.begin_warmup());
        assert!(!readiness.is_ready());
        assert!(readiness.warmup_succeeded());
        assert!(readiness.is_ready());
        readiness.mark_not_ready();
        assert!(!readiness.is_ready());
        assert!(readiness.begin_warmup());
    }

    #[test]
    fn readiness_probes_are_single_flight_and_failure_removes_eligibility() {
        let readiness = DeploymentReadiness::new(DeploymentReadinessState::Ready);
        assert!(readiness.try_begin_probe());
        assert!(!readiness.try_begin_probe());
        readiness.mark_not_ready();
        readiness.finish_probe();
        assert!(!readiness.is_ready());
        assert!(readiness.try_begin_probe());
    }

    #[test]
    fn an_in_flight_probe_cannot_strand_not_ready_as_warming() {
        let readiness = DeploymentReadiness::new(DeploymentReadinessState::Ready);
        assert!(readiness.try_begin_probe());
        readiness.mark_not_ready();

        // The controller now acquires the probe slot first, so it cannot move
        // NotReady to Warming while the prior probe still owns that slot.
        assert!(!readiness.try_begin_probe());
        assert_eq!(readiness.state(), DeploymentReadinessState::NotReady);

        readiness.finish_probe();
        assert!(readiness.try_begin_probe());
        assert!(readiness.begin_warmup());
        readiness.finish_probe();
        assert_eq!(readiness.state(), DeploymentReadinessState::Warming);
    }
}
