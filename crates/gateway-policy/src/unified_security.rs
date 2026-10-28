use crate::Request;
use crate::apikey::{ApiKeyConfig, verify_required_api_key};
use crate::basic_auth::{BasicAuthConfig, verify_basic_auth};
use crate::config_util::{deserialize_string_list, deserialize_typed_list, request_header};
use crate::security::{
    AuthPrincipal, HandlerRejection, SecurityRuntime, verify_jwt_request_with_service_ids,
};
use base64::Engine as _;
use light_runtime::RuntimeError;
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
    #[doc(hidden)]
    pub fn requires_hmac(&self) -> bool {
        self.authentication.as_ref().is_some_and(|authentication| {
            authentication
                .all_of
                .iter()
                .any(|factor| factor.factor_type == UnifiedAuthFactorType::Hmac)
        })
    }
}

pub async fn verify_unified_security(
    session: &mut impl Request,
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
    session: &mut impl Request,
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
    hmac: Option<&dyn HmacValidation>,
) -> Result<(), RuntimeError> {
    let standalone_routes = hmac
        .map(|runtime| runtime.standalone_routes())
        .unwrap_or_default();
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
    session: &mut impl Request,
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
            tracing::trace!(target: "light_pingora::unified_security",
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
            tracing::trace!(target: "light_pingora::unified_security",
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
        tracing::trace!(target: "light_pingora::unified_security",
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
        tracing::trace!(target: "light_pingora::unified_security",
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
    tracing::warn!(target: "light_pingora::unified_security",
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

#[doc(hidden)]
pub fn token_is_jwt_like(token: &str) -> bool {
    jwt_payload_claims(token).is_some()
}

/// Returns `true` if the JWT payload (decoded without signature verification)
/// contains a non-empty `scope` or `scp` claim.
///
/// Used by the unified-security handler to distinguish full JWTs (with scopes)
/// from Simple JWTs (without scopes) when both `jwt=true` and `sjwt=true` are
/// configured for a path prefix.  The signature is intentionally NOT verified
/// here — this is purely a routing decision.
#[doc(hidden)]
pub fn token_has_scope(token: &str) -> bool {
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

#[doc(hidden)]
pub fn is_anonymous(config: &UnifiedSecurityConfig, request_path: &str) -> bool {
    config
        .anonymous_prefixes
        .iter()
        .any(|prefix| request_path.starts_with(prefix.as_str()))
}

#[doc(hidden)]
pub fn best_auth_rule<'a>(
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

/// Immutable validation metadata supplied by a framework adapter. No HMAC
/// request/body verification or secret material crosses this interface.
pub struct HmacRoute {
    pub prefix: String,
    pub methods: Vec<String>,
    pub profile: String,
}
pub trait HmacValidation {
    fn standalone_routes(&self) -> Vec<HmacRoute>;
    fn contains_profile(&self, profile: &str) -> bool;
    fn standalone_policy_overlaps(
        &self,
        prefix: &str,
        methods: &[String],
    ) -> Result<bool, RuntimeError>;
}
