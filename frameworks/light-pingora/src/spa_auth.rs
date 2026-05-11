use crate::config_util::deserialize_string_list;
use crate::direct_registry::direct_registry_match;
use crate::security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SecurityRuntime, verify_jwt_token,
};
use crate::token::load_client_config;
use crate::{
    ClientRequestConfig, ClientTlsConfig, ClientTokenConfig, OAuthAuthorizationCodeConfig,
    OAuthRefreshTokenConfig, OAuthTokenConfig, OAuthTokenExchangeConfig,
};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use light_client::{ClientFactory, EndpointOptions};
use light_runtime::{
    DirectRegistryConfig, DiscoveryNode, DiscoverySubscription, PortalRegistryClient,
    RuntimeConfig, RuntimeError,
};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const ACCESS_TOKEN_COOKIE: &str = "accessToken";
pub const REFRESH_TOKEN_COOKIE: &str = "refreshToken";
pub const CSRF_COOKIE: &str = "csrf";
pub const USER_ID_COOKIE: &str = "userId";
pub const USER_TYPE_COOKIE: &str = "userType";
pub const ROLES_COOKIE: &str = "roles";
pub const HOST_COOKIE: &str = "host";
pub const EMAIL_COOKIE: &str = "email";
pub const EID_COOKIE: &str = "eid";

const AUTHORIZATION_HEADER: &str = "authorization";
const CSRF_HEADER: &str = "X-CSRF-TOKEN";
const CSRF_PROTOCOL_PREFIX: &str = "csrf.";
const CONTENT_TYPE_JSON: &str = "application/json";
const DEFAULT_SUBJECT_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:jwt";
const TOKEN_EXCHANGE_GRANT: &str = "urn:ietf:params:oauth:grant-type:token-exchange";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase")]
pub enum CookieSameSite {
    None,
    Lax,
    Strict,
}

impl Default for CookieSameSite {
    fn default() -> Self {
        Self::None
    }
}

