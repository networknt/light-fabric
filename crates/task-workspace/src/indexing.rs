//! Index only disposable checkpoint copies. Provider commands are host-owned
//! registration, never model-supplied commands or arguments.
use crate::{
    Operation, TaskState, WorkspaceStore, checkpoint, git,
    store::{bounded_output, lock, read, write},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexerCommand {
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexReceipt {
    pub provider: String,
    pub checkpoint_digest: String,
    pub root: PathBuf,
    pub repositories: Vec<PathBuf>,
    pub state: String,
    pub fresh: bool,
}

impl WorkspaceStore {
    pub fn index(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        provider: &str,
    ) -> Result<IndexReceipt> {
        git::valid_id(provider)?;
        let workspace = self.workspace(workspace, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "indexing requires review operation"
        );
        let config = workspace
            .indexers
            .get(provider)
            .context("index provider is not configured by host")?;
        ensure!(
            config.executable.is_absolute() && (1..=300).contains(&config.timeout_seconds),
            "invalid indexer command"
        );
        let task_root = self.task_path(&workspace.id, task_id)?;
        let _task_lock = lock(&task_root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(task.state, TaskState::Frozen | TaskState::Approved),
            "indexing requires a frozen checkpoint"
        );
        let checkpoint = task.checkpoint.as_ref().context("checkpoint missing")?;
        ensure!(
            *checkpoint == checkpoint::capture(&task.checkouts)?,
            "checkpoint changed before indexing"
        );
        let receipt_path = task_root.join(format!("index-{provider}.json"));
        let active: Option<IndexReceipt> = if receipt_path.exists() {
            Some(read(&receipt_path)?)
        } else {
            None
        };
        if let Some(active) = &active {
            validate_index_root(&task_root, provider, &active.root)?;
        }
        cleanup_index_roots(
            &task_root,
            provider,
            active.as_ref().map(|active| active.root.as_path()),
        )?;
        let pending_path = task_root.join(format!("index-{provider}.pending.json"));
        if pending_path.exists() {
            fs::remove_file(&pending_path)?;
        }
        if let Some(active) = active.as_ref().filter(|active| {
            active.state == "succeeded"
                && active.checkpoint_digest == checkpoint.digest
                && active.root.is_dir()
        }) {
            let mut active = active.clone();
            active.fresh = true;
            return Ok(active);
        }
        let root = task_root.join(format!("index-{provider}-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let home = root.join("home");
        fs::create_dir(&home)?;
        let mut receipt = IndexReceipt {
            provider: provider.into(),
            checkpoint_digest: checkpoint.digest.clone(),
            root: root.clone(),
            repositories: Vec::new(),
            state: "in-flight".into(),
            fresh: false,
        };
        write(&pending_path, &receipt)?;
        let build = (|| -> Result<()> {
            for (checkout, reviewed) in task.checkouts.iter().zip(&checkpoint.repositories) {
                let destination = root.join(&checkout.repository);
                let source = self
                    .workspace_path(&workspace.id)?
                    .join("repositories")
                    .join(&checkout.repository);
                git::run(
                    &root,
                    &[
                        "clone",
                        "--no-hardlinks",
                        "--no-checkout",
                        "--",
                        source.to_str().context("source path")?,
                        destination.to_str().context("destination path")?,
                    ],
                )?;
                git::run(&destination, &["checkout", "--detach", &reviewed.head])?;
                // Remove tracked baseline files before overlaying the exact reviewed
                // content so deletions, additions and executable modes are represented.
                let tracked = git::run(&destination, &["ls-files", "-z"])?;
                for name in std::str::from_utf8(&tracked)?
                    .split('\0')
                    .filter(|name| !name.is_empty())
                {
                    let path = checkpoint::safe_file(&destination, name)?;
                    if path.exists() {
                        fs::remove_file(path)?;
                    }
                }
                for file in &reviewed.files {
                    let source = checkpoint::safe_file(&checkout.path, &file.path)?;
                    let target = checkpoint::safe_file(&destination, &file.path)?;
                    fs::create_dir_all(target.parent().context("file parent")?)?;
                    let bytes = fs::read(source)?;
                    ensure!(
                        checkpoint::digest(&bytes) == file.digest,
                        "checkpoint changed while copying index input"
                    );
                    fs::write(&target, bytes)?;
                    fs::set_permissions(
                        target,
                        fs::Permissions::from_mode(if file.executable { 0o700 } else { 0o600 }),
                    )?;
                }
                let mut command = Command::new(&config.executable);
                command
                    .args(&config.args)
                    .current_dir(&destination)
                    .env("HOME", &home)
                    .env("XDG_CACHE_HOME", home.join(".cache"))
                    .env("CBM_CACHE_DIR", home.join(".cache/codebase-memory-mcp"))
                    .env("CBM_ALLOWED_ROOT", &root)
                    .env("TASK_WORKSPACE_CHECKPOINT", &checkpoint.digest)
                    .env("TASK_WORKSPACE_REPOSITORY", &checkout.repository);
                if provider == "codebase-memory-mcp" {
                    ensure!(
                        config.args.is_empty(),
                        "codebase-memory-mcp uses a fixed indexing command"
                    );
                    command.args(["cli", "--json", "index_repository"]).arg(
                        serde_json::json!({"repo_path":destination,"name":checkout.repository})
                            .to_string(),
                    );
                }
                inherit_index_lock(&mut command, &_task_lock);
                let output = bounded_output(&mut command, config.timeout_seconds)?;
                write(
                    &root.join(format!("{}-output.json", checkout.repository)),
                    &output,
                )?;
                ensure!(
                    !output.timed_out && output.exit_code == Some(0),
                    "indexer failed; source task remains frozen"
                );
                if provider == "codebase-memory-mcp" {
                    let response: serde_json::Value = serde_json::from_str(&output.stdout)?;
                    ensure!(
                        response["isError"] == false && response["structuredContent"].is_object(),
                        "codebase-memory-mcp did not return a successful indexing result"
                    );
                }
                receipt.repositories.push(destination);
                write(&pending_path, &receipt)?;
            }
            ensure!(
                *checkpoint == checkpoint::capture(&task.checkouts)?,
                "source changed during indexing"
            );
            Ok(())
        })();
        if let Err(error) = build {
            write(
                &task_root.join(format!("index-{provider}.failure.json")),
                &serde_json::json!({"checkpointDigest":checkpoint.digest,"error":error.to_string()}),
            )?;
            cleanup_index_roots(
                &task_root,
                provider,
                active.as_ref().map(|active| active.root.as_path()),
            )?;
            fs::remove_file(&pending_path)?;
            return Err(error);
        }
        receipt.state = "succeeded".into();
        receipt.fresh = true;
        write(&receipt_path, &receipt)?;
        fs::remove_file(&pending_path)?;
        cleanup_index_roots(&task_root, provider, Some(&receipt.root))?;
        Ok(receipt)
    }

    pub fn index_status(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        provider: &str,
    ) -> Result<IndexReceipt> {
        git::valid_id(provider)?;
        let workspace = self.workspace(workspace, agent)?;
        let root = self.task_path(&workspace.id, task_id)?;
        let _lock = lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        let mut receipt: IndexReceipt = read(&root.join(format!("index-{provider}.json")))?;
        receipt.fresh = receipt.state == "succeeded"
            && matches!(task.state, TaskState::Frozen | TaskState::Approved)
            && checkpoint::capture(&task.checkouts)?.digest == receipt.checkpoint_digest;
        Ok(receipt)
    }
}

impl WorkspaceStore {
    /// Read-only provider query scoped to a repository in this task's current
    /// checkpoint. Provider cache HOME is private to this task/index generation.
    #[allow(clippy::too_many_arguments)]
    pub fn query_index(
        &self,
        workspace: &str,
        task_id: &str,
        agent: &str,
        provider: &str,
        repository: &str,
        query: &str,
    ) -> Result<crate::ExecutionOutput> {
        git::valid_id(provider)?;
        let workspace = self.workspace(workspace, agent)?;
        let config = workspace
            .indexers
            .get(provider)
            .context("index provider is not configured")?;
        let root = self.task_path(&workspace.id, task_id)?;
        let _lock = lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            matches!(task.state, TaskState::Frozen | TaskState::Approved),
            "query requires a frozen checkpoint"
        );
        ensure!(
            task.checkouts.iter().any(|r| r.repository == repository),
            "repository is not in workspace"
        );
        let receipt: IndexReceipt = read(&root.join(format!("index-{provider}.json")))?;
        ensure!(
            receipt.state == "succeeded"
                && checkpoint::capture(&task.checkouts)?.digest == receipt.checkpoint_digest,
            "index is stale; refresh it for the current checkpoint"
        );
        let home = receipt.root.join("home");
        let mut command = Command::new(&config.executable);
        command
            .current_dir(receipt.root.join(repository))
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("CBM_CACHE_DIR", home.join(".cache/codebase-memory-mcp"))
            .env("CBM_ALLOWED_ROOT", &receipt.root);
        match provider {
            "gitnexus" => {
                command.args(["query", "--repo", repository, "--query", query]);
            }
            "codebase-memory-mcp" => {
                command.args(["cli", "--json", "search_graph"]).arg(
                    serde_json::json!({"project":repository,"name_pattern":query}).to_string(),
                );
            }
            _ => anyhow::bail!("provider has no qualified query adapter"),
        }
        let output = bounded_output(&mut command, config.timeout_seconds)?;
        ensure!(
            !output.timed_out && output.exit_code == Some(0),
            "index query failed"
        );
        if provider == "codebase-memory-mcp" {
            let response: serde_json::Value = serde_json::from_str(&output.stdout)?;
            ensure!(
                response["isError"] != true,
                "index query returned a tool error"
            );
        }
        Ok(output)
    }
}

fn validate_index_root(
    task_root: &std::path::Path,
    provider: &str,
    root: &std::path::Path,
) -> Result<()> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("invalid index directory")?;
    ensure!(
        root.parent() == Some(task_root)
            && name
                .strip_prefix(&format!("index-{provider}-"))
                .is_some_and(|suffix| uuid::Uuid::parse_str(suffix).is_ok())
            && !root.is_symlink(),
        "index root is outside the managed task generation directories"
    );
    Ok(())
}

fn cleanup_index_roots(
    task_root: &std::path::Path,
    provider: &str,
    keep: Option<&std::path::Path>,
) -> Result<()> {
    for entry in fs::read_dir(task_root)? {
        let entry = entry?;
        let path = entry.path();
        if keep == Some(path.as_path()) || !entry.file_type()?.is_dir() {
            continue;
        }
        if validate_index_root(task_root, provider, &path).is_ok() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn inherit_index_lock(command: &mut Command, lock: &fs::File) {
    use std::os::{fd::AsRawFd, unix::process::CommandExt};
    let fd = lock.as_raw_fd();
    // The one-shot provider and its descendants keep the kernel lock if this
    // service crashes. A restart therefore cannot reclaim a live generation.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}
