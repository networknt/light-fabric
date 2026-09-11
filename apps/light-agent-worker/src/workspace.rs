//! Task files are reachable only through the fixed tool session. The native
//! adapter runs in a separate filesystem namespace containing no host workspace.
use super::emit;
use agent_core::ResultClass;
use agent_runtime_protocol::{MAX_FRAME_BYTES, RuntimeEventPayload, RuntimeIdentity};
use anyhow::{Context, Result, bail, ensure};
use coding_agent_runtime::{
    CodingAdapterContract, CodingAdapterQualification, CodingAuthenticationProfile,
};
use serde_json::{Value, json};
use std::{path::Path, process::Stdio};
use task_workspace::{JobExecution, RunnerWorkspaceConfig, WorkspaceStore};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::Command,
};
use workspace_execution_protocol::{WorkspaceExecutionSpec, WorkspaceIntent, standalone_intents};

pub(super) async fn run<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
    mut cancel: tokio::sync::watch::Receiver<Option<String>>,
    deadline: Option<tokio::time::Instant>,
) -> Result<()> {
    let spec: WorkspaceExecutionSpec = serde_json::from_value(
        input
            .get("workspaceSpec")
            .context("workspaceSpec missing")?
            .clone(),
    )?;
    let contract: CodingAdapterContract = serde_json::from_value(
        input
            .get("adapterContract")
            .context("adapterContract missing")?
            .clone(),
    )?;
    let qualification: CodingAdapterQualification = serde_json::from_value(
        input
            .get("adapterQualification")
            .context("adapterQualification missing")?
            .clone(),
    )?;
    super::codex_app_server::validate_contract(&contract, &qualification).await?;
    let executable =
        std::env::var_os("LIGHT_CODEX_EXECUTABLE").context("pinned Codex executable is missing")?;
    let helper = Path::new(&executable).with_file_name("codex-code-mode-host");
    ensure!(
        workspace_execution_protocol::sha256(&tokio::fs::read(&helper).await?)
            == "sha256:3e85d67471825f73d02ff5f7e047ca1f6ca8caa3f59e4c6e8d9ca6ca7302cb45",
        "Codex code-mode host does not match the pinned 0.153.4 package"
    );
    let config_path = std::env::var_os("LIGHT_WORKSPACE_CONFIG")
        .context("runner workspace configuration is missing")?;
    let config_path = Path::new(&config_path);
    let config = RunnerWorkspaceConfig::load(config_path)?;
    config.authorize(&spec)?;
    let store = WorkspaceStore::open(&config.store)?;
    let context = spec.context();
    emit(
        writer,
        identity,
        sequence,
        RuntimeEventPayload::Progress {
            message: "Preparing workspace task".into(),
        },
    )
    .await?;
    let job = store.admit_job(
        &spec.request,
        &context,
        &spec.binding,
        &standalone_intents(),
    )?;
    let job = store.provision_job(
        &spec.request.workspace_id,
        &job.job_id,
        &context,
        &spec.binding,
        &standalone_intents(),
    )?;
    let mut execution = match store.claim_job_execution(&job)? {
        JobExecution::Cached(output) => {
            return emit(
                writer,
                identity,
                sequence,
                RuntimeEventPayload::Terminal {
                    class: ResultClass::Success,
                    output: Some(output),
                    error: None,
                },
            )
            .await;
        }
        JobExecution::Active(execution) => execution,
    };
    ensure!(
        cancel.borrow().is_none(),
        "workspace task cancelled before execution"
    );
    let mut tools = None;
    let sandbox = tempfile::tempdir()?;
    std::fs::create_dir(sandbox.path().join("home"))?;
    std::fs::create_dir(sandbox.path().join("work"))?;
    let home = std::env::var_os("LIGHT_CODEX_HOME").context("personal Codex login is missing")?;
    std::fs::copy(
        Path::new(&home).join("auth.json"),
        sandbox.path().join("home/auth.json"),
    )?;
    // Inherit authentication only, never the owner's MCP servers, plugins,
    // hooks, instructions, memories or unrestricted native tool configuration.
    std::fs::write(sandbox.path().join("home/config.toml"), native_config())?;
    let executable =
        std::env::var_os("LIGHT_CODEX_EXECUTABLE").context("pinned Codex executable is missing")?;
    let mut child = sandbox_command(sandbox.path(), Path::new(&executable))
        .spawn()
        .context("spawn isolated workspace Codex")?;
    let mut stdin = child.stdin.take().context("Codex stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("Codex stdout")?);
    let stderr = child.stderr.take().context("Codex stderr")?;
    let drain = tokio::spawn(async move {
        let mut stderr = stderr.take(1024 * 1024);
        tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await
    });
    let work = async {
        rpc(&mut stdin,1,"initialize",json!({"clientInfo":{"name":"light-workspace-worker","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}})).await?;
        response(&mut stdout, 1).await?;
        send(&mut stdin, json!({"method":"initialized"})).await?;
        rpc(&mut stdin, 2, "account/read", json!({"refreshToken":false})).await?;
        let authentication = super::codex_app_server::authentication_evidence(
            CodingAuthenticationProfile::PersonalSubscription,
            &response(&mut stdout, 2).await?,
            None,
        )?;
        rpc(&mut stdin, 3, "thread/start", thread_params()).await?;
        let thread = response(&mut stdout, 3).await?;
        let thread_id = thread
            .pointer("/result/thread/id")
            .and_then(Value::as_str)
            .context("thread ID missing")?
            .to_owned();
        tools = Some(store.begin_tool_session(
            &spec.request.workspace_id,
            &job.task_id,
            &context.agent_id,
            spec.request.intent == WorkspaceIntent::Implement,
            spec.request.expected_checkpoint_digest.as_deref(),
        )?);
        execution.mark_started()?;
        rpc(&mut stdin,4,"turn/start",json!({"threadId":thread_id,"input":[{"type":"text","text":spec.request.instruction}],
            "cwd":"/session/work","approvalPolicy":"never","sandboxPolicy":{"type":"readOnly","networkAccess":false}})).await?;
        let turn = response(&mut stdout, 4).await?;
        let turn_id = turn
            .pointer("/result/turn/id")
            .and_then(Value::as_str)
            .context("turn ID missing")?
            .to_owned();
        let mut answer = None;
        let mut calls = 0usize;
        loop {
            let value = frame(&mut stdout)
                .await?
                .context("Codex closed before task completion")?;
            if let (Some(id), Some(method)) =
                (value.get("id"), value.get("method").and_then(Value::as_str))
            {
                if method != "item/tool/call" {
                    send(&mut stdin,json!({"id":id,"error":{"code":-32601,"message":"Interactive authority is unavailable"}})).await?;
                    continue;
                }
                let params = &value["params"];
                ensure!(
                    params["threadId"] == thread_id
                        && params["turnId"] == turn_id
                        && params["tool"] == "task_workspace",
                    "tool call does not belong to this task turn"
                );
                calls += 1;
                ensure!(calls <= 200, "workspace tool call budget exceeded");
                // A changed or removed host binding revokes the next tool call.
                let current = RunnerWorkspaceConfig::load(config_path)?;
                current.authorize(&spec)?;
                ensure!(
                    current.store == config.store,
                    "workspace store changed during execution"
                );
                let result = tools
                    .as_mut()
                    .context("workspace tool session missing")?
                    .call_tool(params["arguments"].clone());
                let success = result.is_ok();
                let text = match result { Ok(value) => value.to_string(), Err(_) => "Tool request rejected: check operation, repository, path, file digest and task access.".into() };
                send(&mut stdin,json!({"id":id,"result":{"success":success,"contentItems":[{"type":"inputText","text":text}]}})).await?;
                emit(
                    writer,
                    identity,
                    sequence,
                    RuntimeEventPayload::Progress {
                        message: format!("Workspace tool call {calls} completed"),
                    },
                )
                .await?;
                continue;
            }
            if value.pointer("/params/threadId").and_then(Value::as_str) == Some(&thread_id)
                && value.pointer("/params/turnId").and_then(Value::as_str) == Some(&turn_id)
                && value["method"] == "item/completed"
                && value.pointer("/params/item/type").and_then(Value::as_str)
                    == Some("agentMessage")
            {
                answer = value
                    .pointer("/params/item/text")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            if value["method"] == "turn/completed"
                && value.pointer("/params/turn/id").and_then(Value::as_str) == Some(&turn_id)
            {
                ensure!(
                    value.pointer("/params/turn/status").and_then(Value::as_str)
                        == Some("completed"),
                    "Codex workspace turn did not complete"
                );
                let answer = answer.context("Codex completed without an explanation")?;
                ensure!(
                    calls > 0,
                    "Codex completed without accessing the task tools"
                );
                ensure!(
                    answer.len() <= 64 * 1024,
                    "workspace explanation exceeds limit"
                );
                return Ok::<_, anyhow::Error>((answer, authentication));
            }
            if value["method"] == "error" {
                if value.pointer("/params/willRetry").and_then(Value::as_bool) == Some(true) {
                    let code = value
                        .pointer("/params/error/codexErrorInfo")
                        .unwrap_or(&Value::Null);
                    emit(
                        writer,
                        identity,
                        sequence,
                        RuntimeEventPayload::Progress {
                            message: format!("Codex retrying transient error ({code})"),
                        },
                    )
                    .await?;
                    continue;
                }
                let code = value
                    .pointer("/params/error/codexErrorInfo")
                    .unwrap_or(&Value::Null);
                bail!("Codex workspace turn failed ({code})");
            }
        }
    };
    let deadline = deadline
        .unwrap_or_else(|| tokio::time::Instant::now() + std::time::Duration::from_secs(300));
    let result = tokio::select! {
        result = work => result,
        _ = tokio::time::sleep_until(deadline) => Err(anyhow::anyhow!("workspace execution deadline exceeded")),
        _ = cancel.changed() => Err(anyhow::anyhow!("workspace execution cancelled")),
    };
    // Stop the isolated adapter before releasing the manager lease.
    let _ = child.kill().await;
    let _ = child.wait().await;
    drain.abort();
    if !execution.has_started()
        && let Some(tools) = tools.take()
    {
        tools.finish()?;
    }
    let (answer, authentication) = result?;
    RunnerWorkspaceConfig::load(config_path)?.authorize(&spec)?;
    let checkpoint = tools.context("workspace tool session missing")?.finish()?;
    let output = json!({"finalMessage":answer,"authentication":authentication,"workspace":{
        "workspaceId":spec.request.workspace_id,"taskId":job.task_id,"jobId":job.job_id,
        "checkpointDigest":checkpoint.digest,"intent":spec.request.intent}});
    execution.finish(output.clone())?;
    emit(
        writer,
        identity,
        sequence,
        RuntimeEventPayload::Terminal {
            class: ResultClass::Success,
            output: Some(output),
            error: None,
        },
    )
    .await
}

fn native_config() -> &'static str {
    "approval_policy = \"never\"\nsandbox_mode = \"read-only\"\nweb_search = \"disabled\"\n[features]\nshell_tool = false\nunified_exec = false\nview_image = false\napps = false\nplugins = false\nhooks = false\nmemories = false\nmulti_agent = false\nmulti_agent_v2 = false\ncode_mode = false\ncode_mode_host = true\nbrowser_use = false\ncomputer_use = false\nimage_generation = false\n"
}
fn thread_params() -> Value {
    json!({"cwd":"/session/work","approvalPolicy":"never","sandbox":"read-only","ephemeral":true,
        "baseInstructions":"You work on one managed multi-repository task. Use task_workspace for all repository access. Inspect requests are read-only. Implement requests may edit files with digest preconditions. Do not claim to run tests, commit, push, or open PRs: those tools are not available. Explain the result and any validation that remains.",
        "dynamicTools":[task_workspace::workspace_tool_definition()]})
}
fn sandbox_command(root: &Path, executable: &Path) -> Command {
    let mut command = Command::new("/usr/bin/bwrap");
    command.env_clear().env("PATH", "/usr/bin:/bin").args([
        "--die-with-parent",
        "--unshare-all",
        "--share-net",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/bin",
        "/bin",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib64",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
    ]);
    for path in [
        "/etc/ssl",
        "/etc/pki",
        "/etc/resolv.conf",
        "/etc/nsswitch.conf",
        "/etc/hosts",
    ] {
        if Path::new(path).exists() {
            command.args(["--ro-bind", path, path]);
        }
    }
    command
        .arg("--bind")
        .arg(root)
        .arg("/session")
        .arg("--ro-bind")
        .arg(executable)
        .arg("/opt/codex")
        .arg("--ro-bind")
        .arg(executable.with_file_name("codex-code-mode-host"))
        .arg("/opt/codex-code-mode-host")
        .args([
            "--chdir",
            "/session/work",
            "--setenv",
            "HOME",
            "/session/home",
            "--setenv",
            "CODEX_HOME",
            "/session/home",
            "--",
            "/opt/codex",
            "app-server",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}
async fn send<W: AsyncWrite + Unpin>(out: &mut W, value: Value) -> Result<()> {
    out.write_all(&serde_json::to_vec(&value)?).await?;
    out.write_all(b"\n").await?;
    out.flush().await?;
    Ok(())
}
async fn rpc<W: AsyncWrite + Unpin>(
    out: &mut W,
    id: u64,
    method: &str,
    params: Value,
) -> Result<()> {
    send(out, json!({"id":id,"method":method,"params":params})).await
}
async fn frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() <= MAX_FRAME_BYTES && bytes.last() == Some(&b'\n'),
        "oversized Codex frame"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}
async fn response<R: AsyncBufRead + Unpin>(reader: &mut R, id: u64) -> Result<Value> {
    while let Some(value) = frame(reader).await? {
        if value["id"].as_u64() == Some(id) {
            ensure!(
                value.get("error").is_none(),
                "Codex initialization rejected"
            );
            return Ok(value);
        }
    }
    bail!("Codex closed during initialization")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_tools_are_disabled_and_dynamic_tool_has_no_authority_arguments() {
        let config = native_config();
        for feature in [
            "shell_tool",
            "unified_exec",
            "apps",
            "plugins",
            "hooks",
            "multi_agent",
            "browser_use",
            "computer_use",
        ] {
            assert!(config.contains(&format!("{feature} = false")));
        }
        let params = thread_params();
        assert_eq!(params["ephemeral"], true);
        assert_eq!(params["sandbox"], "read-only");
        let properties = &params["dynamicTools"][0]["inputSchema"]["properties"];
        for field in ["agent", "task", "workspace", "store", "command"] {
            assert!(properties.get(field).is_none());
        }
    }
    #[tokio::test]
    #[ignore = "requires host bubblewrap user namespaces"]
    async fn native_namespace_hides_host_files() {
        use std::os::unix::fs::PermissionsExt;
        let host = tempfile::tempdir().unwrap();
        let secret = host.path().join("private-marker");
        std::fs::write(&secret, "must not be visible").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("work")).unwrap();
        std::fs::create_dir(root.path().join("home")).unwrap();
        let script = host.path().join("codex");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntest ! -e '{}' && test ! -e /home/steve && test -d /session/work\n",
                secret.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(host.path().join("codex-code-mode-host"), "unused").unwrap();
        let output = sandbox_command(root.path(), &script)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
