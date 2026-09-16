use serde_json::Value;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("definition path required")?;
    let raw: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let typed: workflow_core::models::workflow::WorkflowDefinition =
        serde_json::from_value(raw.clone())?;
    let normalized = serde_json::to_value(typed)?;
    if raw != normalized {
        println!("{}", serde_json::to_string_pretty(&normalized)?);
        return Err("typed definition changes digest".into());
    }
    let pinned: light_workflow::publication_dispatch::PinnedPublication = serde_json::from_value(
        raw["document"]["metadata"]["developmentWorkflowPublication"].clone(),
    )?;
    if pinned.slots.is_empty() {
        return Err("publication slots required".into());
    }
    println!("{}", workflow_invocation_contract::canonical_sha256(&raw)?);
    Ok(())
}
