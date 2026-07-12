use anyhow::{Context, Result, bail};
use coding_agent_runtime::{CodingFixtureOutput, CodingFixtureRequest};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> Result<()> {
    let mut arguments = std::env::args().skip(1);
    let repository = take_option(&mut arguments, "--repository")?;
    let request = take_option(&mut arguments, "--request-base64")?;
    if arguments.next().is_some() {
        bail!("unexpected coding fixture argument")
    }
    let request = CodingFixtureRequest::decode_argument(&request)?;
    let repository = PathBuf::from(repository);
    verify_repository(&repository, &request.spec.repository_digest)?;
    let workspace = PathBuf::from(&request.spec.workspace_root);
    let output = run_fixture(&repository, request, &workspace)?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

fn run_fixture(
    repository: &Path,
    request: CodingFixtureRequest,
    workspace: &Path,
) -> Result<CodingFixtureOutput> {
    if workspace.exists() {
        bail!("coding workspace already exists")
    }
    let parent = workspace.parent().context("workspace has no parent")?;
    fs::create_dir_all(parent)?;
    git(
        None,
        [
            "clone",
            "--no-checkout",
            "--no-local",
            repository.to_str().context("non-UTF8 repository path")?,
            workspace.to_str().context("non-UTF8 workspace path")?,
        ],
    )?;
    git(
        Some(&workspace),
        [
            "checkout",
            "--detach",
            "--force",
            &request.spec.base_revision,
        ],
    )?;
    let actual = git_output(Some(&workspace), ["rev-parse", "--verify", "HEAD^{commit}"])?;
    if actual.trim() != request.spec.base_revision {
        bail!("repository base revision mismatch")
    }
    edit_fixture(&workspace, &request)?;
    git(Some(&workspace), ["diff", "--check"])?;
    let patch = git_output(
        Some(&workspace),
        [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ],
    )?;
    let paths = git_bytes(Some(&workspace), ["diff", "--name-only", "-z"])?;
    let changed_paths = paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).context("non-UTF8 changed path"))
        .collect::<Result<Vec<_>>>()?;
    if patch.is_empty() || changed_paths.is_empty() {
        bail!("coding fixture produced no patch")
    }
    let output = CodingFixtureOutput {
        adapter_id: "cube-coding-fixture".into(),
        adapter_version: "1".into(),
        repository_digest: request.spec.repository_digest,
        base_revision: request.spec.base_revision,
        patch,
        changed_paths,
    };
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_agent_runtime::CodingTurnSpec;
    use std::collections::BTreeSet;

    #[test]
    fn immutable_bundle_produces_structured_canonical_patch() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        fs::create_dir(&source).unwrap();
        git(None, ["init", source.to_str().unwrap()]).unwrap();
        fs::write(source.join("fixture.txt"), "before\n").unwrap();
        git(Some(&source), ["add", "fixture.txt"]).unwrap();
        let status = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-C")
            .arg(&source)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "base",
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let revision = git_output(Some(&source), ["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        let bundle = root.path().join("repository.bundle");
        git(
            Some(&source),
            ["bundle", "create", bundle.to_str().unwrap(), "--all"],
        )
        .unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(fs::read(&bundle).unwrap()));
        let request = CodingFixtureRequest {
            spec: CodingTurnSpec {
                repository_digest: digest.clone(),
                base_revision: revision.clone(),
                workspace_root: "/workspace/repo".into(),
                prompt: "replace fixture text".into(),
                model_alias: "deterministic-fixture".into(),
                materialization_manifest_digest: format!("sha256:{}", "1".repeat(64)),
                writable_roots: BTreeSet::from(["/workspace/repo".into()]),
                allowed_tools: BTreeSet::from(["fs.read".into(), "fs.write".into()]),
                maximum_patch_bytes: 4096,
                maximum_changed_files: 1,
            },
            target_path: "fixture.txt".into(),
            expected_text: "before".into(),
            replacement_text: "after".into(),
        };
        let output = run_fixture(&bundle, request, &root.path().join("workspace")).unwrap();
        assert_eq!(output.repository_digest, digest);
        assert_eq!(output.base_revision, revision);
        assert_eq!(output.changed_paths, vec!["fixture.txt"]);
        assert!(output.patch.contains("-before"));
        assert!(output.patch.contains("+after"));
    }
}

fn take_option(arguments: &mut impl Iterator<Item = String>, expected: &str) -> Result<String> {
    if arguments.next().as_deref() != Some(expected) {
        bail!("expected {expected}")
    }
    arguments
        .next()
        .with_context(|| format!("{expected} requires a value"))
}

fn verify_repository(path: &Path, expected: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("repository input must be a regular non-symlink file")
    }
    let actual = format!("sha256:{:x}", Sha256::digest(fs::read(path)?));
    if actual != expected {
        bail!("repository input digest mismatch")
    }
    Ok(())
}

fn edit_fixture(workspace: &Path, request: &CodingFixtureRequest) -> Result<()> {
    let target = workspace.join(&request.target_path);
    if !target.starts_with(workspace) {
        bail!("fixture target escapes workspace")
    }
    let metadata = fs::symlink_metadata(&target)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("fixture target must be a regular non-symlink file")
    }
    let content = fs::read_to_string(&target)?;
    if content.matches(&request.expected_text).count() != 1 {
        bail!("fixture expected text must occur exactly once")
    }
    fs::write(
        &target,
        content.replacen(&request.expected_text, &request.replacement_text, 1),
    )?;
    Ok(())
}

fn git<const N: usize>(workspace: Option<&Path>, arguments: [&str; N]) -> Result<()> {
    let output = git_command(workspace, arguments).output()?;
    if !output.status.success() {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    Ok(())
}

fn git_output<const N: usize>(workspace: Option<&Path>, arguments: [&str; N]) -> Result<String> {
    String::from_utf8(git_bytes(workspace, arguments)?).context("git returned non-UTF8 output")
}

fn git_bytes<const N: usize>(workspace: Option<&Path>, arguments: [&str; N]) -> Result<Vec<u8>> {
    let output = git_command(workspace, arguments).output()?;
    if !output.status.success() {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    Ok(output.stdout)
}

fn git_command<const N: usize>(workspace: Option<&Path>, arguments: [&str; N]) -> Command {
    let mut command = Command::new("/usr/bin/git");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", "/nonexistent")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .arg("-c")
        .arg("core.hooksPath=/dev/null")
        .arg("-c")
        // The only file transport target is the runner-uploaded immutable
        // bundle supplied by the fixed argv contract. No submodules or
        // repository-controlled URLs are initialized by this fixture.
        .arg("protocol.file.allow=always")
        .arg("-c")
        .arg("diff.external=");
    if let Some(workspace) = workspace {
        command.arg("-C").arg(workspace);
    }
    command.args(arguments);
    command
}
