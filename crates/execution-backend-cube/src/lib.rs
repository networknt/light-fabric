use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_backend::{
    BackendError, BackendOperationState, BackendOutput, CleanupEvidence, ExecutionBackend,
    Inspection, PreparedExecution, StagedInput,
};
use execution_runner_protocol::{
    ArtifactEvidence, BackendCapability, CommandExecutionSpec, ExecuteLease, HostExposure,
    IsolationBoundary,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::watch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CubeCreateRequest {
    pub idempotency_key: String,
    pub template_id: String,
    pub expires_at: DateTime<Utc>,
    pub deny_all_egress: bool,
    pub credentials_enabled: bool,
    pub tags: BTreeMap<String, String>,
    pub inputs: Vec<CubeInputMount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CubeInputMount {
    pub source: String,
    pub target: String,
    pub digest: String,
    pub read_only: bool,
    pub mount_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CubeResource {
    pub environment_id: String,
    pub idempotency_key: String,
    pub state: CubeState,
    pub expires_at: DateTime<Utc>,
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CubeState {
    Creating,
    Ready,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CubeCommandResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub evidence: BTreeMap<String, String>,
}

#[async_trait]
pub trait CubeApi: Send + Sync {
    async fn create(&self, request: CubeCreateRequest) -> Result<CubeResource, BackendError>;
    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<CubeResource>, BackendError>;
    async fn inspect(&self, environment_id: &str) -> Result<Option<CubeResource>, BackendError>;
    async fn execute(
        &self,
        environment_id: &str,
        command: &CommandExecutionSpec,
    ) -> Result<CubeCommandResult, BackendError>;
    async fn cancel(&self, environment_id: &str) -> Result<(), BackendError>;
    async fn artifacts(&self, environment_id: &str) -> Result<Vec<ArtifactEvidence>, BackendError>;
    async fn delete(&self, environment_id: &str) -> Result<(), BackendError>;
    async fn discover_owned(
        &self,
        owner_runner: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<(Vec<CubeResource>, Option<String>), BackendError>;
}

#[derive(Debug, Clone)]
pub struct CubeBackendConfig {
    pub template_id: String,
    pub compatibility_digest: String,
    pub owner_runner: String,
    pub available_slots: u32,
    pub maximum_native_ttl_seconds: u64,
    pub discovery_page_limit: usize,
}

pub struct CubeExecutionBackend<C> {
    client: Arc<C>,
    config: CubeBackendConfig,
}

impl<C> CubeExecutionBackend<C> {
    pub fn new(client: Arc<C>, config: CubeBackendConfig) -> Self {
        Self { client, config }
    }

    pub async fn discover_all_owned_bounded(
        &self,
        maximum_pages: usize,
    ) -> Result<Vec<CubeResource>, BackendError>
    where
        C: CubeApi,
    {
        let mut cursor = None;
        let mut resources = Vec::new();
        for _ in 0..maximum_pages {
            let (mut page, next) = self
                .client
                .discover_owned(
                    &self.config.owner_runner,
                    cursor.as_deref(),
                    self.config.discovery_page_limit,
                )
                .await?;
            resources.append(&mut page);
            if next.is_none() {
                return Ok(resources);
            }
            cursor = next;
        }
        Err(BackendError::Unknown(
            "Cube owned-resource discovery exceeded its bounded pagination limit".into(),
        ))
    }

    fn tags(&self, lease: &ExecuteLease) -> BTreeMap<String, String> {
        let mut tags = BTreeMap::from([
            ("light.host".into(), lease.lease.origin.host_id.to_string()),
            (
                "light.execution".into(),
                lease.lease.execution_id.to_string(),
            ),
            (
                "light.subject".into(),
                lease.lease.subject.subject_id().to_string(),
            ),
            ("light.attempt".into(), lease.lease.attempt.to_string()),
            ("light.policy".into(), lease.lease.policy_digest.clone()),
            ("light.runner".into(), self.config.owner_runner.clone()),
            ("light.expires".into(), lease.lease.deadline.to_rfc3339()),
        ]);
        match &lease.lease.subject {
            execution_runner_protocol::ExecutionSubject::WorkflowTask {
                process_id,
                task_id,
                ..
            } => {
                tags.insert("light.process".into(), process_id.to_string());
                tags.insert("light.task".into(), task_id.to_string());
            }
            execution_runner_protocol::ExecutionSubject::AgentTurn {
                session_id,
                turn_id,
                ..
            } => {
                tags.insert("light.agentSession".into(), session_id.to_string());
                tags.insert("light.agentTurn".into(), turn_id.to_string());
            }
            execution_runner_protocol::ExecutionSubject::AgentAction {
                session_id,
                turn_id,
                action_id,
                ..
            } => {
                tags.insert("light.agentSession".into(), session_id.to_string());
                tags.insert("light.agentTurn".into(), turn_id.to_string());
                tags.insert("light.agentAction".into(), action_id.to_string());
            }
        }
        tags
    }
}

#[async_trait]
impl<C: CubeApi + 'static> ExecutionBackend for CubeExecutionBackend<C> {
    fn capability(&self) -> BackendCapability {
        BackendCapability {
            backend_id: "cube".into(),
            backend_version: "e2b-compatible-v2".into(),
            boundary: IsolationBoundary::MicroVm,
            host_exposure: HostExposure::None,
            actions: vec!["run.shell".into(), "agent.runtime".into()],
            features: vec![
                "deny-all-egress".into(),
                "native-ttl".into(),
                "idempotent-create".into(),
                "bounded-tag-discovery".into(),
                "artifacts".into(),
            ],
            compatibility_digest: self.config.compatibility_digest.clone(),
            healthy: true,
            available_slots: self.config.available_slots,
        }
    }

    fn validate(&self, lease: &ExecuteLease, staged: &[StagedInput]) -> Result<(), BackendError> {
        if lease.backend_id != "cube" {
            return Err(BackendError::InvalidRequest(
                "lease did not select Cube backend".into(),
            ));
        }
        if lease.lease.compatibility_digest != self.config.compatibility_digest {
            return Err(BackendError::InvalidRequest(
                "Cube compatibility digest mismatch".into(),
            ));
        }
        let command: CommandExecutionSpec = serde_json::from_value(lease.command.clone())
            .map_err(|e| BackendError::InvalidRequest(format!("invalid Cube command: {e}")))?;
        if command.network_enabled || command.credentials_enabled || command.persistent_workspace {
            return Err(BackendError::Unsupported(
                "Cube foundation permits only deny-egress, credential-free, ephemeral executions"
                    .into(),
            ));
        }
        if staged.iter().any(|input| !input.read_only) {
            return Err(BackendError::InvalidRequest(
                "Cube inputs must be immutable".into(),
            ));
        }
        let remaining = lease
            .lease
            .deadline
            .signed_duration_since(Utc::now())
            .num_seconds();
        if remaining <= 0 || remaining as u64 > self.config.maximum_native_ttl_seconds {
            return Err(BackendError::InvalidRequest(
                "lease deadline cannot be enforced by configured Cube native TTL".into(),
            ));
        }
        Ok(())
    }

    async fn prepare(
        &self,
        lease: &ExecuteLease,
        staged: &[StagedInput],
    ) -> Result<PreparedExecution, BackendError> {
        self.validate(lease, staged)?;
        let key = format!("light:{}", lease.lease.execution_id);
        if let Some(found) = self.client.find_by_idempotency_key(&key).await? {
            return prepared(lease, found);
        }
        let request = CubeCreateRequest {
            idempotency_key: key.clone(),
            template_id: self.config.template_id.clone(),
            expires_at: lease.lease.deadline,
            deny_all_egress: true,
            credentials_enabled: false,
            tags: self.tags(lease),
            inputs: staged
                .iter()
                .map(|input| CubeInputMount {
                    source: input.local_path.display().to_string(),
                    target: input.mount_target.clone(),
                    digest: input.source_digest.clone(),
                    read_only: true,
                    mount_options: input.mount_options.clone(),
                })
                .collect(),
        };
        match self.client.create(request).await {
            Ok(resource) => prepared(lease, resource),
            Err(BackendError::Transport(_)) => self.client.find_by_idempotency_key(&key).await?.map(|resource| prepared(lease, resource)).transpose()?.ok_or_else(|| BackendError::Unknown("Cube create response was lost and bounded idempotency lookup found no resource".into())),
            Err(error) => Err(error),
        }
    }

    async fn inspect(&self, id: &str) -> Result<Inspection, BackendError> {
        let resource = self
            .client
            .inspect(id)
            .await?
            .ok_or_else(|| BackendError::NotFound(id.into()))?;
        Ok(Inspection {
            backend_operation_id: id.into(),
            state: map_state(resource.state),
            observed_at: Utc::now(),
            evidence: resource.tags,
        })
    }

    async fn execute(
        &self,
        prepared: &PreparedExecution,
        lease: &ExecuteLease,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<BackendOutput, BackendError> {
        let command: CommandExecutionSpec = serde_json::from_value(lease.command.clone())
            .map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
        tokio::select! {
            result = self.client.execute(&prepared.backend_operation_id, &command) => {
                let result = result?; let failed = result.exit_code != 0;
                Ok(BackendOutput { exit_code: Some(result.exit_code), signal: None, stdout: result.stdout, stderr: result.stderr, structured_output: None, started_at: result.started_at, finished_at: result.finished_at, failure_class: failed.then(|| "non_zero_exit".into()), evidence: result.evidence })
            }
            changed = cancellation.changed() => { if changed.is_ok() && *cancellation.borrow() { self.client.cancel(&prepared.backend_operation_id).await?; Err(BackendError::Cancelled("Cube execution cancelled".into())) } else { Err(BackendError::Unknown("Cube cancellation channel closed".into())) } }
        }
    }

    async fn cancel(&self, id: &str) -> Result<(), BackendError> {
        self.client.cancel(id).await
    }
    async fn collect_artifacts(&self, id: &str) -> Result<Vec<ArtifactEvidence>, BackendError> {
        self.client.artifacts(id).await
    }
    async fn cleanup(&self, id: &str) -> Result<CleanupEvidence, BackendError> {
        if self.client.inspect(id).await?.is_some() {
            self.client.delete(id).await?;
        }
        Ok(CleanupEvidence {
            backend_operation_id: id.into(),
            cleaned_at: Utc::now(),
            evidence_reference: format!("cube:deleted:{id}"),
        })
    }
}

fn prepared(
    lease: &ExecuteLease,
    resource: CubeResource,
) -> Result<PreparedExecution, BackendError> {
    Ok(PreparedExecution {
        backend_operation_id: resource.environment_id,
        execution_id: lease.lease.execution_id,
        backend_id: "cube".into(),
        prepared_at: Utc::now(),
        evidence: resource.tags,
    })
}
fn map_state(state: CubeState) -> BackendOperationState {
    match state {
        CubeState::Creating => BackendOperationState::Prepared,
        CubeState::Ready => BackendOperationState::Prepared,
        CubeState::Running => BackendOperationState::Running,
        CubeState::Succeeded => BackendOperationState::Succeeded,
        CubeState::Failed => BackendOperationState::Failed,
        CubeState::Cancelled => BackendOperationState::Cancelled,
        CubeState::Unknown => BackendOperationState::Unknown,
        CubeState::Deleted => BackendOperationState::Cleaned,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionId, ExecutionSubject, LeaseContext, LeaseId, OriginKind,
        SchedulingRequestId,
    };
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Default)]
    struct FakeCube {
        resource: Mutex<Option<CubeResource>>,
        creates: Mutex<usize>,
        lose_first_response: bool,
    }

    #[async_trait]
    impl CubeApi for FakeCube {
        async fn create(&self, request: CubeCreateRequest) -> Result<CubeResource, BackendError> {
            *self.creates.lock().unwrap() += 1;
            let resource = CubeResource {
                environment_id: "cube-1".into(),
                idempotency_key: request.idempotency_key,
                state: CubeState::Ready,
                expires_at: request.expires_at,
                tags: request.tags,
            };
            *self.resource.lock().unwrap() = Some(resource.clone());
            if self.lose_first_response {
                Err(BackendError::Transport("lost response".into()))
            } else {
                Ok(resource)
            }
        }
        async fn find_by_idempotency_key(
            &self,
            key: &str,
        ) -> Result<Option<CubeResource>, BackendError> {
            Ok(self
                .resource
                .lock()
                .unwrap()
                .clone()
                .filter(|v| v.idempotency_key == key))
        }
        async fn inspect(&self, id: &str) -> Result<Option<CubeResource>, BackendError> {
            Ok(self
                .resource
                .lock()
                .unwrap()
                .clone()
                .filter(|v| v.environment_id == id))
        }
        async fn execute(
            &self,
            _: &str,
            _: &CommandExecutionSpec,
        ) -> Result<CubeCommandResult, BackendError> {
            let now = Utc::now();
            Ok(CubeCommandResult {
                exit_code: 0,
                stdout: b"ok".to_vec(),
                stderr: vec![],
                started_at: now,
                finished_at: now,
                evidence: BTreeMap::new(),
            })
        }
        async fn cancel(&self, _: &str) -> Result<(), BackendError> {
            Ok(())
        }
        async fn artifacts(&self, _: &str) -> Result<Vec<ArtifactEvidence>, BackendError> {
            Ok(vec![])
        }
        async fn delete(&self, _: &str) -> Result<(), BackendError> {
            self.resource.lock().unwrap().take();
            Ok(())
        }
        async fn discover_owned(
            &self,
            owner: &str,
            _: Option<&str>,
            _: usize,
        ) -> Result<(Vec<CubeResource>, Option<String>), BackendError> {
            Ok((
                self.resource
                    .lock()
                    .unwrap()
                    .clone()
                    .filter(|v| v.tags.get("light.runner").map(String::as_str) == Some(owner))
                    .into_iter()
                    .collect(),
                None,
            ))
        }
    }

    fn lease() -> ExecuteLease {
        ExecuteLease {
            lease: LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Workflow,
                    service_id: "workflow".into(),
                    instance_id: "one".into(),
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
                deadline: Utc::now() + Duration::minutes(1),
            },
            backend_id: "cube".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::to_value(CommandExecutionSpec {
                schema_version: 1,
                template_id: "test".into(),
                template_version: 1,
                template_digest: "template".into(),
                executable: "true".into(),
                arguments: vec![],
                working_directory: "/workspace".into(),
                environment: BTreeMap::new(),
                wall_clock_timeout_ms: 1000,
                stdout_limit_bytes: 1024,
                stderr_limit_bytes: 1024,
                network_enabled: false,
                credentials_enabled: false,
                persistent_workspace: false,
            })
            .unwrap(),
            inputs: vec![],
            definition_digest: "definition".into(),
            command_template_digest: "template".into(),
        }
    }

    fn backend(api: Arc<FakeCube>) -> CubeExecutionBackend<FakeCube> {
        CubeExecutionBackend::new(
            api,
            CubeBackendConfig {
                template_id: "immutable-template".into(),
                compatibility_digest: "compat".into(),
                owner_runner: "runner-1".into(),
                available_slots: 1,
                maximum_native_ttl_seconds: 300,
                discovery_page_limit: 20,
            },
        )
    }

    #[tokio::test]
    async fn lost_create_response_is_rediscovered_and_cleanup_is_idempotent() {
        let api = Arc::new(FakeCube {
            lose_first_response: true,
            ..Default::default()
        });
        let backend = backend(Arc::clone(&api));
        let lease = lease();
        let first = backend.prepare(&lease, &[]).await.unwrap();
        let second = backend.prepare(&lease, &[]).await.unwrap();
        assert_eq!(first.backend_operation_id, second.backend_operation_id);
        assert_eq!(*api.creates.lock().unwrap(), 1);
        assert_eq!(
            backend.discover_all_owned_bounded(1).await.unwrap().len(),
            1
        );
        backend.cleanup(&first.backend_operation_id).await.unwrap();
        backend.cleanup(&first.backend_operation_id).await.unwrap();
    }

    #[test]
    fn rejects_network_credentials_and_unenforceable_ttl() {
        let backend = backend(Arc::new(FakeCube::default()));
        let mut lease = lease();
        lease.lease.deadline = Utc::now() + Duration::minutes(10);
        assert!(matches!(
            backend.validate(&lease, &[]),
            Err(BackendError::InvalidRequest(_))
        ));
    }
}
