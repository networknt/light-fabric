use serde_json::Value;
use workspace_execution_protocol::{TaskSelection, WorkspaceExecutionSpec, digest};

pub(crate) fn validate(spec: &WorkspaceExecutionSpec, output: &Value) -> Result<(), String> {
    spec.validate().map_err(|e| e.to_string())?;
    if spec.manager_snapshot.is_some() {
        return validate_snapshot(spec, output);
    }
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

// Selected by the admitted request, never by an output field supplied by a worker.
// Full package hash and Git-tree verification remains the Workflow assembly gate.
fn validate_snapshot(spec: &WorkspaceExecutionSpec, output: &Value) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct SnapshotResult {
        manager_snapshot: workspace_execution_protocol::ManagerSnapshotRead,
        workspace_id: String,
        task_id: String,
        receipt: task_workspace::SnapshotReceipt,
        chunk: task_workspace::SnapshotChunk,
    }
    let result: SnapshotResult =
        serde_json::from_value(output.clone()).map_err(|e| e.to_string())?;
    let request = spec
        .manager_snapshot
        .as_ref()
        .ok_or("snapshot request missing")?;
    let TaskSelection::Existing { task_id } = &spec.request.task else {
        return Err("snapshot requires existing task".into());
    };
    let receipt = &result.receipt;
    let chunk = &result.chunk;
    if result.manager_snapshot != *request
        || result.workspace_id != spec.request.workspace_id
        || result.task_id != *task_id
        || receipt.snapshot_id != request.snapshot_id
        || receipt.checkpoint_digest != request.checkpoint_digest
        || !digest(&receipt.package_digest)
        || request
            .package_digest
            .as_ref()
            .is_some_and(|d| d != &receipt.package_digest)
        || chunk.package_digest != receipt.package_digest
        || chunk.total_bytes != receipt.package_bytes
        || chunk.total_bytes == 0
        || chunk.total_bytes > 32 * 1024 * 1024
        || chunk.offset != request.offset
        || chunk.offset >= chunk.total_bytes
        || chunk.bytes.len()
            != (chunk.total_bytes - chunk.offset).min(task_workspace::SNAPSHOT_CHUNK_BYTES as u64)
                as usize
        || receipt.trees.is_empty()
        || receipt.trees.iter().any(|(name, tree)| {
            !workspace_execution_protocol::identifier(name)
                || !matches!(tree.len(), 40 | 64)
                || !tree
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(
            "snapshot result differs from admitted identity, receipt, or chunk bounds".into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> (WorkspaceExecutionSpec, Value) {
        let hash = workspace_execution_protocol::sha256(b"checkpoint");
        let package = workspace_execution_protocol::sha256(b"package");
        let read = json!({"featureId":"feature","stageId":"stage","snapshotId":"snapshot","checkpointDigest":hash,"offset":0});
        let spec = serde_json::from_value(json!({
            "subject":"owner","agentId":"agent",
            "binding":{"schemaVersion":1,"workspaceId":"workspace","hostId":"host","environment":"loc","runnerId":"runner","membershipRevision":hash,"authorizationRevision":1,"subjects":["owner"],"agents":["agent"],"intents":["review"]},
            "request":{"schemaVersion":1,"requestId":"read-1","workspaceId":"workspace","expectedMembershipRevision":hash,"task":{"kind":"existing","taskId":"task"},"intent":"review","expectedCheckpointDigest":hash,"instruction":"snapshot"},
            "managerSnapshot":read
        })).unwrap();
        let output = json!({"managerSnapshot":read,"workspaceId":"workspace","taskId":"task",
            "receipt":{"snapshotId":"snapshot","packageDigest":package,"packageBytes":3,"checkpointDigest":hash,"trees":{"repo":"a".repeat(40)}},
            "chunk":{"packageDigest":package,"totalBytes":3,"offset":0,"bytes":[1,2,3]}});
        (spec, output)
    }

    #[test]
    fn ordinary_result_still_requires_model_evidence_and_exact_job() {
        let (mut spec, _) = fixture();
        spec.manager_snapshot = None;
        let key = spec.request.admission_key(&spec.context()).unwrap();
        let mut output = json!({"workspace":{"workspaceId":"workspace","taskId":"task",
            "jobId":format!("job-{}",key.strip_prefix("sha256:").unwrap()),"intent":"review",
            "checkpointDigest":spec.request.expected_checkpoint_digest},"finalMessage":"Reviewed",
            "authentication":serde_json::to_value(coding_agent_runtime::CodingAuthenticationEvidence {
                profile:coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription,
                credential_source:coding_agent_runtime::CodingCredentialSource::NativeCodexStore,
                credential_generation:None,authoritative_usage:false,
            }).unwrap()});
        validate(&spec, &output).unwrap();
        output["authentication"] = Value::Null;
        assert!(validate(&spec, &output).is_err());
        output["managerSnapshot"] = json!({});
        assert!(validate(&spec, &output).is_err());
    }

    #[test]
    fn snapshot_result_accepts_exact_admitted_chunk_without_model_fields() {
        let (mut spec, mut output) = fixture();
        validate(&spec, &output).unwrap();
        let request = spec.manager_snapshot.as_mut().unwrap();
        request.offset = task_workspace::SNAPSHOT_CHUNK_BYTES as u64;
        request.package_digest = Some(output["receipt"]["packageDigest"].as_str().unwrap().into());
        output["managerSnapshot"] = serde_json::to_value(&request).unwrap();
        output["chunk"]["offset"] = json!(request.offset);
        output["chunk"]["totalBytes"] = json!(request.offset + 3);
        output["receipt"]["packageBytes"] = json!(request.offset + 3);
        validate(&spec, &output).unwrap();
        output["receipt"]["packageDigest"] =
            json!(workspace_execution_protocol::sha256(b"changed"));
        output["chunk"]["packageDigest"] = output["receipt"]["packageDigest"].clone();
        assert!(validate(&spec, &output).is_err());
    }

    #[test]
    fn snapshot_result_rejects_identity_substitution_and_malformed_chunks() {
        let (spec, output) = fixture();
        for (pointer, value) in [
            ("/workspaceId", json!("other")),
            ("/taskId", json!("other")),
            ("/managerSnapshot/featureId", json!("other")),
            ("/managerSnapshot/stageId", json!("other")),
            ("/managerSnapshot/snapshotId", json!("other")),
            ("/receipt/snapshotId", json!("other")),
            ("/receipt/checkpointDigest", json!("invalid")),
            ("/receipt/packageDigest", json!("invalid")),
            ("/receipt/packageBytes", json!(4)),
            ("/receipt/trees", json!({})),
            ("/chunk/offset", json!(1)),
            ("/chunk/totalBytes", json!(0)),
            ("/chunk/packageDigest", json!("invalid")),
            ("/chunk/bytes", json!([1, 2])),
        ] {
            let mut bad = output.clone();
            *bad.pointer_mut(pointer).unwrap() = value;
            assert!(validate(&spec, &bad).is_err(), "accepted {pointer}");
        }
        let mut bad = output.clone();
        bad["extra"] = json!(true);
        assert!(validate(&spec, &bad).is_err());
        let mut ordinary = spec.clone();
        ordinary.manager_snapshot = None;
        assert!(validate(&ordinary, &output).is_err());
        let mut denied = spec;
        denied.subject = "other-owner".into();
        assert!(validate(&denied, &output).is_err());
    }
}
