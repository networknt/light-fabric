use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

use chrono::Utc;
use dashmap::DashMap;
use execution_backend::{BackendError, BackendOperationState, ExecutionBackend};
use execution_runner_protocol::{
    ActiveLeaseSummary, AttemptState, CancelLease, CleanupState, CommandExecutionSpec,
    ExecuteLease, ExecutionId, LeaseContext, LeaseResultAccepted, NormalizedOutput, OriginKind,
    RetrySafety, RunnerToController, TerminalLeaseResult,
};
use tokio::sync::{Semaphore, mpsc, watch};
use tracing::{error, info, warn};

use crate::journal::{Journal, JournalRecord, JournalState};
use crate::normalization::{from_backend_error, from_backend_output};
use crate::staging::InputStager;
use crate::worker_process::{WorkerProcessConfig, run_worker_process};

#[derive(Clone)]
struct ActiveExecution {
    lease: ExecuteLease,
    backend_operation_id: String,
    cancel: watch::Sender<bool>,
}

pub struct Supervisor {
    backend: Arc<dyn ExecutionBackend>,
    journal: Journal,
    stager: InputStager,
    allowed_template_digests: std::collections::BTreeSet<String>,
    active: DashMap<ExecutionId, ActiveExecution>,
    pending_results: DashMap<ExecutionId, TerminalLeaseResult>,
    capacity: Arc<Semaphore>,
    draining: AtomicBool,
    watchdog_last_tick_ms: AtomicI64,
    agent_worker: Option<WorkerProcessConfig>,
}

