use execution_runner_protocol::{CommandExecutionSpec, canonical_sha256};
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use workflow_core::models::task::TaskDefinition;
use workflow_core::models::workflow::WorkflowDefinition;
use workflow_policy::{CommandParameterSlot, CommandTemplate};

pub fn resolve_run_shell_spec(
    definition_snapshot: serde_json::Value,
    workflow_task_id: &str,
    templates: &BTreeMap<String, CommandTemplate>,
) -> Result<CommandExecutionSpec, String> {
    let definition = serde_json::from_value::<WorkflowDefinition>(definition_snapshot)
        .map_err(|error| format!("invalid workflow definition snapshot: {error}"))?;
    let task = definition
        .do_
        .entries
        .iter()
        .find_map(|entry| entry.get(workflow_task_id))
        .ok_or_else(|| format!("task definition `{workflow_task_id}` was not found"))?;
    let TaskDefinition::Run(run) = task else {
        return Err(format!("task `{workflow_task_id}` is not a run task"));
    };
    let shell = run
        .run
        .shell
        .as_ref()
        .ok_or_else(|| format!("task `{workflow_task_id}` is not run.shell"))?;
    if run.run.container.is_some() || run.run.script.is_some() || run.run.workflow.is_some() {
        return Err("run.shell cannot combine multiple process types".to_string());
    }
    let template_id = shell
        .command_template_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "run.shell requires commandTemplateId".to_string())?;
    if !shell.command.trim().is_empty() && shell.command.trim() != template_id {
        return Err(
            "run.shell command must be empty or equal commandTemplateId; arbitrary executables are forbidden"
                .to_string(),
        );
    }
    let template = templates
        .get(template_id)
        .ok_or_else(|| format!("command template `{template_id}` is not configured"))?;
    validate_command_template(template)?;
    let supplied = parse_parameters(shell.arguments.as_deref().unwrap_or_default())?;
    let arguments = materialize_arguments(template, &supplied)?;
    let environment = validate_environment(
        shell.environment.as_ref(),
        &template.allowed_environment_names,
    )?;
    let template_digest = canonical_sha256(template).map_err(|error| error.to_string())?;

    Ok(CommandExecutionSpec {
        schema_version: 1,
        template_id: template.id.clone(),
        template_version: template.version,
        template_digest,
        executable: template.executable.clone(),
        arguments,
        working_directory: template.working_directory.clone(),
        environment,
        wall_clock_timeout_ms: template.wall_clock_timeout_ms,
        stdout_limit_bytes: template.stdout_limit_bytes,
        stderr_limit_bytes: template.stderr_limit_bytes,
        network_enabled: false,
        credentials_enabled: false,
        persistent_workspace: false,
    })
}

pub fn validate_command_template(template: &CommandTemplate) -> Result<(), String> {
    if template.id.trim().is_empty() || template.version == 0 {
        return Err("command template ID and positive version are required".to_string());
    }
    if !template.executable.starts_with('/')
        || template.executable.contains(char::is_whitespace)
        || template.executable.contains('\0')
    {
        return Err("command template executable must be an absolute literal path".to_string());
    }
    let executable_name = template
        .executable
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        executable_name.as_str(),
        "sh" | "bash" | "dash" | "zsh" | "fish" | "env"
    ) {
        return Err(
            "shell interpreters and env launchers are forbidden command templates".to_string(),
        );
    }
    if !template.working_directory.starts_with("/workspace")
        || template
            .working_directory
            .split('/')
            .any(|part| part == "..")
    {
        return Err("command template workingDirectory must remain under /workspace".to_string());
    }
    if template.wall_clock_timeout_ms == 0
        || template.stdout_limit_bytes == 0
        || template.stderr_limit_bytes == 0
    {
        return Err("command template time and output limits must be positive".to_string());
    }
    let mut names = BTreeSet::new();
    let mut indexes = BTreeSet::new();
    for slot in &template.parameter_slots {
        if slot.name.trim().is_empty() || !names.insert(slot.name.as_str()) {
            return Err(
                "command template parameter names must be non-empty and unique".to_string(),
            );
        }
        if !indexes.insert(slot.argument_index) {
            return Err(
                "command template parameter argumentIndex values must be unique".to_string(),
            );
        }
        if slot.maximum_bytes == 0 {
            return Err(format!(
                "command template parameter `{}` maximumBytes must be positive",
                slot.name
            ));
        }
        if let Some(pattern) = &slot.pattern {
            Regex::new(pattern).map_err(|error| {
                format!(
                    "command template parameter `{}` pattern: {error}",
                    slot.name
                )
            })?;
        }
    }
    for argument in &template.fixed_arguments {
        validate_literal(argument, "fixed argument")?;
    }
    Ok(())
}

fn parse_parameters(arguments: &[String]) -> Result<BTreeMap<String, String>, String> {
    let mut parameters = BTreeMap::new();
    for argument in arguments {
        let (name, value) = argument
            .split_once('=')
            .ok_or_else(|| "run.shell arguments must use parameterName=value".to_string())?;
        let name = name.trim();
        if name.is_empty()
            || parameters
                .insert(name.to_string(), value.to_string())
                .is_some()
        {
            return Err(format!("duplicate or empty run.shell parameter `{name}`"));
        }
    }
    Ok(parameters)
}

