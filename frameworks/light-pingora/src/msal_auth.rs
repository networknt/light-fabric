use crate::HandlerRejection;
use crate::security::SecurityConfig;
use crate::spa_auth::{
    AUTHORIZATION_HEADER, CookieSameSite, SpaAuthCsrfEndpoint, SpaAuthLegacyEndpoint,
    SpaAuthResponse, SpaCookieConfig, SpaSessionOutcome, bearer_token, delete_cookie_header,
    generate_csrf, record_spa_auth_legacy_get, validate_logout_csrf,
};
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

#[path = "msal_verifier.rs"]
mod verifier;
use verifier::MsalVerifier;

pub const MSAL_AUTH_FILE: &str = "msal-auth.yml";
pub const MSAL_AUTH_MODULE_ID: &str = "light-pingora/msal-auth";
pub const MSAL_AUTH_CONFIG_NAME: &str = "msal-auth";
pub const SECURITY_MSAL_FILE: &str = "security-msal.yml";
pub const SECURITY_MSAL_MODULE_ID: &str = "light-pingora/security-msal";
pub const SECURITY_MSAL_CONFIG_NAME: &str = "security-msal";

const ACCESS_TOKEN_COOKIE: &str = "accessToken";
const CSRF_COOKIE: &str = "csrf";

fn method_not_allowed(allow: &'static str) -> HandlerRejection {
    HandlerRejection::new(405, "ERR10008", "method not allowed")
        .with_header("allow", allow)
        .with_header("cache-control", "no-store")
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MsalAuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub jwks_uri: String,
    #[serde(default, deserialize_with = "deserialize_policy_list")]
    pub required_scopes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_policy_list")]
    pub allowed_client_ids: Vec<String>,
    #[serde(default = "default_login_path")]
    pub login_path: String,
    #[serde(default = "default_logout_path")]
    pub logout_path: String,
    #[serde(default)]
    pub logout_csrf_enforced: bool,
    #[serde(default = "default_cookie_domain")]
    pub cookie_domain: String,
    #[serde(default = "default_cookie_path")]
    pub cookie_path: String,
    #[serde(default)]
    pub cookie_secure: bool,
    #[serde(default = "default_session_timeout")]
    pub session_timeout: u64,
    #[serde(default)]
    pub cookie_same_site: CookieSameSite,
}

impl Default for MsalAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tenant_id: String::new(),
            issuer: String::new(),
            audience: String::new(),
            jwks_uri: String::new(),
            required_scopes: Vec::new(),
            allowed_client_ids: Vec::new(),
            login_path: default_login_path(),
            logout_path: default_logout_path(),
            logout_csrf_enforced: false,
            cookie_domain: default_cookie_domain(),
            cookie_path: default_cookie_path(),
            cookie_secure: false,
            session_timeout: default_session_timeout(),
            cookie_same_site: CookieSameSite::None,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_login_path() -> String {
    "/auth/ms/login".to_string()
}

fn default_logout_path() -> String {
    "/auth/ms/logout".to_string()
}

fn default_cookie_domain() -> String {
    "localhost".to_string()
}

fn default_cookie_path() -> String {
    "/".to_string()
}

fn default_session_timeout() -> u64 {
    3600
}

