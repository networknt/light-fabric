use crate::security::{
    JwtExpiryMode, load_security_runtime, load_security_runtime_from_file, verify_jwt_token,
};
use crate::spa_auth::{
    AUTHORIZATION_HEADER, SpaAuthResponse, SpaCookieConfig, SpaSessionOutcome, SpaSessionRuntime,
    bearer_token, bearer_token_from_header, delete_cookie_header, generate_csrf,
    load_spa_token_client, request_cookie, session_cookie_header,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MSAL_EXCHANGE_FILE: &str = "msal-exchange.yml";
pub const MSAL_EXCHANGE_LEGACY_FILE: &str = "msal-exchange.yaml";
pub const MSAL_EXCHANGE_MODULE_ID: &str = "light-pingora/msal-exchange";
pub const MSAL_EXCHANGE_CONFIG_NAME: &str = "msal-exchange";
pub const SECURITY_MSAL_FILE: &str = "security-msal.yml";
pub const SECURITY_MSAL_MODULE_ID: &str = "light-pingora/security-msal";
pub const SECURITY_MSAL_CONFIG_NAME: &str = "security-msal";
const DEFAULT_LIGHT_TOKEN_HEADER: &str = "X-Light-Token";
const DEFAULT_MSAL_ACCESS_TOKEN_HEADER: &str = "X-MSAL-Access-Token";
const DEFAULT_MSAL_ACCESS_TOKEN_COOKIE: &str = "msalAccessToken";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MsalAuthorizationToken {
    LightOauth,
    AzureMsal,
}

impl Default for MsalAuthorizationToken {
    fn default() -> Self {
        Self::LightOauth
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MsalExchangeConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_exchange_path")]
    pub exchange_path: String,
    #[serde(default = "default_logout_path")]
    pub logout_path: String,
    #[serde(default = "default_cookie_domain")]
    pub cookie_domain: String,
    #[serde(default = "default_cookie_path")]
    pub cookie_path: String,
    #[serde(default)]
    pub cookie_secure: bool,
    #[serde(default = "default_session_timeout")]
    pub session_timeout: u64,
    #[serde(default = "default_remember_me_timeout")]
    pub remember_me_timeout: u64,
    #[serde(default = "default_renew_before_seconds")]
    pub renew_before_seconds: u64,
    #[serde(default = "default_refresh_wait_ms")]
    pub refresh_single_flight_wait_ms: u64,
    #[serde(default = "default_refresh_cache_ms")]
    pub refresh_single_flight_cache_ms: u64,
    #[serde(default = "default_refresh_max_entries")]
    pub refresh_single_flight_max_entries: usize,
    #[serde(default)]
    pub cookie_same_site: crate::spa_auth::CookieSameSite,
    #[serde(default = "default_cookie_timeout_uri")]
    pub cookie_timeout_uri: String,
    #[serde(default)]
    pub subject_token_type: Option<String>,
    #[serde(default)]
    pub authorization_token: MsalAuthorizationToken,
    #[serde(default = "default_light_token_header")]
    pub light_token_header: String,
    #[serde(default = "default_msal_access_token_header")]
    pub msal_access_token_header: String,
    #[serde(default = "default_msal_access_token_cookie")]
    pub msal_access_token_cookie: String,
}

impl Default for MsalExchangeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            exchange_path: default_exchange_path(),
            logout_path: default_logout_path(),
            cookie_domain: default_cookie_domain(),
            cookie_path: default_cookie_path(),
            cookie_secure: false,
            session_timeout: default_session_timeout(),
            remember_me_timeout: default_remember_me_timeout(),
            renew_before_seconds: default_renew_before_seconds(),
            refresh_single_flight_wait_ms: default_refresh_wait_ms(),
            refresh_single_flight_cache_ms: default_refresh_cache_ms(),
            refresh_single_flight_max_entries: default_refresh_max_entries(),
            cookie_same_site: crate::spa_auth::CookieSameSite::None,
            cookie_timeout_uri: default_cookie_timeout_uri(),
            subject_token_type: None,
            authorization_token: MsalAuthorizationToken::default(),
            light_token_header: default_light_token_header(),
            msal_access_token_header: default_msal_access_token_header(),
            msal_access_token_cookie: default_msal_access_token_cookie(),
        }
    }
}

