use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_runner_protocol::{ArtifactEvidence, BackendCapability, ExecuteLease};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;

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

    async fn restore(&self, _checkpoint: &Checkpoint) -> Result<PreparedExecution, BackendError> {
        Err(BackendError::Unsupported(
            "backend does not support restore".to_string(),
        ))
    }

    async fn cleanup(&self, backend_operation_id: &str) -> Result<CleanupEvidence, BackendError>;
}
