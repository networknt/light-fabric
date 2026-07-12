use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::worker_process::WorkerProcessConfig;
use execution_backend::ExecutionBackend;
use execution_backend_mock::MockBehavior;
use execution_backend_mock::MockExecutionBackend;
use execution_runner_protocol::{OriginKind, SubjectKind, canonical_sha256};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunnerConfigFile {
    version: u16,
    runner_id: String,
    enrollment_id: String,
    host_id: Uuid,
    controller_url: String,
    jwt_file: PathBuf,
    data_directory: PathBuf,
    health_address: SocketAddr,
    maximum_concurrency: u32,
    heartbeat_interval_ms: u64,
    reconnect_maximum_ms: u64,
    shutdown_grace_ms: u64,
    staging_maximum_bytes: u64,
    backend: MockBackendConfig,
    allowed_command_template_digests: Vec<String>,
    #[serde(default)]
    agent_worker: Option<WorkerProcessConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockBackendConfig {
    pub compatibility_digest: String,
    pub available_slots: u32,
    #[serde(default)]
    pub behavior: MockBehavior,
}

#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub runner_id: String,
    pub enrollment_id: String,
    pub host_id: Uuid,
    pub controller_url: String,
    pub jwt_file: PathBuf,
    pub data_directory: PathBuf,
    pub health_address: SocketAddr,
    pub maximum_concurrency: u32,
    pub heartbeat_interval: std::time::Duration,
    pub reconnect_maximum: std::time::Duration,
    pub shutdown_grace: std::time::Duration,
    pub staging_maximum_bytes: u64,
    pub backend: MockBackendConfig,
    pub allowed_command_template_digests: BTreeSet<String>,
    pub effective_config_digest: String,
    pub command_allowlist_digest: String,
    pub binary_digest: String,
    pub agent_worker: Option<WorkerProcessConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EffectiveConfigEvidence<'a> {
    version: u16,
    runner_id: &'a str,
    enrollment_id: &'a str,
    host_id: Uuid,
    controller_url: &'a str,
    data_directory: &'a Path,
    maximum_concurrency: u32,
    heartbeat_interval_ms: u64,
    staging_maximum_bytes: u64,
    backend: &'a MockBackendConfig,
    allowed_command_template_digests: &'a BTreeSet<String>,
    agent_worker: &'a Option<WorkerProcessConfig>,
}

impl RunnerConfig {
    pub fn load() -> Result<Self, String> {
        let path = env::var("LIGHT_WORKFLOW_RUNNER_CONFIG_FILE")
            .map_err(|_| "LIGHT_WORKFLOW_RUNNER_CONFIG_FILE is required".to_string())?;
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read runner config {path}: {error}"))?;
        let file = serde_yaml::from_str::<RunnerConfigFile>(&content)
            .map_err(|error| format!("failed to parse runner config {path}: {error}"))?;
        Self::from_file(file)
    }

