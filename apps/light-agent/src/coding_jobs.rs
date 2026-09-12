//! Dispatch typed coding jobs admitted by the durable workflow bridge.
use super::*;
#[cfg(test)]
use serde_json::json;

pub(super) fn prepare(
    config: &CodingProfileConfig,
    request: &CodingDispatchRequest,
    prompt: &str,
) -> Result<(
    MaterializationManifest,
    CodingTurnSpec,
    CodingAdapterRuntime,
)> {
    validate_repository_input_uri(
        &request.repository.artifact_uri,
        &config.repository_uri_prefix,
    )?;
    let mut runtime = match request.role {
        CodingRole::Implement => config.runtime.clone(),
        CodingRole::Review => config.reviewer_runtime.clone(),
    };
    runtime.native_model = request.native_model.clone();
    admit_native_selection(&runtime, request)?;
    let writable_roots = match request.role {
        CodingRole::Implement if request.writable_roots.is_empty() => {
            BTreeSet::from([request.workspace_root.clone()])
        }
        CodingRole::Implement => request.writable_roots.clone(),
        CodingRole::Review => BTreeSet::from(["/workspace/review-scratch".into()]),
    };
    let manifest = MaterializationManifest {
        schema_version: 1,
        materializer_id: "coding".into(),
        materializer_version: 1,
        product_profile: ProductProfile::Coding,
        runtime_compatibility: runtime.contract.compatibility_digest.clone(),
        packages: Vec::new(),
        effective_instructions: Vec::new(),
        allowed_tools: BTreeSet::new(),
        writable_roots: writable_roots.clone(),
    };
    let spec = CodingTurnSpec {
        thread: request.thread.clone(),
        repository_digest: request.repository.digest.clone(),
        base_revision: request.base_revision.clone(),
        workspace_root: request.workspace_root.clone(),
        prompt: prompt.into(),
        model_alias: runtime.model.clone(),
        authentication_profile: config.authentication_profile,
        role: request.role,
        role_profile: CodingRoleExecutionProfile::pinned(request.role),
        review_input: request.review_input.clone(),
        remediation: request.remediation.clone(),
        materialization_manifest_digest: manifest.digest()?,
        writable_roots,
        allowed_tools: match request.role {
            CodingRole::Implement => request.allowed_tools.clone(),
            CodingRole::Review => CodingTurnSpec::supported_tools(CodingRole::Review),
        },
        maximum_patch_bytes: request.maximum_patch_bytes,
        maximum_changed_files: request.maximum_changed_files,
    };
    spec.validate()?;
    Ok((manifest, spec, runtime))
}

