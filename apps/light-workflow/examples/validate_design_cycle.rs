//! Validate the exact local catalog digest and native qualification definitions.
//! No model calls, database writes, or authority changes.
use serde_json::Value;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err("expected workspace registration, intake definition, design definition".into());
    }
    let workspace: task_workspace::Workspace = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let membership = task_workspace::membership_revision(&workspace)?;
    println!("runtime membership revision: {membership}");
    let mut mismatches = 0;
    for (path, expected_kind) in args[1..].iter().zip(["intake", "design"]) {
        let raw: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        let typed: workflow_core::models::workflow::WorkflowDefinition =
            serde_json::from_value(raw.clone())?;
        let digest = workflow_invocation_contract::canonical_sha256(&raw)?;
        if digest != workflow_invocation_contract::canonical_sha256(&serde_json::to_value(typed)?)?
        {
            return Err("typed definition changes immutable digest".into());
        }
        let metadata = &raw["document"]["metadata"];
        if metadata["developmentWorkflowStage"]["kind"] != expected_kind {
            return Err("qualification must use separate intake and design stages".into());
        }
        let stage: development_workflow_contract::StageSelector =
            serde_json::from_value(metadata["developmentWorkflowStage"].clone())?;
        let scope = light_workflow::development_store::stage_budget_scope(&stage)?;
        for slot in metadata["developmentWorkflowTurns"]
            .as_object()
            .ok_or("turn slots required")?
            .values()
        {
            let _: development_workflow_contract::TurnKind =
                serde_json::from_value(slot["kind"].clone())?;
            if slot["budgetScope"] != scope {
                return Err("incorrect stage budget scope".into());
            }
        }
        let tasks = raw["do"].as_array().ok_or("tasks required")?;
        if tasks.is_empty() || tasks.len() > 64 {
            return Err("qualification exceeds the one-to-sixty-four task admission bound".into());
        }
        for task in tasks {
            let (name, body) = task
                .as_object()
                .ok_or("task object required")?
                .iter()
                .next()
                .ok_or("empty task")?;
            let Some(request) = body.pointer("/with/input/workspace") else {
                continue;
            };
            if request["workspaceId"] != workspace.id {
                return Err("unexpected workspace".into());
            }
            if request["expectedMembershipRevision"] != membership {
                mismatches += 1;
            }
            if let Some(thread) = request.get("thread") {
                if thread["sessionRef"] != "${{ stageClaim.transitionId }}" {
                    return Err(
                        "session reference must resolve to the stage transition UUID".into(),
                    );
                }
                if thread["mode"] == "resume" {
                    let expected = match name.as_str() {
                        "fix" => "${{ author.codingThread.checkpoint }}",
                        "reviewFix" => "${{ review.codingThread.checkpoint }}",
                        _ => return Err("unrecognized qualification resume pair".into()),
                    };
                    if thread["expectedCheckpoint"] != expected {
                        return Err(
                            "resume must bind the previous native receipt checkpoint".into()
                        );
                    }
                } else if thread.get("expectedCheckpoint").is_some() {
                    return Err("new thread must not consume a prior checkpoint".into());
                }
            }
        }
        println!("{expected_kind}: typed digest and turn scopes valid ({digest})");
    }
    if mismatches != 0 {
        return Err(format!(
            "{mismatches} native requests differ from the actual local membership revision"
        )
        .into());
    }
    Ok(())
}
