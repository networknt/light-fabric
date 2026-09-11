use crate::{
    Access, Checkout, FileContent, FileEdit, Operation, Review, Task, TaskState, Workspace,
    checkpoint, git,
};
use anyhow::{Context, Result, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Host-owned manager. Do not expose its methods directly to an unauthenticated
/// transport. `agent` is an authenticated service identity supplied by the caller.
#[derive(Debug, Clone)]
pub struct WorkspaceStore {
    pub(crate) root: PathBuf,
}

pub(crate) fn directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "managed directory cannot be a symlink"
    );
    Ok(())
}
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // Concurrent process spawning can briefly inherit an unrelated flock before
    // exec closes CLOEXEC descriptors. Tolerate that window, not a live writer.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Err(error) => return Err(error).context("workspace operation is already active"),
        }
    }
    Ok(file)
}
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    Ok(serde_json::from_reader(file)?)
}
pub(crate) fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    File::open(path.parent().context("metadata parent")?)?.sync_all()?;
    Ok(())
}

impl WorkspaceStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        directory(root.as_ref())?;
        let root = root.as_ref().canonicalize()?;
        ensure!(
            fs::metadata(&root)?.permissions().mode() & 0o077 == 0,
            "workspace store must be private (mode 0700)"
        );
        Ok(Self { root })
    }
    pub(crate) fn workspace_path(&self, id: &str) -> Result<PathBuf> {
        git::valid_id(id)?;
        Ok(self.root.join(id))
    }
    pub(crate) fn task_path(&self, workspace: &str, task: &str) -> Result<PathBuf> {
        git::valid_id(task)?;
        Ok(self.workspace_path(workspace)?.join("tasks").join(task))
    }
    pub(crate) fn workspace(&self, id: &str, agent: &str) -> Result<Workspace> {
        let workspace: Workspace = read(&self.workspace_path(id)?.join("workspace.json"))?;
        ensure!(
            workspace.agents.contains(agent),
            "agent has no workspace grant"
        );
        Ok(workspace)
    }
    pub(crate) fn load_task(&self, workspace: &Workspace, task: &str) -> Result<Task> {
        let record: Task = read(&self.task_path(&workspace.id, task)?.join("task.json"))?;
        ensure!(
            record.schema_version == 1
                && record.workspace_id == workspace.id
                && record.task_id == task,
            "task identity mismatch"
        );
        ensure!(
            record.membership_digest == checkpoint::digest(&serde_json::to_vec(workspace)?),
            "workspace membership changed; explicit task migration is required"
        );
        Ok(record)
    }
    fn save(&self, task: &Task) -> Result<()> {
        write(
            &self
                .task_path(&task.workspace_id, &task.task_id)?
                .join("task.json"),
            task,
        )
    }

    /// Idempotent owner-local registration. Cloning is deferred until task
    /// creation; no worktree is attached to an existing user's checkout.
    pub fn register(&self, workspace: &Workspace) -> Result<()> {
        ensure!(
            workspace.schema_version == 1
                && !workspace.host_id.is_empty()
                && !workspace.agents.is_empty()
                && !workspace.repositories.is_empty(),
            "invalid workspace registration"
        );
        let root = self.workspace_path(&workspace.id)?;
        let mut names = BTreeSet::new();
        for repo in &workspace.repositories {
            git::valid_id(&repo.name)?;
            git::valid_branch(&repo.integration_branch)?;
            git::valid_branch(&repo.release_branch)?;
            ensure!(names.insert(&repo.name), "duplicate repository name");
            ensure!(
                !repo.source.is_empty()
                    && !repo.source.starts_with('-')
                    && !repo.source.contains('\n'),
                "invalid repository source"
            );
        }
        directory(&root)?;
        let _lock = lock(&root.join("workspace.lock"))?;
        let path = root.join("workspace.json");
        if path.exists() {
            let existing: Workspace = read(&path)?;
            ensure!(
                existing == *workspace,
                "workspace already registered with different membership or grants"
            );
        } else {
            // Registration is metadata-only; task creation prepares repositories.
            write(&path, workspace)?;
        }
        directory(&root.join("repositories"))?;
        directory(&root.join("tasks"))?;

        Ok(())
    }

    fn prepare_repositories(&self, workspace: &Workspace) -> Result<()> {
        let root = self.workspace_path(&workspace.id)?;
        for repo in &workspace.repositories {
            let target = root.join("repositories").join(&repo.name);
            if !target.exists() {
                let temporary = root
                    .join("repositories")
                    .join(format!("pending-{}", repo.name));
                ensure!(
                    !temporary.exists(),
                    "interrupted clone requires administrator inspection of pending repository"
                );
                git::run(
                    &root,
                    &[
                        "clone",
                        "--bare",
                        "--no-hardlinks",
                        "--",
                        &repo.source,
                        temporary.to_str().context("repository path")?,
                    ],
                )
                .with_context(|| format!("repository {}: clone failed", repo.name))?;
                fs::rename(temporary, &target)?;
            }
            ensure!(
                git::text(&target, &["rev-parse", "--is-bare-repository"])
                    .with_context(|| format!("repository {}: validate managed clone", repo.name))?
                    == "true",
                "managed repository is not bare"
            );
            let reference = format!("refs/heads/{}", repo.integration_branch);
            git::run(&target, &["fetch", "--no-tags", "origin", &format!("+{reference}:{reference}")])
                .with_context(|| format!("repository {}: fetch integration branch {} failed; verify remote access and branch existence", repo.name, repo.integration_branch))?;
            git::text(
                &target,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{}^{{commit}}", repo.integration_branch),
                ],
            )
            .with_context(|| {
                format!(
                    "repository {}: integration branch {} does not resolve to a commit",
                    repo.name, repo.integration_branch
                )
            })?;
        }
        Ok(())
    }

    pub fn create_task(&self, workspace_id: &str, task_id: &str, agent: &str) -> Result<Task> {
        let workspace = self.workspace(workspace_id, agent)?;
        let root = self.workspace_path(workspace_id)?;
        let task_root = self.task_path(workspace_id, task_id)?;
        let _workspace_lock = lock(&root.join("workspace.lock"))?;
        directory(&task_root)?;
        let _task_lock = lock(&task_root.join("task.lock"))?;
        directory(&task_root.join("tree"))?;
        let metadata = task_root.join("task.json");
        let mut task = if metadata.exists() {
            self.load_task(&workspace, task_id)?
        } else {
            // Only new tasks refresh remote branches. Existing tasks retain their pinned bases.
            self.prepare_repositories(&workspace)?;
            let mut checkouts = Vec::new();
            for repo in &workspace.repositories {
                let source = root.join("repositories").join(&repo.name);
                let reference = format!("refs/heads/{}", repo.integration_branch);
                checkouts.push(Checkout {
                    repository: repo.name.clone(),
                    branch: format!("agent/{task_id}"),
                    base_commit: git::text(
                        &source,
                        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
                    )?,
                    integration_branch: repo.integration_branch.clone(),
                    release_branch: repo.release_branch.clone(),
                    path: task_root.join("tree").join(&repo.name),
                    commit: None,
                });
            }
            Task {
                schema_version: 1,
                workspace_id: workspace_id.into(),
                task_id: task_id.into(),
                membership_digest: checkpoint::digest(&serde_json::to_vec(&workspace)?),
                state: TaskState::Provisioning,
                generation: 0,
                writer: None,
                last_implementer: None,
                contributors: BTreeSet::new(),
                checkouts,
                checkpoint: None,
                review: None,
                commit_message: None,
                normalized_commit_message: None,
                execution_context: None,
            }
        };
        if task.state != TaskState::Provisioning {
            return Ok(task);
        }
        self.save(&task)?;
        for checkout in &task.checkouts {
            let source = root.join("repositories").join(&checkout.repository);
            if !checkout.path.exists() {
                repair_missing_worktree(&source, checkout)?;
                let branch = format!("refs/heads/{}", checkout.branch);
                // A prior worktree-add may have created the ref before interruption.
                if git::text(&source, &["rev-parse", "--verify", &branch]).is_ok() {
                    ensure!(
                        git::text(&source, &["rev-parse", &branch])? == checkout.base_commit,
                        "existing task branch has unexpected commit"
                    );
                    git::run(
                        &source,
                        &[
                            "worktree",
                            "add",
                            checkout.path.to_str().context("worktree path")?,
                            &checkout.branch,
                        ],
                    )?;
                } else {
                    git::run(
                        &source,
                        &[
                            "worktree",
                            "add",
                            "-b",
                            &checkout.branch,
                            checkout.path.to_str().context("worktree path")?,
                            &checkout.base_commit,
                        ],
                    )?;
                }
            }
            ensure!(
                git::text(&checkout.path, &["symbolic-ref", "--short", "HEAD"])? == checkout.branch,
                "existing directory is not the task branch"
            );
            ensure!(
                git::text(&checkout.path, &["rev-parse", "HEAD"])? == checkout.base_commit,
                "provisioning worktree moved from base"
            );
            ensure!(
                git::text(
                    &checkout.path,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"]
                )? == source.to_str().context("source path")?,
                "worktree belongs to a different repository"
            );
        }
        task.state = TaskState::Ready;
        self.save(&task)?;
        Ok(task)
    }

    pub fn status(&self, workspace: &str, task: &str, agent: &str) -> Result<Task> {
        self.load_task(&self.workspace(workspace, agent)?, task)
    }

    pub fn freeze(&self, workspace: &str, task_id: &str, agent: &str) -> Result<Task> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "review operation is not granted"
        );
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            task.state == TaskState::Ready,
            "only an idle implementation can be frozen"
        );
        task.checkpoint = Some(checkpoint::capture(&task.checkouts)?);
        task.review = None;
        task.state = TaskState::Frozen;
        self.save(&task)?;
        Ok(task)
    }

    pub fn review(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        digest: &str,
        approved: bool,
        findings: String,
    ) -> Result<Task> {
        ensure!(findings.len() <= 64 * 1024, "review findings exceed 64 KiB");
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "review operation is not granted"
        );
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            task.state == TaskState::Frozen,
            "task is not awaiting review"
        );
        ensure!(
            !task.contributors.contains(agent),
            "implementation requires an independent reviewer"
        );
        let current = checkpoint::capture(&task.checkouts)?;
        ensure!(
            task.checkpoint.as_ref() == Some(&current) && current.digest == digest,
            "review checkpoint changed"
        );
        task.review = Some(Review {
            reviewer: agent.into(),
            checkpoint_digest: digest.into(),
            approved,
            findings,
        });
        if approved {
            task.state = TaskState::Approved;
        }
        self.save(&task)?;
        Ok(task)
    }

    pub fn remediate(&self, workspace: &str, task_id: &str, agent: &str) -> Result<Task> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Edit),
            "edit operation is not granted"
        );
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(task.state, TaskState::Frozen | TaskState::Approved),
            "task cannot enter remediation"
        );
        task.state = TaskState::Ready;
        task.checkpoint = None;
        task.review = None;
        self.save(&task)?;
        Ok(task)
    }

    pub fn files(&self, workspace: &str, task_id: &str, agent: &str) -> Result<crate::Checkpoint> {
        let workspace = self.workspace(workspace, agent)?;
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            !matches!(
                task.state,
                TaskState::Running | TaskState::Interrupted | TaskState::Provisioning
            ),
            "task is not stable"
        );
        checkpoint::capture(&task.checkouts)
    }

    pub fn read_file(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        repository: &str,
        path: &str,
    ) -> Result<FileContent> {
        let workspace = self.workspace(workspace, agent)?;
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            !matches!(
                task.state,
                TaskState::Running | TaskState::Interrupted | TaskState::Provisioning
            ),
            "task is not stable"
        );
        let checkout = task
            .checkouts
            .iter()
            .find(|r| r.repository == repository)
            .context("repository is not in this workspace")?;
        let path = checkpoint::safe_file(&checkout.path, path)?;
        ensure!(
            fs::metadata(&path)?.len() <= 1024 * 1024,
            "file exceeds tool response limit"
        );
        let bytes = fs::read(path)?;
        Ok(FileContent {
            digest: checkpoint::digest(&bytes),
            content: String::from_utf8(bytes).context("file is not UTF-8 text")?,
        })
    }

    /// A single atomic file replacement with compare-and-swap protection. Each
    /// call holds the task lock; a frozen task rejects edits from every agent.
    pub fn edit_file(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        edit: FileEdit,
    ) -> Result<Task> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Edit),
            "edit operation is not granted"
        );
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(task.state == TaskState::Ready, "task is not writable");
        let checkout = task
            .checkouts
            .iter()
            .find(|r| r.repository == edit.repository)
            .context("repository is not in this workspace")?;
        let path = checkpoint::safe_file(&checkout.path, &edit.path)?;
        checkpoint::reject_submodule_edit(&checkout.path, &edit.path)?;
        let existing = if path.exists() {
            Some(checkpoint::digest(&fs::read(&path)?))
        } else {
            None
        };
        ensure!(
            existing == edit.expected_digest,
            "file changed since it was read"
        );
        task.last_implementer = Some(agent.into());
        task.contributors.insert(agent.into());
        task.generation = task
            .generation
            .checked_add(1)
            .context("lease generation exhausted")?;
        self.save(&task)?;
        if let Some(content) = edit.content {
            ensure!(content.len() <= 1024 * 1024, "file exceeds edit limit");
            directory(path.parent().context("file parent")?)?;
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let mut out = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            if path.exists() {
                out.set_permissions(fs::metadata(&path)?.permissions())?;
            }
            out.write_all(content.as_bytes())?;
            out.sync_all()?;
            fs::rename(temporary, &path)?;
        } else {
            ensure!(existing.is_some(), "file does not exist");
            fs::remove_file(&path)?;
        }
        File::open(path.parent().context("file parent")?)?.sync_all()?;
        Ok(task)
    }

    /// The kernel holds the lock throughout the sandbox lifetime. A crashed
    /// supervisor leaves Running in the journal; automatic takeover is forbidden.
    #[allow(clippy::too_many_arguments)]
    pub fn run(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        access: Access,
        executable: &Path,
        args: &[String],
        timeout_seconds: u64,
    ) -> Result<crate::ExecutionOutput> {
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Execute),
            "execute operation is not granted"
        );
        ensure!(
            workspace.operations.contains(&match access {
                Access::Implement => Operation::Edit,
                Access::Review => Operation::Review,
            }),
            "execution role is not granted"
        );
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        match access {
            Access::Implement => ensure!(task.state == TaskState::Ready, "task is not writable"),
            Access::Review => ensure!(
                matches!(task.state, TaskState::Frozen | TaskState::Approved),
                "task is not frozen"
            ),
        }
        if access == Access::Review {
            ensure!(
                task.checkpoint.as_ref() == Some(&checkpoint::capture(&task.checkouts)?),
                "review checkpoint changed"
            );
        }
        ensure!(
            (1..=300).contains(&timeout_seconds),
            "execution timeout must be between 1 and 300 seconds"
        );
        let mut command = self.sandbox_command(&task, access, executable, args)?;
        self.run_prepared(task, agent, access, &mut command, timeout_seconds)
    }

    fn run_prepared(
        &self,
        mut task: Task,
        agent: &str,
        access: Access,
        command: &mut Command,
        timeout_seconds: u64,
    ) -> Result<crate::ExecutionOutput> {
        let previous_task = task.clone();
        let previous_state = task.state;
        task.execution_context = Some(crate::ExecutionContext {
            access,
            previous_state,
        });
        task.state = TaskState::Running;
        task.generation = task
            .generation
            .checked_add(1)
            .context("lease generation exhausted")?;
        task.writer = Some(agent.into());
        if access == Access::Implement {
            task.last_implementer = Some(agent.into());
            task.contributors.insert(agent.into());
        }
        self.save(&task)?;
        let result = bounded_output(command, timeout_seconds);
        task.writer = None;
        task.state = match &result {
            Ok(output) if !output.timed_out && output.exit_code.is_some() => previous_state,
            Err(error) if error.downcast_ref::<SpawnFailure>().is_some() => {
                let generation = task.generation;
                task = previous_task;
                task.generation = generation;
                previous_state
            }
            _ => TaskState::Interrupted,
        };
        if access == Access::Review
            && (task.checkpoint.is_none()
                || checkpoint::capture(&task.checkouts).ok().as_ref() != task.checkpoint.as_ref())
        {
            task.state = TaskState::Interrupted;
        }
        if task.state != TaskState::Interrupted {
            task.execution_context = None;
        }
        self.save(&task)?;
        result.context("run workspace sandbox")
    }

    /// Linux namespace sandbox with no network, host home, or write access to Git
    /// administrative state. Trusted administrative operations stay outside it.
    fn sandbox_command(
        &self,
        task: &Task,
        access: Access,
        executable: &Path,
        args: &[String],
    ) -> Result<Command> {
        ensure!(
            executable.is_absolute() && executable.starts_with("/usr"),
            "sandbox executable must be installed under /usr"
        );
        let mut cmd = Command::new("/usr/bin/bwrap");
        cmd.env_clear().args([
            "--unshare-all",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
            "--ro-bind",
            "/usr",
            "/usr",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
            "--dir",
            "/home/worker",
            "--setenv",
            "HOME",
            "/home/worker",
            "--setenv",
            "PATH",
            "/usr/bin:/bin",
            "--setenv",
            "GIT_OPTIONAL_LOCKS",
            "0",
        ]);
        for path in ["/bin", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                cmd.args(["--ro-bind", path, path]);
            }
        }
        let tree = self
            .task_path(&task.workspace_id, &task.task_id)?
            .join("tree");
        cmd.arg(if access == Access::Implement {
            "--bind"
        } else {
            "--ro-bind"
        })
        .arg(&tree)
        .arg(&tree);
        for checkout in &task.checkouts {
            checkpoint::validate_checkout(checkout)?;
            cmd.arg("--ro-bind")
                .arg(checkout.path.join(".git"))
                .arg(checkout.path.join(".git"));
            let source = self
                .workspace_path(&task.workspace_id)?
                .join("repositories")
                .join(&checkout.repository);
            cmd.arg("--ro-bind").arg(&source).arg(&source);
        }
        cmd.arg("--chdir")
            .arg(&tree)
            .arg("--")
            .arg(executable)
            .args(args);
        Ok(cmd)
    }

    /// Recovery is deliberately explicit and owner-local. Call only after the
    /// runner has confirmed all processes of the old execution are terminated.
    pub fn recover_after_fencing(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
    ) -> Result<Task> {
        self.recover_generation_after_fencing(workspace, task_id, agent, None)
    }

    pub fn recover_generation_after_fencing(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        expected_generation: Option<u64>,
    ) -> Result<Task> {
        let workspace = self.workspace(workspace, agent)?;
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(task.state, TaskState::Running | TaskState::Interrupted),
            "task does not need recovery"
        );
        if let Some(expected) = expected_generation {
            ensure!(
                task.generation == expected,
                "execution generation changed; fencing evidence is stale"
            );
        }
        let restore_review = task
            .execution_context
            .as_ref()
            .filter(|context| {
                context.access == Access::Review
                    && matches!(
                        context.previous_state,
                        TaskState::Frozen | TaskState::Approved
                    )
            })
            .filter(|_| {
                task.checkpoint.is_some()
                    && checkpoint::capture(&task.checkouts).ok().as_ref()
                        == task.checkpoint.as_ref()
            })
            .map(|context| context.previous_state);
        task.state = restore_review.unwrap_or(TaskState::Ready);
        task.writer = None;
        if restore_review.is_none() {
            task.checkpoint = None;
            task.review = None;
        }
        task.execution_context = None;
        task.generation = task
            .generation
            .checked_add(1)
            .context("lease generation exhausted")?;
        self.save(&task)?;
        Ok(task)
    }

    /// Commit exactly the reviewed content, independently in each repository.
    /// No push occurs here. A crash leaves Committing and can be reconciled by
    /// calling with the identical message; already-created commits are recognized
    /// only when parent, tree and message match the prepared content.
    pub fn commit(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        message: &str,
    ) -> Result<Task> {
        ensure!(!message.trim().is_empty(), "commit message is required");
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Commit),
            "commit operation is not granted"
        );
        let _workspace_lock = lock(&self.workspace_path(&workspace.id)?.join("workspace.lock"))?;
        let _lock = lock(&self.task_path(&workspace.id, task_id)?.join("task.lock"))?;
        let mut task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(
                task.state,
                TaskState::Approved | TaskState::Committing | TaskState::Committed
            ),
            "task has not passed review"
        );
        if task.state == TaskState::Committed {
            ensure!(
                task.commit_message.as_deref() == Some(message),
                "commit message differs from completed action"
            );
            return Ok(task);
        }
        let expected = task
            .checkpoint
            .clone()
            .context("review checkpoint missing")?;
        ensure!(
            task.review
                .as_ref()
                .is_some_and(|r| r.approved && r.checkpoint_digest == expected.digest),
            "approved review missing"
        );
        if task.state == TaskState::Approved {
            ensure!(
                checkpoint::capture(&task.checkouts)? == expected,
                "reviewed content changed"
            );
            task.commit_message = Some(message.into());
            task.state = TaskState::Committing;
            self.save(&task)?;
        } else {
            ensure!(
                task.commit_message.as_deref() == Some(message),
                "retry commit message differs"
            );
        }
        if task.normalized_commit_message.is_none() {
            task.normalized_commit_message =
                Some(normalize_commit_message(&task.checkouts[0].path, message)?);
            self.save(&task)?;
        }
        let normalized_message = task
            .normalized_commit_message
            .clone()
            .context("canonical commit message missing")?;
        for index in 0..task.checkouts.len() {
            let checkout = &task.checkouts[index];
            let reviewed = &expected.repositories[index];
            let observed = checkpoint::capture(std::slice::from_ref(checkout))?
                .repositories
                .remove(0);
            ensure!(
                observed.files == reviewed.files,
                "working content differs from review"
            );
            if observed.head != reviewed.head {
                ensure!(
                    git::text(&checkout.path, &["rev-parse", "HEAD^"])? == reviewed.head,
                    "unexpected commit parent"
                );
                ensure!(
                    git::run(
                        &checkout.path,
                        &["show", "-s", "--format=format:%B", "HEAD"]
                    )? == normalized_message.as_bytes(),
                    "unexpected commit message"
                );
                ensure!(
                    git::run(
                        &checkout.path,
                        &["status", "--porcelain=v1", "-z", "--untracked-files=all"]
                    )?
                    .is_empty(),
                    "unexpected changes after commit"
                );
            } else {
                git::run(&checkout.path, &["add", "--all", "--", "."])?;
                // Git attributes/clean filters may transform bytes. Reject those
                // transformations rather than commit content the reviewer did not see.
                for file in &reviewed.files {
                    ensure!(
                        checkpoint::digest(&git::run(
                            &checkout.path,
                            &["show", &format!(":{}", file.path)]
                        )?) == file.digest,
                        "Git clean filter changed reviewed content"
                    );
                }
                if !git::run(&checkout.path, &["diff", "--cached", "--name-only", "-z"])?.is_empty()
                {
                    git::run(
                        &checkout.path,
                        &[
                            "commit",
                            "--no-gpg-sign",
                            "--cleanup=verbatim",
                            "-m",
                            &normalized_message,
                        ],
                    )?;
                }
            }
            task.checkouts[index].commit = Some(git::text(
                &task.checkouts[index].path,
                &["rev-parse", "HEAD"],
            )?);
            self.save(&task)?;
        }
        task.state = TaskState::Committed;
        self.save(&task)?;
        Ok(task)
    }
}