impl CookieSameSite {
    fn as_cookie_value(&self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Lax => "Lax",
            Self::Strict => "Strict",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpaCookieConfig {
    pub cookie_domain: String,
    pub cookie_path: String,
    pub cookie_secure: bool,
    pub session_timeout: u64,
    pub remember_me_timeout: u64,
    #[serde(default)]
    pub cookie_same_site: CookieSameSite,
    #[serde(default = "default_renew_before_seconds")]
    pub renew_before_seconds: u64,
    #[serde(default = "default_refresh_wait_ms")]
    pub refresh_single_flight_wait_ms: u64,
    #[serde(default = "default_refresh_cache_ms")]
    pub refresh_single_flight_cache_ms: u64,
    #[serde(default = "default_refresh_max_entries")]
    pub refresh_single_flight_max_entries: usize,
    #[serde(default)]
    pub cookie_timeout_uri: String,
}

#[derive(Clone)]
pub struct SpaSessionRuntime {
    cookies: SpaCookieConfig,
    token_client: Arc<SpaTokenClient>,
    security: Arc<SecurityRuntime>,
    refresh: Arc<RefreshSingleFlight>,
}

impl SpaSessionRuntime {
    pub fn new(
        cookies: SpaCookieConfig,
        token_client: SpaTokenClient,
        security: SecurityRuntime,
    ) -> Self {
        let refresh = RefreshSingleFlight::new(
            cookies.refresh_single_flight_cache_ms,
            cookies.refresh_single_flight_max_entries,
        );
        Self {
            cookies,
            token_client: Arc::new(token_client),
            security: Arc::new(security),
            refresh: Arc::new(refresh),
        }
    }

    pub fn cookies(&self) -> &SpaCookieConfig {
        &self.cookies
    }

    pub fn token_client(&self) -> &SpaTokenClient {
        self.token_client.as_ref()
    }

    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        csrf: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        self.token_client.authorization_code(code, csrf).await
    }

    pub async fn exchange_subject_token(
        &self,
        subject_token: &str,
        subject_token_type: Option<&str>,
        csrf: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        self.token_client
            .token_exchange(subject_token, subject_token_type, csrf)
            .await
    }

    pub async fn set_login_cookies(
        &self,
        response: &TokenGrantResponse,
        csrf: &str,
    ) -> Result<(Vec<String>, Vec<(String, String)>), HandlerRejection> {
        let principal = verify_jwt_token(
            self.security.as_ref(),
            response.access_token.as_str(),
            JwtExpiryMode::Ignore,
        )
        .await
        .map_err(|error| HandlerRejection::new(error.status, "ERR10000", error.message))?;
        let max_age = response
            .expires_in
            .or_else(|| access_token_max_age(&principal.claims))
            .ok_or_else(|| {
                HandlerRejection::new(
                    502,
                    "ERR10052",
                    "token response must contain expires_in or JWT exp",
                )
            })?;
        let headers = session_cookie_headers(&self.cookies, response, csrf, &principal, max_age);
        let scopes = scopes_from_claims(&principal.claims);
        Ok((scopes, headers))
    }

    pub async fn validate_or_refresh(
        &self,
        session: &mut Session,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        let cookies = request_cookies(session);
        let access_token = cookies.get(ACCESS_TOKEN_COOKIE).cloned();
        let refresh_token = cookies.get(REFRESH_TOKEN_COOKIE).cloned();

        if let Some(access_token) = access_token.as_deref() {
            let principal =
                verify_jwt_token(self.security.as_ref(), access_token, JwtExpiryMode::Ignore)
                    .await
                    .map_err(|error| {
                        HandlerRejection::new(error.status, "ERR10000", error.message)
                    })?;
            validate_csrf(session, &principal.claims)?;
            if token_needs_refresh(&principal.claims, self.cookies.renew_before_seconds) {
                return self
                    .renew_or_expire(session, refresh_token.as_deref())
                    .await;
            }
            inject_authorization(session, access_token)?;
            return Ok(SpaSessionOutcome::Continue {
                auth: Some(principal),
                response_headers: Vec::new(),
            });
        }

        if refresh_token.is_some() {
            return self
                .renew_or_expire(session, refresh_token.as_deref())
                .await;
        }

        Ok(SpaSessionOutcome::Continue {
            auth: None,
            response_headers: Vec::new(),
        })
    }

    async fn renew_or_expire(
        &self,
        session: &mut Session,
        refresh_token: Option<&str>,
    ) -> Result<SpaSessionOutcome, HandlerRejection> {
        let Some(refresh_token) = refresh_token.filter(|value| !value.trim().is_empty()) else {
            return Ok(SpaSessionOutcome::Respond(session_expired_response(
                &self.cookies,
            )));
        };
        match self
            .refresh
            .renew(self.token_client.as_ref(), refresh_token)
            .await
        {
            Ok(result) => {
                inject_authorization(session, result.response.access_token.as_str())?;
                let principal = verify_jwt_token(
                    self.security.as_ref(),
                    result.response.access_token.as_str(),
                    JwtExpiryMode::Ignore,
                )
                .await
                .map_err(|error| HandlerRejection::new(error.status, "ERR10000", error.message))?;
                let (_, headers) = self
                    .set_login_cookies(&result.response, &result.csrf)
                    .await?;
                Ok(SpaSessionOutcome::Continue {
                    auth: Some(principal),
                    response_headers: headers,
                })
            }
            Err(_) => Ok(SpaSessionOutcome::Respond(session_expired_response(
                &self.cookies,
            ))),
        }
    }

    pub fn logout_response(&self) -> SpaAuthResponse {
        SpaAuthResponse {
            status: 200,
            content_type: "text/plain; charset=utf-8".to_string(),
            body: Vec::new(),
            headers: delete_cookie_headers(&self.cookies),
        }
    }
}

pub enum SpaSessionOutcome {
    Continue {
        auth: Option<AuthPrincipal>,
        response_headers: Vec<(String, String)>,
    },
    Respond(SpaAuthResponse),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaAuthResponse {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

impl SpaAuthResponse {
    pub fn json(status: u16, value: JsonValue, headers: Vec<(String, String)>) -> Self {
        Self {
            status,
            content_type: CONTENT_TYPE_JSON.to_string(),
            body: serde_json::to_vec(&value).unwrap_or_default(),
            headers,
        }
    }
}

#[derive(Clone)]
pub struct SpaTokenClient {
    config: ClientTokenConfig,
    direct_registry: DirectRegistryConfig,
    registry_client: Option<Arc<PortalRegistryClient>>,
}

impl SpaTokenClient {
    pub fn new(
        config: ClientTokenConfig,
        registry_client: Option<Arc<PortalRegistryClient>>,
    ) -> Self {
        Self::new_with_direct_registry(config, DirectRegistryConfig::default(), registry_client)
    }

    pub fn new_with_direct_registry(
        config: ClientTokenConfig,
        direct_registry: DirectRegistryConfig,
        registry_client: Option<Arc<PortalRegistryClient>>,
    ) -> Self {
        Self {
            config,
            direct_registry,
            registry_client,
        }
    }

    pub fn config(&self) -> &ClientTokenConfig {
        &self.config
    }

    pub async fn authorization_code(
        &self,
        code: &str,
        csrf: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        let token = &self.config.oauth.token;
        let grant = &token.authorization_code;
        let form = authorization_code_form(grant, code, csrf);
        self.send_token_form(
            token,
            grant.uri.as_str(),
            grant.client_id.as_str(),
            grant.client_secret.as_str(),
            form,
            "authorization code",
        )
        .await
    }

    pub async fn refresh_token(
        &self,
        refresh_token: &str,
        csrf: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        let token = &self.config.oauth.token;
        let grant = &token.refresh_token;
        let form = refresh_token_form(grant, refresh_token, csrf);
        self.send_token_form(
            token,
            grant.uri.as_str(),
            grant.client_id.as_str(),
            grant.client_secret.as_str(),
            form,
            "refresh token",
        )
        .await
    }

    pub async fn token_exchange(
        &self,
        subject_token: &str,
        subject_token_type: Option<&str>,
        csrf: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        let token = &self.config.oauth.token;
        let grant = &token.token_exchange;
        let form = token_exchange_form(grant, subject_token, subject_token_type, csrf);
        self.send_token_form(
            token,
            grant.uri.as_str(),
            grant.client_id.as_str(),
            grant.client_secret.as_str(),
            form,
            "token exchange",
        )
        .await
    }

    async fn send_token_form(
        &self,
        token: &OAuthTokenConfig,
        uri: &str,
        client_id: &str,
        client_secret: &str,
        form: Vec<(&'static str, String)>,
        grant_label: &str,
    ) -> Result<TokenGrantResponse, HandlerRejection> {
        if client_id.trim().is_empty() || client_secret.trim().is_empty() {
            return Err(HandlerRejection::new(
                500,
                "ERR10074",
                format!("client.yml {grant_label} client_id and client_secret are required"),
            ));
        }
        let server_url = resolve_token_server_url(
            token,
            &self.direct_registry,
            self.registry_client.as_deref(),
        )
        .await?;
        let url = token_endpoint_url(server_url.as_str(), uri)?;
        let client = token_http_client(token, &self.config.request, &self.config.tls)?;
        let response = client
            .post(url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .header(
                "authorization",
                basic_authorization(client_id, client_secret),
            )
            .form(&form)
            .send()
            .await
            .map_err(|error| {
                HandlerRejection::new(
                    502,
                    "ERR10052",
                    format!("failed to request {grant_label} token: {error}"),
                )
            })?;
        let status = response.status();
        let body = response.json::<JsonValue>().await.map_err(|error| {
            HandlerRejection::new(
                502,
                "ERR10052",
                format!("failed to parse token response: {error}"),
            )
        })?;
        if !status.is_success() {
            return Err(HandlerRejection::new(
                502,
                "ERR10052",
                format!("token endpoint returned {status}: {}", error_message(&body)),
            ));
        }
        serde_json::from_value::<TokenGrantResponse>(body).map_err(|error| {
            HandlerRejection::new(
                502,
                "ERR10052",
                format!("invalid token response shape: {error}"),
            )
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct TokenGrantResponse {
    #[serde(rename = "access_token")]
    pub access_token: String,
    #[serde(default, rename = "refresh_token")]
    pub refresh_token: Option<String>,
    #[serde(default, rename = "expires_in")]
    pub expires_in: Option<u64>,
    #[serde(default)]
    pub remember: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, JsonValue>,
}

#[derive(Debug, Clone)]
pub struct RefreshResult {
    pub response: TokenGrantResponse,
    pub csrf: String,
    completed_at_millis: u64,
}

struct RefreshSingleFlight {
    cache_ms: u64,
    max_entries: usize,
    entries: Mutex<BTreeMap<String, RefreshResult>>,
}

impl RefreshSingleFlight {
    fn new(cache_ms: u64, max_entries: usize) -> Self {
        Self {
            cache_ms,
            max_entries,
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    async fn renew(
        &self,
        client: &SpaTokenClient,
        refresh_token: &str,
    ) -> Result<RefreshResult, HandlerRejection> {
        let now = now_millis();
        let mut entries = self.entries.lock().await;
        entries.retain(|_, value| now.saturating_sub(value.completed_at_millis) <= self.cache_ms);
        if let Some(result) = entries.get(refresh_token) {
            return Ok(result.clone());
        }
        let csrf = generate_csrf();
        let response = client.refresh_token(refresh_token, csrf.as_str()).await?;
        let result = RefreshResult {
            response,
            csrf,
            completed_at_millis: now_millis(),
        };
        if self.max_entries > 0 {
            if entries.len() >= self.max_entries
                && let Some(first_key) = entries.keys().next().cloned()
            {
                entries.remove(&first_key);
            }
            entries.insert(refresh_token.to_string(), result.clone());
        }
        Ok(result)
    }
}

pub fn load_spa_token_client(
    runtime_config: &RuntimeConfig,
) -> Result<SpaTokenClient, RuntimeError> {
    let client = load_client_config(runtime_config)?;
    Ok(SpaTokenClient::new_with_direct_registry(
        client,
        runtime_config.direct_registry.clone(),
        runtime_config.registry_client.clone(),
    ))
}

pub fn generate_csrf() -> String {
    URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes())
}

pub fn query_param(session: &Session, name: &str) -> Option<String> {
    let query = session.req_header().uri.query()?;
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.trim().is_empty())
}

pub fn bearer_token(session: &Session) -> Option<String> {
    let value = request_header(session, AUTHORIZATION_HEADER)?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

pub fn social_scopes<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_string_list(deserializer)
}

fn validate_csrf(session: &Session, claims: &JsonValue) -> Result<(), HandlerRejection> {
    let header_csrf = request_csrf(session)
        .ok_or_else(|| HandlerRejection::new(401, "ERR10036", "X-CSRF-TOKEN header is missing"))?;
    let jwt_csrf = claim_string(claims, "csrf").ok_or_else(|| {
        HandlerRejection::new(401, "ERR10038", "CSRF token is missing in JWT token")
    })?;
    if header_csrf != jwt_csrf {
        return Err(HandlerRejection::new(
            401,
            "ERR10039",
            "CSRF token in request does not match JWT CSRF token",
        ));
    }
    Ok(())
}

fn request_csrf(session: &Session) -> Option<String> {
    request_header(session, CSRF_HEADER)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| websocket_protocol_csrf(session))
        .or_else(|| query_param(session, CSRF_COOKIE))
}

fn websocket_protocol_csrf(session: &Session) -> Option<String> {
    if request_header(session, "Sec-WebSocket-Key").is_none()
        || request_header(session, "Sec-WebSocket-Version").is_none()
    {
        return None;
    }
    request_header(session, "Sec-WebSocket-Protocol").and_then(|value| {
        value.split(',').find_map(|protocol| {
            protocol
                .trim()
                .strip_prefix(CSRF_PROTOCOL_PREFIX)
                .map(str::to_string)
                .filter(|value| !value.trim().is_empty())
        })
    })
}

fn inject_authorization(session: &mut Session, token: &str) -> Result<(), HandlerRejection> {
    session
        .req_header_mut()
        .insert_header(AUTHORIZATION_HEADER, format!("Bearer {token}"))
        .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid Authorization header"))?;
    Ok(())
}

fn request_header(session: &Session, name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn request_cookies(session: &Session) -> BTreeMap<String, String> {
    let mut cookies = BTreeMap::new();
    for value in session.req_header().headers.get_all("cookie") {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for cookie in value.split(';') {
            let Some((name, value)) = cookie.trim().split_once('=') else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() {
                cookies.insert(name.to_string(), value.trim().to_string());
            }
        }
    }
    cookies
}

fn session_cookie_headers(
    config: &SpaCookieConfig,
    response: &TokenGrantResponse,
    csrf: &str,
    principal: &AuthPrincipal,
    max_age: u64,
) -> Vec<(String, String)> {
    let refresh_max_age = if response
        .remember
        .as_deref()
        .is_none_or(|value| value == "N")
    {
        config.session_timeout
    } else {
        config.remember_me_timeout
    };
    let mut headers = Vec::new();
    push_cookie(
        &mut headers,
        config,
        ACCESS_TOKEN_COOKIE,
        response.access_token.as_str(),
        max_age,
        true,
    );
    if let Some(refresh_token) = response.refresh_token.as_deref() {
        push_cookie(
            &mut headers,
            config,
            REFRESH_TOKEN_COOKIE,
            refresh_token,
            refresh_max_age,
            true,
        );
    }
    if let Some(user_id) = claim_string(&principal.claims, "uid")
        .or_else(|| claim_string(&principal.claims, "user_id"))
        .or_else(|| claim_string(&principal.claims, "sub"))
    {
        push_cookie(
            &mut headers,
            config,
            USER_ID_COOKIE,
            &user_id,
            max_age,
            false,
        );
    }
    if let Some(user_type) = claim_string(&principal.claims, "userType") {
        push_cookie(
            &mut headers,
            config,
            USER_TYPE_COOKIE,
            &user_type,
            max_age,
            false,
        );
    }
    let roles = claim_string(&principal.claims, "role")
        .or_else(|| claim_string(&principal.claims, ROLES_COOKIE))
        .unwrap_or_else(|| "user".to_string());
    push_cookie(
        &mut headers,
        config,
        ROLES_COOKIE,
        STANDARD.encode(roles).as_str(),
        max_age,
        false,
    );
    for (cookie_name, claim_name) in [
        (HOST_COOKIE, "host"),
        (EMAIL_COOKIE, "eml"),
        (EID_COOKIE, "eid"),
    ] {
        if let Some(value) = claim_string(&principal.claims, claim_name) {
            push_cookie(&mut headers, config, cookie_name, &value, max_age, false);
        }
    }
    push_cookie(&mut headers, config, CSRF_COOKIE, csrf, max_age, false);
    headers
}

fn push_cookie(
    headers: &mut Vec<(String, String)>,
    config: &SpaCookieConfig,
    name: &str,
    value: &str,
    max_age: u64,
    http_only: bool,
) {
    let mut cookie = format!(
        "{}={}; Max-Age={}; Domain={}; Path={}; SameSite={}",
        name,
        value,
        max_age,
        config.cookie_domain,
        config.cookie_path,
        config.cookie_same_site.as_cookie_value()
    );
    if http_only {
        cookie.push_str("; HttpOnly");
    }
    if config.cookie_secure {
        cookie.push_str("; Secure");
    }
    headers.push(("set-cookie".to_string(), cookie));
}

fn delete_cookie_headers(config: &SpaCookieConfig) -> Vec<(String, String)> {
    [
        ACCESS_TOKEN_COOKIE,
        REFRESH_TOKEN_COOKIE,
        CSRF_COOKIE,
        USER_ID_COOKIE,
        USER_TYPE_COOKIE,
        ROLES_COOKIE,
        HOST_COOKIE,
        EMAIL_COOKIE,
        EID_COOKIE,
    ]
    .iter()
    .map(|name| {
        let mut cookie = format!(
            "{}=; Max-Age=0; Domain={}; Path={}; SameSite={}",
            name,
            config.cookie_domain,
            config.cookie_path,
            config.cookie_same_site.as_cookie_value()
        );
        if matches!(*name, ACCESS_TOKEN_COOKIE | REFRESH_TOKEN_COOKIE) {
            cookie.push_str("; HttpOnly");
        }
        if config.cookie_secure {
            cookie.push_str("; Secure");
        }
        ("set-cookie".to_string(), cookie)
    })
    .collect()
}

fn session_expired_response(config: &SpaCookieConfig) -> SpaAuthResponse {
    let timeout_uri = if config.cookie_timeout_uri.trim().is_empty() {
        "/"
    } else {
        config.cookie_timeout_uri.as_str()
    };
    SpaAuthResponse::json(
        401,
        json!({
            "code": "ERR10040",
            "message": "SPA session expired",
            "timeoutUri": timeout_uri,
            "authenticated": false,
        }),
        delete_cookie_headers(config),
    )
}

fn authorization_code_form(
    grant: &OAuthAuthorizationCodeConfig,
    code: &str,
    csrf: &str,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("csrf", csrf.to_string()),
    ];
    if let Some(redirect_uri) = grant
        .redirect_uri
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        form.push(("redirect_uri", redirect_uri.to_string()));
    }
    if let Some(scope) = join_scope(&grant.scope) {
        form.push(("scope", scope));
    }
    form
}

fn refresh_token_form(
    grant: &OAuthRefreshTokenConfig,
    refresh_token: &str,
    csrf: &str,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("csrf", csrf.to_string()),
    ];
    if let Some(scope) = join_scope(&grant.scope) {
        form.push(("scope", scope));
    }
    form
}

fn token_exchange_form(
    grant: &OAuthTokenExchangeConfig,
    subject_token: &str,
    subject_token_type: Option<&str>,
    csrf: &str,
) -> Vec<(&'static str, String)> {
    let subject_token_type = subject_token_type
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .or_else(|| grant.subject_token_type.clone())
        .unwrap_or_else(|| DEFAULT_SUBJECT_TOKEN_TYPE.to_string());
    let mut form = vec![
        ("grant_type", TOKEN_EXCHANGE_GRANT.to_string()),
        ("subject_token", subject_token.to_string()),
        ("subject_token_type", subject_token_type),
        ("csrf", csrf.to_string()),
    ];
    if let Some(requested) = grant
        .requested_token_type
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        form.push(("requested_token_type", requested.to_string()));
    }
    if let Some(audience) = grant
        .audience
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        form.push(("audience", audience.to_string()));
    }
    if let Some(scope) = join_scope(&grant.scope) {
        form.push(("scope", scope));
    }
    form
}

fn token_http_client(
    token: &OAuthTokenConfig,
    request: &ClientRequestConfig,
    tls: &ClientTlsConfig,
) -> Result<reqwest::Client, HandlerRejection> {
    ClientFactory::from_parts(request.clone(), tls.clone())
        .reqwest_client(EndpointOptions {
            proxy_host: token.proxy_host.clone(),
            proxy_port: token.proxy_port,
            enable_http2: Some(token.enable_http2),
            ..EndpointOptions::default()
        })
        .map_err(|error| {
            HandlerRejection::new(500, "ERR10056", format!("invalid token client: {error}"))
        })
}

async fn resolve_token_server_url(
    token: &OAuthTokenConfig,
    direct_registry: &DirectRegistryConfig,
    registry_client: Option<&PortalRegistryClient>,
) -> Result<String, HandlerRejection> {
    if let Some(server_url) = token
        .server_url
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        return Ok(server_url.to_string());
    }
    let service_id = token.service_id.as_deref().ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            "client.yml oauth.token.server_url or oauth.token.serviceId is required",
        )
    })?;
    if let Some(matched) = direct_registry_match(direct_registry, service_id, None) {
        return Ok(matched.url.trim().to_string());
    }
    let registry_client = registry_client.ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            "token serviceId discovery requires portal registry to be enabled",
        )
    })?;
    let snapshot = registry_client
        .lookup_discovery(DiscoverySubscription {
            service_id: service_id.to_string(),
            env_tag: None,
            protocol: None,
        })
        .await
        .map_err(|error| {
            HandlerRejection::new(
                502,
                "ERR10056",
                format!("failed to discover token service `{service_id}`: {error}"),
            )
        })?;
    let node = select_token_node(&snapshot.nodes).ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            format!("token service `{service_id}` has no usable discovery nodes"),
        )
    })?;
    Ok(discovery_node_base_url(node))
}

