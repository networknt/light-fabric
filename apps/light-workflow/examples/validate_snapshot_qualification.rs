//! Validate the actual snapshot input against schema and strict intake admission.
use serde_json::{Value, json};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("definition path required")?;
    let definition: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let typed: workflow_core::models::workflow::WorkflowDefinition =
        serde_json::from_value(definition.clone())?;
    let digest = workflow_invocation_contract::canonical_sha256(&definition)?;
    assert_eq!(
        digest,
        workflow_invocation_contract::canonical_sha256(&serde_json::to_value(typed)?)?
    );
    let now = chrono::Utc::now().timestamp() as u64;
    let claim = json!({"featureRunId":uuid::Uuid::now_v7(),"transitionId":uuid::Uuid::now_v7(),"predecessorVersion":1,"stage":{"kind":"intake"},"inputs":{},"definition":{"id":"validation-only","digest":digest},"workspaceBinding":definition["document"]["metadata"]["developmentWorkflowIntake"]["workspaceBinding"],"deadlineEpochSeconds":now+600});
    let input = json!({"stageClaim":claim,"featureIntake":{"issue":{"repository":"networknt/light-fabric","number":392,"url":"https://github.com/networknt/light-fabric/issues/392"}},"snapshotRequest":{"taskId":"validation-task","checkpointDigest":format!("sha256:{}","a".repeat(64))}});
    let validator = jsonschema::Validator::new(&definition["input"]["schema"]["document"])?;
    assert!(validator.is_valid(&input));
    let claim = light_workflow::development_intake::resolve_claim(&definition, &input, None)?
        .ok_or("claim missing")?;
    assert!(light_workflow::development_intake::seed(&definition, &input, &claim, now)?.is_some());
    let mut misplaced = input.clone();
    misplaced["featureIntake"]["snapshotRequest"] = input["snapshotRequest"].clone();
    assert!(
        light_workflow::development_intake::seed(&definition, &misplaced, &claim, now).is_err()
    );
    let mut missing = input.clone();
    missing.as_object_mut().unwrap().remove("snapshotRequest");
    assert!(!validator.is_valid(&missing));
    let mut invalid = input.clone();
    invalid["snapshotRequest"]["checkpointDigest"] = json!("invalid");
    assert!(!validator.is_valid(&invalid));
    println!("PASS: snapshot schema, typed digest, pristine intake and negative input cases");
    Ok(())
}
