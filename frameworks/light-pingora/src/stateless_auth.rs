use crate::security::load_security_runtime;
use crate::spa_auth::{
    SpaAuthResponse, SpaCookieConfig, SpaSessionOutcome, SpaSessionRuntime, generate_csrf,
    load_spa_token_client, query_param, social_scopes,
};
use light_client::{ClientFactory, EndpointOptions};
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use serde_json::json;

pub const STATELESS_AUTH_FILE: &str = "statelessAuth.yml";
pub const STATELESS_AUTH_LEGACY_FILE: &str = "statelessAuth.yaml";
pub const STATELESS_AUTH_MODULE_ID: &str = "light-pingora/stateless-auth";
pub const STATELESS_AUTH_CONFIG_NAME: &str = "statelessAuth";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StatelessAuthConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
    #[serde(default)]
    pub deny_uri: Option<String>,
    #[serde(default)]
    pub enable_http2: bool,
    #[serde(default = "default_auth_path")]
    pub auth_path: String,
    #[serde(default = "default_logout_path")]
    pub logout_path: String,
    #[serde(default = "default_cookie_domain")]
    pub cookie_domain: String,
    #[serde(default = "default_cookie_path")]
    pub cookie_path: String,
    #[serde(default = "default_cookie_timeout_uri")]
    pub cookie_timeout_uri: String,
    #[serde(default = "default_true")]
    pub cookie_secure: bool,
    #[serde(default = "default_session_timeout")]
    pub session_timeout: u64,
    #[serde(default = "default_remember_me_timeout")]
    pub remember_me_timeout: u64,
    #[serde(default)]
    pub bootstrap_token: Option<String>,
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

    #[serde(default = "default_google_path")]
    pub google_path: String,
    #[serde(default)]
    pub google_client_id: String,
    #[serde(default)]
    pub google_client_secret: String,
    #[serde(default)]
    pub google_redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "social_scopes")]
    pub google_scope: Vec<String>,
    #[serde(default = "default_google_token_endpoint")]
    pub google_token_endpoint: String,

    #[serde(default = "default_facebook_path")]
    pub facebook_path: String,
    #[serde(default)]
    pub facebook_client_id: String,
    #[serde(default)]
    pub facebook_client_secret: String,
    #[serde(default)]
    pub facebook_redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "social_scopes")]
    pub facebook_scope: Vec<String>,
    #[serde(default = "default_facebook_token_endpoint")]
    pub facebook_token_endpoint: String,

    #[serde(default = "default_github_path")]
    pub github_path: String,
    #[serde(default)]
    pub github_client_id: String,
    #[serde(default)]
    pub github_client_secret: String,
    #[serde(default)]
    pub github_redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "social_scopes")]
    pub github_scope: Vec<String>,
    #[serde(default = "default_github_token_endpoint")]
    pub github_token_endpoint: String,
}

impl Default for StatelessAuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            redirect_uri: default_redirect_uri(),
            deny_uri: None,
            enable_http2: false,
            auth_path: default_auth_path(),
            logout_path: default_logout_path(),
            cookie_domain: default_cookie_domain(),
            cookie_path: default_cookie_path(),
            cookie_timeout_uri: default_cookie_timeout_uri(),
            cookie_secure: true,
            session_timeout: default_session_timeout(),
            remember_me_timeout: default_remember_me_timeout(),
            bootstrap_token: None,
            renew_before_seconds: default_renew_before_seconds(),
            refresh_single_flight_wait_ms: default_refresh_wait_ms(),
            refresh_single_flight_cache_ms: default_refresh_cache_ms(),
            refresh_single_flight_max_entries: default_refresh_max_entries(),
            cookie_same_site: crate::spa_auth::CookieSameSite::None,
            google_path: default_google_path(),
            google_client_id: String::new(),
            google_client_secret: String::new(),
            google_redirect_uri: None,
            google_scope: Vec::new(),
            google_token_endpoint: default_google_token_endpoint(),
            facebook_path: default_facebook_path(),
            facebook_client_id: String::new(),
            facebook_client_secret: String::new(),
            facebook_redirect_uri: None,
            facebook_scope: Vec::new(),
            facebook_token_endpoint: default_facebook_token_endpoint(),
            github_path: default_github_path(),
            github_client_id: String::new(),
            github_client_secret: String::new(),
            github_redirect_uri: None,
            github_scope: Vec::new(),
            github_token_endpoint: default_github_token_endpoint(),
        }
    }
}

#[derive(Clone)]
pub struct StatelessAuthRuntime {
    config: StatelessAuthConfig,
    session: SpaSessionRuntime,
}

