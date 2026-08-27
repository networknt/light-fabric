use crate::apikey::{ApiKeyConfig, verify_required_api_key};
use crate::basic_auth::{BasicAuthConfig, verify_basic_auth};
use crate::config_util::{deserialize_string_list, deserialize_typed_list, request_header};
use crate::hmac::HmacRuntime;
use crate::security::{
    AuthPrincipal, HandlerRejection, SecurityRuntime, verify_jwt_request_with_service_ids,
};
use base64::Engine as _;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub methods: Vec<String>,
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
    /// JWK service IDs (from `client.yml` `serviceIdAuthServers`) used to
    /// resolve the JWK endpoint for JWT verification on this path prefix.
    /// The first non-empty entry is used.  Falls back to the default key
    /// server when empty.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub jwk_service_ids: Vec<String>,
    /// JWK service IDs used for SJWT (Simple-JWT) verification on this prefix.
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub sjwk_service_ids: Vec<String>,
    /// Introspection service IDs used for SWT verification on this prefix.
    /// (SWT introspection is not yet implemented.)
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub swt_service_ids: Vec<String>,
    #[serde(default)]
    pub authentication: Option<UnifiedAuthentication>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedAuthentication {
    #[serde(default)]
    pub all_of: Vec<UnifiedAuthFactor>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedAuthFactor {
    #[serde(rename = "type")]
    pub factor_type: UnifiedAuthFactorType,
    #[serde(default)]
    pub profile: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub jwk_service_ids: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UnifiedAuthFactorType {
    Hmac,
    Jwt,
    ApiKey,
}

#[derive(Debug, Clone, Default)]
pub struct UnifiedSecurityOutcome {
    pub principal: Option<AuthPrincipal>,
    pub hmac_profile: Option<String>,
}

impl UnifiedSecurityConfig {
    pub fn requires_hmac(&self) -> bool {
        self.path_prefix_auths
            .iter()
            .any(UnifiedPathAuth::requires_hmac)
    }

    pub fn hmac_profile_for(&self, path: &str, method: &str) -> Option<&str> {
        best_auth_rule(self, path, method).and_then(|rule| {
            rule.authentication
                .as_ref()?
                .all_of
                .iter()
                .find(|factor| factor.factor_type == UnifiedAuthFactorType::Hmac)
                .map(|factor| factor.profile.as_str())
        })
    }
}

impl UnifiedPathAuth {
    fn requires_hmac(&self) -> bool {
        self.authentication.as_ref().is_some_and(|authentication| {
            authentication
                .all_of
                .iter()
                .any(|factor| factor.factor_type == UnifiedAuthFactorType::Hmac)
        })
    }
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

    validate_unified_security_config(&config, None)?;

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
    request_method: &str,
) -> Result<UnifiedSecurityOutcome, HandlerRejection> {
    if !config.enabled || is_anonymous(config, request_path) {
        return Ok(UnifiedSecurityOutcome::default());
    }
    let Some(rule) = best_auth_rule(config, request_path, request_method) else {
        return Err(HandlerRejection::new(
            403,
            "ERR10078",
            "request path is not configured for unified security",
        ));
    };

    if let Some(authentication) = rule.authentication.as_ref() {
        return verify_composed_security(
            session,
            authentication,
            api_key_config,
            security_runtime,
            request_path,
        )
        .await;
    }

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
            return Ok(UnifiedSecurityOutcome::default());
        }

        if scheme.eq_ignore_ascii_case("bearer") {
            // Extract the raw token value for scope-detection (SJWT routing).
            // We read it here for the SJWT case; the full verifier re-reads it
            // from the session header via its own bearer_token() call.
            let raw_token = authorization.get(7..).unwrap_or("").trim().to_string();

            let principal =
                dispatch_bearer_token(session, rule, security_runtime, request_path, &raw_token)
                    .await?;
            return Ok(UnifiedSecurityOutcome {
                principal,
                hmac_profile: None,
            });
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
    Ok(UnifiedSecurityOutcome::default())
}

async fn verify_composed_security(
    session: &mut Session,
    authentication: &UnifiedAuthentication,
    api_key_config: Option<&ApiKeyConfig>,
    security_runtime: Option<&SecurityRuntime>,
    request_path: &str,
) -> Result<UnifiedSecurityOutcome, HandlerRejection> {
    let mut outcome = UnifiedSecurityOutcome::default();
    for factor in &authentication.all_of {
        match factor.factor_type {
            UnifiedAuthFactorType::Jwt => {
                let runtime = security_runtime.ok_or_else(|| {
                    HandlerRejection::new(500, "ERR10001", "security.yml is not active")
                })?;
                let principal = verify_jwt_request_with_service_ids(
                    session,
                    runtime,
                    request_path,
                    &factor.jwk_service_ids,
                )
                .await?
                .ok_or_else(|| {
                    HandlerRejection::unauthorized(
                        "JWT authentication is required by the composed policy",
                    )
                })?;
                outcome.principal = Some(principal);
            }
            UnifiedAuthFactorType::ApiKey => {
                let config = api_key_config.ok_or_else(|| {
                    HandlerRejection::new(500, "ERR10001", "apikey.yml is not active")
                })?;
                verify_required_api_key(session, config, request_path)?;
            }
            UnifiedAuthFactorType::Hmac => {
                outcome.hmac_profile = Some(factor.profile.clone());
            }
        }
    }
    Ok(outcome)
}

pub fn validate_unified_security_config(
    config: &UnifiedSecurityConfig,
    hmac: Option<&HmacRuntime>,
) -> Result<(), RuntimeError> {
    let standalone_routes = hmac.map(HmacRuntime::standalone_routes).unwrap_or_default();
    if !config.requires_hmac() && standalone_routes.is_empty() {
        return Ok(());
    }
    for rule in &config.path_prefix_auths {
        normalize_methods(&rule.methods)?;
        let Some(authentication) = rule.authentication.as_ref() else {
            continue;
        };
        if rule_uses_legacy_fields(rule) {
            return config_error(format!(
                "unified security rule `{}` mixes legacy authentication fields with authentication.allOf",
                rule.prefix
            ));
        }
        if rule.prefix.is_empty() {
            return config_error("composed unified security rule prefix must not be empty");
        }
        if authentication.all_of.is_empty() {
            return config_error(format!(
                "unified security rule `{}` authentication.allOf must not be empty",
                rule.prefix
            ));
        }
        let hmac_factors = authentication
            .all_of
            .iter()
            .filter(|factor| factor.factor_type == UnifiedAuthFactorType::Hmac)
            .count();
        let header_factors = authentication.all_of.len().saturating_sub(hmac_factors);
        if hmac_factors != 1 || header_factors > 1 {
            return config_error(format!(
                "unified security rule `{}` requires exactly one HMAC factor and at most one JWT or API-key factor",
                rule.prefix
            ));
        }
        for factor in &authentication.all_of {
            match factor.factor_type {
                UnifiedAuthFactorType::Hmac => {
                    if factor.profile.is_empty() {
                        return config_error(format!(
                            "unified security rule `{}` has an HMAC factor without a profile",
                            rule.prefix
                        ));
                    }
                    if let Some(runtime) = hmac
                        && !runtime.contains_profile(factor.profile.as_str())
                    {
                        return config_error(format!(
                            "unified security rule `{}` references unknown HMAC profile `{}`",
                            rule.prefix, factor.profile
                        ));
                    }
                }
                UnifiedAuthFactorType::Jwt => {
                    if !factor.profile.is_empty() {
                        return config_error(format!(
                            "unified security JWT factor on `{}` must not declare profile",
                            rule.prefix
                        ));
                    }
                }
                UnifiedAuthFactorType::ApiKey => {
                    if !factor.profile.is_empty() || !factor.jwk_service_ids.is_empty() {
                        return config_error(format!(
                            "unified security API-key factor on `{}` contains unsupported fields",
                            rule.prefix
                        ));
                    }
                }
            }
        }
        if let Some(runtime) = hmac
            && runtime.standalone_policy_overlaps(&rule.prefix, &rule.methods)?
        {
            return config_error(format!(
                "standalone and unified-security HMAC policies overlap at prefix `{}`",
                rule.prefix
            ));
        }
        if config.anonymous_prefixes.iter().any(|anonymous| {
            anonymous.starts_with(rule.prefix.as_str()) || rule.prefix.starts_with(anonymous)
        }) {
            return config_error(format!(
                "HMAC-protected unified security rule `{}` overlaps anonymousPrefixes",
                rule.prefix
            ));
        }
    }

    for (index, left) in config.path_prefix_auths.iter().enumerate() {
        for right in config.path_prefix_auths.iter().skip(index + 1) {
            if left.prefix == right.prefix && methods_overlap(&left.methods, &right.methods)? {
                return config_error(format!(
                    "duplicate unified security rules for prefix `{}` have overlapping methods",
                    left.prefix
                ));
            }
            validate_hmac_fallthrough(left, right)?;
        }
    }
    for standalone in &standalone_routes {
        for unified in &config.path_prefix_auths {
            if !unified.requires_hmac() {
                if standalone.prefix == unified.prefix
                    && methods_overlap(&standalone.methods, &unified.methods)?
                {
                    return config_error(format!(
                        "standalone HMAC and non-HMAC unified-security policies overlap at prefix `{}`",
                        standalone.prefix
                    ));
                }
                validate_hmac_fallthrough_between(
                    standalone.prefix.as_str(),
                    &standalone.methods,
                    unified.prefix.as_str(),
                    &unified.methods,
                )?;
            }
        }
    }
    Ok(())
}

fn validate_hmac_fallthrough(
    left: &UnifiedPathAuth,
    right: &UnifiedPathAuth,
) -> Result<(), RuntimeError> {
    if left.requires_hmac() == right.requires_hmac()
        || left.prefix == right.prefix
        || !(left.prefix.starts_with(&right.prefix) || right.prefix.starts_with(&left.prefix))
    {
        return Ok(());
    }
    let (hmac, non_hmac) = if left.requires_hmac() {
        (left, right)
    } else {
        (right, left)
    };
    validate_hmac_fallthrough_between(
        hmac.prefix.as_str(),
        &hmac.methods,
        non_hmac.prefix.as_str(),
        &non_hmac.methods,
    )
}

fn validate_hmac_fallthrough_between(
    hmac_prefix: &str,
    hmac_methods: &[String],
    non_hmac_prefix: &str,
    non_hmac_methods: &[String],
) -> Result<(), RuntimeError> {
    if hmac_prefix == non_hmac_prefix
        || !(hmac_prefix.starts_with(non_hmac_prefix) || non_hmac_prefix.starts_with(hmac_prefix))
    {
        return Ok(());
    }
    if non_hmac_prefix.len() > hmac_prefix.len() {
        if methods_overlap(hmac_methods, non_hmac_methods)? {
            return config_error(format!(
                "more-specific non-HMAC rule `{non_hmac_prefix}` shadows HMAC rule `{hmac_prefix}`"
            ));
        }
    } else if !methods_cover(hmac_methods, non_hmac_methods)? {
        return config_error(format!(
            "non-HMAC ancestor `{non_hmac_prefix}` permits methods that fall through around HMAC rule `{hmac_prefix}`"
        ));
    }
    Ok(())
}

fn rule_uses_legacy_fields(rule: &UnifiedPathAuth) -> bool {
    rule.basic
        || rule.jwt
        || rule.sjwt
        || rule.swt
        || rule.apikey
        || !rule.jwk_service_ids.is_empty()
        || !rule.sjwk_service_ids.is_empty()
        || !rule.swt_service_ids.is_empty()
}

/// Dispatch a Bearer token to the correct verifier based on the matched rule.
///
/// Resolution order mirrors the Java `UnifiedSecurityHandler`:
///
/// 1. If `jwt=true` AND `sjwt=true`: inspect the token payload.
///    - Scope present → JWT verifier with `jwkServiceIds`.
///    - No scope      → SJWT verifier with `sjwkServiceIds`.
/// 2. If only `jwt=true`: JWT verifier with `jwkServiceIds`.
/// 3. If only `sjwt=true`: SJWT verifier with `sjwkServiceIds` (no scope check needed).
/// 4. If `swt=true`: SWT introspection — not yet implemented.
async fn dispatch_bearer_token(
    session: &mut Session,
    rule: &UnifiedPathAuth,
    security_runtime: Option<&SecurityRuntime>,
    request_path: &str,
    raw_token: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    // Helper closure: resolve the required SecurityRuntime or fail fast.
    let require_runtime = || {
        security_runtime
            .ok_or_else(|| HandlerRejection::new(500, "ERR10001", "security.yml is not active"))
    };

    if !token_is_jwt_like(raw_token) {
        if rule.swt {
            return swt_not_implemented(request_path);
        }
        return Err(HandlerRejection::unauthorized(
            "invalid or unsupported bearer token",
        ));
    }

    if rule.jwt && rule.sjwt {
        // Both JWT and SJWT enabled: inspect the token payload to decide.
        let runtime = require_runtime()?;
        if token_has_scope(raw_token) {
            // Full JWT — has scope/scp claim.
            tracing::trace!(
                "unified-security: jwt+sjwt rule, scope present, using jwt verifier \
                 (service_ids={:?}) for path {}",
                rule.jwk_service_ids,
                request_path
            );
            return verify_jwt_request_with_service_ids(
                session,
                runtime,
                request_path,
                &rule.jwk_service_ids,
            )
            .await;
        } else {
            // Simple JWT — no scope claim.
            tracing::trace!(
                "unified-security: jwt+sjwt rule, no scope, using sjwt verifier \
                 (service_ids={:?}) for path {}",
                rule.sjwk_service_ids,
                request_path
            );
            return verify_jwt_request_with_service_ids(
                session,
                runtime,
                request_path,
                &rule.sjwk_service_ids,
            )
            .await;
        }
    }

    if rule.jwt {
        let runtime = require_runtime()?;
        tracing::trace!(
            "unified-security: jwt rule, using jwt verifier (service_ids={:?}) for path {}",
            rule.jwk_service_ids,
            request_path
        );
        return verify_jwt_request_with_service_ids(
            session,
            runtime,
            request_path,
            &rule.jwk_service_ids,
        )
        .await;
    }

    if rule.sjwt {
        // SJWT-only: no scope check needed — all bearer tokens are treated as SJWT.
        let runtime = require_runtime()?;
        tracing::trace!(
            "unified-security: sjwt-only rule, using sjwt verifier (service_ids={:?}) for path {}",
            rule.sjwk_service_ids,
            request_path
        );
        return verify_jwt_request_with_service_ids(
            session,
            runtime,
            request_path,
            &rule.sjwk_service_ids,
        )
        .await;
    }

    if rule.swt {
        return swt_not_implemented(request_path);
    }

    Err(HandlerRejection::unauthorized(
        "no bearer token verifier is enabled for this path",
    ))
}

fn swt_not_implemented(request_path: &str) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    // SWT opaque-token introspection is not yet implemented.
    // It requires an introspection endpoint call keyed by swtServiceIds.
    tracing::warn!(
        "unified-security: swt rule matched for path {} but SWT token introspection \
         is not yet implemented; use jwt or basic authentication for this path",
        request_path
    );
    Err(HandlerRejection::new(
        501,
        "ERR10001",
        "SWT token introspection is not yet implemented; \
         configure jwt or basic authentication for this path prefix",
    ))
}

fn token_is_jwt_like(token: &str) -> bool {
    jwt_payload_claims(token).is_some()
}

/// Returns `true` if the JWT payload (decoded without signature verification)
/// contains a non-empty `scope` or `scp` claim.
///
/// Used by the unified-security handler to distinguish full JWTs (with scopes)
/// from Simple JWTs (without scopes) when both `jwt=true` and `sjwt=true` are
/// configured for a path prefix.  The signature is intentionally NOT verified
/// here — this is purely a routing decision.
fn token_has_scope(token: &str) -> bool {
    let Some(claims) = jwt_payload_claims(token) else {
        return false;
    };
    for key in &["scope", "scp"] {
        match claims.get(key) {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => return true,
            Some(serde_json::Value::Array(arr)) if !arr.is_empty() => return true,
            _ => {}
        }
    }
    false
}

fn jwt_payload_claims(token: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    if parts.len() != 3 {
        return None;
    }
    decode_jwt_part(parts[0])
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())?;
    let payload_bytes = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(bytes) => bytes,
        Err(_) => {
            // Try standard padding variant as fallback.
            match base64::engine::general_purpose::URL_SAFE.decode(parts[1]) {
                Ok(bytes) => bytes,
                Err(_) => return None,
            }
        }
    };
    serde_json::from_slice(&payload_bytes).ok()
}

