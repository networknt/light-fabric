use crate::HandlerRejection;
use crate::security::{
    JwtExpiryMode, SecurityRuntime, load_security_runtime_from_file, verify_jwt_token,
};
use crate::spa_auth::{
    AUTHORIZATION_HEADER, CookieSameSite, SpaAuthResponse, SpaCookieConfig, SpaSessionOutcome,
    bearer_token, delete_cookie_header, generate_csrf,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::warn;

pub const MSAL_AUTH_FILE: &str = "msal-auth.yml";
pub const MSAL_AUTH_MODULE_ID: &str = "light-pingora/msal-auth";
pub const MSAL_AUTH_CONFIG_NAME: &str = "msal-auth";
pub const SECURITY_MSAL_FILE: &str = "security-msal.yml";
pub const SECURITY_MSAL_MODULE_ID: &str = "light-pingora/security-msal";
pub const SECURITY_MSAL_CONFIG_NAME: &str = "security-msal";

const ACCESS_TOKEN_COOKIE: &str = "accessToken";
const CSRF_COOKIE: &str = "csrf";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MsalAuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_login_path")]
    pub login_path: String,
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
    #[serde(default)]
    pub cookie_same_site: CookieSameSite,
}

impl Default for MsalAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            login_path: default_login_path(),
            logout_path: default_logout_path(),
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

pub struct MsalAuthRuntime {
    pub config: MsalAuthConfig,
    pub msal_security: SecurityRuntime,
}

impl MsalAuthRuntime {
    pub fn new(config: MsalAuthConfig, msal_security: SecurityRuntime) -> Self {
        Self {
            config,
            msal_security,
        }
    }

    pub async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        self.msal_security.bootstrap().await
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
            return self.handle_login(session).await;
        }

        if path == self.config.logout_path {
            return self.handle_logout(session).await;
        }

        let cookies = crate::spa_auth::request_cookies(session);
        if let Some(access_token) = cookies.get(ACCESS_TOKEN_COOKIE) {
            let principal = self
                .validate_session(session, access_token, &cookies)
                .await?;
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

        let principal = verify_jwt_token(
            &self.msal_security,
            microsoft_token.as_str(),
            JwtExpiryMode::Enforce,
        )
        .await
        .map_err(|error| HandlerRejection::new(error.status, "ERR10000", error.message))?;

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

        Ok(SpaSessionOutcome::Respond(SpaAuthResponse::json(
            200, body, headers,
        )))
    }

    async fn handle_logout(
        &self,
        _session: &mut Session,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        let headers = vec![
            delete_cookie_header(&self.config.cookie_config(), ACCESS_TOKEN_COOKIE, true),
            delete_cookie_header(&self.config.cookie_config(), CSRF_COOKIE, false),
        ];

        let body = json!({
            "message": "logged out"
        });

        Ok(SpaSessionOutcome::Respond(SpaAuthResponse::json(
            200, body, headers,
        )))
    }

    async fn validate_session(
        &self,
        session: &mut Session,
        access_token: &str,
        cookies: &std::collections::BTreeMap<String, String>,
    ) -> Result<crate::AuthPrincipal, HandlerRejection> {
        let principal = verify_jwt_token(&self.msal_security, access_token, JwtExpiryMode::Enforce)
            .await
            .map_err(|error| HandlerRejection::new(error.status, "ERR10000", error.message))?;

        // Double submit cookie CSRF validation
        let cookie_csrf = cookies
            .get(CSRF_COOKIE)
            .ok_or_else(|| HandlerRejection::new(401, "ERR10036", "Missing CSRF cookie"))?;

        let header_csrf = crate::spa_auth::request_csrf(session)
            .ok_or_else(|| HandlerRejection::new(401, "ERR10036", "Missing CSRF header"))?;

        if cookie_csrf != &header_csrf {
            warn!(
                "CSRF mismatch: header '{}' != cookie '{}'",
                header_csrf, cookie_csrf
            );
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

    let msal_security = match load_security_runtime_from_file(
        runtime_config,
        true,
        SECURITY_MSAL_FILE,
        SECURITY_MSAL_MODULE_ID,
        SECURITY_MSAL_CONFIG_NAME,
    )? {
        Some(s) => s,
        None => {
            return Err(RuntimeError::MissingConfig(SECURITY_MSAL_FILE.to_string()));
        }
    };

    Ok(Some(MsalAuthRuntime::new(config, msal_security)))
}