impl StatelessAuthRuntime {
    pub fn new(config: StatelessAuthConfig, session: SpaSessionRuntime) -> Self {
        Self { config, session }
    }

    pub fn config(&self) -> &StatelessAuthConfig {
        &self.config
    }

    pub async fn handle_request(
        &self,
        session: &mut Session,
        handler_id: &str,
    ) -> Result<StatelessAuthOutcome, crate::HandlerRejection> {
        let path = session.req_header().uri.path();
        match handler_id {
            "stateless" if path == self.config.auth_path => self.handle_login(session).await,
            "stateless" if path == self.config.logout_path => Ok(StatelessAuthOutcome::Respond(
                self.session.logout_response(),
            )),
            "stateless" => self.handle_session(session).await,
            "google" if path == self.config.google_path => {
                self.handle_social_login(session, SocialProvider::Google)
                    .await
            }
            "facebook" if path == self.config.facebook_path => {
                self.handle_social_login(session, SocialProvider::Facebook)
                    .await
            }
            "github" if path == self.config.github_path => {
                self.handle_social_login(session, SocialProvider::Github)
                    .await
            }
            _ => Ok(StatelessAuthOutcome::Continue {
                auth: None,
                response_headers: Vec::new(),
            }),
        }
    }

    async fn handle_login(
        &self,
        session: &mut Session,
    ) -> Result<StatelessAuthOutcome, crate::HandlerRejection> {
        let code = query_param(session, "code").ok_or_else(|| {
            crate::HandlerRejection::new(400, "ERR10035", "authorization code is missing")
        })?;
        let state = query_param(session, "state");
        let csrf = generate_csrf();
        let token = self
            .session
            .exchange_authorization_code(code.as_str(), csrf.as_str())
            .await?;
        let (scopes, headers) = self
            .session
            .set_login_cookies(&token, csrf.as_str())
            .await?;
        Ok(StatelessAuthOutcome::Respond(self.login_response(
            scopes,
            headers,
            state.as_deref(),
        )))
    }

    async fn handle_social_login(
        &self,
        session: &mut Session,
        provider: SocialProvider,
    ) -> Result<StatelessAuthOutcome, crate::HandlerRejection> {
        let state = query_param(session, "state");
        let csrf = generate_csrf();
        let subject = self.provider_subject(session, provider).await?;
        let token = self
            .session
            .exchange_subject_token(
                subject.token.as_str(),
                Some(subject.token_type.as_str()),
                csrf.as_str(),
            )
            .await?;
        let (scopes, headers) = self
            .session
            .set_login_cookies(&token, csrf.as_str())
            .await?;
        Ok(StatelessAuthOutcome::Respond(self.login_response(
            scopes,
            headers,
            state.as_deref(),
        )))
    }

    async fn provider_subject(
        &self,
        session: &Session,
        provider: SocialProvider,
    ) -> Result<SocialSubjectToken, crate::HandlerRejection> {
        match provider {
            SocialProvider::Google => {
                let code = required_query_param(session, "code")?;
                let body = self
                    .exchange_provider_code(
                        self.config.google_token_endpoint.as_str(),
                        self.config.google_client_id.as_str(),
                        self.config.google_client_secret.as_str(),
                        self.config.google_redirect_uri.as_deref(),
                        &[
                            ("grant_type", "authorization_code"),
                            ("code", code.as_str()),
                        ],
                    )
                    .await?;
                provider_token_from_body(
                    &body,
                    &[
                        ("id_token", "urn:ietf:params:oauth:token-type:id_token"),
                        (
                            "access_token",
                            "urn:ietf:params:oauth:token-type:access_token",
                        ),
                    ],
                    "google",
                )
            }
            SocialProvider::Facebook => {
                if let Some(access_token) = query_param(session, "accessToken")
                    .or_else(|| query_param(session, "access_token"))
                {
                    return Ok(SocialSubjectToken {
                        token: access_token,
                        token_type: "urn:ietf:params:oauth:token-type:access_token".to_string(),
                    });
                }
                let code = required_query_param(session, "code")?;
                let body = self
                    .exchange_provider_code(
                        self.config.facebook_token_endpoint.as_str(),
                        self.config.facebook_client_id.as_str(),
                        self.config.facebook_client_secret.as_str(),
                        self.config.facebook_redirect_uri.as_deref(),
                        &[("code", code.as_str())],
                    )
                    .await?;
                provider_token_from_body(
                    &body,
                    &[(
                        "access_token",
                        "urn:ietf:params:oauth:token-type:access_token",
                    )],
                    "facebook",
                )
            }
            SocialProvider::Github => {
                let code = required_query_param(session, "code")?;
                let body = self
                    .exchange_provider_code(
                        self.config.github_token_endpoint.as_str(),
                        self.config.github_client_id.as_str(),
                        self.config.github_client_secret.as_str(),
                        self.config.github_redirect_uri.as_deref(),
                        &[("code", code.as_str())],
                    )
                    .await?;
                provider_token_from_body(
                    &body,
                    &[(
                        "access_token",
                        "urn:ietf:params:oauth:token-type:access_token",
                    )],
                    "github",
                )
            }
        }
    }