impl Supervisor {
    pub fn new(
        backend: Arc<dyn ExecutionBackend>,
        journal: Journal,
        stager: InputStager,
        allowed_template_digests: std::collections::BTreeSet<String>,
        maximum_concurrency: u32,
        agent_worker: Option<WorkerProcessConfig>,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend,
            journal,
            stager,
            allowed_template_digests,
            active: DashMap::new(),
            pending_results: DashMap::new(),
            capacity: Arc::new(Semaphore::new(maximum_concurrency as usize)),
            draining: AtomicBool::new(false),
            watchdog_last_tick_ms: AtomicI64::new(Utc::now().timestamp_millis()),
            agent_worker,
        })
    }

    pub fn backend_capability(&self) -> execution_runner_protocol::BackendCapability {
        let mut capability = self.backend.capability();
        capability.available_slots = self.available_capacity();
        capability.healthy = !self.draining.load(Ordering::Acquire) && self.journal.is_healthy();
        capability
    }

    pub fn available_capacity(&self) -> u32 {
        if self.draining.load(Ordering::Acquire) {
            0
        } else {
            u32::try_from(self.capacity.available_permits()).unwrap_or(u32::MAX)
        }
    }

    pub fn active_leases(&self) -> Vec<ActiveLeaseSummary> {
        let mut leases = self
            .active
            .iter()
            .map(|entry| ActiveLeaseSummary {
                execution_id: *entry.key(),
                lease_id: entry.lease.lease.lease_id,
                fencing_token: entry.lease.lease.fencing_token,
                state: AttemptState::Started,
            })
            .collect::<Vec<_>>();
        leases.sort_by_key(|lease| lease.execution_id.0);
        leases
    }

    /// Returns complete fenced lease contexts for transport-level renewals.
    pub fn active_lease_contexts(&self) -> Vec<LeaseContext> {
        let mut leases = self
            .active
            .iter()
            .map(|entry| entry.lease.lease.clone())
            .collect::<Vec<_>>();
        leases.sort_by_key(|lease| lease.execution_id.0);
        leases
    }

    pub fn cleanup_backlog(&self) -> u32 {
        self.journal.cleanup_backlog().unwrap_or(u32::MAX)
    }

    pub fn journal_healthy(&self) -> bool {
        self.journal.is_healthy()
    }

    pub fn watchdog_healthy(&self) -> bool {
        Utc::now()
            .timestamp_millis()
            .saturating_sub(self.watchdog_last_tick_ms.load(Ordering::Acquire))
            <= 2_000
    }

    pub fn drain(&self) {
        self.draining.store(true, Ordering::Release);
    }

    pub async fn accept_execute(
        self: &Arc<Self>,
        lease: ExecuteLease,
        outbound: mpsc::Sender<RunnerToController>,
    ) -> Result<(), String> {
        if self.draining.load(Ordering::Acquire) {
            return Err("runner is draining".to_string());
        }
        if lease.lease.deadline <= Utc::now() {
            return Err("execution lease is already expired".to_string());
        }
        if let Some(record) = self.journal.find(lease.lease.execution_id)? {
            if record.lease_id != lease.lease.lease_id
                || record.fencing_token != lease.lease.fencing_token
            {
                return Err("duplicate execution carries a stale lease or fence".to_string());
            }
            outbound
                .send(RunnerToController::RunnerLeaseAccepted(lease.lease.clone()))
                .await
                .map_err(|_| "controller outbound channel closed".to_string())?;
            if let Some(result) = self.pending_results.get(&lease.lease.execution_id) {
                send_terminal(&outbound, result.clone()).await?;
            }
            return Ok(());
        }

        if lease.lease.origin.kind == OriginKind::Agent {
            return self.accept_agent_worker(lease, outbound).await;
        }
        validate_command(&lease, &self.allowed_template_digests)?;
        self.journal.record_intent(&lease)?;
        outbound
            .send(RunnerToController::RunnerLeaseAccepted(lease.lease.clone()))
            .await
            .map_err(|_| "controller outbound channel closed".to_string())?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| "runner capacity was exhausted".to_string())?;
        self.journal
            .set_state(lease.lease.execution_id, JournalState::Preparing, None)?;
        let command = serde_json::from_value::<CommandExecutionSpec>(lease.command.clone())
            .map_err(|error| format!("invalid command execution spec: {error}"))?;
        let staged = match self.stager.stage(&lease) {
            Ok(staged) => staged,
            Err(error) => {
                drop(permit);
                self.report_setup_failure(lease, BackendError::InvalidRequest(error), outbound)
                    .await?;
                return Ok(());
            }
        };
        if let Err(error) = self.backend.validate(&lease, &staged) {
            drop(permit);
            self.report_setup_failure(lease, error, outbound).await?;
            return Ok(());
        }
        let prepared = match self.backend.prepare(&lease, &staged).await {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(permit);
                self.report_setup_failure(lease, error, outbound).await?;
                return Ok(());
            }
        };
        self.journal.set_state(
            lease.lease.execution_id,
            JournalState::Prepared,
            Some(&prepared.backend_operation_id),
        )?;
        let (cancel, cancellation) = watch::channel(false);
        self.active.insert(
            lease.lease.execution_id,
            ActiveExecution {
                lease: lease.clone(),
                backend_operation_id: prepared.backend_operation_id.clone(),
                cancel,
            },
        );
        outbound
            .send(RunnerToController::RunnerLeaseStarted(lease.lease.clone()))
            .await
            .map_err(|_| "controller outbound channel closed".to_string())?;
        self.journal.set_state(
            lease.lease.execution_id,
            JournalState::Executing,
            Some(&prepared.backend_operation_id),
        )?;

        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            let started_at = Utc::now();
            let remaining = lease
                .lease
                .deadline
                .signed_duration_since(Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO);
            let wall_clock = Duration::from_millis(command.wall_clock_timeout_ms).min(remaining);
            let execution = tokio::time::timeout(
                wall_clock,
                supervisor.backend.execute(&prepared, &lease, cancellation),
            )
            .await;
            let mut result = match execution {
                Ok(Ok(output)) => from_backend_output(
                    &lease,
                    &command,
                    prepared.backend_operation_id.clone(),
                    output,
                ),
                Ok(Err(error)) => {
                    // The watchdog is deliberately independent of the controller
                    // connection. If it wins the race at the lease deadline, a
                    // backend can surface that stop as `Cancelled`; the durable
                    // outcome is still a deadline expiry, not a user cancellation.
                    let deadline_error;
                    let error = if Utc::now() >= lease.lease.deadline {
                        deadline_error =
                            BackendError::TimedOut("execution lease deadline expired".to_string());
                        &deadline_error
                    } else {
                        &error
                    };
                    from_backend_error(
                        &lease,
                        prepared.backend_operation_id.clone(),
                        started_at,
                        error,
                    )
                }
                Err(_) => {
                    let _ = supervisor
                        .backend
                        .cancel(&prepared.backend_operation_id)
                        .await;
                    from_backend_error(
                        &lease,
                        prepared.backend_operation_id.clone(),
                        started_at,
                        &BackendError::TimedOut("local wall-clock deadline expired".to_string()),
                    )
                }
            };
            match supervisor
                .backend
                .collect_artifacts(&prepared.backend_operation_id)
                .await
            {
                Ok(artifacts) if artifacts.len() <= 1024 => result.artifacts = artifacts,
                Ok(_) => {
                    result.state = AttemptState::Failed;
                    result.failure_class = Some("artifact_manifest_too_large".to_string());
                }
                Err(error) => {
                    warn!(execution_id = %lease.lease.execution_id, %error, "artifact collection failed");
                }
            }
            match supervisor
                .cleanup_with_retry(&prepared.backend_operation_id)
                .await
            {
                Ok(evidence) => {
                    result.cleanup_state = CleanupState::Confirmed;
                    result
                        .evidence
                        .insert("cleanupEvidence".to_string(), evidence.evidence_reference);
                    let _ = supervisor.journal.set_state(
                        lease.lease.execution_id,
                        JournalState::CleanupRequired,
                        Some(&prepared.backend_operation_id),
                    );
                }
                Err(error) => {
                    result.cleanup_state = CleanupState::Failed;
                    result
                        .evidence
                        .insert("cleanupError".to_string(), error.to_string());
                }
            }
            let terminal = TerminalLeaseResult {
                lease: lease.lease.clone(),
                result,
            };
            if let Err(error) = supervisor.journal.set_terminal(&terminal) {
                error!(execution_id = %lease.lease.execution_id, %error, "failed to persist terminal-pending journal state");
                return;
            }
            supervisor.active.remove(&lease.lease.execution_id);
            supervisor
                .pending_results
                .insert(lease.lease.execution_id, terminal.clone());
            if let Err(error) = send_terminal(&outbound, terminal).await {
                warn!(execution_id = %lease.lease.execution_id, %error, "terminal result queued for reconnect recovery");
            }
        });
        Ok(())
    }

    async fn accept_agent_worker(
        self: &Arc<Self>,
        lease: ExecuteLease,
        outbound: mpsc::Sender<RunnerToController>,
    ) -> Result<(), String> {
        let config = self
            .agent_worker
            .clone()
            .ok_or_else(|| "agent worker execution is disabled on this runner".to_string())?;
        if lease.lease.origin.service_id != config.origin_service_id {
            return Err("agent lease origin is not admitted by this runner".into());
        }
        if !self
            .allowed_template_digests
            .contains(&lease.command_template_digest)
        {
            return Err("agent worker template digest is not admitted by the runner".into());
        }
        let spec = serde_json::from_value::<agent_runtime_protocol::AgentWorkerExecutionSpec>(
            lease.command.clone(),
        )
        .map_err(|error| format!("invalid agent worker execution spec: {error}"))?;
        self.journal.record_intent(&lease)?;
        outbound
            .send(RunnerToController::RunnerLeaseAccepted(lease.lease.clone()))
            .await
            .map_err(|_| "controller outbound channel closed".to_string())?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| "runner capacity was exhausted".to_string())?;
        let operation_id = format!("agent-worker:{}", lease.lease.execution_id);
        let (cancel, cancellation) = watch::channel(false);
        self.active.insert(
            lease.lease.execution_id,
            ActiveExecution {
                lease: lease.clone(),
                backend_operation_id: operation_id.clone(),
                cancel,
            },
        );
        self.journal.set_state(
            lease.lease.execution_id,
            JournalState::Executing,
            Some(&operation_id),
        )?;
        outbound
            .send(RunnerToController::RunnerLeaseStarted(lease.lease.clone()))
            .await
            .map_err(|_| "controller outbound channel closed".to_string())?;

        let supervisor = Arc::clone(self);
        tokio::spawn(async move {
            let _permit = permit;
            let started_at = Utc::now();
            let execution =
                run_worker_process(&lease, &spec, &config, &supervisor.journal, cancellation).await;
            let finished_at = Utc::now();
            let worker_error = execution.as_ref().err().cloned();
            let mut result = match execution {
                Ok(outcome) => {
                    let (state, failure_class) = match outcome.class {
                        agent_core::ResultClass::Success
                        | agent_core::ResultClass::ApprovalRequired => {
                            (AttemptState::Succeeded, None)
                        }
                        agent_core::ResultClass::RecoverableFailure => {
                            (AttemptState::Failed, Some("recoverable_failure".into()))
                        }
                        agent_core::ResultClass::TerminalFailure => {
                            (AttemptState::Failed, Some("terminal_failure".into()))
                        }
                        agent_core::ResultClass::Cancelled => {
                            (AttemptState::Cancelled, Some("cancelled".into()))
                        }
                        agent_core::ResultClass::Unknown => {
                            (AttemptState::Unknown, Some("unknown".into()))
                        }
                    };
                    let mut evidence = std::collections::BTreeMap::from([
                        ("runtimeResultClass".into(), format!("{:?}", outcome.class)),
                        ("runtimeEventCount".into(), outcome.events.to_string()),
                    ]);
                    if let Some(error) = &outcome.error {
                        evidence.insert("runtimeError".into(), error.clone());
                    }
                    execution_runner_protocol::NormalizedExecutionResult {
                        execution_id: lease.lease.execution_id,
                        origin: lease.lease.origin.clone(),
                        subject: lease.lease.subject.clone(),
                        attempt: lease.lease.attempt,
                        state,
                        failure_class,
                        exit_code: Some(0),
                        signal: None,
                        started_at,
                        finished_at,
                        duration_ms: duration_millis(started_at, finished_at),
                        stdout: empty_output(),
                        stderr: empty_output(),
                        structured_output: outcome.output,
                        artifacts: Vec::new(),
                        backend_operation_id: operation_id.clone(),
                        cleanup_state: CleanupState::Confirmed,
                        policy_digest: lease.lease.policy_digest.clone(),
                        compatibility_digest: lease.lease.compatibility_digest.clone(),
                        definition_digest: lease.definition_digest.clone(),
                        command_template_digest: lease.command_template_digest.clone(),
                        retry_safety: RetrySafety::InspectRequired,
                        evidence,
                    }
                }
                Err(error) => {
                    let backend_error = if error.contains("deadline") {
                        BackendError::TimedOut(error)
                    } else if error.contains("cancelled") {
                        BackendError::Cancelled(error)
                    } else {
                        BackendError::Unknown(error)
                    };
                    let mut result = from_backend_error(
                        &lease,
                        operation_id.clone(),
                        started_at,
                        &backend_error,
                    );
                    result.cleanup_state = CleanupState::Confirmed;
                    result
                }
            };
            if let Some(error) = worker_error {
                result.evidence.insert("workerError".into(), error);
            }
            let terminal = TerminalLeaseResult {
                lease: lease.lease.clone(),
                result,
            };
            if let Err(error) = supervisor.journal.set_terminal(&terminal) {
                error!(execution_id = %lease.lease.execution_id, %error, "failed to persist agent worker terminal result");
                return;
            }
            supervisor.active.remove(&lease.lease.execution_id);
            supervisor
                .pending_results
                .insert(lease.lease.execution_id, terminal.clone());
            if let Err(error) = send_terminal(&outbound, terminal).await {
                warn!(execution_id = %lease.lease.execution_id, %error, "agent terminal result queued for reconnect recovery");
            }
        });
        Ok(())
    }

    async fn report_setup_failure(
        &self,
        lease: ExecuteLease,
        error: BackendError,
        outbound: mpsc::Sender<RunnerToController>,
    ) -> Result<(), String> {
        let operation_id = format!("runner-rejected:{}", lease.lease.execution_id);
        let mut result = from_backend_error(&lease, operation_id, Utc::now(), &error);
        result.cleanup_state = CleanupState::NotRequired;
        let terminal = TerminalLeaseResult {
            lease: lease.lease.clone(),
            result,
        };
        self.journal.set_terminal(&terminal)?;
        self.pending_results
            .insert(lease.lease.execution_id, terminal.clone());
        send_terminal(&outbound, terminal).await
    }

    pub async fn result_accepted(&self, accepted: &LeaseResultAccepted) -> Result<(), String> {
        let result = self
            .pending_results
            .get(&accepted.execution_id)
            .ok_or_else(|| "controller acknowledged an unknown terminal result".to_string())?;
        if result.lease.lease_id != accepted.lease_id
            || result.lease.fencing_token != accepted.fencing_token
            || result.result.state != accepted.state
        {
            return Err("controller terminal acknowledgement is stale or conflicting".to_string());
        }
        drop(result);
        self.journal
            .set_state(accepted.execution_id, JournalState::TerminalReported, None)?;
        self.journal
            .set_state(accepted.execution_id, JournalState::CleanupConfirmed, None)?;
        self.pending_results.remove(&accepted.execution_id);
        self.stager.cleanup(accepted.execution_id)?;
        Ok(())
    }

    /// Requests cancellation of the exact fenced lease named by the controller.
    ///
    /// A cancellation for an older lease or fencing token must never affect the
    /// execution currently occupying the same execution ID. Repeated cancellation
    /// of the same lease is intentionally idempotent, including after it has
    /// produced a terminal result.
    pub async fn cancel_lease(&self, cancellation: &CancelLease) -> Result<(), String> {
        let execution_id = cancellation.lease.execution_id;

        if let Some(active) = self.active.get(&execution_id) {
            validate_lease_identity(&active.lease.lease, &cancellation.lease, "cancellation")?;
            let operation_id = active.backend_operation_id.clone();
            let cancel = active.cancel.clone();
            drop(active);

            let _ = cancel.send(true);
            match self.backend.cancel(&operation_id).await {
                Ok(()) | Err(BackendError::NotFound(_)) => return Ok(()),
                Err(error) => return Err(format!("cancel backend operation: {error}")),
            }
        }

        if let Some(terminal) = self.pending_results.get(&execution_id) {
            validate_lease_identity(&terminal.lease, &cancellation.lease, "cancellation")?;
            return Ok(());
        }

        let record = self
            .journal
            .find(execution_id)?
            .ok_or_else(|| "controller cancelled an unknown execution".to_string())?;
        validate_lease_identity(&record.lease_context, &cancellation.lease, "cancellation")?;

        // A matching, acknowledged terminal record makes a delayed duplicate
        // cancellation harmless. Other non-active states require recovery before
        // cancellation can safely decide the backend operation's outcome.
        if matches!(
            record.state,
            JournalState::TerminalReported | JournalState::CleanupConfirmed
        ) {
            Ok(())
        } else {
            Err("matching execution is not active; reconcile runner recovery first".to_string())
        }
    }

    /// Replays the runner's authoritative state for an exact fenced lease.
    pub async fn reconcile_lease(
        &self,
        lease: &LeaseContext,
        outbound: &mpsc::Sender<RunnerToController>,
    ) -> Result<(), String> {
        if let Some(terminal) = self.pending_results.get(&lease.execution_id) {
            validate_lease_identity(&terminal.lease, lease, "reconciliation")?;
            let replay = terminal.clone();
            drop(terminal);
            return send_terminal(outbound, replay).await;
        }

        if let Some(active) = self.active.get(&lease.execution_id) {
            validate_lease_identity(&active.lease.lease, lease, "reconciliation")?;
            let active_lease = active.lease.lease.clone();
            drop(active);
            outbound
                .send(RunnerToController::RunnerLeaseAccepted(
                    active_lease.clone(),
                ))
                .await
                .map_err(|_| "controller outbound channel closed".to_string())?;
            outbound
                .send(RunnerToController::RunnerLeaseStarted(active_lease))
                .await
                .map_err(|_| "controller outbound channel closed".to_string())?;
            return Ok(());
        }

        let record = self
            .journal
            .find(lease.execution_id)?
            .ok_or_else(|| "controller reconciled an unknown execution".to_string())?;
        validate_lease_identity(&record.lease_context, lease, "reconciliation")?;
        Err("matching execution has no replayable in-memory state; run recovery first".to_string())
    }

    pub async fn resend_pending(&self, outbound: &mpsc::Sender<RunnerToController>) {
        let pending = self
            .pending_results
            .iter()
            .map(|entry| entry.clone())
            .collect::<Vec<_>>();
        for terminal in pending {
            if let Err(error) = send_terminal(outbound, terminal).await {
                warn!(%error, "failed to resend pending terminal result");
                break;
            }
        }
    }

    pub async fn recover(
        self: &Arc<Self>,
        outbound: &mpsc::Sender<RunnerToController>,
    ) -> Result<(), String> {
        for record in self.journal.unfinished()? {
            if self.pending_results.contains_key(&record.execution_id)
                || self.active.contains_key(&record.execution_id)
            {
                continue;
            }
            if let Some(terminal) = record.terminal_result.clone() {
                self.pending_results
                    .insert(record.execution_id, terminal.clone());
                send_terminal(outbound, terminal).await?;
                continue;
            }

            // The controller already accepted a TERMINAL_REPORTED result. A
            // crash in the tiny window before CLEANUP_CONFIRMED must finish
            // local reclamation, not invent and send a new UNKNOWN outcome.
            if record.state == JournalState::TerminalReported {
                if let Some(operation_id) = &record.backend_operation_id {
                    self.cleanup_with_retry(operation_id)
                        .await
                        .map_err(|error| format!("recover terminal cleanup: {error}"))?;
                }
                self.journal.set_state(
                    record.execution_id,
                    JournalState::CleanupConfirmed,
                    record.backend_operation_id.as_deref(),
                )?;
                continue;
            }

            let lease = recovery_lease(&record);
            if let Some(operation_id) = &record.backend_operation_id {
                match self.backend.inspect(operation_id).await {
                    Ok(inspection)
                        if matches!(
                            inspection.state,
                            BackendOperationState::Prepared | BackendOperationState::Running
                        ) =>
                    {
                        let _ = self.backend.cancel(operation_id).await;
                    }
                    Ok(_) | Err(_) => {}
                }
                let _ = self.cleanup_with_retry(operation_id).await;
            }
            let mut result = from_backend_error(
                &lease,
                record
                    .backend_operation_id
                    .clone()
                    .unwrap_or_else(|| format!("recovery:{}", record.execution_id)),
                Utc::now(),
                &BackendError::Unknown(
                    "runner restarted before a terminal result was acknowledged".to_string(),
                ),
            );
            result.cleanup_state = if record.backend_operation_id.is_some() {
                CleanupState::Confirmed
            } else {
                CleanupState::NotRequired
            };
            let terminal = TerminalLeaseResult {
                lease: record.lease_context.clone(),
                result,
            };
            self.journal.set_terminal(&terminal)?;
            self.pending_results
                .insert(record.execution_id, terminal.clone());
            send_terminal(outbound, terminal).await?;
        }
        Ok(())
    }

    /// Cleanup is a security operation, so transient backend errors receive a
    /// small bounded retry budget. A persistent failure remains durable in the
    /// terminal result and cleanup backlog for operator reconciliation.
    async fn cleanup_with_retry(
        &self,
        operation_id: &str,
    ) -> Result<execution_backend::CleanupEvidence, BackendError> {
        const ATTEMPTS: u32 = 3;
        let mut delay = Duration::from_millis(25);
        for attempt in 1..=ATTEMPTS {
            match self.backend.cleanup(operation_id).await {
                Ok(evidence) => return Ok(evidence),
                Err(error) if attempt == ATTEMPTS => return Err(error),
                Err(error) => {
                    warn!(%operation_id, attempt, %error, "backend cleanup failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2);
                }
            }
        }
        unreachable!("cleanup retry loop always returns")
    }

    pub async fn run_watchdog(self: Arc<Self>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(250));
        loop {
            ticker.tick().await;
            self.watchdog_last_tick_ms
                .store(Utc::now().timestamp_millis(), Ordering::Release);
            let expired = self
                .active
                .iter()
                .filter(|entry| entry.lease.lease.deadline <= Utc::now())
                .map(|entry| {
                    (
                        *entry.key(),
                        entry.backend_operation_id.clone(),
                        entry.cancel.clone(),
                    )
                })
                .collect::<Vec<_>>();
            for (execution_id, operation_id, cancel) in expired {
                let _ = cancel.send(true);
                if let Err(error) = self.backend.cancel(&operation_id).await {
                    warn!(%execution_id, %error, "watchdog backend cancellation failed");
                } else {
                    info!(%execution_id, "watchdog cancelled expired execution");
                }
            }
        }
    }
}

