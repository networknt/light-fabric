//! A runner-owned file-tool session. Identity and task are fixed at construction;
//! model arguments cannot select a store, workspace, task, or agent.
use crate::{
    Access, Checkpoint, ExecutionContext, FileContent, FileEdit, Operation, Task, TaskState,
    WorkspaceStore, checkpoint,
    store::{directory, lock, write},
};
use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::PathBuf};

pub struct WorkspaceToolSession {
    _lock: fs::File,
    journal: PathBuf,
    task: Task,
    snapshot: Checkpoint,
    writable: bool,
    agent: String,
    finished: bool,
}

impl WorkspaceStore {
    pub fn begin_tool_session(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        writable: bool,
        expected_checkpoint: Option<&str>,
    ) -> Result<WorkspaceToolSession> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&if writable {
                Operation::Edit
            } else {
                Operation::Review
            }),
            "workspace tool access is not granted"
        );
        let root = self.task_path(&workspace.id, task_id)?;
        let guard = lock(&root.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            if writable {
                task.state == TaskState::Ready
            } else {
                matches!(
                    task.state,
                    TaskState::Ready
                        | TaskState::Frozen
                        | TaskState::Approved
                        | TaskState::Committed
                )
            },
            "task is not available for this tool session"
        );
        let snapshot = checkpoint::capture(&task.checkouts)?;
        if let Some(expected) = expected_checkpoint {
            ensure!(
                snapshot.digest == expected,
                "checkpoint precondition failed"
            );
        }
        if matches!(task.state, TaskState::Frozen | TaskState::Approved) {
            ensure!(
                task.checkpoint.as_ref() == Some(&snapshot),
                "frozen checkpoint changed"
            );
        }
        let journal = root.join("task.json");
        if writable {
            task.execution_context = Some(ExecutionContext {
                access: Access::Implement,
                previous_state: task.state,
            });
            task.state = TaskState::Running;
            task.generation = task
                .generation
                .checked_add(1)
                .context("generation exhausted")?;
            task.writer = Some(agent.into());
            write(&journal, &task)?;
        }
        Ok(WorkspaceToolSession {
            _lock: guard,
            journal,
            task,
            snapshot,
            writable,
            agent: agent.into(),
            finished: false,
        })
    }
}

impl WorkspaceToolSession {
    pub fn checkpoint(&self) -> &Checkpoint {
        &self.snapshot
    }

    pub fn read_file(&self, repository: &str, path: &str) -> Result<FileContent> {
        let checkout = self
            .task
            .checkouts
            .iter()
            .find(|c| c.repository == repository)
            .context("repository is not in this task")?;
        let entry = self
            .snapshot
            .repositories
            .iter()
            .find(|r| r.repository == repository)
            .and_then(|r| r.files.iter().find(|f| f.path == path))
            .context("file is not in the checkpoint")?;
        let file = checkpoint::safe_file(&checkout.path, path)?;
        ensure!(
            fs::metadata(&file)?.len() <= 1024 * 1024,
            "file exceeds read limit"
        );
        let bytes = fs::read(file)?;
        ensure!(
            checkpoint::digest(&bytes) == entry.digest,
            "file changed outside the manager"
        );
        Ok(FileContent {
            content: String::from_utf8(bytes)?,
            digest: entry.digest.clone(),
        })
    }

    pub fn edit_file(&mut self, edit: FileEdit) -> Result<String> {
        ensure!(
            self.writable && !self.finished,
            "tool session is read-only or closed"
        );
        if let Some(content) = &edit.content {
            ensure!(content.len() <= 1024 * 1024, "file exceeds edit limit");
        }
        ensure!(
            checkpoint::capture(&self.task.checkouts)? == self.snapshot,
            "task changed outside the tool session"
        );
        let checkout = self
            .task
            .checkouts
            .iter()
            .find(|c| c.repository == edit.repository)
            .context("repository is not in this task")?;
        let path = checkpoint::safe_file(&checkout.path, &edit.path)?;
        checkpoint::reject_submodule_edit(&checkout.path, &edit.path)?;
        let existing = match fs::read(&path) {
            Ok(bytes) => Some(checkpoint::digest(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            existing == edit.expected_digest,
            "file changed since it was read"
        );
        // Journal attribution before touching the tree, including a possible crash.
        self.task.last_implementer = Some(self.agent.clone());
        self.task.contributors.insert(self.agent.clone());
        write(&self.journal, &self.task)?;
        if let Some(content) = edit.content {
            let parent = path.parent().context("file parent")?;
            directory(parent)?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            if let Ok(metadata) = fs::metadata(&path) {
                temporary
                    .as_file()
                    .set_permissions(fs::Permissions::from_mode(
                        metadata.permissions().mode() & 0o777,
                    ))?;
            }
            temporary.write_all(content.as_bytes())?;
            temporary.as_file().sync_all()?;
            temporary.persist(&path)?;
            fs::File::open(parent)?.sync_all()?;
        } else if existing.is_some() {
            fs::remove_file(&path)?;
            fs::File::open(path.parent().context("file parent")?)?.sync_all()?;
        }
        self.snapshot = checkpoint::capture(&self.task.checkouts)?;
        Ok(self.snapshot.digest.clone())
    }

    /// Call only after the adapter has stopped. An unfinished writer requires
    /// operator fencing even if its last edit appears to have succeeded.
    pub fn finish(mut self) -> Result<Checkpoint> {
        let snapshot = checkpoint::capture(&self.task.checkouts)?;
        ensure!(
            snapshot == self.snapshot,
            "task changed outside the tool session"
        );
        if self.writable {
            self.task.state = TaskState::Ready;
            self.task.writer = None;
            self.task.execution_context = None;
            write(&self.journal, &self.task)?;
        }
        self.finished = true;
        Ok(snapshot)
    }
}

impl Drop for WorkspaceToolSession {
    fn drop(&mut self) {
        if self.writable && !self.finished {
            self.task.state = TaskState::Interrupted;
            // On persistence failure the existing Running journal still fails closed.
            let _ = write(&self.journal, &self.task);
        }
    }
}
