use crate::broker::{AttemptBroker, AttemptBrokerConfig};
use crate::journal::Journal;
use agent_core::ResultClass;
use agent_runtime_protocol::{
    AgentWorkerExecutionSpec, RuntimeCommand, RuntimeEvent, RuntimeEventPayload, RuntimeIdentity,
};
use execution_runner_protocol::ExecuteLease;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::watch,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerProcessConfig {
    pub origin_service_id: String,
    pub executable: std::path::PathBuf,
    pub binary_digest: String,
    pub capability_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox_launcher: Option<WorkerSandboxLauncherConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_home: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<AttemptBrokerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerSandboxLauncherConfig {
    /// Pinned executable implementing the per-attempt sandbox-launch contract.
    /// It must create the boundary and then `exec` the worker without forking so
    /// Unix peer credentials remain bound to the PID admitted by the broker.
    pub executable: std::path::PathBuf,
    pub binary_digest: String,
    pub profile_digest: String,
    /// True only when the launcher/deployment enforces an allowlist that limits
    /// App Server model traffic to the configured gateway.
    pub restricted_model_egress: bool,
}

impl WorkerProcessConfig {
    pub fn has_restricted_model_egress(&self) -> bool {
        self.sandbox_launcher
            .as_ref()
            .is_some_and(|launcher| launcher.restricted_model_egress)
    }

    pub fn requires_exclusive_runner(&self) -> bool {
        self.sandbox_launcher.is_none()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.origin_service_id.is_empty()
            || !self
                .origin_service_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err("agent worker originServiceId is invalid".into());
        }
        if !self.executable.is_absolute() {
            return Err("agent worker executable must be an absolute path".into());
        }
        if self
            .codex_home
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err("agent worker codexHome must be an absolute path".into());
        }
        if self.codex_home.is_some() && self.broker.is_some() {
            return Err(
                "personal Codex home and enterprise credential broker require separate runner pools"
                    .into(),
            );
        }
        #[cfg(unix)]
        if let Some(path) = &self.codex_home {
            let metadata = std::fs::symlink_metadata(path)
                .map_err(|error| format!("agent worker codexHome is unavailable: {error}"))?;
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_dir()
                || metadata.permissions().mode() & 0o077 != 0
                || metadata.uid() != unsafe { libc::geteuid() }
            {
                return Err("agent worker codexHome must be an owner-only real directory".into());
            }
        }
        for (name, digest) in [
            ("agent worker binary", self.binary_digest.as_str()),
            ("agent worker capability", self.capability_digest.as_str()),
        ] {
            let value = digest.strip_prefix("sha256:").unwrap_or_default();
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{name} digest must be sha256 plus 64 hex digits"));
            }
        }
        if let Some(launcher) = &self.sandbox_launcher {
            if !launcher.executable.is_absolute() {
                return Err("agent worker sandbox launcher must be an absolute path".into());
            }
            for (name, digest) in [
                ("sandbox launcher binary", launcher.binary_digest.as_str()),
                ("sandbox launcher profile", launcher.profile_digest.as_str()),
            ] {
                let value = digest.strip_prefix("sha256:").unwrap_or_default();
                if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(format!("{name} digest must be sha256 plus 64 hex digits"));
                }
            }
        }
        if self.broker.is_some()
            && self
                .sandbox_launcher
                .as_ref()
                .is_none_or(|launcher| !launcher.restricted_model_egress)
        {
            return Err(
                "enterprise agent workers require a pinned per-attempt sandbox launcher with restricted model egress"
                    .into(),
            );
        }
        if let Some(broker) = &self.broker {
            broker.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct WorkerOutcome {
    pub class: ResultClass,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub events: u64,
}

pub async fn run_worker_process(
    lease: &ExecuteLease,
    spec: &AgentWorkerExecutionSpec,
    config: &WorkerProcessConfig,
    journal: &Journal,
    mut cancel: watch::Receiver<bool>,
) -> Result<WorkerOutcome, String> {
    config.validate()?;
    validate_spec(lease, spec)?;
    validate_authentication_profile(lease, spec, config)?;
    if spec.expected_capability_digest != config.capability_digest {
        return Err("worker capability digest is not admitted by this runner".into());
    }
    verify_binary(&config.executable, &config.binary_digest).await?;
    if let Some(launcher) = &config.sandbox_launcher {
        verify_binary(&launcher.executable, &launcher.binary_digest).await?;
    }
    let identity = RuntimeIdentity {
        execution_id: lease.lease.execution_id,
        lease_id: lease.lease.lease_id,
        fencing_token: lease.lease.fencing_token,
        transport_nonce: Uuid::new_v4().simple().to_string(),
    };
    let broker = match (&spec.broker, &config.broker) {
        (Some(grant), Some(config)) => Some(
            AttemptBroker::bind(config, grant.clone(), identity.clone(), journal.clone()).await?,
        ),
        (None, None) => None,
        _ => {
            return Err(
                "agent broker grant and runner broker configuration must both be present".into(),
            );
        }
    };
    let mut broker_unknown = broker.as_ref().map(AttemptBroker::unknown_receiver);
    let mut command = if let Some(launcher) = &config.sandbox_launcher {
        let mut command = Command::new(&launcher.executable);
        command
            .arg("--profile-digest")
            .arg(&launcher.profile_digest)
            .arg("--execution-id")
            .arg(lease.lease.execution_id.to_string());
        if let Some(broker) = &broker {
            command.arg("--broker-socket").arg(broker.socket_path());
        }
        if let Some(inputs) = spec
            .input
            .get("runtimeStagedInputs")
            .and_then(serde_json::Value::as_array)
        {
            for path in inputs
                .iter()
                .filter_map(|input| input.get("localPath"))
                .filter_map(serde_json::Value::as_str)
            {
                command.arg("--read-only-input").arg(path);
            }
        }
        command.arg("--worker").arg(&config.executable).arg("--");
        command
    } else {
        Command::new(&config.executable)
    };
    command
        .env_clear()
        .env("HOME", "/var/lib/light-workflow-runner")
        .env("PATH", "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin")
        .env("CARGO_HOME", "/usr/local/cargo")
        .env("RUSTUP_HOME", "/usr/local/rustup")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(broker) = &broker {
        command.env("LIGHT_AGENT_BROKER_SOCKET", broker.socket_path());
    }
    if let Some(codex_home) = &config.codex_home {
        command.env("LIGHT_CODEX_HOME", codex_home);
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn agent worker: {e}"))?;
    let (broker_shutdown, broker_task) = if let Some(broker) = broker {
        let expected_pid = child.id().ok_or("spawned worker has no process id")?;
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(broker.serve(expected_pid, receiver));
        (Some(shutdown), Some(task))
    } else {
        (None, None)
    };
    let admitted_duration = std::time::Duration::from_millis(spec.wall_clock_timeout_ms).min(
        lease
            .lease
            .deadline
            .signed_duration_since(chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    );
    let execution_deadline = tokio::time::Instant::now() + admitted_duration;
    if *cancel.borrow() {
        kill_tree(&mut child).await;
        return Err("agent worker cancelled".into());
    }
    let mut stdin = child.stdin.take().ok_or("worker stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("worker stdout unavailable")?;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = child.stderr.take().ok_or("worker stderr unavailable")?;
    let stderr_limit = spec.maximum_stderr_bytes;
    let mut stderr_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stderr.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if out.len() + n > stderr_limit {
                return Err("worker stderr limit exceeded".into());
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok::<_, String>(out)
    });
    write_command(
        &mut stdin,
        &RuntimeCommand::Hello {
            identity: identity.clone(),
            expected_capability_digest: spec.expected_capability_digest.clone(),
        },
    )
    .await?;
    let ready = tokio::select! {
        event = read_event(&mut stdout, spec.maximum_event_bytes) => event?,
        changed = cancel.changed() => {
            kill_tree(&mut child).await;
            if changed.is_ok() && *cancel.borrow() {
                return Err("agent worker cancelled".into());
            }
            return Err("agent worker cancellation channel closed".into());
        }
        stderr = &mut stderr_task => {
            kill_tree(&mut child).await;
            return Err(stderr.map_err(|error| error.to_string())?.err()
                .unwrap_or_else(|| "agent worker exited before ready".into()));
        }
        _ = tokio::time::sleep_until(execution_deadline) => {
            kill_tree(&mut child).await;
            return Err("agent worker deadline expired before ready".into());
        }
        _ = wait_for_broker_unknown(&mut broker_unknown) => {
            kill_tree(&mut child).await;
            return Err("broker effect outcome is UNKNOWN and requires reconciliation".into());
        }
    };
    ready.validate(&identity, 0).map_err(|e| e.to_string())?;
    match &ready.payload {
        RuntimeEventPayload::Ready { capabilities }
            if agent_runtime_protocol::canonical_digest(capabilities)
                .map_err(|e| e.to_string())?
                == spec.expected_capability_digest => {}
        _ => return Err("worker did not return admitted capabilities".into()),
    }
    journal.record_runtime_event(&ready)?;
    write_command(
        &mut stdin,
        &RuntimeCommand::Start {
            session_id: spec.session_id,
            turn_id: spec.turn_id,
            action_attempt_id: spec.action_attempt_id,
            policy_digest: spec.policy_digest.clone(),
            enterprise_gateway: spec.enterprise_gateway.clone().map(Box::new),
            input: spec.input.clone(),
        },
    )
    .await?;
    let deadline = tokio::time::sleep_until(execution_deadline);
    tokio::pin!(deadline);
    let mut sequence = ready.sequence;
    let coding_spec = spec
        .input
        .get("codingSpec")
        .cloned()
        .map(serde_json::from_value::<coding_agent_runtime::CodingTurnSpec>)
        .transpose()
        .map_err(|error| format!("invalid worker codingSpec: {error}"))?;
    if let Some(coding_spec) = &coding_spec {
        coding_spec
            .validate()
            .map_err(|error| format!("invalid worker codingSpec: {error}"))?;
    }
    let mut validated_patch = None;
    let outcome = loop {
        tokio::select! {
            event = read_event(&mut stdout, spec.maximum_event_bytes) => {
                let event = event?;
                event.validate(&identity, sequence).map_err(|e| e.to_string())?;
                sequence = event.sequence;
                if let RuntimeEventPayload::CodingPatch { base_revision, patch, changed_paths, .. } = &event.payload {
                    let admitted = coding_spec.as_ref().ok_or("worker emitted a coding patch for a non-coding turn")?;
                    if admitted.role != coding_agent_runtime::CodingRole::Implement {
                        return Err("review worker attempted to mutate the candidate repository".into());
                    }
                    let validated = coding_agent_runtime::validate_patch(
                        admitted,
                        &execution_security::ProtectedPathPolicy::default_deny(),
                        base_revision,
                        patch,
                        changed_paths,
                    ).map_err(|error| format!("worker coding patch rejected: {error}"))?;
                    validated_patch = Some(serde_json::to_value(validated).map_err(|error| error.to_string())?);
                }
                journal.record_runtime_event(&event)?;
                if let RuntimeEventPayload::Terminal { class, mut output, error } = event.payload {
                    if class == ResultClass::Success && let Some(admitted) = &coding_spec {
                        let authentication: coding_agent_runtime::CodingAuthenticationEvidence =
                            serde_json::from_value(
                                output
                                    .as_ref()
                                    .and_then(|value| value.get("authentication"))
                                    .cloned()
                                    .ok_or("coding worker omitted authentication evidence")?,
                            )
                            .map_err(|error| {
                                format!("invalid coding authentication evidence: {error}")
                            })?;
                        authentication.validate().map_err(|error| error.to_string())?;
                        if authentication.profile != admitted.authentication_profile {
                            return Err(
                                "coding authentication evidence differs from admitted profile"
                                    .into(),
                            );
                        }
                        match admitted.role {
                            coding_agent_runtime::CodingRole::Implement => {
                                let validation_evidence: Vec<coding_agent_runtime::CodingValidationEvidence> = serde_json::from_value(
                                    output.as_ref()
                                        .and_then(|value| value.get("validationEvidence"))
                                        .cloned()
                                        .ok_or("coding implementer omitted observed validation evidence")?
                                ).map_err(|error| format!("invalid observed validation evidence: {error}"))?;
                                let patch_value = validated_patch.take().ok_or("coding implementer succeeded without a validated canonical patch")?;
                                let patch: coding_agent_runtime::ValidatedPatch = serde_json::from_value(patch_value.clone()).map_err(|error| error.to_string())?;
                                let contract: coding_agent_runtime::CodingAdapterContract = serde_json::from_value(
                                    spec.input.get("adapterContract").cloned().ok_or("coding adapter contract is missing")?
                                ).map_err(|error| error.to_string())?;
                                let implementation = coding_agent_runtime::CodingImplementationArtifact {
                                    schema_version: coding_agent_runtime::CODING_ARTIFACT_SCHEMA_VERSION,
                                    adapter_contract_digest: contract.digest().map_err(|error| error.to_string())?,
                                    repository_digest: admitted.repository_digest.clone(),
                                    base_revision: admitted.base_revision.clone(),
                                    patch_digest: patch.patch_digest.clone(),
                                    changed_paths: patch.changed_paths.clone(),
                                    validation_evidence,
                                    resolved_finding_ids: admitted.remediation.as_deref()
                                        .map(coding_agent_runtime::CodingRemediationInput::finding_ids)
                                        .unwrap_or_default(),
                                };
                                implementation.validate().map_err(|error| error.to_string())?;
                                output = Some(serde_json::json!({"worker":output,"codingPatch":patch_value,"codingImplementation":implementation}));
                            }
                            coding_agent_runtime::CodingRole::Review => {
                                if validated_patch.is_some() {
                                    return Err("review worker emitted a patch".into());
                                }
                                let review_value = output.as_ref()
                                    .and_then(|value| value.get("codingReview"))
                                    .cloned()
                                    .ok_or("review worker succeeded without CodingReviewResult")?;
                                let _: Vec<coding_agent_runtime::CodingValidationEvidence> = serde_json::from_value(
                                    output.as_ref()
                                        .and_then(|value| value.get("reviewValidationEvidence"))
                                        .cloned()
                                        .ok_or("review worker omitted observed validation evidence")?
                                ).map_err(|error| format!("invalid reviewer validation evidence: {error}"))?;
                                let review: coding_agent_runtime::CodingReviewResult = serde_json::from_value(review_value)
                                    .map_err(|error| format!("invalid CodingReviewResult: {error}"))?;
                                review.validate().map_err(|error| error.to_string())?;
                                let review_input = admitted.review_input.as_deref().ok_or("review input is missing")?;
                                if review.review_id != review_input.review_id
                                    || review.artifact_digest != review_input.implementation.patch_digest
                                {
                                    return Err("review result is not bound to the admitted candidate".into());
                                }
                            }
                        }
                    }
                    break WorkerOutcome { class, output, error, events: sequence };
                }
            }
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow() {
                    write_command(&mut stdin, &RuntimeCommand::Cancel { reason: "runner cancellation requested".into() }).await?;
                    let interrupted = async {
                        loop {
                            let event = read_event(&mut stdout, spec.maximum_event_bytes).await?;
                            event.validate(&identity, sequence).map_err(|e| e.to_string())?;
                            sequence = event.sequence;
                            if matches!(event.payload, RuntimeEventPayload::CodingPatch { .. }) {
                                kill_tree(&mut child).await;
                                return Err("cancelled coding worker emitted a patch after authority was revoked".into());
                            }
                            journal.record_runtime_event(&event)?;
                            if let RuntimeEventPayload::Terminal { .. } = event.payload {
                                return Ok::<_, String>(WorkerOutcome {
                                    class: ResultClass::Cancelled,
                                    output: None,
                                    error: Some("runner cancellation requested".into()),
                                    events: sequence,
                                });
                            }
                        }
                    };
                    match tokio::time::timeout(std::time::Duration::from_secs(1), interrupted).await {
                        Ok(Ok(outcome)) => break outcome,
                        _ => {
                            kill_tree(&mut child).await;
                            return Err("agent worker cancelled".into());
                        }
                    }
                }
            }
            stderr = &mut stderr_task => {
                kill_tree(&mut child).await;
                return Err(stderr.map_err(|error|error.to_string())?.err().unwrap_or_else(||"agent worker exited before terminal event".into()));
            }
            _ = &mut deadline => {
                kill_tree(&mut child).await;
                return Err("agent worker deadline expired".into());
            }
            _ = wait_for_broker_unknown(&mut broker_unknown) => {
                kill_tree(&mut child).await;
                return Err("broker effect outcome is UNKNOWN and requires reconciliation".into());
            }
        }
    };
    drop(stdin);
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr = stderr_task.await.map_err(|e| e.to_string())??;
    if !status.success() {
        return Err(format!(
            "agent worker exited {status}: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    if let Some(shutdown) = broker_shutdown {
        let _ = shutdown.send(true);
    }
    if let Some(task) = broker_task {
        task.await.map_err(|e| e.to_string())??;
    }
    Ok(outcome)
}
async fn wait_for_broker_unknown(receiver: &mut Option<watch::Receiver<bool>>) {
    match receiver {
        Some(receiver) => loop {
            if *receiver.borrow() {
                return;
            }
            if receiver.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        },
        None => std::future::pending::<()>().await,
    }
}
fn validate_spec(lease: &ExecuteLease, s: &AgentWorkerExecutionSpec) -> Result<(), String> {
    if s.schema_version != 1
        || s.policy_digest != lease.lease.policy_digest
        || s.template_digest != lease.command_template_digest
        || s.wall_clock_timeout_ms == 0
        || s.maximum_event_bytes == 0
        || s.maximum_event_bytes > agent_runtime_protocol::MAX_FRAME_BYTES
        || s.maximum_stderr_bytes == 0
        || s.maximum_stderr_bytes > agent_runtime_protocol::MAX_FRAME_BYTES
        || s.broker.as_ref().is_some_and(|grant| {
            grant.validate(chrono::Utc::now()).is_err()
                || grant.policy_digest != s.policy_digest
                || grant.expires_at > lease.lease.deadline
        })
        || s.enterprise_gateway.as_ref().is_some_and(|gateway| {
            gateway.validate().is_err()
                || gateway.binding.session_id != s.session_id
                || gateway.binding.turn_id != s.turn_id
                || gateway.binding.action_attempt_id != s.action_attempt_id
                || gateway.binding.policy_digest != s.policy_digest
                || s.broker.as_ref().is_none_or(|grant| {
                    gateway.binding.digest().ok().as_ref() != grant.gateway_binding_digest.as_ref()
                        || !grant
                            .allowed_operations
                            .contains(&agent_runtime_protocol::BrokerOperation::CredentialedRequest)
                        || !grant.allowed_targets.contains(&gateway.credential_target)
                })
        })
        || (s.enterprise_gateway.is_none()
            && s.broker
                .as_ref()
                .is_some_and(|grant| grant.gateway_binding_digest.is_some()))
    {
        return Err("invalid agent worker execution specification".into());
    }
    Ok(())
}

fn validate_authentication_profile(
    lease: &ExecuteLease,
    spec: &AgentWorkerExecutionSpec,
    config: &WorkerProcessConfig,
) -> Result<(), String> {
    let Some(value) = spec.input.get("codingSpec") else {
        return Ok(());
    };
    let coding: coding_agent_runtime::CodingTurnSpec =
        serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
    coding.validate().map_err(|error| error.to_string())?;
    let valid = match coding.authentication_profile {
        coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription => {
            config.codex_home.is_some()
                && config.broker.is_none()
                && spec.broker.is_none()
                && spec.enterprise_gateway.is_none()
        }
        coding_agent_runtime::CodingAuthenticationProfile::EnterpriseApi => {
            config.codex_home.is_none()
                && config.broker.is_some()
                && spec.broker.is_some()
                && spec.enterprise_gateway.as_ref().is_some_and(|gateway| {
                    gateway.binding.host_id == lease.lease.origin.host_id
                        && gateway.binding.end_user_subject == gateway.binding.billing_subject
                })
        }
    };
    valid.then_some(()).ok_or_else(|| {
        "coding authentication profile does not match the isolated runner pool or user binding"
            .into()
    })
}
async fn verify_binary(path: &Path, expected: &str) -> Result<(), String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read worker binary: {e}"))?;
    let actual = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if actual != expected {
        return Err("agent worker binary digest mismatch".into());
    }
    Ok(())
}
async fn write_command<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    c: &RuntimeCommand,
) -> Result<(), String> {
    let mut b = serde_json::to_vec(c).map_err(|e| e.to_string())?;
    b.push(b'\n');
    w.write_all(&b).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}
async fn read_event<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    maximum_bytes: usize,
) -> Result<RuntimeEvent, String> {
    // `read_until` avoids one async read per byte. Wrapping the borrowed reader
    // in `take` is essential: plain `read_until` can allocate without bound if
    // a hostile worker never emits a newline.
    let limit = u64::try_from(maximum_bytes)
        .map_err(|_| "worker event limit does not fit u64".to_string())?
        .saturating_add(1);
    let mut bounded = reader.take(limit);
    let mut frame = Vec::with_capacity(maximum_bytes.min(8 * 1024).saturating_add(1));
    let bytes = bounded
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|error| format!("worker event stream ended: {error}"))?;
    if bytes == 0 {
        return Err("worker event stream ended before a complete frame".into());
    }
    if frame.last() != Some(&b'\n') {
        return Err("worker event exceeds admitted limit".into());
    }
    frame.pop();
    if frame.len() > maximum_bytes {
        return Err("worker event exceeds admitted limit".into());
    }
    serde_json::from_slice(&frame).map_err(|error| format!("invalid worker event: {error}"))
}
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId};
    use agent_runtime_protocol::{
        AttemptBrokerGrant, BrokerOperation, EnterpriseGatewayConfig, GatewayAttemptBinding,
        RuntimeCapabilities, canonical_digest,
    };
    use chrono::Utc;
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionId, ExecutionSubject, LeaseContext, LeaseId, OriginKind,
        SchedulingRequestId,
    };
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    fn lease() -> ExecuteLease {
        ExecuteLease {
            lease: LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Agent,
                    service_id: "light-agent".into(),
                    instance_id: "test".into(),
                    host_id: Uuid::nil(),
                },
                subject: ExecutionSubject::AgentTurn {
                    subject_id: Uuid::new_v4(),
                    session_id: Uuid::new_v4(),
                    turn_id: Uuid::new_v4(),
                },
                attempt: 1,
                lease_id: LeaseId::new(),
                fencing_token: 7,
                policy_digest: "sha256:policy".into(),
                compatibility_digest: "sha256:compatibility".into(),
                deadline: Utc::now() + chrono::Duration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::json!({}),
            inputs: Vec::new(),
            definition_digest: "sha256:definition".into(),
            command_template_digest: "sha256:template".into(),
        }
    }

    fn capabilities() -> RuntimeCapabilities {
        RuntimeCapabilities {
            adapter_id: "fixture".into(),
            adapter_version: "1".into(),
            adapter_protocol_version: "fixture-v1".into(),
            protocol_version: agent_runtime_protocol::PROTOCOL_VERSION.into(),
            actions: BTreeSet::from(["mock".into()]),
            supports_approvals: false,
            supports_checkpoint: false,
            supports_session_reuse: false,
            supports_streaming: true,
            supports_thread_turn_identity: true,
            supports_usage: false,
            maximum_event_bytes: 4096,
        }
    }

    fn fixture(
        directory: &std::path::Path,
        capabilities: &RuntimeCapabilities,
        wait: bool,
    ) -> WorkerProcessConfig {
        let executable = directory.join("worker.py");
        let capability_digest = canonical_digest(capabilities).unwrap();
        let capabilities = serde_json::to_string(capabilities).unwrap();
        let wait = if wait {
            // A hostile/buggy worker acknowledges cancellation with success.
            // The runner must still force the authoritative result to Cancelled.
            "sys.stdin.readline()\nterminal()"
        } else {
            "terminal()"
        };
        let protocol_version = agent_runtime_protocol::PROTOCOL_VERSION;
        let source = format!(
            r##"#!/usr/bin/python3
import datetime,json,sys,time,uuid
hello=json.loads(sys.stdin.readline())
i=hello["identity"]
caps=json.loads(r'''{capabilities}''')
def event(sequence,payload):
 print(json.dumps({{"protocolVersion":"{protocol_version}","eventId":str(uuid.uuid4()),"executionId":i["executionId"],"leaseId":i["leaseId"],"fencingToken":i["fencingToken"],"sequence":sequence,"occurredAt":datetime.datetime.now(datetime.timezone.utc).isoformat(),"payload":payload}},separators=(",",":")),flush=True)
def terminal():
 event(2,{{"type":"terminal","class":"success","output":{{"ok":True}},"error":None}})
event(1,{{"type":"ready","capabilities":caps}})
sys.stdin.readline()
{wait}
"##
        );
        std::fs::write(&executable, source).unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let digest = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()))
        );
        WorkerProcessConfig {
            origin_service_id: "light-agent".into(),
            executable,
            binary_digest: digest,
            capability_digest,
            sandbox_launcher: None,
            codex_home: None,
            broker: None,
        }
    }

    fn spec(capability_digest: String) -> AgentWorkerExecutionSpec {
        AgentWorkerExecutionSpec {
            schema_version: 1,
            template_digest: "sha256:template".into(),
            expected_capability_digest: capability_digest,
            session_id: AgentSessionId::new(),
            turn_id: AgentTurnId::new(),
            action_attempt_id: AgentActionAttemptId::new(),
            policy_digest: "sha256:policy".into(),
            input: serde_json::json!({"scenario":"success"}),
            wall_clock_timeout_ms: 5_000,
            maximum_event_bytes: 16 * 1024,
            maximum_stderr_bytes: 16 * 1024,
            broker: None,
            enterprise_gateway: None,
        }
    }

    fn coding_spec(
        authentication_profile: coding_agent_runtime::CodingAuthenticationProfile,
    ) -> serde_json::Value {
        serde_json::to_value(coding_agent_runtime::CodingTurnSpec {
            repository_digest: format!("sha256:{}", "1".repeat(64)),
            base_revision: "a".repeat(40),
            workspace_root: "/workspace/repository".into(),
            prompt: "implement".into(),
            model_alias: coding_agent_runtime::CODING_IMPLEMENTER_ALIAS.into(),
            authentication_profile,
            role: coding_agent_runtime::CodingRole::Implement,
            role_profile: coding_agent_runtime::CodingRoleExecutionProfile::pinned(
                coding_agent_runtime::CodingRole::Implement,
            ),
            review_input: None,
            remediation: None,
            materialization_manifest_digest: format!("sha256:{}", "2".repeat(64)),
            writable_roots: BTreeSet::from(["/workspace/repository".into()]),
            allowed_tools: coding_agent_runtime::CodingTurnSpec::supported_tools(
                coding_agent_runtime::CodingRole::Implement,
            ),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 10,
        })
        .unwrap()
    }

    #[test]
    fn authentication_profiles_require_separate_pools_and_exact_user_tenant_binding() {
        let directory = tempfile::tempdir().unwrap();
        let mut lease = lease();
        lease.lease.origin.host_id = Uuid::new_v4();
        let mut config = fixture(directory.path(), &capabilities(), false);
        let codex_home = directory.path().join("codex-home");
        std::fs::create_dir(&codex_home).unwrap();
        std::fs::set_permissions(&codex_home, std::fs::Permissions::from_mode(0o700)).unwrap();
        config.codex_home = Some(codex_home);
        assert!(config.validate().is_ok());
        let mut mixed_pool = config.clone();
        mixed_pool.broker = Some(crate::broker::AttemptBrokerConfig {
            socket_directory: directory.path().join("mixed-broker"),
            maximum_request_bytes: 1024,
            request_timeout_ms: 1000,
            routes: Vec::new(),
        });
        assert!(mixed_pool.validate().is_err());
        let mut admitted = spec(config.capability_digest.clone());
        admitted.input = serde_json::json!({
            "codingSpec": coding_spec(
                coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription
            )
        });
        assert!(validate_authentication_profile(&lease, &admitted, &config).is_ok());

        let mut wrong_pool = config.clone();
        wrong_pool.codex_home = None;
        assert!(validate_authentication_profile(&lease, &admitted, &wrong_pool).is_err());

        admitted.input = serde_json::json!({
            "codingSpec": coding_spec(coding_agent_runtime::CodingAuthenticationProfile::EnterpriseApi)
        });
        let binding = GatewayAttemptBinding {
            audience: "llm-gateway".into(),
            host_id: lease.lease.origin.host_id,
            end_user_subject: "user-1".into(),
            principal_subject: "user-1".into(),
            workload_actor: "light-agent/worker-1".into(),
            workflow_id: Some(Uuid::new_v4()),
            session_id: admitted.session_id,
            turn_id: admitted.turn_id,
            action_attempt_id: admitted.action_attempt_id,
            policy_digest: format!("sha256:{}", "3".repeat(64)),
            data_boundary_digest: format!("sha256:{}", "4".repeat(64)),
            route_alias: "coding-implementer".into(),
            billing_subject: "user-1".into(),
            budget_policy_id: "developer-default".into(),
            correlation_id: Uuid::new_v4(),
        };
        admitted.enterprise_gateway = Some(EnterpriseGatewayConfig {
            provider_id: "light_gateway".into(),
            base_url: "https://llm-gateway.example/v1".into(),
            credential_target: "llm-gateway-attempt".into(),
            credential_env: "LIGHT_LLM_ATTEMPT_TOKEN".into(),
            binding,
        });
        admitted.broker = Some(AttemptBrokerGrant {
            policy_digest: admitted.policy_digest.clone(),
            data_boundary_digest: "sha256:boundary".into(),
            route_digest: "sha256:route".into(),
            allowed_operations: BTreeSet::from([BrokerOperation::CredentialedRequest]),
            allowed_targets: BTreeSet::from(["llm-gateway-attempt".into()]),
            maximum_requests: 1,
            maximum_tokens: 1,
            maximum_cost_micros: 1,
            maximum_response_bytes: 1024,
            expires_at: Utc::now() + chrono::Duration::seconds(30),
            gateway_binding_digest: Some(format!("sha256:{}", "5".repeat(64))),
        });
        config.codex_home = None;
        config.broker = Some(crate::broker::AttemptBrokerConfig {
            socket_directory: directory.path().join("broker"),
            maximum_request_bytes: 1024,
            request_timeout_ms: 1000,
            routes: Vec::new(),
        });
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("per-attempt sandbox launcher")
        );
        assert!(validate_authentication_profile(&lease, &admitted, &config).is_ok());

        admitted
            .enterprise_gateway
            .as_mut()
            .unwrap()
            .binding
            .host_id = Uuid::new_v4();
        assert!(validate_authentication_profile(&lease, &admitted, &config).is_err());
        admitted
            .enterprise_gateway
            .as_mut()
            .unwrap()
            .binding
            .host_id = lease.lease.origin.host_id;
        admitted
            .enterprise_gateway
            .as_mut()
            .unwrap()
            .binding
            .billing_subject = "user-2".into();
        assert!(validate_authentication_profile(&lease, &admitted, &config).is_err());
        admitted
            .enterprise_gateway
            .as_mut()
            .unwrap()
            .binding
            .billing_subject = "user-1".into();
        admitted.input = serde_json::json!({
            "codingSpec": coding_spec(
                coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription
            )
        });
        assert!(validate_authentication_profile(&lease, &admitted, &config).is_err());
    }

    #[test]
    fn execution_spec_rejects_expired_broker_grant() {
        let lease = lease();
        let mut spec = spec("sha256:capability".into());
        spec.broker = Some(AttemptBrokerGrant {
            policy_digest: spec.policy_digest.clone(),
            data_boundary_digest: "sha256:data".into(),
            route_digest: "sha256:route".into(),
            allowed_operations: BTreeSet::from([BrokerOperation::ModelInference]),
            allowed_targets: BTreeSet::from(["llm-gateway".into()]),
            maximum_requests: 1,
            maximum_tokens: 1,
            maximum_cost_micros: 1,
            maximum_response_bytes: 1,
            expires_at: Utc::now() - chrono::Duration::seconds(1),
            gateway_binding_digest: None,
        });
        assert!(validate_spec(&lease, &spec).is_err());
    }

    #[test]
    fn execution_spec_rejects_cross_turn_enterprise_gateway_binding() {
        let lease = lease();
        let mut spec = spec("sha256:capability".into());
        let binding = GatewayAttemptBinding {
            audience: "llm-gateway".into(),
            host_id: Uuid::new_v4(),
            end_user_subject: "user-1".into(),
            principal_subject: "user-1".into(),
            workload_actor: "light-agent/worker-1".into(),
            workflow_id: Some(Uuid::new_v4()),
            session_id: spec.session_id,
            turn_id: AgentTurnId::new(),
            action_attempt_id: spec.action_attempt_id,
            policy_digest: format!("sha256:{}", "1".repeat(64)),
            data_boundary_digest: format!("sha256:{}", "2".repeat(64)),
            route_alias: "coding-implementer".into(),
            billing_subject: "user-1".into(),
            budget_policy_id: "developer-default".into(),
            correlation_id: Uuid::new_v4(),
        };
        let binding_digest = binding.digest().unwrap();
        spec.enterprise_gateway = Some(EnterpriseGatewayConfig {
            provider_id: "light_gateway".into(),
            base_url: "https://llm-gateway.example/v1".into(),
            credential_target: "llm-gateway-attempt".into(),
            credential_env: "LIGHT_LLM_ATTEMPT_TOKEN".into(),
            binding,
        });
        spec.broker = Some(AttemptBrokerGrant {
            policy_digest: spec.policy_digest.clone(),
            data_boundary_digest: "sha256:boundary".into(),
            route_digest: "sha256:route".into(),
            allowed_operations: BTreeSet::from([BrokerOperation::CredentialedRequest]),
            allowed_targets: BTreeSet::from(["llm-gateway-attempt".into()]),
            maximum_requests: 1,
            maximum_tokens: 100,
            maximum_cost_micros: 100,
            maximum_response_bytes: 1024,
            expires_at: Utc::now() + chrono::Duration::seconds(30),
            gateway_binding_digest: Some(binding_digest),
        });

        assert!(validate_spec(&lease, &spec).is_err());
    }

    #[tokio::test]
    async fn authenticates_journals_and_returns_terminal_event() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let config = fixture(directory.path(), &capabilities(), false);
        let spec = spec(config.capability_digest.clone());
        let (_cancel, cancellation) = watch::channel(false);

        let outcome = run_worker_process(&lease, &spec, &config, &journal, cancellation)
            .await
            .unwrap();

        assert_eq!(outcome.class, ResultClass::Success);
        assert_eq!(outcome.events, 2);
        assert_eq!(
            journal
                .runtime_events_after(lease.lease.execution_id, 0)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_cannot_be_overridden_by_a_worker_success_terminal() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let config = fixture(directory.path(), &capabilities(), true);
        let spec = spec(config.capability_digest.clone());
        let (cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(async move {
            run_worker_process(&lease, &spec, &config, &journal, cancellation).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.send(true).unwrap();

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(outcome.class, ResultClass::Cancelled);
        assert!(outcome.output.is_none());
        assert_eq!(
            outcome.error.as_deref(),
            Some("runner cancellation requested")
        );
    }

    #[tokio::test]
    async fn rejects_worker_binary_drift_before_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let mut config = fixture(directory.path(), &capabilities(), false);
        config.binary_digest = format!("sha256:{}", "0".repeat(64));
        let spec = spec(config.capability_digest.clone());
        let (_cancel, cancellation) = watch::channel(false);

        let error = run_worker_process(&lease, &spec, &config, &journal, cancellation)
            .await
            .unwrap_err();
        assert_eq!(error, "agent worker binary digest mismatch");
    }

    #[tokio::test]
    async fn buffered_reader_bounds_a_frame_without_a_newline() {
        let bytes = vec![b'x'; 1024];
        let mut reader = BufReader::new(bytes.as_slice());

        let error = read_event(&mut reader, 128).await.unwrap_err();

        assert_eq!(error, "worker event exceeds admitted limit");
    }
}
