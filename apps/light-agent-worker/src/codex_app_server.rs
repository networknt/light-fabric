use super::emit;
use agent_core::ResultClass;
use agent_materializer::MaterializationManifest;
use agent_runtime_protocol::{
    AttemptCredentialEnvelope, BrokerOperation, BrokerRequest, BrokerResponse,
    EnterpriseGatewayConfig, MAX_FRAME_BYTES, RuntimeEventPayload, RuntimeIdentity,
};
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use coding_agent_runtime::{
    CODEX_APP_SERVER_ADAPTER_ID, CODEX_APP_SERVER_BINARY_DIGEST, CODEX_APP_SERVER_PROTOCOL_VERSION,
    CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST, CODEX_APP_SERVER_SCHEMA_DIGEST,
    CODEX_APP_SERVER_VERSION, CodingAdapterContract, CodingAdapterQualification,
    CodingAuthenticationEvidence, CodingAuthenticationProfile, CodingCredentialSource,
    CodingReviewResult, CodingRole, CodingTurnSpec, CodingValidationEvidence, ValidationStatus,
    patch_digest, validate_patch,
};
use execution_backend::StagedInput;
use execution_security::ProtectedPathPolicy;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    process::{Child, Command},
};

const GIT_EXECUTABLE: &str = "/usr/bin/git";

