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
use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};
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
}

#[derive(Clone)]
pub struct OciExecutionBackend {
    config: OciBackendConfig,
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
                "{{.State.Status}}".into(),
                id.into(),
            ])
            .await?;
        if !output.status.success() {
            return Err(BackendError::NotFound(id.into()));
        }
        let state = match String::from_utf8_lossy(&output.stdout).trim() {
            "created" => BackendOperationState::Prepared,
            "running" => BackendOperationState::Running,
            "exited" => BackendOperationState::Succeeded,
            "dead" => BackendOperationState::Failed,
            _ => BackendOperationState::Unknown,
        };
        Ok(Inspection {
            backend_operation_id: id.into(),
            state,
            observed_at: Utc::now(),
            evidence: BTreeMap::new(),
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
}