    async fn exchange_provider_code(
        &self,
        endpoint: &str,
        client_id: &str,
        client_secret: &str,
        redirect_uri: Option<&str>,
        extra_form: &[(&str, &str)],
    ) -> Result<JsonValue, crate::HandlerRejection> {
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Err(crate::HandlerRejection::new(
                500,
                "ERR10074",
                "social provider client id and secret are required",
            ));
        }
        let endpoint = endpoint.trim();
        if endpoint.is_empty() {
            return Err(crate::HandlerRejection::new(
                500,
                "ERR10074",
                "social provider token endpoint is required",
            ));
        }
        let config = self.session.token_client().config();
        let mut form = vec![
            ("client_id", client_id.to_string()),
            ("client_secret", client_secret.to_string()),
        ];
        if let Some(redirect_uri) = redirect_uri.filter(|value| !value.trim().is_empty()) {
            form.push(("redirect_uri", redirect_uri.to_string()));
        }
        for (name, value) in extra_form {
            form.push((*name, (*value).to_string()));
        }
        let response = ClientFactory::from_config(config)
            .reqwest_client(EndpointOptions {
                enable_http2: Some(self.config.enable_http2),
                ..EndpointOptions::default()
            })
            .map_err(|error| {
                crate::HandlerRejection::new(
                    500,
                    "ERR10056",
                    format!("invalid social provider HTTP client: {error}"),
                )
            })?
            .post(endpoint)
            .header("accept", "application/json")
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                crate::HandlerRejection::new(
                    502,
                    "ERR10052",
                    format!("failed to exchange social provider code: {error}"),
                )
            })?;
        let status = response.status();
        let body = response.json::<JsonValue>().await.map_err(|error| {
            crate::HandlerRejection::new(
                502,
                "ERR10052",
                format!("failed to parse social provider response: {error}"),
            )
        })?;
        if !status.is_success() {
            return Err(crate::HandlerRejection::new(
                502,
                "ERR10052",
                format!(
                    "social provider token endpoint returned {status}: {}",
                    provider_error_message(&body)
                ),
            ));
        }
        Ok(body)
    }

    async fn handle_session(
        &self,
        session: &mut Session,
    ) -> Result<StatelessAuthOutcome, crate::HandlerRejection> {
        match self.session.validate_or_refresh(session).await? {
            SpaSessionOutcome::Continue {
                auth,
                response_headers,
            } => Ok(StatelessAuthOutcome::Continue {
                auth,
                response_headers,
            }),
            SpaSessionOutcome::Respond(response) => Ok(StatelessAuthOutcome::Respond(response)),
        }
    }

    fn login_response(
        &self,
        scopes: Vec<String>,
        headers: Vec<(String, String)>,
        state: Option<&str>,
    ) -> SpaAuthResponse {
        let redirect_uri = append_state(self.config.redirect_uri.as_str(), state);
        let deny_uri = self
            .config
            .deny_uri
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| append_state(value, state))
            .unwrap_or_else(|| redirect_uri.clone());
        SpaAuthResponse::json(
            200,
            json!({
                "scopes": scopes,
                "redirectUri": redirect_uri,
                "denyUri": deny_uri,
            }),
            headers,
        )
    }
}

pub enum StatelessAuthOutcome {
    Continue {
        auth: Option<crate::AuthPrincipal>,
        response_headers: Vec<(String, String)>,
    },
    Respond(SpaAuthResponse),
}

pub fn load_stateless_auth_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<StatelessAuthRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = load_stateless_auth_config(runtime_config)?.unwrap_or_default();
    runtime_config.module_registry.register_loaded_config(
        STATELESS_AUTH_MODULE_ID,
        STATELESS_AUTH_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [
            MaskSpec::key("googleClientSecret"),
            MaskSpec::key("facebookClientSecret"),
            MaskSpec::key("githubClientSecret"),
            MaskSpec::key("bootstrapToken"),
        ],
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
            "security.yml must enable JWT verification for stateless auth".to_string(),
        )
    })?;
    let session = SpaSessionRuntime::new(cookie_config(&config), token_client, security);
    Ok(Some(StatelessAuthRuntime::new(config, session)))
}