fn empty_output() -> NormalizedOutput {
    NormalizedOutput {
        inline: None,
        reference: None,
        truncated: false,
        original_bytes: 0,
    }
}

fn duration_millis(started_at: chrono::DateTime<Utc>, finished_at: chrono::DateTime<Utc>) -> u64 {
    u64::try_from(
        finished_at
            .signed_duration_since(started_at)
            .num_milliseconds()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

fn validate_command(
    lease: &ExecuteLease,
    allowed_digests: &std::collections::BTreeSet<String>,
) -> Result<CommandExecutionSpec, String> {
    let command = serde_json::from_value::<CommandExecutionSpec>(lease.command.clone())
        .map_err(|error| format!("invalid command execution spec: {error}"))?;
    if command.schema_version != 1
        || command.template_digest != lease.command_template_digest
        || !allowed_digests.contains(&command.template_digest)
    {
        return Err("command template digest is not admitted by the runner".to_string());
    }
    if command.network_enabled || command.credentials_enabled || command.persistent_workspace {
        return Err(
            "credential-free run.shell forbids network, credentials, and persistent workspace"
                .to_string(),
        );
    }
    if !command.executable.starts_with('/')
        || command.executable.contains(char::is_whitespace)
        || command.executable.contains('\0')
    {
        return Err("command executable must be an absolute literal path".to_string());
    }
    let executable = command
        .executable
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    // This denylist is defense-in-depth for operator mistakes, not the command
    // authority boundary. The exact canonical command-template digest above
    // must be reviewed and allowlisted; interpreter templates such as Python,
    // Node, Ruby, or Perl inherently permit arbitrary code within the selected
    // sandbox and therefore require the same scrutiny as a shell template.
    if matches!(
        executable.as_str(),
        "sh" | "bash" | "dash" | "zsh" | "fish" | "env"
    ) {
        return Err("shell interpreters and env launchers are forbidden".to_string());
    }
    if !command.working_directory.starts_with("/workspace")
        || command
            .working_directory
            .split('/')
            .any(|component| component == "..")
        || command.wall_clock_timeout_ms == 0
        || command.stdout_limit_bytes == 0
        || command.stderr_limit_bytes == 0
    {
        return Err("command path and limits are invalid".to_string());
    }
    for value in command.arguments.iter().chain(command.environment.values()) {
        if value.contains('\0')
            || ["$(", "`", ";", "&&", "||", "\n", "\r", ">", "<", "|"]
                .iter()
                .any(|marker| value.contains(marker))
            || value.contains("${")
        {
            return Err("command contains forbidden shell or expansion semantics".to_string());
        }
    }
    for (name, value) in &command.environment {
        if secret_shaped(name) || secret_shaped(value) {
            return Err(format!(
                "environment variable {name} looks credential-bearing"
            ));
        }
    }
    Ok(command)
}

fn secret_shaped(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "privatekey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
        || value.to_ascii_lowercase().contains("bearer ")
}

fn validate_lease_identity(
    authoritative: &LeaseContext,
    requested: &LeaseContext,
    operation: &str,
) -> Result<(), String> {
    if authoritative.execution_id != requested.execution_id
        || authoritative.lease_id != requested.lease_id
        || authoritative.fencing_token != requested.fencing_token
    {
        return Err(format!(
            "controller {operation} carries a stale or conflicting lease fence"
        ));
    }
    Ok(())
}

async fn send_terminal(
    outbound: &mpsc::Sender<RunnerToController>,
    terminal: TerminalLeaseResult,
) -> Result<(), String> {
    let message = match terminal.result.state {
        AttemptState::Succeeded => RunnerToController::RunnerLeaseSucceeded(terminal),
        AttemptState::Failed | AttemptState::TimedOut => {
            RunnerToController::RunnerLeaseFailed(terminal)
        }
        AttemptState::Cancelled => RunnerToController::RunnerLeaseCancelled(terminal),
        AttemptState::Unknown => RunnerToController::RunnerLeaseUnknown(terminal),
        other => return Err(format!("invalid terminal result state {other:?}")),
    };
    outbound
        .send(message)
        .await
        .map_err(|_| "controller outbound channel closed".to_string())
}

fn recovery_lease(record: &JournalRecord) -> ExecuteLease {
    ExecuteLease {
        lease: record.lease_context.clone(),
        backend_id: "mock".to_string(),
        execution_profile: serde_json::Value::Object(Default::default()),
        command: serde_json::Value::Object(Default::default()),
        inputs: Vec::new(),
        definition_digest: record.definition_digest.clone(),
        command_template_digest: record.command_template_digest.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use execution_backend_mock::{MockBehavior, MockExecutionBackend, MockOutcome};
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionSubject, LeaseId, OriginKind, SchedulingRequestId,
    };
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use uuid::Uuid;

    #[test]
    fn secret_shaped_environment_is_rejected() {
        assert!(secret_shaped("API_TOKEN"));
        assert!(secret_shaped("Bearer abc"));
        assert!(!secret_shaped("LANG"));
    }

    fn lease() -> ExecuteLease {
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "test".into(),
            template_version: 1,
            template_digest: "template".into(),
            executable: "/usr/bin/true".into(),
            arguments: Vec::new(),
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms: 5_000,
            stdout_limit_bytes: 1024,
            stderr_limit_bytes: 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        ExecuteLease {
            lease: LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Workflow,
                    service_id: "workflow".into(),
                    instance_id: "workflow-1".into(),
                    host_id: Uuid::nil(),
                },
                subject: ExecutionSubject::WorkflowTask {
                    subject_id: Uuid::new_v4(),
                    process_id: Uuid::new_v4(),
                    task_id: Uuid::new_v4(),
                },
                attempt: 1,
                lease_id: LeaseId::new(),
                fencing_token: 7,
                policy_digest: "policy".into(),
                compatibility_digest: "compat".into(),
                deadline: Utc::now() + ChronoDuration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::to_value(command).unwrap(),
            inputs: Vec::new(),
            definition_digest: "definition".into(),
            command_template_digest: "template".into(),
        }
    }

    fn supervisor(behavior: MockBehavior) -> (Arc<Supervisor>, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("runner-supervisor-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let journal = Journal::open(&root.join("journal.sqlite")).unwrap();
        let stager = InputStager::new(root.join("staging"), 1024).unwrap();
        let backend = Arc::new(MockExecutionBackend::new("compat", behavior));
        (
            Supervisor::new(
                backend,
                journal,
                stager,
                BTreeSet::from(["template".to_string()]),
                1,
                None,
            ),
            root,
        )
    }

    #[tokio::test]
    async fn stale_cancel_is_rejected_without_stopping_execution() {
        let behavior = MockBehavior {
            duration_ms: 100,
            outcome: MockOutcome::Success,
            ..MockBehavior::default()
        };
        let (supervisor, root) = supervisor(behavior);
        let lease = lease();
        let (outbound, mut messages) = mpsc::channel(8);
        supervisor
            .accept_execute(lease.clone(), outbound)
            .await
            .unwrap();
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseAccepted(_))
        ));
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseStarted(_))
        ));
        assert_eq!(
            supervisor.active_lease_contexts(),
            vec![lease.lease.clone()]
        );

        let mut stale = lease.lease.clone();
        stale.fencing_token -= 1;
        assert!(
            supervisor
                .cancel_lease(&CancelLease {
                    lease: stale,
                    reason: "stale request".into(),
                    grace_ms: 0,
                })
                .await
                .unwrap_err()
                .contains("stale or conflicting")
        );
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseSucceeded(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancel_and_reconcile_replay_cancelled_terminal_result() {
        let behavior = MockBehavior {
            duration_ms: 5_000,
            ..MockBehavior::default()
        };
        let (supervisor, root) = supervisor(behavior);
        let lease = lease();
        let (outbound, mut messages) = mpsc::channel(8);
        supervisor
            .accept_execute(lease.clone(), outbound)
            .await
            .unwrap();
        let _ = messages.recv().await;
        let _ = messages.recv().await;

        supervisor
            .cancel_lease(&CancelLease {
                lease: lease.lease.clone(),
                reason: "test cancellation".into(),
                grace_ms: 0,
            })
            .await
            .unwrap();
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseCancelled(_))
        ));

        let (replay_outbound, mut replay_messages) = mpsc::channel(1);
        supervisor
            .reconcile_lease(&lease.lease, &replay_outbound)
            .await
            .unwrap();
        assert!(matches!(
            replay_messages.recv().await,
            Some(RunnerToController::RunnerLeaseCancelled(_))
        ));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn watchdog_enforces_deadline_while_controller_is_disconnected() {
        let behavior = MockBehavior {
            duration_ms: 5_000,
            ..MockBehavior::default()
        };
        let (supervisor, root) = supervisor(behavior);
        let mut lease = lease();
        lease.lease.deadline = Utc::now() + ChronoDuration::milliseconds(350);
        let (outbound, mut messages) = mpsc::channel(8);
        supervisor
            .accept_execute(lease.clone(), outbound)
            .await
            .unwrap();
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseAccepted(_))
        ));
        assert!(matches!(
            messages.recv().await,
            Some(RunnerToController::RunnerLeaseStarted(_))
        ));
        drop(messages); // simulate loss of the SaaS/control-plane connection

        let watchdog = tokio::spawn(Arc::clone(&supervisor).run_watchdog());
        tokio::time::timeout(Duration::from_secs(2), async {
            while !supervisor
                .pending_results
                .contains_key(&lease.lease.execution_id)
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("deadline watchdog did not terminate execution");
        let terminal = supervisor
            .pending_results
            .get(&lease.lease.execution_id)
            .unwrap();
        assert_eq!(terminal.result.state, AttemptState::TimedOut);
        assert_eq!(
            terminal.result.failure_class.as_deref(),
            Some("deadline_exceeded")
        );
        watchdog.abort();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn restart_replays_exact_unacknowledged_terminal_result() {
        let (first, root) = supervisor(MockBehavior::default());
        let lease = lease();
        let (outbound, mut messages) = mpsc::channel(8);
        first.accept_execute(lease.clone(), outbound).await.unwrap();
        let _ = messages.recv().await;
        let _ = messages.recv().await;
        let original = match messages.recv().await.unwrap() {
            RunnerToController::RunnerLeaseSucceeded(terminal) => terminal,
            message => panic!("unexpected terminal message: {message:?}"),
        };
        drop(first);

        let restarted = Supervisor::new(
            Arc::new(MockExecutionBackend::new("compat", MockBehavior::default())),
            Journal::open(&root.join("journal.sqlite")).unwrap(),
            InputStager::new(root.join("staging"), 1024).unwrap(),
            BTreeSet::from(["template".to_string()]),
            1,
            None,
        );
        let (recovery_outbound, mut recovery_messages) = mpsc::channel(2);
        restarted.recover(&recovery_outbound).await.unwrap();
        let replayed = match recovery_messages.recv().await.unwrap() {
            RunnerToController::RunnerLeaseSucceeded(terminal) => terminal,
            message => panic!("unexpected recovery message: {message:?}"),
        };
        assert_eq!(replayed, original);
        assert_eq!(restarted.cleanup_backlog(), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistent_cleanup_failure_is_retried_and_remains_in_backlog() {
        let behavior = MockBehavior {
            cleanup_fails: true,
            ..MockBehavior::default()
        };
        let (supervisor, root) = supervisor(behavior);
        let lease = lease();
        let (outbound, mut messages) = mpsc::channel(8);
        let started = std::time::Instant::now();
        supervisor.accept_execute(lease, outbound).await.unwrap();
        let _ = messages.recv().await;
        let _ = messages.recv().await;
        let terminal = match messages.recv().await.unwrap() {
            RunnerToController::RunnerLeaseSucceeded(terminal) => terminal,
            message => panic!("unexpected terminal message: {message:?}"),
        };
        assert_eq!(terminal.result.cleanup_state, CleanupState::Failed);
        assert!(started.elapsed() >= Duration::from_millis(70));
        assert_eq!(supervisor.cleanup_backlog(), 1);
        let _ = fs::remove_dir_all(root);
    }
}
