use agent_core::ResultClass;
use agent_materializer::MaterializationManifest;
use agent_runtime_protocol::{
    PROTOCOL_VERSION, RuntimeCapabilities, RuntimeCommand, RuntimeEvent, RuntimeEventPayload,
    RuntimeIdentity, canonical_digest,
};
use anyhow::{Context, Result, bail};
use coding_agent_runtime::{CodingTurnSpec, validate_patch};
use execution_security::ProtectedPathPolicy;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub fn capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        adapter_id: "deterministic-mock".into(),
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION.into(),
        actions: BTreeSet::from(["mock".into(), "coding.pi-rpc-v1".into()]),
        supports_checkpoint: true,
        maximum_event_bytes: 1024 * 1024,
    }
}

pub async fn serve<R, W>(reader: R, mut writer: W) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = reader.lines();
    let hello = lines
        .next_line()
        .await?
        .context("runner closed before hello")?;
    let RuntimeCommand::Hello {
        identity,
        expected_capability_digest,
    } = serde_json::from_str(&hello).context("invalid hello")?
    else {
        bail!("first command must be hello")
    };
    if identity.transport_nonce.len() < 32 {
        bail!("transport nonce is too short")
    }
    let caps = capabilities();
    if canonical_digest(&caps)? != expected_capability_digest {
        bail!("capability digest mismatch")
    }
    let mut sequence = 0;
    emit(
        &mut writer,
        &identity,
        &mut sequence,
        RuntimeEventPayload::Ready { capabilities: caps },
    )
    .await?;
    while let Some(line) = lines.next_line().await? {
        match serde_json::from_str::<RuntimeCommand>(&line).context("invalid runtime command")? {
            RuntimeCommand::Start { input, .. } => {
                run_scenario(&mut writer, &identity, &mut sequence, input).await?
            }
            RuntimeCommand::Cancel { reason } => {
                emit(
                    &mut writer,
                    &identity,
                    &mut sequence,
                    RuntimeEventPayload::Terminal {
                        class: ResultClass::Cancelled,
                        output: None,
                        error: Some(reason),
                    },
                )
                .await?
            }
            RuntimeCommand::Checkpoint { reason } => {
                emit(
                    &mut writer,
                    &identity,
                    &mut sequence,
                    RuntimeEventPayload::Checkpoint {
                        reference: format!("mock:{reason}"),
                        digest: agent_core::sha256_digest(reason.as_bytes()),
                    },
                )
                .await?
            }
            RuntimeCommand::Resume { after_sequence } if after_sequence <= sequence => {}
            RuntimeCommand::Resume { .. } => bail!("resume sequence is ahead of worker journal"),
            RuntimeCommand::Hello { .. } => bail!("hello cannot be repeated"),
        }
    }
    Ok(())
}

async fn run_scenario<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
) -> Result<()> {
    let scenario = input
        .get("scenario")
        .and_then(Value::as_str)
        .unwrap_or("success");
    if scenario == "coding-fixture" {
        return run_coding_fixture(writer, identity, sequence, input).await;
    }
    let (class, output, error) = match scenario {
        "success" => (ResultClass::Success, Some(json!({"ok": true})), None),
        "recoverable-failure" => (
            ResultClass::RecoverableFailure,
            None,
            Some("mock recoverable failure".into()),
        ),
        "terminal-failure" => (
            ResultClass::TerminalFailure,
            None,
            Some("mock terminal failure".into()),
        ),
        "approval" => (
            ResultClass::ApprovalRequired,
            Some(json!({"approvalSubject": input.get("subject").cloned().unwrap_or(Value::Null)})),
            None,
        ),
        "cancelled" => (
            ResultClass::Cancelled,
            None,
            Some("mock cancellation".into()),
        ),
        "unknown" | "missing-event" => (
            ResultClass::Unknown,
            None,
            Some("mock outcome unknown".into()),
        ),
        "checkpoint" => {
            emit(
                writer,
                identity,
                sequence,
                RuntimeEventPayload::Checkpoint {
                    reference: "mock:checkpoint".into(),
                    digest: agent_core::sha256_digest(b"checkpoint"),
                },
            )
            .await?;
            (
                ResultClass::Success,
                Some(json!({"checkpointed": true})),
                None,
            )
        }
        other => (
            ResultClass::TerminalFailure,
            None,
            Some(format!("unsupported mock scenario {other}")),
        ),
    };
    if scenario != "missing-event" {
        emit(
            writer,
            identity,
            sequence,
            RuntimeEventPayload::Terminal {
                class,
                output,
                error,
            },
        )
        .await?;
    }
    Ok(())
}

