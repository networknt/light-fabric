//! Validate a qualification definition without starting work or minting authority.
use development_workflow_contract::{ArtifactRef, StageClaim, StageSelector};
use light_workflow::{development_intake, development_store};
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("definition path required")?;
    let definition: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let typed: workflow_core::models::workflow::WorkflowDefinition =
        serde_json::from_value(definition.clone())?;
    let metadata = &definition["document"]["metadata"];
    let stage: StageSelector =
        serde_json::from_value(metadata["developmentWorkflowStage"].clone())?;
    let scope = development_store::stage_budget_scope(&stage)?;
    for slot in metadata["developmentWorkflowTurns"]
        .as_object()
        .ok_or("turn slots required")?
        .values()
    {
        if slot["budgetScope"] != scope {
            return Err("turn budget scope mismatch".into());
        }
    }
    let digest = workflow_invocation_contract::canonical_sha256(&definition)?;
    let runtime_digest =
        workflow_invocation_contract::canonical_sha256(&serde_json::to_value(&typed)?)?;
    if digest != runtime_digest {
        return Err(format!(
            "published/runtime definition digest mismatch: {digest} versus {runtime_digest}"
        )
        .into());
    }
    let now = chrono::Utc::now().timestamp() as u64;
    let claim = StageClaim {
        feature_run_id: uuid::Uuid::now_v7().to_string(),
        transition_id: uuid::Uuid::now_v7().to_string(),
        predecessor_version: 1,
        stage,
        inputs: Default::default(),
        definition: ArtifactRef {
            id: "validation-only".into(),
            digest: digest.clone(),
        },
        workspace_binding: serde_json::from_value(
            metadata["developmentWorkflowIntake"]["workspaceBinding"].clone(),
        )?,
        deadline_epoch_seconds: now + 600,
    };
    let input = json!({"stageClaim":claim,"featureIntake":{"issue":{
        "repository":"networknt/light-fabric","number":392,
        "url":"https://github.com/networknt/light-fabric/issues/392"}}});
    development_intake::resolve_claim(&definition, &input, Some(claim.clone()))?;
    let feature =
        development_intake::seed(&definition, &input, &claim, now)?.ok_or("intake missing")?;
    println!(
        "{} {}: typed definition, round scopes and pristine intake passed; maximum turns {}; digest {}",
        typed.document.name, typed.document.version, feature.budgets.maximum_turns, digest
    );
    Ok(())
}