pub(crate) fn bounded_output(
    command: &mut Command,
    timeout_seconds: u64,
) -> Result<crate::ExecutionOutput> {
    command.stdin(Stdio::null());
    bounded_command(command, timeout_seconds)
}

pub(crate) fn bounded_command(
    command: &mut Command,
    timeout_seconds: u64,
) -> Result<crate::ExecutionOutput> {
    use std::os::unix::process::CommandExt;
    let mut child = command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(SpawnFailure)?;
    let process_group = i32::try_from(child.id()).context("process group id")?;
    fn drain(mut input: impl Read) -> Vec<u8> {
        let mut saved = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(count) = input.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let keep = count.min((1024 * 1024usize).saturating_sub(saved.len()));
            saved.extend_from_slice(&chunk[..keep]);
        }
        saved
    }
    let stdout = child.stdout.take().context("sandbox stdout")?;
    let stderr = child.stderr.take().context("sandbox stderr")?;
    let out_reader = std::thread::spawn(move || drain(stdout));
    let err_reader = std::thread::spawn(move || drain(stderr));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_seconds);
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if std::time::Instant::now() >= deadline {
            // The configured one-shot command owns this fresh process group.
            // Kill descendants too so inherited output pipes cannot hold the
            // caller forever after an indexer or sandbox timeout.
            unsafe {
                libc::kill(-process_group, libc::SIGKILL);
            }
            break (child.wait()?, true);
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    unsafe {
        libc::kill(-process_group, libc::SIGKILL);
    }
    Ok(crate::ExecutionOutput {
        exit_code: status.code(),
        timed_out,
        stdout: String::from_utf8_lossy(
            &out_reader
                .join()
                .map_err(|_| anyhow::anyhow!("stdout reader failed"))?,
        )
        .into(),
        stderr: String::from_utf8_lossy(
            &err_reader
                .join()
                .map_err(|_| anyhow::anyhow!("stderr reader failed"))?,
        )
        .into(),
    })
}

