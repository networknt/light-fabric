use super::*;
use workspace_execution_protocol::{WorkspaceExecutionSpec, standalone_intents};

fn bindings(
    state: &AgentState,
    principal: Uuid,
) -> Vec<&workspace_execution_protocol::WorkspaceAccessPolicy> {
    let Some(profile) = state
        .agent_config
        .agent_policy
        .execution
        .coding_profile
        .as_ref()
    else {
        return vec![];
    };
    if profile.authentication_profile != CodingAuthenticationProfile::PersonalSubscription
        || profile.product_profile_digest != state.policy_snapshot.product_profile_digest
        || authorize_turn_type(
            RequestedProfile::Coding,
            state
                .agent_config
                .agent_policy
                .execution
                .turn_policy
                .as_ref(),
        )
        .is_err()
    {
        return vec![];
    }
    profile
        .workspace_bindings
        .iter()
        .filter(|b| {
            b.host_id == state.host_id.to_string()
                && b.environment == state.env_tag.as_deref().unwrap_or_default()
                && b.subjects.contains(&principal.to_string())
                && b.agents.contains(&state.service_id)
        })
        .collect()
}

pub(super) fn catalog(state: &AgentState, principal: Uuid) -> serde_json::Value {
    serde_json::json!({"type":"workspaceCatalog", "workspaces": bindings(state, principal).into_iter().map(|b|
        serde_json::json!({"workspaceId":b.workspace_id,"membershipRevision":b.membership_revision,"runnerId":b.runner_id,
            "intents":b.intents.intersection(&standalone_intents()).collect::<Vec<_>>() })).collect::<Vec<_>>()})
}

pub(super) fn admit(
    state: &AgentState,
    message: &ClientMessage,
    principal: Uuid,
) -> Result<Option<WorkspaceExecutionSpec>> {
    let Some(request) = &message.workspace else {
        return Ok(None);
    };
    if message.client_message_id.as_deref() != Some(request.request_id.as_str())
        || message.text != request.instruction
    {
        bail!("workspace request must match the Chat message and request ID");
    }
    let binding = bindings(state, principal)
        .into_iter()
        .find(|b| b.workspace_id == request.workspace_id)
        .context("workspace is not available to this session")?
        .clone();
    let spec = WorkspaceExecutionSpec {
        request: request.clone(),
        binding,
        subject: principal.to_string(),
        agent_id: state.service_id.clone(),
    };
    spec.validate()?;
    Ok(Some(spec))
}
