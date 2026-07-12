use anyhow::{Context, Result, bail};
use coding_agent_runtime::{
    CodingFixtureOutput, CodingTurnSpec, PI_RPC_ADAPTER_ID, PI_RPC_ADAPTER_VERSION,
    PI_RPC_IMPLEMENTATION_VERSION, validate_patch,
};
use execution_security::ProtectedPathPolicy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

const MAXIMUM_RPC_FRAME_BYTES: usize = 1024 * 1024;
const MAXIMUM_STDERR_BYTES: usize = 1024 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse()?;
    let spec = CodingTurnSpec::decode_argument(&arguments.request_base64)?;
    verify_regular_digest(&arguments.repository, &spec.repository_digest)?;
    verify_executable_digest(&arguments.pi, &arguments.pi_digest)?;
    let workspace = PathBuf::from(&spec.workspace_root);
    materialize_repository(&arguments.repository, &workspace, &spec.base_revision)?;
    run_pi(&arguments, &spec.prompt, &spec.allowed_tools, &workspace).await?;
    let output = export_patch(spec, &workspace)?;
    println!("{}", serde_json::to_string(&output)?);
    Ok(())
}

struct Arguments {
    repository: PathBuf,
    request_base64: String,
    pi: PathBuf,
    pi_digest: String,
    provider: String,
    model: String,
}

impl Arguments {
    fn parse() -> Result<Self> {
        let mut values = std::env::args().skip(1);
        let repository = PathBuf::from(option(&mut values, "--repository")?);
        let request_base64 = option(&mut values, "--request-base64")?;
        let pi = PathBuf::from(option(&mut values, "--pi")?);
        let pi_digest = option(&mut values, "--pi-digest")?;
        let provider = option(&mut values, "--provider")?;
        let model = option(&mut values, "--model")?;
        if values.next().is_some()
            || !pi.is_absolute()
            || provider.is_empty()
            || model.is_empty()
            || provider.starts_with('-')
            || model.starts_with('-')
        {
            bail!("invalid Pi RPC adapter arguments")
        }
        Ok(Self {
            repository,
            request_base64,
            pi,
            pi_digest,
            provider,
            model,
        })
    }
}

async fn run_pi(
    arguments: &Arguments,
    prompt: &str,
    allowed_tools: &BTreeSet<String>,
    workspace: &Path,
) -> Result<()> {
    let pi_tools = pi_tools(allowed_tools)?;
    let version = Command::new(&arguments.pi)
        .arg("--version")
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .output()
        .await?;
    let reported = String::from_utf8_lossy(&version.stdout);
    if !version.status.success()
        || !reported
            .split_whitespace()
            .any(|v| v == PI_RPC_IMPLEMENTATION_VERSION)
    {
        bail!("Pi version differs from the admitted adapter version")
    }
    let mut child = Command::new(&arguments.pi);
    child
        .args([
            "--mode",
            "rpc",
            "--no-session",
            "--provider",
            &arguments.provider,
            "--model",
            &arguments.model,
            "--tools",
            &pi_tools,
        ])
        .current_dir(workspace)
        .env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/tmp/pi-home")
        .env("PI_CODING_AGENT_DIR", "/opt/light-pi/config")
        .env("PI_OFFLINE", "1")
        .env("PI_SKIP_VERSION_CHECK", "1")
        .env("PI_TELEMETRY", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    child.process_group(0);
    let mut child = child.spawn().context("spawn pinned Pi RPC process")?;
    let mut stdin = child.stdin.take().context("Pi stdin unavailable")?;
    let stdout = child.stdout.take().context("Pi stdout unavailable")?;
    let stderr = child.stderr.take().context("Pi stderr unavailable")?;
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr
            .take((MAXIMUM_STDERR_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::Ok(bytes)
    });
    let request_id = "light-pi-turn-1";
    let mut command = serde_json::to_vec(&json!({
        "id": request_id,
        "type": "prompt",
        "message": prompt
    }))?;
    command.push(b'\n');
    stdin.write_all(&command).await?;
    stdin.flush().await?;
    let mut lines = BufReader::new(stdout).lines();
    let mut accepted = false;
    let mut settled = false;
    while let Some(line) = read_bounded_line(&mut lines).await? {
        let event: Value = serde_json::from_str(&line).context("Pi emitted non-JSON RPC output")?;
        let kind = event
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response" if event.get("id").and_then(Value::as_str) == Some(request_id) => {
                if event.get("command").and_then(Value::as_str) != Some("prompt")
                    || event.get("success").and_then(Value::as_bool) != Some(true)
                {
                    bail!("Pi rejected the admitted prompt")
                }
                accepted = true;
            }
            "agent_settled" => {
                settled = true;
                break;
            }
            "extension_ui_request" => bail!("Pi requested interactive authority in RPC mode"),
            "extension_error" => bail!("Pi extension failed"),
            _ => {}
        }
    }
    drop(stdin);
    let status = child.wait().await?;
    let stderr = stderr_task.await??;
    if stderr.len() > MAXIMUM_STDERR_BYTES {
        bail!("Pi stderr exceeded its admitted limit")
    }
    if !status.success() || !accepted || !settled {
        bail!(
            "Pi RPC did not settle successfully: {}",
            String::from_utf8_lossy(&stderr)
        )
    }
    Ok(())
}