fn materialize_arguments(
    template: &CommandTemplate,
    supplied: &BTreeMap<String, String>,
) -> Result<Vec<String>, String> {
    let known = template
        .parameter_slots
        .iter()
        .map(|slot| slot.name.as_str())
        .collect::<BTreeSet<_>>();
    for name in supplied.keys() {
        if !known.contains(name.as_str()) {
            return Err(format!("unknown command parameter `{name}`"));
        }
    }
    let mut arguments = template.fixed_arguments.clone();
    let mut slots = template.parameter_slots.iter().collect::<Vec<_>>();
    slots.sort_by_key(|slot| slot.argument_index);
    for slot in slots {
        let value = supplied.get(&slot.name);
        if slot.required && value.is_none() {
            return Err(format!(
                "required command parameter `{}` is missing",
                slot.name
            ));
        }
        let Some(value) = value else {
            continue;
        };
        validate_parameter(slot, value)?;
        if slot.argument_index > arguments.len() {
            return Err(format!(
                "command parameter `{}` argumentIndex {} leaves an argv gap",
                slot.name, slot.argument_index
            ));
        }
        arguments.insert(slot.argument_index, value.clone());
    }
    Ok(arguments)
}

fn validate_parameter(slot: &CommandParameterSlot, value: &str) -> Result<(), String> {
    if value.len() > slot.maximum_bytes {
        return Err(format!(
            "command parameter `{}` exceeds {} bytes",
            slot.name, slot.maximum_bytes
        ));
    }
    validate_literal(value, &format!("command parameter `{}`", slot.name))?;
    if !slot.allowed_values.is_empty()
        && !slot.allowed_values.iter().any(|allowed| allowed == value)
    {
        return Err(format!(
            "command parameter `{}` is not allowlisted",
            slot.name
        ));
    }
    if let Some(pattern) = &slot.pattern
        && !Regex::new(pattern)
            .map_err(|error| error.to_string())?
            .is_match(value)
    {
        return Err(format!(
            "command parameter `{}` does not match its pattern",
            slot.name
        ));
    }
    Ok(())
}

fn validate_environment(
    environment: Option<&std::collections::HashMap<String, String>>,
    allowed_names: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let allowed = allowed_names
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut output = BTreeMap::new();
    for (name, value) in environment.into_iter().flatten() {
        if !allowed.contains(name.as_str()) {
            return Err(format!("environment variable `{name}` is not allowlisted"));
        }
        if secret_shaped(name) || secret_shaped(value) {
            return Err(format!(
                "environment variable `{name}` looks credential-bearing"
            ));
        }
        validate_literal(value, &format!("environment variable `{name}`"))?;
        output.insert(name.clone(), value.clone());
    }
    Ok(output)
}

fn validate_literal(value: &str, kind: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{kind} contains NUL"));
    }
    if ["$(", "`", ";", "&&", "||", "\n", "\r", ">", "<", "|"]
        .iter()
        .any(|marker| value.contains(marker))
    {
        return Err(format!("{kind} contains forbidden shell semantics"));
    }
    if value.contains("${{") || value.contains("${") {
        return Err(format!("{kind} contains unresolved template expansion"));
    }
    Ok(())
}

fn secret_shaped(value: &str) -> bool {
    let normalized = value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "authorization",
        "apikey",
        "privatekey",
    ]
    .iter()
    .any(|marker| normalized.contains(marker))
        || value.to_ascii_lowercase().contains("bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> CommandTemplate {
        CommandTemplate {
            id: "print-message".into(),
            version: 1,
            executable: "/usr/bin/printf".into(),
            fixed_arguments: vec!["%s\\n".into()],
            parameter_slots: vec![CommandParameterSlot {
                name: "message".into(),
                argument_index: 1,
                required: true,
                allowed_values: Vec::new(),
                pattern: Some("^[A-Za-z0-9 ._-]+$".into()),
                maximum_bytes: 64,
            }],
            working_directory: "/workspace".into(),
            allowed_environment_names: vec!["LANG".into()],
            wall_clock_timeout_ms: 5_000,
            stdout_limit_bytes: 16_384,
            stderr_limit_bytes: 16_384,
        }
    }

    #[test]
    fn shell_semantics_and_secret_environment_are_rejected() {
        assert!(validate_literal("hello; rm -rf /", "argument").is_err());
        assert!(
            validate_environment(
                Some(&std::collections::HashMap::from([(
                    "API_TOKEN".to_string(),
                    "secret".to_string()
                )])),
                &["API_TOKEN".to_string()]
            )
            .is_err()
        );
    }

    #[test]
    fn malformed_parameter_pattern_is_rejected_before_scheduling() {
        let mut template = template();
        template.parameter_slots[0].pattern = Some("[unterminated".into());
        assert!(
            validate_command_template(&template)
                .unwrap_err()
                .contains("pattern")
        );
    }

    #[test]
    fn template_materializes_direct_argv() {
        let template = template();
        let arguments = materialize_arguments(
            &template,
            &BTreeMap::from([("message".to_string(), "hello world".to_string())]),
        )
        .unwrap();
        assert_eq!(arguments, ["%s\\n", "hello world"]);
    }
}