fn decode_jwt_part(part: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(part))
        .ok()
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
    request_method: &str,
) -> Option<&'a UnifiedPathAuth> {
    let request_method = request_method.to_ascii_uppercase();
    config
        .path_prefix_auths
        .iter()
        .filter(|rule| {
            request_path.starts_with(rule.prefix.as_str())
                && (rule.methods.is_empty()
                    || rule
                        .methods
                        .iter()
                        .any(|method| method.eq_ignore_ascii_case(&request_method)))
        })
        .max_by_key(|rule| rule.prefix.len())
}

fn normalize_methods(methods: &[String]) -> Result<BTreeSet<String>, RuntimeError> {
    let mut normalized = BTreeSet::new();
    for method in methods {
        let method = method.trim().to_ascii_uppercase();
        if method.is_empty() || http::Method::from_bytes(method.as_bytes()).is_err() {
            return config_error(format!(
                "invalid HTTP method `{method}` in unified security policy"
            ));
        }
        if !normalized.insert(method.clone()) {
            return config_error(format!(
                "duplicate HTTP method `{method}` in unified security policy"
            ));
        }
    }
    Ok(normalized)
}

fn methods_overlap(left: &[String], right: &[String]) -> Result<bool, RuntimeError> {
    let left = normalize_methods(left)?;
    let right = normalize_methods(right)?;
    Ok(left.is_empty() || right.is_empty() || left.iter().any(|method| right.contains(method)))
}

