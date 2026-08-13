use std::{env, fs};

use workflow_core::models::workflow::WorkflowDefinition;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = env::args()
        .nth(1)
        .ok_or("usage: workflow_definition_digest <definition.yaml>")?;
    let source = fs::read_to_string(path)?;
    let definition: WorkflowDefinition = serde_yaml::from_str(&source)?;
    let value = serde_json::to_value(definition)?;
    let digest = execution_runner_protocol::canonical_sha256(&value)?;
    println!("sha256:{digest}");
    Ok(())
}