#[derive(Clone)]
pub struct MsalExchangeRuntime {
    config: MsalExchangeConfig,
    session: SpaSessionRuntime,
    msal_security: crate::SecurityRuntime,
}

impl MsalExchangeRuntime {
    pub fn new(
        config: MsalExchangeConfig,
        session: SpaSessionRuntime,
        msal_security: crate::SecurityRuntime,
    ) -> Self {
        Self {
            config,
            session,
            msal_security,
        }
    }

    pub async fn bootstrap(&self) -> Result<(), crate::HandlerRejection> {
        self.msal_security.bootstrap().await
    }

    pub fn config(&self) -> &MsalExchangeConfig {
        &self.config
    }

    pub async fn handle_request(
        &self,
        session: &mut Session,
    ) -> Result<MsalExchangeOutcome, crate::HandlerRejection> {
        let path = session.req_header().uri.path();
        if path == self.config.exchange_path {
            return self.handle_exchange(session).await;
        }
        if path == self.config.logout_path {
            return Ok(MsalExchangeOutcome::Respond(self.logout_response()));
        }
        if matches!(
            self.config.authorization_token,
            MsalAuthorizationToken::AzureMsal
        ) && self.session.has_session_cookie(session)
        {
            let msal_access_token = self.msal_access_token_from_cookie(session).await?;
            self.inject_msal_authorization(session, msal_access_token.as_str())?;
        }
        let light_token_header = self.light_token_header();
        match self
            .session
            .validate_or_refresh_with_token_header(session, light_token_header)
            .await?
        {
            SpaSessionOutcome::Continue {
                auth,
                response_headers,
            } => Ok(MsalExchangeOutcome::Continue {
                auth,
                response_headers,
            }),
            SpaSessionOutcome::Respond(response) => Ok(MsalExchangeOutcome::Respond(
                self.with_msal_access_delete(response),
            )),
        }
    }

    async fn verify_msal_token(
        &self,
        token: &str,
    ) -> Result<crate::AuthPrincipal, crate::HandlerRejection> {
        verify_jwt_token(&self.msal_security, token, JwtExpiryMode::Enforce)
            .await
            .map_err(|error| crate::HandlerRejection::new(error.status, "ERR10000", error.message))
    }

    async fn msal_access_token_from_cookie(
        &self,
        session: &Session,
    ) -> Result<String, crate::HandlerRejection> {
        let token = request_cookie(session, self.config.msal_access_token_cookie.as_str())
            .ok_or_else(|| {
                crate::HandlerRejection::new(401, "ERR11647", "Microsoft access token is missing")
            })?;
        self.verify_msal_token(token.as_str()).await?;
        Ok(token)
    }

    fn inject_msal_authorization(
        &self,
        session: &mut Session,
        token: &str,
    ) -> Result<(), crate::HandlerRejection> {
        session
            .req_header_mut()
            .insert_header(AUTHORIZATION_HEADER, format!("Bearer {token}"))
            .map_err(|_| {
                crate::HandlerRejection::new(500, "ERR10001", "invalid Authorization header")
            })?;
        Ok(())
    }

    fn light_token_header(&self) -> &str {
        match self.config.authorization_token {
            MsalAuthorizationToken::LightOauth => AUTHORIZATION_HEADER,
            MsalAuthorizationToken::AzureMsal => self.config.light_token_header.as_str(),
        }
    }

