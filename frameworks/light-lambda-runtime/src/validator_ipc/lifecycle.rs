//! Opt-in qualification evidence from the actual child-owning process. No worker
//! selection, lifecycle policy, or default activation is changed by observing.
use super::{ControllerSession, Engine, Failure, Session, Settings, controller};
use async_trait::async_trait;
use gateway_policy::openapi_operation::SourceFormat;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::unix::{fs::OpenOptionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::{Arc, Mutex},
};
use tokio::{sync::Notify, time::Instant};
const MAX_CHILDREN: usize = 8;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CompletionCause {
    Success,
    AssertionFailure,
    Timeout,
    ControlClosed,
    RuntimeTerminated,
}
impl CompletionCause {
    pub fn wire(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::AssertionFailure => "assertionFailure",
            Self::Timeout => "timeout",
            Self::ControlClosed => "controlClosed",
            Self::RuntimeTerminated => "runtimeTerminated",
        }
    }
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "assertionFailure" => Some(Self::AssertionFailure),
            "timeout" => Some(Self::Timeout),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShutdownTrigger {
    InitRejected,
    InitFailed,
    ReadinessReceiverDropped,
    RequestFailed,
    ActorSendersDropped,
    ExplicitShutdown,
    OwnerTaskDropped,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub owner_pid: u32,
    pub child_pid: u32,
    pub start_time_ticks: Option<u64>,
    pub generation: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitEvidence {
    pub waiter_pid: u32,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub raw_status: Option<i32>,
    pub wait_error: Option<String>,
    pub reaped_by_owner: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildLifecycle {
    pub identity: Identity,
    pub shutdown_trigger: Option<ShutdownTrigger>,
    pub completion_cause: Option<CompletionCause>,
    pub kill_attempted: bool,
    pub kill_error: Option<String>,
    pub wait: Option<WaitEvidence>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub evidence_origin: String,
    pub owner_pid: u32,
    pub active_owners: usize,
    pub completion_cause: Option<CompletionCause>,
    pub recording_failed: bool,
    pub children: Vec<ChildLifecycle>,
}
impl Snapshot {
    pub fn clean(&self) -> bool {
        !self.recording_failed
            && self.active_owners == 0
            && self.children.iter().all(|c| {
                c.identity.owner_pid == self.owner_pid
                    && c.identity.start_time_ticks.is_some()
                    && c.shutdown_trigger.is_some()
                    && c.kill_attempted
                    && c.wait.as_ref().is_some_and(|w| {
                        w.reaped_by_owner
                            && w.waiter_pid == self.owner_pid
                            && w.wait_error.is_none()
                            && w.raw_status.is_some()
                    })
            })
    }
}
struct State {
    snapshot: Snapshot,
    output: Option<File>,
}
pub struct Lifecycle {
    state: Mutex<State>,
    changed: Notify,
}
impl Lifecycle {
    /// Qualification composition supplies an owned, previously absent path.
    pub fn create(path: &Path) -> io::Result<Arc<Self>> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self::new(Some(file), "actualChildOwner"))
    }
    fn new(output: Option<File>, origin: &str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                snapshot: Snapshot {
                    evidence_origin: origin.into(),
                    owner_pid: std::process::id(),
                    active_owners: 0,
                    completion_cause: None,
                    recording_failed: false,
                    children: Vec::with_capacity(MAX_CHILDREN),
                },
                output,
            }),
            changed: Notify::new(),
        })
    }
    fn publish(&self, state: &mut State) {
        if let Some(file) = &mut state.output {
            let written = serde_json::to_writer(&mut *file, &state.snapshot)
                .and_then(|_| file.write_all(b"\n").map_err(serde_json::Error::io))
                .and_then(|_| file.flush().map_err(serde_json::Error::io));
            if written.is_err() {
                state.snapshot.recording_failed = true;
            }
        }
        self.changed.notify_waiters();
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.lock().unwrap().snapshot.clone()
    }
    pub fn request_shutdown(&self, cause: CompletionCause) {
        let mut state = self.state.lock().unwrap();
        // First request is sticky; no assertion/timeout failure can be relabeled success.
        if state.snapshot.completion_cause.is_none() {
            state.snapshot.completion_cause = Some(cause);
        }
        self.publish(&mut state);
    }
    pub(crate) fn owner_started(self: &Arc<Self>) -> OwnerObservation {
        let mut state = self.state.lock().unwrap();
        state.snapshot.active_owners += 1;
        self.publish(&mut state);
        OwnerObservation(self.clone())
    }
    pub(crate) fn spawned(self: &Arc<Self>, pid: u32) -> ChildObservation {
        let start = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| start_time(&s));
        self.register(pid, start)
    }
    fn register(self: &Arc<Self>, pid: u32, start: Option<u64>) -> ChildObservation {
        let mut state = self.state.lock().unwrap();
        let index = state.snapshot.children.len();
        if index == MAX_CHILDREN {
            state.snapshot.recording_failed = true;
        } else {
            let owner_pid = state.snapshot.owner_pid;
            state.snapshot.children.push(ChildLifecycle {
                identity: Identity {
                    owner_pid,
                    child_pid: pid,
                    start_time_ticks: start,
                    generation: index + 1,
                },
                shutdown_trigger: None,
                completion_cause: None,
                kill_attempted: false,
                kill_error: None,
                wait: None,
            });
        }
        self.publish(&mut state);
        ChildObservation {
            lifecycle: self.clone(),
            index,
        }
    }
    /// Called after dropping the application runtime future; owner guards cover
    /// queued/startup tasks too, so zero records alone never proves quiescence.
    pub async fn wait_idle(&self, deadline: Instant) -> io::Result<Snapshot> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = self.snapshot();
            if snapshot.active_owners == 0 {
                return if snapshot.clean() {
                    Ok(snapshot)
                } else {
                    Err(io::Error::other(
                        "precision child lifecycle evidence incomplete",
                    ))
                };
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "precision child cleanup not observed",
                    )
                })?;
        }
    }
}
/// Parse Linux stat field22 after the final ')' of comm, which may contain spaces
/// or parentheses. No process command line, source, request, or token is logged.
fn start_time(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}
pub(crate) struct OwnerObservation(Arc<Lifecycle>);
impl Drop for OwnerObservation {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.snapshot.active_owners -= 1;
        self.0.publish(&mut state);
    }
}
pub(crate) struct ChildObservation {
    lifecycle: Arc<Lifecycle>,
    index: usize,
}
impl ChildObservation {
    /// Drop/unwind cannot await or manufacture a wait. Preserve a previously
    /// recorded shutdown and report only bounded error kinds on best-effort kill.
    pub(crate) fn abandoned(&self, result: &io::Result<()>) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if self.lifecycle.state.is_poisoned() {
            state.snapshot.recording_failed = true;
        }
        let cause = state.snapshot.completion_cause;
        if let Some(child) = state.snapshot.children.get_mut(self.index) {
            child
                .shutdown_trigger
                .get_or_insert(ShutdownTrigger::OwnerTaskDropped);
            child.completion_cause = child.completion_cause.or(cause);
            child.kill_attempted = true;
            if let Err(error) = result {
                child
                    .kill_error
                    .get_or_insert_with(|| format!("{:?}", error.kind()));
            }
        }
        self.lifecycle.publish(&mut state);
    }
    pub(crate) fn shutdown(&self, trigger: ShutdownTrigger) {
        let mut state = self.lifecycle.state.lock().unwrap();
        let cause = state.snapshot.completion_cause;
        if let Some(child) = state.snapshot.children.get_mut(self.index) {
            child.shutdown_trigger = Some(trigger);
            child.completion_cause = cause;
        }
        self.lifecycle.publish(&mut state);
    }
    /// Only the Process holding tokio::process::Child calls this with its actual
    /// Child::wait result. An error is never recorded as successful reaping.
    pub(crate) fn waited(&self, result: &io::Result<ExitStatus>) {
        let mut state = self.lifecycle.state.lock().unwrap();
        if let Some(child) = state.snapshot.children.get_mut(self.index) {
            let waiter_pid = std::process::id();
            let evidence = match result {
                Ok(status) => WaitEvidence {
                    waiter_pid,
                    exit_code: status.code(),
                    signal: status.signal(),
                    raw_status: Some(status.into_raw()),
                    wait_error: None,
                    reaped_by_owner: waiter_pid == child.identity.owner_pid,
                },
                Err(error) => WaitEvidence {
                    waiter_pid,
                    exit_code: None,
                    signal: None,
                    raw_status: None,
                    wait_error: Some(format!("{:?}", error.kind())),
                    reaped_by_owner: false,
                },
            };
            child.wait = Some(evidence);
        }
        self.lifecycle.publish(&mut state);
    }
    pub(crate) fn kill_attempted(&self, result: &io::Result<()>) {
        let mut state = self.lifecycle.state.lock().unwrap();
        if let Some(child) = state.snapshot.children.get_mut(self.index) {
            child.kill_attempted = true;
            child.kill_error = result.as_ref().err().map(|e| format!("{:?}", e.kind()));
        }
        self.lifecycle.publish(&mut state);
    }
}
/// Separate explicit qualification engine. Default engines have no observer.
pub struct ObservedProfileEngine {
    pub executable: PathBuf,
    pub profile: controller::ResourceProfile,
    pub lifecycle: Arc<Lifecycle>,
}
#[async_trait]
impl Engine for ObservedProfileEngine {
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_shared(source, format, Arc::new(settings), deadline)
            .await
    }
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        let format = match format {
            SourceFormat::Json => controller::SourceFormat::Json,
            SourceFormat::Yaml => controller::SourceFormat::Yaml,
        };
        let controller = controller::Controller::prepare(
            &self.executable,
            &source,
            format,
            settings,
            self.profile,
        )?;
        *controller.stats.lifecycle.lock().unwrap() = Some(self.lifecycle.clone());
        controller.recover(deadline).await?;
        let ready = controller.actor().await?.ready;
        Ok(Arc::new(ControllerSession { controller, ready }))
    }
}