fn pi_tools(allowed: &BTreeSet<String>) -> Result<String> {
    let mut tools = BTreeSet::new();
    for tool in allowed {
        match tool.as_str() {
            "fs.read" => {
                tools.insert("read");
            }
            "fs.write" => {
                tools.insert("write");
                tools.insert("edit");
            }
            _ => bail!("coding specification requests an unsupported Pi tool"),
        }
    }
    if tools.is_empty() {
        bail!("coding specification grants no Pi tools")
    }
    Ok(tools.into_iter().collect::<Vec<_>>().join(","))
}

async fn read_bounded_line<R: tokio::io::AsyncRead + Unpin>(
    lines: &mut tokio::io::Lines<BufReader<R>>,
) -> Result<Option<String>> {
    let line = lines.next_line().await?;
    if line
        .as_ref()
        .is_some_and(|line| line.len() > MAXIMUM_RPC_FRAME_BYTES)
    {
        bail!("Pi RPC frame exceeded its admitted limit")
    }
    Ok(line)
}

fn materialize_repository(repository: &Path, workspace: &Path, revision: &str) -> Result<()> {
    if workspace.exists() {
        bail!("coding workspace already exists")
    }
    fs::create_dir_all(workspace.parent().context("workspace has no parent")?)?;
    git(
        None,
        &[
            "clone",
            "--no-checkout",
            "--no-local",
            path(repository)?,
            path(workspace)?,
        ],
    )?;
    git(
        Some(workspace),
        &["checkout", "--detach", "--force", revision],
    )?;
    let actual = git_output(Some(workspace), &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if actual.trim() != revision {
        bail!("repository base revision mismatch")
    }
    Ok(())
}

fn export_patch(spec: CodingTurnSpec, workspace: &Path) -> Result<CodingFixtureOutput> {
    git(Some(workspace), &["diff", "--check"])?;
    let patch = git_output(
        Some(workspace),
        &[
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--binary",
            "--src-prefix=a/",
            "--dst-prefix=b/",
        ],
    )?;
    let names = git_bytes(Some(workspace), &["diff", "--name-only", "-z"])?;
    let changed_paths = names
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8(path.to_vec()).context("non-UTF8 changed path"))
        .collect::<Result<Vec<_>>>()?;
    let validated = validate_patch(
        &spec,
        &ProtectedPathPolicy::default_deny(),
        &spec.base_revision,
        &patch,
        &changed_paths,
    )?;
    Ok(CodingFixtureOutput {
        adapter_id: PI_RPC_ADAPTER_ID.into(),
        adapter_version: PI_RPC_ADAPTER_VERSION.into(),
        repository_digest: spec.repository_digest,
        base_revision: validated.base_revision,
        patch: validated.patch,
        changed_paths: validated.changed_paths.into_iter().collect(),
    })
}