async fn run_coding_fixture<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
) -> Result<()> {
    let spec: CodingTurnSpec = serde_json::from_value(
        input
            .get("codingSpec")
            .cloned()
            .context("codingSpec is required")?,
    )?;
    spec.validate()?;
    let manifest: MaterializationManifest = serde_json::from_value(
        input
            .get("materializationManifest")
            .cloned()
            .context("materializationManifest is required")?,
    )?;
    if manifest.digest()? != spec.materialization_manifest_digest {
        bail!("mounted materialization manifest digest mismatch")
    }
    for package in &manifest.packages {
        let root = std::path::Path::new(&package.mount_target);
        let entrypoint = root.join(&package.entrypoint);
        if !root.is_dir() || !entrypoint.is_file() {
            bail!("materialized package {} is unavailable", package.package_id)
        }
    }
    for phase in ["inspect", "edit", "build", "test", "export"] {
        emit(
            writer,
            identity,
            sequence,
            RuntimeEventPayload::Progress {
                message: phase.into(),
            },
        )
        .await?;
    }
    // This adapter is a deterministic structured-RPC fixture. A production Pi
    // process supplies the same fields over the protected worker transport.
    let path = input
        .get("fixturePath")
        .and_then(Value::as_str)
        .unwrap_or("src/lib.rs");
    let patch = format!(
        "diff --git a/{path} b/{path}\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-before\n+after\n"
    );
    let validated = validate_patch(
        &spec,
        &ProtectedPathPolicy::default_deny(),
        &spec.base_revision,
        &patch,
        &[path.into()],
    )?;
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
    emit(
        writer,
        identity,
        sequence,
        RuntimeEventPayload::Terminal {
            class: ResultClass::Success,
            output: Some(json!({"adapter":"pi-rpc","adapterVersion":"1"})),
            error: None,
        },
    )
    .await
}

async fn emit<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    payload: RuntimeEventPayload,
) -> Result<()> {
    *sequence += 1;
    let event = RuntimeEvent {
        protocol_version: PROTOCOL_VERSION.into(),
        event_id: Uuid::now_v7(),
        execution_id: identity.execution_id,
        lease_id: identity.lease_id,
        fencing_token: identity.fencing_token,
        sequence: *sequence,
        occurred_at: chrono::Utc::now(),
        payload,
    };
    let bytes = serde_json::to_vec(&event)?;
    writer.write_all(&bytes).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId};
    use agent_materializer::{MaterializationManifest, ProductProfile};
    use coding_agent_runtime::CodingTurnSpec;
    use execution_runner_protocol::{ExecutionId, LeaseId};
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    #[tokio::test]
    async fn mock_worker_authenticates_pipe_and_emits_terminal_event() {
        let identity = RuntimeIdentity {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            fencing_token: 7,
            transport_nonce: "a".repeat(32),
        };
        let (client, server) = duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server);
        let task = tokio::spawn(serve(BufReader::new(server_read), server_write));
        let (client_read, mut client_write) = tokio::io::split(client);
        for command in [
            RuntimeCommand::Hello {
                identity: identity.clone(),
                expected_capability_digest: canonical_digest(&capabilities()).unwrap(),
            },
            RuntimeCommand::Start {
                session_id: AgentSessionId::new(),
                turn_id: AgentTurnId::new(),
                action_attempt_id: AgentActionAttemptId::new(),
                policy_digest: "sha256:policy".into(),
                input: json!({"scenario":"success"}),
            },
        ] {
            client_write
                .write_all(&serde_json::to_vec(&command).unwrap())
                .await
                .unwrap();
            client_write.write_all(b"\n").await.unwrap();
        }
        client_write.shutdown().await.unwrap();
        let mut lines = BufReader::new(client_read).lines();
        let ready: RuntimeEvent =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        let terminal: RuntimeEvent =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert!(matches!(ready.payload, RuntimeEventPayload::Ready { .. }));
        assert!(matches!(
            terminal.payload,
            RuntimeEventPayload::Terminal {
                class: ResultClass::Success,
                ..
            }
        ));
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn coding_fixture_emits_structured_patch_without_terminal_scraping() {
        let identity = RuntimeIdentity {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            fencing_token: 8,
            transport_nonce: "b".repeat(32),
        };
        let manifest = MaterializationManifest {
            schema_version: 1,
            materializer_id: "coding".into(),
            materializer_version: 1,
            product_profile: ProductProfile::Coding,
            runtime_compatibility: "pi-rpc-v1".into(),
            packages: vec![],
            effective_instructions: vec![],
            allowed_tools: Default::default(),
            writable_roots: Default::default(),
        };
        let spec = CodingTurnSpec {
            repository_digest: format!("sha256:{:064x}", 1),
            base_revision: "a".repeat(40),
            workspace_root: "/workspace/repo".into(),
            prompt: "fix".into(),
            model_alias: "approved".into(),
            materialization_manifest_digest: manifest.digest().unwrap(),
            writable_roots: Default::default(),
            allowed_tools: Default::default(),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 10,
        };
        let (client, server) = duplex(128 * 1024);
        let (sr, sw) = tokio::io::split(server);
        let task = tokio::spawn(serve(BufReader::new(sr), sw));
        let (cr, mut cw) = tokio::io::split(client);
        for command in [
            RuntimeCommand::Hello {
                identity: identity.clone(),
                expected_capability_digest: canonical_digest(&capabilities()).unwrap(),
            },
            RuntimeCommand::Start {
                session_id: AgentSessionId::new(),
                turn_id: AgentTurnId::new(),
                action_attempt_id: AgentActionAttemptId::new(),
                policy_digest: "sha256:policy".into(),
                input: json!({"scenario":"coding-fixture","codingSpec":spec,"materializationManifest":manifest}),
            },
        ] {
            cw.write_all(&serde_json::to_vec(&command).unwrap())
                .await
                .unwrap();
            cw.write_all(b"\n").await.unwrap();
        }
        cw.shutdown().await.unwrap();
        let mut lines = BufReader::new(cr).lines();
        let mut patch = false;
        while let Some(line) = lines.next_line().await.unwrap() {
            let event: RuntimeEvent = serde_json::from_str(&line).unwrap();
            if matches!(event.payload, RuntimeEventPayload::CodingPatch { .. }) {
                patch = true
            }
        }
        assert!(patch);
        task.await.unwrap().unwrap();
    }
}