/// Explicit sealed-main-image qualification, separate from legacy path profiles.
/// Trusted composition pins the worker; default application admission is closed.
pub struct SealedObservedProfileEngine {
    pub executable: crate::bundle_snapshot::Pin,
    pub profile: controller::ResourceProfile,
    pub lifecycle: Arc<Lifecycle>,
}
#[async_trait]
impl Engine for SealedObservedProfileEngine {
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_shared(source, format, Arc::new(settings), deadline)
            .await
    }
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        let format = match format {
            SourceFormat::Json => controller::SourceFormat::Json,
            SourceFormat::Yaml => controller::SourceFormat::Yaml,
        };
        let controller = controller::Controller::prepare_pinned(
            &self.executable,
            &source,
            format,
            settings,
            self.profile,
        )?;
        *controller.stats.lifecycle.lock().unwrap() = Some(self.lifecycle.clone());
        controller.recover(deadline).await?;
        let ready = controller.actor().await?.ready;
        Ok(Arc::new(ControllerSession { controller, ready }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_missing_owner_identity_kill_wait_and_output_witnesses_fail_closed() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let child = lifecycle.register(123, Some(456));
        child.shutdown(ShutdownTrigger::ActorSendersDropped);
        child.kill_attempted(&Ok(()));
        child.waited(&Ok(ExitStatus::from_raw(0)));
        let valid = lifecycle.snapshot();
        assert!(valid.clean());
        let mut bad = valid.clone();
        bad.children[0].identity.start_time_ticks = None;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].identity.owner_pid += 1;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].shutdown_trigger = None;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].kill_attempted = false;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].wait = None;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].wait.as_mut().unwrap().waiter_pid += 1;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.children[0].wait.as_mut().unwrap().raw_status = None;
        assert!(!bad.clean());
        let mut bad = valid.clone();
        bad.active_owners = 1;
        assert!(!bad.clean());
        let mut bad = valid;
        bad.recording_failed = true;
        assert!(!bad.clean());
    }
    #[test]
    fn proc_identity_parser_handles_spaces_and_parentheses() {
        let stat = format!("42 (name (with spaces)) S {} 987", vec!["0"; 18].join(" "));
        assert_eq!(start_time(&stat), Some(987));
        assert_eq!(start_time("malformed"), None);
    }
    #[tokio::test]
    async fn simulated_owner_wait_records_exit_and_trigger_without_spawning() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let owner = lifecycle.owner_started();
        let child = lifecycle.register(123, Some(456));
        lifecycle.request_shutdown(CompletionCause::AssertionFailure);
        child.shutdown(ShutdownTrigger::ActorSendersDropped);
        child.kill_attempted(&Ok(()));
        child.waited(&Ok(ExitStatus::from_raw(9)));
        assert!(!lifecycle.snapshot().clean());
        drop(owner);
        let snapshot = lifecycle.wait_idle(Instant::now()).await.unwrap();
        assert_eq!(snapshot.evidence_origin, "simulatedNoChild");
        let child = &snapshot.children[0];
        assert_eq!(
            child.completion_cause,
            Some(CompletionCause::AssertionFailure)
        );
        assert_eq!(child.wait.as_ref().unwrap().signal, Some(9));
        assert!(child.wait.as_ref().unwrap().reaped_by_owner);
    }
    #[tokio::test]
    async fn wait_error_and_missing_identity_cannot_count_as_reaped() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let child = lifecycle.register(123, Some(456));
        child.shutdown(ShutdownTrigger::InitFailed);
        child.waited(&Err(io::Error::from(io::ErrorKind::Other)));
        assert!(
            !lifecycle.snapshot().children[0]
                .wait
                .as_ref()
                .unwrap()
                .reaped_by_owner
        );
        assert!(lifecycle.wait_idle(Instant::now()).await.is_err());
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let child = lifecycle.register(123, None);
        child.shutdown(ShutdownTrigger::InitFailed);
        child.waited(&Ok(ExitStatus::from_raw(0)));
        assert!(!lifecycle.snapshot().clean());
    }
    #[tokio::test]
    async fn outstanding_owner_and_missing_wait_are_incomplete() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let owner = lifecycle.owner_started();
        let child = lifecycle.register(123, Some(456));
        child.shutdown(ShutdownTrigger::ActorSendersDropped);
        assert!(lifecycle.wait_idle(Instant::now()).await.is_err());
        drop(owner);
        assert!(lifecycle.wait_idle(Instant::now()).await.is_err());
    }
    #[test]
    fn event_file_is_exclusive_bounded_and_excludes_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.jsonl");
        let lifecycle = Lifecycle::create(&path).unwrap();
        assert!(Lifecycle::create(&path).is_err());
        lifecycle.request_shutdown(CompletionCause::Timeout);
        lifecycle.request_shutdown(CompletionCause::Success);
        assert_eq!(
            lifecycle.snapshot().completion_cause,
            Some(CompletionCause::Timeout)
        );
        for _ in 0..=MAX_CHILDREN {
            lifecycle.register(123, Some(456));
        }
        let snapshot = lifecycle.snapshot();
        assert_eq!(snapshot.children.len(), MAX_CHILDREN);
        assert!(snapshot.recording_failed);
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines() {
            let _: Snapshot = serde_json::from_str(line).unwrap();
        }
        for field in ["payload", "source", "token", "commandLine"] {
            assert!(!text.contains(field));
        }
    }

    #[test]
    fn abandoned_owner_records_no_wait_and_preserves_first_trigger_and_privacy() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let child = lifecycle.register(123, Some(456));
        child.shutdown(ShutdownTrigger::InitFailed);
        child.abandoned(&Err(io::Error::other("synthetic-private-payload")));
        let snapshot = lifecycle.snapshot();
        let record = &snapshot.children[0];
        assert_eq!(record.shutdown_trigger, Some(ShutdownTrigger::InitFailed));
        assert!(record.wait.is_none());
        assert!(!snapshot.clean());
        let text = serde_json::to_string(&snapshot).unwrap();
        assert!(!text.contains("synthetic-private-payload"));
        assert_eq!(record.kill_error.as_deref(), Some("Other"));
        let child = lifecycle.register(124, Some(457));
        child.abandoned(&Ok(()));
        assert_eq!(
            lifecycle.snapshot().children[1].shutdown_trigger,
            Some(ShutdownTrigger::OwnerTaskDropped)
        );
    }
    #[test]
    fn abandoned_observer_recovers_poison_without_claiming_reap() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let child = lifecycle.register(123, Some(456));
        let other = lifecycle.clone();
        assert!(
            std::thread::spawn(move || {
                let _lock = other.state.lock().unwrap();
                panic!("synthetic");
            })
            .join()
            .is_err()
        );
        child.abandoned(&Ok(()));
        let state = lifecycle.state.lock().unwrap_or_else(|e| e.into_inner());
        assert!(state.snapshot.recording_failed);
        assert!(state.snapshot.children[0].wait.is_none());
    }
}