fn verify_regular_digest(path: &Path, expected: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("pinned input must be a regular non-symlink file")
    }
    let actual = format!("sha256:{:x}", Sha256::digest(fs::read(path)?));
    if actual != expected {
        bail!("pinned input digest mismatch")
    }
    Ok(())
}

fn verify_executable_digest(path: &Path, expected: &str) -> Result<()> {
    let resolved = fs::canonicalize(path)?;
    let metadata = fs::metadata(&resolved)?;
    if !metadata.is_file() {
        bail!("pinned executable must resolve to a regular file")
    }
    let actual = format!("sha256:{:x}", Sha256::digest(fs::read(resolved)?));
    if actual != expected {
        bail!("pinned executable digest mismatch")
    }
    Ok(())
}

fn git(workspace: Option<&Path>, arguments: &[&str]) -> Result<()> {
    let output = git_command(workspace, arguments).output()?;
    if !output.status.success() {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    Ok(())
}
fn git_output(workspace: Option<&Path>, arguments: &[&str]) -> Result<String> {
    String::from_utf8(git_bytes(workspace, arguments)?).context("git returned non-UTF8 output")
}
fn git_bytes(workspace: Option<&Path>, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = git_command(workspace, arguments).output()?;
    if !output.status.success() {
        bail!(
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    }
    Ok(output.stdout)
}
fn git_command(workspace: Option<&Path>, arguments: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new("/usr/bin/git");
    command.env_clear().env("PATH", "/usr/bin:/bin");
    if let Some(workspace) = workspace {
        command.arg("-C").arg(workspace);
    }
    command.args(arguments);
    command
}
fn path(path: &Path) -> Result<&str> {
    path.to_str().context("path is not UTF-8")
}
fn option(values: &mut impl Iterator<Item = String>, expected: &str) -> Result<String> {
    if values.next().as_deref() != Some(expected) {
        bail!("expected {expected}")
    }
    values
        .next()
        .with_context(|| format!("{expected} requires a value"))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_pi(directory: &Path, events: &str) -> (PathBuf, String) {
        let path = directory.join("pi");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'pi {PI_RPC_IMPLEMENTATION_VERSION}'; exit 0; fi\nIFS= read -r request\nprintf '%b\\n' '{events}'\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
        let digest = format!("sha256:{:x}", Sha256::digest(fs::read(&path).unwrap()));
        (path, digest)
    }

    #[tokio::test]
    async fn pinned_pi_rpc_acceptance_and_settled_event_are_required() {
        let root = tempfile::tempdir().unwrap();
        let events = r#"{"id":"light-pi-turn-1","type":"response","command":"prompt","success":true}\n{"type":"agent_settled"}"#;
        let (pi, pi_digest) = fake_pi(root.path(), events);
        let arguments = Arguments {
            repository: root.path().join("unused"),
            request_base64: String::new(),
            pi,
            pi_digest,
            provider: "brokered".into(),
            model: "approved".into(),
        };

        run_pi(
            &arguments,
            "edit fixture",
            &BTreeSet::from(["fs.read".into(), "fs.write".into()]),
            root.path(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn interactive_extension_authority_fails_closed() {
        let root = tempfile::tempdir().unwrap();
        let events = r#"{"id":"light-pi-turn-1","type":"response","command":"prompt","success":true}\n{"type":"extension_ui_request","id":"ui","method":"confirm"}"#;
        let (pi, pi_digest) = fake_pi(root.path(), events);
        let arguments = Arguments {
            repository: root.path().join("unused"),
            request_base64: String::new(),
            pi,
            pi_digest,
            provider: "brokered".into(),
            model: "approved".into(),
        };

        let error = run_pi(
            &arguments,
            "edit fixture",
            &BTreeSet::from(["fs.read".into(), "fs.write".into()]),
            root.path(),
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("interactive authority"));
    }
}
