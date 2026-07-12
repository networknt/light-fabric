use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_runner_protocol::{ArtifactEvidence, BackendCapability, ExecuteLease};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

pub fn validate_artifact_manifest(
    artifacts: &[ArtifactEvidence],
    maximum_entries: usize,
    maximum_total_bytes: u64,
) -> Result<(), BackendError> {
    if artifacts.len() > maximum_entries {
        return Err(BackendError::InvalidRequest(
            "artifact manifest has too many entries".into(),
        ));
    }
    let mut names = BTreeSet::new();
    let mut total = 0_u64;
    for artifact in artifacts {
        if artifact.file_type != "regular-file" {
            return Err(BackendError::InvalidRequest(
                "artifact is not a verified regular file".into(),
            ));
        }
        let path = std::path::Path::new(&artifact.logical_name);
        if artifact.logical_name.is_empty()
            || artifact.logical_name.len() > 1024
            || artifact.logical_name.contains(['\\', '\0'])
            || path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
            || !names.insert(artifact.logical_name.clone())
        {
            return Err(BackendError::InvalidRequest(
                "artifact logical name is unsafe or duplicated".into(),
            ));
        }
        if artifact.media_type.is_empty()
            || artifact.media_type.len() > 255
            || artifact.reference.is_empty()
            || artifact.reference.len() > 4096
        {
            return Err(BackendError::InvalidRequest(
                "artifact metadata is missing or oversized".into(),
            ));
        }
        let digest = artifact.digest.strip_prefix("sha256:").ok_or_else(|| {
            BackendError::InvalidRequest("artifact digest must use sha256".into())
        })?;
        if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(BackendError::InvalidRequest(
                "artifact digest is malformed".into(),
            ));
        }
        total = total
            .checked_add(artifact.size)
            .ok_or_else(|| BackendError::InvalidRequest("artifact size overflow".into()))?;
        if total > maximum_total_bytes {
            return Err(BackendError::InvalidRequest(
                "artifact bytes exceed the admitted total".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum BackendError {
    #[error("execution request is invalid: {0}")]
    InvalidRequest(String),
    #[error("backend operation was not found: {0}")]
    NotFound(String),
    #[error("backend operation outcome is unknown: {0}")]
    Unknown(String),
    #[error("backend operation was cancelled: {0}")]
    Cancelled(String),
    #[error("backend deadline expired: {0}")]
    TimedOut(String),
    #[error("backend transport failed: {0}")]
    Transport(String),
    #[error("backend cleanup failed: {0}")]
    Cleanup(String),
    #[error("backend operation is not supported: {0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StagedInput {
    pub input_id: uuid::Uuid,
    pub source_digest: String,
    pub local_path: PathBuf,
    pub mount_target: String,
    pub media_type: String,
    pub size: u64,
    pub read_only: bool,
    pub executable: bool,
    pub mount_options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedExecution {
    pub backend_operation_id: String,
    pub execution_id: execution_runner_protocol::ExecutionId,
    pub backend_id: String,
    pub prepared_at: DateTime<Utc>,
    pub evidence: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BackendOperationState {
    Prepared,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    Cleaned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inspection {
    pub backend_operation_id: String,
    pub state: BackendOperationState,
    pub observed_at: DateTime<Utc>,
    pub evidence: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendOutput {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub structured_output: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub failure_class: Option<String>,
    pub evidence: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CleanupEvidence {
    pub backend_operation_id: String,
    pub cleaned_at: DateTime<Utc>,
    pub evidence_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub backend_operation_id: String,
    pub checkpoint_handle: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogCursor {
    pub stream: String,
    pub offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LogChunk {
    pub cursor: LogCursor,
    pub next_cursor: Option<LogCursor>,
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub end_of_stream: bool,
}

#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    fn capability(&self) -> BackendCapability;

    fn validate(
        &self,
        lease: &ExecuteLease,
        staged_inputs: &[StagedInput],
    ) -> Result<(), BackendError>;

    async fn prepare(
        &self,
        lease: &ExecuteLease,
        staged_inputs: &[StagedInput],
    ) -> Result<PreparedExecution, BackendError>;

    async fn inspect(&self, backend_operation_id: &str) -> Result<Inspection, BackendError>;

    async fn execute(
        &self,
        prepared: &PreparedExecution,
        lease: &ExecuteLease,
        cancellation: watch::Receiver<bool>,
    ) -> Result<BackendOutput, BackendError>;

    async fn cancel(&self, backend_operation_id: &str) -> Result<(), BackendError>;

    async fn collect_artifacts(
        &self,
        _backend_operation_id: &str,
    ) -> Result<Vec<ArtifactEvidence>, BackendError> {
        Ok(Vec::new())
    }

    async fn checkpoint(&self, _backend_operation_id: &str) -> Result<Checkpoint, BackendError> {
        Err(BackendError::Unsupported(
            "backend does not support checkpoint".to_string(),
        ))
    }

    async fn read_logs(
        &self,
        _backend_operation_id: &str,
        _cursor: &LogCursor,
        _maximum_bytes: u64,
    ) -> Result<LogChunk, BackendError> {
        Err(BackendError::Unsupported(
            "backend does not support cursor log streaming".into(),
        ))
    }

    async fn restore(&self, _checkpoint: &Checkpoint) -> Result<PreparedExecution, BackendError> {
        Err(BackendError::Unsupported(
            "backend does not support restore".to_string(),
        ))
    }

    async fn cleanup(&self, backend_operation_id: &str) -> Result<CleanupEvidence, BackendError>;

    async fn reconcile_orphans(
        &self,
        _retained_operation_ids: &BTreeSet<String>,
        _now: DateTime<Utc>,
    ) -> Result<Vec<CleanupEvidence>, BackendError> {
        Err(BackendError::Unsupported(
            "backend does not support owned-resource reconciliation".into(),
        ))
    }
}
