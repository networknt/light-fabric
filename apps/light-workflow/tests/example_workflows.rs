use std::fs;
use std::path::Path;

use execution_runner_protocol::canonical_sha256;
use light_workflow::command_template::resolve_run_shell_spec;
use workflow_core::models::workflow::WorkflowDefinition;
use workflow_policy::{
    CommandTemplate, ExecutionProfile, TaskKind, parse_security_policy, resolve_policy,
};

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

#[test]
fn mock_run_shell_example_resolves_against_local_configuration() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let workflow_yaml = fs::read_to_string(manifest.join("examples/run-shell-mock-v1.yaml"))
        .expect("run.shell example should exist");
    let workflow_value: serde_yaml::Value =
        serde_yaml::from_str(&workflow_yaml).expect("run.shell example should be YAML");
    let workflow: WorkflowDefinition =
        serde_yaml::from_str(&workflow_yaml).expect("run.shell example should parse");

    let config_yaml = fs::read_to_string(manifest.join("config/runner-execution.mock.yml"))
        .expect("mock runner execution config should exist");
    let config: serde_yaml::Value =
        serde_yaml::from_str(&config_yaml).expect("mock config should be YAML");
    let profiles = config["profiles"]
        .as_sequence()
        .expect("profiles should be a sequence")
        .iter()
        .cloned()
        .map(serde_yaml::from_value::<ExecutionProfile>)
        .collect::<Result<Vec<_>, _>>()
        .expect("profiles should match the policy contract");
    let templates = config["commandTemplates"]
        .as_sequence()
        .expect("command templates should be a sequence")
        .iter()
        .cloned()
        .map(serde_yaml::from_value::<CommandTemplate>)
        .collect::<Result<Vec<_>, _>>()
        .expect("templates should match the policy contract");
    let profiles = profiles
        .into_iter()
        .map(|profile| (profile.id.clone(), profile))
        .collect();
    let templates = templates
        .into_iter()
        .map(|template| (template.id.clone(), template))
        .collect();

    let security = parse_security_policy(&workflow_value)
        .expect("security metadata should parse")
        .expect("security metadata should be present");
    let resolved = resolve_policy(TaskKind::RunShell, Some(&security), &profiles)
        .expect("mock profile should satisfy the workflow policy");
    assert_eq!(resolved.action_kind, "run.shell");

    let snapshot = serde_json::to_value(workflow).expect("workflow should serialize");
    let command = resolve_run_shell_spec(snapshot, "printMessage", &templates)
        .expect("run.shell should resolve to a direct argv command");
    assert_eq!(command.executable, "/usr/bin/printf");
    assert_eq!(command.arguments, ["%s\\n", "hello from isolated runner"]);
    assert!(!command.network_enabled);
    assert!(!command.credentials_enabled);
    assert!(!command.persistent_workspace);
    assert_eq!(
        command.template_digest,
        canonical_sha256(&templates["print-message"]).unwrap()
    );
    assert_eq!(
        command.template_digest,
        "23b66a65d1802f3491bc749d89be9c1771204824b71ffff7e70b204f4d2b63ab"
    );
}