fn methods_cover(hmac: &[String], non_hmac: &[String]) -> Result<bool, RuntimeError> {
    let hmac = normalize_methods(hmac)?;
    let non_hmac = normalize_methods(non_hmac)?;
    if hmac.is_empty() {
        return Ok(true);
    }
    if non_hmac.is_empty() {
        return Ok(false);
    }
    Ok(non_hmac.iter().all(|method| hmac.contains(method)))
}

fn config_error<T>(message: impl Into<String>) -> Result<T, RuntimeError> {
    Err(RuntimeError::Config(message.into()))
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hmac::{HmacConfig, HmacSecretResolver};

    struct TestSecretResolver;

    impl HmacSecretResolver for TestSecretResolver {
        fn resolve(&self, _environment_name: &str) -> Result<Vec<u8>, RuntimeError> {
            Ok(b"test-secret".to_vec())
        }
    }

    fn hmac_runtime(standalone: bool) -> HmacRuntime {
        let standalone_rule = if standalone {
            r#"
pathPrefixAuths:
  - prefix: /standalone
    methods: [POST]
    profile: github
"#
        } else {
            ""
        };
        let config: HmacConfig = serde_yaml::from_str(
            format!(
                r#"
{standalone_rule}
profiles:
  github:
    secrets:
      defaultEnvNames: [TEST_SECRET]
"#
            )
            .as_str(),
        )
        .expect("parse HMAC config");
        HmacRuntime::compile(&config, &TestSecretResolver).expect("compile HMAC runtime")
    }

    // ---------------------------------------------------------------------------
    // Config parsing
    // ---------------------------------------------------------------------------

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
        assert!(best_auth_rule(&config, "/api/pets", "GET").unwrap().jwt);
        assert!(
            best_auth_rule(&config, "/admin/users", "GET")
                .unwrap()
                .basic
        );
    }

    #[test]
    fn path_prefix_auth_parses_jwk_service_ids() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /salesforce
    jwt: true
    jwkServiceIds:
      - com.networknt.oauth2-salesforce-1.0.0
  - prefix: /internal
    jwt: true
    sjwt: true
    jwkServiceIds:
      - com.networknt.oauth2-internal-1.0.0
    sjwkServiceIds:
      - com.networknt.oauth2-simple-1.0.0
