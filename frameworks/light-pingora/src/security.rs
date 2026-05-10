use crate::config_util::{
    deserialize_string_list, deserialize_string_map, request_header, request_header as header_value,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SECURITY_FILE: &str = "security.yml";
pub const SECURITY_MODULE_ID: &str = "light-pingora/security";
pub const SECURITY_CONFIG_NAME: &str = "security";

const AUTHORIZATION: &str = "authorization";
const WWW_AUTHENTICATE: &str = "www-authenticate";
const SCOPE_TOKEN: &str = "X-Scope-Token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerRejection {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub headers: Vec<(String, String)>,
}

impl HandlerRejection {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            headers: Vec::new(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "ERR10002", message).with_header(WWW_AUTHENTICATE, "Bearer")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "ERR10007", message)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthPrincipal {
    pub client_id: Option<String>,
    pub user_id: Option<String>,
    pub issuer: Option<String>,
    pub email: Option<String>,
    pub host: Option<String>,
    pub role: Option<String>,
    pub claims: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub enable_verify_jwt: bool,
    #[serde(default)]
    pub enable_verify_swt: bool,
    #[serde(default = "default_swt_client_id_header")]
    pub swt_client_id_header: String,
    #[serde(default = "default_swt_client_secret_header")]
    pub swt_client_secret_header: String,
    #[serde(default = "default_true")]
    pub enable_extract_scope_token: bool,
    #[serde(default = "default_true")]
    pub enable_verify_scope: bool,
    #[serde(default)]
    pub skip_verify_scope_without_spec: bool,
    #[serde(default)]
    pub ignore_jwt_expiry: bool,
    #[serde(default)]
    pub enable_h2c: bool,
    #[serde(default)]
    pub enable_mock_jwt: bool,
    #[serde(default)]
    pub enable_relaxed_key_validation: bool,
    #[serde(default)]
    pub jwt: SecurityJwtConfig,
    #[serde(default)]
    pub log_jwt_token: bool,
    #[serde(default)]
    pub log_client_user_scope: bool,
    #[serde(default = "default_true")]
    pub enable_jwt_cache: bool,
    #[serde(default = "default_jwt_cache_full_size")]
    pub jwt_cache_full_size: usize,
    #[serde(default)]
    pub bootstrap_from_key_service: bool,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub audience: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub skip_path_prefixes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub pass_through_claims: BTreeMap<String, String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enable_verify_jwt: true,
            enable_verify_swt: false,
            swt_client_id_header: default_swt_client_id_header(),
            swt_client_secret_header: default_swt_client_secret_header(),
            enable_extract_scope_token: true,
            enable_verify_scope: true,
            skip_verify_scope_without_spec: false,
            ignore_jwt_expiry: false,
            enable_h2c: false,
            enable_mock_jwt: false,
            enable_relaxed_key_validation: false,
            jwt: SecurityJwtConfig::default(),
            log_jwt_token: false,
            log_client_user_scope: false,
            enable_jwt_cache: true,
            jwt_cache_full_size: default_jwt_cache_full_size(),
            bootstrap_from_key_service: false,
            provider_id: String::new(),
            issuer: String::new(),
            audience: Vec::new(),
            skip_path_prefixes: Vec::new(),
            pass_through_claims: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityJwtConfig {
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub certificate: BTreeMap<String, String>,
    #[serde(default = "default_clock_skew_seconds")]
    pub clock_skew_in_seconds: u64,
    #[serde(default)]
    pub key_resolver: String,
}

impl Default for SecurityJwtConfig {
    fn default() -> Self {
        Self {
            certificate: BTreeMap::new(),
            clock_skew_in_seconds: default_clock_skew_seconds(),
            key_resolver: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct SecurityRuntime {
    pub config: SecurityConfig,
    certificates: Arc<BTreeMap<String, Vec<u8>>>,
    cache: Arc<Mutex<BTreeMap<String, CachedPrincipal>>>,
}

impl SecurityRuntime {
    fn new(config: SecurityConfig, runtime_config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let mut certificates = BTreeMap::new();
        for (kid, file_name) in &config.jwt.certificate {
            let file_name = file_name.trim();
            if file_name.is_empty() {
                continue;
            }
            let path = resolve_config_file(runtime_config, file_name);
            let content = std::fs::read(&path).map_err(|error| {
                RuntimeError::Unsupported(format!(
                    "failed to read JWT certificate `{}`: {error}",
                    path.display()
                ))
            })?;
            certificates.insert(kid.clone(), content);
        }
        Ok(Self {
            config,
            certificates: Arc::new(certificates),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtExpiryMode {
    Enforce,
    Ignore,
}

#[derive(Clone)]
struct CachedPrincipal {
    principal: AuthPrincipal,
    expires_at: Option<u64>,
}

pub fn load_security_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<SecurityRuntime>, RuntimeError> {
    load_security_runtime_from_file(
        runtime_config,
        active,
        SECURITY_FILE,
        SECURITY_MODULE_ID,
        SECURITY_CONFIG_NAME,
    )
}

pub fn load_security_runtime_from_file(
    runtime_config: &RuntimeConfig,
    active: bool,
    file_name: &str,
    module_id: &str,
    config_name: &str,
) -> Result<Option<SecurityRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<SecurityConfig>(runtime_config, file_name)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == file_name => SecurityConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        module_id,
        config_name,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("swtClientSecretHeader")],
        config.enable_verify_jwt || config.enable_verify_swt,
        Some(config.enable_verify_jwt || config.enable_verify_swt),
        true,
    )?;

    Ok(
        (config.enable_verify_jwt || config.enable_verify_swt || config.enable_mock_jwt)
            .then(|| SecurityRuntime::new(config, runtime_config))
            .transpose()?,
    )
}

pub fn verify_jwt_token(
    runtime: &SecurityRuntime,
    token: &str,
    expiry_mode: JwtExpiryMode,
) -> Result<AuthPrincipal, HandlerRejection> {
    let token = token.trim();
    if token.is_empty() {
        return Err(HandlerRejection::unauthorized("missing bearer token"));
    }
    if matches!(expiry_mode, JwtExpiryMode::Enforce)
        && let Some(principal) = cached_principal(runtime, token)
    {
        return Ok(principal);
    }

    let header =
        decode_header(token).map_err(|_| HandlerRejection::unauthorized("invalid JWT header"))?;
    let pem = certificate_for_token(runtime, header.kid.as_deref())
        .ok_or_else(|| HandlerRejection::unauthorized("JWT key is not configured"))?;
    let key = decoding_key_for_alg(pem.as_slice(), header.alg)
        .map_err(|_| HandlerRejection::unauthorized("JWT key cannot verify token algorithm"))?;
    let validation = validation_for_mode(&runtime.config, header.alg, expiry_mode);
    let decoded = decode::<JsonValue>(token, &key, &validation)
        .map_err(|_| HandlerRejection::unauthorized("JWT validation failed"))?;
    validate_issuer_and_audience(&runtime.config, &decoded.claims)?;
    let principal = principal_from_claims(decoded.claims);
    if matches!(expiry_mode, JwtExpiryMode::Enforce) {
        cache_principal(runtime, token.to_string(), &principal);
    }
    Ok(principal)
}

pub fn verify_jwt_request(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    let config = &runtime.config;
    if request_path_is_skipped(config, request_path) {
        return Ok(None);
    }
    if !config.enable_h2c && is_h2c_upgrade(session) {
        return Err(HandlerRejection::new(
            405,
            "ERR10048",
            "cleartext HTTP/2 upgrade is not allowed",
        ));
    }
    if config.enable_mock_jwt {
        return Ok(Some(mock_principal()));
    }
    if !config.enable_verify_jwt {
        return Ok(None);
    }

    let token = bearer_token(session).or_else(|| {
        config
            .enable_extract_scope_token
            .then(|| request_header(session, SCOPE_TOKEN))
            .flatten()
    });
    let token = token.ok_or_else(|| HandlerRejection::unauthorized("missing bearer token"))?;

    if let Some(principal) = cached_principal(runtime, token.as_str()) {
        apply_pass_through_claims(session, config, &principal)?;
        return Ok(Some(principal));
    }

    let principal = verify_jwt_token(runtime, token.as_str(), JwtExpiryMode::Enforce)?;
    apply_pass_through_claims(session, config, &principal)?;
    Ok(Some(principal))
}

fn validation_for_mode(
    config: &SecurityConfig,
    algorithm: Algorithm,
    expiry_mode: JwtExpiryMode,
) -> Validation {
    let mut validation = Validation::new(algorithm);
    validation.leeway = config.jwt.clock_skew_in_seconds;
    validation.validate_aud = false;
    if config.ignore_jwt_expiry || matches!(expiry_mode, JwtExpiryMode::Ignore) {
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
    }
    validation
}

fn validate_issuer_and_audience(
    config: &SecurityConfig,
    claims: &JsonValue,
) -> Result<(), HandlerRejection> {
    let issuer = config.issuer.trim();
    if !issuer.is_empty() && claim_string(claims, "iss").as_deref() != Some(issuer) {
        return Err(HandlerRejection::unauthorized(
            "JWT issuer validation failed",
        ));
    }

    if !config.audience.is_empty() {
        let configured = config
            .audience
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !configured.is_empty() && !claim_audience_matches(claims, &configured) {
            return Err(HandlerRejection::unauthorized(
                "JWT audience validation failed",
            ));
        }
    }
    Ok(())
}

fn claim_audience_matches(claims: &JsonValue, configured: &[&str]) -> bool {
    match claims.get("aud") {
        Some(JsonValue::String(value)) => configured.iter().any(|expected| value == expected),
        Some(JsonValue::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|audience| configured.iter().any(|expected| audience == *expected))
        }),
        _ => false,
    }
}

fn certificate_for_token(runtime: &SecurityRuntime, kid: Option<&str>) -> Option<Vec<u8>> {
    if let Some(kid) = kid {
        if let Some(certificate) = runtime.certificates.get(kid) {
            return Some(certificate.clone());
        }
    }
    if runtime.certificates.len() == 1 || runtime.config.enable_relaxed_key_validation {
        return runtime.certificates.values().next().cloned();
    }
    None
}

fn decoding_key_for_alg(
    pem: &[u8],
    algorithm: Algorithm,
) -> Result<DecodingKey, jsonwebtoken::errors::Error> {
    match algorithm {
        Algorithm::RS256
        | Algorithm::RS384
        | Algorithm::RS512
        | Algorithm::PS256
        | Algorithm::PS384
        | Algorithm::PS512 => DecodingKey::from_rsa_pem(pem),
        Algorithm::ES256 | Algorithm::ES384 => DecodingKey::from_ec_pem(pem),
        _ => DecodingKey::from_rsa_pem(pem),
    }
}

fn bearer_token(session: &Session) -> Option<String> {
    let value = header_value(session, AUTHORIZATION)?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn principal_from_claims(claims: JsonValue) -> AuthPrincipal {
    AuthPrincipal {
        client_id: claim_string(&claims, "client_id").or_else(|| claim_string(&claims, "cid")),
        user_id: claim_string(&claims, "user_id")
            .or_else(|| claim_string(&claims, "uid"))
            .or_else(|| claim_string(&claims, "sub")),
        issuer: claim_string(&claims, "iss"),
        email: claim_string(&claims, "email").or_else(|| claim_string(&claims, "eml")),
        host: claim_string(&claims, "host"),
        role: claim_string(&claims, "role"),
        claims,
    }
}

fn claim_string(claims: &JsonValue, name: &str) -> Option<String> {
    let value = claims.get(name)?;
    if let Some(value) = value.as_str() {
        return Some(value.to_string());
    }
    if value.is_number() || value.is_boolean() {
        return Some(value.to_string());
    }
    None
}

fn apply_pass_through_claims(
    session: &mut Session,
    config: &SecurityConfig,
    principal: &AuthPrincipal,
) -> Result<(), HandlerRejection> {
    for (claim_name, header_name) in &config.pass_through_claims {
        let Some(value) = claim_string(&principal.claims, claim_name) else {
            continue;
        };
        session
            .req_header_mut()
            .insert_header(header_name.to_string(), value)
            .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid pass-through header"))?;
    }
    Ok(())
}

fn cached_principal(runtime: &SecurityRuntime, token: &str) -> Option<AuthPrincipal> {
    if !runtime.config.enable_jwt_cache {
        return None;
    }
    let cached = runtime
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(token)
        .cloned()?;
    if runtime.config.ignore_jwt_expiry {
        return Some(cached.principal);
    }
    let Some(expires_at) = cached.expires_at else {
        return Some(cached.principal);
    };
    (now_epoch_seconds() <= expires_at.saturating_add(runtime.config.jwt.clock_skew_in_seconds))
        .then_some(cached.principal)
}

fn cache_principal(runtime: &SecurityRuntime, token: String, principal: &AuthPrincipal) {
    if !runtime.config.enable_jwt_cache {
        return;
    }
    let expires_at = principal.claims.get("exp").and_then(JsonValue::as_u64);
    let mut cache = runtime
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cache.len() >= runtime.config.jwt_cache_full_size {
        if let Some(first_key) = cache.keys().next().cloned() {
            cache.remove(&first_key);
        }
    }
    cache.insert(
        token,
        CachedPrincipal {
            principal: principal.clone(),
            expires_at,
        },
    );
}

fn request_path_is_skipped(config: &SecurityConfig, request_path: &str) -> bool {
    config
        .skip_path_prefixes
        .iter()
        .any(|prefix| request_path.starts_with(prefix.as_str()))
}

fn is_h2c_upgrade(session: &Session) -> bool {
    let Some(upgrade) = request_header(session, "upgrade") else {
        return false;
    };
    if !upgrade.eq_ignore_ascii_case("h2c") {
        return false;
    }
    request_header(session, "connection")
        .map(|value| value.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false)
}

fn mock_principal() -> AuthPrincipal {
    AuthPrincipal {
        client_id: Some("mock-client".to_string()),
        user_id: Some("mock-user".to_string()),
        issuer: Some("mock".to_string()),
        claims: serde_json::json!({
            "client_id": "mock-client",
            "user_id": "mock-user",
            "iss": "mock"
        }),
        ..AuthPrincipal::default()
    }
}

fn resolve_config_file(runtime_config: &RuntimeConfig, file_name: &str) -> PathBuf {
    let path = Path::new(file_name);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let external = runtime_config.external_config_dir.join(path);
    if external.exists() {
        return external;
    }
    runtime_config.config_dir.join(path)
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn default_true() -> bool {
    true
}

fn default_swt_client_id_header() -> String {
    "swt-client-id".to_string()
}

fn default_swt_client_secret_header() -> String {
    "swt-client-secret".to_string()
}

fn default_jwt_cache_full_size() -> usize {
    1000
}

fn default_clock_skew_seconds() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_config_accepts_java_style_maps_and_lists() {
        let config: SecurityConfig = serde_yaml::from_str(
            r#"
jwt:
  certificate: '100=primary.crt&101=secondary.crt'
skipPathPrefixes: /health,/info
passThroughClaims: 'client_id=x-client-id,sub=x-user-id'
"#,
        )
        .expect("parse security config");

        assert_eq!(config.jwt.certificate["100"], "primary.crt");
        assert_eq!(config.skip_path_prefixes, ["/health", "/info"]);
        assert_eq!(config.pass_through_claims["client_id"], "x-client-id");
    }

    #[test]
    fn principal_uses_light_claim_fallbacks() {
        let principal = principal_from_claims(serde_json::json!({
            "cid": "client",
            "uid": "user",
            "eml": "user@example.com"
        }));

        assert_eq!(principal.client_id.as_deref(), Some("client"));
        assert_eq!(principal.user_id.as_deref(), Some("user"));
        assert_eq!(principal.email.as_deref(), Some("user@example.com"));
    }
}