impl MsalAuthConfig {
    fn validate(&self) -> Result<(), RuntimeError> {
        if !self.enabled {
            return Ok(());
        }
        let invalid =
            |field| RuntimeError::Unsupported(format!("msal-auth.{field} is missing or invalid"));
        let uuid = |value: &str| {
            uuid::Uuid::parse_str(value)
                .is_ok_and(|parsed| parsed.to_string() == value && !parsed.is_nil())
        };
        if !uuid(&self.tenant_id) {
            return Err(invalid("tenantId"));
        }
        let jwks = url::Url::parse(&self.jwks_uri).map_err(|_| invalid("jwksUri"))?;
        if jwks.scheme() != "https"
            || jwks.host_str().is_none()
            || !jwks.username().is_empty()
            || jwks.password().is_some()
            || jwks.fragment().is_some()
            || self.jwks_uri.trim() != self.jwks_uri
        {
            return Err(invalid("jwksUri"));
        }
        let issuer = url::Url::parse(&self.issuer).map_err(|_| invalid("issuer"))?;
        if issuer.scheme() != "https"
            || issuer.host_str().is_none()
            || !issuer.username().is_empty()
            || issuer.password().is_some()
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || self.issuer.trim() != self.issuer
            || ![
                format!("/{}/", self.tenant_id),
                format!("/{}/v2.0", self.tenant_id),
            ]
            .contains(&issuer.path().to_owned())
        {
            return Err(invalid("issuer"));
        }
        if self.audience.trim().is_empty() || self.audience.trim() != self.audience {
            return Err(invalid("audience"));
        }
        if self.required_scopes.is_empty()
            || self
                .required_scopes
                .iter()
                .any(|scope| scope.is_empty() || scope.chars().any(char::is_whitespace))
        {
            return Err(invalid("requiredScopes"));
        }
        if self.allowed_client_ids.is_empty() || self.allowed_client_ids.iter().any(|id| !uuid(id))
        {
            return Err(invalid("allowedClientIds"));
        }
        Ok(())
    }

    /// Checks application trust and lifetime after signature verification on every request.
    fn validate_claims(
        &self,
        claims: &serde_json::Value,
        now: u64,
        skew: u64,
    ) -> Result<(), &'static str> {
        let text = |name| claims.get(name).and_then(serde_json::Value::as_str);
        if text("iss") != Some(self.issuer.as_str()) {
            return Err("issuer");
        }
        if text("tid") != Some(self.tenant_id.as_str()) {
            return Err("tenant");
        }
        let version = if self.issuer.ends_with("/v2.0") {
            "2.0"
        } else {
            "1.0"
        };
        if text("ver") != Some(version) {
            return Err("version");
        }
        let audience_matches = match claims.get("aud") {
            Some(serde_json::Value::String(value)) => value == &self.audience,
            Some(serde_json::Value::Array(values)) => {
                values.len() == 1 && values[0].as_str() == Some(self.audience.as_str())
            }
            _ => false,
        };
        if !audience_matches {
            return Err("audience");
        }
        let client_claim = if version == "2.0" { "azp" } else { "appid" };
        if !text(client_claim)
            .is_some_and(|client| self.allowed_client_ids.iter().any(|id| id == client))
        {
            return Err("client");
        }
        if !text("sub").is_some_and(|subject| !subject.trim().is_empty())
            || claims
                .get("idtyp")
                .is_some_and(|value| value.as_str() != Some("user"))
        {
            return Err("token_purpose");
        }
        let scopes = text("scp").ok_or("scopes")?;
        if self
            .required_scopes
            .iter()
            .any(|required| !scopes.split(' ').any(|scope| scope == required))
        {
            return Err("scopes");
        }
        let exp = claims
            .get("exp")
            .and_then(serde_json::Value::as_u64)
            .ok_or("expiry")?;
        if now >= exp.saturating_add(skew) {
            return Err("expired");
        }
        if let Some(nbf) = claims.get("nbf") {
            if nbf
                .as_u64()
                .is_none_or(|nbf| nbf > now.saturating_add(skew))
            {
                return Err("not_before");
            }
        }
        Ok(())
    }

    fn cookie_config(&self) -> SpaCookieConfig {
        SpaCookieConfig {
            cookie_domain: self.cookie_domain.clone(),
            cookie_path: self.cookie_path.clone(),
            cookie_secure: self.cookie_secure,
            cookie_same_site: self.cookie_same_site.clone(),
            session_timeout: self.session_timeout,
            remember_me_timeout: self.session_timeout,
            renew_before_seconds: 60,
            refresh_single_flight_wait_ms: 50,
            refresh_single_flight_cache_ms: 5000,
            refresh_single_flight_max_entries: 1000,
            cookie_timeout_uri: String::new(),
        }
    }
}

