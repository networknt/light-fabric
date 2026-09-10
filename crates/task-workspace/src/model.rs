use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Repository {
    pub name: String,
    /// Host administrator supplied Git clone source. Never supplied by a worker.
    pub source: String,
    pub integration_branch: String,
    pub release_branch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Workspace {
    pub schema_version: u16,
    pub id: String,
    pub host_id: String,
    /// A grant covers ALL repositories in this membership revision.
    pub agents: BTreeSet<String>,
    pub repositories: Vec<Repository>,
    /// Workspace-wide operation grants, independent of repository membership.
    #[serde(default)]
    pub operations: BTreeSet<Operation>,
    #[serde(default)]
    pub indexers: std::collections::BTreeMap<String, crate::IndexerCommand>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Operation {
    Edit,
    Execute,
    Review,
    Commit,
    Push,
    Issue,
    PullRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileEdit {
    pub repository: String,
    pub path: String,
    /// None deletes a file. A digest precondition prevents lost updates.
    pub content: Option<String>,
    pub expected_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileContent {
    pub content: String,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkout {
    pub repository: String,
    pub branch: String,
    pub base_commit: String,
    pub integration_branch: String,
    pub release_branch: String,
    pub path: PathBuf,
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    Provisioning,
    Ready,
    Running,
    Interrupted,
    Frozen,
    Approved,
    Committing,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Task {
    pub schema_version: u16,
    pub workspace_id: String,
    pub task_id: String,
    pub membership_digest: String,
    pub state: TaskState,
    pub generation: u64,
    pub writer: Option<String>,
    pub last_implementer: Option<String>,
    #[serde(default)]
    pub contributors: BTreeSet<String>,
    pub checkouts: Vec<Checkout>,
    pub checkpoint: Option<Checkpoint>,
    pub review: Option<Review>,
    pub commit_message: Option<String>,
    #[serde(default)]
    pub normalized_commit_message: Option<String>,
    #[serde(default)]
    pub execution_context: Option<ExecutionContext>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Checkpoint {
    pub digest: String,
    pub repositories: Vec<RepositoryCheckpoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RepositoryCheckpoint {
    pub repository: String,
    pub head: String,
    pub index_digest: String,
    pub files: Vec<FileEntry>,
    pub status_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileEntry {
    pub path: String,
    pub digest: String,
    pub executable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Review {
    pub reviewer: String,
    pub checkpoint_digest: String,
    pub approved: bool,
    pub findings: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Access {
    Implement,
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionOutput {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionContext {
    pub access: Access,
    pub previous_state: TaskState,
}
