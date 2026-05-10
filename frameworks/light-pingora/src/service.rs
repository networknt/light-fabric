use crate::config_util::deserialize_string_map;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const PATH_PREFIX_SERVICE_FILE: &str = "pathPrefixService.yml";
pub const PATH_PREFIX_SERVICE_LEGACY_FILE: &str = "pathPrefixService.yaml";
pub const PATH_PREFIX_SERVICE_MODULE_ID: &str = "light-pingora/path-prefix-service";
pub const PATH_PREFIX_SERVICE_CONFIG_NAME: &str = "pathPrefixService";

const SERVICE_ID_HEADER: &str = "service_id";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PathPrefixServiceConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub mapping: BTreeMap<String, String>,
}

impl Default for PathPrefixServiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mapping: BTreeMap::new(),
        }
    }
}

pub fn load_path_prefix_service_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<PathPrefixServiceConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<PathPrefixServiceConfig>(runtime_config, PATH_PREFIX_SERVICE_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == PATH_PREFIX_SERVICE_FILE => {
            match runtime_config
                .module_registry
                .load_config::<PathPrefixServiceConfig>(
                    runtime_config,
                    PATH_PREFIX_SERVICE_LEGACY_FILE,
                ) {
                Ok(config) => config,
                Err(RuntimeError::MissingConfig(file))
                    if file == PATH_PREFIX_SERVICE_LEGACY_FILE =>
                {
                    PathPrefixServiceConfig::default()
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        PATH_PREFIX_SERVICE_MODULE_ID,
        PATH_PREFIX_SERVICE_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn apply_path_prefix_service(
    session: &mut Session,
    config: &PathPrefixServiceConfig,
    request_path: &str,
) -> pingora::Result<Option<String>> {
    if !config.enabled || has_header(session, SERVICE_ID_HEADER) {
        return Ok(None);
    }

    let Some(service_id) = service_id_for_path(&config.mapping, request_path) else {
        return Ok(None);
    };
    session
        .req_header_mut()
        .insert_header(SERVICE_ID_HEADER, service_id.clone())?;
    Ok(Some(service_id))
}

pub fn service_id_for_path(
    mapping: &BTreeMap<String, String>,
    request_path: &str,
) -> Option<String> {
    best_path_prefix(mapping, request_path).cloned()
}

pub(crate) fn best_path_prefix<'a, T>(
    mapping: &'a BTreeMap<String, T>,
    request_path: &str,
) -> Option<&'a T> {
    let request_path = normalize_request_path(request_path);
    mapping
        .iter()
        .filter_map(|(prefix, value)| {
            let prefix = normalize_prefix(prefix);
            path_matches_prefix(request_path.as_str(), prefix.as_str())
                .then_some((prefix.len(), value))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, value)| value)
}

fn has_header(session: &Session, name: &str) -> bool {
    session.req_header().headers.contains_key(name)
}

fn normalize_request_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn normalize_prefix(prefix: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix == "/" {
        return "/".to_string();
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        format!("/{prefix}")
    }
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_accepts_map_and_string_mapping() {
        let config: PathPrefixServiceConfig = serde_yaml::from_str(
            r#"
enabled: true
mapping: '{"/v1": "petstore", "/v2": "market"}'
"#,
        )
        .expect("parse string mapping");

        assert_eq!(config.mapping["/v1"], "petstore");
        assert_eq!(config.mapping["/v2"], "market");

        let config: PathPrefixServiceConfig = serde_yaml::from_str(
            r#"
mapping:
  /v1: petstore
"#,
        )
        .expect("parse map mapping");

        assert_eq!(config.mapping["/v1"], "petstore");
    }

    #[test]
    fn service_id_uses_longest_boundary_prefix() {
        let mapping = BTreeMap::from([
            ("/".to_string(), "root".to_string()),
            ("/v1".to_string(), "v1".to_string()),
            ("/v1/pets".to_string(), "pets".to_string()),
        ]);

        assert_eq!(
            service_id_for_path(&mapping, "/v1/pets/123"),
            Some("pets".to_string())
        );
        assert_eq!(
            service_id_for_path(&mapping, "/v1/orders"),
            Some("v1".to_string())
        );
        assert_eq!(
            service_id_for_path(&mapping, "/other"),
            Some("root".to_string())
        );
    }

    #[test]
    fn service_id_does_not_match_partial_segment() {
        let mapping = BTreeMap::from([("/v1/address".to_string(), "address".to_string())]);

        assert_eq!(service_id_for_path(&mapping, "/v1/address2"), None);
        assert_eq!(
            service_id_for_path(&mapping, "/v1/address/2"),
            Some("address".to_string())
        );
    }
}
