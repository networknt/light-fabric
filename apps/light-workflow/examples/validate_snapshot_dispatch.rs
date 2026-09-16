//! Check every published snapshot request against the real execution contract.
use serde_json::{Value, json};
use workspace_execution_protocol::WorkspaceExecutionSpec;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("definition path required")?;
    let definition: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let owner = "01964b05-5532-7c79-8cde-191dcbd421b8";
    let checkpoint = format!("sha256:{}", "a".repeat(64));
    let mut count = 0;
    for task in definition["do"].as_array().ok_or("tasks required")? {
        let body = task
            .as_object()
            .ok_or("task object required")?
            .values()
            .next()
            .unwrap();
        let input = &body["with"]["input"];
        if input["managerSnapshot"].is_null() {
            continue;
        }
        let mut request = input["workspace"].clone();
        assert_eq!(request["task"]["taskId"], "${{ snapshotRequest.taskId }}");
        assert_eq!(
            request["expectedCheckpointDigest"],
            "${{ snapshotRequest.checkpointDigest }}"
        );
        assert_eq!(
            input["managerSnapshot"]["checkpointDigest"],
            request["expectedCheckpointDigest"]
        );
        // Substitute caller values; the production dispatcher injects stageId.
        request["requestId"] = json!(format!("validation-{count}"));
        request["task"]["taskId"] = json!("validation-task");
        request["expectedCheckpointDigest"] = json!(checkpoint);
        let spec: WorkspaceExecutionSpec = serde_json::from_value(json!({
            "request": request,
            "binding": {"schemaVersion":1,"workspaceId":request["workspaceId"],
                "hostId":"host","environment":"loc","runnerId":"runner",
                "membershipRevision":request["expectedMembershipRevision"],
                "authorizationRevision":1,"subjects":[owner],"agents":["agent"],"intents":["review"]},
            "subject":owner,"agentId":"agent",
            "managerSnapshot":{"featureId":"feature","stageId":"stage","snapshotId":"snapshot",
                "checkpointDigest":checkpoint,"offset":0}
        }))?;
        spec.validate()?;
        count += 1;
    }
    assert_eq!(count, 16);
    println!("PASS: all 16 fixture requests satisfy owner-bound snapshot execution contract");
    Ok(())
}
