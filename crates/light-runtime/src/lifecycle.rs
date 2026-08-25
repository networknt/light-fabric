use async_trait::async_trait;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{RuntimeConfig, RuntimeError};

pub const MANDATORY_CLEANUP_FLOOR: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Interrupt,
    Terminate,
    Programmatic,
}

impl ShutdownReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Interrupt => "interrupt",
            Self::Terminate => "terminate",
            Self::Programmatic => "programmatic",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownMode {
    Graceful,
    StartupAbort,
    Emergency,
}

#[derive(Clone)]
pub struct ShutdownContext {
    pub reason: ShutdownReason,
    pub mode: ShutdownMode,
    pub deadline: Instant,
    pub cancellation: CancellationToken,
}

impl ShutdownContext {
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub async fn cancelled(&self) {
        self.cancellation.cancelled().await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionKind {
    Application,
    Control,
}

#[derive(Debug, thiserror::Error)]
#[error("application admission is closed")]
pub struct AdmissionClosed;

#[derive(Clone, Default)]
pub struct AdmissionGate {
    inner: Arc<AdmissionInner>,
}

#[derive(Default)]
struct AdmissionInner {
    open: std::sync::atomic::AtomicBool,
    failed: std::sync::atomic::AtomicBool,
    application: std::sync::atomic::AtomicUsize,
    control: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

impl AdmissionGate {
    /// Opens application admission unless a critical startup/runtime
    /// participant has permanently failed.
    pub fn open(&self) {
        let _ = self.try_open();
    }

    /// Attempts to open application admission and reports whether it opened.
    pub fn try_open(&self) -> bool {
        if self.has_failed() {
            return false;
        }
        self.inner
            .open
            .store(true, std::sync::atomic::Ordering::Release);
        if self.has_failed() {
            self.close();
            return false;
        }
        true
    }

    pub fn close(&self) {
        self.inner
            .open
            .store(false, std::sync::atomic::Ordering::Release);
    }

    /// Permanently fails this gate. Unlike close(), a later startup open()
    /// cannot undo a critical participant failure.
    pub fn fail(&self) {
        self.inner
            .failed
            .store(true, std::sync::atomic::Ordering::Release);
        self.close();
    }

    pub fn has_failed(&self) -> bool {
        self.inner.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn is_open(&self) -> bool {
        self.inner.open.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn try_enter(&self, kind: AdmissionKind) -> Result<AdmissionPermit, AdmissionClosed> {
        if kind == AdmissionKind::Application && !self.is_open() {
            return Err(AdmissionClosed);
        }
        let counter = match kind {
            AdmissionKind::Application => &self.inner.application,
            AdmissionKind::Control => &self.inner.control,
        };
        counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if kind == AdmissionKind::Application && !self.is_open() {
            counter.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return Err(AdmissionClosed);
        }
        Ok(AdmissionPermit {
            inner: Arc::clone(&self.inner),
            kind,
        })
    }

    pub fn active(&self, kind: AdmissionKind) -> usize {
        match kind {
            AdmissionKind::Application => &self.inner.application,
            AdmissionKind::Control => &self.inner.control,
        }
        .load(std::sync::atomic::Ordering::Acquire)
    }

    pub async fn wait_for_zero(&self, kind: AdmissionKind, context: &ShutdownContext) -> bool {
        loop {
            let changed = self.inner.changed.notified();
            if self.active(kind) == 0 {
                return true;
            }
            tokio::select! {
                _ = context.cancelled() => return false,
                result = tokio::time::timeout_at(context.deadline, changed) => {
                    if result.is_err() {
                        return false;
                    }
                }
            }
        }
    }
}

pub struct AdmissionPermit {
    inner: Arc<AdmissionInner>,
    kind: AdmissionKind,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        match self.kind {
            AdmissionKind::Application => &self.inner.application,
            AdmissionKind::Control => &self.inner.control,
        }
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.inner.changed.notify_waiters();
    }
}

#[async_trait]
pub trait LifecycleParticipant: Send + Sync {
    fn name(&self) -> &'static str;

    /// Stops admission to participant-owned background work before the
    /// transport drains requests that were already accepted.
    async fn quiesce(
        &self,
        _config: &RuntimeConfig,
        _context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn shutdown(
        &self,
        config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError>;
}

#[derive(Clone, Default)]
pub struct LifecycleRegistrar {
    inner: Arc<Mutex<LifecycleState>>,
}

#[derive(Default)]
struct LifecycleState {
    sealed: bool,
    names: HashSet<&'static str>,
    participants: Vec<Arc<dyn LifecycleParticipant>>,
}

#[derive(Clone, Default)]
pub struct LifecycleRegistry {
    registrar: LifecycleRegistrar,
}

impl LifecycleRegistry {
    pub fn registrar(&self) -> LifecycleRegistrar {
        self.registrar.clone()
    }

    pub fn seal(&self) {
        self.registrar.inner.lock().expect("lifecycle lock").sealed = true;
    }

    pub fn len(&self) -> usize {
        self.registrar
            .inner
            .lock()
            .expect("lifecycle lock")
            .participants
            .len()
    }

    pub async fn shutdown(
        &self,
        config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Vec<RuntimeError> {
        let participants = self
            .registrar
            .inner
            .lock()
            .expect("lifecycle lock")
            .participants
            .clone();
        let mut errors = Vec::new();
        for participant in participants.into_iter().rev() {
            if let Err(error) = participant.shutdown(config, context).await {
                errors.push(error);
            }
        }
        errors
    }

    pub async fn quiesce(
        &self,
        config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Vec<RuntimeError> {
        let participants = self
            .registrar
            .inner
            .lock()
            .expect("lifecycle lock")
            .participants
            .clone();
        let mut errors = Vec::new();
        for participant in participants.into_iter().rev() {
            if let Err(error) = participant.quiesce(config, context).await {
                errors.push(error);
            }
        }
        errors
    }
}

impl LifecycleRegistrar {
    pub fn register(&self, participant: Arc<dyn LifecycleParticipant>) -> Result<(), RuntimeError> {
        let mut state = self.inner.lock().expect("lifecycle lock");
        if state.sealed {
            return Err(RuntimeError::LifecycleSealed);
        }
        let name = participant.name();
        if !state.names.insert(name) {
            return Err(RuntimeError::DuplicateLifecycleParticipant(name));
        }
        state.participants.push(participant);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, ServerConfig, ServiceIdentity,
    };
    use std::path::PathBuf;

    struct Recorder {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
        quiesce_fail: bool,
    }

    #[async_trait]
    impl LifecycleParticipant for Recorder {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn quiesce(
            &self,
            _config: &RuntimeConfig,
            _context: &ShutdownContext,
        ) -> Result<(), RuntimeError> {
            self.calls.lock().unwrap().push(self.name);
            if self.quiesce_fail {
                Err(RuntimeError::Unsupported(format!(
                    "{} quiesce failed",
                    self.name
                )))
            } else {
                Ok(())
            }
        }

        async fn shutdown(
            &self,
            _config: &RuntimeConfig,
            _context: &ShutdownContext,
        ) -> Result<(), RuntimeError> {
            self.calls.lock().unwrap().push(self.name);
            if self.fail {
                Err(RuntimeError::Unsupported(format!("{} failed", self.name)))
            } else {
                Ok(())
            }
        }
    }

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config"),
            resolved_values: Default::default(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn admission_is_closed_by_default_and_counts_permits() {
        let gate = AdmissionGate::default();
        assert!(gate.try_enter(AdmissionKind::Application).is_err());
        let control = gate.try_enter(AdmissionKind::Control).unwrap();
        assert_eq!(gate.active(AdmissionKind::Control), 1);
        gate.open();
        let application = gate.try_enter(AdmissionKind::Application).unwrap();
        assert_eq!(gate.active(AdmissionKind::Application), 1);
        gate.close();
        assert!(gate.try_enter(AdmissionKind::Application).is_err());
        drop(application);
        drop(control);
        assert_eq!(gate.active(AdmissionKind::Application), 0);
        assert_eq!(gate.active(AdmissionKind::Control), 0);
    }

    #[test]
    fn critical_failure_permanently_prevents_admission_reopen() {
        let gate = AdmissionGate::default();
        assert!(gate.try_open());
        gate.fail();
        assert!(gate.has_failed());
        assert!(!gate.try_open());
        assert!(!gate.is_open());
        assert!(gate.try_enter(AdmissionKind::Application).is_err());
    }

    #[tokio::test]
    async fn admission_wait_for_zero_tracks_the_full_permit_lifetime() {
        let gate = AdmissionGate::default();
        gate.open();
        let permit = gate.try_enter(AdmissionKind::Application).unwrap();
        let context = ShutdownContext {
            reason: ShutdownReason::Programmatic,
            mode: ShutdownMode::Graceful,
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        };
        let waiting = gate.wait_for_zero(AdmissionKind::Application, &context);
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        drop(permit);
        assert!(waiting.await);
    }

    #[tokio::test]
    async fn lifecycle_runs_in_reverse_order_and_aggregates_errors() {
        let registry = LifecycleRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for (name, fail) in [("first", true), ("second", false), ("third", true)] {
            registry
                .registrar()
                .register(Arc::new(Recorder {
                    name,
                    calls: Arc::clone(&calls),
                    fail,
                    quiesce_fail: false,
                }))
                .unwrap();
        }
        registry.seal();
        assert!(matches!(
            registry.registrar().register(Arc::new(Recorder {
                name: "late",
                calls: Arc::clone(&calls),
                fail: false,
                quiesce_fail: false,
            })),
            Err(RuntimeError::LifecycleSealed)
        ));
        let context = ShutdownContext {
            reason: ShutdownReason::Programmatic,
            mode: ShutdownMode::Graceful,
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        };
        let errors = registry.shutdown(&config(), &context).await;
        assert_eq!(*calls.lock().unwrap(), vec!["third", "second", "first"]);
        assert_eq!(errors.len(), 2);
    }

    #[test]
    fn lifecycle_rejects_duplicate_names() {
        let registry = LifecycleRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for expected_ok in [true, false] {
            let result = registry.registrar().register(Arc::new(Recorder {
                name: "duplicate",
                calls: Arc::clone(&calls),
                fail: false,
                quiesce_fail: false,
            }));
            assert_eq!(result.is_ok(), expected_ok);
        }
    }

    #[tokio::test]
    async fn lifecycle_quiesces_in_reverse_order_and_aggregates_errors() {
        let registry = LifecycleRegistry::default();
        let calls = Arc::new(Mutex::new(Vec::new()));
        for (name, quiesce_fail) in [("first", true), ("second", false), ("third", true)] {
            registry
                .registrar()
                .register(Arc::new(Recorder {
                    name,
                    calls: Arc::clone(&calls),
                    fail: false,
                    quiesce_fail,
                }))
                .unwrap();
        }
        registry.seal();
        let context = ShutdownContext {
            reason: ShutdownReason::Programmatic,
            mode: ShutdownMode::Graceful,
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        };

        let errors = registry.quiesce(&config(), &context).await;

        assert_eq!(*calls.lock().unwrap(), vec!["third", "second", "first"]);
        assert_eq!(errors.len(), 2);
    }
}