"#,
        )
        .expect("parse config with jwkServiceIds");

        let sf_rule = best_auth_rule(&config, "/salesforce/data", "GET").unwrap();
        assert!(sf_rule.jwt);
        assert_eq!(
            sf_rule.jwk_service_ids,
            ["com.networknt.oauth2-salesforce-1.0.0"]
        );

        let int_rule = best_auth_rule(&config, "/internal/api", "GET").unwrap();
        assert!(int_rule.jwt && int_rule.sjwt);
        assert_eq!(
            int_rule.jwk_service_ids,
            ["com.networknt.oauth2-internal-1.0.0"]
        );
        assert_eq!(
            int_rule.sjwk_service_ids,
            ["com.networknt.oauth2-simple-1.0.0"]
        );
    }

    #[test]
    fn path_prefix_auth_parses_jwk_service_ids_comma_format() {
        // Java-style comma-separated string in JSON array format
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths: '[{"prefix":"/sf","jwt":true,"jwkServiceIds":"svc1,svc2"}]'
"#,
        )
        .expect("parse JSON-string config");

        let rule = best_auth_rule(&config, "/sf/foo", "GET").unwrap();
        assert_eq!(rule.jwk_service_ids, ["svc1", "svc2"]);
    }

    // ---------------------------------------------------------------------------
    // Prefix matching
    // ---------------------------------------------------------------------------

    #[test]
    fn best_auth_rule_selects_longest_prefix() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /api
    jwt: true
  - prefix: /api/v2
    basic: true
