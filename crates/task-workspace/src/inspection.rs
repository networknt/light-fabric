//! Stable inspection without changing the task's review disposition.
use crate::{
    Checkpoint, FileContent, Operation, Task, TaskState, WorkspaceStore, checkpoint, store::lock,
};
use anyhow::{Result, ensure};
use std::fs;

/// Owns the same kernel lock used by every task mutation. The runner must retain
/// this guard for the whole inspection job; this is not a serializable grant.
/// It exposes only verified file reads, never a mutable workspace directory.
pub struct WorkspaceInspection {
    _lock: fs::File,
    task: Task,
    checkpoint: Checkpoint,
}
impl WorkspaceInspection {
    pub fn checkpoint(&self) -> &Checkpoint {
        &self.checkpoint
    }

    pub fn read_file(&self, repository: &str, path: &str) -> Result<FileContent> {
        let checkout = self
            .task
            .checkouts
            .iter()
            .find(|checkout| checkout.repository == repository)
            .ok_or_else(|| anyhow::anyhow!("repository is not in the inspected task"))?;
        let entry = self
            .checkpoint
            .repositories
            .iter()
            .find(|entry| entry.repository == repository)
            .and_then(|entry| entry.files.iter().find(|entry| entry.path == path))
            .ok_or_else(|| anyhow::anyhow!("file is not in the inspection checkpoint"))?;
        let file = checkpoint::safe_file(&checkout.path, path)?;
        ensure!(
            fs::metadata(&file)?.len() <= 1024 * 1024,
            "file exceeds the 1 MiB read limit"
        );
        let bytes = fs::read(file)?;
        ensure!(
            checkpoint::digest(&bytes) == entry.digest,
            "inspection checkpoint changed outside the manager"
        );
        Ok(FileContent {
            content: String::from_utf8(bytes)?,
            digest: entry.digest.clone(),
        })
    }
}
impl WorkspaceStore {
    pub fn begin_inspection(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        expected_checkpoint: Option<&str>,
    ) -> Result<WorkspaceInspection> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "inspection is not granted"
        );
        let root = self.task_path(&workspace.id, task_id)?;
        let guard = lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(
                task.state,
                TaskState::Ready | TaskState::Frozen | TaskState::Approved | TaskState::Committed
            ),
            "task is not stable for inspection"
        );
        let snapshot = checkpoint::capture(&task.checkouts)?;
        if let Some(expected) = expected_checkpoint {
            ensure!(
                snapshot.digest == expected,
                "inspection checkpoint precondition failed"
            );
        }
        if matches!(task.state, TaskState::Frozen | TaskState::Approved) {
            ensure!(
                task.checkpoint.as_ref() == Some(&snapshot),
                "frozen checkpoint changed outside the manager"
            );
        }
        Ok(WorkspaceInspection {
            _lock: guard,
            task,
            checkpoint: snapshot,
        })
    }
}
