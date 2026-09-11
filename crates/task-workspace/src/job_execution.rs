use crate::{
    WorkspaceJob, WorkspaceJobState, WorkspaceStore,
    store::{lock, read, write},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{fs::File, path::PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    input_digest: String,
    state: String,
    output: Option<Value>,
}

pub enum JobExecution {
    Cached(Value),
    Active(WorkspaceJobExecution),
}
pub struct WorkspaceJobExecution {
    _lock: File,
    path: PathBuf,
    record: Record,
}
impl WorkspaceStore {
    /// A retry can read a committed receipt, but never re-run an uncertain model
    /// turn. This job lock is held independently of the task writer lock.
    pub fn claim_job_execution(&self, job: &WorkspaceJob) -> Result<JobExecution> {
        let root = self.workspace_path(&job.request.workspace_id)?.join("jobs");
        super::git::valid_id(&job.job_id)?;
        let guard = lock(&root.join(format!("{}.execution.lock", job.job_id)))?;
        let stored: WorkspaceJob = read(&root.join(format!("{}.json", job.job_id)))?;
        ensure!(
            &stored == job && job.state == WorkspaceJobState::Ready,
            "job is not ready or admission changed"
        );
        let path = root.join(format!("{}.execution.json", job.job_id));
        if path.exists() {
            let record: Record = read(&path)?;
            ensure!(record.input_digest == job.input_digest, "job input changed");
            ensure!(
                record.state == "completed",
                "previous job execution is uncertain; inspect the task before submitting a new request"
            );
            return Ok(JobExecution::Cached(
                record
                    .output
                    .ok_or_else(|| anyhow::anyhow!("completed job has no result"))?,
            ));
        }
        let record = Record {
            input_digest: job.input_digest.clone(),
            state: "prepared".into(),
            output: None,
        };
        // Holding the lock excludes concurrent attempts; setup has no durable uncertainty.
        Ok(JobExecution::Active(WorkspaceJobExecution {
            _lock: guard,
            path,
            record,
        }))
    }
}
impl WorkspaceJobExecution {
    /// Persist uncertainty immediately before sending turn/start, never after it.
    pub fn mark_started(&mut self) -> Result<()> {
        ensure!(
            self.record.state == "prepared",
            "job execution already started"
        );
        self.record.state = "running".into();
        write(&self.path, &self.record)
    }

    pub fn has_started(&self) -> bool {
        self.record.state != "prepared"
    }

    pub fn finish(mut self, output: Value) -> Result<()> {
        ensure!(
            self.record.state == "running",
            "job execution has not started"
        );
        ensure!(
            serde_json::to_vec(&output)?.len() <= 1024 * 1024,
            "job output exceeds limit"
        );
        self.record.output = Some(output);
        self.record.state = "completed".into();
        write(&self.path, &self.record)
    }
}