// Match configuration substitution: native lists, JSON/YAML arrays, CSV and empty defaults.
fn deserialize_policy_list<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Setting {
        List(Vec<String>),
        Text(String),
    }
    match Setting::deserialize(deserializer)? {
        Setting::List(values) => Ok(values),
        Setting::Text(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(Vec::new());
            }
            if value.starts_with('[') {
                serde_yaml::from_str(value).map_err(serde::de::Error::custom)
            } else {
                Ok(value
                    .split(',')
                    .map(|item| item.trim().to_owned())
                    .collect())
            }
        }
    }
}

pub struct MsalAuthRuntime {
    pub config: MsalAuthConfig,
    security: SecurityConfig,
    verifier: MsalVerifier,
}

impl MsalAuthRuntime {
    pub fn new(config: MsalAuthConfig, security: SecurityConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        let verifier = MsalVerifier::new(&config.jwks_uri)?;
        Ok(Self {
            config,
            security,
            verifier,
        })
    }

    pub async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        self.verifier
            .bootstrap()
            .await
            .map_err(msal_verification_error)
    }

    pub async fn handle_request(
        &self,
        session: &mut Session,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        if !self.config.enabled {
            return Ok(SpaSessionOutcome::Continue {
                auth: None,
                response_headers: Vec::new(),
            });
        }

        let path = session.req_header().uri.path();
        if path == self.config.login_path {
            if session
                .req_header()
                .method
                .as_str()
                .eq_ignore_ascii_case("OPTIONS")
            {
                return Ok(SpaSessionOutcome::Continue {
                    auth: None,
                    response_headers: Vec::new(),
                });
            }
            require_post(session.req_header().method.as_str())?;
            return self.handle_login(session).await;
        }

        if path == self.config.logout_path {
            if session
                .req_header()
                .method
                .as_str()
                .eq_ignore_ascii_case("OPTIONS")
            {
                return Ok(SpaSessionOutcome::Continue {
                    auth: None,
                    response_headers: Vec::new(),
                });
            }
            check_logout_method(session.req_header().method.as_str())?;
            return self.handle_logout(session).await;
        }

        let cookies = crate::spa_auth::request_cookies(session);
        if let Some(access_token) = cookies.get(ACCESS_TOKEN_COOKIE) {
            let principal = self
                .validate_session(session, access_token, &cookies)
                .await
                .map_err(|mut error| {
                    // Only invalid authentication clears the session; a CSRF failure must not log users out.
                    if error.status == 401 && error.code == "ERR10000" {
                        let config = self.config.cookie_config();
                        error.headers.push(delete_cookie_header(
                            &config,
                            ACCESS_TOKEN_COOKIE,
                            true,
                        ));
                        error
                            .headers
                            .push(delete_cookie_header(&config, CSRF_COOKIE, false));
                        error
                            .headers
                            .push(("cache-control".into(), "no-store".into()));
                    }
                    error
                })?;
            return Ok(SpaSessionOutcome::Continue {
                auth: Some(principal),
                response_headers: Vec::new(),
            });
        }

        Ok(SpaSessionOutcome::Continue {
            auth: None,
            response_headers: Vec::new(),
        })
    }

    async fn handle_login(
        &self,
        session: &mut Session,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        let microsoft_token = bearer_token(session).ok_or_else(|| {
            HandlerRejection::new(401, "ERR11000", "Microsoft bearer token is missing")
        })?;

        let principal = self.verify_microsoft_token(&microsoft_token).await?;

        let csrf = generate_csrf();

        let expires_in = if let Some(exp) = principal.claims.get("exp").and_then(|v| v.as_u64()) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            exp.saturating_sub(now)
        } else {
            self.config.session_timeout
        };

        let mut headers = vec![];
        crate::spa_auth::push_cookie(
            &mut headers,
            &self.config.cookie_config(),
            ACCESS_TOKEN_COOKIE,
            &microsoft_token,
            expires_in,
            true,
        );
        crate::spa_auth::push_cookie(
            &mut headers,
            &self.config.cookie_config(),
            CSRF_COOKIE,
            &csrf,
            expires_in,
            false,
        );

        let body = json!({
            "message": "success"
        });

        Ok(SpaSessionOutcome::Respond(
            SpaAuthResponse::json(200, body, headers).with_no_store(),
        ))
    }

    async fn handle_logout(
        &self,
        session: &mut Session,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        validate_logout_csrf(
            session,
            SpaAuthCsrfEndpoint::MsalAuthLogout,
            self.config.logout_csrf_enforced,
            &[],
        )?;
        Ok(SpaSessionOutcome::Respond(msal_auth_logout_response(
            &self.config,
        )))
    }

    async fn validate_session(
        &self,
        session: &mut Session,
        access_token: &str,
        cookies: &std::collections::BTreeMap<String, String>,
    ) -> Result<crate::AuthPrincipal, HandlerRejection> {
        let principal = self.verify_microsoft_token(access_token).await?;

        // Double submit cookie CSRF validation
        let cookie_csrf = cookies
            .get(CSRF_COOKIE)
            .ok_or_else(|| HandlerRejection::new(401, "ERR10036", "Missing CSRF cookie"))?;

        let header_csrf = crate::spa_auth::request_csrf(session)
            .ok_or_else(|| HandlerRejection::new(401, "ERR10036", "Missing CSRF header"))?;

        if cookie_csrf != &header_csrf {
            warn!("CSRF validation failed");
            return Err(HandlerRejection::new(
                401,
                "ERR10039",
                "CSRF header does not match cookie",
            ));
        }

        // Inject downstream Authorization header
        session
            .req_header_mut()
            .insert_header(AUTHORIZATION_HEADER, format!("Bearer {access_token}"))
            .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid Authorization header"))?;

        Ok(principal)
    }

    async fn verify_microsoft_token(
        &self,
        token: &str,
    ) -> Result<crate::AuthPrincipal, HandlerRejection> {
        let claims = self
            .verifier
            .verify(token, self.security.jwt.clock_skew_in_seconds)
            .await
            .map_err(msal_verification_error)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.config
            .validate_claims(&claims, now, self.security.jwt.clock_skew_in_seconds)
            .map_err(|reason| {
                warn!(reason, "Microsoft token validation failed");
                HandlerRejection::new(401, "ERR10000", "Invalid Microsoft access token")
            })?;
        let text = |name| {
            claims
                .get(name)
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        Ok(crate::AuthPrincipal {
            client_id: text("client_id").or_else(|| text("cid")),
            user_id: text("user_id")
                .or_else(|| text("uid"))
                .or_else(|| text("sub")),
            issuer: text("iss"),
            email: text("email").or_else(|| text("eml")),
            host: text("host"),
            role: text("role"),
            claims,
        })
    }
}

