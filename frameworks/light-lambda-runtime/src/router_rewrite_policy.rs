//! Explicit source-selected qualification profile. Default registration stays
//! forwarding-only; no configuration/environment executable escape is added.
use crate::{
    http_router::{ControllerDispatchFailure, HttpRouterBackend},
    init::{HandlerFactoryProvider, RuntimeLifecycle},
    init_error::InitError,
    router_java_worker::{
        Artifacts, Candidate, Failure, QualificationLimits,
        recovery::{RecoveryController, RecoveryFailure, RecoveryPolicy},
    },
    router_policy::{
        CaptureLimits, CapturedRouterSources, PreparedRouterResources, PreparedRouterRuntime,
        RouterPolicyProvider,
    },
    router_program::{LoadingLimits, Program},
};
use async_trait::async_trait;
pub use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{plan::HandlerFactory, *};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Typed boundary for lifecycle-only non-child tests. Tests must not synthesize
/// successful Candidates or claim Java semantics from scripted observations.
#[async_trait]
pub trait Rewriter: Send + Sync {
    #[must_use = "the rewrite future must be awaited to perform the operation"]
    async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure>;
}
#[async_trait]
pub trait Controller: Rewriter {
    #[must_use = "the initialize future must be awaited to perform the operation"]
    async fn initialize(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), RecoveryFailure>;
    #[must_use = "the shutdown future must be awaited to perform the operation"]
    async fn shutdown(&self);
}
#[async_trait]
impl Rewriter for RecoveryController {
    async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure> {
        RecoveryController::rewrite(self, path, deadline, cancel).await
    }
}
pub trait ControllerFactory: Send + Sync {
    /// Must be pure with respect to processes; startup belongs to activate.
    fn create(&self, program: Arc<Program>) -> Result<Arc<dyn Controller>, InitError>;
}

/// Explicit experimental worker controls; this constructor admits no production
/// defaults and is never called by ordinary application registration.
pub struct JavaControllerFactory {
    pub artifacts: Arc<Artifacts>,
    pub worker_limits: QualificationLimits,
    pub worker_slots: u32,
    pub recovery: RecoveryPolicy,
}
struct JavaController {
    controller: RecoveryController,
    slots: u32,
}
impl ControllerFactory for JavaControllerFactory {
    fn create(&self, program: Arc<Program>) -> Result<Arc<dyn Controller>, InitError> {
        if self.worker_slots == 0
            || self.worker_slots as usize > tokio::sync::Semaphore::MAX_PERMITS
        {
            return Err(invalid());
        }
        let controller = RecoveryController::new(
            self.artifacts.clone(),
            program.rules(),
            self.worker_limits.clone(),
            Arc::new(tokio::sync::Semaphore::new(self.worker_slots as usize)),
            RecoveryPolicy {
                attempts: self.recovery.attempts,
                cooldown: self.recovery.cooldown,
            },
        )
        .map_err(|_| invalid())?;
        Ok(Arc::new(JavaController {
            controller,
            slots: self.worker_slots,
        }))
    }
}
#[async_trait]
impl Controller for JavaController {
    async fn initialize(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), RecoveryFailure> {
        self.controller.initialize(deadline, cancel).await
    }
    async fn shutdown(&self) {
        self.controller.shutdown_owned(self.slots).await;
    }
}
#[async_trait]
impl Rewriter for JavaController {
    async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure> {
        self.controller.rewrite(path, deadline, cancel).await
    }
}

fn invalid() -> InitError {
    InitError::InvalidConfiguration("router: qualification initialization unavailable")
}
pub fn failure_status(failure: RecoveryFailure) -> StatusError {
    if let RecoveryFailure::Worker(failure) = failure
        && let Some(status) = failure.approved_status()
    {
        return status;
    }
    StatusError::new(
        503,
        "ERR14035",
        "ROUTER_REWRITE_UNAVAILABLE",
        "Router rewrite execution is unavailable.",
    )
}

