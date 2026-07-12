use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::worker_process::WorkerProcessConfig;
use execution_backend::ExecutionBackend;
use execution_backend_cube::{
    CubeBackendConfig, CubeExecutionBackend, CubeHttpClient, CubeHttpClientConfig,
};
use execution_backend_mock::MockBehavior;
use execution_backend_mock::MockExecutionBackend;
use execution_backend_oci::{OciBackendConfig, OciExecutionBackend, OciRuntime};
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
    backend: RunnerBackendConfig,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CubeImplementation {
    Cube,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CubeRunnerBackendConfig {
    pub implementation: CubeImplementation,
    pub api_url: String,
    #[serde(default)]
    pub sandbox_url: Option<String>,
    pub api_key_file: PathBuf,
    #[serde(default)]
    pub tls_ca_file: Option<PathBuf>,
    #[serde(default)]
    pub allow_insecure_http: bool,
    pub template_id: String,
    pub compatibility_digest: String,
    pub available_slots: u32,
    pub maximum_native_ttl_seconds: u64,
    #[serde(default = "default_cube_discovery_page_limit")]
    pub discovery_page_limit: usize,
    #[serde(default = "default_cube_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default = "default_cube_maximum_response_bytes")]
    pub maximum_response_bytes: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OciImplementation {
    Docker,
    RootlessOci,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OciRunnerBackendConfig {
    pub implementation: OciImplementation,
    pub binary: PathBuf,
    pub image: String,
    pub compatibility_digest: String,
    pub available_slots: u32,
    pub maximum_memory_bytes: u64,
    pub maximum_pids: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunnerBackendConfig {
    Cube(CubeRunnerBackendConfig),
    Oci(OciRunnerBackendConfig),
    Mock(MockBackendConfig),
}

impl RunnerBackendConfig {
    fn available_slots(&self) -> u32 {
        match self {
            Self::Mock(config) => config.available_slots,
            Self::Cube(config) => config.available_slots,
            Self::Oci(config) => config.available_slots,
        }
    }

    fn compatibility_digest(&self) -> &str {
        match self {
            Self::Mock(config) => &config.compatibility_digest,
            Self::Cube(config) => &config.compatibility_digest,
            Self::Oci(config) => &config.compatibility_digest,
        }
    }
}

fn default_cube_discovery_page_limit() -> usize {
    200
}
fn default_cube_request_timeout_ms() -> u64 {
    30_000
}
fn default_cube_maximum_response_bytes() -> usize {
    4 * 1024 * 1024
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
    pub backend: RunnerBackendConfig,
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
    backend: &'a RunnerBackendConfig,
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
            || file.backend.available_slots() == 0
            || file.backend.available_slots() > file.maximum_concurrency
        {
            return Err("runner concurrency, intervals, staging limit, and backend slots must be positive and consistent".to_string());
        }
        validate_digest(
            "backend compatibility digest",
            file.backend.compatibility_digest(),
        )?;
        if let RunnerBackendConfig::Cube(cube) = &file.backend {
            validate_cube_backend(cube)?;
        }
        if let RunnerBackendConfig::Oci(oci) = &file.backend {
            validate_oci_backend(oci)?;
        }
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
        read_secret_file(&self.jwt_file, "jwtFile")
    }

    pub fn build_backend(&self) -> Result<std::sync::Arc<dyn ExecutionBackend>, String> {
        match &self.backend {
            RunnerBackendConfig::Mock(config) => Ok(std::sync::Arc::new(
                MockExecutionBackend::new(
                    config.compatibility_digest.clone(),
                    config.behavior.clone(),
                )
                .with_available_slots(config.available_slots),
            )),
            RunnerBackendConfig::Cube(config) => {
                let api_key = read_secret_file(&config.api_key_file, "backend.apiKeyFile")?;
                let tls_ca_pem = config
                    .tls_ca_file
                    .as_ref()
                    .map(|path| {
                        fs::read(path).map_err(|error| {
                            format!("read backend.tlsCaFile {}: {error}", path.display())
                        })
                    })
                    .transpose()?;
                let client = CubeHttpClient::new(CubeHttpClientConfig {
                    api_url: url::Url::parse(&config.api_url)
                        .map_err(|error| format!("invalid backend.apiUrl: {error}"))?,
                    sandbox_url: config
                        .sandbox_url
                        .as_deref()
                        .map(url::Url::parse)
                        .transpose()
                        .map_err(|error| format!("invalid backend.sandboxUrl: {error}"))?,
                    api_key,
                    request_timeout: std::time::Duration::from_millis(config.request_timeout_ms),
                    maximum_response_bytes: config.maximum_response_bytes,
                    allow_insecure_http: config.allow_insecure_http,
                    tls_ca_pem,
                })?;
                Ok(std::sync::Arc::new(CubeExecutionBackend::new(
                    std::sync::Arc::new(client),
                    CubeBackendConfig {
                        template_id: config.template_id.clone(),
                        compatibility_digest: config.compatibility_digest.clone(),
                        owner_runner: self.runner_id.clone(),
                        available_slots: config.available_slots,
                        maximum_native_ttl_seconds: config.maximum_native_ttl_seconds,
                        discovery_page_limit: config.discovery_page_limit,
                    },
                )))
            }
            RunnerBackendConfig::Oci(config) => Ok(std::sync::Arc::new(
                OciExecutionBackend::new(oci_backend_config(config, &self.runner_id))
                    .map_err(|error| error.to_string())?,
            )),
        }
    }

    pub fn admission_document(
        &self,
        authenticated_subject: &str,
        origin_service_id: &str,
    ) -> Result<serde_json::Value, String> {
        validate_id("authenticated subject", authenticated_subject)?;
        validate_id("origin service ID", origin_service_id)?;
        let capability = match &self.backend {
            RunnerBackendConfig::Mock(config) => MockExecutionBackend::new(
                config.compatibility_digest.clone(),
                config.behavior.clone(),
            )
            .with_available_slots(config.available_slots)
            .capability(),
            RunnerBackendConfig::Cube(config) => CubeBackendConfig {
                template_id: config.template_id.clone(),
                compatibility_digest: config.compatibility_digest.clone(),
                owner_runner: self.runner_id.clone(),
                available_slots: config.available_slots,
                maximum_native_ttl_seconds: config.maximum_native_ttl_seconds,
                discovery_page_limit: config.discovery_page_limit,
            }
            .capability(),
            RunnerBackendConfig::Oci(config) => {
                OciExecutionBackend::new(oci_backend_config(config, &self.runner_id))
                    .map_err(|error| error.to_string())?
                    .capability()
            }
        };
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
                    "maximumSlots": self.backend.available_slots()
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

fn validate_cube_backend(config: &CubeRunnerBackendConfig) -> Result<(), String> {
    if config.template_id.is_empty()
        || config.maximum_native_ttl_seconds == 0
        || config.discovery_page_limit == 0
        || config.discovery_page_limit > 200
        || config.request_timeout_ms == 0
        || config.maximum_response_bytes == 0
    {
        return Err("Cube template, TTL, discovery, timeout, and response limits must be positive and bounded".into());
    }
    url::Url::parse(&config.api_url).map_err(|error| format!("invalid backend.apiUrl: {error}"))?;
    if let Some(url) = &config.sandbox_url {
        url::Url::parse(url).map_err(|error| format!("invalid backend.sandboxUrl: {error}"))?;
    }
    validate_secret_file(&config.api_key_file, "backend.apiKeyFile")?;
    if let Some(path) = &config.tls_ca_file {
        let metadata = fs::metadata(path).map_err(|error| {
            format!(
                "backend.tlsCaFile {} is unavailable: {error}",
                path.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(format!(
                "backend.tlsCaFile {} is not a regular file",
                path.display()
            ));
        }
    }
    Ok(())
}

fn oci_backend_config(config: &OciRunnerBackendConfig, owner_runner: &str) -> OciBackendConfig {
    OciBackendConfig {
        runtime: match config.implementation {
            OciImplementation::Docker => OciRuntime::Docker,
            OciImplementation::RootlessOci => OciRuntime::Podman,
        },
        binary: config.binary.clone(),
        image: config.image.clone(),
        compatibility_digest: config.compatibility_digest.clone(),
        available_slots: config.available_slots,
        rootless: matches!(config.implementation, OciImplementation::RootlessOci),
        maximum_memory_bytes: config.maximum_memory_bytes,
        maximum_pids: config.maximum_pids,
        owner_runner: owner_runner.to_string(),
    }
}

fn validate_oci_backend(config: &OciRunnerBackendConfig) -> Result<(), String> {
    OciExecutionBackend::new(oci_backend_config(config, "configuration-validation"))
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_token_file(path: &Path) -> Result<(), String> {
    validate_secret_file(path, "jwtFile")
}

fn read_secret_file(path: &Path, name: &str) -> Result<String, String> {
    validate_secret_file(path, name)?;
    let token = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {name} {}: {error}", path.display()))?;
    let token = token.trim();
    if token.is_empty() || token.contains(char::is_whitespace) {
        return Err(format!("{name} must contain exactly one non-empty token"));
    }
    Ok(token.to_string())
}

fn validate_secret_file(path: &Path, name: &str) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("{name} {} is unavailable: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{name} {} is not a regular file", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(format!(
                "{name} must not be accessible by group or other users"
            ));
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
            backend: RunnerBackendConfig::Mock(MockBackendConfig {
                compatibility_digest: "sha256:compatibility".into(),
                available_slots: 2,
                behavior: MockBehavior::default(),
            }),
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

        config.backend = RunnerBackendConfig::Cube(CubeRunnerBackendConfig {
            implementation: CubeImplementation::Cube,
            api_url: "https://cube.example/".into(),
            sandbox_url: Some("https://sandbox.example/".into()),
            api_key_file: "/tmp/cube.key".into(),
            tls_ca_file: None,
            allow_insecure_http: false,
            template_id: "immutable-template".into(),
            compatibility_digest: "sha256:cube-compatible".into(),
            available_slots: 2,
            maximum_native_ttl_seconds: 300,
            discovery_page_limit: 200,
            request_timeout_ms: 30_000,
            maximum_response_bytes: 4 * 1024 * 1024,
        });
        let document = config
            .admission_document("runner-subject", "light-workflow")
            .unwrap();
        assert_eq!(
            document["enrollments"][0]["backends"][0]["backendId"],
            "cube"
        );
        assert_eq!(
            document["enrollments"][0]["backends"][0]["boundary"],
            "micro-vm"
        );
        assert_eq!(document["enrollments"][0]["backends"][0]["maximumSlots"], 2);

        config.backend = RunnerBackendConfig::Oci(OciRunnerBackendConfig {
            implementation: OciImplementation::RootlessOci,
            binary: "/bin/true".into(),
            image: format!("registry.example/execution@sha256:{}", "a".repeat(64)),
            compatibility_digest: "sha256:rootless-compatible".into(),
            available_slots: 2,
            maximum_memory_bytes: 1024 * 1024,
            maximum_pids: 32,
        });
        let document = config
            .admission_document("runner-subject", "light-workflow")
            .unwrap();
        assert_eq!(
            document["enrollments"][0]["backends"][0]["backendId"],
            "rootless-oci"
        );
        assert_eq!(
            document["enrollments"][0]["backends"][0]["boundary"],
            "user-namespace"
        );
        assert_eq!(
            document["enrollments"][0]["backends"][0]["hostExposure"],
            "explicit-mounts"
        );
    }
}