    fn from_file(file: RunnerConfigFile) -> Result<Self, String> {
        if file.version != 1 {
            return Err(format!(
                "unsupported runner config version {}",
                file.version
            ));
        }
        validate_id("runnerId", &file.runner_id)?;
        validate_id("enrollmentId", &file.enrollment_id)?;
        let controller = url::Url::parse(&file.controller_url)
            .map_err(|error| format!("invalid controllerUrl: {error}"))?;
        if !matches!(controller.scheme(), "ws" | "wss") || controller.path() != "/ws/runner" {
            return Err("controllerUrl must be ws(s) and end at /ws/runner".to_string());
        }
        validate_token_file(&file.jwt_file)?;
        if file.maximum_concurrency == 0
            || file.heartbeat_interval_ms == 0
            || file.reconnect_maximum_ms == 0
            || file.shutdown_grace_ms == 0
            || file.staging_maximum_bytes == 0
            || file.backend.available_slots == 0
            || file.backend.available_slots > file.maximum_concurrency
        {
            return Err("runner concurrency, intervals, staging limit, and backend slots must be positive and consistent".to_string());
        }
        validate_digest(
            "backend compatibility digest",
            &file.backend.compatibility_digest,
        )?;
        let allowed = file
            .allowed_command_template_digests
            .into_iter()
            .map(|value| {
                validate_digest("command template digest", &value)?;
                Ok(value)
            })
            .collect::<Result<BTreeSet<_>, String>>()?;
        if allowed.is_empty() {
            return Err("allowedCommandTemplateDigests must not be empty".to_string());
        }
        if let Some(worker) = &file.agent_worker {
            worker.validate()?;
        }
        let evidence = EffectiveConfigEvidence {
            version: file.version,
            runner_id: &file.runner_id,
            enrollment_id: &file.enrollment_id,
            host_id: file.host_id,
            controller_url: &file.controller_url,
            data_directory: &file.data_directory,
            maximum_concurrency: file.maximum_concurrency,
            heartbeat_interval_ms: file.heartbeat_interval_ms,
            staging_maximum_bytes: file.staging_maximum_bytes,
            backend: &file.backend,
            allowed_command_template_digests: &allowed,
            agent_worker: &file.agent_worker,
        };
        let effective_config_digest = canonical_sha256(&evidence)
            .map_err(|error| format!("effective config digest failed: {error}"))?;
        let command_allowlist_digest = canonical_sha256(&allowed)
            .map_err(|error| format!("command allowlist digest failed: {error}"))?;
        let binary_digest = executable_digest()?;
        Ok(Self {
            runner_id: file.runner_id,
            enrollment_id: file.enrollment_id,
            host_id: file.host_id,
            controller_url: file.controller_url,
            jwt_file: file.jwt_file,
            data_directory: file.data_directory,
            health_address: file.health_address,
            maximum_concurrency: file.maximum_concurrency,
            heartbeat_interval: std::time::Duration::from_millis(file.heartbeat_interval_ms),
            reconnect_maximum: std::time::Duration::from_millis(file.reconnect_maximum_ms),
            shutdown_grace: std::time::Duration::from_millis(file.shutdown_grace_ms),
            staging_maximum_bytes: file.staging_maximum_bytes,
            backend: file.backend,
            allowed_command_template_digests: allowed,
            effective_config_digest,
            command_allowlist_digest,
            binary_digest,
            agent_worker: file.agent_worker,
        })
    }

    pub fn read_jwt(&self) -> Result<String, String> {
        validate_token_file(&self.jwt_file)?;
        let token = fs::read_to_string(&self.jwt_file)
            .map_err(|error| format!("failed to read jwtFile: {error}"))?;
        let token = token.trim();
        if token.is_empty() || token.contains(char::is_whitespace) {
            return Err("jwtFile must contain exactly one non-empty token".to_string());
        }
        Ok(token.to_string())
    }

    pub fn admission_document(
        &self,
        authenticated_subject: &str,
        origin_service_id: &str,
    ) -> Result<serde_json::Value, String> {
        validate_id("authenticated subject", authenticated_subject)?;
        validate_id("origin service ID", origin_service_id)?;
        let capability = MockExecutionBackend::new(
            self.backend.compatibility_digest.clone(),
            self.backend.behavior.clone(),
        )
        .with_available_slots(self.backend.available_slots)
        .capability();
        let mut origins = vec![serde_json::json!({
            "kind": OriginKind::Workflow,
            "serviceId": origin_service_id,
            "allowedSubjectKinds": [SubjectKind::WorkflowTask]
        })];
        if let Some(worker) = &self.agent_worker {
            origins.push(serde_json::json!({
                "kind": OriginKind::Agent,
                "serviceId": worker.origin_service_id,
                "allowedSubjectKinds": [SubjectKind::AgentTurn, SubjectKind::AgentAction]
            }));
        }
        Ok(serde_json::json!({
            "version": 1,
            "origins": origins,
            "enrollments": [{
                "enrollmentId": self.enrollment_id,
                "runnerId": self.runner_id,
                "authenticatedSubject": authenticated_subject,
                "hostId": self.host_id,
                "binaryDigest": self.binary_digest,
                "effectiveConfigDigest": self.effective_config_digest,
                "commandAllowlistDigest": self.command_allowlist_digest,
                "maximumConcurrency": self.maximum_concurrency,
                "heartbeatIntervalMs": u64::try_from(self.heartbeat_interval.as_millis())
                    .map_err(|_| "heartbeat interval does not fit admission schema".to_string())?,
                "backends": [{
                    "backendId": capability.backend_id,
                    "boundary": capability.boundary,
                    "hostExposure": capability.host_exposure,
                    "actions": capability.actions,
                    "features": capability.features,
                    "compatibilityDigest": capability.compatibility_digest,
                    "maximumSlots": self.backend.available_slots
                }]
            }]
        }))
    }
}

