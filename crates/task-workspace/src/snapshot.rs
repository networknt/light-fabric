//! Manager-owned retained bytes and Git trees. These methods run under the same
//! task lock as worker edits; no real index, HEAD or task branch is changed.
use crate::{Checkpoint, Operation, TaskState, WorkspaceStore, checkpoint, git, store};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::Path, process::Stdio};

const MAX_PACKAGE: usize = 32 * 1024 * 1024;
pub const SNAPSHOT_CHUNK_BYTES: usize = 128 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotFile {
    pub executable: bool,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotRepository {
    pub base_commit: String,
    pub tree: String,
    pub files: BTreeMap<String, SnapshotFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotPackage {
    pub schema_version: u16,
    pub workspace_id: String,
    pub task_id: String,
    pub feature_id: String,
    pub stage_id: String,
    pub snapshot_id: String,
    pub checkpoint: Checkpoint,
    pub repositories: BTreeMap<String, SnapshotRepository>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotReceipt {
    pub snapshot_id: String,
    pub package_digest: String,
    pub package_bytes: u64,
    pub checkpoint_digest: String,
    pub trees: BTreeMap<String, String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotChunk {
    pub package_digest: String,
    pub total_bytes: u64,
    pub offset: u64,
    pub bytes: Vec<u8>,
}

impl SnapshotPackage {
    /// Derive a review delta solely from retained, digest-bound packages. This
    /// works after runner refs/cache disappear and never opens the live task.
    pub fn verified_delta(
        &self,
        after: &Self,
        expected_before: &str,
        expected_after: &str,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        self.validate()?;
        after.validate()?;
        ensure!(
            self.receipt()?.package_digest == expected_before
                && after.receipt()?.package_digest == expected_after,
            "delta package digest mismatch"
        );
        ensure!(
            self.workspace_id == after.workspace_id
                && self.task_id == after.task_id
                && self.feature_id == after.feature_id
                && self.stage_id == after.stage_id
                && self.repositories.keys().eq(after.repositories.keys()),
            "delta scope changed"
        );
        let mut output = BTreeMap::new();
        let mut bytes = 0usize;
        for (name, first) in &self.repositories {
            let last = &after.repositories[name];
            ensure!(first.base_commit == last.base_commit, "delta base changed");
            ensure!(
                first.tree.len() == last.tree.len(),
                "delta object format changed"
            );
            let format = match first.tree.len() {
                40 => "--object-format=sha1",
                64 => "--object-format=sha256",
                _ => anyhow::bail!("invalid snapshot tree object ID"),
            };
            let temporary = tempfile::tempdir()?;
            git::run(temporary.path(), &["init", "--bare", format])?;
            ensure!(
                write_tree(temporary.path(), &first.files)? == first.tree
                    && write_tree(temporary.path(), &last.files)? == last.tree,
                "delta tree corrupt"
            );
            let delta = git::run(
                temporary.path(),
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--binary",
                    "--full-index",
                    &first.tree,
                    &last.tree,
                    "--",
                ],
            )?;
            bytes = bytes
                .checked_add(delta.len())
                .context("delta size overflow")?;
            ensure!(bytes <= MAX_PACKAGE, "review delta exceeds bound");
            output.insert(name.clone(), delta);
        }
        Ok(output)
    }

    /// Verify an exported package without relying on runner-local refs/cache.
    /// Git runs only against disposable private object databases, never a task.
    pub fn verified_receipt(&self) -> Result<SnapshotReceipt> {
        self.validate()?;
        for repository in self.repositories.values() {
            let temporary = tempfile::tempdir()?;
            let format = match repository.tree.len() {
                40 => "--object-format=sha1",
                64 => "--object-format=sha256",
                _ => anyhow::bail!("invalid snapshot tree object ID"),
            };
            git::run(temporary.path(), &["init", "--bare", format])?;
            ensure!(
                write_tree(temporary.path(), &repository.files)? == repository.tree,
                "snapshot tree does not match retained bytes"
            );
        }
        self.receipt()
    }

    fn receipt(&self) -> Result<SnapshotReceipt> {
        let bytes = serde_json::to_vec(self)?;
        ensure!(bytes.len() <= MAX_PACKAGE, "snapshot package exceeds bound");
        Ok(SnapshotReceipt {
            snapshot_id: self.snapshot_id.clone(),
            package_digest: checkpoint::digest(&bytes),
            package_bytes: bytes.len() as u64,
            checkpoint_digest: self.checkpoint.digest.clone(),
            trees: self
                .repositories
                .iter()
                .map(|(name, repo)| (name.clone(), repo.tree.clone()))
                .collect(),
        })
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && !self.repositories.is_empty(),
            "invalid snapshot version or repositories"
        );
        for id in [
            &self.workspace_id,
            &self.task_id,
            &self.feature_id,
            &self.stage_id,
            &self.snapshot_id,
        ] {
            git::valid_id(id)?;
        }
        ensure!(
            checkpoint::digest(&serde_json::to_vec(&self.checkpoint.repositories)?)
                == self.checkpoint.digest,
            "snapshot checkpoint digest mismatch"
        );
        ensure!(
            self.repositories.len() == self.checkpoint.repositories.len(),
            "snapshot repository set mismatch"
        );
        for cp in &self.checkpoint.repositories {
            let repo = self
                .repositories
                .get(&cp.repository)
                .context("snapshot repository missing")?;
            ensure!(
                repo.files.len() == cp.files.len(),
                "snapshot file set mismatch"
            );
            for file in &cp.files {
                let content = repo
                    .files
                    .get(&file.path)
                    .context("snapshot file missing")?;
                ensure!(
                    content.executable == file.executable
                        && checkpoint::digest(&content.bytes) == file.digest,
                    "snapshot bytes or mode mismatch"
                );
            }
        }
        self.receipt()?;
        Ok(())
    }
}

impl WorkspaceStore {
    /// Fixed job output travels through the existing Controller result channel.
    /// A runner-local path is never returned as transferable evidence.
    pub fn read_manager_snapshot(
        &self,
        spec: &workspace_execution_protocol::WorkspaceExecutionSpec,
    ) -> Result<serde_json::Value> {
        use workspace_execution_protocol::TaskSelection;
        spec.validate()?;
        let read = spec
            .manager_snapshot
            .as_ref()
            .context("manager snapshot read missing")?;
        let TaskSelection::Existing { task_id } = &spec.request.task else {
            anyhow::bail!("snapshot reads require an existing task");
        };
        let receipt = self.capture_snapshot(
            &spec.request.workspace_id,
            task_id,
            &spec.agent_id,
            &read.feature_id,
            &read.stage_id,
            &read.snapshot_id,
            &read.checkpoint_digest,
        )?;
        ensure!(
            read.package_digest
                .as_ref()
                .is_none_or(|d| d == &receipt.package_digest),
            "snapshot package changed during transfer"
        );
        let chunk = self.snapshot_chunk(
            &spec.request.workspace_id,
            task_id,
            &spec.agent_id,
            &read.snapshot_id,
            &receipt.package_digest,
            read.offset,
        )?;
        Ok(
            serde_json::json!({"managerSnapshot":read,"workspaceId":spec.request.workspace_id,
            "taskId":task_id,"receipt":receipt,"chunk":chunk}),
        )
    }

    // Explicit manager scope and immutable claim inputs; none are optional.
    #[allow(clippy::too_many_arguments)]
    pub fn capture_snapshot(
        &self,
        workspace_id: &str,
        task_id: &str,
        agent: &str,
        feature_id: &str,
        stage_id: &str,
        snapshot_id: &str,
        expected_checkpoint: &str,
    ) -> Result<SnapshotReceipt> {
        for id in [feature_id, stage_id, snapshot_id] {
            git::valid_id(id)?;
        }
        let workspace = self.workspace(workspace_id, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "snapshot review permission missing"
        );
        let root = self.task_path(workspace_id, task_id)?;
        let _lock = store::lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        let directory = root.join("snapshots");
        store::directory(&directory)?;
        let path = directory.join(format!("{snapshot_id}.json"));
        if path.exists() {
            let package: SnapshotPackage = store::read(&path)?;
            package.validate()?;
            ensure!(
                package.feature_id == feature_id
                    && package.stage_id == stage_id
                    && package.checkpoint.digest == expected_checkpoint,
                "snapshot replay input changed"
            );
            return package.receipt();
        }
        ensure!(
            !matches!(
                task.state,
                TaskState::Running | TaskState::Interrupted | TaskState::Provisioning
            ),
            "snapshot requires stable task"
        );
        let before = checkpoint::capture(&task.checkouts)?;
        ensure!(
            before.digest == expected_checkpoint,
            "snapshot checkpoint changed"
        );
        let mut repositories = BTreeMap::new();
        let mut total = 0usize;
        for checkout in &task.checkouts {
            let index = git::run(&checkout.path, &["ls-files", "--stage", "-z"])?;
            ensure!(
                !index.split(|b| *b == 0).any(|e| e.starts_with(b"160000 ")),
                "snapshot submodule requires separately registered repository"
            );
            let cp = before
                .repositories
                .iter()
                .find(|r| r.repository == checkout.repository)
                .context("checkpoint repository")?;
            let mut files = BTreeMap::new();
            for file in &cp.files {
                let path = checkpoint::safe_file(&checkout.path, &file.path)?;
                ensure!(
                    fs::metadata(&path)?.len() <= MAX_PACKAGE.saturating_sub(total) as u64,
                    "snapshot size exceeds limit"
                );
                let bytes = fs::read(path)?;
                total = total
                    .checked_add(bytes.len())
                    .context("snapshot size overflow")?;
                ensure!(
                    total <= MAX_PACKAGE && checkpoint::digest(&bytes) == file.digest,
                    "snapshot drift or size limit"
                );
                files.insert(
                    file.path.clone(),
                    SnapshotFile {
                        executable: file.executable,
                        bytes,
                    },
                );
            }
            let tree = write_tree(&checkout.path, &files)?;
            repositories.insert(
                checkout.repository.clone(),
                SnapshotRepository {
                    base_commit: checkout.base_commit.clone(),
                    tree,
                    files,
                },
            );
        }
        ensure!(
            checkpoint::capture(&task.checkouts)? == before,
            "workspace drift during snapshot"
        );
        let package = SnapshotPackage {
            schema_version: 1,
            workspace_id: workspace_id.into(),
            task_id: task_id.into(),
            feature_id: feature_id.into(),
            stage_id: stage_id.into(),
            snapshot_id: snapshot_id.into(),
            checkpoint: before,
            repositories,
        };
        package.validate()?;
        for checkout in &task.checkouts {
            retain_ref(
                &checkout.path,
                &snapshot_ref(&package),
                &package.repositories[&checkout.repository].tree,
            )?;
        }
        store::write(&path, &package)?;
        package.receipt()
    }

    pub fn snapshot_chunk(
        &self,
        workspace_id: &str,
        task_id: &str,
        agent: &str,
        snapshot_id: &str,
        expected_digest: &str,
        offset: u64,
    ) -> Result<SnapshotChunk> {
        let package =
            self.load_snapshot(workspace_id, task_id, agent, snapshot_id, expected_digest)?;
        let bytes = serde_json::to_vec(&package)?;
        let offset = usize::try_from(offset)?;
        ensure!(offset <= bytes.len(), "snapshot offset outside package");
        let end = bytes.len().min(offset.saturating_add(SNAPSHOT_CHUNK_BYTES));
        Ok(SnapshotChunk {
            package_digest: expected_digest.into(),
            total_bytes: bytes.len() as u64,
            offset: offset as u64,
            bytes: bytes[offset..end].to_vec(),
        })
    }

    fn load_snapshot(
        &self,
        workspace_id: &str,
        task_id: &str,
        agent: &str,
        snapshot_id: &str,
        expected_digest: &str,
    ) -> Result<SnapshotPackage> {
        git::valid_id(snapshot_id)?;
        let workspace = self.workspace(workspace_id, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "snapshot review permission missing"
        );
        self.load_task(&workspace, task_id)?;
        let package: SnapshotPackage = store::read(
            &self
                .task_path(workspace_id, task_id)?
                .join("snapshots")
                .join(format!("{snapshot_id}.json")),
        )?;
        package.validate()?;
        ensure!(
            package.workspace_id == workspace_id
                && package.task_id == task_id
                && package.snapshot_id == snapshot_id
                && package.receipt()?.package_digest == expected_digest,
            "snapshot reference mismatch"
        );
        Ok(package)
    }

    pub fn restore_snapshot(
        &self,
        workspace_id: &str,
        task_id: &str,
        agent: &str,
        bytes: &[u8],
        expected_digest: &str,
    ) -> Result<SnapshotReceipt> {
        ensure!(
            bytes.len() <= MAX_PACKAGE && checkpoint::digest(bytes) == expected_digest,
            "snapshot recovery digest/size mismatch"
        );
        let package: SnapshotPackage = serde_json::from_slice(bytes)?;
        package.validate()?;
        ensure!(
            package.workspace_id == workspace_id && package.task_id == task_id,
            "snapshot recovery task mismatch"
        );
        let workspace = self.workspace(workspace_id, agent)?;
        ensure!(
            workspace.operations.contains(&Operation::Review),
            "snapshot review permission missing"
        );
        let root = self.task_path(workspace_id, task_id)?;
        let _lock = store::lock(&root.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        ensure!(
            task.checkouts.len() == package.repositories.len(),
            "snapshot recovery membership mismatch"
        );
        let path = root
            .join("snapshots")
            .join(format!("{}.json", package.snapshot_id));
        if path.exists() {
            let previous: SnapshotPackage = store::read(&path)?;
            ensure!(
                previous == package,
                "snapshot recovery conflicts with retained record"
            );
        }
        for checkout in &task.checkouts {
            checkpoint::validate_checkout(checkout)?;
            let repo = package
                .repositories
                .get(&checkout.repository)
                .context("snapshot recovery repository mismatch")?;
            ensure!(
                repo.base_commit == checkout.base_commit
                    && write_tree(&checkout.path, &repo.files)? == repo.tree,
                "snapshot recovery tree mismatch"
            );
        }
        for checkout in &task.checkouts {
            retain_ref(
                &checkout.path,
                &snapshot_ref(&package),
                &package.repositories[&checkout.repository].tree,
            )?;
        }
        store::directory(&root.join("snapshots"))?;
        store::write(&path, &package)?;
        package.receipt()
    }

    pub fn snapshot_delta(
        &self,
        workspace_id: &str,
        task_id: &str,
        agent: &str,
        before: &SnapshotReceipt,
        after: &SnapshotReceipt,
    ) -> Result<BTreeMap<String, Vec<u8>>> {
        let first = self.load_snapshot(
            workspace_id,
            task_id,
            agent,
            &before.snapshot_id,
            &before.package_digest,
        )?;
        let last = self.load_snapshot(
            workspace_id,
            task_id,
            agent,
            &after.snapshot_id,
            &after.package_digest,
        )?;
        ensure!(
            first.feature_id == last.feature_id && first.stage_id == last.stage_id,
            "delta scope changed"
        );
        let workspace = self.workspace(workspace_id, agent)?;
        let _lock = store::lock(&self.task_path(workspace_id, task_id)?.join("task.lock"))?;
        let task = self.load_task(&workspace, task_id)?;
        let mut output = BTreeMap::new();
        for checkout in &task.checkouts {
            let a = first
                .repositories
                .get(&checkout.repository)
                .context("delta repository missing")?;
            let b = last
                .repositories
                .get(&checkout.repository)
                .context("delta repository missing")?;
            ensure!(a.base_commit == b.base_commit, "delta base changed");
            ensure!(
                write_tree(&checkout.path, &a.files)? == a.tree
                    && write_tree(&checkout.path, &b.files)? == b.tree,
                "delta tree corrupt"
            );
            output.insert(
                checkout.repository.clone(),
                git::run(
                    &checkout.path,
                    &[
                        "diff",
                        "--no-ext-diff",
                        "--no-textconv",
                        "--binary",
                        "--full-index",
                        &a.tree,
                        &b.tree,
                        "--",
                    ],
                )?,
            );
        }
        Ok(output)
    }
}

fn snapshot_ref(package: &SnapshotPackage) -> String {
    format!(
        "refs/light-workflow/{}/snapshots/{}",
        package.feature_id, package.snapshot_id
    )
}
fn retain_ref(root: &Path, reference: &str, tree: &str) -> Result<()> {
    // A previous attempt may have pinned the tree before the package rename.
    // Never overwrite a different retained tree, including on recovery.
    if git::run(root, &["update-ref", reference, tree, ""]).is_err() {
        let existing = git::run(root, &["rev-parse", "--verify", reference])?;
        ensure!(
            std::str::from_utf8(&existing)?.trim() == tree,
            "snapshot ref conflicts with retained tree"
        );
    }
    Ok(())
}
fn write_tree(root: &Path, files: &BTreeMap<String, SnapshotFile>) -> Result<String> {
    let temporary = tempfile::tempdir()?;
    let index = temporary.path().join("index");
    let mut entries = Vec::new();
    for (path, file) in files {
        checkpoint::safe_file(root, path)?;
        let oid = git_input(
            root,
            None,
            &["hash-object", "--no-filters", "-w", "--stdin"],
            &file.bytes,
        )?;
        let oid = std::str::from_utf8(&oid)?.trim();
        entries.extend_from_slice(
            format!(
                "{} {oid}\t{path}\0",
                if file.executable { "100755" } else { "100644" }
            )
            .as_bytes(),
        );
    }
    git_input(root, Some(&index), &["read-tree", "--empty"], &[])?;
    git_input(
        root,
        Some(&index),
        &["update-index", "-z", "--index-info"],
        &entries,
    )?;
    Ok(
        String::from_utf8(git_input(root, Some(&index), &["write-tree"], &[])?)?
            .trim()
            .into(),
    )
}
fn git_input(root: &Path, index: Option<&Path>, args: &[&str], bytes: &[u8]) -> Result<Vec<u8>> {
    let mut cmd = git::command(root);
    if let Some(index) = index {
        cmd.env("GIT_INDEX_FILE", index);
    }
    let mut child = cmd
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    child.stdin.take().context("Git stdin")?.write_all(bytes)?;
    let output = child.wait_with_output()?;
    ensure!(output.status.success(), "snapshot Git operation failed");
    Ok(output.stdout)
}