/// Isolated observation adapter. Always uses the real CatalogueEngine gate.
pub struct CatalogueObservedProfileEngine {
    pub engine: Arc<super::bundle_admission::CatalogueEngine>,
    pub lifecycle: Arc<Lifecycle>,
}
#[async_trait]
impl Engine for CatalogueObservedProfileEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        self.engine.configuration_preparation()
    }
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_shared(source, format, Arc::new(settings), deadline)
            .await
    }
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_owned(source, format, settings, deadline, None)
            .await
    }
    async fn initialize_owned_diagnosed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        diagnostic: Arc<crate::validator_init_diagnostic::InitDiagnostic>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.engine
            .initialize_observed(
                source,
                format,
                settings,
                deadline,
                context,
                super::EngineObservation {
                    lifecycle: Some(self.lifecycle.clone()),
                    diagnostic: Some(diagnostic),
                },
            )
            .await
    }
    async fn initialize_owned(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.engine
            .initialize_observed(
                source,
                format,
                settings,
                deadline,
                context,
                super::EngineObservation {
                    lifecycle: Some(self.lifecycle.clone()),
                    diagnostic: None,
                },
            )
            .await
    }
}
impl CatalogueObservedProfileEngine {
    pub async fn shutdown(&self, deadline: Instant) -> Result<super::ownership::Snapshot, Failure> {
        self.engine.engine.shutdown(deadline).await
    }
}

