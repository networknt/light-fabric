use crate::config_util::{deserialize_typed_list, request_header};
use crate::security::HandlerRejection;
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pbkdf2::pbkdf2_hmac;
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use subtle::ConstantTimeEq;

pub const APIKEY_FILE: &str = "apikey.yml";
pub const APIKEY_MODULE_ID: &str = "light-pingora/apikey";
pub const APIKEY_CONFIG_NAME: &str = "apikey";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub hash_enabled: bool,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub path_prefix_auths: Vec<ApiKeyRule>,
}

impl Default for ApiKeyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hash_enabled: false,
            path_prefix_auths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyRule {
    #[serde(default)]
    pub path_prefix: String,
    #[serde(default)]
    pub header_name: String,
    #[serde(default)]
    pub api_key: String,
}

pub fn load_api_key_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<ApiKeyConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<ApiKeyConfig>(runtime_config, APIKEY_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == APIKEY_FILE => ApiKeyConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        APIKEY_MODULE_ID,
        APIKEY_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("apiKey")],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn verify_api_key(
    session: &Session,
    config: &ApiKeyConfig,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    verify_api_key_internal(session, config, request_path, false)
}

pub fn verify_required_api_key(
    session: &Session,
    config: &ApiKeyConfig,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    verify_api_key_internal(session, config, request_path, true)
}

fn verify_api_key_internal(
    session: &Session,
    config: &ApiKeyConfig,
    request_path: &str,
    require_match: bool,
) -> Result<(), HandlerRejection> {
    if !config.enabled {
        return Ok(());
    }

    let Some(rule) = best_rule(config, request_path) else {
        if require_match {
            return Err(HandlerRejection::new(
                401,
                "ERR10075",
                "API key rule is missing for this path",
            ));
        }
        return Ok(());
    };
    if rule.header_name.trim().is_empty() || rule.api_key.trim().is_empty() {
        return Err(HandlerRejection::new(
            500,
            "ERR10001",
            "API key rule is missing headerName or apiKey",
        ));
    }

    let provided = request_header(session, rule.header_name.as_str()).ok_or_else(|| {
        HandlerRejection::new(401, "ERR10075", "API key header is missing or invalid")
    })?;
    let accepted = if config.hash_enabled {
        validate_pbkdf2_hash(provided.as_str(), rule.api_key.as_str())
    } else {
        provided.as_bytes().ct_eq(rule.api_key.as_bytes()).into()
    };
    if accepted {
        Ok(())
    } else {
        Err(HandlerRejection::new(
            401,
            "ERR10075",
            "API key header is missing or invalid",
        ))
    }
}

fn best_rule<'a>(config: &'a ApiKeyConfig, request_path: &str) -> Option<&'a ApiKeyRule> {
    config
        .path_prefix_auths
        .iter()
        .filter(|rule| request_path.starts_with(rule.path_prefix.as_str()))
        .max_by_key(|rule| rule.path_prefix.len())
}

fn validate_pbkdf2_hash(password: &str, stored: &str) -> bool {
    let mut parts = stored.split(':');
    let Some(iterations) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
        return false;
    };
    let Some(salt_hex) = parts.next() else {
        return false;
    };
    let Some(hash_hex) = parts.next() else {
        return false;
    };
    if parts.next().is_some() {
        return false;
    }
    let Ok(salt) = hex::decode(salt_hex) else {
        return false;
    };
    let Ok(expected) = hex::decode(hash_hex) else {
        return false;
    };
    let mut derived = vec![0; expected.len()];
    pbkdf2_hmac::<Sha1>(password.as_bytes(), &salt, iterations, &mut derived);
    derived.ct_eq(expected.as_slice()).into()
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apikey_config_accepts_json_string_rules() {
        let config: ApiKeyConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths: '[{"pathPrefix":"/api","headerName":"x-api-key","apiKey":"secret"}]'
"#,
        )
        .expect("parse apikey config");

        let rule = best_rule(&config, "/api/pets").expect("rule");
        assert_eq!(rule.header_name, "x-api-key");
    }

    #[test]
    fn validates_light_4j_pbkdf2_format() {
        let salt = "00112233445566778899aabbccddeeff";
        let mut expected = vec![0; 64];
        pbkdf2_hmac::<Sha1>(
            "secret".as_bytes(),
            &hex::decode(salt).unwrap(),
            1000,
            &mut expected,
        );
        let stored = format!("1000:{salt}:{}", hex::encode(expected));

        assert!(validate_pbkdf2_hash("secret", stored.as_str()));
        assert!(!validate_pbkdf2_hash("other", stored.as_str()));
    }
}
