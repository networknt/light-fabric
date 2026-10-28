use crate::{policy_adapter::PolicyRequestRef, security::HandlerRejection};
pub use gateway_policy::apikey::*;
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
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
    gateway_policy::apikey::verify_api_key(&PolicyRequestRef(session), config, request_path)
}
pub fn verify_required_api_key(
    session: &Session,
    config: &ApiKeyConfig,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    gateway_policy::apikey::verify_required_api_key(
        &PolicyRequestRef(session),
        config,
        request_path,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use pbkdf2::pbkdf2_hmac;
    use sha1::Sha1;

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
