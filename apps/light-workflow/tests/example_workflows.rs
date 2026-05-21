use std::fs;
use std::path::Path;

use workflow_core::models::workflow::WorkflowDefinition;

#[test]
fn example_workflows_parse() {
    let examples_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let entries = fs::read_dir(&examples_dir).expect("examples directory should exist");
    let mut parsed = 0;

    for entry in entries {
        let path = entry.expect("example directory entry").path();
        let Some(extension) = path.extension().and_then(|extension| extension.to_str()) else {
            continue;
        };
        if !matches!(extension, "yaml" | "yml") {
            continue;
        }

        let content = fs::read_to_string(&path).expect("example workflow should be readable");
        let workflow: WorkflowDefinition =
            serde_yaml::from_str(&content).expect("example workflow should parse");

        assert!(
            !workflow.document.name.is_empty(),
            "{} should have a workflow name",
            path.display()
        );
        assert!(
            !workflow.do_.entries.is_empty(),
            "{} should define at least one task",
            path.display()
        );
        parsed += 1;
    }

    assert!(parsed >= 3, "expected at least three example workflows");
}
