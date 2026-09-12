//! Claude projection onto the existing fenced runner protocol.
use super::*;
use crate::emit;
use agent_core::ResultClass;
use agent_runtime_protocol::{EnterpriseGatewayConfig, RuntimeEventPayload, RuntimeIdentity};
use tokio::io::AsyncWrite;

pub(crate) async fn run<W: AsyncWrite + Unpin>(
    writer: &mut W,
    identity: &RuntimeIdentity,
    sequence: &mut u64,
    input: Value,
    gateway: Option<EnterpriseGatewayConfig>,
    mut cancellation: watch::Receiver<Option<String>>,
    deadline: Option<Instant>,
) -> Result<()> {
    let (cancel_tx, cancel_rx) = watch::channel(cancellation.borrow().is_some());
    let budget = deadline.context("Claude execution requires a runner deadline")?;
    let (tx, mut rx) = mpsc::channel(64);
    let work = async {
        ensure!(
            gateway.is_none() && std::env::var_os("LIGHT_CODEX_HOME").is_none(),
            "Claude forbids enterprise gateway or Codex credentials"
        );
        let contract = serde_json::from_value(input["adapterContract"].clone())?;
        let qualification = serde_json::from_value(input["adapterQualification"].clone())?;
        coding_agent_runtime::claude::require_local_contract(&contract, &qualification)?;
        let spec: CodingTurnSpec = serde_json::from_value(input["codingSpec"].clone())?;
        let host = HostContext {
            executable: std::env::var_os("LIGHT_CLAUDE_EXECUTABLE")
                .context("Claude executable is not runner-configured")?
                .into(),
            native_home: std::env::var_os("LIGHT_CLAUDE_HOME")
                .context("Claude native home is not runner-configured")?
                .into(),
            working_directory: std::env::current_dir()?,
            thread_scope: input["threadScope"]
                .as_str()
                .context("trusted thread scope missing")?
                .into(),
        };
        let staged: Vec<execution_backend::StagedInput> =
            serde_json::from_value(input["runtimeStagedInputs"].clone())?;
        let bundle = staged
            .iter()
            .find(|s| s.mount_target == "/inputs/repository.bundle")
            .context("immutable repository staging missing")?;
        ensure!(
            bundle.source_digest == spec.repository_digest
                && bundle.media_type == "application/x-git-bundle"
                && bundle.read_only
                && !bundle.executable,
            "Claude staged repository differs from admitted input"
        );
        let turn = ClaudeTurn {
            coding: spec,
            policy: serde_json::from_value(input["claudePolicy"].clone())?,
            native_model: serde_json::from_value(
                input.get("nativeModel").cloned().unwrap_or(Value::Null),
            )?,
        };
        let manifest: agent_materializer::MaterializationManifest =
            serde_json::from_value(input["materializationManifest"].clone())?;
        ensure!(
            manifest.runtime_compatibility == contract.compatibility_digest,
            "Claude manifest compatibility mismatch"
        );
        coding::execute_coding(
            &host,
            Path::new(&bundle.local_path),
            &manifest,
            turn,
            cancel_rx,
            budget,
            tx,
        )
        .await
    };
    tokio::pin!(work);
    let result = loop {
        tokio::select! {
            result=&mut work => break result,
            changed=cancellation.changed() => {
                if changed.is_err() || cancellation.borrow().is_some() { let _=cancel_tx.send(true); }
            }
            Some(event)=rx.recv() => {
                let message=match event {
                    Event::TextDelta{text}=>text,
                    Event::Initialized{..}=>"Claude native session initialized".into(),
                    Event::ToolObserved{name,..}=>format!("Claude tool: {name}"),
                    Event::PermissionDenied=>"Claude permission denied".into(),
                };
                tokio::time::timeout_at(budget,emit(writer,identity,sequence,RuntimeEventPayload::Progress{message})).await??;
            }
        }
    };
    match result {
        Ok(output) => {
            if let Some(patch) = &output.patch {
                let patch: coding_agent_runtime::ValidatedPatch =
                    serde_json::from_value(patch.clone())?;
                emit(
                    writer,
                    identity,
                    sequence,
                    RuntimeEventPayload::CodingPatch {
                        base_revision: patch.base_revision,
                        patch: patch.patch,
                        patch_digest: patch.patch_digest,
                        changed_paths: patch.changed_paths.into_iter().collect(),
                    },
                )
                .await?;
            }
            let mut value = serde_json::to_value(output)?;
            // The native CLI does not report reliable exit-code evidence in the
            // qualified protocol. Empty is truthful; prose is never promoted.
            value["validationEvidence"] = json!([]);
            value["reviewValidationEvidence"] = json!([]);
            value.as_object_mut().unwrap().remove("patch");
            emit(
                writer,
                identity,
                sequence,
                RuntimeEventPayload::Terminal {
                    class: ResultClass::Success,
                    output: Some(value),
                    error: None,
                },
            )
            .await
        }
        Err(error) => {
            emit(
                writer,
                identity,
                sequence,
                RuntimeEventPayload::Terminal {
                    class: if *cancel_tx.borrow() {
                        ResultClass::Cancelled
                    } else {
                        ResultClass::TerminalFailure
                    },
                    output: None,
                    error: Some(error.to_string()),
                },
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_runtime_protocol::{RuntimeCommand, canonical_digest};
    #[tokio::test]
    async fn binary_selection_rejects_the_other_adapter_before_native_launch() {
        let identity = RuntimeIdentity {
            execution_id: execution_runner_protocol::ExecutionId::new(),
            lease_id: execution_runner_protocol::LeaseId::new(),
            fencing_token: 3,
            transport_nonce: "n".repeat(32),
        };
        for claude in [false, true] {
            let caps = if claude {
                coding_agent_runtime::claude::capabilities()
            } else {
                crate::capabilities()
            };
            let wrong = if claude {
                coding_agent_runtime::CODEX_APP_SERVER_ADAPTER_ID
            } else {
                ADAPTER_ID
            };
            let commands = [
                RuntimeCommand::Hello {
                    identity: identity.clone(),
                    expected_capability_digest: canonical_digest(&caps).unwrap(),
                },
                RuntimeCommand::Start {
                    session_id: agent_core::AgentSessionId::new(),
                    turn_id: agent_core::AgentTurnId::new(),
                    action_attempt_id: agent_core::AgentActionAttemptId::new(),
                    policy_digest: "p".into(),
                    enterprise_gateway: None,
                    input: json!({"adapterContract":{"adapterId":wrong}}),
                    deadline_ms: Some(1000),
                },
            ];
            let bytes = commands
                .iter()
                .map(|c| serde_json::to_string(c).unwrap() + "\n")
                .collect::<String>();
            let result = if claude {
                crate::serve_claude(bytes.as_bytes(), tokio::io::sink()).await
            } else {
                crate::serve(bytes.as_bytes(), tokio::io::sink()).await
            };
            assert!(result.unwrap_err().to_string().contains("does not match"));
        }
    }
}
