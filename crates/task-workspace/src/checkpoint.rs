use crate::{Checkout, Checkpoint, FileEntry, RepositoryCheckpoint, git};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Component, Path},
};

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub(crate) fn safe_file(root: &Path, relative: &str) -> Result<std::path::PathBuf> {
    let path = Path::new(relative);
    if relative.is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)) || c.as_os_str() == ".git")
    {
        bail!("invalid repository-relative path");
    }
    let mut current = root.to_path_buf();
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        bail!("worktree root cannot be a symlink");
    }
    for part in path.components() {
        current.push(part);
        if let Ok(metadata) = fs::symlink_metadata(&current)
            && metadata.file_type().is_symlink()
        {
            bail!("symlink entries are not supported in workspace checkpoints");
        }
    }
    Ok(current)
}

pub(crate) fn capture(checkouts: &[Checkout]) -> Result<Checkpoint> {
    let mut repositories = Vec::new();
    for checkout in checkouts {
        let root = &checkout.path;
        validate_checkout(checkout)?;
        if git::text(root, &["symbolic-ref", "--short", "HEAD"])? != checkout.branch {
            bail!("task branch changed outside workspace manager");
        }
        let head = git::text(root, &["rev-parse", "HEAD"])?;
        let index = git::run(root, &["ls-files", "--stage", "-z"])?;
        if index
            .split(|b| *b == 0)
            .any(|entry| entry.starts_with(b"160000 "))
        {
            bail!("submodules require separate workspace registration and are not supported yet");
        }
        let names = git::run(
            root,
            &[
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ],
        )?;
        let names: BTreeSet<&str> = std::str::from_utf8(&names)?
            .split('\0')
            .filter(|n| !n.is_empty())
            .collect();
        let mut files = Vec::new();
        for name in names {
            let path = safe_file(root, name)?;
            let metadata = match fs::symlink_metadata(&path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if !metadata.is_file() {
                bail!("checkpoint entry is not a regular file");
            }
            files.push(FileEntry {
                path: name.into(),
                digest: digest(&fs::read(path)?),
                executable: metadata.permissions().mode() & 0o111 != 0,
            });
        }
        repositories.push(RepositoryCheckpoint {
            repository: checkout.repository.clone(),
            head,
            index_digest: digest(&index),
            files,
            status_digest: digest(&git::run(
                root,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            )?),
        });
    }
    Ok(Checkpoint {
        digest: digest(&serde_json::to_vec(&repositories)?),
        repositories,
    })
}

pub(crate) fn validate_checkout(checkout: &Checkout) -> Result<()> {
    let root = &checkout.path;
    if fs::symlink_metadata(root)?.file_type().is_symlink() {
        bail!("worktree root cannot be a symlink");
    }
    let link = root.join(".git");
    if !fs::symlink_metadata(&link)?.is_file() {
        bail!("invalid worktree Git link");
    }
    let text = fs::read_to_string(&link)?;
    let admin = Path::new(
        text.trim()
            .strip_prefix("gitdir: ")
            .ok_or_else(|| anyhow::anyhow!("invalid worktree Git link"))?,
    );
    let workspace = root
        .ancestors()
        .nth(4)
        .ok_or_else(|| anyhow::anyhow!("invalid managed worktree path"))?;
    let expected_parent = workspace
        .join("repositories")
        .join(&checkout.repository)
        .join("worktrees");
    if admin.parent() != Some(expected_parent.as_path())
        || admin.canonicalize()? != admin
        || fs::read_to_string(admin.join("gitdir"))?.trim()
            != link
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("invalid worktree path"))?
    {
        bail!("worktree Git link is outside its managed repository");
    }
    Ok(())
}
