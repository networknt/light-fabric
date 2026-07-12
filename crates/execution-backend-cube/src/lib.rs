use async_trait::async_trait;
use chrono::{DateTime, Utc};
use coding_agent_runtime::{
    CodingFixtureOutput, CodingFixtureRequest, CodingTurnSpec, PI_RPC_ADAPTER_ID,
    PI_RPC_ADAPTER_VERSION, validate_patch,
};
use execution_backend::{
    BackendError, BackendOperationState, BackendOutput, CleanupEvidence, ExecutionBackend,
    Inspection, PreparedExecution, StagedInput,
};
use execution_runner_protocol::{
    ArtifactEvidence, BackendCapability, CommandExecutionSpec, ExecuteLease, HostExposure,
    IsolationBoundary,
};
use execution_security::ProtectedPathPolicy;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
use tokio::sync::watch;

mod http;
pub use http::{CubeHttpClient, CubeHttpClientConfig};

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
    async fn stage_inputs(
        &self,
        environment_id: &str,
        inputs: &[CubeInputMount],
    ) -> Result<(), BackendError>;
    async fn inspect(&self, environment_id: &str) -> Result<Option<CubeResource>, BackendError>;
    async fn execute(
        &self,
        environment_id: &str,
        command: &CommandExecutionSpec,
    ) -> Result<CubeCommandResult, BackendError>;
    async fn set_timeout(
        &self,
        environment_id: &str,
        timeout_seconds: u64,
    ) -> Result<(), BackendError>;
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

impl CubeBackendConfig {
    pub fn capability(&self) -> BackendCapability {
        BackendCapability {
            backend_id: "cube".into(),
            backend_version: "cube-e2b-connect-v1".into(),
            boundary: IsolationBoundary::MicroVm,
            host_exposure: HostExposure::None,
            actions: vec![
                "run.shell".into(),
                "coding.fixture".into(),
                "coding.pi-rpc-v1".into(),
            ],
            features: vec![
                "deny-all-egress".into(),
                "native-ttl".into(),
                "bounded-metadata-recovery".into(),
                "immutable-repository-upload".into(),
                "canonical-patch-output".into(),
                "pi-rpc-v1".into(),
                "bounded-tag-discovery".into(),
            ],
            compatibility_digest: self.compatibility_digest.clone(),
            healthy: true,
            available_slots: self.available_slots,
        }
    }
}