fn select_token_node(nodes: &[DiscoveryNode]) -> Option<&DiscoveryNode> {
    nodes
        .iter()
        .filter(|node| node.connected && node.port != 0)
        .find(|node| node.protocol.eq_ignore_ascii_case("https"))
        .or_else(|| {
            nodes
                .iter()
                .filter(|node| node.connected && node.port != 0)
                .find(|node| node.protocol.eq_ignore_ascii_case("http"))
        })
}

fn discovery_node_base_url(node: &DiscoveryNode) -> String {
    let host = if node.address.contains(':') && !node.address.starts_with('[') {
        format!("[{}]", node.address)
    } else {
        node.address.clone()
    };
    format!(
        "{}://{}:{}",
        node.protocol.to_ascii_lowercase(),
        host,
        node.port
    )
}

fn token_endpoint_url(server_url: &str, uri: &str) -> Result<String, HandlerRejection> {
    let server_url = server_url.trim().trim_end_matches('/');
    if server_url.is_empty() {
        return Err(HandlerRejection::new(
            502,
            "ERR10056",
            "token server_url is empty",
        ));
    }
    let uri = uri.trim();
    let uri = if uri.starts_with('/') {
        uri.to_string()
    } else {
        format!("/{uri}")
    };
    let url = format!("{server_url}{uri}");
    url::Url::parse(url.as_str()).map_err(|error| {
        HandlerRejection::new(
            502,
            "ERR10056",
            format!("invalid token endpoint URL `{url}`: {error}"),
        )
    })?;
    Ok(url)
}