pub(super) fn spawn(state: Arc<AgentState>) {
    tokio::spawn(async move {
        loop {
            if let Err(error) = dispatch(&state).await {
                warn!("workflow coding dispatch failed: {error}");
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

async fn dispatch(state: &AgentState) -> Result<()> {
    for (job, turn, product_profile_digest, input) in state.domain.pending_coding_jobs().await? {
        let result: Result<()> = async {
            let message: ClientMessage = serde_json::from_value(input)?;
            if message.edge_action.is_some()
                || message
                    .profile
                    .is_some_and(|p| p != RequestedProfile::Coding)
            {
                bail!("workflow coding job has conflicting profiles");
            }
            let config = state
                .coding_profile
                .as_ref()
                .context("coding profile is disabled")?;
            // The admitted policy snapshot must bind the same product profile the coding
            // policy was published for. The interactive path enforces this before it
            // schedules; a workflow-admitted turn is not more trusted than a chat turn.
            if product_profile_digest != config.product_profile_digest {
                bail!("turn policy does not authorize the coding profile");
            }
            if let Some(request) = &message.workspace {
                if message.coding.is_some()
                    || message.text != request.instruction
                    || message.client_message_id.as_deref() != Some(request.request_id.as_str())
                {
                    bail!("workflow workspace request conflicts with the coding message");
                }
                let subject = format!("workflow-agent:{}", state.agent_def_id);
                let profile = state
                    .agent_config
                    .agent_policy
                    .execution
                    .coding_profile
                    .as_ref()
                    .context("workspace policy missing")?;
                let binding = profile
                    .workspace_bindings
                    .iter()
                    .find(|b| {
                        b.workspace_id == request.workspace_id
                            && b.host_id == state.host_id.to_string()
                            && b.environment == state.env_tag.as_deref().unwrap_or_default()
                            && b.subjects.contains(&subject)
                            && b.agents.contains(&state.service_id)
                    })
                    .context("workflow workspace is not authorized")?
                    .clone();
                let spec = workspace_execution_protocol::WorkspaceExecutionSpec {
                    request: request.clone(),
                    binding,
                    subject,
                    agent_id: state.service_id.clone(),
                };
                spec.validate()?;
                if request.thread.is_none() {
                    bail!("workflow workspace turns require explicit conversation control");
                }
                state
                    .domain
                    .schedule_workspace_turn(
                        state.host_id,
                        AgentSessionId(job),
                        AgentTurnId(turn),
                        &spec,
                        &config.runtime,
                    )
                    .await?;
                return Ok(());
            }
            let request = message
                .coding
                .context("workflow coding job requires typed coding input")?;
            let (manifest, spec, runtime) = prepare(config, &request, &message.text)?;
            state
                .domain
                .schedule_coding_adapter_turn(
                    state.host_id,
                    AgentSessionId(job),
                    AgentTurnId(turn),
                    &state.service_id,
                    &manifest,
                    &spec,
                    &request.repository,
                    &runtime,
                )
                .await?;
            Ok(())
        }
        .await;
        if let Err(error) = result {
            // Release session capacity only if this dispatch still owns RECEIVED. A
            // failure to record that must not abandon the rest of this batch; the next
            // pass retries the turn, which is still RECEIVED.
            if let Err(unrecorded) = state
                .domain
                .fail_received_coding_turn(
                    state.host_id,
                    AgentSessionId(job),
                    AgentTurnId(turn),
                    &error.to_string(),
                )
                .await
            {
                warn!("workflow coding dispatch failure was not recorded: {unrecorded}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use coding_agent_runtime::*;

    fn config() -> CodingProfileConfig {
        let d = agent_core::sha256_digest(b"test");
        let contract = CodingAdapterContract {
            schema_version: 1,
            adapter_id: CODEX_APP_SERVER_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            adapter_protocol_version: CODEX_APP_SERVER_PROTOCOL_VERSION.into(),
            action_kind: "coding.codex-app-server-v1".into(),
            compatibility_digest: d.clone(),
            image_digest: d.clone(),
            capability_digest: d.clone(),
            template_id: "coding-codex-app-server-v1".into(),
            template_version: 1,
            template_digest: d.clone(),
            executable: "/usr/local/bin/codex".into(),
            binary_digest: CODEX_APP_SERVER_BINARY_DIGEST.into(),
            schema_digest: CODEX_APP_SERVER_SCHEMA_DIGEST.into(),
            required_features: BTreeSet::from(["codex-app-server-v1".into()]),
        };
        let runtime = CodingAdapterRuntime {
            claude_policy: None,
            native_model: None,
            qualification: CodingAdapterQualification {
                schema_version: 1,
                adapter_id: contract.adapter_id.clone(),
                adapter_version: contract.adapter_version.clone(),
                status: CodingAdapterQualificationStatus::Qualified,
                evaluated_dimensions: CodingAdapterQualificationDimension::required(),
                contract_digest: Some(contract.digest().unwrap()),
                evidence_digest: CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST.into(),
            },
            contract,
            model: CODING_IMPLEMENTER_ALIAS.into(),
            enterprise_gateway: None,
        };
        let mut reviewer = runtime.clone();
        reviewer.model = CODING_REVIEWER_ALIAS.into();
        CodingProfileConfig {
            product_profile_digest: d,
            repository_uri_prefix: "file:///spool/".into(),
            authentication_profile: CodingAuthenticationProfile::PersonalSubscription,
            runtime,
            reviewer_runtime: reviewer,
        }
    }

    #[test]
    fn workflow_input_preserves_explicit_thread_control_and_enforces_repository_scope() {
        let config = config();
        let mut input = json!({"text":"Implement the approved phase", "profile":"coding", "coding":{
            "repository":{"artifactUri":"file:///spool/repo.bundle","digest":agent_core::sha256_digest(b"bundle"),"size":10,"mediaType":"application/x-git-bundle"},
            "baseRevision":"a".repeat(40),"workspaceRoot":"/workspace/repository", "allowedTools":["fs.read","fs.write","process.exec"],
            "maximumPatchBytes":4096,"maximumChangedFiles":1,
            "thread":{"runnerId":"runner","sessionRef":Uuid::now_v7(),"stageId":"phase-1","mode":"new","closeAfterTurn":false}
        }});
        for mode in ["new", "resume", "close"] {
            input["coding"]["thread"]["mode"] = json!(mode);
            if mode != "new" {
                input["coding"]["thread"]["expectedCheckpoint"] = json!(Uuid::now_v7());
            }
            let message: ClientMessage = serde_json::from_value(input.clone()).unwrap();
            let request = message.coding.unwrap();
            let (manifest, spec, runtime) = prepare(&config, &request, &message.text).unwrap();
            assert_eq!(spec.thread, request.thread);
            assert_eq!(
                spec.materialization_manifest_digest,
                manifest.digest().unwrap()
            );
            assert_eq!(runtime.model, CODING_IMPLEMENTER_ALIAS);
        }
        input["coding"]["repository"]["artifactUri"] = json!("file:///another-owner/repo.bundle");
        let message: ClientMessage = serde_json::from_value(input.clone()).unwrap();
        assert!(prepare(&config, &message.coding.unwrap(), &message.text).is_err());
        input["coding"]["threadScope"] = json!("foreign-workflow");
        assert!(serde_json::from_value::<ClientMessage>(input).is_err());
    }
}