"#,
        )
        .expect("parse");

        // /api/v2/pets should match /api/v2, not /api
        let rule = best_auth_rule(&config, "/api/v2/pets", "GET").unwrap();
        assert!(rule.basic, "longest prefix /api/v2 should win");

        // /api/v1/pets should fall through to /api
        let rule = best_auth_rule(&config, "/api/v1/pets", "GET").unwrap();
        assert!(rule.jwt, "/api should match /api/v1/pets");
    }

    #[test]
    fn best_auth_rule_returns_none_for_unmatched_path() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /api
    jwt: true
"#,
        )
        .expect("parse");

        assert!(best_auth_rule(&config, "/other/path", "GET").is_none());
    }

    #[test]
    fn method_aware_all_of_selects_hmac_profile() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
  - prefix: /partner
    methods: [GET]
    jwt: true
"#,
        )
        .expect("parse composed config");
        let runtime = hmac_runtime(false);
        validate_unified_security_config(&config, Some(&runtime)).expect("validate allOf");
        assert!(
            best_auth_rule(&config, "/partner/event", "POST")
                .unwrap()
                .requires_hmac()
        );
        assert_eq!(
            config.hmac_profile_for("/partner/event", "POST"),
            Some("github")
        );
        assert_eq!(config.hmac_profile_for("/partner/event", "GET"), None);
        assert!(
            best_auth_rule(&config, "/partner/event", "GET")
                .unwrap()
                .jwt
        );
    }

    #[test]
    fn rejects_unknown_hmac_profile_and_standalone_overlap() {
        let unknown: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: missing
"#,
        )
        .expect("parse unknown profile");
        assert!(validate_unified_security_config(&unknown, Some(&hmac_runtime(false))).is_err());

        let overlap: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /standalone/specific
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse overlapping policy");
        assert!(validate_unified_security_config(&overlap, Some(&hmac_runtime(true))).is_err());
    }

    #[test]
    fn rejects_both_method_fallthrough_directions() {
        let ancestor_fallthrough: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    jwt: true
  - prefix: /webhook/github
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse ancestor fallthrough");
        assert!(
            validate_unified_security_config(&ancestor_fallthrough, Some(&hmac_runtime(false)))
                .is_err()
        );

        let specific_shadow: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    authentication:
      allOf:
        - type: hmac
          profile: github
  - prefix: /webhook/github
    methods: [POST]
    apikey: true
"#,
        )
        .expect("parse specific shadow");
        assert!(
            validate_unified_security_config(&specific_shadow, Some(&hmac_runtime(false))).is_err()
        );
    }

    #[test]
    fn rejects_fallthrough_between_standalone_hmac_and_legacy_unified_rules() {
        let hmac_config: HmacConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github
    methods: [POST]
    profile: github
profiles:
  github:
    secrets:
      defaultEnvNames: [TEST_SECRET]
"#,
        )
        .expect("parse standalone HMAC config");
        let hmac = HmacRuntime::compile(&hmac_config, &TestSecretResolver)
            .expect("compile standalone HMAC runtime");

        let ancestor: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    jwt: true