fn msal_verification_error(error: HandlerRejection) -> HandlerRejection {
    if error.status >= 500 {
        warn!(
            reason = "key_resolution_unavailable",
            "Microsoft token verification unavailable"
        );
        HandlerRejection::new(
            error.status,
            "ERR10056",
            "Microsoft token verification unavailable",
        )
    } else {
        warn!(
            reason = "signature_or_standard_claims",
            "Microsoft token validation failed"
        );
        HandlerRejection::new(401, "ERR10000", "Invalid Microsoft access token")
    }
}

fn msal_auth_logout_response(config: &MsalAuthConfig) -> SpaAuthResponse {
    let cookies = config.cookie_config();
    SpaAuthResponse::no_content(vec![
        delete_cookie_header(&cookies, ACCESS_TOKEN_COOKIE, true),
        delete_cookie_header(&cookies, CSRF_COOKIE, false),
    ])
}

fn require_post(method: &str) -> Result<(), HandlerRejection> {
    if method.eq_ignore_ascii_case("POST") {
        return Ok(());
    }
    Err(method_not_allowed("POST"))
}

fn check_logout_method(method: &str) -> Result<(), HandlerRejection> {
    if method.eq_ignore_ascii_case("POST") {
        return Ok(());
    }
    if method.eq_ignore_ascii_case("GET") {
        record_spa_auth_legacy_get(SpaAuthLegacyEndpoint::MsalAuthLogout);
    }
    Err(method_not_allowed("POST"))
}

