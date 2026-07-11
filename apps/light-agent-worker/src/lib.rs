use agent_core::ResultClass;
use agent_runtime_protocol::{
    PROTOCOL_VERSION, RuntimeCapabilities, RuntimeCommand, RuntimeEvent, RuntimeEventPayload,
    RuntimeIdentity, canonical_digest,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

pub fn capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        adapter_id: "deterministic-mock".into(),
        adapter_version: env!("CARGO_PKG_VERSION").into(),
        protocol_version: PROTOCOL_VERSION.into(),
        actions: BTreeSet::from(["mock".into()]),
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
}