fn executable_digest() -> Result<String, String> {
    let path =
        env::current_exe().map_err(|error| format!("resolve current executable: {error}"))?;
    let bytes = fs::read(&path)
        .map_err(|error| format!("read current executable {}: {error}", path.display()))?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}

fn validate_id(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value != value.trim() || value.len() > 126 {
        return Err(format!("{name} must be a trimmed non-empty identifier"));
    }
    Ok(())
}

fn validate_digest(name: &str, value: &str) -> Result<(), String> {
    if value.len() < 16 || value.contains(char::is_whitespace) {
        return Err(format!("{name} is invalid"));
    }
    Ok(())
}

fn validate_token_file(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("jwtFile {} is unavailable: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("jwtFile {} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("jwtFile must not be accessible by group or other users".to_string());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_config_authority_is_rejected() {
        let value = r#"
version: 1
runnerId: runner
enrollmentId: enrollment
hostId: 00000000-0000-0000-0000-000000000000
controllerUrl: ws://localhost:8438/ws/runner
jwtFile: /tmp/token
dataDirectory: /tmp/runner
healthAddress: 127.0.0.1:9444
maximumConcurrency: 1
heartbeatIntervalMs: 1000
reconnectMaximumMs: 10000
shutdownGraceMs: 10000
stagingMaximumBytes: 1024
allowDockerSocket: true
backend:
  compatibilityDigest: sha256:mock-digest
  availableSlots: 1
allowedCommandTemplateDigests: [sha256:template-digest]
"#;
        assert!(serde_yaml::from_str::<RunnerConfigFile>(value).is_err());
    }

    #[test]
    fn admission_document_uses_exact_runtime_evidence() {
        let mut config = RunnerConfig {
            runner_id: "runner".into(),
            enrollment_id: "enrollment".into(),
            host_id: Uuid::nil(),
            controller_url: "ws://localhost:8438/ws/runner".into(),
            jwt_file: "/tmp/runner.jwt".into(),
            data_directory: "/tmp/runner".into(),
            health_address: "127.0.0.1:9444".parse().unwrap(),
            maximum_concurrency: 2,
            heartbeat_interval: std::time::Duration::from_millis(1234),
            reconnect_maximum: std::time::Duration::from_secs(10),
            shutdown_grace: std::time::Duration::from_secs(10),
            staging_maximum_bytes: 1024,
            backend: MockBackendConfig {
                compatibility_digest: "sha256:compatibility".into(),
                available_slots: 2,
                behavior: MockBehavior::default(),
            },
            allowed_command_template_digests: BTreeSet::from(["sha256:template".into()]),
            effective_config_digest: "sha256:effective".into(),
            command_allowlist_digest: "sha256:allowlist".into(),
            binary_digest: "sha256:binary".into(),
            agent_worker: None,
        };
        let document = config
            .admission_document("runner-subject", "light-workflow")
            .unwrap();
        let enrollment = &document["enrollments"][0];
        assert_eq!(enrollment["binaryDigest"], "sha256:binary");
        assert_eq!(enrollment["effectiveConfigDigest"], "sha256:effective");
        assert_eq!(enrollment["commandAllowlistDigest"], "sha256:allowlist");
        assert_eq!(enrollment["heartbeatIntervalMs"], 1234);
        assert_eq!(enrollment["backends"][0]["maximumSlots"], 2);

        config.agent_worker = Some(WorkerProcessConfig {
            origin_service_id: "light-agent".into(),
            executable: "/usr/local/bin/light-agent-worker".into(),
            binary_digest: format!("sha256:{}", "1".repeat(64)),
            capability_digest: format!("sha256:{}", "2".repeat(64)),
        });
        let document = config
            .admission_document("runner-subject", "light-workflow")
            .unwrap();
        assert_eq!(document["origins"][1]["kind"], "agent");
        assert_eq!(document["origins"][1]["serviceId"], "light-agent");
        assert_eq!(
            document["origins"][1]["allowedSubjectKinds"],
            serde_json::json!(["agent-turn", "agent-action"])
        );
    }
}
