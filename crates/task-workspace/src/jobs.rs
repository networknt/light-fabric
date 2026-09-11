//! Durable, owner-local admission journal for runner workspace jobs.
//! Callers must supply authenticated context and verified policy. This module
//! does not turn the local CLI into an authenticated network API.
use crate::{
    Operation, Workspace, WorkspaceStore, checkpoint,
    store::{lock, read, write},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use workspace_execution_protocol::{
    AdmissionContext, TaskSelection, WorkspaceAccessPolicy, WorkspaceIntent, WorkspaceRequest,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceJobState {
    Queued,
    Provisioning,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceJob {
    pub schema_version: u16,
    pub job_id: String,
    pub input_digest: String,
    pub task_id: String,
    pub request: WorkspaceRequest,
    pub subject: String,
    pub agent_id: String,
    pub host_id: String,
    pub environment: String,
    pub runner_id: String,
    pub authorization_revision: u64,
    pub state: WorkspaceJobState,
    pub failure: Option<String>,
}

/// Stable repository catalog revision, separate from grants and indexer commands.
/// Legacy Task.membership_digest remains unchanged and still covers the entire
/// local registration. Jobs do not rewrite or reinterpret existing task digests.
pub fn membership_revision(workspace: &Workspace) -> Result<String> {
    let mut repositories = workspace.repositories.clone();
    repositories.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(checkpoint::digest(&serde_json::to_vec(&(
        workspace.schema_version,
        &workspace.id,
        &workspace.host_id,
        repositories,
    ))?))
}

impl WorkspaceStore {
    fn authorize_job_input(
        &self,
        request: &WorkspaceRequest,
        context: &AdmissionContext,
        policy: &WorkspaceAccessPolicy,
        qualified_intents: &BTreeSet<WorkspaceIntent>,
    ) -> Result<Workspace> {
        policy.authorize(request, context, qualified_intents)?;
        let workspace = self.workspace(&request.workspace_id, &context.agent_id)?;
        ensure!(
            workspace.host_id == context.host_id,
            "local workspace Host differs from admitted Host"
        );
        ensure!(
            membership_revision(&workspace)? == request.expected_membership_revision,
            "local workspace membership differs from published revision"
        );
        let required: &[Operation] = match request.intent {
            WorkspaceIntent::Inspect | WorkspaceIntent::Review => &[Operation::Review],
            WorkspaceIntent::Implement => &[Operation::Edit],
            WorkspaceIntent::ImplementAndReview => &[Operation::Edit, Operation::Review],
        };
        ensure!(
            required
                .iter()
                .all(|operation| workspace.operations.contains(operation)),
            "local workspace operation is not granted"
        );
        Ok(workspace)
    }

    /// Persist intent before provisioning. A scoped retry returns the same job;
    /// reusing its request ID with different content is a conflict. No remote
    /// Git work or model call occurs here.
    pub fn admit_job(
        &self,
        request: &WorkspaceRequest,
        context: &AdmissionContext,
        policy: &WorkspaceAccessPolicy,
        qualified_intents: &BTreeSet<WorkspaceIntent>,
    ) -> Result<WorkspaceJob> {
        self.authorize_job_input(request, context, policy, qualified_intents)?;
        let root = self.workspace_path(&request.workspace_id)?;
        let _lock = lock(&root.join("workspace.lock"))?;
        // Recheck after locking so local registration changes cannot race admission.
        self.authorize_job_input(request, context, policy, qualified_intents)?;
        let key = request.admission_key(context)?;
        let job_id = format!(
            "job-{}",
            key.strip_prefix("sha256:").context("admission key")?
        );
        let directory = root.join("jobs");
        super::store::directory(&directory)?;
        let path = directory.join(format!("{job_id}.json"));
        let input_digest = request.input_digest()?;
        if path.exists() {
            let previous: WorkspaceJob = read(&path)?;
            ensure!(
                previous.input_digest == input_digest,
                "request ID already used with different workspace input"
            );
            ensure!(
                previous.subject == context.subject
                    && previous.agent_id == context.agent_id
                    && previous.host_id == context.host_id
                    && previous.environment == context.environment
                    && previous.runner_id == context.runner_id,
                "job admission scope changed"
            );
            ensure!(
                policy.authorization_revision >= previous.authorization_revision,
                "authorization revision regressed"
            );
            return Ok(previous);
        }
        let task_id = match &request.task {
            TaskSelection::Existing { task_id } => {
                let task = self.status(&request.workspace_id, task_id, &context.agent_id)?;
                if let Some(expected) = &request.expected_checkpoint_digest {
                    ensure!(
                        task.checkpoint.as_ref().map(|c| &c.digest) == Some(expected),
                        "checkpoint precondition failed"
                    );
                }
                task_id.clone()
            }
            TaskSelection::New { .. } => format!(
                "task-{}",
                key.strip_prefix("sha256:").context("admission key")?
            ),
        };
        let job = WorkspaceJob {
            schema_version: 1,
            job_id,
            input_digest,
            task_id,
            request: request.clone(),
            subject: context.subject.clone(),
            agent_id: context.agent_id.clone(),
            host_id: context.host_id.clone(),
            environment: context.environment.clone(),
            runner_id: context.runner_id.clone(),
            authorization_revision: policy.authorization_revision,
            state: WorkspaceJobState::Queued,
            failure: None,
        };
        write(&path, &job)?;
        Ok(job)
    }

    /// A bounded runner stage. Re-authorize with current verified policy before
    /// every retry. Holding a job lock excludes duplicate provisioning; the task
    /// and workspace manager retain their existing cross-job locks.
    pub fn provision_job(
        &self,
        workspace: &str,
        job_id: &str,
        context: &AdmissionContext,
        policy: &WorkspaceAccessPolicy,
        qualified_intents: &BTreeSet<WorkspaceIntent>,
    ) -> Result<WorkspaceJob> {
        super::git::valid_id(job_id)?;
        let directory = self.workspace_path(workspace)?.join("jobs");
        let path = directory.join(format!("{job_id}.json"));
        let _lock = lock(&directory.join(format!("{job_id}.lock")))?;
        let mut job: WorkspaceJob = read(&path)?;
        ensure!(
            job.schema_version == 1
                && job.request.workspace_id == workspace
                && job.job_id == job_id,
            "job identity mismatch"
        );
        ensure!(
            job.subject == context.subject
                && job.agent_id == context.agent_id
                && job.host_id == context.host_id
                && job.environment == context.environment
                && job.runner_id == context.runner_id,
            "job execution scope changed"
        );
        self.authorize_job_input(&job.request, context, policy, qualified_intents)?;
        ensure!(
            policy.authorization_revision >= job.authorization_revision,
            "authorization revision regressed"
        );
        if job.state == WorkspaceJobState::Ready {
            self.status(workspace, &job.task_id, &context.agent_id)?;
            if let Some(expected) = &job.request.expected_checkpoint_digest {
                ensure!(
                    self.files(workspace, &job.task_id, &context.agent_id)?
                        .digest
                        == *expected,
                    "checkpoint changed after provisioning"
                );
            }
            return Ok(job);
        }
        job.authorization_revision = policy.authorization_revision;
        job.state = WorkspaceJobState::Provisioning;
        job.failure = None;
        write(&path, &job)?;
        let result = match job.request.task {
            TaskSelection::New { .. } => {
                self.create_task(workspace, &job.task_id, &context.agent_id)
            }
            TaskSelection::Existing { .. } => {
                self.status(workspace, &job.task_id, &context.agent_id)
            }
        };
        match result {
            Ok(task) => {
                if let Some(expected) = &job.request.expected_checkpoint_digest
                    && (task.checkpoint.as_ref().map(|c| &c.digest) != Some(expected)
                        || self
                            .files(workspace, &job.task_id, &context.agent_id)?
                            .digest
                            != *expected)
                {
                    job.state = WorkspaceJobState::Failed;
                    job.failure = Some("checkpoint changed before provisioning completed".into());
                    write(&path, &job)?;
                    anyhow::bail!("checkpoint changed before provisioning completed");
                }
                job.state = WorkspaceJobState::Ready;
                write(&path, &job)?;
                Ok(job)
            }
            Err(error) => {
                job.state = WorkspaceJobState::Failed;
                job.failure = Some(error.to_string());
                write(&path, &job)?;
                Err(error)
            }
        }
    }
}
