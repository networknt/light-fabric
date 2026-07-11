use execution_runner_protocol::{
    BackendCapability, ExecutionRequirements, HostExposure, IsolationBoundary, canonical_sha256,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};
use thiserror::Error;

pub const POLICY_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionPlacement {
    Host,
    Runner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    Ask,
    Assert,
    Set,
    Switch,
    CallAgent,
    CallHttp,
    CallMcp,
    RunShell,
    RunContainer,
    RunScript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkloadTrust {
    TrustedTemplate,
    TenantAuthored,
    ModelGenerated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NetworkMode {
    DenyAll,
    Allowlisted,
}

impl Default for NetworkMode {
    fn default() -> Self {
        Self::DenyAll
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PersistenceMode {
    Ephemeral,
    Session,
}

impl Default for PersistenceMode {
    fn default() -> Self {
        Self::Ephemeral
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowSecurityPolicy {
    pub version: u16,
    #[serde(default)]
    pub placement: Option<ExecutionPlacement>,
    #[serde(default)]
    pub execution_profile_id: Option<String>,
    #[serde(default)]
    pub minimum_boundary: Option<IsolationBoundary>,
    #[serde(default)]
    pub maximum_host_exposure: Option<HostExposure>,
    #[serde(default)]
    pub workload_trust: Option<WorkloadTrust>,
    #[serde(default)]
    pub network: NetworkMode,
    #[serde(default)]
    pub credential_classes: Vec<String>,
    #[serde(default)]
    pub persistence: PersistenceMode,
    #[serde(default)]
    pub artifact_export: bool,
    #[serde(default)]
    pub approval_required: bool,
    #[serde(default)]
    pub protected_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionProfile {
    pub id: String,
    pub version: u32,
    pub boundary: IsolationBoundary,
    pub host_exposure: HostExposure,
    pub maximum_workload_trust: WorkloadTrust,
    pub network: NetworkMode,
    #[serde(default)]
    pub credential_classes: Vec<String>,
    pub persistence: PersistenceMode,
    pub artifact_export: bool,
    pub approval_supported: bool,
    #[serde(default)]
    pub protected_paths: Vec<String>,
    pub compatibility_digest: String,
    #[serde(default)]
    pub allowed_actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandTemplate {
    pub id: String,
    pub version: u32,
    pub executable: String,
    #[serde(default)]
    pub fixed_arguments: Vec<String>,
    #[serde(default)]
    pub parameter_slots: Vec<CommandParameterSlot>,
    pub working_directory: String,
    #[serde(default)]
    pub allowed_environment_names: Vec<String>,
    pub wall_clock_timeout_ms: u64,
    pub stdout_limit_bytes: u64,
    pub stderr_limit_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandParameterSlot {
    pub name: String,
    pub argument_index: usize,
    pub required: bool,
    #[serde(default)]
    pub allowed_values: Vec<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    pub maximum_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResolvedExecutionPolicy {
    pub policy_version: u16,
    pub placement: ExecutionPlacement,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<ExecutionProfile>,
    pub action_kind: String,
    pub minimum_boundary: IsolationBoundary,
    pub maximum_host_exposure: HostExposure,
    pub workload_trust: WorkloadTrust,
    pub network: NetworkMode,
    pub credential_classes: Vec<String>,
    pub persistence: PersistenceMode,
    pub artifact_export: bool,
    pub approval_required: bool,
    pub protected_paths: Vec<String>,
    pub policy_digest: String,
}

impl ResolvedExecutionPolicy {
    pub fn requirements(&self) -> Option<ExecutionRequirements> {
        let profile = self.profile.as_ref()?;
        Some(ExecutionRequirements {
            action_kind: self.action_kind.clone(),
            minimum_boundary: self.minimum_boundary,
            maximum_host_exposure: self.maximum_host_exposure,
            network_enabled: self.network != NetworkMode::DenyAll,
            credential_classes: self.credential_classes.clone(),
            persistent_workspace: self.persistence == PersistenceMode::Session,
            required_features: required_features(self),
            policy_digest: self.policy_digest.clone(),
            compatibility_digest: profile.compatibility_digest.clone(),
        })
    }
}

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("metadata.lightWorkflow.security must be an object")]
    InvalidSecurityObject,
    #[error("failed to parse metadata.lightWorkflow.security: {0}")]
    Parse(#[from] serde_yaml::Error),
    #[error("unsupported workflow security policy version {0}")]
    UnsupportedVersion(u16),
    #[error("task action `{0}` requires runner placement")]
    RunnerRequired(String),
    #[error("runner placement requires executionProfileId")]
    MissingProfile,
    #[error("execution profile `{0}` is not configured")]
    UnknownProfile(String),
    #[error("execution profile `{profile}` does not allow action `{action}`")]
    ActionDenied { profile: String, action: String },
    #[error("execution profile `{0}` provides a weaker isolation boundary than requested")]
    BoundaryTooWeak(String),
    #[error("execution profile `{0}` exposes more of the host than requested")]
    HostExposureTooBroad(String),
    #[error("execution profile `{0}` does not support requested workload trust")]
    WorkloadTrustTooWeak(String),
    #[error("execution profile `{0}` does not permit network access")]
    NetworkDenied(String),
    #[error("execution profile `{profile}` does not permit credential class `{credential}")]
    CredentialDenied { profile: String, credential: String },
    #[error("execution profile `{0}` does not support session persistence")]
    PersistenceDenied(String),
    #[error("execution profile `{0}` does not support artifact export")]
    ArtifactExportDenied(String),
    #[error("execution profile `{0}` does not support approval handoff")]
    ApprovalDenied(String),
    #[error("protected path `{path}` is not enforced by execution profile `{profile}`")]
    ProtectedPathMissing { profile: String, path: String },
    #[error("failed to compute policy digest: {0}")]
    Digest(String),
}

pub fn parse_security_policy(
    workflow_document: &serde_yaml::Value,
) -> Result<Option<WorkflowSecurityPolicy>, PolicyError> {
    let Some(metadata) = mapping_value(workflow_document, "metadata") else {
        return Ok(None);
    };
    let Some(light_workflow) = mapping_value(metadata, "lightWorkflow") else {
        return Ok(None);
    };
    let Some(security) = mapping_value(light_workflow, "security") else {
        return Ok(None);
    };
    if !security.is_mapping() {
        return Err(PolicyError::InvalidSecurityObject);
    }
    let policy = serde_yaml::from_value::<WorkflowSecurityPolicy>(security.clone())?;
    if policy.version != POLICY_VERSION {
        return Err(PolicyError::UnsupportedVersion(policy.version));
    }
    Ok(Some(policy))
}

fn mapping_value<'a>(value: &'a serde_yaml::Value, key: &str) -> Option<&'a serde_yaml::Value> {
    value
        .as_mapping()
        .and_then(|mapping| mapping.get(serde_yaml::Value::String(key.to_string())))
}

pub fn resolve_policy(
    task_kind: TaskKind,
    security: Option<&WorkflowSecurityPolicy>,
    profiles: &BTreeMap<String, ExecutionProfile>,
) -> Result<ResolvedExecutionPolicy, PolicyError> {
    let action_kind = action_kind(task_kind).to_string();
    let default_placement = default_placement(task_kind);
    let requested_placement = if is_pure_orchestration(task_kind) {
        ExecutionPlacement::Host
    } else {
        security
            .and_then(|security| security.placement)
            .unwrap_or(default_placement)
    };
    if requires_runner(task_kind) && requested_placement != ExecutionPlacement::Runner {
        return Err(PolicyError::RunnerRequired(action_kind));
    }

    let minimum_boundary = security
        .and_then(|security| security.minimum_boundary)
        .unwrap_or(IsolationBoundary::Process);
    let maximum_host_exposure = security
        .and_then(|security| security.maximum_host_exposure)
        .unwrap_or(HostExposure::None);
    let workload_trust = security
        .and_then(|security| security.workload_trust)
        .unwrap_or(WorkloadTrust::TrustedTemplate);
    let network = security
        .map(|security| security.network)
        .unwrap_or_default();
    let credential_classes = normalized_strings(
        security
            .map(|security| security.credential_classes.as_slice())
            .unwrap_or_default(),
    );
    let persistence = security
        .map(|security| security.persistence)
        .unwrap_or_default();
    let artifact_export = security.is_some_and(|security| security.artifact_export);
    let approval_required = security.is_some_and(|security| security.approval_required);
    let protected_paths = normalized_strings(
        security
            .map(|security| security.protected_paths.as_slice())
            .unwrap_or_default(),
    );

    let profile = if requested_placement == ExecutionPlacement::Runner {
        let profile_id = security
            .and_then(|security| security.execution_profile_id.as_deref())
            .filter(|profile| !profile.trim().is_empty())
            .ok_or(PolicyError::MissingProfile)?;
        let profile = profiles
            .get(profile_id)
            .cloned()
            .ok_or_else(|| PolicyError::UnknownProfile(profile_id.to_string()))?;
        validate_profile(
            &profile,
            &action_kind,
            minimum_boundary,
            maximum_host_exposure,
            workload_trust,
            network,
            &credential_classes,
            persistence,
            artifact_export,
            approval_required,
            &protected_paths,
        )?;
        Some(profile)
    } else {
        None
    };

    let mut resolved = ResolvedExecutionPolicy {
        policy_version: POLICY_VERSION,
        placement: requested_placement,
        profile,
        action_kind,
        minimum_boundary,
        maximum_host_exposure,
        workload_trust,
        network,
        credential_classes,
        persistence,
        artifact_export,
        approval_required,
        protected_paths,
        policy_digest: String::new(),
    };
    resolved.policy_digest =
        canonical_sha256(&resolved).map_err(|error| PolicyError::Digest(error.to_string()))?;
    Ok(resolved)
}

#[allow(clippy::too_many_arguments)]
fn validate_profile(
    profile: &ExecutionProfile,
    action_kind: &str,
    minimum_boundary: IsolationBoundary,
    maximum_host_exposure: HostExposure,
    workload_trust: WorkloadTrust,
    network: NetworkMode,
    credential_classes: &[String],
    persistence: PersistenceMode,
    artifact_export: bool,
    approval_required: bool,
    protected_paths: &[String],
) -> Result<(), PolicyError> {
    if !profile
        .allowed_actions
        .iter()
        .any(|allowed| allowed == action_kind)
    {
        return Err(PolicyError::ActionDenied {
            profile: profile.id.clone(),
            action: action_kind.to_string(),
        });
    }
    if profile.boundary < minimum_boundary {
        return Err(PolicyError::BoundaryTooWeak(profile.id.clone()));
    }
    if profile.host_exposure > maximum_host_exposure {
        return Err(PolicyError::HostExposureTooBroad(profile.id.clone()));
    }
    if workload_trust_rank(profile.maximum_workload_trust) < workload_trust_rank(workload_trust) {
        return Err(PolicyError::WorkloadTrustTooWeak(profile.id.clone()));
    }
    if network == NetworkMode::Allowlisted && profile.network == NetworkMode::DenyAll {
        return Err(PolicyError::NetworkDenied(profile.id.clone()));
    }
    let allowed_credentials = profile
        .credential_classes
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for credential in credential_classes {
        if !allowed_credentials.contains(credential.as_str()) {
            return Err(PolicyError::CredentialDenied {
                profile: profile.id.clone(),
                credential: credential.clone(),
            });
        }
    }
    if persistence == PersistenceMode::Session && profile.persistence != PersistenceMode::Session {
        return Err(PolicyError::PersistenceDenied(profile.id.clone()));
    }
    if artifact_export && !profile.artifact_export {
        return Err(PolicyError::ArtifactExportDenied(profile.id.clone()));
    }
    if approval_required && !profile.approval_supported {
        return Err(PolicyError::ApprovalDenied(profile.id.clone()));
    }
    for path in protected_paths {
        if !profile
            .protected_paths
            .iter()
            .any(|profile_path| protected_path_covers(profile_path, path))
        {
            return Err(PolicyError::ProtectedPathMissing {
                profile: profile.id.clone(),
                path: path.clone(),
            });
        }
    }
    Ok(())
}

fn protected_path_covers(profile_path: &str, requested_path: &str) -> bool {
    let Some(profile) = normalized_workspace_path(profile_path) else {
        return false;
    };
    let Some(requested) = normalized_workspace_path(requested_path) else {
        return false;
    };
    requested.starts_with(&profile)
}

fn normalized_workspace_path(value: &str) -> Option<Vec<String>> {
    let path = Path::new(value.trim());
    if value.trim().is_empty() {
        return None;
    }
    let mut components = if path.is_absolute() {
        Vec::new()
    } else {
        vec!["workspace".to_string()]
    };
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(value) => components.push(value.to_str()?.to_string()),
            Component::ParentDir | Component::Prefix(_) => return None,
        }
    }
    Some(components)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncompatibilityReason {
    pub code: &'static str,
    pub message: String,
}

pub fn match_backend(
    requirements: &ExecutionRequirements,
    backend: &BackendCapability,
) -> Result<(), IncompatibilityReason> {
    if !backend.healthy || backend.available_slots == 0 {
        return Err(IncompatibilityReason {
            code: "backend_unavailable",
            message: format!("backend {} has no healthy capacity", backend.backend_id),
        });
    }
    if backend.boundary < requirements.minimum_boundary {
        return Err(IncompatibilityReason {
            code: "boundary_too_weak",
            message: format!("backend {} isolation is too weak", backend.backend_id),
        });
    }
    if backend.host_exposure > requirements.maximum_host_exposure {
        return Err(IncompatibilityReason {
            code: "host_exposure_too_broad",
            message: format!("backend {} exposes too much host state", backend.backend_id),
        });
    }
    if backend.compatibility_digest != requirements.compatibility_digest {
        return Err(IncompatibilityReason {
            code: "compatibility_digest_mismatch",
            message: format!(
                "backend {} compatibility digest differs",
                backend.backend_id
            ),
        });
    }
    if !backend
        .actions
        .iter()
        .any(|action| action == &requirements.action_kind)
    {
        return Err(IncompatibilityReason {
            code: "action_unsupported",
            message: format!(
                "backend {} does not support {}",
                backend.backend_id, requirements.action_kind
            ),
        });
    }
    for feature in &requirements.required_features {
        if !backend
            .features
            .iter()
            .any(|available| available == feature)
        {
            return Err(IncompatibilityReason {
                code: "feature_unsupported",
                message: format!("backend {} lacks feature {feature}", backend.backend_id),
            });
        }
    }
    Ok(())
}

fn action_kind(task_kind: TaskKind) -> &'static str {
    match task_kind {
        TaskKind::Ask => "ask",
        TaskKind::Assert => "assert",
        TaskKind::Set => "set",
        TaskKind::Switch => "switch",
        TaskKind::CallAgent => "call.agent",
        TaskKind::CallHttp => "call.http",
        TaskKind::CallMcp => "call.mcp",
        TaskKind::RunShell => "run.shell",
        TaskKind::RunContainer => "run.container",
        TaskKind::RunScript => "run.script",
    }
}

fn requires_runner(task_kind: TaskKind) -> bool {
    matches!(
        task_kind,
        TaskKind::RunShell | TaskKind::RunContainer | TaskKind::RunScript
    )
}

fn is_pure_orchestration(task_kind: TaskKind) -> bool {
    matches!(
        task_kind,
        TaskKind::Ask | TaskKind::Assert | TaskKind::Set | TaskKind::Switch
    )
}

fn default_placement(task_kind: TaskKind) -> ExecutionPlacement {
    if requires_runner(task_kind) {
        ExecutionPlacement::Runner
    } else {
        ExecutionPlacement::Host
    }
}

fn workload_trust_rank(trust: WorkloadTrust) -> u8 {
    match trust {
        WorkloadTrust::TrustedTemplate => 0,
        WorkloadTrust::TenantAuthored => 1,
        WorkloadTrust::ModelGenerated => 2,
    }
}

fn normalized_strings(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn required_features(policy: &ResolvedExecutionPolicy) -> Vec<String> {
    let mut features = BTreeSet::new();
    if policy.network == NetworkMode::Allowlisted {
        features.insert("network-allowlist".to_string());
    }
    if !policy.credential_classes.is_empty() {
        features.insert("credential-broker".to_string());
    }
    if policy.persistence == PersistenceMode::Session {
        features.insert("session-workspace".to_string());
    }
    if policy.artifact_export {
        features.insert("artifact-export".to_string());
    }
    features.into_iter().collect()
}

pub fn policy_snapshot(policy: &ResolvedExecutionPolicy) -> Result<Value, PolicyError> {
    serde_json::to_value(policy).map_err(|error| PolicyError::Digest(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/v1");

    fn fixture(path: &str) -> String {
        std::fs::read_to_string(format!("{FIXTURES}/{path}"))
            .unwrap_or_else(|error| panic!("read fixture {path}: {error}"))
    }

    fn mock_profile() -> ExecutionProfile {
        ExecutionProfile {
            id: "mock-ephemeral".into(),
            version: 1,
            boundary: IsolationBoundary::Container,
            host_exposure: HostExposure::None,
            maximum_workload_trust: WorkloadTrust::TenantAuthored,
            network: NetworkMode::DenyAll,
            credential_classes: Vec::new(),
            persistence: PersistenceMode::Ephemeral,
            artifact_export: false,
            approval_supported: false,
            protected_paths: vec![".github/workflows".into()],
            compatibility_digest: "sha256:mock".into(),
            allowed_actions: vec!["run.shell".into()],
        }
    }

    #[test]
    fn absent_security_keeps_existing_tasks_on_host() {
        let policy = resolve_policy(TaskKind::CallMcp, None, &BTreeMap::new()).unwrap();
        assert_eq!(policy.placement, ExecutionPlacement::Host);
        assert!(policy.profile.is_none());
    }

    #[test]
    fn pure_orchestration_cannot_be_moved_to_a_runner() {
        let profile = mock_profile();
        let profiles = BTreeMap::from([(profile.id.clone(), profile)]);
        let policy = WorkflowSecurityPolicy {
            version: 1,
            placement: Some(ExecutionPlacement::Runner),
            execution_profile_id: Some("mock-ephemeral".into()),
            minimum_boundary: Some(IsolationBoundary::Container),
            maximum_host_exposure: Some(HostExposure::None),
            workload_trust: Some(WorkloadTrust::TenantAuthored),
            network: NetworkMode::DenyAll,
            credential_classes: Vec::new(),
            persistence: PersistenceMode::Ephemeral,
            artifact_export: false,
            approval_required: false,
            protected_paths: Vec::new(),
        };

        let resolved = resolve_policy(TaskKind::Set, Some(&policy), &profiles).unwrap();
        assert_eq!(resolved.placement, ExecutionPlacement::Host);
        assert!(resolved.profile.is_none());
    }

    #[test]
    fn run_shell_requires_a_known_runner_profile() {
        let policy = WorkflowSecurityPolicy {
            version: 1,
            placement: Some(ExecutionPlacement::Runner),
            execution_profile_id: Some("missing".into()),
            minimum_boundary: Some(IsolationBoundary::Container),
            maximum_host_exposure: Some(HostExposure::None),
            workload_trust: Some(WorkloadTrust::TenantAuthored),
            network: NetworkMode::DenyAll,
            credential_classes: Vec::new(),
            persistence: PersistenceMode::Ephemeral,
            artifact_export: false,
            approval_required: false,
            protected_paths: Vec::new(),
        };

        assert!(matches!(
            resolve_policy(TaskKind::RunShell, Some(&policy), &BTreeMap::new()),
            Err(PolicyError::UnknownProfile(_))
        ));
    }

    #[test]
    fn strict_metadata_rejects_unknown_security_keys() {
        let workflow = serde_yaml::from_str::<serde_yaml::Value>(
            r#"
document:
  dsl: 1.0.0
metadata:
  lightWorkflow:
    security:
      version: 1
      allowHostDockerSocket: true
"#,
        )
        .unwrap();

        assert!(parse_security_policy(&workflow).is_err());
    }

    #[test]
    fn valid_profile_resolves_to_stable_requirements() {
        let profile = mock_profile();
        let profiles = BTreeMap::from([(profile.id.clone(), profile)]);
        let policy = WorkflowSecurityPolicy {
            version: 1,
            placement: Some(ExecutionPlacement::Runner),
            execution_profile_id: Some("mock-ephemeral".into()),
            minimum_boundary: Some(IsolationBoundary::Container),
            maximum_host_exposure: Some(HostExposure::None),
            workload_trust: Some(WorkloadTrust::TenantAuthored),
            network: NetworkMode::DenyAll,
            credential_classes: Vec::new(),
            persistence: PersistenceMode::Ephemeral,
            artifact_export: false,
            approval_required: false,
            protected_paths: vec![".github/workflows".into()],
        };

        let first = resolve_policy(TaskKind::RunShell, Some(&policy), &profiles).unwrap();
        let second = resolve_policy(TaskKind::RunShell, Some(&policy), &profiles).unwrap();

        assert_eq!(first.policy_digest, second.policy_digest);
        assert_eq!(first.requirements().unwrap().action_kind, "run.shell");
    }

    #[test]
    fn policy_cannot_weaken_operator_profile() {
        let profile = mock_profile();
        let profiles = BTreeMap::from([(profile.id.clone(), profile)]);
        let policy = WorkflowSecurityPolicy {
            version: 1,
            placement: Some(ExecutionPlacement::Runner),
            execution_profile_id: Some("mock-ephemeral".into()),
            minimum_boundary: Some(IsolationBoundary::MicroVm),
            maximum_host_exposure: Some(HostExposure::None),
            workload_trust: Some(WorkloadTrust::TenantAuthored),
            network: NetworkMode::DenyAll,
            credential_classes: Vec::new(),
            persistence: PersistenceMode::Ephemeral,
            artifact_export: false,
            approval_required: false,
            protected_paths: Vec::new(),
        };

        assert!(matches!(
            resolve_policy(TaskKind::RunShell, Some(&policy), &profiles),
            Err(PolicyError::BoundaryTooWeak(_))
        ));
    }

    #[test]
    fn protected_paths_use_component_aware_subsumption() {
        assert!(protected_path_covers("/", ".github/workflows"));
        assert!(protected_path_covers("/workspace", ".github/workflows"));
        assert!(protected_path_covers(
            ".github",
            ".github/workflows/release.yml"
        ));
        assert!(protected_path_covers(
            ".github/workflows",
            ".github/workflows"
        ));
        assert!(!protected_path_covers("/workspace", "/workspace-old/file"));
        assert!(!protected_path_covers(
            ".github/workflows",
            ".github/actions"
        ));
        assert!(!protected_path_covers("/workspace", "../etc/passwd"));
    }

    #[test]
    fn published_v1_schema_and_valid_fixtures_track_rust_contract() {
        let schema = include_str!("../schema/workflow-execution-policy-v1.schema.json");
        let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
        assert_eq!(
            schema["$schema"],
            "https://json-schema.org/draft/2020-12/schema"
        );
        assert_eq!(
            schema["$defs"]["workflowSecurityPolicy"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schema["$defs"]["commandParameterSlot"]["properties"]["pattern"]["format"],
            "regex"
        );
        assert_eq!(
            schema["$defs"]["sha256Digest"]["pattern"],
            "^sha256:[a-fA-F0-9]{64}$"
        );

        let security: WorkflowSecurityPolicy =
            serde_json::from_str(&fixture("valid/security-run-shell.json")).unwrap();
        let profile: ExecutionProfile =
            serde_json::from_str(&fixture("valid/profile-mock.json")).unwrap();
        let template: CommandTemplate =
            serde_json::from_str(&fixture("valid/template-print-message.json")).unwrap();

        assert_eq!(security.version, POLICY_VERSION);
        assert_eq!(profile.allowed_actions, ["run.shell"]);
        assert_eq!(template.executable, "/usr/bin/printf");
        let profiles = BTreeMap::from([(profile.id.clone(), profile)]);
        resolve_policy(TaskKind::RunShell, Some(&security), &profiles).unwrap();
    }

    #[test]
    fn invalid_v1_fixtures_are_rejected_or_fail_closed() {
        assert!(
            serde_json::from_str::<WorkflowSecurityPolicy>(&fixture(
                "invalid/security-unknown-authority.json"
            ))
            .is_err()
        );
        let version: WorkflowSecurityPolicy =
            serde_json::from_str(&fixture("invalid/security-unsupported-version.json")).unwrap();
        assert_ne!(version.version, POLICY_VERSION);
        assert!(
            serde_json::from_str::<ExecutionProfile>(&fixture(
                "invalid/profile-unknown-field.json"
            ))
            .is_err()
        );
        let template: CommandTemplate =
            serde_json::from_str(&fixture("invalid/template-relative-executable.json")).unwrap();
        assert!(!template.executable.starts_with('/'));
    }
}
