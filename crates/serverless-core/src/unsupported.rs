//! Light Lambda Init policy only. Shared security callers retain their configuration rules.
use crate::InitError;
use light_security::SecurityConfig;
use serde_json::{Map, Value};

pub struct SecuritySettings {
    pub enabled: bool,
    pub config: SecurityConfig,
}

pub fn object<'a>(
    value: &'a Value,
    error: &'static str,
) -> Result<&'a Map<String, Value>, InitError> {
    value.as_object().ok_or(InitError::Malformed(error))
}

pub fn boolean(map: &Map<String, Value>, key: &str, default: bool) -> Result<bool, InitError> {
    match map.get(key) {
        None => Ok(default),
        Some(Value::Bool(v)) => Ok(*v),
        _ => Err(InitError::Malformed(
            "security configuration: boolean required",
        )),
    }
}

pub fn strings(value: Option<&Value>) -> Result<Vec<String>, InitError> {
    match value {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) if s.trim().starts_with('[') => {
            let parsed: Value = serde_json::from_str(s)
                .map_err(|_| InitError::Malformed("security configuration: string list"))?;
            strings(Some(&parsed))
        }
        Some(Value::String(s)) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()),
        Some(Value::Array(v)) => v
            .iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or(InitError::Malformed("security configuration: string list"))
            })
            .collect(),
        _ => Err(InitError::Malformed("security configuration: string list")),
    }
}

pub fn validate_security(
    value: &Value,
    insecure_test: bool,
) -> Result<SecuritySettings, InitError> {
    let map = object(value, "security.yml: object required")?;
    const KEYS: &[&str] = &[
        "enabled",
        "enableVerifyJwt",
        "enableVerifySwt",
        "swtClientIdHeader",
        "swtClientSecretHeader",
        "enableExtractScopeToken",
        "enableVerifyScope",
        "skipVerifyScopeWithoutSpec",
        "ignoreJwtExpiry",
        "enableH2c",
        "enableMockJwt",
        "enableRelaxedKeyValidation",
        "jwt",
        "logJwtToken",
        "logClientUserScope",
        "enableJwtCache",
        "jwtCacheFullSize",
        "bootstrapFromKeyService",
        "jwksRefreshMinIntervalSecs",
        "providerId",
        "issuer",
        "audience",
        "skipPathPrefixes",
        "passThroughClaims",
    ];
    if map.keys().any(|k| !KEYS.contains(&k.as_str())) {
        return Err(InitError::Malformed("security.yml: unknown key"));
    }
    if let Some(jwt) = map.get("jwt") {
        let jwt = object(jwt, "security.yml: jwt object required")?;
        if jwt
            .keys()
            .any(|k| !["clockSkewInSeconds", "keyResolver", "certificate"].contains(&k.as_str()))
        {
            return Err(InitError::Malformed("security.yml: unknown jwt key"));
        }
        if jwt.get("keyResolver").is_some_and(|v| v != "JsonWebKeySet")
            || jwt
                .get("certificate")
                .is_some_and(|v| v.as_object().is_none_or(|m| !m.is_empty()))
        {
            return Err(InitError::Malformed(
                "security.yml: certificate/key resolver not supported until WP12",
            ));
        }
    }
    let config: SecurityConfig = serde_json::from_value(value.clone())
        .map_err(|_| InitError::Malformed("security.yml: invalid setting"))?;
    if config.jwt.clock_skew_in_seconds > 60 {
        return Err(InitError::Malformed(
            "security.yml: jwt.clockSkewInSeconds must be <=60",
        ));
    }
    if !insecure_test
        && (config.ignore_jwt_expiry || config.enable_mock_jwt || config.log_jwt_token)
    {
        return Err(InitError::Malformed(
            "security.yml: insecure setting requires LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG=1",
        ));
    }
    if config.enable_verify_swt
        || config.enable_relaxed_key_validation
        || config.enable_h2c
        || config.log_client_user_scope
        || !config.pass_through_claims.is_empty()
        || !config.provider_id.is_empty()
        || config.swt_client_id_header != SecurityConfig::default().swt_client_id_header
        || config.swt_client_secret_header != SecurityConfig::default().swt_client_secret_header
    {
        return Err(InitError::Malformed(
            "security.yml: feature not supported until WP12",
        ));
    }
    for prefix in &config.skip_path_prefixes {
        if !prefix.starts_with('/') {
            return Err(InitError::Malformed("security.yml: invalid skip prefix"));
        }
    }
    Ok(SecuritySettings {
        enabled: boolean(map, "enabled", true)?,
        config,
    })
}
pub mod audit;
pub mod metrics;
pub mod remaining;