pub fn load_msal_auth_config(
    runtime_config: &RuntimeConfig,
) -> Result<Option<MsalAuthConfig>, RuntimeError> {
    runtime_config
        .module_registry
        .load_config(runtime_config, MSAL_AUTH_FILE)
        .map(Some)
        .or_else(|e| {
            if let RuntimeError::MissingConfig(_) = e {
                Ok(None)
            } else {
                Err(e)
            }
        })
}

pub fn load_msal_auth_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<MsalAuthRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = load_msal_auth_config(runtime_config)?.unwrap_or_default();
    config.validate()?;
    runtime_config.module_registry.register_loaded_config(
        MSAL_AUTH_MODULE_ID,
        MSAL_AUTH_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        false,
    )?;

    if !config.enabled {
        return Ok(None);
    }

    // Only load MSAL verifier settings. Never construct the generic verifier or
    // consult its client.yml key providers, caches, TLS settings or routing.
    let security = match runtime_config
        .module_registry
        .load_config::<SecurityConfig>(runtime_config, SECURITY_MSAL_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(_)) => SecurityConfig::default(),
        Err(error) => return Err(error),
    };
    runtime_config.module_registry.register_loaded_config(
        SECURITY_MSAL_MODULE_ID,
        SECURITY_MSAL_CONFIG_NAME,
        ModuleKind::Framework,
        &security,
        [MaskSpec::key("swtClientSecretHeader")],
        true,
        Some(true),
        true,
    )?;

    Ok(Some(MsalAuthRuntime::new(config, security)?))
}

#[cfg(test)]
#[path = "msal_auth_tests.rs"]
mod policy_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_and_logout_method_contracts_are_distinct() {
        assert!(require_post("POST").is_ok());
        let login_rejection = require_post("GET").expect_err("login GET");
        assert_eq!(login_rejection.status, 405);
        assert!(
            login_rejection
                .headers
                .contains(&("allow".into(), "POST".into()))
        );
        assert!(check_logout_method("POST").is_ok());
        let before =
            crate::spa_auth::spa_auth_legacy_get_count(SpaAuthLegacyEndpoint::MsalAuthLogout);
        let get_rejection = check_logout_method("GET").expect_err("logout GET");
        assert_eq!(get_rejection.status, 405);
        assert!(
            get_rejection
                .headers
                .contains(&("allow".into(), "POST".into()))
        );
        assert_eq!(
            crate::spa_auth::spa_auth_legacy_get_count(SpaAuthLegacyEndpoint::MsalAuthLogout),
            before + 1
        );
        let rejection = check_logout_method("PATCH").expect_err("logout PATCH");
        assert_eq!(rejection.status, 405);
        assert_eq!(rejection.code, "ERR10008");
        assert!(rejection.headers.contains(&("allow".into(), "POST".into())));
    }

    #[test]
    fn msal_auth_logout_csrf_defaults_to_observe_only() {
        assert!(!MsalAuthConfig::default().logout_csrf_enforced);
    }

    #[test]
    fn msal_auth_logout_matches_no_content_contract() {
        let response = msal_auth_logout_response(&MsalAuthConfig::default());
        assert_eq!(response.status, 204);
        assert_eq!(response.content_type, None);
        assert!(response.body.is_empty());
        assert_eq!(
            response
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .count(),
            2
        );
        assert!(
            response
                .headers
                .contains(&("cache-control".into(), "no-store".into()))
        );
    }
}