pub(super) async fn run<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
    enterprise_gateway: Option<EnterpriseGatewayConfig>,
    cancel: tokio::sync::watch::Receiver<Option<String>>,
    deadline: Option<tokio::time::Instant>,
) -> Result<()> {
    let spec: CodingTurnSpec = serde_json::from_value(required(&input, "codingSpec")?.clone())?;
    spec.validate()?;
    let manifest: MaterializationManifest =
        serde_json::from_value(required(&input, "materializationManifest")?.clone())?;
    if manifest.digest()? != spec.materialization_manifest_digest
        || manifest.writable_roots != spec.writable_roots
    {
        bail!("Codex materialization manifest differs from the admitted turn")
    }
    let contract: CodingAdapterContract =
        serde_json::from_value(required(&input, "adapterContract")?.clone())?;
    let qualification: CodingAdapterQualification =
        serde_json::from_value(required(&input, "adapterQualification")?.clone())?;
    validate_contract(&contract, &qualification).await?;
    let staged: Vec<StagedInput> =
        serde_json::from_value(required(&input, "runtimeStagedInputs")?.clone())?;
    let bundle = staged
        .iter()
        .find(|entry| entry.mount_target == "/inputs/repository.bundle")
        .context("staged immutable repository bundle is unavailable")?;
    if bundle.source_digest != spec.repository_digest
        || bundle.media_type != "application/x-git-bundle"
        || !bundle.read_only
        || bundle.executable
    {
        bail!("staged repository does not match the admitted immutable input")
    }
    // Decided before any thread checkpoint is claimed: a route mismatch is a pre-Codex
    // rejection and must not consume the workflow's conversation.
    match (spec.authentication_profile, enterprise_gateway.is_some()) {
        (CodingAuthenticationProfile::PersonalSubscription, false) => {
            if std::env::var_os("LIGHT_CODEX_HOME").is_none() {
                bail!("personal-subscription requires a runner-projected native Codex home")
            }
        }
        (CodingAuthenticationProfile::EnterpriseApi, true) => {
            if std::env::var_os("LIGHT_CODEX_HOME").is_some() {
                bail!("enterprise-api forbids native Codex credential-store visibility")
            }
        }
        _ => bail!("Codex authentication profile and enterprise gateway route differ"),
    }
    let workspace = tempfile::tempdir().context("create Codex workspace")?;
    let repository = workspace.path().join("repository");
    git(
        ["clone", "--quiet"],
        Some(&bundle.local_path),
        &repository,
        None,
    )
    .await?;
    checkout_base(&repository, &spec.base_revision).await?;
    // Claiming the checkpoint marks the thread IN_FLIGHT, which is unrecoverable. Every
    // failure above this point provably never reached Codex, so it leaves the workflow's
    // thread resumable instead of forcing a brand-new conversation.
    let mut session = if spec.thread.is_some() {
        let home = std::env::var_os("LIGHT_CODEX_HOME")
            .context("persistent coding threads require native Codex home")?;
        let scope = required(&input, "threadScope")?
            .as_str()
            .context("invalid thread scope")?;
        Some(super::coding_session::CodingSession::open(
            Path::new(&home),
            scope,
            &spec,
            &contract.digest()?,
        )?)
    } else {
        None
    };
    if spec.role == CodingRole::Implement
        && let Some(session) = &session
    {
        if !session.patch().is_empty() {
            git_apply(&repository, session.patch().as_bytes()).await?;
        }
        if let Some(remediation) = &spec.remediation
            && patch_digest(session.patch()) != remediation.prior_review.artifact_digest
        {
            bail!("remediation findings refer to a different session checkpoint patch");
        }
    }

    let review_scratch = if spec.role == CodingRole::Review {
        let review = spec
            .review_input
            .as_deref()
            .context("review role requires immutable review input")?;
        git_apply(&repository, review.candidate_patch.as_bytes()).await?;
        verify_candidate_unchanged(&repository, &review.candidate_patch).await?;
        Some(tempfile::tempdir().context("create review build scratch")?)
    } else {
        None
    };
    let turn_cwd = review_scratch
        .as_ref()
        .map_or(repository.as_path(), |scratch| scratch.path());

    let executable = std::env::var_os("LIGHT_CODEX_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&contract.executable));
    let enterprise_home = if let Some(gateway) = &enterprise_gateway {
        Some(prepare_enterprise_gateway(gateway, identity).await?)
    } else {
        None
    };
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("PATH", "/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin")
        .env("HOME", "/var/lib/light-agent-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(scratch) = &review_scratch {
        for directory in ["tmp", "cargo-target", "gradle", "maven"] {
            std::fs::create_dir(scratch.path().join(directory))?;
        }
        command
            .env("TMPDIR", scratch.path().join("tmp"))
            .env("CARGO_TARGET_DIR", scratch.path().join("cargo-target"))
            .env("GRADLE_USER_HOME", scratch.path().join("gradle"))
            .env(
                "MAVEN_OPTS",
                format!(
                    "-Dmaven.repo.local={}",
                    scratch.path().join("maven").display()
                ),
            );
    }
    if let Some((home, token, credential_env, _generation)) = enterprise_home.as_ref() {
        for argument in enterprise_cli_overrides(
            enterprise_gateway
                .as_ref()
                .expect("enterprise home requires enterprise gateway"),
        ) {
            command.arg("-c").arg(argument);
        }
        command
            .env("CODEX_HOME", home.path())
            .env(credential_env, token);
    } else if let Some(home) = std::env::var_os("LIGHT_CODEX_HOME") {
        command.env("CODEX_HOME", home);
    }
    command.arg("app-server");
    let mut child = command.spawn().context("spawn pinned Codex App Server")?;
    let mut stdin = child.stdin.take().context("Codex stdin unavailable")?;
    let stdout = child.stdout.take().context("Codex stdout unavailable")?;
    let stderr = child.stderr.take().context("Codex stderr unavailable")?;
    let stderr_drain = tokio::spawn(async move {
        let mut bounded = stderr.take(1024 * 1024 + 1);
        let mut bytes = Vec::new();
        bounded.read_to_end(&mut bytes).await
    });
    let mut stdout = BufReader::new(stdout);

    request(
        &mut stdin,
        1,
        "initialize",
        json!({"clientInfo":{"name":"light-agent-worker","title":"Light Agent Worker","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":false,"requestAttestation":false}}),
    )
    .await?;
    response(&mut stdout, 1).await?;
    notification(&mut stdin, "initialized").await?;
    request(&mut stdin, 2, "account/read", json!({"refreshToken":false})).await?;
    let account = response(&mut stdout, 2).await?;
    let authentication = authentication_evidence(
        spec.authentication_profile,
        &account,
        enterprise_home.as_ref().map(|value| value.3),
    )?;
    let (method, params) = thread_open_params(
        &spec,
        turn_cwd,
        session.as_ref().and_then(|s| s.thread_id()),
    );
    // The last point at which nothing durable has happened to the native thread. Patch
    // restoration, remediation binding, review preparation, the Codex spawn and the
    // authentication checks have all passed, so claiming here is the narrowest window in
    // which an interruption is genuinely uncertain.
    if let Some(session) = session.as_mut() {
        session.begin()?;
    }
    request(&mut stdin, 3, method, params).await?;
    let thread = response(&mut stdout, 3).await?;
    let thread_id = string_at(&thread, "/result/thread/id")?;
    if let Some(expected) = session.as_ref().and_then(|s| s.thread_id())
        && expected != thread_id
    {
        bail!("Codex resumed a different thread");
    }
    if spec
        .thread
        .as_ref()
        .is_some_and(|t| t.mode == coding_agent_runtime::CodingThreadMode::Close)
    {
        // A dedicated close turn carries no other result, so a failed archive fails it.
        // It is still bounded: an unanswered request should fail the turn promptly rather
        // than hold the worker until the runner kills it at the execution deadline.
        let budget = archive_budget(deadline)
            .context("too little execution budget left to close the thread")?;
        tokio::time::timeout(budget, archive_thread(&mut stdin, &mut stdout, thread_id))
            .await
            .with_context(|| {
                format!(
                    "thread/archive did not answer within {}s",
                    budget.as_secs_f32()
                )
            })??;
        let receipt = session
            .as_mut()
            .context("close requires persisted session")?
            .finish(thread_id, "", true)?;
        shutdown(&mut child).await;
        stderr_drain.abort();
        return emit(
            writer,
            identity,
            sequence,
            RuntimeEventPayload::Terminal {
                class: ResultClass::Success,
                output: Some(json!({"authentication":authentication,"codingThread":receipt})),
                error: None,
            },
        )
        .await;
    }
    request(
        &mut stdin,
        4,
        "turn/start",
        turn_start_params(&spec, thread_id, &repository, turn_cwd),
    )
    .await?;
    let turn = response(&mut stdout, 4).await?;
    let turn_id = string_at(&turn, "/result/turn/id")?;
    let terminal = drive_turn(
        writer,
        identity,
        sequence,
        &mut stdin,
        &mut stdout,
        thread_id,
        turn_id,
        cancel,
    )
    .await?;
    if terminal.status != "completed" {
        shutdown(&mut child).await;
        stderr_drain.abort();
        emit(
            writer,
            identity,
            sequence,
            RuntimeEventPayload::Terminal {
                class: if terminal.status == "interrupted" {
                    ResultClass::Cancelled
                } else {
                    ResultClass::TerminalFailure
                },
                output: None,
                error: Some(format!("Codex turn ended with status {}", terminal.status)),
            },
        )
        .await?;
        return Ok(());
    }
    if spec.role == CodingRole::Review {
        let review_input = spec
            .review_input
            .as_deref()
            .context("review input disappeared")?;
        verify_candidate_unchanged(&repository, &review_input.candidate_patch).await?;
        let result: CodingReviewResult = serde_json::from_str(
            terminal
                .final_message
                .as_deref()
                .context("reviewer did not return a structured final message")?,
        )
        .context("reviewer final message is not CodingReviewResult")?;
        result.validate()?;
        if result.review_id != review_input.review_id
            || result.artifact_digest != review_input.implementation.patch_digest
        {
            bail!("review result differs from the admitted review input")
        }
        let receipt = finish_thread(
            &mut stdin,
            &mut stdout,
            &spec,
            &mut session,
            thread_id,
            "",
            deadline,
        )
        .await?;
        shutdown(&mut child).await;
        stderr_drain.abort();
        return emit(
            writer,
            identity,
            sequence,
            RuntimeEventPayload::Terminal {
                class: ResultClass::Success,
                output: Some(json!({"adapter":CODEX_APP_SERVER_ADAPTER_ID,"adapterVersion":CODEX_APP_SERVER_VERSION,"threadId":thread_id,"turnId":turn_id,"authentication":authentication,"codingThread":receipt,"codingReview":result,"reviewValidationEvidence":terminal.validation_evidence})),
                error: None,
            },
        )
        .await;
    }

    git_simple(&repository, &["add", "-N", "."]).await?;
    let patch = canonical_git_diff(&repository).await?;
    let names = git_output(&repository, &["diff", "--name-only", "HEAD"]).await?;
    let changed_paths: Vec<String> = names
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let validated = validate_patch(
        &spec,
        &ProtectedPathPolicy::default_deny(),
        &spec.base_revision,
        &patch,
        &changed_paths,
    )?;
    // The durable artifact is published before the thread bookkeeping. A failing
    // thread/archive must not discard an implementation that already succeeded.
    emit(
        writer,
        identity,
        sequence,
        RuntimeEventPayload::CodingPatch {
            base_revision: validated.base_revision,
            patch: validated.patch,
            patch_digest: validated.patch_digest,
            changed_paths: validated.changed_paths.into_iter().collect(),
        },
    )
    .await?;
    let receipt = finish_thread(
        &mut stdin,
        &mut stdout,
        &spec,
        &mut session,
        thread_id,
        &patch,
        deadline,
    )
    .await?;
    shutdown(&mut child).await;
    stderr_drain.abort();
    emit(
        writer,
        identity,
        sequence,
        RuntimeEventPayload::Terminal {
            class: ResultClass::Success,
            output: Some(json!({"adapter":CODEX_APP_SERVER_ADAPTER_ID,"adapterVersion":CODEX_APP_SERVER_VERSION,"threadId":thread_id,"turnId":turn_id,"authentication":authentication,"codingThread":receipt,"validationEvidence":terminal.validation_evidence,"finalMessage":terminal.final_message})),
            error: None,
        },
    )
    .await
}

async fn prepare_enterprise_gateway(
    gateway: &EnterpriseGatewayConfig,
    identity: &RuntimeIdentity,
) -> Result<(tempfile::TempDir, String, String, u64)> {
    gateway.validate()?;
    if std::env::var_os("LIGHT_CODEX_HOME").is_some() {
        bail!("personal Codex home and enterprise gateway profile are mutually exclusive")
    }
    let socket = std::env::var_os("LIGHT_AGENT_BROKER_SOCKET")
        .context("enterprise gateway requires the protected attempt broker")?;
    let request = BrokerRequest {
        request_id: uuid::Uuid::new_v4(),
        execution_id: identity.execution_id,
        lease_id: identity.lease_id,
        fencing_token: identity.fencing_token,
        policy_digest: gateway.binding.policy_digest.clone(),
        data_boundary_digest: gateway.binding.data_boundary_digest.clone(),
        operation: BrokerOperation::CredentialedRequest,
        target: gateway.credential_target.clone(),
        method: "GET".into(),
        path: "credential".into(),
        body_base64: String::new(),
        declared_tokens: 0,
        declared_cost_micros: 0,
    };
    let mut stream = tokio::net::UnixStream::connect(socket)
        .await
        .context("connect attempt credential broker")?;
    stream.write_all(&serde_json::to_vec(&request)?).await?;
    stream.write_all(b"\n").await?;
    stream.shutdown().await?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(20 * 1024)
        .read_to_end(&mut response)
        .await?;
    let response: BrokerResponse = serde_json::from_slice(&response)?;
    if response.request_id != request.request_id || response.status != 200 {
        bail!("attempt credential broker returned an invalid response")
    }
    let envelope: AttemptCredentialEnvelope =
        serde_json::from_slice(&STANDARD.decode(response.body_base64)?)?;
    let binding_digest = gateway.binding.digest()?;
    envelope.validate(
        &gateway.binding.audience,
        &binding_digest,
        chrono::Utc::now(),
    )?;
    let home = tempfile::tempdir().context("create enterprise Codex home")?;
    let config = render_enterprise_config(gateway);
    let path = home.path().join("config.toml");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    use std::io::Write as _;
    options.open(&path)?.write_all(config.as_bytes())?;
    Ok((
        home,
        envelope.token,
        gateway.credential_env.clone(),
        envelope.generation,
    ))
}

pub(super) fn authentication_evidence(
    profile: CodingAuthenticationProfile,
    account_response: &Value,
    credential_generation: Option<u64>,
) -> Result<CodingAuthenticationEvidence> {
    let requires_openai_auth = account_response
        .pointer("/result/requiresOpenaiAuth")
        .and_then(Value::as_bool)
        .context("Codex account response omitted requiresOpenaiAuth")?;
    let account_type = account_response
        .pointer("/result/account/type")
        .and_then(Value::as_str);
    let evidence = match profile {
        CodingAuthenticationProfile::PersonalSubscription => {
            if !requires_openai_auth || account_type != Some("chatgpt") {
                bail!("personal-subscription requires an authenticated ChatGPT Codex account")
            }
            CodingAuthenticationEvidence {
                profile,
                credential_source: CodingCredentialSource::NativeCodexStore,
                credential_generation: None,
                authoritative_usage: false,
            }
        }
        CodingAuthenticationProfile::EnterpriseApi => {
            if requires_openai_auth
                || account_response
                    .pointer("/result/account")
                    .is_some_and(|value| !value.is_null())
            {
                bail!("enterprise-api must not discover an ambient OpenAI account")
            }
            CodingAuthenticationEvidence {
                profile,
                credential_source: CodingCredentialSource::AttemptBroker,
                credential_generation,
                authoritative_usage: true,
            }
        }
    };
    evidence.validate()?;
    Ok(evidence)
}

fn render_enterprise_config(gateway: &EnterpriseGatewayConfig) -> String {
    let quote = |value: &str| serde_json::to_string(value).expect("string JSON is TOML compatible");
    format!(
        "model_provider = {provider}\n\n[model_providers.{provider_id}]\nname = \"Light LLM Gateway\"\nbase_url = {base_url}\nwire_api = \"responses\"\nenv_key = {credential_env}\nrequires_openai_auth = false\n\n[shell_environment_policy]\nexclude = [{credential_env}]\n",
        provider = quote(&gateway.provider_id),
        provider_id = gateway.provider_id,
        base_url = quote(&gateway.base_url),
        credential_env = quote(&gateway.credential_env),
    )
}

fn enterprise_cli_overrides(gateway: &EnterpriseGatewayConfig) -> Vec<String> {
    let quote = |value: &str| serde_json::to_string(value).expect("string JSON is TOML compatible");
    vec![
        format!("model_provider={}", quote(&gateway.provider_id)),
        format!(
            "model_providers.{}.base_url={}",
            gateway.provider_id,
            quote(&gateway.base_url)
        ),
        format!(
            "model_providers.{}.wire_api=\"responses\"",
            gateway.provider_id
        ),
        format!(
            "model_providers.{}.env_key={}",
            gateway.provider_id,
            quote(&gateway.credential_env)
        ),
        format!(
            "model_providers.{}.requires_openai_auth=false",
            gateway.provider_id
        ),
        format!(
            "shell_environment_policy.exclude=[{}]",
            quote(&gateway.credential_env)
        ),
    ]
}

struct TurnTerminal {
    status: String,
    final_message: Option<String>,
    validation_evidence: Vec<CodingValidationEvidence>,
}

async fn drive_turn<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    thread_id: &str,
    turn_id: &str,
    mut cancel: tokio::sync::watch::Receiver<Option<String>>,
) -> Result<TurnTerminal> {
    let mut interrupt_sent = false;
    let mut final_message = None;
    let mut validation_evidence = Vec::new();
    loop {
        let line = tokio::select! {
            frame = read_app_server_frame(stdout) => frame?,
            changed = cancel.changed(), if !interrupt_sent => {
                if changed.is_ok() && cancel.borrow().is_some() {
                    request(stdin, 99, "turn/interrupt", json!({"threadId":thread_id,"turnId":turn_id})).await?;
                    interrupt_sent = true;
                    continue;
                }
                continue;
            }
        };
        let Some(value) = line else { break };
        if let (Some(id), Some(method)) =
            (value.get("id"), value.get("method").and_then(Value::as_str))
        {
            emit(
                writer,
                identity,
                sequence,
                RuntimeEventPayload::ApprovalRequested {
                    request_id: id.to_string(),
                    kind: method.to_owned(),
                    subject: value.get("params").cloned().unwrap_or(Value::Null),
                },
            )
            .await?;
            let decision = if matches!(
                method,
                "item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
                    | "applyPatchApproval"
                    | "execCommandApproval"
            ) {
                json!({"decision":"cancel"})
            } else {
                json!({"error":{"code":-32601,"message":"worker does not grant interactive requests"}})
            };
            let response = if decision.get("error").is_some() {
                json!({"id":id,"error":decision["error"]})
            } else {
                json!({"id":id,"result":decision})
            };
            write_json(stdin, &response).await?;
            continue;
        }
        match value.get("method").and_then(Value::as_str) {
            Some("item/completed")
                if value.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
                    && value.pointer("/params/turnId").and_then(Value::as_str) == Some(turn_id)
                    && value.pointer("/params/item/type").and_then(Value::as_str)
                        == Some("agentMessage") =>
            {
                final_message = value
                    .pointer("/params/item/text")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("item/completed")
                if value.pointer("/params/threadId").and_then(Value::as_str) == Some(thread_id)
                    && value.pointer("/params/turnId").and_then(Value::as_str) == Some(turn_id)
                    && value.pointer("/params/item/type").and_then(Value::as_str)
                        == Some("commandExecution") =>
            {
                let command = value
                    .pointer("/params/item/command")
                    .and_then(Value::as_str)
                    .context("completed command execution has no command")?;
                let exit_code = value
                    .pointer("/params/item/exitCode")
                    .and_then(Value::as_i64)
                    .and_then(|code| i32::try_from(code).ok());
                let status = match exit_code {
                    Some(0) => ValidationStatus::Passed,
                    Some(_) => ValidationStatus::Failed,
                    None => ValidationStatus::NotRun,
                };
                let output_digest = value
                    .pointer("/params/item/aggregatedOutput")
                    .and_then(Value::as_str)
                    .map(patch_digest);
                validation_evidence.push(CodingValidationEvidence {
                    command: vec![command.to_owned()],
                    status,
                    exit_code,
                    artifact_digest: output_digest,
                });
            }
            Some(
                "item/started"
                | "item/agentMessage/delta"
                | "item/commandExecution/outputDelta"
                | "item/fileChange/outputDelta",
            ) => {
                emit(
                    writer,
                    identity,
                    sequence,
                    RuntimeEventPayload::Progress {
                        message: value["method"].as_str().unwrap().to_owned(),
                    },
                )
                .await?;
            }
            Some("thread/tokenUsage/updated") => {
                let total = &value["params"]["tokenUsage"]["total"];
                emit(
                    writer,
                    identity,
                    sequence,
                    RuntimeEventPayload::Usage {
                        input_tokens: u64_at(total, "inputTokens"),
                        cached_input_tokens: u64_at(total, "cachedInputTokens"),
                        output_tokens: u64_at(total, "outputTokens"),
                        reasoning_output_tokens: u64_at(total, "reasoningOutputTokens"),
                        total_tokens: u64_at(total, "totalTokens"),
                        authoritative: false,
                    },
                )
                .await?;
            }
            Some("turn/completed")
                if value.pointer("/params/turn/id").and_then(Value::as_str) == Some(turn_id) =>
            {
                return Ok(TurnTerminal {
                    status: value
                        .pointer("/params/turn/status")
                        .and_then(Value::as_str)
                        .unwrap_or("failed")
                        .to_owned(),
                    final_message,
                    validation_evidence,
                });
            }
            Some("error") => bail!("Codex App Server error: {}", value["params"]),
            _ => {}
        }
    }
    bail!("Codex App Server closed before turn completion")
}

fn thread_open_params(
    spec: &CodingTurnSpec,
    cwd: &Path,
    thread_id: Option<&str>,
) -> (&'static str, Value) {
    let mut params = thread_start_params(spec, cwd);
    if let Some(thread_id) = thread_id {
        params.as_object_mut().unwrap().remove("ephemeral");
        params.as_object_mut().unwrap().remove("serviceName");
        params["threadId"] = json!(thread_id);
        ("thread/resume", params)
    } else {
        ("thread/start", params)
    }
}

/// `thread/archive` is a trivial local RPC on an App Server that has just completed a
/// turn. It is bounded because a server that accepts the request and then never answers
/// would otherwise hold the worker until the runner kills it at the execution deadline,
/// destroying a result that was already earned.
const ARCHIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Kept back from the archive budget for emitting the terminal event and for the runner
/// to read and journal it. The runner kills the worker at the deadline without draining
/// stdout, so time spent archiving past this margin buys nothing and costs the result.
const DELIVERY_RESERVE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the optional post-turn archive may take.
///
/// The turn's result is already committed and is worth more than the cleanup, so the
/// budget is whatever is left after reserving delivery time — never the flat timeout when
/// the lease is nearly spent. `None` means the archive must be skipped outright: there is
/// no time to both close and deliver, and delivery wins.
fn archive_budget(deadline: Option<tokio::time::Instant>) -> Option<std::time::Duration> {
    let Some(deadline) = deadline else {
        // A runner older than `deadlineMs` tells us nothing; fall back to the flat bound.
        return Some(ARCHIVE_TIMEOUT);
    };
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    let budget = remaining
        .saturating_sub(DELIVERY_RESERVE)
        .min(ARCHIVE_TIMEOUT);
    (!budget.is_zero()).then_some(budget)
}

/// Commits the checkpoint for a turn that has already succeeded.
///
/// The order here is the contract. `finish` runs first, so the turn's result is durable
/// before any optional bookkeeping is attempted; only then is `closeAfterTurn` honoured,
/// under a timeout. An archive that fails, or never answers, therefore leaves a committed
/// READY checkpoint plus a `closeError` in the receipt: the result stands, the thread is
/// still resumable, and the workflow can close it with a turn of its own.
///
/// A dedicated `mode: close` turn is handled separately in `run`, where the archive is
/// the entire point of the turn and a failure must fail it.
async fn finish_thread(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    spec: &CodingTurnSpec,
    session: &mut Option<super::coding_session::CodingSession>,
    thread_id: &str,
    patch: &str,
    deadline: Option<tokio::time::Instant>,
) -> Result<Value> {
    let Some(session) = session else {
        return Ok(Value::Null);
    };
    // Durable before optional: nothing below may cost the caller this result.
    let receipt = session.finish(thread_id, patch, false)?;
    if !spec.thread.as_ref().is_some_and(|t| t.close_after_turn) {
        return Ok(receipt);
    }
    let close = match archive_budget(deadline) {
        None => Err(
            "skipped: too little execution budget left to close the thread and still deliver \
             the result"
                .to_string(),
        ),
        Some(budget) => {
            match tokio::time::timeout(budget, archive_thread(stdin, stdout, thread_id)).await {
                Ok(Ok(())) => session.mark_closed().map_err(|error| error.to_string()),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err(format!(
                    "thread/archive did not answer within {}s",
                    budget.as_secs_f32()
                )),
            }
        }
    };
    Ok(match close {
        Ok(closed) => closed,
        // The thread is recorded as it can be proven to be. If the archive itself
        // succeeded and only the local write failed, a later resume fails against Codex
        // rather than this turn losing a completed implementation.
        Err(error) => {
            let mut receipt = receipt;
            receipt["closeError"] = json!(error);
            receipt
        }
    })
}

async fn archive_thread(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut BufReader<tokio::process::ChildStdout>,
    thread_id: &str,
) -> Result<()> {
    request(stdin, 5, "thread/archive", json!({"threadId":thread_id})).await?;
    response(stdout, 5).await?;
    Ok(())
}

fn thread_start_params(spec: &CodingTurnSpec, cwd: &Path) -> Value {
    let mut value = json!({
        "cwd": cwd,
        "approvalPolicy": "never",
        "sandbox": "workspace-write",
        "serviceName": "light-agent-worker",
        "ephemeral": spec.thread.is_none()
    });
    // Only the enterprise route resolves a Light model alias. A personal subscription
    // runs the account's own default model, so role separation between implement and
    // review rests on the prompt, allowed tools and workspace authority pinned by
    // CodingRoleExecutionProfile, not on the model alias.
    if spec.authentication_profile == CodingAuthenticationProfile::EnterpriseApi {
        value["model"] = Value::String(spec.model_alias.clone());
    }
    value
}

fn turn_start_params(
    spec: &CodingTurnSpec,
    thread_id: &str,
    repository: &Path,
    cwd: &Path,
) -> Value {
    let mut prompt = format!(
        "Repository for this turn: {}. It has been reconstructed from the admitted immutable base and candidate/checkpoint patch. Paths and tool results from earlier turns may be stale; use this repository and verify current contents.\n\n{}",
        repository.display(),
        spec.prompt
    );
    let mut value = json!({
        "threadId": thread_id,
        "input": [{"type":"text","text":prompt,"text_elements":[]}],
        "cwd": cwd,
        "approvalPolicy": "never",
    });
    if spec.authentication_profile == CodingAuthenticationProfile::EnterpriseApi {
        value["model"] = Value::String(spec.model_alias.clone());
    }
    if spec.role == CodingRole::Implement
        && let Some(remediation) = &spec.remediation
    {
        let findings = serde_json::to_string(&remediation.prior_review.findings)
            .expect("review findings are serializable");
        prompt = format!(
            "Remediate every accepted finding in this implementation turn. Prior artifact: {}. Findings: {}\n\n{}",
            remediation.prior_review.artifact_digest, findings, prompt
        );
        value["input"][0]["text"] = Value::String(prompt.clone());
    }
    if spec.role == CodingRole::Implement {
        value["sandboxPolicy"] = json!({
            "type": "workspaceWrite",
            "writableRoots": materialized_writable_roots(spec, repository)
                .expect("validated writable roots map into the materialized repository"),
            "networkAccess": false,
            "excludeSlashTmp": true,
            "excludeTmpdirEnvVar": false
        });
    }
    if spec.role == CodingRole::Review {
        let review = spec
            .review_input
            .as_deref()
            .expect("validated review input");
        let evidence = serde_json::to_string(&review.implementation.validation_evidence)
            .expect("validation evidence is serializable");
        let prior =
            serde_json::to_string(&review.prior_review).expect("prior review is serializable");
        prompt = format!(
            "Review the immutable candidate repository at {}. Return only the required JSON result for review {} bound to artifact {}. Requirements digest: {}.\n\nRequirements:\n{}\n\nImplementer validation evidence:\n{}\n\nPrior finding ledger:\n{}\n\nReview task:\n{}",
            repository.display(),
            review.review_id,
            review.implementation.patch_digest,
            review.requirements_digest,
            review.requirements,
            evidence,
            prior,
            spec.prompt
        );
        value["input"][0]["text"] = Value::String(prompt);
        value["sandboxPolicy"] = json!({
            "type": "workspaceWrite",
            "writableRoots": [cwd],
            "networkAccess": false,
            "excludeSlashTmp": true,
            "excludeTmpdirEnvVar": false
        });
        value["outputSchema"] = coding_review_output_schema();
    }
    value
}

fn coding_review_output_schema() -> Value {
    json!({
        "type":"object",
        "additionalProperties":false,
        "required":["schemaVersion","reviewId","artifactDigest","verdict","findings","validationGaps"],
        "properties":{
            "schemaVersion":{"type":"integer","const":1},
            "reviewId":{"type":"string"},
            "artifactDigest":{"type":"string"},
            "verdict":{"type":"string","enum":["approved","changes-required"]},
            "findings":{"type":"array","items":{
                "type":"object","additionalProperties":false,
                "required":["findingId","severity","repository","location","summary","evidence","requiredResolution"],
                "properties":{
                    "findingId":{"type":"string"},
                    "severity":{"type":"string","enum":["low","medium","high","critical"]},
                    "repository":{"type":"string"},
                    "location":{"type":"string"},
                    "summary":{"type":"string"},
                    "evidence":{"type":"string"},
                    "requiredResolution":{"type":"string"}
                }
            }},
            "validationGaps":{"type":"array","items":{"type":"string"}}
        }
    })
}

async fn git_apply(repository: &Path, patch: &[u8]) -> Result<()> {
    let mut command = hardened_git_command();
    let mut child = command
        .args(["apply", "--binary", "--whitespace=nowarn", "-"])
        .current_dir(repository)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .context("git apply stdin unavailable")?
        .write_all(patch)
        .await?;
    if !child.wait().await?.success() {
        bail!("candidate patch cannot be reconstructed from immutable base")
    }
    Ok(())
}

async fn checkout_base(repository: &Path, base_revision: &str) -> Result<()> {
    git(
        ["checkout", "--quiet"],
        None,
        Path::new(base_revision),
        Some(repository),
    )
    .await
}

async fn verify_candidate_unchanged(repository: &Path, admitted_patch: &str) -> Result<()> {
    git_simple(repository, &["add", "-N", "."]).await?;
    let actual = canonical_git_diff(repository).await?;
    if patch_digest(&actual) != patch_digest(admitted_patch) || actual != admitted_patch {
        bail!("reviewer candidate repository was mutated")
    }
    Ok(())
}

pub(super) async fn validate_contract(
    contract: &CodingAdapterContract,
    qualification: &CodingAdapterQualification,
) -> Result<()> {
    validate_contract_metadata(contract, qualification)?;
    let executable = std::env::var_os("LIGHT_CODEX_EXECUTABLE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&contract.executable));
    let bytes = tokio::fs::read(executable)
        .await
        .context("read pinned Codex binary")?;
    let actual = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if actual != contract.binary_digest {
        bail!("Codex binary digest mismatch")
    }
    Ok(())
}

fn validate_contract_metadata(
    contract: &CodingAdapterContract,
    qualification: &CodingAdapterQualification,
) -> Result<()> {
    contract.validate()?;
    if contract.adapter_id != CODEX_APP_SERVER_ADAPTER_ID
        || contract.adapter_version != CODEX_APP_SERVER_VERSION
        || contract.adapter_protocol_version != CODEX_APP_SERVER_PROTOCOL_VERSION
        || contract.binary_digest != CODEX_APP_SERVER_BINARY_DIGEST
        || contract.schema_digest != CODEX_APP_SERVER_SCHEMA_DIGEST
    {
        bail!("Codex binary or generated schema version mismatch")
    }
    if qualification.evidence_digest != CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST {
        bail!("Codex qualification evidence digest mismatch")
    }
    qualification.require_selectable(contract)?;
    Ok(())
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value.get(key).with_context(|| format!("{key} is required"))
}
fn string_at<'a>(value: &'a Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .with_context(|| format!("missing App Server field {pointer}"))
}
fn u64_at(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}
async fn request(
    stdin: &mut tokio::process::ChildStdin,
    id: u64,
    method: &str,
    params: Value,
) -> Result<()> {
    write_json(stdin, &json!({"id":id,"method":method,"params":params})).await
}
async fn notification(stdin: &mut tokio::process::ChildStdin, method: &str) -> Result<()> {
    write_json(stdin, &json!({"method":method})).await
}
async fn write_json(stdin: &mut tokio::process::ChildStdin, value: &Value) -> Result<()> {
    stdin.write_all(&serde_json::to_vec(value)?).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await?;
    Ok(())
}
async fn response(stdout: &mut BufReader<tokio::process::ChildStdout>, id: u64) -> Result<Value> {
    while let Some(value) = read_app_server_frame(stdout).await? {
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            if value.get("error").is_some() {
                bail!("App Server request {id} failed: {}", value["error"]);
            }
            return Ok(value);
        }
    }
    bail!("App Server closed before response {id}")
}

