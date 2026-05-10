use crate::config_util::{
    deserialize_optional_u16, deserialize_optional_u64, deserialize_string_list,
    deserialize_string_map, deserialize_typed_map, request_header,
};
use crate::security::HandlerRejection;
use crate::service::service_id_for_path;
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use light_runtime::{MaskSpec, ModuleKind, RuntimeCache, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const TOKEN_FILE: &str = "token.yml";
pub const TOKEN_LEGACY_FILE: &str = "token.yaml";
pub const TOKEN_MODULE_ID: &str = "light-pingora/token";
pub const TOKEN_CONFIG_NAME: &str = "token";
pub const TOKEN_CACHE_NAME: &str = "light-pingora/token-cache";
pub const CLIENT_FILE: &str = "client.yml";
pub const CLIENT_TOKEN_MODULE_ID: &str = "light-pingora/client-token";
pub const CLIENT_TOKEN_CONFIG_NAME: &str = "client-token";
pub const SCOPE_TOKEN_HEADER: &str = "X-Scope-Token";

const AUTHORIZATION_HEADER: &str = "authorization";
const SERVICE_ID_HEADER: &str = "service_id";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TokenHandlerConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub applied_path_prefixes: Vec<String>,
}

impl Default for TokenHandlerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            applied_path_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientTokenConfig {
    #[serde(default)]
    pub tls: ClientTlsConfig,
    #[serde(default)]
    pub oauth: ClientOauthConfig,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub path_prefix_services: BTreeMap<String, String>,
    #[serde(default)]
    pub request: ClientRequestConfig,
}