#[cfg(test)]
mod catalogue_observer_tests {
    use super::*;
    #[tokio::test]
    async fn mismatch_rejects_before_child_or_observation() {
        let lifecycle = Lifecycle::new(None, "simulatedNoChild");
        let engine = CatalogueObservedProfileEngine {
            engine: Arc::new(super::super::bundle_admission::CatalogueEngine {
                engine: Arc::new(super::super::SupervisedStaticEngine {
                    executable: crate::bundle_snapshot::Pin {
                        path: "/not-opened-catalogue-observer".into(),
                        bytes: 64,
                        sha256: [0; 32],
                    },
                    policy: super::super::supervision::Policy {
                        platform_bytes: 3,
                        parent_requested_bytes: 1,
                        worker_requested_bytes: 1,
                        headroom_bytes: 1,
                        worker_address_space_bytes: 1,
                        lifetime_cpu_seconds: 1,
                    },
                }),
                catalogue: super::super::bundle_admission::CataloguePin {
                    source_bytes: 3,
                    source_sha256: [0; 32],
                    format: SourceFormat::Yaml,
                    settings: Settings::default(),
                },
            }),
            lifecycle: lifecycle.clone(),
        };
        assert!(engine.configuration_preparation().is_some());
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(b"bad".as_slice()),
                    SourceFormat::Yaml,
                    Settings::default(),
                    Instant::now() + std::time::Duration::from_secs(1)
                )
                .await,
            Err(Failure::AdapterInvariant)
        ));
        let state = lifecycle.snapshot();
        assert_eq!(state.active_owners, 0);
        assert!(state.children.is_empty());
    }
}
