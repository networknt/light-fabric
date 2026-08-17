use serde::Deserialize;
use std::{collections::BTreeMap, env, fs, path::Path};
use workflow_policy::{CommandTemplate, ExecutionProfile};

use crate::command_template::validate_command_template;

pub const DEFAULT_MAXIMUM_PARALLELISM: usize = 64;
const ABSOLUTE_MAXIMUM_PARALLELISM: usize = 64;

pub fn maximum_parallelism_from_environment() -> Result<usize, String> {
    parse_maximum_parallelism(env::var("WORKFLOW_MAXIMUM_PARALLELISM").ok().as_deref())
}

fn parse_maximum_parallelism(value: Option<&str>) -> Result<usize, String> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(DEFAULT_MAXIMUM_PARALLELISM);
    };
    let maximum = value.parse::<usize>().map_err(|_| {
        "WORKFLOW_MAXIMUM_PARALLELISM must be an integer between 1 and 64".to_string()
    })?;
    if !(1..=ABSOLUTE_MAXIMUM_PARALLELISM).contains(&maximum) {
        return Err("WORKFLOW_MAXIMUM_PARALLELISM must be an integer between 1 and 64".to_string());
    }
    Ok(maximum)
}

#[derive(Debug, Clone)]
pub struct RunnerExecutionConfig {
    pub enabled: bool,
    pub origin_service_id: String,
    pub origin_instance_id: String,
    pub profiles: BTreeMap<String, ExecutionProfile>,
    pub command_templates: BTreeMap<String, CommandTemplate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunnerExecutionConfigFile {
    version: u16,
    #[serde(default)]
    profiles: Vec<ExecutionProfile>,
    #[serde(default)]
    command_templates: Vec<CommandTemplate>,
}

impl RunnerExecutionConfig {
    pub fn load() -> Result<Self, String> {
        let enabled = env_bool("LIGHT_WORKFLOW_RUNNER_ENABLED", false)?;
        let origin_service_id = env::var("LIGHT_WORKFLOW_ORIGIN_SERVICE_ID")
            .unwrap_or_else(|_| "com.networknt.light-workflow-1.0.0".to_string());
        let origin_instance_id = env::var("LIGHT_WORKFLOW_INSTANCE_ID")
            .unwrap_or_else(|_| "light-workflow-1".to_string());
        if origin_service_id.trim().is_empty() || origin_instance_id.trim().is_empty() {
            return Err("workflow origin service and instance IDs must not be empty".to_string());
        }

        let config = match env::var("LIGHT_WORKFLOW_RUNNER_CONFIG_FILE") {
            Ok(path) if !path.trim().is_empty() => load_file(Path::new(path.trim()))?,
            _ => RunnerExecutionConfigFile {
                version: 1,
                profiles: Vec::new(),
                command_templates: Vec::new(),
            },
        };
        if config.version != 1 {
            return Err(format!(
                "unsupported runner execution config version {}",
                config.version
            ));
        }
        let profiles = unique_by_id(config.profiles, |profile| profile.id.as_str(), "profile")?;
        let command_templates = unique_by_id(
            config.command_templates,
            |template| template.id.as_str(),
            "command template",
        )?;
        for template in command_templates.values() {
            validate_command_template(template)?;
        }
        if enabled && (profiles.is_empty() || command_templates.is_empty()) {
            return Err(
                "enabled runner execution requires at least one profile and command template"
                    .to_string(),
            );
        }

        Ok(Self {
            enabled,
            origin_service_id,
            origin_instance_id,
            profiles,
            command_templates,
        })
    }
}

fn load_file(path: &Path) -> Result<RunnerExecutionConfigFile, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_yaml::from_str(&content)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    let Ok(value) = env::var(name) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!("{name} must be true or false")),
    }
}

fn unique_by_id<T, F>(values: Vec<T>, id: F, kind: &str) -> Result<BTreeMap<String, T>, String>
where
    F: Fn(&T) -> &str,
{
    let mut output = BTreeMap::new();
    for value in values {
        let value_id = id(&value).trim().to_string();
        if value_id.is_empty() {
            return Err(format!("{kind} ID must not be empty"));
        }
        if output.insert(value_id.clone(), value).is_some() {
            return Err(format!("duplicate {kind} ID `{value_id}`"));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{RunnerExecutionConfigFile, parse_maximum_parallelism};

    #[test]
    fn maximum_parallelism_configuration_is_bounded() {
        assert_eq!(parse_maximum_parallelism(None).unwrap(), 64);
        assert_eq!(parse_maximum_parallelism(Some("3")).unwrap(), 3);
        assert!(parse_maximum_parallelism(Some("0")).is_err());
        assert!(parse_maximum_parallelism(Some("65")).is_err());
        assert!(parse_maximum_parallelism(Some("many")).is_err());
    }

    #[test]
    fn config_rejects_unknown_authority_fields() {
        let config = r#"
version: 1
allowHostDockerSocket: true
profiles: []
commandTemplates: []
"#;
        assert!(serde_yaml::from_str::<RunnerExecutionConfigFile>(config).is_err());
    }
}