#[derive(Debug)]
struct SpawnFailure(std::io::Error);
impl std::fmt::Display for SpawnFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "could not spawn command: {}", self.0)
    }
}
impl std::error::Error for SpawnFailure {}

fn normalize_commit_message(repository: &Path, message: &str) -> Result<String> {
    use std::io::Seek;
    let mut input = tempfile::tempfile()?;
    input.write_all(message.as_bytes())?;
    input.rewind()?;
    let output = git::command(repository)
        .arg("stripspace")
        .stdin(Stdio::from(input))
        .output()?;
    ensure!(output.status.success(), "cannot normalize commit message");
    let normalized = String::from_utf8(output.stdout)?;
    ensure!(
        !normalized.is_empty(),
        "commit message is empty after normalization"
    );
    Ok(normalized)
}

fn repair_missing_worktree(source: &Path, checkout: &Checkout) -> Result<()> {
    let listing = git::run(source, &["worktree", "list", "--porcelain", "-z"])?;
    let listing = std::str::from_utf8(&listing)?;
    for record in listing.split("\0\0") {
        let fields: Vec<_> = record.split('\0').collect();
        if fields.first().copied() == Some(format!("worktree {}", checkout.path.display()).as_str())
        {
            ensure!(
                fields.contains(&format!("HEAD {}", checkout.base_commit).as_str())
                    && fields.contains(&format!("branch refs/heads/{}", checkout.branch).as_str())
                    && !fields.iter().any(|field| field.starts_with("locked")),
                "stale worktree registration differs from provisioning intent"
            );
            // The caller already proved the directory is missing and holds the
            // workspace lock. Remove only this task's stale registration, never
            // broadly prune other tasks' missing/locked worktrees.
            git::run(
                source,
                &[
                    "worktree",
                    "remove",
                    "--force",
                    "--",
                    checkout.path.to_str().context("worktree path")?,
                ],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_spawn_restores_persisted_state_without_fencing() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let store = WorkspaceStore::open(temp.path()).unwrap();
        fs::create_dir_all(temp.path().join("portal/tasks/spawn")).unwrap();
        let task: Task = serde_json::from_value(serde_json::json!({
            "schemaVersion":1,"workspaceId":"portal","taskId":"spawn",
            "membershipDigest":"fixture","state":"ready","generation":3,
            "writer":null,"lastImplementer":null,"checkouts":[],"checkpoint":null,
            "review":null,"commitMessage":null
        }))
        .unwrap();
        let error = store
            .run_prepared(
                task.clone(),
                "codex",
                Access::Implement,
                &mut Command::new(temp.path().join("nonexistent-sandbox")),
                1,
            )
            .unwrap_err();
        assert!(error.downcast_ref::<SpawnFailure>().is_some());
        let restored: Task = read(&temp.path().join("portal/tasks/spawn/task.json")).unwrap();
        let mut expected = task;
        expected.generation += 1;
        assert_eq!(restored, expected);
    }
}