"#,
        )
        .expect("parse legacy ancestor");
        assert!(
            validate_unified_security_config(&ancestor, Some(&hmac)).is_err(),
            "an all-method legacy ancestor must not capture methods omitted by standalone HMAC"
        );

        let covered_ancestor: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    jwt: true
"#,
        )
        .expect("parse method-covered legacy ancestor");
        validate_unified_security_config(&covered_ancestor, Some(&hmac))
            .expect("a legacy ancestor restricted to HMAC-covered methods is valid");

        let specific: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github/private
    methods: [POST]
    apikey: true
"#,
        )
        .expect("parse legacy specific rule");
        assert!(
            validate_unified_security_config(&specific, Some(&hmac)).is_err(),
            "a more-specific legacy rule must not shadow standalone HMAC"
        );

        let same_prefix: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /webhook/github
    methods: [POST]
    jwt: true
"#,
        )
        .expect("parse same-prefix legacy rule");
        assert!(
            validate_unified_security_config(&same_prefix, Some(&hmac)).is_err(),
            "equal-prefix policies from different entry points must not overlap"
        );
    }

    #[test]
    fn rejects_mixed_legacy_and_all_of_or_anonymous_overlap() {
        let mixed: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
pathPrefixAuths:
  - prefix: /partner
    jwt: true
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse mixed rule");
        assert!(validate_unified_security_config(&mixed, Some(&hmac_runtime(false))).is_err());

        let anonymous: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes: [/partner/health]
pathPrefixAuths:
  - prefix: /partner
    authentication:
      allOf:
        - type: hmac
          profile: github
"#,
        )
        .expect("parse anonymous overlap");
        assert!(validate_unified_security_config(&anonymous, Some(&hmac_runtime(false))).is_err());
    }

    #[test]
    fn anonymous_prefix_matches_correctly() {
        let config: UnifiedSecurityConfig = serde_yaml::from_str(
            r#"
anonymousPrefixes:
  - /health
  - /server/info
pathPrefixAuths:
  - prefix: /
    jwt: true
"#,
        )
        .expect("parse");

        assert!(is_anonymous(&config, "/health"));
        assert!(is_anonymous(&config, "/health/live"));
        assert!(is_anonymous(&config, "/server/info"));
        assert!(!is_anonymous(&config, "/api/data"));
    }

    // ---------------------------------------------------------------------------
    // token_has_scope helper
    // ---------------------------------------------------------------------------

    /// Build a minimal unsigned JWT with the given payload for testing purposes.
    fn make_jwt_payload(payload_json: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload_json.as_bytes());
        format!("{header}.{payload}.fakesig")
    }

    #[test]
    fn token_has_scope_detects_string_scope() {
        let token = make_jwt_payload(r#"{"sub":"user","scope":"read:api write:api"}"#);
        assert!(token_has_scope(&token));
    }

    #[test]
    fn token_is_jwt_like_requires_decodable_jwt() {
        let token = make_jwt_payload(r#"{"sub":"user"}"#);

        assert!(token_is_jwt_like(&token));
        assert!(!token_is_jwt_like("opaque-token"));
        assert!(!token_is_jwt_like("not.a.jwt"));
    }

    #[test]
    fn token_has_scope_detects_array_scp() {
        let token = make_jwt_payload(r#"{"sub":"user","scp":["read","write"]}"#);
        assert!(token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_empty_scope() {
        let token = make_jwt_payload(r#"{"sub":"user","scope":""}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_when_no_scope_claim() {
        let token = make_jwt_payload(r#"{"sub":"user","client_id":"app"}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_empty_array() {
        let token = make_jwt_payload(r#"{"sub":"user","scp":[]}"#);
        assert!(!token_has_scope(&token));
    }

    #[test]
    fn token_has_scope_returns_false_for_invalid_token() {
        assert!(!token_has_scope("not.a.jwt"));
        assert!(!token_has_scope(""));
        assert!(!token_has_scope("onlyonepart"));
    }
}
