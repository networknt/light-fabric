use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt, str::FromStr};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_MAJOR: u16 = 1;
pub const PROTOCOL_VERSION: &str = "1.0";
pub const MAX_CAPABILITY_DOCUMENT_BYTES: usize = 256 * 1024;

macro_rules! opaque_uuid {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

opaque_uuid!(MessageId);
opaque_uuid!(RunnerSessionId);
opaque_uuid!(SchedulingRequestId);
opaque_uuid!(ExecutionId);
opaque_uuid!(LeaseId);
opaque_uuid!(ExecutionSessionId);
opaque_uuid!(CleanupRequestId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OriginKind {
    Workflow,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthenticatedOrigin {
    pub kind: OriginKind,
    pub service_id: String,
    pub instance_id: String,
    pub host_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SubjectKind {
    WorkflowTask,
    AgentTurn,
    AgentAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum ExecutionSubject {
    WorkflowTask {
        subject_id: Uuid,
        process_id: Uuid,
        task_id: Uuid,
    },
    AgentTurn {
        subject_id: Uuid,
        session_id: Uuid,
        turn_id: Uuid,
    },
    AgentAction {
        subject_id: Uuid,
        session_id: Uuid,
        turn_id: Uuid,
        action_id: Uuid,
    },
}

impl ExecutionSubject {
    pub fn kind(&self) -> SubjectKind {
        match self {
            Self::WorkflowTask { .. } => SubjectKind::WorkflowTask,
            Self::AgentTurn { .. } => SubjectKind::AgentTurn,
            Self::AgentAction { .. } => SubjectKind::AgentAction,
        }
    }

    pub fn subject_id(&self) -> Uuid {
        match self {
            Self::WorkflowTask { subject_id, .. }
            | Self::AgentTurn { subject_id, .. }
            | Self::AgentAction { subject_id, .. } => *subject_id,
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OriginSubjectError {
    #[error("workflow origins may submit only workflow-task subjects")]
    WorkflowSubjectMismatch,
    #[error("agent origins may submit only agent-turn or agent-action subjects")]
    AgentSubjectMismatch,
    #[error("origin service and instance identifiers must not be empty")]
    EmptyOriginIdentity,
}

pub fn validate_origin_subject(
    origin: &AuthenticatedOrigin,
    subject: &ExecutionSubject,
) -> Result<(), OriginSubjectError> {
    if origin.service_id.trim().is_empty() || origin.instance_id.trim().is_empty() {
        return Err(OriginSubjectError::EmptyOriginIdentity);
    }
    match (origin.kind, subject.kind()) {
        (OriginKind::Workflow, SubjectKind::WorkflowTask) => Ok(()),
        (OriginKind::Workflow, _) => Err(OriginSubjectError::WorkflowSubjectMismatch),
        (OriginKind::Agent, SubjectKind::AgentTurn | SubjectKind::AgentAction) => Ok(()),
        (OriginKind::Agent, SubjectKind::WorkflowTask) => {
            Err(OriginSubjectError::AgentSubjectMismatch)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageEnvelope<T> {
    pub schema_version: String,
    pub message_id: MessageId,
    pub sent_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_session_id: Option<RunnerSessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_generation: Option<u64>,
    pub payload: T,
}

impl<T> MessageEnvelope<T> {
    pub fn new(payload: T) -> Self {
        Self {
            schema_version: PROTOCOL_VERSION.to_string(),
            message_id: MessageId::new(),
            sent_at: Utc::now(),
            runner_session_id: None,
            connection_generation: None,
            payload,
        }
    }

    pub fn protocol_major(&self) -> Result<u16, ProtocolVersionError> {
        parse_protocol_major(&self.schema_version)
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolVersionError {
    #[error("invalid protocol version `{0}`")]
    Invalid(String),
    #[error("unsupported protocol major {actual}; expected {expected}")]
    UnsupportedMajor { expected: u16, actual: u16 },
}

pub fn parse_protocol_major(version: &str) -> Result<u16, ProtocolVersionError> {
    let major = version
        .split('.')
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| ProtocolVersionError::Invalid(version.to_string()))?;
    if major != PROTOCOL_MAJOR {
        return Err(ProtocolVersionError::UnsupportedMajor {
            expected: PROTOCOL_MAJOR,
            actual: major,
        });
    }
    Ok(major)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationBoundary {
    Process,
    Container,
    UserNamespace,
    MicroVm,
    RemoteSandbox,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostExposure {
    None,
    ReadOnlyWorkspace,
    ExplicitMounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SchedulingState {
    PendingCapacity,
    Reserved,
    AttemptCreated,
    Leased,
    Satisfied,
    Cancelled,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AttemptState {
    Created,
    Leased,
    Started,
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Unknown,
    Cleaned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CleanupState {
    NotRequired,
    Required,
    InProgress,
    Confirmed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RetrySafety {
    Safe,
    Unsafe,
    InspectRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionRequirements {
    pub action_kind: String,
    pub minimum_boundary: IsolationBoundary,
    pub maximum_host_exposure: HostExposure,
    pub network_enabled: bool,
    pub credential_classes: Vec<String>,
    pub persistent_workspace: bool,
    pub required_features: Vec<String>,
    pub policy_digest: String,
    pub compatibility_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandExecutionSpec {
    pub schema_version: u16,
    pub template_id: String,
    pub template_version: u32,
    pub template_digest: String,
    pub executable: String,
    pub arguments: Vec<String>,
    pub working_directory: String,
    pub environment: BTreeMap<String, String>,
    pub wall_clock_timeout_ms: u64,
    pub stdout_limit_bytes: u64,
    pub stderr_limit_bytes: u64,
    pub network_enabled: bool,
    pub credentials_enabled: bool,
    pub persistent_workspace: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendCapability {
    pub backend_id: String,
    pub backend_version: String,
    pub boundary: IsolationBoundary,
    pub host_exposure: HostExposure,
    pub actions: Vec<String>,
    pub features: Vec<String>,
    pub compatibility_digest: String,
    pub healthy: bool,
    pub available_slots: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerCapabilityDocument {
    pub runner_version: String,
    pub protocol_versions: Vec<String>,
    pub maximum_concurrency: u32,
    pub effective_config_digest: String,
    pub command_allowlist_digest: String,
    pub watchdog_healthy: bool,
    pub journal_healthy: bool,
    pub backends: Vec<BackendCapability>,
}

impl RunnerCapabilityDocument {
    pub fn validate_size(&self) -> Result<(), CanonicalJsonError> {
        let size = canonical_json_bytes(self)?.len();
        if size > MAX_CAPABILITY_DOCUMENT_BYTES {
            return Err(CanonicalJsonError::TooLarge {
                actual: size,
                maximum: MAX_CAPABILITY_DOCUMENT_BYTES,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerRegistration {
    pub runner_id: String,
    pub host_id: Uuid,
    pub capability: RunnerCapabilityDocument,
    pub binary_digest: String,
    pub enrollment_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseContext {
    pub scheduling_request_id: SchedulingRequestId,
    pub execution_id: ExecutionId,
    pub origin: AuthenticatedOrigin,
    pub subject: ExecutionSubject,
    pub attempt: u32,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub policy_digest: String,
    pub compatibility_digest: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    pub truncated: bool,
    pub original_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactEvidence {
    pub logical_name: String,
    #[serde(default = "unknown_artifact_file_type")]
    pub file_type: String,
    pub media_type: String,
    pub size: u64,
    pub digest: String,
    pub reference: String,
}

fn unknown_artifact_file_type() -> String {
    "unknown".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedExecutionResult {
    pub execution_id: ExecutionId,
    pub origin: AuthenticatedOrigin,
    pub subject: ExecutionSubject,
    pub attempt: u32,
    pub state: AttemptState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub duration_ms: u64,
    pub stdout: NormalizedOutput,
    pub stderr: NormalizedOutput,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<Value>,
    pub artifacts: Vec<ArtifactEvidence>,
    pub backend_operation_id: String,
    pub cleanup_state: CleanupState,
    pub policy_digest: String,
    pub compatibility_digest: String,
    pub definition_digest: String,
    pub command_template_digest: String,
    pub retry_safety: RetrySafety,
    pub evidence: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum RunnerToController {
    RunnerRegister(RunnerRegistration),
    RunnerHeartbeat(RunnerHeartbeat),
    RunnerLeaseAccepted(LeaseContext),
    RunnerLeaseStarted(LeaseContext),
    RunnerLeaseRenew(LeaseRenewal),
    RunnerLeaseSucceeded(TerminalLeaseResult),
    RunnerLeaseFailed(TerminalLeaseResult),
    RunnerLeaseUnknown(TerminalLeaseResult),
    RunnerLeaseCancelled(TerminalLeaseResult),
    RunnerCleanupCompleted(CleanupCompleted),
    RunnerSessionUpdated(SessionUpdated),
    RunnerDrain(RunnerDrain),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "camelCase")]
pub enum ControllerToRunner {
    RunnerRegisterAccepted(RegisterAccepted),
    RunnerExecuteLease(ExecuteLease),
    RunnerLeaseResultAccepted(LeaseResultAccepted),
    RunnerCancelLease(CancelLease),
    RunnerHoldSession(SessionDirective),
    RunnerResumeSession(SessionDirective),
    RunnerCleanupSession(SessionDirective),
    RunnerDrainRequested(RunnerDrain),
    RunnerReconcileLease(LeaseContext),
    RunnerSessionRejected(SessionRejected),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerHeartbeat {
    pub effective_config_digest: String,
    pub command_allowlist_digest: String,
    pub watchdog_healthy: bool,
    pub journal_healthy: bool,
    pub cleanup_backlog: u32,
    pub available_capacity: u32,
    pub active_leases: Vec<ActiveLeaseSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActiveLeaseSummary {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub state: AttemptState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseRenewal {
    pub lease: LeaseContext,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TerminalLeaseResult {
    pub lease: LeaseContext,
    pub result: NormalizedExecutionResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanupCompleted {
    pub cleanup_request_id: CleanupRequestId,
    pub execution_session_id: ExecutionSessionId,
    pub cleanup_state: CleanupState,
    pub evidence_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionUpdated {
    pub execution_session_id: ExecutionSessionId,
    pub session_version: u64,
    pub session_fence: u64,
    pub state: String,
    pub backend_operation_id: Option<String>,
    pub checkpoint_handle: Option<String>,
    pub checkpoint_digest: Option<String>,
    pub evidence_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerDrain {
    pub reason: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterAccepted {
    pub runner_session_id: RunnerSessionId,
    pub connection_generation: u64,
    pub heartbeat_interval_ms: u64,
    pub admission_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecuteLease {
    pub lease: LeaseContext,
    pub backend_id: String,
    pub execution_profile: Value,
    pub command: Value,
    pub inputs: Vec<ExecutionInput>,
    pub definition_digest: String,
    pub command_template_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseResultAccepted {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub state: AttemptState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionInput {
    pub input_id: Uuid,
    pub kind: String,
    pub artifact_uri: String,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
    pub mount_target: String,
    pub read_only: bool,
    pub executable: bool,
    #[serde(default)]
    pub verification: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CancelLease {
    pub lease: LeaseContext,
    pub reason: String,
    pub grace_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionDirective {
    pub execution_session_id: ExecutionSessionId,
    #[serde(default)]
    pub cleanup_request_id: Option<CleanupRequestId>,
    pub session_version: u64,
    pub session_fence: u64,
    pub compatibility_digest: String,
    #[serde(default)]
    pub backend_operation_id: Option<String>,
    #[serde(default)]
    pub checkpoint_handle: Option<String>,
    #[serde(default)]
    pub checkpoint_digest: Option<String>,
    pub reason: String,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionRejected {
    pub reason_code: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum CanonicalJsonError {
    #[error("failed to serialize canonical JSON: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("canonical document is {actual} bytes; maximum is {maximum}")]
    TooLarge { actual: usize, maximum: usize },
}

pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, CanonicalJsonError> {
    let value = serde_json::to_value(value)?;
    serde_json::to_vec(&canonicalize(value)).map_err(CanonicalJsonError::from)
}

pub fn canonical_sha256<T: Serialize>(value: &T) -> Result<String, CanonicalJsonError> {
    let bytes = canonical_json_bytes(value)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn canonicalize(value: Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.into_iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let sorted = values
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_digest_has_stable_golden_vector() {
        let first = json!({"b": 2, "a": 1});
        let second = json!({"a": 1, "b": 2});

        let first_bytes = canonical_json_bytes(&first).unwrap();
        let second_bytes = canonical_json_bytes(&second).unwrap();

        assert_eq!(first_bytes, br#"{"a":1,"b":2}"#);
        assert_eq!(first_bytes, second_bytes);
        assert_eq!(
            canonical_sha256(&first).unwrap(),
            "43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777"
        );
    }

    #[test]
    fn origin_cannot_submit_another_services_subject_kind() {
        let origin = AuthenticatedOrigin {
            kind: OriginKind::Workflow,
            service_id: "com.networknt.light-workflow-1.0.0".into(),
            instance_id: "workflow-1".into(),
            host_id: Uuid::new_v4(),
        };
        let agent_action = ExecutionSubject::AgentAction {
            subject_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            turn_id: Uuid::new_v4(),
            action_id: Uuid::new_v4(),
        };

        assert_eq!(
            validate_origin_subject(&origin, &agent_action),
            Err(OriginSubjectError::WorkflowSubjectMismatch)
        );
    }

    #[test]
    fn unknown_registration_fields_fail_closed() {
        let document = json!({
            "runnerId": "runner-1",
            "hostId": Uuid::new_v4(),
            "binaryDigest": "sha256:abc",
            "enrollmentId": "enrollment-1",
            "unexpectedAuthority": true,
            "capability": {
                "runnerVersion": "0.1.0",
                "protocolVersions": ["1.0"],
                "maximumConcurrency": 1,
                "effectiveConfigDigest": "sha256:config",
                "commandAllowlistDigest": "sha256:commands",
                "watchdogHealthy": true,
                "journalHealthy": true,
                "backends": []
            }
        });

        assert!(serde_json::from_value::<RunnerRegistration>(document).is_err());
    }

    #[test]
    fn unsupported_protocol_major_is_rejected() {
        assert_eq!(
            parse_protocol_major("2.0"),
            Err(ProtocolVersionError::UnsupportedMajor {
                expected: 1,
                actual: 2
            })
        );
    }
}