fn basic_authorization(client_id: &str, client_secret: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{client_id}:{client_secret}"))
    )
}

fn token_needs_refresh(claims: &JsonValue, renew_before_seconds: u64) -> bool {
    let exp = claims.get("exp").and_then(JsonValue::as_u64).unwrap_or(0);
    exp.saturating_sub(renew_before_seconds) <= now_seconds()
}

fn access_token_max_age(claims: &JsonValue) -> Option<u64> {
    let exp = claims.get("exp")?.as_u64()?;
    Some(exp.saturating_sub(now_seconds()))
}

fn scopes_from_claims(claims: &JsonValue) -> Vec<String> {
    match claims.get("scp") {
        Some(JsonValue::Array(values)) => values
            .iter()
            .filter_map(JsonValue::as_str)
            .map(str::to_string)
            .collect(),
        Some(JsonValue::String(value)) => value.split_whitespace().map(str::to_string).collect(),
        _ => Vec::new(),
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

fn join_scope(scope: &[String]) -> Option<String> {
    let scope = scope
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!scope.is_empty()).then_some(scope)
}

fn error_message(body: &JsonValue) -> String {
    [
        "message",
        "description",
        "error_description",
        "error",
        "code",
    ]
    .into_iter()
    .find_map(|key| body.get(key).and_then(JsonValue::as_str))
    .unwrap_or("unknown token endpoint error")
    .to_string()
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn merge_extra_response_headers(
    target: &mut Vec<(String, String)>,
    headers: Vec<(String, String)>,
) {
    target.extend(headers);
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
    fn cookie_headers_include_java_compatible_names() {
        let config = SpaCookieConfig {
            cookie_domain: "localhost".to_string(),
            cookie_path: "/".to_string(),
            cookie_secure: true,
            session_timeout: 3600,
            remember_me_timeout: 604800,
            cookie_same_site: CookieSameSite::None,
            renew_before_seconds: 90,
            refresh_single_flight_wait_ms: 5000,
            refresh_single_flight_cache_ms: 3000,
            refresh_single_flight_max_entries: 10000,
            cookie_timeout_uri: "/login".to_string(),
        };
        let headers = delete_cookie_headers(&config);
        assert!(
            headers
                .iter()
                .any(|(_, value)| value.starts_with("accessToken="))
        );
        assert!(
            headers
                .iter()
                .any(|(_, value)| value.starts_with("refreshToken="))
        );
        assert!(
            headers
                .iter()
                .any(|(_, value)| value.contains("SameSite=None"))
        );
        assert!(headers.iter().any(|(_, value)| value.contains("Secure")));
    }

    #[test]
    fn cookie_headers_use_java_role_claim() {
        let config = SpaCookieConfig {
            cookie_domain: "localhost".to_string(),
            cookie_path: "/".to_string(),
            cookie_secure: true,
            session_timeout: 3600,
            remember_me_timeout: 604800,
            cookie_same_site: CookieSameSite::None,
            renew_before_seconds: 90,
            refresh_single_flight_wait_ms: 5000,
            refresh_single_flight_cache_ms: 3000,
            refresh_single_flight_max_entries: 10000,
            cookie_timeout_uri: "/login".to_string(),
        };
        let response = TokenGrantResponse {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_in: Some(60),
            remember: None,
            scope: None,
            extra: HashMap::new(),
        };
        let principal = AuthPrincipal {
            claims: json!({
                "uid": "steve",
                "role": "admin"
            }),
            ..AuthPrincipal::default()
        };

        let headers = session_cookie_headers(&config, &response, "csrf", &principal, 60);

        assert!(
            headers
                .iter()
                .any(|(_, value)| value.starts_with("roles=YWRtaW4="))
        );
    }

    #[test]
    fn token_exchange_form_prefers_handler_subject_token_type() {
        let grant = OAuthTokenExchangeConfig {
            subject_token_type: Some("from-client".to_string()),
            scope: vec!["a".to_string(), "b".to_string()],
            ..OAuthTokenExchangeConfig::default()
        };
        let form = token_exchange_form(&grant, "token", Some("from-handler"), "csrf")
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        assert_eq!(form["subject_token_type"], "from-handler");
        assert_eq!(form["scope"], "a b");
    }
}