async fn read_app_server_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Option<Value>> {
    let mut bounded = reader.take((MAX_FRAME_BYTES as u64).saturating_add(1));
    let mut frame = Vec::with_capacity(8 * 1024);
    let bytes = bounded.read_until(b'\n', &mut frame).await?;
    if bytes == 0 {
        return Ok(None);
    }
    if frame.last() != Some(&b'\n') || frame.len() > MAX_FRAME_BYTES {
        bail!("Codex App Server frame exceeds the admitted limit")
    }
    frame.pop();
    let value = serde_json::from_slice(&frame).context("invalid Codex JSONL frame")?;
    Ok(Some(value))
}
async fn shutdown(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}
async fn git<const N: usize>(
    prefix: [&str; N],
    source: Option<&Path>,
    tail: &Path,
    cwd: Option<&Path>,
) -> Result<()> {
    let mut command = hardened_git_command();
    command.args(prefix);
    if let Some(source) = source {
        command.arg(source);
    }
    command.arg(tail);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let status = command.status().await?;
    if !status.success() {
        bail!("git command failed: {status}")
    }
    Ok(())
}
async fn git_simple(cwd: &Path, args: &[&str]) -> Result<()> {
    let status = hardened_git_command()
        .args(args)
        .current_dir(cwd)
        .status()
        .await?;
    if !status.success() {
        bail!("git command failed: {status}")
    }
    Ok(())
}
async fn git_output(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = hardened_git_command()
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        bail!("git command failed: {}", output.status)
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn hardened_git_command() -> Command {
    let mut command = Command::new(GIT_EXECUTABLE);
    command
        .env_clear()
        .env("HOME", "/nonexistent")
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    command
}

async fn canonical_git_diff(repository: &Path) -> Result<String> {
    git_output(
        repository,
        &[
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
        ],
    )
    .await
}

fn materialized_writable_roots(spec: &CodingTurnSpec, repository: &Path) -> Result<Vec<String>> {
    let workspace = spec.workspace_root.trim_end_matches('/');
    spec.writable_roots
        .iter()
        .map(|root| {
            let suffix = root
                .strip_prefix(workspace)
                .filter(|suffix| suffix.is_empty() || suffix.starts_with('/'))
                .context("writable root is outside the admitted workspace")?;
            let relative = suffix.trim_start_matches('/');
            Ok(repository.join(relative).to_string_lossy().into_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review_spec(patch: &str) -> CodingTurnSpec {
        let implementation = coding_agent_runtime::CodingImplementationArtifact {
            schema_version: coding_agent_runtime::CODING_ARTIFACT_SCHEMA_VERSION,
            adapter_contract_digest: format!("sha256:{}", "1".repeat(64)),
            repository_digest: format!("sha256:{}", "2".repeat(64)),
            base_revision: "a".repeat(40),
            patch_digest: patch_digest(patch),
            changed_paths: std::collections::BTreeSet::from(["src/lib.rs".into()]),
            validation_evidence: Vec::new(),
            resolved_finding_ids: std::collections::BTreeSet::new(),
        };
        CodingTurnSpec {
            thread: None,
            repository_digest: implementation.repository_digest.clone(),
            base_revision: implementation.base_revision.clone(),
            workspace_root: "/workspace/repository".into(),
            prompt: "perform an independent review".into(),
            model_alias: coding_agent_runtime::CODING_REVIEWER_ALIAS.into(),
            authentication_profile:
                coding_agent_runtime::CodingAuthenticationProfile::EnterpriseApi,
            role: CodingRole::Review,
            role_profile: coding_agent_runtime::CodingRoleExecutionProfile::pinned(
                CodingRole::Review,
            ),
            review_input: Some(Box::new(coding_agent_runtime::CodingReviewInput {
                review_id: "review-1".into(),
                repository: "networknt/light-fabric".into(),
                requirements: "preserve behavior".into(),
                requirements_digest: patch_digest("preserve behavior"),
                candidate_patch: patch.into(),
                implementation,
                prior_review: None,
            })),
            remediation: None,
            materialization_manifest_digest: format!("sha256:{}", "3".repeat(64)),
            writable_roots: std::collections::BTreeSet::from(["/workspace/review-scratch".into()]),
            allowed_tools: std::collections::BTreeSet::from([
                "fs.read".into(),
                "process.exec".into(),
            ]),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 10,
        }
    }

    fn persistent_spec() -> CodingTurnSpec {
        let mut spec = review_spec("");
        spec.role = CodingRole::Implement;
        spec.role_profile =
            coding_agent_runtime::CodingRoleExecutionProfile::pinned(CodingRole::Implement);
        spec.model_alias = spec.role_profile.model_alias.clone();
        spec.review_input = None;
        spec.allowed_tools = CodingTurnSpec::supported_tools(CodingRole::Implement);
        spec.writable_roots = std::collections::BTreeSet::from([spec.workspace_root.clone()]);
        spec.authentication_profile = CodingAuthenticationProfile::PersonalSubscription;
        spec.thread = Some(coding_agent_runtime::CodingThreadControl {
            runner_id: "personal-codex-runner".into(),
            session_ref: uuid::Uuid::now_v7(),
            stage_id: "implementation-1".into(),
            mode: coding_agent_runtime::CodingThreadMode::New,
            expected_checkpoint: None,
            close_after_turn: false,
        });
        spec
    }

    #[test]
    fn thread_checkpoints_survive_restart_reject_stale_scope_and_close() {
        use super::super::coding_session::CodingSession;
        use coding_agent_runtime::CodingThreadMode;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let mut spec = persistent_spec();
        spec.validate().unwrap();
        let mut first = CodingSession::open(home.path(), &scope, &spec, "contract-a").unwrap();
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-a").is_err());
        let receipt = first
            .finish("native-thread-a", "candidate-patch", false)
            .unwrap();
        drop(first);
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-a").is_err());
        spec.thread.as_mut().unwrap().mode = CodingThreadMode::Resume;
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
        for change in ["stage", "role", "base", "tools"] {
            let mut changed = spec.clone();
            match change {
                "stage" => changed.thread.as_mut().unwrap().stage_id = "next-stage".into(),
                "role" => changed.role = CodingRole::Review,
                "base" => changed.base_revision = "b".repeat(40),
                _ => {
                    changed.allowed_tools.insert("fs.delete".into());
                }
            }
            assert!(CodingSession::open(home.path(), &scope, &changed, "contract-a").is_err());
        }
        assert!(
            CodingSession::open(
                home.path(),
                &format!("sha256:{}", "b".repeat(64)),
                &spec,
                "contract-a"
            )
            .is_err()
        );
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-b").is_err());
        let mut resumed = CodingSession::open(home.path(), &scope, &spec, "contract-a").unwrap();
        assert_eq!(resumed.thread_id(), Some("native-thread-a"));
        assert_eq!(resumed.patch(), "candidate-patch");
        let receipt2 = resumed
            .finish("native-thread-a", "updated-patch", false)
            .unwrap();
        drop(resumed);
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-a").is_err());
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(receipt2["checkpoint"].clone()).unwrap());
        let mut closing = CodingSession::open(home.path(), &scope, &spec, "contract-a").unwrap();
        let closed = closing.finish("native-thread-a", "", true).unwrap();
        drop(closing);
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(closed["checkpoint"].clone()).unwrap());
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-a").is_err());
        spec.thread.as_mut().unwrap().session_ref = uuid::Uuid::now_v7();
        spec.thread.as_mut().unwrap().mode = CodingThreadMode::New;
        spec.thread.as_mut().unwrap().expected_checkpoint = None;
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract-a").is_ok());
    }

    #[test]
    fn interrupted_session_cannot_silently_resume_or_restart() {
        use super::super::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let mut spec = persistent_spec();
        let mut session = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        session.begin().unwrap();
        let receipt = session.finish("native-a", "patch", false).unwrap();
        drop(session);
        spec.thread.as_mut().unwrap().mode = coding_agent_runtime::CodingThreadMode::Resume;
        spec.thread.as_mut().unwrap().expected_checkpoint =
            Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());

        // A preflight rejection releases the thread untouched: it never claimed the
        // native operation, so the very same resume still succeeds afterwards.
        drop(CodingSession::open(home.path(), &scope, &spec, "contract").unwrap());
        let mut claimed = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();

        // Once the native attempt is claimed, an interruption is indistinguishable from
        // a lost one and neither resume nor restart may silently continue it.
        claimed.begin().unwrap();
        drop(claimed);
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
        spec.thread.as_mut().unwrap().mode = coding_agent_runtime::CodingThreadMode::New;
        spec.thread.as_mut().unwrap().expected_checkpoint = None;
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
    }

    #[test]
    fn retention_reclaims_only_long_closed_threads() {
        use super::super::coding_session::CodingSession;
        use coding_agent_runtime::CodingThreadMode;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let root = home.path().join("light-worker-threads");
        let age = |state: &std::path::Path, old: bool| {
            let when = if old {
                std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60)
            } else {
                std::time::SystemTime::now()
            };
            let file = std::fs::OpenOptions::new().write(true).open(state).unwrap();
            file.set_times(std::fs::FileTimes::new().set_modified(when))
                .unwrap();
        };
        let checkpoint = |root: &std::path::Path| {
            std::fs::read_dir(root)
                .unwrap()
                .flatten()
                .map(|entry| entry.path())
                .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        };

        // A live (IN_FLIGHT) checkpoint is never reclaimed, however old the file is.
        let mut spec = persistent_spec();
        let mut open = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        open.begin().unwrap();
        let in_flight = checkpoint(&root).unwrap();
        age(&in_flight, true);
        // persistent_spec() mints a fresh sessionRef, so reuse `spec` to hit this key.
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
        assert!(in_flight.exists());

        // A recently closed checkpoint is retained so a late resume is still rejected.
        open.finish("native-a", "", true).unwrap();
        drop(open);
        age(&in_flight, false);
        spec.thread.as_mut().unwrap().mode = CodingThreadMode::Resume;
        spec.thread.as_mut().unwrap().expected_checkpoint = Some(uuid::Uuid::now_v7());
        assert!(CodingSession::open(home.path(), &scope, &spec, "contract").is_err());
        assert!(in_flight.exists());

        // Past the retention window the closed checkpoint is reclaimed by the next
        // unrelated open, so the owner's Codex home does not grow without bound. The lock
        // file must survive: flock ownership is per-inode, so unlinking it would let two
        // processes lock different inodes and both believe they own the thread.
        age(&in_flight, true);
        let lock = in_flight.with_extension("lock");
        age(&lock, true);
        let mut unrelated = persistent_spec();
        unrelated.thread.as_mut().unwrap().session_ref = uuid::Uuid::now_v7();
        drop(CodingSession::open(home.path(), &scope, &unrelated, "contract").unwrap());
        assert!(!in_flight.exists(), "closed checkpoint was not reclaimed");
        assert!(lock.exists(), "lock identity must outlive the checkpoint");

        // Reclamation also frees the sessionRef for a genuinely fresh conversation.
        spec.thread.as_mut().unwrap().mode = CodingThreadMode::New;
        spec.thread.as_mut().unwrap().expected_checkpoint = None;
        drop(CodingSession::open(home.path(), &scope, &spec, "contract").unwrap());
    }

    /// A server that accepts `thread/archive` and then simply never answers, holding
    /// stdout open. Nothing but a timeout distinguishes this from a slow success.
    ///
    /// `sleep` is the fake precisely because it never reads and never writes: the request
    /// lands in the pipe buffer and is never consumed, and the stdout pipe stays open for
    /// the process lifetime without ever producing a frame. An echoing stand-in such as
    /// `cat` would be no use here, since it answers `id: 5` with the request itself.
    fn unresponsive_app_server() -> (
        tokio::process::Child,
        tokio::process::ChildStdin,
        BufReader<tokio::process::ChildStdout>,
    ) {
        let mut child = Command::new("/bin/sleep")
            .arg("300")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn stand-in App Server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        (child, stdin, stdout)
    }

    #[tokio::test(start_paused = true)]
    async fn a_nearly_spent_lease_skips_the_archive_rather_than_the_delivery() {
        use super::super::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let mut spec = persistent_spec();
        spec.thread.as_mut().unwrap().close_after_turn = true;
        let mut session =
            Some(CodingSession::open(home.path(), &scope, &spec, "contract").unwrap());
        session.as_mut().unwrap().begin().unwrap();
        let (mut child, mut stdin, mut stdout) = unresponsive_app_server();

        // Two seconds left: less than the delivery reserve, so there is no budget to both
        // close the thread and hand back the result.
        let deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_millis(2_000));
        assert!(archive_budget(deadline).is_none());
        let started = tokio::time::Instant::now();
        let receipt = finish_thread(
            &mut stdin,
            &mut stdout,
            &spec,
            &mut session,
            "native-a",
            "the-earned-patch",
            deadline,
        )
        .await
        .unwrap();
        let _ = child.kill().await;

        // It returned without ever waiting on the unresponsive server, leaving the whole
        // remaining lease for the terminal event.
        assert!(
            tokio::time::Instant::now().duration_since(started)
                < std::time::Duration::from_millis(2_000)
        );
        assert_eq!(receipt["state"], json!("READY"));
        assert!(
            receipt["closeError"]
                .as_str()
                .is_some_and(|error| error.contains("skipped")),
            "the workflow must be told the close was never attempted: {receipt}"
        );

        // The budget scales with what is left, and is never more than the flat bound.
        let far = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
        assert_eq!(archive_budget(Some(far)), Some(ARCHIVE_TIMEOUT));
        let tight =
            tokio::time::Instant::now() + DELIVERY_RESERVE + std::time::Duration::from_secs(3);
        assert_eq!(
            archive_budget(Some(tight)),
            Some(std::time::Duration::from_secs(3))
        );
        assert!(archive_budget(Some(tokio::time::Instant::now())).is_none());
        // A runner too old to send a budget falls back to the flat bound.
        assert_eq!(archive_budget(None), Some(ARCHIVE_TIMEOUT));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_archive_still_delivers_the_committed_result() {
        use super::super::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let mut spec = persistent_spec();
        spec.thread.as_mut().unwrap().close_after_turn = true;
        let mut session =
            Some(CodingSession::open(home.path(), &scope, &spec, "contract").unwrap());
        session.as_mut().unwrap().begin().unwrap();
        let (mut child, mut stdin, mut stdout) = unresponsive_app_server();

        let receipt = finish_thread(
            &mut stdin,
            &mut stdout,
            &spec,
            &mut session,
            "native-a",
            "the-earned-patch",
            None,
        )
        .await
        .expect("a hung archive must not fail the turn");
        let _ = child.kill().await;

        // The turn's result is delivered, and the thread is reported exactly as it can be
        // proven to be: still open, with the reason the close did not happen.
        assert_eq!(receipt["state"], json!("READY"));
        assert_eq!(
            receipt["sessionRef"],
            json!(spec.thread.unwrap().session_ref)
        );
        assert!(
            receipt["checkpoint"]
                .as_str()
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .is_some()
        );
        assert!(
            receipt["closeError"]
                .as_str()
                .is_some_and(|error| error.contains("did not answer")),
            "the workflow must be told why the thread is still open: {receipt}"
        );

        // Committed before the wait: a kill at this point still leaves the patch and a
        // resumable checkpoint on disk, not an IN_FLIGHT thread the workflow must abandon.
        drop(session);
        let stored: Value = serde_json::from_slice(
            &std::fs::read(
                std::fs::read_dir(home.path().join("light-worker-threads"))
                    .unwrap()
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(stored["state"], json!("READY"));
        assert_eq!(stored["patch"], json!("the-earned-patch"));
        assert_eq!(stored["threadId"], json!("native-a"));
        assert_eq!(stored["checkpoint"], receipt["checkpoint"]);
    }

    #[test]
    fn cleanup_never_reclaims_a_thread_another_holder_owns() {
        use super::super::coding_session::CodingSession;
        let home = tempfile::tempdir().unwrap();
        let scope = format!("sha256:{}", "a".repeat(64));
        let root = home.path().join("light-worker-threads");
        let spec = persistent_spec();

        // An expired CLOSED record whose lock is still held. This is the observation a
        // concurrent cleanup pass races on: it looks reclaimable, but its holder may
        // replace it at any moment.
        let mut held = CodingSession::open(home.path(), &scope, &spec, "contract").unwrap();
        held.begin().unwrap();
        held.finish("native-a", "", true).unwrap();
        let checkpoint = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
            .unwrap();
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&checkpoint)
            .unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(
            std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60),
        ))
        .unwrap();

        // Cleanup runs on every open. While the lock is held it must leave the record
        // alone, so the holder's next write cannot be destroyed by a stale observation.
        let mut unrelated = persistent_spec();
        unrelated.thread.as_mut().unwrap().session_ref = uuid::Uuid::now_v7();
        drop(CodingSession::open(home.path(), &scope, &unrelated, "contract").unwrap());
        assert!(
            checkpoint.exists(),
            "cleanup reclaimed a thread whose lock was held"
        );

        // Released, the same record is reclaimable.
        drop(held);
        let mut next = persistent_spec();
        next.thread.as_mut().unwrap().session_ref = uuid::Uuid::now_v7();
        drop(CodingSession::open(home.path(), &scope, &next, "contract").unwrap());
        assert!(!checkpoint.exists());
    }

    #[test]
    fn persistent_thread_protocol_matches_pinned_schema() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../contracts/codex-app-server/v0.153.4/json/ClientRequest.json"
        ))
        .unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        let spec = persistent_spec();
        for previous in [None, Some("native-a")] {
            let (method, params) =
                thread_open_params(&spec, Path::new("/workspace/repository"), previous);
            let request = json!({"id":3,"method":method,"params":params});
            assert!(validator.is_valid(&request), "{request}");
            if previous.is_none() {
                assert_eq!(params["ephemeral"], false);
            } else {
                assert_eq!(method, "thread/resume");
                assert!(params.get("ephemeral").is_none());
            }
        }
        assert!(
            validator.is_valid(
                &json!({"id":5,"method":"thread/archive","params":{"threadId":"native-a"}})
            )
        );
    }

    #[test]
    fn outbound_protocol_shapes_match_the_pinned_generated_schema() {
        let schema: Value = serde_json::from_str(include_str!(
            "../../../contracts/codex-app-server/v0.153.4/json/ClientRequest.json"
        ))
        .unwrap();
        let validator = jsonschema::Validator::new(&schema).unwrap();
        for request in [
            json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"light-agent-worker","title":"Light Agent Worker","version":"0.2.1"},"capabilities":{"experimentalApi":false,"requestAttestation":false}}}),
            json!({"id":2,"method":"account/read","params":{"refreshToken":false}}),
            json!({"id":3,"method":"thread/start","params":{"model":"coding-implementer","cwd":"/workspace/repository","approvalPolicy":"never","sandbox":"workspace-write","serviceName":"light-agent-worker","ephemeral":true}}),
            json!({"id":4,"method":"turn/start","params":{"threadId":"thread-1","input":[{"type":"text","text":"implement","text_elements":[]}],"cwd":"/workspace/repository","approvalPolicy":"never","model":"coding-implementer"}}),
            json!({"id":99,"method":"turn/interrupt","params":{"threadId":"thread-1","turnId":"turn-1"}}),
        ] {
            assert!(validator.is_valid(&request), "invalid request: {request}");
        }
        let review_patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let review = review_spec(review_patch);
        let review_request = json!({
            "id": 5,
            "method": "turn/start",
            "params": turn_start_params(
                &review,
                "fresh-review-thread",
                Path::new("/isolated/repository"),
                Path::new("/isolated/scratch")
            )
        });
        assert!(
            validator.is_valid(&review_request),
            "invalid review request: {review_request}"
        );
        let notification_schema: Value = serde_json::from_str(include_str!(
            "../../../contracts/codex-app-server/v0.153.4/json/ClientNotification.json"
        ))
        .unwrap();
        assert!(
            jsonschema::Validator::new(&notification_schema)
                .unwrap()
                .is_valid(&json!({"method":"initialized"}))
        );
    }

    #[test]
    fn personal_subscription_uses_the_native_codex_default_model() {
        let mut spec = review_spec(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n",
        );
        spec.authentication_profile = CodingAuthenticationProfile::PersonalSubscription;
        let thread = thread_start_params(&spec, Path::new("/isolated/scratch"));
        let turn = turn_start_params(
            &spec,
            "fresh-review-thread",
            Path::new("/isolated/repository"),
            Path::new("/isolated/scratch"),
        );
        assert!(thread.get("model").is_none());
        assert!(turn.get("model").is_none());
    }

    #[test]
    fn pinned_contract_rejects_schema_drift() {
        let mut contract = CodingAdapterContract {
            schema_version: 1,
            adapter_id: CODEX_APP_SERVER_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            adapter_protocol_version: CODEX_APP_SERVER_PROTOCOL_VERSION.into(),
            action_kind: "coding.codex-app-server-v1".into(),
            compatibility_digest: format!("sha256:{:064x}", 1),
            image_digest: format!("sha256:{:064x}", 2),
            capability_digest: format!("sha256:{:064x}", 3),
            template_id: "coding-codex-app-server-v1".into(),
            template_version: 1,
            template_digest: format!("sha256:{:064x}", 4),
            executable: "/usr/local/bin/codex".into(),
            binary_digest: CODEX_APP_SERVER_BINARY_DIGEST.into(),
            schema_digest: CODEX_APP_SERVER_SCHEMA_DIGEST.into(),
            required_features: std::collections::BTreeSet::from(["canonical-patch-output".into()]),
        };
        let qualification = CodingAdapterQualification {
            schema_version: coding_agent_runtime::CODING_ADAPTER_QUALIFICATION_VERSION,
            adapter_id: CODEX_APP_SERVER_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            status: coding_agent_runtime::CodingAdapterQualificationStatus::Qualified,
            evaluated_dimensions:
                coding_agent_runtime::CodingAdapterQualificationDimension::required(),
            contract_digest: Some(contract.digest().unwrap()),
            evidence_digest: CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST.into(),
        };
        contract.schema_digest = format!("sha256:{:064x}", 9);
        assert!(validate_contract_metadata(&contract, &qualification).is_err());
    }

    #[test]
    fn enterprise_config_exposes_only_gateway_alias_and_scrubs_attempt_token_from_shells() {
        let gateway = EnterpriseGatewayConfig {
            provider_id: "light_gateway".into(),
            base_url: "https://llm-gateway.example/v1".into(),
            credential_target: "llm-gateway-attempt".into(),
            credential_env: "LIGHT_LLM_ATTEMPT_TOKEN".into(),
            binding: agent_runtime_protocol::GatewayAttemptBinding {
                audience: "llm-gateway".into(),
                host_id: uuid::Uuid::new_v4(),
                end_user_subject: "user-1".into(),
                principal_subject: "user-1".into(),
                workload_actor: "light-agent/worker-1".into(),
                workflow_id: Some(uuid::Uuid::new_v4()),
                session_id: agent_core::AgentSessionId::new(),
                turn_id: agent_core::AgentTurnId::new(),
                action_attempt_id: agent_core::AgentActionAttemptId::new(),
                policy_digest: format!("sha256:{}", "1".repeat(64)),
                data_boundary_digest: format!("sha256:{}", "2".repeat(64)),
                route_alias: "coding-implementer".into(),
                billing_subject: "user-1".into(),
                budget_policy_id: "developer-default".into(),
                correlation_id: uuid::Uuid::new_v4(),
            },
        };
        let config = render_enterprise_config(&gateway);
        assert!(config.contains("wire_api = \"responses\""));
        assert!(config.contains("exclude = [\"LIGHT_LLM_ATTEMPT_TOKEN\"]"));
        assert!(config.contains("https://llm-gateway.example/v1"));
        assert!(!config.contains("attempt-only-token"));
        assert!(!config.contains("physical"));
        let arguments = enterprise_cli_overrides(&gateway);
        assert!(
            arguments
                .iter()
                .any(|value| value.contains("wire_api=\"responses\""))
        );
        assert!(
            arguments
                .iter()
                .all(|value| !value.contains("attempt-only-token"))
        );
        assert!(arguments.iter().all(|value| !value.contains("physical")));
    }

    #[test]
    fn authentication_status_is_profile_specific_and_metadata_only() {
        let personal = authentication_evidence(
            CodingAuthenticationProfile::PersonalSubscription,
            &json!({"result":{"requiresOpenaiAuth":true,"account":{"type":"chatgpt","email":"private@example.com","planType":"plus"}}}),
            None,
        )
        .unwrap();
        assert_eq!(
            personal.credential_source,
            CodingCredentialSource::NativeCodexStore
        );
        assert!(!personal.authoritative_usage);
        let serialized = serde_json::to_string(&personal).unwrap();
        assert!(!serialized.contains("private@example.com"));
        assert!(!serialized.contains("plus"));

        let enterprise = authentication_evidence(
            CodingAuthenticationProfile::EnterpriseApi,
            &json!({"result":{"requiresOpenaiAuth":false,"account":null}}),
            Some(4),
        )
        .unwrap();
        assert_eq!(
            enterprise.credential_source,
            CodingCredentialSource::AttemptBroker
        );
        assert!(enterprise.authoritative_usage);

        assert!(
            authentication_evidence(
                CodingAuthenticationProfile::PersonalSubscription,
                &json!({"result":{"requiresOpenaiAuth":true,"account":{"type":"apiKey"}}}),
                None,
            )
            .is_err()
        );
        assert!(
            authentication_evidence(
                CodingAuthenticationProfile::EnterpriseApi,
                &json!({"result":{"requiresOpenaiAuth":false,"account":{"type":"chatgpt"}}}),
                Some(1),
            )
            .is_err()
        );
    }

    #[test]
    fn reviewer_turn_uses_fresh_ephemeral_thread_structured_output_and_scratch_only_writes() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let spec = review_spec(patch);
        spec.validate().unwrap();
        let repository = Path::new("/isolated/review/repository");
        let scratch = Path::new("/isolated/review/scratch");
        let thread = thread_start_params(&spec, scratch);
        assert_eq!(thread["ephemeral"], true);
        assert_eq!(thread["model"], coding_agent_runtime::CODING_REVIEWER_ALIAS);
        assert_eq!(thread["cwd"], scratch.display().to_string());
        let turn = turn_start_params(&spec, "fresh-thread", repository, scratch);
        assert_eq!(turn["sandboxPolicy"]["writableRoots"], json!([scratch]));
        assert_eq!(turn["sandboxPolicy"]["networkAccess"], false);
        assert!(turn.get("outputSchema").is_some());
        assert!(
            turn["input"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Implementer validation evidence")
        );
        assert!(!turn.to_string().contains("implementerThread"));
    }

    #[test]
    fn implement_turn_maps_only_admitted_writable_roots() {
        let mut spec = review_spec(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n",
        );
        spec.role = CodingRole::Implement;
        spec.role_profile =
            coding_agent_runtime::CodingRoleExecutionProfile::pinned(CodingRole::Implement);
        spec.model_alias = coding_agent_runtime::CODING_IMPLEMENTER_ALIAS.into();
        spec.review_input = None;
        spec.writable_roots =
            std::collections::BTreeSet::from(["/workspace/repository/src".into()]);
        spec.allowed_tools = CodingTurnSpec::supported_tools(CodingRole::Implement);
        spec.validate().unwrap();
        let turn = turn_start_params(
            &spec,
            "implement-thread",
            Path::new("/isolated/repository"),
            Path::new("/isolated/repository"),
        );
        assert_eq!(
            turn["sandboxPolicy"]["writableRoots"],
            json!(["/isolated/repository/src"])
        );
        assert_eq!(turn["sandboxPolicy"]["networkAccess"], false);
    }

    #[tokio::test]
    async fn app_server_frames_are_bounded_before_json_parsing() {
        let (mut writer, reader) = tokio::io::duplex(MAX_FRAME_BYTES + 2);
        let mut reader = BufReader::new(reader);
        let write = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; MAX_FRAME_BYTES + 1])
                .await
                .unwrap();
        });
        let error = read_app_server_frame(&mut reader).await.unwrap_err();
        assert!(error.to_string().contains("exceeds the admitted limit"));
        write.await.unwrap();
    }

    #[tokio::test]
    async fn reviewer_candidate_mutation_is_detected_while_external_scratch_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "user.email", "test@example.com"],
            vec!["config", "user.name", "Test"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repository)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::create_dir(repository.join("src")).unwrap();
        std::fs::write(repository.join("src/lib.rs"), "a\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["commit", "--quiet", "-m", "base"])
                .current_dir(&repository)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repository.join("src/lib.rs"), "b\n").unwrap();
        let patch = canonical_git_diff(&repository).await.unwrap();
        std::fs::write(repository.join("src/lib.rs"), "a\n").unwrap();
        git_apply(&repository, patch.as_bytes()).await.unwrap();
        let scratch = root.path().join("scratch");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::write(scratch.join("build-output"), "ignored").unwrap();
        verify_candidate_unchanged(&repository, &patch)
            .await
            .unwrap();
        std::fs::write(repository.join("src/lib.rs"), "mutated\n").unwrap();
        assert!(
            verify_candidate_unchanged(&repository, &patch)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn canonical_diff_overrides_repository_prefix_and_textconv_configuration() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.email", "test@example.com"][..],
            &["config", "user.name", "Test"][..],
            &["config", "diff.noprefix", "true"][..],
        ] {
            git_simple(&repository, args).await.unwrap();
        }
        std::fs::create_dir(repository.join("src")).unwrap();
        std::fs::write(repository.join("src/lib.rs"), "before\n").unwrap();
        git_simple(&repository, &["add", "."]).await.unwrap();
        git_simple(&repository, &["commit", "--quiet", "-m", "base"])
            .await
            .unwrap();
        std::fs::write(repository.join("src/lib.rs"), "after\n").unwrap();
        let patch = canonical_git_diff(&repository).await.unwrap();
        assert!(patch.contains("--- a/src/lib.rs"));
        assert!(patch.contains("+++ b/src/lib.rs"));
    }

    #[tokio::test]
    async fn reviewer_checks_out_admitted_base_before_applying_candidate() {
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("repository");
        std::fs::create_dir(&repository).unwrap();
        for args in [
            &["init", "--quiet"][..],
            &["config", "user.email", "test@example.com"][..],
            &["config", "user.name", "Test"][..],
        ] {
            git_simple(&repository, args).await.unwrap();
        }
        std::fs::write(repository.join("src.txt"), "base\n").unwrap();
        git_simple(&repository, &["add", "src.txt"]).await.unwrap();
        git_simple(&repository, &["commit", "--quiet", "-m", "base"])
            .await
            .unwrap();
        let base = git_output(&repository, &["rev-parse", "HEAD"])
            .await
            .unwrap()
            .trim()
            .to_owned();
        std::fs::write(repository.join("src.txt"), "new-head\n").unwrap();
        git_simple(&repository, &["commit", "--quiet", "-am", "new head"])
            .await
            .unwrap();

        checkout_base(&repository, &base).await.unwrap();
        let patch = "diff --git a/src.txt b/src.txt\nindex df967b9..02e8acd 100644\n--- a/src.txt\n+++ b/src.txt\n@@ -1 +1 @@\n-base\n+candidate\n";
        git_apply(&repository, patch.as_bytes()).await.unwrap();
        verify_candidate_unchanged(&repository, patch)
            .await
            .unwrap();
    }
}
