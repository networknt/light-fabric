use async_trait::async_trait;
use chrono::Utc;
use execution_backend::{
    BackendError, BackendOperationState, BackendOutput, CleanupEvidence, ExecutionBackend,
    Inspection, PreparedExecution, StagedInput,
};
use execution_runner_protocol::{
    BackendCapability, CommandExecutionSpec, ExecuteLease, HostExposure, IsolationBoundary,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, sync::watch};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OciRuntime {
    Docker,
    Podman,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OciBackendConfig {
    pub runtime: OciRuntime,
    pub binary: PathBuf,
    pub image: String,
    pub compatibility_digest: String,
    pub available_slots: u32,
    pub rootless: bool,
    pub maximum_memory_bytes: u64,
    pub maximum_pids: u32,
    pub owner_runner: String,
}

#[derive(Clone)]
pub struct OciExecutionBackend {
    config: OciBackendConfig,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct OciContainerState {
    status: String,
    #[serde(default)]
    exit_code: Option<i32>,
    #[serde(rename = "OOMKilled", default)]
    oom_killed: bool,
    #[serde(default)]
    error: String,
    #[serde(default)]
    started_at: String,
    #[serde(default)]
    finished_at: String,
}

fn inspection_state(
    raw: &[u8],
) -> Result<(BackendOperationState, BTreeMap<String, String>), BackendError> {
    let state: OciContainerState = serde_json::from_slice(raw).map_err(|error| {
        BackendError::Unknown(format!(
            "OCI runtime returned invalid state evidence: {error}"
        ))
    })?;
    let status = state.status.trim().to_ascii_lowercase();
    let failed = state.oom_killed
        || state.exit_code.is_some_and(|code| code != 0)
        || !state.error.trim().is_empty();
    let operation_state = match status.as_str() {
        "created" | "configured" => BackendOperationState::Prepared,
        "running" | "paused" | "restarting" | "removing" | "stopping" => {
            BackendOperationState::Running
        }
        "exited" | "stopped" if state.exit_code.is_none() => BackendOperationState::Unknown,
        "exited" | "stopped" if failed => BackendOperationState::Failed,
        "exited" | "stopped" => BackendOperationState::Succeeded,
        "dead" => BackendOperationState::Failed,
        _ => BackendOperationState::Unknown,
    };
    let mut evidence = BTreeMap::from([
        ("runtimeStatus".into(), status),
        ("oomKilled".into(), state.oom_killed.to_string()),
    ]);
    if let Some(exit_code) = state.exit_code {
        evidence.insert("exitCode".into(), exit_code.to_string());
    }
    if !state.error.trim().is_empty() {
        evidence.insert("runtimeError".into(), state.error);
    }
    if !state.started_at.trim().is_empty() {
        evidence.insert("startedAt".into(), state.started_at);
    }
    if !state.finished_at.trim().is_empty() {
        evidence.insert("finishedAt".into(), state.finished_at);
    }
    Ok((operation_state, evidence))
}

impl OciExecutionBackend {
    pub fn new(config: OciBackendConfig) -> Result<Self, BackendError> {
        if !config.binary.is_absolute() || !config.binary.is_file() {
            return Err(BackendError::InvalidRequest(
                "OCI runtime binary must be an absolute regular file".into(),
            ));
        }
        if !config.image.contains("@sha256:")
            || config.available_slots == 0
            || config.maximum_memory_bytes == 0
            || config.maximum_pids == 0
            || config.owner_runner.trim().is_empty()
        {
            return Err(BackendError::InvalidRequest(
                "OCI image must be digest-pinned and resource limits must be positive".into(),
            ));
        }
        if matches!(config.runtime, OciRuntime::Docker) && config.rootless {
            return Err(BackendError::InvalidRequest(
                "rootless=true is supported only by the Podman backend profile".into(),
            ));
        }
        Ok(Self { config })
    }

    fn backend_id(&self) -> &'static str {
        if self.config.rootless {
            "rootless-oci"
        } else {
            "docker"
        }
    }
    fn name(lease: &ExecuteLease) -> String {
        format!("light-{}", lease.lease.execution_id)
    }

    async fn runtime(&self, args: &[String]) -> Result<std::process::Output, BackendError> {
        Command::new(&self.config.binary)
            .args(args)
            .env_clear()
            .output()
            .await
            .map_err(|e| BackendError::Transport(format!("OCI runtime launch failed: {e}")))
    }

    async fn exists(&self, name: &str) -> Result<bool, BackendError> {
        let output = self
            .runtime(&["container".into(), "inspect".into(), name.into()])
            .await?;
        Ok(output.status.success())
    }
}

#[async_trait]
impl ExecutionBackend for OciExecutionBackend {
    fn capability(&self) -> BackendCapability {
        BackendCapability {
            backend_id: self.backend_id().into(),
            backend_version: "oci-cli-v1".into(),
            boundary: if self.config.rootless {
                IsolationBoundary::UserNamespace
            } else {
                IsolationBoundary::Container
            },
            host_exposure: HostExposure::ExplicitMounts,
            actions: vec!["run.shell".into()],
            features: vec![
                "deny-all-egress".into(),
                "read-only-rootfs".into(),
                "digest-pinned-image".into(),
                "bounded-resources".into(),
            ],
            compatibility_digest: self.config.compatibility_digest.clone(),
            healthy: true,
            available_slots: self.config.available_slots,
        }
    }

    fn validate(&self, lease: &ExecuteLease, staged: &[StagedInput]) -> Result<(), BackendError> {
        if lease.backend_id != self.backend_id()
            || lease.lease.compatibility_digest != self.config.compatibility_digest
        {
            return Err(BackendError::InvalidRequest(
                "OCI backend selection or compatibility digest mismatch".into(),
            ));
        }
        let command: CommandExecutionSpec = serde_json::from_value(lease.command.clone())
            .map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
        if command.network_enabled || command.credentials_enabled || command.persistent_workspace {
            return Err(BackendError::Unsupported(
                "OCI foundation supports only deny-egress, credential-free, ephemeral execution"
                    .into(),
            ));
        }
        if !command.working_directory.starts_with("/workspace")
            || staged.iter().any(|v| {
                !v.read_only || v.executable || !v.mount_options.iter().any(|o| o == "noexec")
            })
        {
            return Err(BackendError::InvalidRequest(
                "OCI workdir must be under /workspace and all inputs must be read-only/noexec"
                    .into(),
            ));
        }
        if lease.lease.deadline <= Utc::now() {
            return Err(BackendError::TimedOut("lease deadline expired".into()));
        }
        Ok(())
    }

    async fn prepare(
        &self,
        lease: &ExecuteLease,
        staged: &[StagedInput],
    ) -> Result<PreparedExecution, BackendError> {
        self.validate(lease, staged)?;
        let name = Self::name(lease);
        if !self.exists(&name).await? {
            let spec: CommandExecutionSpec = serde_json::from_value(lease.command.clone())
                .map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
            let mut args = vec![
                "container".into(),
                "create".into(),
                "--name".into(),
                name.clone(),
                "--network".into(),
                "none".into(),
                "--read-only".into(),
                "--tmpfs".into(),
                "/workspace:rw,noexec,nosuid,nodev,size=67108864".into(),
                "--cap-drop".into(),
                "ALL".into(),
                "--security-opt".into(),
                "no-new-privileges".into(),
                "--pids-limit".into(),
                self.config.maximum_pids.to_string(),
                "--memory".into(),
                self.config.maximum_memory_bytes.to_string(),
                "--workdir".into(),
                spec.working_directory.clone(),
                "--label".into(),
                format!("light.execution={}", lease.lease.execution_id),
                "--label".into(),
                format!("light.runner={}", self.config.owner_runner),
                "--label".into(),
                format!("light.expires={}", lease.lease.deadline.to_rfc3339()),
            ];
            for input in staged {
                args.extend([
                    "--mount".into(),
                    format!(
                        "type=bind,src={},dst={},readonly",
                        input.local_path.display(),
                        input.mount_target
                    ),
                ]);
            }
            for (key, value) in &spec.environment {
                args.extend(["--env".into(), format!("{key}={value}")]);
            }
            args.push(self.config.image.clone());
            args.push(spec.executable);
            args.extend(spec.arguments);
            let output = self.runtime(&args).await?;
            if !output.status.success() && !self.exists(&name).await? {
                return Err(BackendError::Transport(format!(
                    "OCI create failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
        }
        Ok(PreparedExecution {
            backend_operation_id: name,
            execution_id: lease.lease.execution_id,
            backend_id: self.backend_id().into(),
            prepared_at: Utc::now(),
            evidence: BTreeMap::from([("image".into(), self.config.image.clone())]),
        })
    }

    async fn inspect(&self, id: &str) -> Result<Inspection, BackendError> {
        let output = self
            .runtime(&[
                "container".into(),
                "inspect".into(),
                "--format".into(),
                "{{json .State}}".into(),
                id.into(),
            ])
            .await?;
        if !output.status.success() {
            return Err(BackendError::NotFound(id.into()));
        }
        let (state, evidence) = inspection_state(&output.stdout)?;
        Ok(Inspection {
            backend_operation_id: id.into(),
            state,
            observed_at: Utc::now(),
            evidence,
        })
    }

    async fn execute(
        &self,
        prepared: &PreparedExecution,
        lease: &ExecuteLease,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<BackendOutput, BackendError> {
        let spec: CommandExecutionSpec = serde_json::from_value(lease.command.clone())
            .map_err(|e| BackendError::InvalidRequest(e.to_string()))?;
        let state = self.inspect(&prepared.backend_operation_id).await?;
        if !matches!(state.state, BackendOperationState::Prepared) {
            return Err(BackendError::Unknown(format!(
                "refusing to restart OCI operation in {:?}; inspect existing outcome",
                state.state
            )));
        }
        let started_at = Utc::now();
        let mut child = Command::new(&self.config.binary)
            .args([
                "container",
                "start",
                "--attach",
                &prepared.backend_operation_id,
            ])
            .env_clear()
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| BackendError::Transport("missing OCI stdout".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| BackendError::Transport("missing OCI stderr".into()))?;
        let out_limit = spec.stdout_limit_bytes.saturating_add(1);
        let err_limit = spec.stderr_limit_bytes.saturating_add(1);
        let out_task = tokio::spawn(async move {
            let mut b = Vec::new();
            stdout.take(out_limit).read_to_end(&mut b).await.map(|_| b)
        });
        let err_task = tokio::spawn(async move {
            let mut b = Vec::new();
            stderr.take(err_limit).read_to_end(&mut b).await.map(|_| b)
        });
        let remaining = lease
            .lease
            .deadline
            .signed_duration_since(Utc::now())
            .to_std()
            .map_err(|_| BackendError::TimedOut("lease deadline expired".into()))?;
        let status = tokio::select! {
            value = child.wait() => value.map_err(|e| BackendError::Transport(e.to_string()))?,
            _ = tokio::time::sleep(std::cmp::min(remaining, Duration::from_millis(spec.wall_clock_timeout_ms))) => { let _=self.cancel(&prepared.backend_operation_id).await; let _=child.kill().await; return Err(BackendError::TimedOut("OCI execution deadline expired".into())); },
            changed = cancellation.changed() => { if changed.is_ok() && *cancellation.borrow() { let _=self.cancel(&prepared.backend_operation_id).await; let _=child.kill().await; return Err(BackendError::Cancelled("OCI execution cancelled".into())); } return Err(BackendError::Unknown("cancellation channel closed".into())); }
        };
        let stdout = out_task
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        let stderr = err_task
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if stdout.len() as u64 > spec.stdout_limit_bytes
            || stderr.len() as u64 > spec.stderr_limit_bytes
        {
            return Err(BackendError::Unknown(
                "OCI output exceeded admitted limit".into(),
            ));
        }
        Ok(BackendOutput {
            exit_code: status.code(),
            signal: None,
            stdout,
            stderr,
            structured_output: None,
            started_at,
            finished_at: Utc::now(),
            failure_class: (!status.success()).then(|| "non_zero_exit".into()),
            evidence: BTreeMap::from([("image".into(), self.config.image.clone())]),
        })
    }

    async fn cancel(&self, id: &str) -> Result<(), BackendError> {
        let output = self
            .runtime(&["container".into(), "kill".into(), id.into()])
            .await?;
        if output.status.success() || !self.exists(id).await? {
            Ok(())
        } else {
            Err(BackendError::Transport(
                String::from_utf8_lossy(&output.stderr).into(),
            ))
        }
    }
    async fn cleanup(&self, id: &str) -> Result<CleanupEvidence, BackendError> {
        if self.exists(id).await? {
            let output = self
                .runtime(&["container".into(), "rm".into(), "--force".into(), id.into()])
                .await?;
            if !output.status.success() && self.exists(id).await? {
                return Err(BackendError::Cleanup(
                    String::from_utf8_lossy(&output.stderr).into(),
                ));
            }
        }
        Ok(CleanupEvidence {
            backend_operation_id: id.into(),
            cleaned_at: Utc::now(),
            evidence_reference: format!("oci:deleted:{id}"),
        })
    }

    async fn reconcile_orphans(
        &self,
        retained_operation_ids: &BTreeSet<String>,
        now: chrono::DateTime<Utc>,
    ) -> Result<Vec<CleanupEvidence>, BackendError> {
        let output = self
            .runtime(&[
                "container".into(),
                "ls".into(),
                "--all".into(),
                "--filter".into(),
                format!("label=light.runner={}", self.config.owner_runner),
                "--format".into(),
                "{{.Names}}".into(),
            ])
            .await?;
        if !output.status.success() {
            return Err(BackendError::Transport(format!(
                "OCI owned-resource discovery failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let names = String::from_utf8(output.stdout)
            .map_err(|_| BackendError::Unknown("OCI discovery returned non-UTF8 names".into()))?;
        let names = names
            .lines()
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if names.len() > 1_000 {
            return Err(BackendError::Unknown(
                "OCI owned-resource discovery exceeded 1000 containers".into(),
            ));
        }
        let mut cleaned = Vec::new();
        for name in names {
            if retained_operation_ids.contains(name) {
                continue;
            }
            let labels = self
                .runtime(&[
                    "container".into(),
                    "inspect".into(),
                    "--format".into(),
                    "{{json .Config.Labels}}".into(),
                    name.into(),
                ])
                .await?;
            if !labels.status.success() {
                continue;
            }
            let labels: BTreeMap<String, String> =
                serde_json::from_slice(&labels.stdout).map_err(|error| {
                    BackendError::Unknown(format!("OCI labels are invalid: {error}"))
                })?;
            if labels.get("light.runner").map(String::as_str)
                != Some(self.config.owner_runner.as_str())
            {
                continue;
            }
            let expires = labels.get("light.expires").ok_or_else(|| {
                BackendError::Unknown(format!("owned OCI container {name} has no expiry label"))
            })?;
            let expires = chrono::DateTime::parse_from_rfc3339(expires)
                .map_err(|error| {
                    BackendError::Unknown(format!(
                        "owned OCI container {name} has invalid expiry: {error}"
                    ))
                })?
                .with_timezone(&Utc);
            if expires > now {
                continue;
            }
            let evidence = self.cleanup(name).await?;
            cleaned.push(CleanupEvidence {
                evidence_reference: format!("oci:orphan-deleted:{name}"),
                ..evidence
            });
        }
        Ok(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_uses_exit_code_and_oom_evidence() {
        let (state, evidence) = inspection_state(
            br#"{"Status":"exited","ExitCode":17,"OOMKilled":false,"Error":"","StartedAt":"start","FinishedAt":"finish"}"#,
        )
        .unwrap();
        assert_eq!(state, BackendOperationState::Failed);
        assert_eq!(evidence.get("exitCode").map(String::as_str), Some("17"));

        let (state, _) =
            inspection_state(br#"{"Status":"exited","ExitCode":0,"OOMKilled":true,"Error":""}"#)
                .unwrap();
        assert_eq!(state, BackendOperationState::Failed);
    }

    #[test]
    fn inspection_reports_only_proven_zero_exit_as_success() {
        let (state, _) =
            inspection_state(br#"{"Status":"exited","ExitCode":0,"OOMKilled":false,"Error":""}"#)
                .unwrap();
        assert_eq!(state, BackendOperationState::Succeeded);

        let (state, _) =
            inspection_state(br#"{"Status":"exited","OOMKilled":false,"Error":""}"#).unwrap();
        assert_eq!(state, BackendOperationState::Unknown);
    }

    #[test]
    fn inspection_fails_closed_on_runtime_error_or_malformed_evidence() {
        let (state, evidence) = inspection_state(
            br#"{"Status":"exited","ExitCode":0,"OOMKilled":false,"Error":"runtime failure"}"#,
        )
        .unwrap();
        assert_eq!(state, BackendOperationState::Failed);
        assert_eq!(
            evidence.get("runtimeError").map(String::as_str),
            Some("runtime failure")
        );
        assert!(matches!(
            inspection_state(br#"{"Status":"exited","ExitCode":"bad"}"#),
            Err(BackendError::Unknown(_))
        ));
    }
}