impl Default for ClientTokenConfig {
    fn default() -> Self {
        Self {
            tls: ClientTlsConfig::default(),
            oauth: ClientOauthConfig::default(),
            path_prefix_services: BTreeMap::new(),
            request: ClientRequestConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientTlsConfig {
    #[serde(default = "default_true")]
    pub verify_hostname: bool,
}

impl Default for ClientTlsConfig {
    fn default() -> Self {
        Self {
            verify_hostname: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClientRequestConfig {
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub inject_caller_id: bool,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
}

impl Default for ClientRequestConfig {
    fn default() -> Self {
        Self {
            connect_timeout: default_connect_timeout(),
            timeout: default_timeout(),
            inject_caller_id: false,
            enable_http2: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ClientOauthConfig {
    #[serde(default)]
    pub multiple_auth_servers: bool,
    #[serde(default)]
    pub token: OAuthTokenConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenConfig {
    #[serde(default)]
    pub cache: OAuthTokenCacheConfig,
    #[serde(default = "default_token_renew_before_expired")]
    pub token_renew_before_expired: u64,
    #[serde(default = "default_expired_refresh_retry_delay")]
    pub expired_refresh_retry_delay: u64,
    #[serde(default = "default_early_refresh_retry_delay")]
    pub early_refresh_retry_delay: u64,
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default, rename = "client_credentials", alias = "clientCredentials")]
    pub client_credentials: OAuthClientCredentialsConfig,
}

impl Default for OAuthTokenConfig {
    fn default() -> Self {
        Self {
            cache: OAuthTokenCacheConfig::default(),
            token_renew_before_expired: default_token_renew_before_expired(),
            expired_refresh_retry_delay: default_expired_refresh_retry_delay(),
            early_refresh_retry_delay: default_early_refresh_retry_delay(),
            server_url: None,
            service_id: None,
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
            client_credentials: OAuthClientCredentialsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenCacheConfig {
    #[serde(default = "default_cache_capacity")]
    pub capacity: usize,
}

impl Default for OAuthTokenCacheConfig {
    fn default() -> Self {
        Self {
            capacity: default_cache_capacity(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OAuthClientCredentialsConfig {
    #[serde(default = "default_token_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub service_id_auth_servers: BTreeMap<String, AuthServerConfig>,
}

impl Default for OAuthClientCredentialsConfig {
    fn default() -> Self {
        Self {
            uri: default_token_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: Vec::new(),
            service_id_auth_servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuthServerConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: Option<String>,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default)]
    pub enable_http2: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub token_renew_before_expired: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub expired_refresh_retry_delay: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub early_refresh_retry_delay: Option<u64>,
}

#[derive(Clone)]
pub struct TokenRuntime {
    handler: TokenHandlerConfig,
    client: ClientTokenConfig,
    cache: Arc<TokenCache>,
}

impl TokenRuntime {
    fn new(handler: TokenHandlerConfig, client: ClientTokenConfig) -> Self {
        let capacity = client.oauth.token.cache.capacity;
        Self {
            handler,
            client,
            cache: Arc::new(TokenCache::new(capacity)),
        }
    }

    pub fn handler_config(&self) -> &TokenHandlerConfig {
        &self.handler
    }

    pub fn client_config(&self) -> &ClientTokenConfig {
        &self.client
    }

    pub fn cache(&self) -> Arc<TokenCache> {
        Arc::clone(&self.cache)
    }

    async fn get_client_credentials_token(
        &self,
        request_path: &str,
        service_id: Option<String>,
    ) -> Result<String, HandlerRejection> {
        let options = self.resolve_request_options(request_path, service_id)?;
        let key = TokenCacheKey::new(options.cache_service_id.clone(), options.scope.clone());
        let now = now_millis();
        if let Some(cached) = self.cache.get(&key).await
            && !cached.needs_refresh(now, options.token_renew_before_expired)
        {
            return Ok(cached.token);
        }

        let fetched =
            fetch_client_credentials_token(&options, &self.client.request, &self.client.tls)
                .await?;
        let cached = CachedToken {
            token: fetched.access_token.clone(),
            expires_at_millis: fetched.expires_at_millis,
            scope: fetched.scope,
        };
        self.cache.insert(key, cached).await;
        Ok(fetched.access_token)
    }

    fn resolve_request_options(
        &self,
        request_path: &str,
        service_id: Option<String>,
    ) -> Result<TokenRequestOptions, HandlerRejection> {
        let token = &self.client.oauth.token;
        let base = &token.client_credentials;
        let service_id = service_id
            .filter(|value| !value.trim().is_empty())
            .or_else(|| service_id_for_path(&self.client.path_prefix_services, request_path));

        if self.client.oauth.multiple_auth_servers {
            let service_id = service_id.ok_or_else(|| {
                HandlerRejection::new(
                    500,
                    "ERR10074",
                    "client.yml multipleAuthServers requires service_id or pathPrefixServices",
                )
            })?;
            let auth_server = base
                .service_id_auth_servers
                .get(service_id.as_str())
                .ok_or_else(|| {
                    HandlerRejection::new(
                        500,
                        "ERR10075",
                        format!(
                            "client.yml client_credentials.serviceIdAuthServers is missing `{service_id}`"
                        ),
                    )
                })?;
            return merged_options(token, base, Some(auth_server), Some(service_id));
        }

        let service_id = service_id.or_else(|| token.service_id.clone());
        merged_options(token, base, None, service_id)
    }
}

pub fn load_token_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<TokenRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let handler = match runtime_config
        .module_registry
        .load_config::<TokenHandlerConfig>(runtime_config, TOKEN_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == TOKEN_FILE => match runtime_config
            .module_registry
            .load_config::<TokenHandlerConfig>(runtime_config, TOKEN_LEGACY_FILE)
        {
            Ok(config) => config,
            Err(RuntimeError::MissingConfig(file)) if file == TOKEN_LEGACY_FILE => {
                TokenHandlerConfig::default()
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        TOKEN_MODULE_ID,
        TOKEN_CONFIG_NAME,
        ModuleKind::Framework,
        &handler,
        [],
        handler.enabled,
        Some(handler.enabled),
        true,
    )?;

    if !handler.enabled {
        return Ok(None);
    }

    let client = runtime_config
        .module_registry
        .load_config::<ClientTokenConfig>(runtime_config, CLIENT_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        CLIENT_TOKEN_MODULE_ID,
        CLIENT_TOKEN_CONFIG_NAME,
        ModuleKind::Framework,
        &client,
        [
            MaskSpec::key("client_secret"),
            MaskSpec::key("clientSecret"),
            MaskSpec::key("trustStorePass"),
            MaskSpec::key("keyStorePass"),
            MaskSpec::key("keyPass"),
        ],
        true,
        Some(true),
        true,
    )?;

    let runtime = TokenRuntime::new(handler, client);
    if let Some(cache_registry) = runtime_config.cache_registry.as_ref() {
        let cache: Arc<dyn RuntimeCache> = runtime.cache();
        cache_registry.register_arc(TOKEN_CACHE_NAME, cache);
    }
    Ok(Some(runtime))
}

pub async fn apply_token_request(
    session: &mut Session,
    runtime: &TokenRuntime,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    if !runtime.handler.enabled || !applies_to_path(&runtime.handler, request_path) {
        return Ok(());
    }

    let service_id = request_header(session, SERVICE_ID_HEADER);
    let token = runtime
        .get_client_credentials_token(request_path, service_id)
        .await?;
    let token = bearer_token(token.as_str());
    let target_header = if request_header(session, AUTHORIZATION_HEADER).is_some() {
        SCOPE_TOKEN_HEADER
    } else {
        AUTHORIZATION_HEADER
    };

    session
        .req_header_mut()
        .insert_header(target_header, token)
        .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid token header"))?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCacheKey {
    pub service_id: Option<String>,
    pub scope: Option<String>,
}

impl TokenCacheKey {
    fn new(service_id: Option<String>, scope: Vec<String>) -> Self {
        Self {
            service_id: service_id.and_then(non_empty),
            scope: join_scope(&scope).and_then(non_empty),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at_millis: u64,
    scope: Option<String>,
}

impl CachedToken {
    fn needs_refresh(&self, now_millis: u64, renew_before_millis: u64) -> bool {
        self.expires_at_millis.saturating_sub(renew_before_millis) <= now_millis
    }
}

pub struct TokenCache {
    capacity: usize,
    entries: tokio::sync::Mutex<BTreeMap<TokenCacheKey, CachedToken>>,
}

impl TokenCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    async fn get(&self, key: &TokenCacheKey) -> Option<CachedToken> {
        self.entries.lock().await.get(key).cloned()
    }

    async fn insert(&self, key: TokenCacheKey, value: CachedToken) {
        if self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.lock().await;
        if !entries.contains_key(&key)
            && entries.len() >= self.capacity
            && let Some(evict_key) = entries
                .iter()
                .min_by_key(|(_, cached)| cached.expires_at_millis)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&evict_key);
        }
        entries.insert(key, value);
    }
}

#[async_trait]
impl RuntimeCache for TokenCache {
    async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    async fn entries_summary(&self) -> JsonValue {
        let entries = self.entries.lock().await;
        let mut summary = JsonMap::new();
        for (key, token) in entries.iter() {
            summary.insert(
                cache_key_to_string(key),
                json!({
                    "expiresAtMillis": token.expires_at_millis,
                    "scope": token.scope,
                }),
            );
        }
        JsonValue::Object(summary)
    }

    async fn clear(&self) {
        self.entries.lock().await.clear();
    }
}

#[derive(Debug, Clone)]
struct TokenRequestOptions {
    server_url: String,
    uri: String,
    client_id: String,
    client_secret: String,
    scope: Vec<String>,
    proxy_host: Option<String>,
    proxy_port: Option<u16>,
    enable_http2: bool,
    token_renew_before_expired: u64,
    cache_service_id: Option<String>,
}

#[derive(Debug, Clone)]
struct FetchedToken {
    access_token: String,
    expires_at_millis: u64,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(flatten)]
    extra: HashMap<String, JsonValue>,
}

async fn fetch_client_credentials_token(
    options: &TokenRequestOptions,
    request: &ClientRequestConfig,
    tls: &ClientTlsConfig,
) -> Result<FetchedToken, HandlerRejection> {
    let url = token_endpoint_url(options.server_url.as_str(), options.uri.as_str())?;
    let client = token_http_client(options, request, tls)?;
    let response = client
        .post(url)
        .header("content-type", "application/x-www-form-urlencoded")
        .header("accept", "application/json")
        .header(
            "authorization",
            basic_authorization(options.client_id.as_str(), options.client_secret.as_str()),
        )
        .form(&client_credentials_form(&options.scope))
        .send()
        .await
        .map_err(|error| {
            HandlerRejection::new(
                502,
                "ERR10052",
                format!("failed to request client credentials token: {error}"),
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

    let token_response = serde_json::from_value::<TokenResponse>(body).map_err(|error| {
        HandlerRejection::new(
            502,
            "ERR10052",
            format!("invalid token response shape: {error}"),
        )
    })?;
    let access_token = token_response.access_token.ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10052",
            "token response does not contain access_token",
        )
    })?;
    let expires_at_millis =
        token_expires_at_millis(access_token.as_str(), token_response.expires_in)?;
    let scope = token_response
        .scope
        .or_else(|| string_extra(&token_response.extra, "scope"));

    Ok(FetchedToken {
        access_token,
        expires_at_millis,
        scope,
    })
}

fn merged_options(
    token: &OAuthTokenConfig,
    base: &OAuthClientCredentialsConfig,
    auth_server: Option<&AuthServerConfig>,
    cache_service_id: Option<String>,
) -> Result<TokenRequestOptions, HandlerRejection> {
    let server_url = auth_server
        .and_then(|auth| auth.server_url.clone())
        .or_else(|| token.server_url.clone())
        .and_then(non_empty)
        .ok_or_else(|| {
            HandlerRejection::new(
                502,
                "ERR10056",
                "client.yml oauth.token.server_url is required until discovery is available",
            )
        })?;
    let uri = auth_server
        .and_then(|auth| auth.uri.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| base.uri.clone());
    let client_id = auth_server
        .and_then(|auth| auth.client_id.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| base.client_id.clone());
    let client_secret = auth_server
        .and_then(|auth| auth.client_secret.clone())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| base.client_secret.clone());
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(HandlerRejection::new(
            500,
            "ERR10074",
            "client.yml client_credentials client_id and client_secret are required",
        ));
    }
    let scope = auth_server
        .filter(|auth| !auth.scope.is_empty())
        .map(|auth| auth.scope.clone())
        .unwrap_or_else(|| base.scope.clone());

    Ok(TokenRequestOptions {
        server_url,
        uri,
        client_id,
        client_secret,
        scope,
        proxy_host: auth_server
            .and_then(|auth| auth.proxy_host.clone())
            .or_else(|| token.proxy_host.clone())
            .and_then(non_empty),
        proxy_port: auth_server
            .and_then(|auth| auth.proxy_port)
            .or(token.proxy_port),
        enable_http2: auth_server
            .and_then(|auth| auth.enable_http2)
            .unwrap_or(token.enable_http2),
        token_renew_before_expired: auth_server
            .and_then(|auth| auth.token_renew_before_expired)
            .unwrap_or(token.token_renew_before_expired),
        cache_service_id: cache_service_id.and_then(non_empty),
    })
}

fn applies_to_path(config: &TokenHandlerConfig, request_path: &str) -> bool {
    config
        .applied_path_prefixes
        .iter()
        .any(|prefix| path_matches_prefix(request_path, prefix))
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    let path = normalize_path(path);
    let prefix = normalize_prefix(prefix);
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn normalize_prefix(prefix: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix == "/" {
        return "/".to_string();
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        format!("/{prefix}")
    }
}

fn token_http_client(
    options: &TokenRequestOptions,
    request: &ClientRequestConfig,
    tls: &ClientTlsConfig,
) -> Result<reqwest::Client, HandlerRejection> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(request.connect_timeout))
        .timeout(Duration::from_millis(request.timeout));
    if !tls.verify_hostname {
        builder = builder.danger_accept_invalid_hostnames(true);
    }
    if !options.enable_http2 {
        builder = builder.http1_only();
    }
    if let Some(proxy_host) = options.proxy_host.as_deref() {
        let proxy_url = format!(
            "http://{}:{}",
            proxy_host,
            options.proxy_port.unwrap_or(443)
        );
        let proxy = reqwest::Proxy::all(proxy_url.as_str()).map_err(|error| {
            HandlerRejection::new(500, "ERR10056", format!("invalid token proxy: {error}"))
        })?;
        builder = builder.proxy(proxy);
    }
    builder.build().map_err(|error| {
        HandlerRejection::new(500, "ERR10056", format!("invalid token client: {error}"))
    })
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

fn client_credentials_form(scope: &[String]) -> Vec<(&'static str, String)> {
    let mut form = vec![("grant_type", "client_credentials".to_string())];
    if let Some(scope) = join_scope(scope) {
        form.push(("scope", scope));
    }
    form
}

fn basic_authorization(client_id: &str, client_secret: &str) -> String {
    format!(
        "Basic {}",
        STANDARD.encode(format!("{client_id}:{client_secret}"))
    )
}

fn bearer_token(token: &str) -> String {
    if token
        .get(..7)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer "))
    {
        token.to_string()
    } else {
        format!("Bearer {token}")
    }
}

fn token_expires_at_millis(token: &str, expires_in: Option<u64>) -> Result<u64, HandlerRejection> {
    if let Some(exp) = jwt_exp_millis(token) {
        return Ok(exp);
    }
    if let Some(expires_in) = expires_in {
        return Ok(now_millis().saturating_add(expires_in.saturating_mul(1000)));
    }
    Err(HandlerRejection::new(
        502,
        "ERR10052",
        "token response must contain a JWT exp claim or expires_in",
    ))
}

fn jwt_exp_millis(token: &str) -> Option<u64> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| URL_SAFE.decode(payload))
        .or_else(|_| STANDARD.decode(payload))
        .ok()?;
    let value = serde_json::from_slice::<JsonValue>(&decoded).ok()?;
    value
        .get("exp")?
        .as_u64()
        .map(|exp| exp.saturating_mul(1000))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn join_scope(scope: &[String]) -> Option<String> {
    let scope = scope
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    non_empty(scope)
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn cache_key_to_string(key: &TokenCacheKey) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| "<unserializable-key>".to_string())
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

fn string_extra(extra: &HashMap<String, JsonValue>, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(JsonValue::as_str)
        .map(str::to_string)
}

fn default_true() -> bool {
    true
}

fn default_cache_capacity() -> usize {
    200
}

fn default_token_renew_before_expired() -> u64 {
    60_000
}

fn default_expired_refresh_retry_delay() -> u64 {
    2_000
}

fn default_early_refresh_retry_delay() -> u64 {
    4_000
}

fn default_connect_timeout() -> u64 {
    2_000
}

fn default_timeout() -> u64 {
    3_000
}

fn default_token_uri() -> String {
    "/oauth2/token".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_token_config_accepts_single_and_multiple_auth_server_shapes() {
        let config: ClientTokenConfig = serde_yaml::from_str(
            r#"
tls:
  verifyHostname: false
oauth:
  multipleAuthServers: true
  token:
    cache:
      capacity: 8
    tokenRenewBeforeExpired: 1000
    server_url: https://oauth.example.com
    client_credentials:
      uri: /oauth2/token
      client_id: default-client
      client_secret: default-secret
      scope: '[ "default.r" ]'
      serviceIdAuthServers: '{"petstore":{"server_url":"https://pet-oauth.example.com","client_id":"pet-client","client_secret":"pet-secret","scope":["pet.r"]}}'
pathPrefixServices: '{"/v1/pets": "petstore"}'
request:
  connectTimeout: 200
  timeout: 300
"#,
        )
        .expect("parse client token config");

        assert!(!config.tls.verify_hostname);
        assert!(config.oauth.multiple_auth_servers);
        assert_eq!(config.oauth.token.cache.capacity, 8);
        assert_eq!(
            config.path_prefix_services["/v1/pets"],
            "petstore".to_string()
        );
        assert_eq!(
            config
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers["petstore"]
                .client_id,
            Some("pet-client".to_string())
        );
    }

    #[test]
    fn token_handler_applies_only_to_boundary_prefixes() {
        let config = TokenHandlerConfig {
            enabled: true,
            applied_path_prefixes: vec!["/v1/address".to_string()],
        };

        assert!(applies_to_path(&config, "/v1/address/1"));
        assert!(!applies_to_path(&config, "/v1/address2"));
    }

    #[test]
    fn client_credentials_form_and_authorization_are_java_compatible() {
        let form = client_credentials_form(&["pet.r".to_string(), "pet.w".to_string()]);

        assert_eq!(
            form,
            vec![
                ("grant_type", "client_credentials".to_string()),
                ("scope", "pet.r pet.w".to_string())
            ]
        );
        assert_eq!(
            basic_authorization("client", "secret"),
            "Basic Y2xpZW50OnNlY3JldA=="
        );
    }

    #[test]
    fn jwt_exp_claim_is_used_for_cache_expiry() {
        let payload = URL_SAFE_NO_PAD.encode(r#"{"exp":12345}"#);
        let token = format!("header.{payload}.signature");

        assert_eq!(jwt_exp_millis(token.as_str()), Some(12_345_000));
    }

    #[tokio::test]
    async fn token_cache_masks_tokens_in_summary_and_evicts_shortest_expiry() {
        let cache = TokenCache::new(1);
        cache
            .insert(
                TokenCacheKey {
                    service_id: Some("a".to_string()),
                    scope: None,
                },
                CachedToken {
                    token: "secret-a".to_string(),
                    expires_at_millis: 200,
                    scope: None,
                },
            )
            .await;
        cache
            .insert(
                TokenCacheKey {
                    service_id: Some("b".to_string()),
                    scope: Some("pet.r".to_string()),
                },
                CachedToken {
                    token: "secret-b".to_string(),
                    expires_at_millis: 300,
                    scope: Some("pet.r".to_string()),
                },
            )
            .await;

        let summary = cache.entries_summary().await;
        let summary_text = summary.to_string();
        assert_eq!(cache.len().await, 1);
        assert!(summary_text.contains("expiresAtMillis"));
        assert!(summary_text.contains("pet.r"));
        assert!(!summary_text.contains("secret"));
    }
}