    async fn handle_exchange(
        &self,
        session: &mut Session,
    ) -> Result<MsalExchangeOutcome, crate::HandlerRejection> {
        let microsoft_token = bearer_token(session).ok_or_else(|| {
            crate::HandlerRejection::new(401, "ERR11647", "Microsoft bearer token is missing")
        })?;
        self.verify_msal_token(microsoft_token.as_str()).await?;

        let msal_access_cookie = if matches!(
            self.config.authorization_token,
            MsalAuthorizationToken::AzureMsal
        ) {
            let access_token =
                bearer_token_from_header(session, self.config.msal_access_token_header.as_str())
                    .ok_or_else(|| {
                        crate::HandlerRejection::new(
                            401,
                            "ERR11647",
                            "Microsoft access token is missing",
                        )
                    })?;
            let principal = self.verify_msal_token(access_token.as_str()).await?;
            Some((
                access_token,
                msal_access_cookie_max_age(&principal, self.config.session_timeout),
            ))
        } else {
            None
        };

        let csrf = generate_csrf();
        let token = self
            .session
            .exchange_subject_token(
                microsoft_token.as_str(),
                self.config.subject_token_type.as_deref(),
                csrf.as_str(),
            )
            .await
            .map_err(|error| {
                crate::HandlerRejection::new(error.status, "ERR11648", error.message)
            })?;
        let (scopes, headers) = self
            .session
            .set_login_cookies(&token, csrf.as_str())
            .await?;
        let headers = self.with_msal_access_cookie(
            headers,
            msal_access_cookie
                .as_ref()
                .map(|(token, max_age)| (token.as_str(), *max_age)),
        );
        Ok(MsalExchangeOutcome::Respond(SpaAuthResponse::json(
            200,
            json!({ "scopes": scopes }),
            headers,
        )))
    }

    fn with_msal_access_cookie(
        &self,
        mut headers: Vec<(String, String)>,
        msal_access_token: Option<(&str, u64)>,
    ) -> Vec<(String, String)> {
        if let Some((token, max_age)) = msal_access_token {
            headers.push(session_cookie_header(
                self.session.cookies(),
                self.config.msal_access_token_cookie.as_str(),
                token,
                max_age,
                true,
            ));
        }
        headers
    }

    fn logout_response(&self) -> SpaAuthResponse {
        let mut response = self.session.logout_response();
        response.headers.push(delete_cookie_header(
            self.session.cookies(),
            self.config.msal_access_token_cookie.as_str(),
            true,
        ));
        response
    }

    fn with_msal_access_delete(&self, mut response: SpaAuthResponse) -> SpaAuthResponse {
        if matches!(
            self.config.authorization_token,
            MsalAuthorizationToken::AzureMsal
        ) {
            response.headers.push(delete_cookie_header(
                self.session.cookies(),
                self.config.msal_access_token_cookie.as_str(),
                true,
            ));
        }
        response
    }
}

pub enum MsalExchangeOutcome {
    Continue {
        auth: Option<crate::AuthPrincipal>,
        response_headers: Vec<(String, String)>,
    },
    Respond(SpaAuthResponse),
}

pub fn load_msal_exchange_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<MsalExchangeRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = load_msal_exchange_config(runtime_config)?.unwrap_or_default();
    validate_msal_exchange_config(&config)?;
    runtime_config.module_registry.register_loaded_config(
        MSAL_EXCHANGE_MODULE_ID,
        MSAL_EXCHANGE_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;
    if !config.enabled {
        return Ok(None);
    }

    let token_client = load_spa_token_client(runtime_config)?;
    let security = load_security_runtime(runtime_config, true)?.ok_or_else(|| {
        RuntimeError::Unsupported(
            "security.yml must enable JWT verification for MSAL exchange".to_string(),
        )
    })?;
    if !runtime_config.config_dir.join(SECURITY_MSAL_FILE).exists()
        && !runtime_config
            .external_config_dir
            .join(SECURITY_MSAL_FILE)
            .exists()
    {
        return Err(RuntimeError::MissingConfig(SECURITY_MSAL_FILE.to_string()));
    }
    let msal_security = load_security_runtime_from_file(
        runtime_config,
        true,
        SECURITY_MSAL_FILE,
        SECURITY_MSAL_MODULE_ID,
        SECURITY_MSAL_CONFIG_NAME,
    )?
    .ok_or_else(|| {
        RuntimeError::Unsupported(
            "security-msal.yml must enable JWT verification for MSAL exchange".to_string(),
        )
    })?;
    let session = SpaSessionRuntime::new(cookie_config(&config), token_client, security);
    Ok(Some(MsalExchangeRuntime::new(
        config,
        session,
        msal_security,
    )))
}

