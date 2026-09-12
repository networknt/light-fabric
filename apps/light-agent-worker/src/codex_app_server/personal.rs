//! Codex 0.153.4 personal configuration and model admission.
use super::*;
use anyhow::ensure;
use coding_agent_runtime::codex::PermissionMode;
use std::{
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
};

/// Kill the complete process group on errors, cancellation, and normal completion.
/// This also runs when the enclosing future is dropped at the runner deadline.
pub(super) struct ProcessGroup(pub u32);
impl ProcessGroup {
    pub(super) fn terminate(&mut self) {
        if self.0 != 0 {
            unsafe {
                libc::kill(-(self.0 as i32), libc::SIGKILL);
            }
            self.0 = 0;
        }
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub(super) fn permissions(params: &mut Value, spec: &CodingTurnSpec, thread: bool) {
    let Some(policy) = &spec.codex_policy else {
        return;
    };
    if !policy.native_permissions() {
        return;
    }
    let object = params.as_object_mut().unwrap();
    object.remove("approvalPolicy");
    object.remove(if thread { "sandbox" } else { "sandboxPolicy" });
    if policy.permission_mode == PermissionMode::TrustedPersonalUnattended {
        params["approvalPolicy"] = json!("never");
        if thread {
            params["sandbox"] = json!("danger-full-access");
        } else {
            params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        }
    }
}

/// Bubblewrap applies independently of native approval/sandbox settings. The source
/// repository and private Light checkpoints cannot be modified by native tools.
/// Paths stay identical, preserving relative config and executable references.
pub(super) fn isolate(
    command: &mut Command,
    spec: &CodingTurnSpec,
    repository: &Path,
    cwd: &Path,
) -> Result<()> {
    let launcher = Path::new("/usr/bin/bwrap");
    let meta = std::fs::symlink_metadata(launcher)
        .context("native Codex permissions require bubblewrap")?;
    ensure!(
        meta.is_file() && meta.uid() == 0 && meta.permissions().mode() & 0o022 == 0,
        "bubblewrap must be a root-owned non-writable executable"
    );
    let home = PathBuf::from(
        command
            .as_std()
            .get_envs()
            .find_map(|(k, v)| (k == "CODEX_HOME").then_some(v).flatten())
            .context("native Codex home missing")?,
    );
    let home = std::fs::canonicalize(home)?;
    ensure!(
        home != Path::new("/") && !repository.starts_with(&home),
        "invalid native home"
    );
    // Protect absent policy surfaces as well: a prompt must not install rules for
    // the next invocation. Empty defaults have no native permission grants.
    for name in ["config.toml", "AGENTS.md", "managed_config.toml"] {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(home.join(name))
        {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    for name in ["rules", "skills", "plugins"] {
        std::fs::create_dir_all(home.join(name))?;
    }
    let project_config = repository.join(".codex");
    ensure!(
        !project_config.is_symlink(),
        "staged Codex configuration cannot be a symlink"
    );
    std::fs::create_dir_all(&project_config)?;
    let original = command.as_std();
    let program = original.get_program().to_owned();
    let args: Vec<_> = original.get_args().map(|v| v.to_owned()).collect();
    let env: Vec<_> = original
        .get_envs()
        .filter_map(|(k, v)| v.map(|v| (k.to_owned(), v.to_owned())))
        .collect();
    let mut isolated = Command::new(launcher);
    isolated
        .env_clear()
        .envs(env)
        .current_dir(cwd)
        .args([
            "--die-with-parent",
            "--unshare-pid",
            "--unshare-ipc",
            "--unshare-uts",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--tmpfs",
            "/tmp",
        ])
        .arg("--bind")
        .arg(&home)
        .arg(&home);
    // Native shell/sandbox helpers need private writable temporary storage. Restore
    // staged paths at their original locations after hiding the host's /tmp.
    isolated.arg("--ro-bind").arg(repository).arg(repository);
    let executable = std::fs::canonicalize(&program)?;
    if executable.starts_with("/tmp") {
        let directory = executable
            .parent()
            .context("native executable has no directory")?;
        let distribution = if directory.file_name().is_some_and(|name| name == "bin") {
            directory
                .parent()
                .context("native distribution has no root")?
        } else {
            directory
        };
        ensure!(
            distribution != Path::new("/tmp"),
            "native executable requires its own distribution directory"
        );
        isolated
            .arg("--ro-bind")
            .arg(distribution)
            .arg(distribution);
    }
    // Configuration is owner-managed, never rewritten by a delegated coding turn.
    for name in [
        "config.toml",
        "rules",
        "skills",
        "plugins",
        "AGENTS.md",
        "managed_config.toml",
    ] {
        let path = home.join(name);
        if path.exists() {
            isolated.arg("--ro-bind").arg(&path).arg(&path);
        }
    }
    // The native home remains the owner's live configuration/login/session store.
    // Hide the worker's control records even though the parent holds their locks.
    let checkpoints = home.join("light-worker-threads");
    if checkpoints.exists() {
        isolated.arg("--tmpfs").arg(&checkpoints);
    }
    if spec.role == CodingRole::Implement {
        for root in materialized_writable_roots(spec, repository)? {
            isolated.arg("--bind").arg(&root).arg(&root);
        }
    } else {
        isolated.arg("--bind").arg(cwd).arg(cwd);
    }
    // Native execution produces a candidate patch; Git metadata belongs to the worker.
    isolated
        .arg("--ro-bind")
        .arg(repository.join(".git"))
        .arg(repository.join(".git"));
    isolated
        .arg("--ro-bind")
        .arg(&project_config)
        .arg(&project_config);
    isolated
        .arg("--")
        .arg(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    *command = isolated;
    Ok(())
}

pub(super) fn bound_model(spec: &CodingTurnSpec, state: Option<&Value>) -> Result<Option<String>> {
    let Some(policy) = &spec.codex_policy else {
        return Ok(None);
    };
    policy.validate(spec.native_model.as_deref())?;
    if spec
        .thread
        .as_ref()
        .is_some_and(|t| t.mode != coding_agent_runtime::CodingThreadMode::New)
    {
        let model = state
            .and_then(|s| s["nativeModel"].as_str())
            .context("session has no bound native model; start a new session")?;
        ensure!(
            spec.native_model.as_deref().is_none_or(|m| m == model),
            "native model change requires a new session"
        );
        policy.validate(Some(model))?;
        Ok(Some(model.to_owned()))
    } else {
        Ok(spec
            .native_model
            .clone()
            .or_else(|| policy.default_model.clone()))
    }
}

pub(super) async fn select_model(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    spec: &mut CodingTurnSpec,
    session: Option<&mut super::super::coding_session::CodingSession>,
    cwd: &Path,
    control: &TurnControl,
) -> Result<Value> {
    let Some(policy) = &spec.codex_policy else {
        return Ok(Value::Null);
    };
    let selected = bound_model(spec, session.as_ref().and_then(|s| s.adapter_state()))?;
    if spec
        .thread
        .as_ref()
        .is_some_and(|t| t.mode == coding_agent_runtime::CodingThreadMode::Close)
    {
        let evidence = session
            .as_ref()
            .and_then(|s| s.adapter_state())
            .context("missing native session evidence")?
            .clone();
        spec.native_model = selected;
        return Ok(evidence);
    }
    control
        .request(
            stdin,
            20,
            "config/read",
            json!({"cwd":cwd,"includeLayers":true}),
        )
        .await?;
    let config = response_controlled(stdout, 20, true, Some(control)).await?;
    let provider = config
        .pointer("/result/config/model_provider")
        .and_then(Value::as_str)
        .unwrap_or("openai");
    ensure!(
        provider == "openai",
        "personal Codex cannot use an API provider override"
    );
    // The pinned 0.153.4 config loader rejects reserved built-in provider IDs.
    // Config's serialized schema does not expose model_providers; do not infer
    // provider safety from an absent field in this response.
    // Hash only native opaque layer versions. Never serialize raw settings/credentials.
    let versions: Vec<_> = config
        .pointer("/result/layers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|layer| layer.get("version").and_then(Value::as_str))
        .collect();
    let revision = agent_runtime_protocol::canonical_digest(&versions)?;
    let ignored_layers = config
        .pointer("/result/layers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|layer| {
            layer
                .get("disabledReason")
                .is_some_and(|reason| !reason.is_null())
        })
        .count();
    let mut selected = selected.or_else(|| {
        config
            .pointer("/result/config/model")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let mut catalog = Vec::new();
    let mut cursor = Value::Null;
    for page in 0..32 {
        control
            .request(
                stdin,
                21,
                "model/list",
                json!({"cursor":cursor,"limit":100,"includeHidden":true}),
            )
            .await?;
        let response = response_controlled(stdout, 21, true, Some(control)).await?;
        let models = response
            .pointer("/result/data")
            .and_then(Value::as_array)
            .context("native model catalog missing")?;
        catalog.extend(models.iter().cloned());
        cursor = response
            .pointer("/result/nextCursor")
            .cloned()
            .unwrap_or(Value::Null);
        if cursor.is_null() {
            break;
        }
        ensure!(page < 31, "native model catalog pagination exceeded");
    }
    if selected.is_none() {
        selected = catalog
            .iter()
            .find(|m| m["isDefault"] == true)
            .and_then(|m| m["model"].as_str())
            .map(str::to_owned);
    }
    let selected = selected.context("native default model unavailable")?;
    policy.validate(Some(&selected))?;
    ensure!(
        catalog
            .iter()
            .any(|m| m["model"].as_str() == Some(&selected)),
        "native model unavailable in account catalog"
    );
    let evidence = json!({"schemaVersion":1,"permissionSource":policy.permission_source,
        "permissionMode":policy.permission_mode,"interaction":policy.interaction,
        "policyRevision":agent_runtime_protocol::canonical_digest(policy)?,"configurationRevision":revision,
        "ignoredConfigurationLayers":ignored_layers,
        "nativeModel":selected});
    if let Some(session) = session {
        session.set_adapter_state(evidence.clone());
    }
    spec.native_model = Some(selected);
    Ok(evidence)
}

pub(super) fn verify_thread(response: &Value, spec: &CodingTurnSpec) -> Result<()> {
    ensure!(
        response.pointer("/result/model").and_then(Value::as_str) == spec.native_model.as_deref(),
        "Codex substituted the selected native model"
    );
    ensure!(
        response
            .pointer("/result/modelProvider")
            .and_then(Value::as_str)
            == Some("openai"),
        "Codex changed the native provider"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> CodingTurnSpec {
        let mut spec = super::super::tests::persistent_spec();
        spec.codex_policy = Some(serde_json::from_value(json!({"schemaVersion":1,"permissionSource":"codex-cli",
            "permissionMode":"inherit","interaction":"unattended","allowedModels":["model-a","model-b"]})).unwrap());
        spec
    }
    #[test]
    fn native_inheritance_removes_overrides_and_trusted_mode_is_explicit() {
        let mut spec = spec();
        let p = thread_start_params(&spec, Path::new("/workspace/repository"));
        assert!(p.get("approvalPolicy").is_none() && p.get("sandbox").is_none());
        let p = turn_start_params(
            &spec,
            "thread",
            Path::new("/workspace/repository"),
            Path::new("/workspace/repository"),
        );
        assert!(p.get("approvalPolicy").is_none() && p.get("sandboxPolicy").is_none());
        spec.codex_policy.as_mut().unwrap().permission_mode =
            PermissionMode::TrustedPersonalUnattended;
        let p = thread_start_params(&spec, Path::new("/workspace/repository"));
        assert_eq!(p["sandbox"], "danger-full-access");
        assert_eq!(p["approvalPolicy"], "never");
    }
    #[test]
    fn resume_omission_binds_model_and_rejects_switches_even_when_default_changes() {
        let mut spec = spec();
        spec.thread.as_mut().unwrap().mode = coding_agent_runtime::CodingThreadMode::Resume;
        spec.codex_policy.as_mut().unwrap().default_model = Some("model-b".into());
        let state = json!({"nativeModel":"model-a"});
        assert_eq!(
            bound_model(&spec, Some(&state)).unwrap().as_deref(),
            Some("model-a")
        );
        spec.native_model = Some("model-b".into());
        assert!(bound_model(&spec, Some(&state)).is_err());
        spec.native_model = Some("model-a".into());
        assert!(bound_model(&spec, Some(&state)).is_ok());
        assert!(bound_model(&spec, None).is_err());
    }
    #[test]
    fn native_substitution_is_rejected() {
        let mut spec = spec();
        spec.native_model = Some("model-a".into());
        assert!(
            verify_thread(
                &json!({"result":{"model":"model-b","modelProvider":"openai"}}),
                &spec
            )
            .is_err()
        );
        assert!(
            verify_thread(
                &json!({"result":{"model":"model-a","modelProvider":"other"}}),
                &spec
            )
            .is_err()
        );
        verify_thread(
            &json!({"result":{"model":"model-a","modelProvider":"openai"}}),
            &spec,
        )
        .unwrap();
    }
    #[test]
    fn durable_native_binding_rejects_policy_drift_before_claiming() {
        use crate::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = agent_core::sha256_digest(b"scope");
        let mut spec = spec();
        let mut session = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        session.set_adapter_state(json!({"nativeModel":"model-a"}));
        session.begin().unwrap();
        let receipt = session.finish("native-id", "", false).unwrap();
        drop(session);
        spec.thread.as_mut().unwrap().mode = coding_agent_runtime::CodingThreadMode::Resume;
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        let session = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        assert_eq!(
            bound_model(&spec, session.adapter_state())
                .unwrap()
                .as_deref(),
            Some("model-a")
        );
        drop(session);
        spec.codex_policy.as_mut().unwrap().permission_mode =
            PermissionMode::TrustedPersonalUnattended;
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
        spec.codex_policy.as_mut().unwrap().permission_mode = PermissionMode::Inherit;
        let mut session = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        session.begin().unwrap();
        drop(session);
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
    }

    #[tokio::test]
    async fn outer_namespace_preserves_configuration_and_blocks_candidate_and_control_writes() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let repository = root.path().join("repository");
        let scratch = root.path().join("scratch");
        for path in [&home, &repository, &scratch] {
            std::fs::create_dir(path).unwrap();
        }
        std::fs::create_dir(repository.join(".git")).unwrap();
        std::fs::create_dir(home.join("light-worker-threads")).unwrap();
        std::fs::write(home.join("light-worker-threads/secret"), "checkpoint").unwrap();
        std::fs::write(home.join("config.toml"), "native-config").unwrap();
        let mut spec = spec();
        for role in [CodingRole::Implement, CodingRole::Review] {
            spec.role = role;
            let cwd = if role == CodingRole::Implement {
                &repository
            } else {
                &scratch
            };
            let mut cmd = Command::new("/bin/sh");
            cmd.env("CODEX_HOME", &home).current_dir(cwd)
                .arg("-c").arg("test \"$(cat \"$1/config.toml\")\" = native-config && test ! -e \"$1/light-worker-threads/secret\" && ! touch \"$2/.git/forbidden\" && ! sh -c 'echo changed > \"$1/config.toml\"' sh \"$1\" && ! touch \"$1/rules/injected.rules\" && ! touch \"$2/.codex/config.toml\" && echo allowed > allowed")
                .arg("sh").arg(&home).arg(&repository);
            // Every policy surface starts absent except config.toml. Verify both
            // content writes and replacement attacks, then allowed session writes.
            let script = cmd
                .as_std()
                .get_args()
                .nth(1)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let extra = r#" && ! touch "$1/skills/injected" && ! touch "$1/plugins/injected" && ! sh -c 'echo injected > "$1/AGENTS.md"' sh "$1" && ! sh -c 'echo injected > "$1/managed_config.toml"' sh "$1" && ! rm "$1/AGENTS.md" && ! mv "$1/skills" "$1/skills-old" && touch "$1/session-write""#;
            let mut cmd = Command::new("/bin/sh");
            cmd.env("CODEX_HOME", &home)
                .current_dir(cwd)
                .arg("-c")
                .arg(script + extra)
                .arg("sh")
                .arg(&home)
                .arg(&repository);
            isolate(&mut cmd, &spec, &repository, cwd).unwrap();
            let output = cmd.output().await.unwrap();
            assert!(
                output.status.success(),
                "namespace unavailable or boundary failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(cwd.join("allowed").exists());
            if role == CodingRole::Review {
                let mut cmd = Command::new("/usr/bin/touch");
                cmd.env("CODEX_HOME", &home)
                    .current_dir(cwd)
                    .arg(repository.join("forbidden"));
                isolate(&mut cmd, &spec, &repository, cwd).unwrap();
                assert!(!cmd.output().await.unwrap().status.success());
            }
        }
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            "native-config"
        );
        assert!(!repository.join("forbidden").exists());
    }

    #[tokio::test]
    async fn process_guard_kills_descendants_on_cancel_or_error() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("escaped");
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "(sleep 0.2; touch \"$1\") & wait", "sh"])
            .arg(&marker)
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let guard = ProcessGroup(child.id().unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        drop(guard);
        child.wait().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn native_errors_and_reroutes_do_not_expose_payloads() {
        for frame in [
            json!({"id":3,"error":{"code":-1,"message":"secret-native-settings"}}),
            json!({"method":"model/rerouted","params":{"toModel":"secret-native-settings"}}),
        ] {
            let mut child = Command::new("/bin/echo")
                .arg(frame.to_string())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let error = response_controlled(
                &mut BufReader::new(child.stdout.take().unwrap()),
                3,
                true,
                None,
            )
            .await
            .unwrap_err();
            assert!(!error.to_string().contains("secret-native-settings"));
            child.wait().await.unwrap();
        }
    }

    struct CancellingWriter {
        bytes: Vec<u8>,
        cancel: tokio::sync::watch::Sender<Option<String>>,
        yielded: bool,
    }
    impl AsyncWrite for CancellingWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            // Cancel between the JSON body and its newline and force the executor
            // to poll other futures before allowing the rest of the frame.
            if bytes == b"\n" && !self.yielded {
                self.cancel.send(Some("late cancellation".into())).unwrap();
                self.yielded = true;
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            self.bytes.extend_from_slice(bytes);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn cancellation_between_event_body_and_newline_preserves_both_frames() {
        let (sender, cancel) = tokio::sync::watch::channel(None);
        let control = TurnControl {
            cancel,
            deadline: None,
        };
        let mut writer = CancellingWriter {
            bytes: Vec::new(),
            cancel: sender,
            yielded: false,
        };
        let id = identity();
        let (mut child, _stdin, mut stdout) = fake_server("import time; time.sleep(300)");
        complete_operation(&mut writer, &id, &mut 0, async |writer, sequence| {
            emit(
                writer,
                &id,
                sequence,
                RuntimeEventPayload::Progress {
                    message: "native progress".into(),
                },
            )
            .await?;
            response_controlled(&mut stdout, 1, true, Some(&control)).await?;
            Ok(())
        })
        .await
        .unwrap();
        child.kill().await.unwrap();
        let frames: Vec<Value> = writer
            .bytes
            .split(|b| *b == b'\n')
            .filter(|s| !s.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["payload"]["type"], "progress");
        assert_eq!(frames[1]["payload"]["class"], "cancelled");
    }

    #[tokio::test]
    async fn late_cancellation_delivers_the_committed_checkpoint_and_allows_resume() {
        use crate::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = agent_core::sha256_digest(b"late-cancel-scope");
        let mut spec = spec();
        let mut session =
            Some(CodingSession::open(home.path(), &scope, &spec, "contract").unwrap());
        session.as_mut().unwrap().begin().unwrap();
        let (mut child, mut stdin, mut stdout) = fake_server("import time; time.sleep(300)");
        let (sender, cancel) = tokio::sync::watch::channel(None);
        let mut writer = CancellingWriter {
            bytes: Vec::new(),
            cancel: sender.clone(),
            yielded: false,
        };
        let id = identity();
        complete_operation(&mut writer, &id, &mut 0, async |writer, sequence| {
            let receipt = finish_thread(
                &mut stdin,
                &mut stdout,
                &spec,
                &mut session,
                "native-id",
                "earned-patch",
                None,
            )
            .await?;
            // The exact former race: READY has been written, but shutdown and
            // terminal frame delivery still await. Neither may lose the receipt.
            sender.send(Some("cancel during shutdown".into())).unwrap();
            tokio::task::yield_now().await;
            shutdown(&mut child).await;
            emit(
                writer,
                &id,
                sequence,
                RuntimeEventPayload::Terminal {
                    class: ResultClass::Success,
                    output: Some(json!({"codingThread":receipt})),
                    error: None,
                },
            )
            .await
        })
        .await
        .unwrap();
        assert!(cancel.borrow().is_some());
        let event: Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(event["payload"]["class"], "success");
        let receipt = &event["payload"]["output"]["codingThread"];
        drop(session);
        spec.thread.as_mut().unwrap().mode = coding_agent_runtime::CodingThreadMode::Resume;
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        let resumed = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        assert_eq!(resumed.thread_id(), Some("native-id"));
    }
    fn identity() -> RuntimeIdentity {
        serde_json::from_value(json!({
            "executionId":uuid::Uuid::now_v7(),"leaseId":uuid::Uuid::now_v7(),
            "fencingToken":1,"transportNonce":"a".repeat(32)}))
        .unwrap()
    }

    fn fake_server(
        script: &str,
    ) -> (
        Child,
        tokio::process::ChildStdin,
        BufReader<tokio::process::ChildStdout>,
    ) {
        let mut child = Command::new("python3")
            .args(["-u", "-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        (child, stdin, stdout)
    }

    #[tokio::test]
    async fn cancellation_interrupts_without_discarding_a_partial_native_frame() {
        let (mut child, mut stdin, mut stdout) = fake_server(
            r#"
import json,sys
sys.stdout.write('{"method":"turn/');sys.stdout.flush()
request=json.loads(sys.stdin.readline())
assert request['method']=='turn/interrupt'
assert request['params']=={'threadId':'thread','turnId':'turn'}
sys.stdout.write('completed","params":{"turn":{"id":"turn","status":"interrupted"}}}\n');sys.stdout.flush()
"#,
        );
        // Consume neither half of the frame in the test. The worker must retain
        // its own partial-read buffer while it sends the cancellation request.
        let (sender, cancel) = tokio::sync::watch::channel(None);
        let control = TurnControl {
            cancel,
            deadline: None,
        };
        let signal = async {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            sender.send(Some("cancel".into())).unwrap();
        };
        let mut output = Vec::new();
        let id = identity();
        let mut sequence = 0;
        let run = drive_turn(
            &mut output,
            &id,
            &mut sequence,
            &mut stdin,
            &mut stdout,
            "thread",
            "turn",
            &control,
            true,
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(run, signal)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap().status, "interrupted");
        assert!(child.wait().await.unwrap().success());
    }

    #[tokio::test(start_paused = true)]
    async fn native_waits_observe_existing_cancel_and_deadline_and_bound_interrupt_grace() {
        for existing_cancel in [true, false] {
            let (mut child, mut stdin, mut stdout) = fake_server("import time; time.sleep(300)");
            let (_sender, cancel) =
                tokio::sync::watch::channel(existing_cancel.then(|| "cancel".into()));
            let control = TurnControl {
                cancel,
                deadline: Some(tokio::time::Instant::now()),
            };
            assert!(
                response_controlled(&mut stdout, 1, true, Some(&control))
                    .await
                    .unwrap_err()
                    .is::<TurnCancelled>()
            );
            let result = drive_turn(
                &mut Vec::new(),
                &identity(),
                &mut 0,
                &mut stdin,
                &mut stdout,
                "thread",
                "turn",
                &control,
                true,
            )
            .await;
            assert!(result.err().unwrap().is::<TurnCancelled>());
            child.kill().await.unwrap();
        }
    }

    #[tokio::test]
    async fn cancellation_bounds_a_blocked_native_request_write() {
        let (mut child, mut stdin, _stdout) = fake_server("import time; time.sleep(300)");
        let (sender, cancel) = tokio::sync::watch::channel(None);
        let control = TurnControl {
            cancel,
            deadline: None,
        };
        let signal = async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            sender.send(Some("cancel blocked write".into())).unwrap();
        };
        let request = control.request(
            &mut stdin,
            4,
            "turn/start",
            json!({"input":"x".repeat(1024 * 1024)}),
        );
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(request, signal)
        })
        .await
        .unwrap();
        assert!(result.unwrap_err().is::<TurnCancelled>());
        child.kill().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_responses_keep_error_evidence_and_allow_native_reroutes() {
        let (mut child, _stdin, mut stdout) = fake_server(
            r#"
import json
print(json.dumps({'method':'model/rerouted','params':{'toModel':'other'}}))
print(json.dumps({'id':3,'error':{'message':'legacy-evidence'}}))
"#,
        );
        let error = response(&mut stdout, 3).await.unwrap_err();
        assert!(error.to_string().contains("legacy-evidence"));
        child.wait().await.unwrap();
    }

    #[tokio::test]
    async fn approval_evidence_is_redacted_only_for_personal_policy() {
        for personal in [false, true] {
            let (mut child, mut stdin, mut stdout) = fake_server(
                r#"
import json,sys
print(json.dumps({'id':8,'method':'item/commandExecution/requestApproval','params':{'command':'approval-evidence'}}),flush=True)
assert json.loads(sys.stdin.readline())['result']['decision']=='cancel'
print(json.dumps({'method':'turn/completed','params':{'turn':{'id':'turn','status':'completed'}}}),flush=True)
"#,
            );
            let (_sender, cancel) = tokio::sync::watch::channel(None);
            let control = TurnControl {
                cancel,
                deadline: None,
            };
            let mut output = Vec::new();
            let result = drive_turn(
                &mut output,
                &identity(),
                &mut 0,
                &mut stdin,
                &mut stdout,
                "thread",
                "turn",
                &control,
                personal,
            )
            .await
            .unwrap();
            assert_eq!(result.status, "completed");
            let event: Value = serde_json::from_slice(&output).unwrap();
            assert_eq!(
                event["payload"]["subject"],
                if personal {
                    json!({"interaction":"unattended"})
                } else {
                    json!({"command":"approval-evidence"})
                }
            );
            assert!(child.wait().await.unwrap().success());
        }
    }

    #[tokio::test]
    async fn managed_native_process_and_descendants_stay_in_runner_group() {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "sleep 300 & echo $!; wait"])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let (mut child, guard) = spawn_native_process(&mut command, false).unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        let descendant: i32 = line.trim().parse().unwrap();
        unsafe {
            assert_eq!(libc::getpgid(child.id().unwrap() as i32), libc::getpgrp());
            assert_eq!(libc::getpgid(descendant), libc::getpgrp());
            libc::kill(descendant, libc::SIGKILL);
        }
        assert_eq!(guard.0, 0);
        child.kill().await.unwrap();
    }
    #[tokio::test]
    async fn preflight_failure_emits_terminal_without_waiting_for_runner_eof() {
        let identity: RuntimeIdentity = serde_json::from_value(json!({
            "executionId":uuid::Uuid::now_v7(),"leaseId":uuid::Uuid::now_v7(),
            "fencingToken":1,"transportNonce":"a".repeat(32)}))
        .unwrap();
        let (_sender, receiver) = tokio::sync::watch::channel(None);
        let mut output = Vec::new();
        super::super::run(
            &mut output,
            &identity,
            &mut 0,
            json!({}),
            None,
            receiver,
            None,
        )
        .await
        .unwrap();
        let event: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(event["payload"]["type"], "terminal");
        assert_eq!(event["payload"]["class"], "terminal_failure");
    }
}