fn load_stateless_auth_config(
    runtime_config: &RuntimeConfig,
) -> Result<Option<StatelessAuthConfig>, RuntimeError> {
    for file in [STATELESS_AUTH_FILE, STATELESS_AUTH_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<StatelessAuthConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(config)),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn cookie_config(config: &StatelessAuthConfig) -> SpaCookieConfig {
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

fn append_state(uri: &str, state: Option<&str>) -> String {
    let Some(state) = state else {
        return uri.to_string();
    };
    let separator = if uri.contains('?') { '&' } else { '?' };
    format!("{uri}{separator}state={state}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SocialProvider {
    Google,
    Facebook,
    Github,
}

struct SocialSubjectToken {
    token: String,
    token_type: String,
}

fn required_query_param(session: &Session, name: &str) -> Result<String, crate::HandlerRejection> {
    query_param(session, name).ok_or_else(|| {
        crate::HandlerRejection::new(400, "ERR10035", "authorization code is missing")
    })
}

fn provider_token_from_body(
    body: &JsonValue,
    fields: &[(&str, &str)],
    provider: &str,
) -> Result<SocialSubjectToken, crate::HandlerRejection> {
    fields
        .iter()
        .find_map(|(field, token_type)| {
            body.get(field)
                .and_then(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|value| SocialSubjectToken {
                    token: value.to_string(),
                    token_type: (*token_type).to_string(),
                })
        })
        .ok_or_else(|| {
            crate::HandlerRejection::new(
                502,
                "ERR10052",
                format!("{provider} token response does not contain a usable subject token"),
            )
        })
}

fn provider_error_message(body: &JsonValue) -> String {
    [
        "error_description",
        "error_message",
        "message",
        "error",
        "code",
    ]
    .into_iter()
    .find_map(|key| body.get(key).and_then(JsonValue::as_str))
    .unwrap_or("unknown social provider error")
    .to_string()
}

fn default_true() -> bool {
    true
}

fn default_redirect_uri() -> String {
    "https://localhost:3000/#/app/dashboard".to_string()
}

fn default_auth_path() -> String {
    "/authorization".to_string()
}

fn default_logout_path() -> String {
    "/logout".to_string()
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

fn default_google_path() -> String {
    "/google".to_string()
}

fn default_google_token_endpoint() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

fn default_facebook_path() -> String {
    "/facebook".to_string()
}

fn default_facebook_token_endpoint() -> String {
    "https://graph.facebook.com/v19.0/oauth/access_token".to_string()
}

fn default_github_path() -> String {
    "/github".to_string()
}

fn default_github_token_endpoint() -> String {
    "https://github.com/login/oauth/access_token".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stateless_auth_config_accepts_java_social_fields() {
        let config: StatelessAuthConfig = serde_yaml::from_str(
            r#"
googlePath: /google
googleClientId: google-client
googleClientSecret: google-secret
googleTokenEndpoint: https://oauth2.googleapis.com/token
facebookPath: /facebook
githubPath: /github
"#,
        )
        .expect("parse config");

        assert_eq!(config.google_path, "/google");
        assert_eq!(config.google_client_id, "google-client");
        assert_eq!(config.google_client_secret, "google-secret");
        assert_eq!(
            config.google_token_endpoint,
            "https://oauth2.googleapis.com/token"
        );
        assert_eq!(config.facebook_path, "/facebook");
        assert_eq!(config.github_path, "/github");
    }

    #[test]
    fn append_state_preserves_existing_query() {
        assert_eq!(append_state("/app", Some("abc")), "/app?state=abc");
        assert_eq!(append_state("/app?x=1", Some("abc")), "/app?x=1&state=abc");
        assert_eq!(append_state("/app", None), "/app");
    }

    #[test]
    fn provider_token_prefers_google_id_token() {
        let body = json!({
            "access_token": "access-token",
            "id_token": "id-token"
        });

        let subject = provider_token_from_body(
            &body,
            &[
                ("id_token", "urn:ietf:params:oauth:token-type:id_token"),
                (
                    "access_token",
                    "urn:ietf:params:oauth:token-type:access_token",
                ),
            ],
            "google",
        )
        .expect("subject token");

        assert_eq!(subject.token, "id-token");
        assert_eq!(
            subject.token_type,
            "urn:ietf:params:oauth:token-type:id_token"
        );
    }
}
