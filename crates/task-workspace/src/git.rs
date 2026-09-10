use anyhow::{Context, Result, bail};
use std::{path::Path, process::Command};

pub(crate) fn command(path: &Path) -> Command {
    let mut cmd = Command::new("git");
    // Host-owned credentials remain available to trusted administrative actions.
    // No repository hooks, external diff helpers, or inherited Git overrides.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("diff.external=")
        .arg("-c")
        .arg("protocol.ext.allow=never")
        .arg("-C")
        .arg(path)
        .env("GIT_TERMINAL_PROMPT", "0");
    cmd
}

pub(crate) fn run(path: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = command(path).args(args).output().context("start Git")?;
    if !output.status.success() {
        // Do not echo remote URLs or credential-bearing command output.
        bail!(
            "Git {} failed (exit {:?})",
            args.first().unwrap_or(&"command"),
            output.status.code()
        );
    }
    Ok(output.stdout)
}

pub(crate) fn text(path: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(run(path, args)?)?.trim().into())
}

pub(crate) fn valid_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 100
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("invalid workspace, task, or repository identifier");
    }
    Ok(())
}

pub(crate) fn valid_branch(branch: &str) -> Result<()> {
    if branch.starts_with('-') || branch.starts_with('/') || branch.contains("..") {
        bail!("invalid branch");
    }
    let status = Command::new("git")
        .args(["check-ref-format", &format!("refs/heads/{branch}")])
        .status()?;
    if !status.success() {
        bail!("invalid branch");
    }
    Ok(())
}
