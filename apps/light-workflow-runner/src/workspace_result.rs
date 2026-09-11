use serde_json::Value;
use workspace_execution_protocol::{TaskSelection, WorkspaceExecutionSpec, digest};

pub(crate) fn validate(spec: &WorkspaceExecutionSpec, output: &Value) -> Result<(), String> {
    spec.validate().map_err(|e| e.to_string())?;
    let key = spec
        .request
        .admission_key(&spec.context())
        .map_err(|e| e.to_string())?;
    let suffix = key.strip_prefix("sha256:").ok_or("invalid job key")?;
    let task = match &spec.request.task {
        TaskSelection::Existing { task_id } => task_id.clone(),
        TaskSelection::New { .. } => format!("task-{suffix}"),
    };
    let workspace = &output["workspace"];
    if workspace["workspaceId"] != spec.request.workspace_id
        || workspace["taskId"] != task
        || workspace["jobId"] != format!("job-{suffix}")
        || workspace["intent"]
            != serde_json::to_value(spec.request.intent).map_err(|e| e.to_string())?
        || !workspace["checkpointDigest"].as_str().is_some_and(digest)
        || !output["finalMessage"]
            .as_str()
            .is_some_and(|s| !s.trim().is_empty() && s.len() <= 64 * 1024)
    {
        return Err(
            "workspace result differs from the admitted task or has no bounded explanation".into(),
        );
    }
    let authentication: coding_agent_runtime::CodingAuthenticationEvidence =
        serde_json::from_value(output["authentication"].clone()).map_err(|e| e.to_string())?;
    authentication.validate().map_err(|e| e.to_string())?;
    if authentication.profile
        != coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription
    {
        return Err("workspace authentication profile differs from the personal runner".into());
    }
    Ok(())
}
