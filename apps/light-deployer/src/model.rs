use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum DeploymentAction {
    Render,
    DryRun,
    Diff,
    Deploy,
    Undeploy,
    Status,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DeploymentStatus {
    Accepted,
    Rendered,
    Validated,
    Applying,
    Deployed,
    Deleted,
    RolledBack,
    RequiresOverride,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentRequest {
    pub request_id: Option<String>,
    pub host_id: String,
    pub instance_id: String,
    pub environment: String,
    pub cluster_id: String,
    pub namespace: String,
    pub action: DeploymentAction,
    #[serde(default)]
    pub values: Option<JsonValue>,
    #[serde(default)]
    pub values_ref: Option<ValuesRef>,
    #[serde(default)]
    pub values_snapshot_id: Option<String>,
    #[serde(default)]
    pub values_hash: Option<String>,
    #[serde(default)]
    pub runtime_values_ref: Option<ValuesRef>,
    #[serde(default)]
    pub runtime_values_snapshot_id: Option<String>,
    #[serde(default)]
    pub runtime_values_hash: Option<String>,
    pub template: TemplateRef,
    #[serde(default)]
    pub rollback_snapshot_id: Option<String>,
    #[serde(default)]
    pub options: DeploymentOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValuesRef {
    pub source: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TemplateRef {
    pub repo_url: String,
    #[serde(default = "default_template_ref")]
    pub r#ref: String,
    #[serde(default = "default_template_path")]
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentOptions {
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default = "default_wait_for_rollout")]
    pub wait_for_rollout: bool,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub prune_override: bool,
}

impl Default for DeploymentOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            wait_for_rollout: default_wait_for_rollout(),
            timeout_seconds: default_timeout_seconds(),
            prune_override: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentResponse {
    pub request_id: String,
    pub action: DeploymentAction,
    pub status: DeploymentStatus,
    pub deployer_id: String,
    pub cluster_id: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values_snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_values_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_values_snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_commit_sha: Option<String>,
    #[serde(default)]
    pub resources: Vec<ResourceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<ArtifactRef>,
    #[serde(default)]
    pub events: Vec<DeploymentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<DeploymentError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct ResourceIdentity {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceSummary {
    pub api_version: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub action: ResourceAction,
}

impl ResourceSummary {
    pub fn identity(&self) -> ResourceIdentity {
        ResourceIdentity {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            namespace: self.namespace.clone(),
            name: self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ResourceAction {
    Added,
    Modified,
    Deleted,
    Unchanged,
    Pruned,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffSummary {
    pub added: usize,
    pub modified: usize,
    pub deleted: usize,
    pub resources: Vec<ResourceSummary>,
    pub unified_diff: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    pub provider: String,
    pub uri: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentEvent {
    pub request_id: String,
    pub timestamp: DateTime<Utc>,
    pub status: DeploymentStatus,
    pub message: String,
    #[serde(default)]
    pub resource: Option<ResourceIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: BTreeMap<String, JsonValue>,
}

pub fn default_template_ref() -> String {
    "main".to_string()
}

pub fn default_template_path() -> String {
    "k8s".to_string()
}

pub fn default_wait_for_rollout() -> bool {
    true
}

pub fn default_timeout_seconds() -> u64 {
    300
}
