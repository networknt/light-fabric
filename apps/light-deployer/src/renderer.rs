use crate::model::{ResourceAction, ResourceSummary};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use serde_yaml::{Mapping, Value as YamlValue};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("template YAML is invalid: {0}")]
    InvalidYaml(#[from] serde_yaml::Error),
    #[error("missing value for placeholder `{0}`")]
    MissingValue(String),
    #[error("resource is missing required field `{0}`")]
    MissingResourceField(&'static str),
}

#[derive(Debug, Clone)]
pub struct TemplateDocument {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct RenderedManifestSet {
    pub documents: Vec<RenderedDocument>,
    pub manifest_hash: String,
}

#[derive(Debug, Clone)]
pub struct RenderedDocument {
    pub value: YamlValue,
    pub redacted: YamlValue,
    pub summary: ResourceSummary,
}

pub trait Renderer: Send + Sync {
    fn render(
        &self,
        templates: &[TemplateDocument],
        values: &JsonValue,
        namespace: &str,
    ) -> Result<RenderedManifestSet, RenderError>;
}

#[derive(Debug, Default, Clone)]
pub struct AstRenderer;

impl Renderer for AstRenderer {
    fn render(
        &self,
        templates: &[TemplateDocument],
        values: &JsonValue,
        namespace: &str,
    ) -> Result<RenderedManifestSet, RenderError> {
        let mut documents = Vec::new();
        let mut hasher = Sha256::new();

        for template in templates {
            hasher.update(template.name.as_bytes());
            for (_index, document) in
                serde_yaml::Deserializer::from_str(&template.content).enumerate()
            {
                let mut value = YamlValue::deserialize(document)?;
                if value.is_null() {
                    continue;
                }
                replace_placeholders(&mut value, values)?;
                ensure_namespace(&mut value, namespace);
                let summary = summarize_resource(&value, ResourceAction::Unchanged)?;
                let redacted = redact_yaml(&value);
                let rendered_yaml = serde_yaml::to_string(&value)?;
                hasher.update(rendered_yaml.as_bytes());
                documents.push(RenderedDocument {
                    value,
                    redacted,
                    summary,
                });
            }
        }

        Ok(RenderedManifestSet {
            documents,
            manifest_hash: format!("sha256:{:x}", hasher.finalize()),
        })
    }
}

pub fn add_management_labels(
    value: &mut YamlValue,
    host_id: &str,
    instance_id: &str,
    request_id: &str,
) {
    let YamlValue::Mapping(root) = value else {
        return;
    };
    let metadata_key = YamlValue::String("metadata".to_string());
    let metadata = root
        .entry(metadata_key)
        .or_insert_with(|| YamlValue::Mapping(Mapping::new()));
    let YamlValue::Mapping(metadata) = metadata else {
        return;
    };
    let labels_key = YamlValue::String("labels".to_string());
    let labels = metadata
        .entry(labels_key)
        .or_insert_with(|| YamlValue::Mapping(Mapping::new()));
    let YamlValue::Mapping(labels) = labels else {
        return;
    };

    labels.insert(
        YamlValue::String("app.kubernetes.io/managed-by".to_string()),
        YamlValue::String("light-deployer".to_string()),
    );
    labels.insert(
        YamlValue::String("lightapi.net/host-id".to_string()),
        YamlValue::String(host_id.to_string()),
    );
    labels.insert(
        YamlValue::String("lightapi.net/instance-id".to_string()),
        YamlValue::String(instance_id.to_string()),
    );
    labels.insert(
        YamlValue::String("lightapi.net/request-id".to_string()),
        YamlValue::String(request_id.to_string()),
    );
}

pub fn summarize_resource(
    value: &YamlValue,
    action: ResourceAction,
) -> Result<ResourceSummary, RenderError> {
    Ok(ResourceSummary {
        api_version: required_string(value, &["apiVersion"], "apiVersion")?,
        kind: required_string(value, &["kind"], "kind")?,
        namespace: string_at(value, &["metadata", "namespace"]).unwrap_or_else(|| "default".into()),
        name: required_string(value, &["metadata", "name"], "metadata.name")?,
        action,
    })
}

pub fn redact_yaml(value: &YamlValue) -> YamlValue {
    match value {
        YamlValue::Mapping(mapping) => redact_mapping(mapping),
        YamlValue::Sequence(sequence) => {
            YamlValue::Sequence(sequence.iter().map(redact_yaml).collect())
        }
        _ => value.clone(),
    }
}

fn redact_mapping(mapping: &Mapping) -> YamlValue {
    if mapping.contains_key(YamlValue::String("$secret".into()))
        || mapping.contains_key(YamlValue::String("$sealedSecret".into()))
    {
        return YamlValue::String("<REDACTED>".to_string());
    }

    let kind = mapping
        .get(YamlValue::String("kind".into()))
        .and_then(YamlValue::as_str);
    if kind == Some("Secret") {
        return redact_secret_resource(mapping);
    }

    let mut redacted = Mapping::new();
    for (key, value) in mapping {
        redacted.insert(key.clone(), redact_yaml(value));
    }
    YamlValue::Mapping(redacted)
}

fn redact_secret_resource(mapping: &Mapping) -> YamlValue {
    let mut redacted = Mapping::new();
    for (key, value) in mapping {
        let key_name = key.as_str().unwrap_or_default();
        let redacted_value = if matches!(key_name, "data" | "stringData") {
            redact_secret_data(value)
        } else {
            redact_yaml(value)
        };
        redacted.insert(key.clone(), redacted_value);
    }
    YamlValue::Mapping(redacted)
}

fn redact_secret_data(value: &YamlValue) -> YamlValue {
    match value {
        YamlValue::Mapping(mapping) => {
            let mut redacted = Mapping::new();
            for (key, _) in mapping {
                redacted.insert(key.clone(), YamlValue::String("<REDACTED>".to_string()));
            }
            YamlValue::Mapping(redacted)
        }
        _ => YamlValue::String("<REDACTED>".to_string()),
    }
}

fn replace_placeholders(value: &mut YamlValue, values: &JsonValue) -> Result<(), RenderError> {
    match value {
        YamlValue::String(input) => {
            *input = replace_placeholders_in_string(input, values)?;
        }
        YamlValue::Sequence(sequence) => {
            for item in sequence {
                replace_placeholders(item, values)?;
            }
        }
        YamlValue::Mapping(mapping) => {
            for (_, item) in mapping {
                replace_placeholders(item, values)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn replace_placeholders_in_string(input: &str, values: &JsonValue) -> Result<String, RenderError> {
    let mut output = String::with_capacity(input.len());
    let mut rest = input;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            output.push_str(&rest[start..]);
            return Ok(output);
        };
        let expression = &after_start[..end];
        let replacement = resolve_expression(expression, values)?;
        output.push_str(&replacement);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

fn resolve_expression(expression: &str, values: &JsonValue) -> Result<String, RenderError> {
    let (path, default) = expression
        .split_once(':')
        .map(|(path, default)| (path.trim(), Some(default)))
        .unwrap_or_else(|| (expression.trim(), None));

    if let Some(value) = json_path(values, path) {
        return Ok(json_to_string(value));
    }

    default
        .map(str::to_string)
        .ok_or_else(|| RenderError::MissingValue(path.to_string()))
}

fn json_path<'a>(value: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    let mut current = value;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn json_to_string(value: &JsonValue) -> String {
    match value {
        JsonValue::Null => String::new(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::String(value) => value.clone(),
        JsonValue::Array(_) | JsonValue::Object(_) => value.to_string(),
    }
}

fn ensure_namespace(value: &mut YamlValue, namespace: &str) {
    let YamlValue::Mapping(root) = value else {
        return;
    };
    let metadata_key = YamlValue::String("metadata".to_string());
    let metadata = root
        .entry(metadata_key)
        .or_insert_with(|| YamlValue::Mapping(Mapping::new()));
    let YamlValue::Mapping(metadata) = metadata else {
        return;
    };
    metadata
        .entry(YamlValue::String("namespace".to_string()))
        .or_insert_with(|| YamlValue::String(namespace.to_string()));
}

fn required_string(
    value: &YamlValue,
    path: &[&str],
    field: &'static str,
) -> Result<String, RenderError> {
    string_at(value, path).ok_or(RenderError::MissingResourceField(field))
}

fn string_at(value: &YamlValue, path: &[&str]) -> Option<String> {
    let mut current = value;
    for segment in path {
        let YamlValue::Mapping(mapping) = current else {
            return None;
        };
        current = mapping.get(YamlValue::String((*segment).to_string()))?;
    }
    current.as_str().map(str::to_string)
}

pub fn redacted_unified_diff(current: &[YamlValue], target: &[YamlValue]) -> String {
    let current_yaml = stable_yaml_list(current);
    let target_yaml = stable_yaml_list(target);
    simple_unified_diff(&current_yaml, &target_yaml)
}

fn stable_yaml_list(values: &[YamlValue]) -> String {
    values
        .iter()
        .map(|value| serde_yaml::to_string(value).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("---\n")
}

fn simple_unified_diff(current: &str, target: &str) -> String {
    if current == target {
        return String::new();
    }
    let current_lines: Vec<_> = current.lines().collect();
    let target_lines: Vec<_> = target.lines().collect();
    let mut diff = String::from("--- current\n+++ target\n");
    let max = current_lines.len().max(target_lines.len());
    for index in 0..max {
        match (current_lines.get(index), target_lines.get(index)) {
            (Some(left), Some(right)) if left == right => {
                diff.push(' ');
                diff.push_str(left);
                diff.push('\n');
            }
            (Some(left), Some(right)) => {
                diff.push('-');
                diff.push_str(left);
                diff.push('\n');
                diff.push('+');
                diff.push_str(right);
                diff.push('\n');
            }
            (Some(left), None) => {
                diff.push('-');
                diff.push_str(left);
                diff.push('\n');
            }
            (None, Some(right)) => {
                diff.push('+');
                diff.push_str(right);
                diff.push('\n');
            }
            (None, None) => {}
        }
    }
    diff
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_placeholder_defaults() {
        let renderer = AstRenderer;
        let templates = [TemplateDocument {
            name: "deployment.yaml".into(),
            content: r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ${name}
spec:
  replicas: ${replicas:1}
"#
            .into(),
        }];
        let rendered = renderer
            .render(&templates, &json!({ "name": "petstore" }), "dev")
            .unwrap();
        assert_eq!(rendered.documents[0].summary.name, "petstore");
    }

    #[test]
    fn redacts_secret_values() {
        let value: YamlValue = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: Secret
metadata:
  name: db
data:
  password: c2VjcmV0
"#,
        )
        .unwrap();
        let redacted = serde_yaml::to_string(&redact_yaml(&value)).unwrap();
        assert!(redacted.contains("<REDACTED>"));
        assert!(!redacted.contains("c2VjcmV0"));
    }

    #[test]
    fn does_not_redact_configmap_data() {
        let value: YamlValue = serde_yaml::from_str(
            r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: app
data:
  LOG_LEVEL: info
"#,
        )
        .unwrap();
        let redacted = serde_yaml::to_string(&redact_yaml(&value)).unwrap();
        assert!(redacted.contains("LOG_LEVEL: info"));
    }
}
