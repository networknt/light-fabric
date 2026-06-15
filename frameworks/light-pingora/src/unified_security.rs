use crate::apikey::{ApiKeyConfig, verify_required_api_key};
use crate::basic_auth::{BasicAuthConfig, verify_basic_auth};
use crate::config_util::{deserialize_string_list, deserialize_typed_list, request_header};
use crate::security::{
    AuthPrincipal, HandlerRejection, SecurityRuntime, verify_jwt_request_with_service_ids,
};
use base64::Engine as _;
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
        return Err(HandlerRejection::new(
            403,
            "ERR10078",
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
            // Extract the raw token value for scope-detection (SJWT routing).
            // We read it here for the SJWT case; the full verifier re-reads it
            // from the session header via its own bearer_token() call.
            let raw_token = authorization.get(7..).unwrap_or("").trim().to_string();

            return dispatch_bearer_token(
                session,
                rule,
                security_runtime,
                request_path,
                &raw_token,
            )
            .await;
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
        assert!(best_auth_rule(&config, "/api/pets").unwrap().jwt);
        assert!(best_auth_rule(&config, "/admin/users").unwrap().basic);
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

        let sf_rule = best_auth_rule(&config, "/salesforce/data").unwrap();
        assert!(sf_rule.jwt);
        assert_eq!(
            sf_rule.jwk_service_ids,
            ["com.networknt.oauth2-salesforce-1.0.0"]
        );

        let int_rule = best_auth_rule(&config, "/internal/api").unwrap();
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

        let rule = best_auth_rule(&config, "/sf/foo").unwrap();
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
        let rule = best_auth_rule(&config, "/api/v2/pets").unwrap();
        assert!(rule.basic, "longest prefix /api/v2 should win");

        // /api/v1/pets should fall through to /api
        let rule = best_auth_rule(&config, "/api/v1/pets").unwrap();
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

        assert!(best_auth_rule(&config, "/other/path").is_none());
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