pub struct QualificationRouterPolicyProvider {
    factory: Arc<dyn ControllerFactory>,
    init_timeout: Duration,
}
impl QualificationRouterPolicyProvider {
    pub fn new(
        factory: Arc<dyn ControllerFactory>,
        init_timeout: Duration,
    ) -> Result<Self, InitError> {
        if init_timeout.is_zero() {
            return Err(invalid());
        }
        Ok(Self {
            factory,
            init_timeout,
        })
    }
    /// Owner-approved qualification-only loading profile, not resource bounds.
    pub fn loading_limits() -> LoadingLimits {
        LoadingLimits {
            rules: 64,
            per_rule_bytes: 16 << 10,
            aggregate_bytes: 256 << 10,
            token_utf16: 8192,
        }
    }
}
struct Managed {
    controller: Arc<dyn Controller>,
    lifetime: CancellationToken,
    ready: AtomicBool,
    stopped: tokio::sync::OnceCell<()>,
    timeout: Duration,
}
impl Managed {
    async fn stop(&self) {
        self.ready.store(false, Ordering::SeqCst);
        self.lifetime.cancel();
        self.stopped
            .get_or_init(|| async { self.controller.shutdown().await })
            .await;
    }
}
#[async_trait]
impl RuntimeLifecycle for Managed {
    async fn activate(&self) -> Result<(), InitError> {
        let deadline = Instant::now()
            .checked_add(self.timeout)
            .ok_or_else(invalid)?;
        let result = tokio::time::timeout_at(
            deadline,
            self.controller
                .initialize(deadline, self.lifetime.child_token()),
        )
        .await;
        if !matches!(result, Ok(Ok(()))) || self.lifetime.is_cancelled() {
            self.stop().await;
            return Err(invalid());
        }
        self.ready.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn deactivate(&self) {
        self.stop().await;
    }
}
#[async_trait]
impl Rewriter for Managed {
    async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure> {
        if !self.ready.load(Ordering::SeqCst) {
            return Err(RecoveryFailure::NotInitialized);
        }
        let combined = self.lifetime.child_token();
        struct CancelOnDrop(CancellationToken);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _guard = CancelOnDrop(combined.clone());
        tokio::select! { biased;
            _=cancel.cancelled()=>Err(RecoveryFailure::Worker(Failure::Cancelled)),
            _=self.lifetime.cancelled()=>Err(RecoveryFailure::NotInitialized),
            result=self.controller.rewrite(path, deadline, combined)=>result,
        }
    }
}
struct BackendAdapter {
    http: Arc<HttpRouterBackend>,
    managed: Arc<Managed>,
}
#[async_trait]
impl Backend for BackendAdapter {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        match self
            .http
            .dispatch_with_rewriter(inv, state, target, self.managed.as_ref())
            .await
        {
            Ok(response) => Ok(response),
            Err(ControllerDispatchFailure::Transport(error)) => Err(error),
            Err(ControllerDispatchFailure::Rewrite(failure)) => {
                Err(DispatchError(failure_status(failure)))
            }
        }
    }
}
impl HandlerFactoryProvider for QualificationRouterPolicyProvider {
    fn application_available(&self) -> bool {
        false
    }
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Err(invalid())
    }
    fn prepare_router_runtime(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        running: bool,
        limits: &serverless_aws::limits::Limits,
    ) -> Result<PreparedRouterRuntime, InitError> {
        if !running {
            return Err(InitError::InvalidConfiguration(
                "router: qualification rewrite requires runtime Init; offline validation never starts a worker",
            ));
        }
        let sources = CapturedRouterSources::capture(
            config.embedded_config,
            config.default_config_dir.as_deref(),
            &config.config_dir,
            &config.external_config_dir,
            CaptureLimits {
                per_source: 1 << 20,
                aggregate: 4 << 20,
            },
        )?;
        let prepared =
            RouterPolicyProvider.prepare_rewrite_program(&sources, Self::loading_limits())?;
        let http = crate::router_policy::prepare_backend_from_resources(
            config,
            limits,
            &prepared.resources,
        )?;
        let managed = Arc::new(Managed {
            controller: self.factory.create(prepared.program)?,
            lifetime: CancellationToken::new(),
            ready: AtomicBool::new(false),
            stopped: tokio::sync::OnceCell::new(),
            timeout: self.init_timeout,
        });
        Ok(PreparedRouterRuntime {
            prepared: PreparedRouterResources {
                factory: prepared.factory,
                resources: Some(prepared.resources),
            },
            backend: Some(Arc::new(BackendAdapter {
                http,
                managed: managed.clone(),
            })),
            lifecycle: Some(managed),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approved_failure_matrix_is_static_and_preserves_existing_mappings() {
        for failure in [
            Failure::Artifact,
            Failure::Init,
            Failure::Resource,
            Failure::InputProfile,
            Failure::OutputProfile,
            Failure::ChildLost,
            Failure::Unclassified,
            Failure::NoReady,
        ] {
            let status = failure_status(RecoveryFailure::Worker(failure));
            assert_eq!(
                (
                    status.status_code,
                    status.code.as_str(),
                    status.message.as_str(),
                    status.description.as_str()
                ),
                (
                    503,
                    "ERR14035",
                    "ROUTER_REWRITE_UNAVAILABLE",
                    "Router rewrite execution is unavailable."
                )
            );
        }
        for failure in [
            RecoveryFailure::NotInitialized,
            RecoveryFailure::Cooldown,
            RecoveryFailure::Stale,
        ] {
            assert_eq!(failure_status(failure).code, "ERR14035");
        }
        for failure in [Failure::Deadline, Failure::Cancelled] {
            assert_eq!(
                failure_status(RecoveryFailure::Worker(failure)).code,
                "ERR14017"
            );
        }
        for failure in [
            Failure::InvalidUtf16,
            Failure::InvalidReplacement,
            Failure::Protocol,
            Failure::Panic,
        ] {
            assert_eq!(
                failure_status(RecoveryFailure::Worker(failure)).code,
                "ERR14004"
            );
        }
    }
    #[test]
    fn qualification_loading_limits_accept_exact_boundaries_and_reject_excess() {
        use crate::router_program::Failure as LoadingFailure;
        use serde_json::json;
        let load = |rules: Vec<String>| {
            Program::capture(
                &json!({"urlRewriteRules":rules}),
                QualificationRouterPolicyProvider::loading_limits(),
            )
        };
        assert!(load(vec!["^/x$ /x".into(); 64]).is_ok());
        assert!(matches!(
            load(vec!["^/x$ /x".into(); 65]),
            Err(LoadingFailure::Profile)
        ));
        assert!(load(vec![format!("{} /x", "a".repeat(8192))]).is_ok());
        assert!(matches!(
            load(vec![format!("{} /x", "a".repeat(8193))]),
            Err(LoadingFailure::Profile)
        ));
        let exact = format!("{} {}", "é".repeat(4096), "b".repeat(8191));
        assert_eq!(exact.len(), 16 << 10);
        assert!(load(vec![exact.clone(); 16]).is_ok());
        assert!(matches!(
            load(vec![exact.clone(); 17]),
            Err(LoadingFailure::Profile)
        ));
        assert!(matches!(
            load(vec![format!("{exact}b")]),
            Err(LoadingFailure::Profile)
        ));
    }
}
