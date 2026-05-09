use crate::config_util::{
    best_prefix, deserialize_string_list, deserialize_string_map, deserialize_typed_map,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::http::ResponseHeader;
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const HEADER_FILE: &str = "header.yml";
pub const HEADER_MODULE_ID: &str = "light-pingora/header";
pub const HEADER_CONFIG_NAME: &str = "header";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub request: HeaderMutation,
    #[serde(default)]
    pub response: HeaderMutation,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub path_prefix_header: BTreeMap<String, HeaderPathPrefixConfig>,
}

impl Default for HeaderConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            request: HeaderMutation::default(),
            response: HeaderMutation::default(),
            path_prefix_header: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderMutation {
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub remove: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub update: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HeaderPathPrefixConfig {
    #[serde(default)]
    pub request: HeaderMutation,
    #[serde(default)]
    pub response: HeaderMutation,
}

pub fn load_header_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<HeaderConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<HeaderConfig>(runtime_config, HEADER_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == HEADER_FILE => HeaderConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        HEADER_MODULE_ID,
        HEADER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn apply_header_request(
    session: &mut Session,
    config: &HeaderConfig,
    request_path: &str,
) -> pingora::Result<()> {
    if !config.enabled {
        return Ok(());
    }
    apply_request_mutation(session, &config.request)?;
    if let Some((_, prefix_config)) = best_prefix(&config.path_prefix_header, request_path) {
        apply_request_mutation(session, &prefix_config.request)?;
    }
    Ok(())
}

pub fn apply_header_response(
    response: &mut ResponseHeader,
    config: &HeaderConfig,
    request_path: &str,
) -> pingora::Result<()> {
    if !config.enabled {
        return Ok(());
    }
    apply_response_mutation(response, &config.response)?;
    if let Some((_, prefix_config)) = best_prefix(&config.path_prefix_header, request_path) {
        apply_response_mutation(response, &prefix_config.response)?;
    }
    Ok(())
}

fn apply_request_mutation(session: &mut Session, mutation: &HeaderMutation) -> pingora::Result<()> {
    let headers = session.req_header_mut();
    for name in &mutation.remove {
        headers.remove_header(name.as_str());
    }
    for (name, value) in &mutation.update {
        headers.insert_header(name.to_string(), value.to_string())?;
    }
    Ok(())
}

fn apply_response_mutation(
    response: &mut ResponseHeader,
    mutation: &HeaderMutation,
) -> pingora::Result<()> {
    for name in &mutation.remove {
        response.remove_header(name.as_str());
    }
    for (name, value) in &mutation.update {
        response.insert_header(name.to_string(), value.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_config_accepts_string_shortcuts() {
        let config: HeaderConfig = serde_yaml::from_str(
            r#"
enabled: true
request:
  remove: a,b
  update: 'x-one=1,x-two=2'
pathPrefixHeader:
  /api:
    response:
      update:
        x-api: true
"#,
        )
        .expect("parse header config");

        assert_eq!(config.request.remove, ["a", "b"]);
        assert_eq!(config.request.update["x-one"], "1");
        assert_eq!(
            config
                .path_prefix_header
                .get("/api")
                .expect("path config")
                .response
                .update["x-api"],
            "true"
        );
    }
}
