use crate::apikey::{ApiKeyConfig, verify_required_api_key};
use crate::basic_auth::{BasicAuthConfig, verify_basic_auth};
use crate::config_util::{deserialize_string_list, deserialize_typed_list, request_header};
use crate::security::{AuthPrincipal, HandlerRejection, SecurityRuntime, verify_jwt_request};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};

pub const UNIFIED_SECURITY_FILE: &str = "unified-security.yml";
pub const UNIFIED_SECURITY_MODULE_ID: &str = "light-pingora/unified-security";
pub const UNIFIED_SECURITY_CONFIG_NAME: &str = "unified-security";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedSecurityConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub anonymous_prefixes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub path_prefix_auths: Vec<UnifiedPathAuth>,
}

impl Default for UnifiedSecurityConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            anonymous_prefixes: Vec::new(),
            path_prefix_auths: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedPathAuth {
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub basic: bool,
    #[serde(default)]
    pub jwt: bool,
    #[serde(default)]
    pub sjwt: bool,
    #[serde(default)]
    pub swt: bool,
    #[serde(default)]
    pub apikey: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub jwk_service_ids: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub sjwk_service_ids: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub swt_service_ids: Vec<String>,
}

pub fn load_unified_security_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<UnifiedSecurityConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<UnifiedSecurityConfig>(runtime_config, UNIFIED_SECURITY_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == UNIFIED_SECURITY_FILE => {
            UnifiedSecurityConfig::default()
        }
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        UNIFIED_SECURITY_MODULE_ID,
        UNIFIED_SECURITY_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub async fn verify_unified_security(
    session: &mut Session,
    config: &UnifiedSecurityConfig,
    basic_config: Option<&BasicAuthConfig>,
    api_key_config: Option<&ApiKeyConfig>,
    security_runtime: Option<&SecurityRuntime>,
    request_path: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    if !config.enabled || is_anonymous(config, request_path) {
        return Ok(None);
    }
    let Some(rule) = best_auth_rule(config, request_path) else {
        return Err(HandlerRejection::forbidden(
            "request path is not configured for unified security",
        ));
    };

    if rule.basic || rule.jwt || rule.sjwt || rule.swt {
        let authorization = request_header(session, "authorization")
            .ok_or_else(|| HandlerRejection::unauthorized("Authorization header is required"))?;
        let (scheme, _) = authorization
            .split_once(' ')
            .ok_or_else(|| HandlerRejection::unauthorized("invalid Authorization header"))?;
        if scheme.eq_ignore_ascii_case("basic") {
            if !rule.basic {
                return Err(HandlerRejection::unauthorized(
                    "Basic authentication is not allowed for this path",
                ));
            }
            let config = basic_config.ok_or_else(|| {
                HandlerRejection::new(500, "ERR10001", "basic-auth.yml is not active")
            })?;
            verify_basic_auth(session, config, request_path)?;
            return Ok(None);
        }
        if scheme.eq_ignore_ascii_case("bearer") {
            if rule.jwt {
                let runtime = security_runtime.ok_or_else(|| {
                    HandlerRejection::new(500, "ERR10001", "security.yml is not active")
                })?;
                return verify_jwt_request(session, runtime, request_path).await;
            }
            if rule.sjwt || rule.swt {
                return Err(HandlerRejection::new(
                    501,
                    "ERR10001",
                    "SJWT/SWT verification is not implemented in light-pingora phase 4",
                ));
            }
        }
        return Err(HandlerRejection::unauthorized(
            "Authorization scheme is not allowed for this path",
        ));
    }

    if rule.apikey {
        let config = api_key_config
            .ok_or_else(|| HandlerRejection::new(500, "ERR10001", "apikey.yml is not active"))?;
        verify_required_api_key(session, config, request_path)?;
    }
    Ok(None)
}

fn is_anonymous(config: &UnifiedSecurityConfig, request_path: &str) -> bool {
    config
        .anonymous_prefixes
        .iter()
        .any(|prefix| request_path.starts_with(prefix.as_str()))
}

fn best_auth_rule<'a>(
    config: &'a UnifiedSecurityConfig,
    request_path: &str,
) -> Option<&'a UnifiedPathAuth> {
    config
        .path_prefix_auths
        .iter()
        .filter(|rule| request_path.starts_with(rule.prefix.as_str()))
        .max_by_key(|rule| rule.prefix.len())
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_security_accepts_java_style_lists() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes: /health,/info
pathPrefixAuths: '[{"prefix":"/api","jwt":true},{"prefix":"/admin","basic":true}]'
"#,
        )
        .expect("parse unified config");

        assert!(is_anonymous(&config, "/health"));
        assert!(best_auth_rule(&config, "/api/pets").unwrap().jwt);
        assert!(best_auth_rule(&config, "/admin/users").unwrap().basic);
    }
}
