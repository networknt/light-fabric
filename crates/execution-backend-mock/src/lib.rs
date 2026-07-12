use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use execution_backend::{
    BackendError, BackendOperationState, BackendOutput, CleanupEvidence, ExecutionBackend,
    Inspection, PreparedExecution, StagedInput,
};
use execution_runner_protocol::{BackendCapability, ExecuteLease, HostExposure, IsolationBoundary};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MockOutcome {
    Success,
    Failure,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockBehavior {
    pub duration_ms: u64,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub outcome: MockOutcome,
    pub lose_first_response: bool,
    pub cleanup_fails: bool,
    #[serde(default)]
    pub cleanup_failures: u32,
}

impl Default for MockBehavior {
    fn default() -> Self {
        Self {
            duration_ms: 10,
            stdout: "mock execution completed\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            outcome: MockOutcome::Success,
            lose_first_response: false,
            cleanup_fails: false,
            cleanup_failures: 0,
        }
    }
}

#[derive(Debug, Clone)]
struct Operation {
    state: BackendOperationState,
    output: Option<BackendOutput>,
    response_lost: bool,
}

#[derive(Clone)]
pub struct MockExecutionBackend {
    capability: BackendCapability,
    behavior: MockBehavior,
    operations: Arc<Mutex<HashMap<String, Operation>>>,
    cleanup_failures: Arc<Mutex<u32>>,
}

impl MockExecutionBackend {
    pub fn new(compatibility_digest: impl Into<String>, behavior: MockBehavior) -> Self {
        Self {
            capability: BackendCapability {
                backend_id: "mock".to_string(),
                backend_version: "1.0.0".to_string(),
                boundary: IsolationBoundary::Container,
                host_exposure: HostExposure::None,
                actions: vec!["run.shell".to_string()],
                features: Vec::new(),
                compatibility_digest: compatibility_digest.into(),
                healthy: true,
                available_slots: 1,
            },
            cleanup_failures: Arc::new(Mutex::new(behavior.cleanup_failures)),
            behavior,
            operations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn with_available_slots(mut self, slots: u32) -> Self {
        self.capability.available_slots = slots;
        self
    }
}

#[async_trait]
impl ExecutionBackend for MockExecutionBackend {
    fn capability(&self) -> BackendCapability {
        self.capability.clone()
    }

    fn validate(
        &self,
        lease: &ExecuteLease,
        staged_inputs: &[StagedInput],
    ) -> Result<(), BackendError> {
        if lease.backend_id != self.capability.backend_id {
            return Err(BackendError::InvalidRequest(format!(
                "lease selected backend {}, expected {}",
                lease.backend_id, self.capability.backend_id
            )));
        }
        if lease.lease.compatibility_digest != self.capability.compatibility_digest {
            return Err(BackendError::InvalidRequest(
                "lease compatibility digest does not match mock backend".to_string(),
            ));
        }
        if staged_inputs.iter().any(|input| !input.read_only) {
            return Err(BackendError::InvalidRequest(
                "mock backend accepts only read-only staged inputs".to_string(),
            ));
        }
        Ok(())
    }

    async fn prepare(
        &self,
        lease: &ExecuteLease,
        staged_inputs: &[StagedInput],
    ) -> Result<PreparedExecution, BackendError> {
        self.validate(lease, staged_inputs)?;
        let operation_id = format!("mock:{}", lease.lease.execution_id);
        self.operations
            .lock()
            .await
            .entry(operation_id.clone())
            .or_insert(Operation {
                state: BackendOperationState::Prepared,
                output: None,
                response_lost: false,
            });
        Ok(PreparedExecution {
            backend_operation_id: operation_id,
            execution_id: lease.lease.execution_id,
            backend_id: self.capability.backend_id.clone(),
            prepared_at: Utc::now(),
            evidence: BTreeMap::from([("backend".to_string(), "mock".to_string())]),
        })
    }

    async fn inspect(&self, operation_id: &str) -> Result<Inspection, BackendError> {
        let operations = self.operations.lock().await;
        let operation = operations
            .get(operation_id)
            .ok_or_else(|| BackendError::NotFound(operation_id.to_string()))?;
        Ok(Inspection {
            backend_operation_id: operation_id.to_string(),
            state: operation.state,
            observed_at: Utc::now(),
            evidence: BTreeMap::from([("backend".to_string(), "mock".to_string())]),
        })
    }

    async fn execute(
        &self,
        prepared: &PreparedExecution,
        lease: &ExecuteLease,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<BackendOutput, BackendError> {
        {
            let mut operations = self.operations.lock().await;
            let operation = operations
                .get_mut(&prepared.backend_operation_id)
                .ok_or_else(|| BackendError::NotFound(prepared.backend_operation_id.clone()))?;
            if let Some(output) = &operation.output {
                return Ok(output.clone());
            }
            if operation.state == BackendOperationState::Running {
                return Err(BackendError::Unknown(
                    "duplicate execute observed while operation is running".to_string(),
                ));
            }
            operation.state = BackendOperationState::Running;
        }

        let started_at = Utc::now();
        let until_deadline = lease
            .lease
            .deadline
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or_default();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(self.behavior.duration_ms)) => {}
            _ = tokio::time::sleep(until_deadline) => {
                self.cancel(&prepared.backend_operation_id).await?;
                return Err(BackendError::TimedOut("mock operation deadline expired".into()));
            }
            changed = cancellation.changed() => {
                if changed.is_ok() && *cancellation.borrow() {
                    self.cancel(&prepared.backend_operation_id).await?;
                    return Err(BackendError::Cancelled("mock operation cancelled".to_string()));
                }
            }
        }
        let finished_at = Utc::now();
        if self.behavior.outcome == MockOutcome::Unknown {
            let mut operations = self.operations.lock().await;
            if let Some(operation) = operations.get_mut(&prepared.backend_operation_id) {
                operation.state = BackendOperationState::Unknown;
            }
            return Err(BackendError::Unknown(
                "mock backend injected unknown outcome".to_string(),
            ));
        }
        let succeeded =
            self.behavior.outcome == MockOutcome::Success && self.behavior.exit_code == 0;
        let output = BackendOutput {
            exit_code: Some(self.behavior.exit_code),
            signal: None,
            stdout: self.behavior.stdout.as_bytes().to_vec(),
            stderr: self.behavior.stderr.as_bytes().to_vec(),
            structured_output: Some(serde_json::json!({
                "backend": "mock",
                "exitCode": self.behavior.exit_code
            })),
            started_at,
            finished_at,
            failure_class: (!succeeded).then(|| "non_zero_exit".to_string()),
            evidence: BTreeMap::from([("deterministic".to_string(), "true".to_string())]),
        };
        let lose_response = {
            let mut operations = self.operations.lock().await;
            let operation = operations
                .get_mut(&prepared.backend_operation_id)
                .ok_or_else(|| BackendError::NotFound(prepared.backend_operation_id.clone()))?;
            operation.state = if succeeded {
                BackendOperationState::Succeeded
            } else {
                BackendOperationState::Failed
            };
            operation.output = Some(output.clone());
            if self.behavior.lose_first_response && !operation.response_lost {
                operation.response_lost = true;
                true
            } else {
                false
            }
        };
        if lose_response {
            Err(BackendError::Transport(
                "mock backend injected lost response".to_string(),
            ))
        } else {
            Ok(output)
        }
    }

    async fn cancel(&self, operation_id: &str) -> Result<(), BackendError> {
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| BackendError::NotFound(operation_id.to_string()))?;
        if !matches!(
            operation.state,
            BackendOperationState::Succeeded
                | BackendOperationState::Failed
                | BackendOperationState::Cleaned
        ) {
            operation.state = BackendOperationState::Cancelled;
        }
        Ok(())
    }

    async fn cleanup(&self, operation_id: &str) -> Result<CleanupEvidence, BackendError> {
        if self.behavior.cleanup_fails {
            return Err(BackendError::Cleanup(
                "mock backend injected cleanup failure".to_string(),
            ));
        }
        {
            let mut remaining = self.cleanup_failures.lock().await;
            if *remaining > 0 {
                *remaining -= 1;
                return Err(BackendError::Cleanup(
                    "mock backend injected transient cleanup failure".to_string(),
                ));
            }
        }
        let mut operations = self.operations.lock().await;
        let operation = operations
            .get_mut(operation_id)
            .ok_or_else(|| BackendError::NotFound(operation_id.to_string()))?;
        operation.state = BackendOperationState::Cleaned;
        Ok(CleanupEvidence {
            backend_operation_id: operation_id.to_string(),
            cleaned_at: Utc::now(),
            evidence_reference: format!("mock-cleanup:{operation_id}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionId, ExecutionSubject, LeaseContext, LeaseId, OriginKind,
        SchedulingRequestId,
    };
    use uuid::Uuid;

    fn lease() -> ExecuteLease {
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
                fencing_token: 1,
                policy_digest: "policy".into(),
                compatibility_digest: "compat".into(),
                deadline: Utc::now() + ChronoDuration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::json!({}),
            inputs: Vec::new(),
            definition_digest: "definition".into(),
            command_template_digest: "template".into(),
        }
    }

    #[tokio::test]
    async fn duplicate_prepare_and_execute_use_one_operation() {
        let backend = MockExecutionBackend::new("compat", MockBehavior::default());
        let lease = lease();
        let first = backend.prepare(&lease, &[]).await.unwrap();
        let second = backend.prepare(&lease, &[]).await.unwrap();
        assert_eq!(first.backend_operation_id, second.backend_operation_id);
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let first_output = backend
            .execute(&first, &lease, cancel_rx.clone())
            .await
            .unwrap();
        let second_output = backend.execute(&first, &lease, cancel_rx).await.unwrap();
        assert_eq!(first_output, second_output);
    }

    #[tokio::test]
    async fn passes_shared_backend_conformance() {
        let backend = MockExecutionBackend::new("compat", MockBehavior::default());
        execution_backend_conformance::exercise_validation_guards(&backend, &lease(), &[])
            .await
            .unwrap();
        execution_backend_conformance::exercise_lifecycle(&backend, &lease(), &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn passes_shared_failure_unknown_and_cancellation_conformance() {
        let failure = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                outcome: MockOutcome::Failure,
                exit_code: 17,
                ..Default::default()
            },
        );
        execution_backend_conformance::exercise_nonzero_failure(&failure, &lease())
            .await
            .unwrap();

        let unknown = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                outcome: MockOutcome::Unknown,
                ..Default::default()
            },
        );
        execution_backend_conformance::exercise_unknown_outcome(&unknown, &lease())
            .await
            .unwrap();

        let cancellable = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                duration_ms: 250,
                ..Default::default()
            },
        );
        execution_backend_conformance::exercise_cancellation(&cancellable, &lease())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn passes_shared_recovery_conformance() {
        let lost = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                lose_first_response: true,
                ..Default::default()
            },
        );
        execution_backend_conformance::exercise_lost_terminal_response(&lost, &lease())
            .await
            .unwrap();

        let cleanup = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                cleanup_failures: 1,
                ..Default::default()
            },
        );
        execution_backend_conformance::exercise_cleanup_retry(&cleanup, &lease())
            .await
            .unwrap();

        let deadline = MockExecutionBackend::new(
            "compat",
            MockBehavior {
                duration_ms: 500,
                ..Default::default()
            },
        );
        let mut deadline_lease = lease();
        deadline_lease.lease.deadline = Utc::now() + ChronoDuration::milliseconds(50);
        execution_backend_conformance::exercise_deadline(&deadline, &deadline_lease)
            .await
            .unwrap();
    }
}