#[async_trait]
impl<C: CubeApi + 'static> ExecutionBackend for CubeExecutionBackend<C> {
    fn capability(&self) -> BackendCapability {
        self.config.capability()
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
        if matches!(
            command.executable.as_str(),
            "/usr/local/bin/light-coding-agent-fixture" | "/usr/local/bin/light-pi-rpc-adapter"
        ) {
            let repository_digest = coding_spec(&command)?.repository_digest;
            if staged.len() != 1
                || staged[0].mount_target != "/inputs/repository.bundle"
                || staged[0].media_type != "application/x-git-bundle"
                || staged[0].source_digest != repository_digest
                || staged[0].executable
                || !staged[0]
                    .mount_options
                    .iter()
                    .any(|option| option == "noexec")
            {
                return Err(BackendError::InvalidRequest(
                    "coding fixture requires exactly one immutable non-executable Git bundle"
                        .into(),
                ));
            }
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
        let inputs = staged
            .iter()
            .map(|input| CubeInputMount {
                source: input.local_path.display().to_string(),
                target: input.mount_target.clone(),
                digest: input.source_digest.clone(),
                read_only: true,
                mount_options: input.mount_options.clone(),
            })
            .collect::<Vec<_>>();
        if let Some(found) = self.client.find_by_idempotency_key(&key).await? {
            self.client
                .stage_inputs(&found.environment_id, &inputs)
                .await?;
            return prepared(lease, found);
        }
        let mut tags = self.tags(lease);
        tags.insert("light.idempotency".into(), key.clone());
        let request = CubeCreateRequest {
            idempotency_key: key.clone(),
            template_id: self.config.template_id.clone(),
            expires_at: lease.lease.deadline,
            deny_all_egress: true,
            credentials_enabled: false,
            tags,
            inputs: inputs.clone(),
        };
        match self.client.create(request).await {
            Ok(resource) => {
                if let Err(error) = self
                    .client
                    .stage_inputs(&resource.environment_id, &inputs)
                    .await
                {
                    let _ = self.client.delete(&resource.environment_id).await;
                    return Err(error);
                }
                prepared(lease, resource)
            }
            Err(BackendError::Transport(_)) => {
                let resource = self.client.find_by_idempotency_key(&key).await?.ok_or_else(|| BackendError::Unknown("Cube create response was lost and bounded idempotency lookup found no resource".into()))?;
                self.client
                    .stage_inputs(&resource.environment_id, &inputs)
                    .await?;
                prepared(lease, resource)
            }
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
        let remaining = lease
            .lease
            .deadline
            .signed_duration_since(Utc::now())
            .num_seconds();
        if remaining <= 0 {
            return Err(BackendError::TimedOut("Cube lease deadline expired".into()));
        }
        self.client
            .set_timeout(&prepared.backend_operation_id, remaining as u64)
            .await?;
        tokio::select! {
            result = self.client.execute(&prepared.backend_operation_id, &command) => {
                let result = result?; let failed = result.exit_code != 0;
                let structured_output = if failed { None } else { validate_coding_fixture_output(lease, &result.stdout)? };
                Ok(BackendOutput { exit_code: Some(result.exit_code), signal: None, stdout: result.stdout, stderr: result.stderr, structured_output, started_at: result.started_at, finished_at: result.finished_at, failure_class: failed.then(|| "non_zero_exit".into()), evidence: result.evidence })
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

    async fn reconcile_orphans(
        &self,
        retained_operation_ids: &BTreeSet<String>,
        now: DateTime<Utc>,
    ) -> Result<Vec<CleanupEvidence>, BackendError> {
        let resources = self.discover_all_owned_bounded(10).await?;
        let mut cleaned = Vec::new();
        for resource in resources {
            if retained_operation_ids.contains(&resource.environment_id)
                || resource.expires_at > now
                || resource.tags.get("light.runner").map(String::as_str)
                    != Some(self.config.owner_runner.as_str())
            {
                continue;
            }
            self.client.delete(&resource.environment_id).await?;
            cleaned.push(CleanupEvidence {
                backend_operation_id: resource.environment_id.clone(),
                cleaned_at: Utc::now(),
                evidence_reference: format!("cube:orphan-deleted:{}", resource.environment_id),
            });
        }
        Ok(cleaned)
    }
}

fn validate_coding_fixture_output(
    lease: &ExecuteLease,
    stdout: &[u8],
) -> Result<Option<serde_json::Value>, BackendError> {
    let command: CommandExecutionSpec =
        serde_json::from_value(lease.command.clone()).map_err(|error| {
            BackendError::InvalidRequest(format!("invalid coding command: {error}"))
        })?;
    if !matches!(
        command.executable.as_str(),
        "/usr/local/bin/light-coding-agent-fixture" | "/usr/local/bin/light-pi-rpc-adapter"
    ) {
        return Ok(None);
    }
    let spec = coding_spec(&command)?;
    let output: CodingFixtureOutput = serde_json::from_slice(stdout).map_err(|error| {
        BackendError::Unknown(format!("invalid coding fixture output: {error}"))
    })?;
    let expected_adapter = if command.executable == "/usr/local/bin/light-pi-rpc-adapter" {
        (PI_RPC_ADAPTER_ID, PI_RPC_ADAPTER_VERSION)
    } else {
        ("cube-coding-fixture", "1")
    };
    if output.adapter_id != expected_adapter.0
        || output.adapter_version != expected_adapter.1
        || output.repository_digest != spec.repository_digest
    {
        return Err(BackendError::Unknown(
            "coding fixture identity or repository digest mismatch".into(),
        ));
    }
    let validated = validate_patch(
        &spec,
        &ProtectedPathPolicy::default_deny(),
        &output.base_revision,
        &output.patch,
        &output.changed_paths,
    )
    .map_err(|error| BackendError::InvalidRequest(format!("coding patch rejected: {error}")))?;
    serde_json::to_value(validated)
        .map(Some)
        .map_err(|error| BackendError::Unknown(format!("serialize validated patch: {error}")))
}

fn coding_spec(command: &CommandExecutionSpec) -> Result<CodingTurnSpec, BackendError> {
    if command.executable == "/usr/local/bin/light-coding-agent-fixture" {
        return coding_fixture_request(command).map(|request| request.spec);
    }
    if command.executable == "/usr/local/bin/light-pi-rpc-adapter"
        && command.arguments.len() == 12
        && command.arguments[0] == "--repository"
        && command.arguments[1] == "/inputs/repository.bundle"
        && command.arguments[2] == "--request-base64"
        && command.arguments[4] == "--pi"
        && command.arguments[5] == "/usr/local/bin/pi"
        && command.arguments[6] == "--pi-digest"
        && command.arguments[8] == "--provider"
        && command.arguments[10] == "--model"
    {
        return CodingTurnSpec::decode_argument(&command.arguments[3]).map_err(|error| {
            BackendError::InvalidRequest(format!("invalid Pi coding request: {error}"))
        });
    }
    Err(BackendError::InvalidRequest(
        "coding adapter executable or arguments do not match an admitted contract".into(),
    ))
}

fn coding_fixture_request(
    command: &CommandExecutionSpec,
) -> Result<CodingFixtureRequest, BackendError> {
    if command.arguments.len() != 4
        || command.arguments[0] != "--repository"
        || command.arguments[1] != "/inputs/repository.bundle"
        || command.arguments[2] != "--request-base64"
    {
        return Err(BackendError::InvalidRequest(
            "coding fixture arguments do not match the admitted contract".into(),
        ));
    }
    let request =
        CodingFixtureRequest::decode_argument(&command.arguments[3]).map_err(|error| {
            BackendError::InvalidRequest(format!("invalid coding fixture request: {error}"))
        })?;
    Ok(request)
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
    use coding_agent_runtime::{CodingFixtureRequest, CodingTurnSpec};
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionId, ExecutionSubject, LeaseContext, LeaseId, OriginKind,
        SchedulingRequestId,
    };
    use sha2::Digest;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use uuid::Uuid;

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    enum FakeOutcome {
        #[default]
        Success,
        Failure,
        Unknown,
    }

    #[derive(Default)]
    struct FakeCube {
        resource: Mutex<Option<CubeResource>>,
        creates: Mutex<usize>,
        stages: Mutex<usize>,
        result: Mutex<Option<CubeCommandResult>>,
        outcome: FakeOutcome,
        duration_ms: u64,
        lose_first_response: bool,
        lose_execute_response: bool,
        execute_response_lost: Mutex<bool>,
        timeout_seconds: Mutex<Option<u64>>,
        delete_failures: Mutex<u32>,
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
        async fn stage_inputs(&self, _: &str, _: &[CubeInputMount]) -> Result<(), BackendError> {
            *self.stages.lock().unwrap() += 1;
            Ok(())
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
            if let Some(result) = self.result.lock().unwrap().clone() {
                return Ok(result);
            }
            if self.duration_ms > 0 {
                let timeout = { *self.timeout_seconds.lock().unwrap() };
                let duration =
                    tokio::time::sleep(std::time::Duration::from_millis(self.duration_ms));
                tokio::pin!(duration);
                if let Some(timeout) = timeout {
                    tokio::select! {
                        _ = &mut duration => {}
                        _ = tokio::time::sleep(std::time::Duration::from_secs(timeout)) => {
                            if let Some(resource) = self.resource.lock().unwrap().as_mut() {
                                resource.state = CubeState::Unknown;
                            }
                            return Err(BackendError::TimedOut("injected Cube native timeout".into()));
                        }
                    }
                } else {
                    duration.await;
                }
            }
            if self.outcome == FakeOutcome::Unknown {
                if let Some(resource) = self.resource.lock().unwrap().as_mut() {
                    resource.state = CubeState::Unknown;
                }
                return Err(BackendError::Unknown(
                    "injected unknown Cube outcome".into(),
                ));
            }
            let now = Utc::now();
            let failed = self.outcome == FakeOutcome::Failure;
            let result = CubeCommandResult {
                exit_code: if failed { 17 } else { 0 },
                stdout: if failed { vec![] } else { b"ok".to_vec() },
                stderr: if failed { b"failure".to_vec() } else { vec![] },
                started_at: now,
                finished_at: now,
                evidence: BTreeMap::new(),
            };
            if let Some(resource) = self.resource.lock().unwrap().as_mut() {
                resource.state = if failed {
                    CubeState::Failed
                } else {
                    CubeState::Succeeded
                };
            }
            *self.result.lock().unwrap() = Some(result.clone());
            if self.lose_execute_response {
                let mut lost = self.execute_response_lost.lock().unwrap();
                if !*lost {
                    *lost = true;
                    return Err(BackendError::Transport(
                        "injected lost Cube execute response".into(),
                    ));
                }
            }
            Ok(result)
        }
        async fn set_timeout(&self, _: &str, timeout: u64) -> Result<(), BackendError> {
            *self.timeout_seconds.lock().unwrap() = Some(timeout);
            Ok(())
        }
        async fn cancel(&self, _: &str) -> Result<(), BackendError> {
            if let Some(resource) = self.resource.lock().unwrap().as_mut() {
                resource.state = CubeState::Cancelled;
            }
            Ok(())
        }
        async fn artifacts(&self, _: &str) -> Result<Vec<ArtifactEvidence>, BackendError> {
            Ok(vec![])
        }
        async fn delete(&self, _: &str) -> Result<(), BackendError> {
            let mut failures = self.delete_failures.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                return Err(BackendError::Cleanup(
                    "injected transient Cube delete failure".into(),
                ));
            }
            drop(failures);
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
        assert_eq!(*api.stages.lock().unwrap(), 2);
        assert_eq!(
            backend.discover_all_owned_bounded(1).await.unwrap().len(),
            1
        );
        backend.cleanup(&first.backend_operation_id).await.unwrap();
        backend.cleanup(&first.backend_operation_id).await.unwrap();
    }

    #[tokio::test]
    async fn passes_shared_backend_conformance() {
        let backend = backend(Arc::new(FakeCube::default()));
        execution_backend_conformance::exercise_validation_guards(&backend, &lease(), &[])
            .await
            .unwrap();
        execution_backend_conformance::exercise_lifecycle(&backend, &lease(), &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn passes_shared_failure_unknown_and_cancellation_conformance() {
        let failure = backend(Arc::new(FakeCube {
            outcome: FakeOutcome::Failure,
            ..Default::default()
        }));
        execution_backend_conformance::exercise_nonzero_failure(&failure, &lease())
            .await
            .unwrap();

        let unknown = backend(Arc::new(FakeCube {
            outcome: FakeOutcome::Unknown,
            ..Default::default()
        }));
        execution_backend_conformance::exercise_unknown_outcome(&unknown, &lease())
            .await
            .unwrap();

        let cancellable = backend(Arc::new(FakeCube {
            duration_ms: 250,
            ..Default::default()
        }));
        execution_backend_conformance::exercise_cancellation(&cancellable, &lease())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn passes_shared_recovery_conformance() {
        let lost = backend(Arc::new(FakeCube {
            lose_execute_response: true,
            ..Default::default()
        }));
        execution_backend_conformance::exercise_lost_terminal_response(&lost, &lease())
            .await
            .unwrap();

        let cleanup = backend(Arc::new(FakeCube {
            delete_failures: Mutex::new(1),
            ..Default::default()
        }));
        execution_backend_conformance::exercise_cleanup_retry(&cleanup, &lease())
            .await
            .unwrap();

        let deadline = backend(Arc::new(FakeCube {
            duration_ms: 1_500,
            ..Default::default()
        }));
        let mut deadline_lease = lease();
        deadline_lease.lease.deadline = Utc::now() + Duration::milliseconds(1_100);
        execution_backend_conformance::exercise_deadline(&deadline, &deadline_lease)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn orphan_reconciliation_is_expiry_and_ownership_fenced() {
        let api = Arc::new(FakeCube::default());
        let backend = backend(Arc::clone(&api));
        let prepared = backend.prepare(&lease(), &[]).await.unwrap();
        api.resource.lock().unwrap().as_mut().unwrap().expires_at =
            Utc::now() - Duration::seconds(1);
        let retained = BTreeSet::from([prepared.backend_operation_id.clone()]);
        assert!(
            backend
                .reconcile_orphans(&retained, Utc::now())
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            backend
                .reconcile_orphans(&BTreeSet::new(), Utc::now())
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(api.resource.lock().unwrap().is_none());
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

    #[test]
    fn trusted_side_canonicalizes_cube_coding_patch() {
        let spec = CodingTurnSpec {
            repository_digest: format!("sha256:{}", "1".repeat(64)),
            base_revision: "a".repeat(40),
            workspace_root: "/workspace/repo".into(),
            prompt: "fixture".into(),
            model_alias: "fixture".into(),
            materialization_manifest_digest: format!("sha256:{}", "2".repeat(64)),
            writable_roots: BTreeSet::from(["/workspace/repo".into()]),
            allowed_tools: BTreeSet::from(["fs.read".into(), "fs.write".into()]),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 1,
        };
        let request = CodingFixtureRequest {
            spec: spec.clone(),
            target_path: "fixture.txt".into(),
            expected_text: "before".into(),
            replacement_text: "after".into(),
        };
        let mut lease = lease();
        lease.command = serde_json::to_value(CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-coding-fixture-v1".into(),
            template_version: 1,
            template_digest: lease.command_template_digest.clone(),
            executable: "/usr/local/bin/light-coding-agent-fixture".into(),
            arguments: vec![
                "--repository".into(),
                "/inputs/repository.bundle".into(),
                "--request-base64".into(),
                request.encode_argument().unwrap(),
            ],
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms: 10_000,
            stdout_limit_bytes: 4096,
            stderr_limit_bytes: 4096,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        })
        .unwrap();
        let patch = "diff --git a/fixture.txt b/fixture.txt\n--- a/fixture.txt\n+++ b/fixture.txt\n@@ -1 +1 @@\n-before\n+after\n";
        let output = CodingFixtureOutput {
            adapter_id: "cube-coding-fixture".into(),
            adapter_version: "1".into(),
            repository_digest: spec.repository_digest.clone(),
            base_revision: spec.base_revision.clone(),
            patch: patch.into(),
            changed_paths: vec!["fixture.txt".into()],
        };
        let validated =
            validate_coding_fixture_output(&lease, &serde_json::to_vec(&output).unwrap())
                .unwrap()
                .unwrap();
        assert_eq!(validated["baseRevision"], spec.base_revision);
        assert_eq!(
            validated["patchDigest"],
            format!("sha256:{:x}", sha2::Sha256::digest(patch.as_bytes()))
        );
        let mut tampered = output;
        tampered.changed_paths = vec!["other.txt".into()];
        assert!(
            validate_coding_fixture_output(&lease, &serde_json::to_vec(&tampered).unwrap())
                .is_err()
        );

        lease.command = serde_json::to_value(CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-pi-rpc-v1".into(),
            template_version: 1,
            template_digest: lease.command_template_digest.clone(),
            executable: "/usr/local/bin/light-pi-rpc-adapter".into(),
            arguments: vec![
                "--repository".into(),
                "/inputs/repository.bundle".into(),
                "--request-base64".into(),
                spec.encode_argument().unwrap(),
                "--pi".into(),
                "/usr/local/bin/pi".into(),
                "--pi-digest".into(),
                format!("sha256:{}", "3".repeat(64)),
                "--provider".into(),
                "brokered".into(),
                "--model".into(),
                "approved".into(),
            ],
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms: 10_000,
            stdout_limit_bytes: 4096,
            stderr_limit_bytes: 4096,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        })
        .unwrap();
        let pi_output = CodingFixtureOutput {
            adapter_id: PI_RPC_ADAPTER_ID.into(),
            adapter_version: PI_RPC_ADAPTER_VERSION.into(),
            repository_digest: spec.repository_digest.clone(),
            base_revision: spec.base_revision.clone(),
            patch: patch.into(),
            changed_paths: vec!["fixture.txt".into()],
        };
        assert!(
            validate_coding_fixture_output(&lease, &serde_json::to_vec(&pi_output).unwrap())
                .unwrap()
                .is_some()
        );
    }
}