fn load_msal_exchange_config(
    runtime_config: &RuntimeConfig,
) -> Result<Option<MsalExchangeConfig>, RuntimeError> {
    for file in [MSAL_EXCHANGE_FILE, MSAL_EXCHANGE_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<MsalExchangeConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(config)),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn cookie_config(config: &MsalExchangeConfig) -> SpaCookieConfig {
    SpaCookieConfig {
        cookie_domain: config.cookie_domain.clone(),
        cookie_path: config.cookie_path.clone(),
        cookie_secure: config.cookie_secure,
        session_timeout: config.session_timeout,
        remember_me_timeout: config.remember_me_timeout,
        cookie_same_site: config.cookie_same_site.clone(),
        renew_before_seconds: config.renew_before_seconds,
        refresh_single_flight_wait_ms: config.refresh_single_flight_wait_ms,
        refresh_single_flight_cache_ms: config.refresh_single_flight_cache_ms,
        refresh_single_flight_max_entries: config.refresh_single_flight_max_entries,
        cookie_timeout_uri: config.cookie_timeout_uri.clone(),
    }
}

fn default_true() -> bool {
    true
}

fn default_exchange_path() -> String {
    "/auth/ms/exchange".to_string()
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

fn default_cookie_timeout_uri() -> String {
    "/".to_string()
}

fn default_light_token_header() -> String {
    DEFAULT_LIGHT_TOKEN_HEADER.to_string()
}

fn default_msal_access_token_header() -> String {
    DEFAULT_MSAL_ACCESS_TOKEN_HEADER.to_string()
}

fn default_msal_access_token_cookie() -> String {
    DEFAULT_MSAL_ACCESS_TOKEN_COOKIE.to_string()
}

fn validate_msal_exchange_config(config: &MsalExchangeConfig) -> Result<(), RuntimeError> {
    if matches!(
        config.authorization_token,
        MsalAuthorizationToken::AzureMsal
    ) {
        let header = config.light_token_header.trim();
        if header.is_empty() {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.lightTokenHeader must not be empty when authorizationToken is azure-msal"
                    .to_string(),
            ));
        }
        if header.eq_ignore_ascii_case(AUTHORIZATION_HEADER) {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.lightTokenHeader must not be Authorization; use authorizationToken: light-oauth for that placement"
                    .to_string(),
            ));
        }
        let access_header = config.msal_access_token_header.trim();
        if access_header.is_empty() {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.msalAccessTokenHeader must not be empty when authorizationToken is azure-msal"
                    .to_string(),
            ));
        }
        if access_header.eq_ignore_ascii_case(AUTHORIZATION_HEADER) {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.msalAccessTokenHeader must not be Authorization because Authorization carries the MSAL ID token on the exchange endpoint"
                    .to_string(),
            ));
        }
        if access_header.eq_ignore_ascii_case(header) {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.msalAccessTokenHeader must be different from lightTokenHeader"
                    .to_string(),
            ));
        }
        if config.msal_access_token_cookie.trim().is_empty() {
            return Err(RuntimeError::Unsupported(
                "msal-exchange.msalAccessTokenCookie must not be empty when authorizationToken is azure-msal"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn msal_access_cookie_max_age(principal: &crate::AuthPrincipal, session_timeout: u64) -> u64 {
    principal
        .claims
        .get("exp")
        .and_then(serde_json::Value::as_u64)
        .map(|exp| exp.saturating_sub(now_seconds()).min(session_timeout))
        .filter(|max_age| *max_age > 0)
        .unwrap_or(session_timeout)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn default_session_timeout() -> u64 {
    3600
}

fn default_remember_me_timeout() -> u64 {
    604800
}

fn default_renew_before_seconds() -> u64 {
    90
}

fn default_refresh_wait_ms() -> u64 {
    5_000
}

fn default_refresh_cache_ms() -> u64 {
    3_000
}

fn default_refresh_max_entries() -> usize {
    10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msal_exchange_config_accepts_subject_token_override() {
        let config: MsalExchangeConfig = serde_yaml::from_str(
            r#"
exchangePath: /ms/exchange
subjectTokenType: urn:ietf:params:oauth:token-type:jwt
"#,
        )
        .expect("parse config");

        assert_eq!(config.exchange_path, "/ms/exchange");
        assert_eq!(
            config.subject_token_type.as_deref(),
            Some("urn:ietf:params:oauth:token-type:jwt")
        );
        assert_eq!(
            config.authorization_token,
            MsalAuthorizationToken::LightOauth
        );
        assert_eq!(config.light_token_header, DEFAULT_LIGHT_TOKEN_HEADER);
        assert_eq!(
            config.msal_access_token_header,
            DEFAULT_MSAL_ACCESS_TOKEN_HEADER
        );
        assert_eq!(
            config.msal_access_token_cookie,
            DEFAULT_MSAL_ACCESS_TOKEN_COOKIE
        );
    }

    #[test]
    fn msal_exchange_config_accepts_azure_msal_token_placement() {
        let config: MsalExchangeConfig = serde_yaml::from_str(
            r#"
authorizationToken: azure-msal
lightTokenHeader: X-Light-Token
msalAccessTokenHeader: X-MSAL-Access-Token
msalAccessTokenCookie: msalAccessToken
"#,
        )
        .expect("parse config");

        assert_eq!(
            config.authorization_token,
            MsalAuthorizationToken::AzureMsal
        );
        assert_eq!(config.light_token_header, "X-Light-Token");
        assert_eq!(config.msal_access_token_header, "X-MSAL-Access-Token");
        assert_eq!(config.msal_access_token_cookie, "msalAccessToken");
        validate_msal_exchange_config(&config).expect("valid placement");
    }

    #[test]
    fn msal_exchange_config_rejects_authorization_as_light_header_in_azure_mode() {
        let config: MsalExchangeConfig = serde_yaml::from_str(
            r#"
authorizationToken: azure-msal
lightTokenHeader: Authorization
"#,
        )
        .expect("parse config");

        let error = validate_msal_exchange_config(&config).expect_err("invalid placement");
        assert!(
            error
                .to_string()
                .contains("lightTokenHeader must not be Authorization")
        );
    }

    #[test]
    fn msal_exchange_config_rejects_authorization_as_msal_access_header_in_azure_mode() {
        let config: MsalExchangeConfig = serde_yaml::from_str(
            r#"
authorizationToken: azure-msal
lightTokenHeader: X-Light-Token
msalAccessTokenHeader: Authorization
"#,
        )
        .expect("parse config");

        let error = validate_msal_exchange_config(&config).expect_err("invalid placement");
        assert!(
            error
                .to_string()
                .contains("msalAccessTokenHeader must not be Authorization")
        );
    }

    #[test]
    fn msal_exchange_config_rejects_shared_msal_access_and_light_headers() {
        let config: MsalExchangeConfig = serde_yaml::from_str(
            r#"
authorizationToken: azure-msal
lightTokenHeader: X-Light-Token
msalAccessTokenHeader: X-Light-Token
"#,
        )
        .expect("parse config");

        let error = validate_msal_exchange_config(&config).expect_err("invalid placement");
        assert!(
            error
                .to_string()
                .contains("msalAccessTokenHeader must be different from lightTokenHeader")
        );
    }

    #[test]
    fn test_msal_access_cookie_max_age() {
        let principal = crate::AuthPrincipal {
            claims: serde_json::json!({
                "exp": now_seconds() + 500
            }),
            ..Default::default()
        };
        assert_eq!(msal_access_cookie_max_age(&principal, 1000), 500);

        let principal_no_exp = crate::AuthPrincipal {
            claims: serde_json::json!({}),
            ..Default::default()
        };
        assert_eq!(msal_access_cookie_max_age(&principal_no_exp, 1000), 1000);
    }
}
