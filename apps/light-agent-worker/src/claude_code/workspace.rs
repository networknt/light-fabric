//! Claude uses the same runner-owned task tools as Codex through a fixed MCP bridge.
//! No checkout or workspace store is mounted into the native CLI namespace.
use super::*;
use agent_core::ResultClass;
use agent_runtime_protocol::{RuntimeEventPayload, RuntimeIdentity};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use task_workspace::{JobExecution, RunnerWorkspaceConfig, WorkspaceStore};
use tokio::{io::AsyncWrite, net::UnixListener};
use workspace_execution_protocol::{WorkspaceExecutionSpec, WorkspaceIntent, standalone_intents};

pub(crate) async fn run<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
    mut cancellation: watch::Receiver<Option<String>>,
    deadline: Option<Instant>,
) -> Result<()> {
    let spec: WorkspaceExecutionSpec = serde_json::from_value(input["workspaceSpec"].clone())?;
    let contract = serde_json::from_value(input["adapterContract"].clone())?;
    let qualification = serde_json::from_value(input["adapterQualification"].clone())?;
    coding_agent_runtime::claude::require_local_contract(&contract, &qualification)?;
    let policy: LaunchPolicy = serde_json::from_value(input["claudePolicy"].clone())?;
    let model = policy
        .resolve(spec.request.native_model.as_deref())?
        .to_owned();
    let path: PathBuf = std::env::var_os("LIGHT_WORKSPACE_CONFIG")
        .context("workspace config missing")?
        .into();
    let config = RunnerWorkspaceConfig::load(&path)?;
    config.authorize(&spec)?;
    let store = WorkspaceStore::open(&config.store)?;
    let context = spec.context();
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
            return crate::emit(
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
        JobExecution::Active(job) => job,
    };
    let mut conversation = crate::workspace_session::Conversation::open(
        &config.store,
        &spec,
        &job.task_id,
        &json!({"adapter":contract,"policy":policy,"model":model}),
    )?;
    let sandbox = tempfile::tempdir()?;
    std::fs::create_dir(sandbox.path().join("work"))?;
    std::fs::create_dir(sandbox.path().join("home"))?;
    let host = HostContext {
        executable: std::env::var_os("LIGHT_CLAUDE_EXECUTABLE")
            .context("Claude executable missing")?
            .into(),
        native_home: std::env::var_os("LIGHT_CLAUDE_HOME")
            .context("Claude native home missing")?
            .into(),
        working_directory: sandbox.path().join("work"),
        thread_scope: String::new(),
    };
    let deadline = deadline.context("workspace deadline missing")?;
    let (cancel_tx, cancel_rx) = watch::channel(cancellation.borrow().is_some());
    let env = environment(&host.native_home)?;
    ensure!(
        agent_core::sha256_digest(&tokio::fs::read(&host.executable).await?)
            == coding_agent_runtime::claude::BINARY_DIGEST,
        "Claude executable pin mismatch"
    );
    ensure!(
        capture(&host, &env, &["--version"], cancel_rx.clone(), deadline)
            .await?
            .trim()
            == VERSION,
        "Claude version mismatch"
    );
    let auth: Value = serde_json::from_str(
        &capture(
            &host,
            &env,
            &["auth", "status"],
            cancel_rx.clone(),
            deadline,
        )
        .await?,
    )?;
    ensure!(
        auth["loggedIn"] == true
            && auth["authMethod"] == "claude.ai"
            && auth["apiProvider"] == "firstParty",
        "native Claude subscription required"
    );
    let authentication = coding_agent_runtime::CodingAuthenticationEvidence {
        profile: CodingAuthenticationProfile::PersonalSubscription,
        credential_source: coding_agent_runtime::CodingCredentialSource::NativeClaudeStore,
        credential_generation: None,
        authoritative_usage: false,
    };
    let mut tools;
    let (answer, native_id) = if conversation.control.mode == CodingThreadMode::Close {
        tools = store.begin_tool_session(
            &spec.request.workspace_id,
            &job.task_id,
            &context.agent_id,
            conversation.control.mode != CodingThreadMode::Close
                && spec.request.intent == WorkspaceIntent::Implement,
            spec.request.expected_checkpoint_digest.as_deref(),
        )?;

        execution.mark_started()?;
        ("Conversation closed".to_owned(), None)
    } else {
        let native_id = if conversation.control.mode == CodingThreadMode::Resume {
            conversation
                .session
                .thread_id()
                .context("native session missing")?
                .to_owned()
        } else {
            Uuid::new_v4().to_string()
        };
        std::fs::write(
            sandbox.path().join("bridge.py"),
            include_str!("workspace_proxy.py"),
        )?;
        let listener = UnixListener::bind(sandbox.path().join("tools.sock"))?;
        let command = sandbox_command(
            sandbox.path(),
            &conversation.home,
            &host,
            &model,
            &native_id,
            conversation.control.mode,
        )?;
        let mut stream = Stream::new(&native_id, &policy.models[&model]);
        stream.expected_permission = Some("dontAsk");
        let prompt = format!(
            "Use task_workspace for every repository access. Files may have changed since previous turns: reread current files before making repository claims or edits. Purely conversational follow-ups need no tools. This is a {:?} turn. Only implementation may edit; edits require the current file digest. Never claim to execute tests, commit or publish. Task instruction:\n{}",
            spec.request.intent, spec.request.instruction
        );
        let mut calls = 0usize;
        tools = store.begin_tool_session(
            &spec.request.workspace_id,
            &job.task_id,
            &context.agent_id,
            conversation.control.mode != CodingThreadMode::Close
                && spec.request.intent == WorkspaceIntent::Implement,
            spec.request.expected_checkpoint_digest.as_deref(),
        )?;

        // Deterministic preparation is complete before either durable uncertainty boundary.
        execution.mark_started()?;
        conversation.session.begin()?;
        let outcome = {
            let native = supervise(
                command,
                prompt.as_bytes(),
                cancel_rx,
                deadline,
                None,
                |line| {
                    stream.feed(line)?;
                    Ok(Vec::new())
                },
            );
            tokio::pin!(native);
            let bridge = async {
                // Claude may reconnect its MCP transport within a turn.
                loop {
                    let (socket, _) = listener.accept().await?;
                    let (reader, mut out) = socket.into_split();
                    let mut reader = BufReader::new(reader);
                    loop {
                        let mut line = Vec::new();
                        let n = (&mut reader)
                            .take(FRAME as u64 + 1)
                            .read_until(b'\n', &mut line)
                            .await?;
                        if n == 0 {
                            break;
                        }
                        ensure!(
                            n <= FRAME && line.last() == Some(&b'\n'),
                            "oversized MCP frame"
                        );
                        let request: Value = serde_json::from_slice(&line)?;
                        let id = request.get("id").cloned();
                        let Some(id) = id else {
                            out.write_all(b"\n").await?;
                            continue;
                        };
                        let result = match request["method"].as_str() {
                            Some("initialize") => {
                                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"task-workspace","version":"1"}})
                            }
                            Some("ping") => json!({}),
                            Some("tools/list") => {
                                json!({"tools":[task_workspace::workspace_tool_definition()]})
                            }
                            Some("tools/call") => {
                                ensure!(
                                    request["params"]["name"] == "task_workspace",
                                    "unknown task tool"
                                );
                                calls += 1;
                                ensure!(calls <= 200, "workspace tool budget exceeded");
                                let current = RunnerWorkspaceConfig::load(&path)?;
                                current.authorize(&spec)?;
                                ensure!(current.store == config.store, "workspace store changed");
                                let result =
                                    tools.call_tool(request["params"]["arguments"].clone());
                                let failed = result.is_err();
                                let text=match result {Ok(v)=>v.to_string(),Err(_)=>"Tool rejected: check operation, repository, path, digest and access.".into()};
                                json!({"isError":failed,"content":[{"type":"text","text":text}]})
                            }
                            _ => {
                                out.write_all(&serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported method"}}))?).await?;
                                out.write_all(b"\n").await?;
                                continue;
                            }
                        };
                        let bytes =
                            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":result}))?;
                        ensure!(bytes.len() < FRAME, "MCP response exceeds bound");
                        out.write_all(&bytes).await?;
                        out.write_all(b"\n").await?;
                    }
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let outcome = tokio::select! {
                result=&mut native => result,
                result=bridge => { let _=cancel_tx.send(true); let _=native.await; result?; bail!("MCP bridge ended"); },
                _=cancellation.changed() => {let _=cancel_tx.send(true); let _=native.await; bail!("workspace cancelled");},
            };
            outcome
        };
        ensure!(outcome?, "Claude workspace process failed");
        let result = stream.finish()?;
        (
            crate::workspace_session::public_answer(Some(&result.text)),
            Some(native_id),
        )
    };
    RunnerWorkspaceConfig::load(&path)?.authorize(&spec)?;
    let checkpoint = tools.finish()?;
    let receipt = if let Some(native_id) = native_id {
        conversation.finish(&native_id)?
    } else {
        conversation.session.mark_closed()?
    };
    let output = json!({"finalMessage":answer,"authentication":authentication,"codingThread":receipt,"workspace":{
        "workspaceId":spec.request.workspace_id,"taskId":job.task_id,"jobId":job.job_id,"checkpointDigest":checkpoint.digest,"intent":spec.request.intent}});
    execution.finish(output.clone())?;
    crate::emit(
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

fn sandbox_command(
    root: &Path,
    home: &Path,
    host: &HostContext,
    model: &str,
    session: &str,
    mode: CodingThreadMode,
) -> Result<Command> {
    let credentials = host.native_home.join(".credentials.json");
    let meta = std::fs::symlink_metadata(&credentials)?;
    ensure!(
        meta.is_file()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() },
        "unsafe native credential store"
    );
    std::fs::create_dir_all(home.join(".claude"))?;
    std::fs::set_permissions(home.join(".claude"), std::fs::Permissions::from_mode(0o700))?;
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
    let mcp=json!({"mcpServers":{"task":{"command":"/usr/bin/python3","args":["-I","/session/bridge.py"]}}}).to_string();
    command
        .arg("--ro-bind")
        .arg(root)
        .arg("/session")
        .arg("--bind")
        .arg(home)
        .arg("/session/home")
        .arg("--ro-bind")
        .arg(credentials)
        .arg("/session/home/.claude/.credentials.json")
        .arg("--ro-bind")
        .arg(&host.executable)
        .arg("/opt/claude")
        .args([
            "--chdir",
            "/session/work",
            "--setenv",
            "HOME",
            "/session/home",
            "--setenv",
            "CLAUDE_CONFIG_DIR",
            "/session/home/.claude",
            "--",
            "/opt/claude",
            "-p",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompts",
            "none",
            "--model",
            model,
            if mode == CodingThreadMode::Resume {
                "--resume"
            } else {
                "--session-id"
            },
            session,
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            &mcp,
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true}",
            "--disable-slash-commands",
            "--permission-mode",
            "dontAsk",
            "--allowedTools",
            "mcp__task__task_workspace",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires host bubblewrap user namespaces"]
    async fn claude_workspace_namespace_hides_host_files() {
        let host_root = tempfile::tempdir().unwrap();
        let secret = host_root.path().join("private-marker");
        std::fs::write(&secret, "must not be visible").unwrap();
        std::fs::write(host_root.path().join(".credentials.json"), "{}").unwrap();
        let executable = host_root.path().join("claude");
        std::fs::write(&executable, format!(
            "#!/bin/sh\ntest ! -e '{}' && test ! -e /home/steve && test -d /session/work && test -f /session/home/.claude/.credentials.json && ! echo changed > /session/home/.claude/.credentials.json\n",
            secret.display()
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sandbox = tempfile::tempdir().unwrap();
        std::fs::create_dir(sandbox.path().join("work")).unwrap();
        std::fs::create_dir(sandbox.path().join("home")).unwrap();
        let home = tempfile::tempdir().unwrap();
        let host = HostContext {
            executable,
            native_home: host_root.path().into(),
            working_directory: sandbox.path().join("work"),
            thread_scope: String::new(),
        };
        let output = sandbox_command(
            sandbox.path(),
            home.path(),
            &host,
            "sonnet",
            &Uuid::new_v4().to_string(),
            CodingThreadMode::New,
        )
        .unwrap()
        .output()
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(host_root.path().join(".credentials.json")).unwrap(),
            "{}"
        );
    }
}
