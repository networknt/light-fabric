use crate::config_util::{deserialize_string_list, request_header};
use crate::direct_registry::direct_registry_match;
use crate::security::HandlerRejection;
use crate::service::service_id_for_path;
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
pub use light_client::{
    AuthServerConfig, ClientConfig as ClientTokenConfig, ClientOauthConfig, ClientRequestConfig,
    ClientTlsConfig, OAuthClientCredentialsConfig, OAuthKeyConfig,
    OAuthTokenAuthorizationCodeConfig as OAuthAuthorizationCodeConfig, OAuthTokenCacheConfig,
    OAuthTokenConfig, OAuthTokenExchangeConfig,
    OAuthTokenRefreshTokenConfig as OAuthRefreshTokenConfig,
};
use light_client::{ClientFactory, EndpointOptions};
use light_runtime::{
    CLIENT_CONFIG_NAME, CLIENT_MODULE_ID, DirectRegistryConfig, DiscoveryNode,
    DiscoverySubscription, ModuleKind, PortalRegistryClient, RuntimeCache, RuntimeConfig,
    RuntimeError, client_config_masks,
};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

pub const TOKEN_FILE: &str = "token.yml";
pub const TOKEN_LEGACY_FILE: &str = "token.yaml";
pub const TOKEN_MODULE_ID: &str = "light-pingora/token";
pub const TOKEN_CONFIG_NAME: &str = "token";
pub const TOKEN_CACHE_NAME: &str = "light-pingora/token-cache";
pub const SIDECAR_FILE: &str = "sidecar.yml";
pub const SIDECAR_LEGACY_FILE: &str = "sidecar.yaml";
pub const SIDECAR_MODULE_ID: &str = "light-pingora/sidecar";
pub const SIDECAR_CONFIG_NAME: &str = "sidecar";
pub const CLIENT_FILE: &str = "client.yml";
pub const CLIENT_TOKEN_MODULE_ID: &str = CLIENT_MODULE_ID;
pub const CLIENT_TOKEN_CONFIG_NAME: &str = CLIENT_CONFIG_NAME;
pub const SCOPE_TOKEN_HEADER: &str = "X-Scope-Token";

const AUTHORIZATION_HEADER: &str = "authorization";
const SERVICE_ID_HEADER: &str = "service_id";
const SERVICE_URL_HEADER: &str = "service_url";
const SIDECAR_MODE_HEADER: &str = "header";
const SIDECAR_MODE_PROTOCOL: &str = "protocol";

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
pub struct SidecarTrafficConfig {
    #[serde(default = "default_sidecar_indicator")]
    pub egress_ingress_indicator: String,
}

impl Default for SidecarTrafficConfig {
    fn default() -> Self {
        Self {
            egress_ingress_indicator: default_sidecar_indicator(),
        }
    }
}

impl SidecarTrafficConfig {
    fn allows_token(
        &self,
        service_id: Option<&str>,
        service_url: Option<&str>,
        request_is_http: bool,
    ) -> bool {
        match self
            .egress_ingress_indicator
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            SIDECAR_MODE_HEADER => has_non_blank(service_id) || has_non_blank(service_url),
            SIDECAR_MODE_PROTOCOL => request_is_http,
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct TokenRuntime {
    handler: TokenHandlerConfig,
    sidecar: SidecarTrafficConfig,
    client: ClientTokenConfig,
    cache: Arc<TokenCache>,
    direct_registry: DirectRegistryConfig,
    registry_client: Option<Arc<PortalRegistryClient>>,
}

impl TokenRuntime {
    fn new(
        handler: TokenHandlerConfig,
        sidecar: SidecarTrafficConfig,
        client: ClientTokenConfig,
        direct_registry: DirectRegistryConfig,
        registry_client: Option<Arc<PortalRegistryClient>>,
    ) -> Self {
        let capacity = client.oauth.token.cache.capacity;
        Self {
            handler,
            sidecar,
            client,
            cache: Arc::new(TokenCache::new(capacity)),
            direct_registry,
            registry_client,
        }
    }

    pub fn handler_config(&self) -> &TokenHandlerConfig {
        &self.handler
    }

    pub fn client_config(&self) -> &ClientTokenConfig {
        &self.client
    }

    pub fn sidecar_config(&self) -> &SidecarTrafficConfig {
        &self.sidecar
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
        let entry = self.cache.get_or_insert_empty(key).await;
        let mut cached = entry.lock().await;
        let now = now_millis();

        if let Some(token) = cached.token.clone() {
            if !cached.needs_refresh(now, options.token_renew_before_expired) {
                return Ok(token);
            }
            if !cached.is_expired(now) {
                self.maybe_start_early_refresh(&mut cached, Arc::clone(&entry), options, now);
                return Ok(token);
            }
        }

        if now < cached.expired_retry_timeout {
            return Err(client_credentials_token_not_available());
        }

        cached.renewing = true;
        let fetched = fetch_client_credentials_token(
            &options,
            &self.client.request,
            &self.client.tls,
            &self.direct_registry,
            self.registry_client.as_deref(),
        )
        .await;
        match fetched {
            Ok(fetched) => {
                let token = fetched.access_token.clone();
                cached.update(fetched);
                Ok(token)
            }
            Err(error) => {
                cached.renewing = false;
                cached.expired_retry_timeout =
                    now_millis().saturating_add(options.expired_refresh_retry_delay);
                Err(error)
            }
        }
    }

    fn maybe_start_early_refresh(
        &self,
        cached: &mut CachedToken,
        entry: Arc<Mutex<CachedToken>>,
        options: TokenRequestOptions,
        now: u64,
    ) {
        if cached.renewing || now < cached.early_retry_timeout {
            return;
        }

        cached.renewing = true;
        cached.early_retry_timeout = now.saturating_add(options.early_refresh_retry_delay);
        let request = self.client.request.clone();
        let tls = self.client.tls.clone();
        let direct_registry = self.direct_registry.clone();
        let registry_client = self.registry_client.clone();
        tokio::spawn(async move {
            let fetched = fetch_client_credentials_token(
                &options,
                &request,
                &tls,
                &direct_registry,
                registry_client.as_deref(),
            )
            .await;
            let mut cached = entry.lock().await;
            match fetched {
                Ok(fetched) => cached.update(fetched),
                Err(_) => {
                    cached.renewing = false;
                }
            }
        });
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

    let sidecar = match runtime_config
        .module_registry
        .load_config::<SidecarTrafficConfig>(runtime_config, SIDECAR_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == SIDECAR_FILE => match runtime_config
            .module_registry
            .load_config::<SidecarTrafficConfig>(runtime_config, SIDECAR_LEGACY_FILE)
        {
            Ok(config) => config,
            Err(RuntimeError::MissingConfig(file)) if file == SIDECAR_LEGACY_FILE => {
                SidecarTrafficConfig::default()
            }
            Err(error) => return Err(error),
        },
        Err(error) => return Err(error),
    };
    runtime_config.module_registry.register_loaded_config(
        SIDECAR_MODULE_ID,
        SIDECAR_CONFIG_NAME,
        ModuleKind::Framework,
        &sidecar,
        [],
        true,
        Some(true),
        true,
    )?;

    let client = load_client_config(runtime_config)?;

    let runtime = TokenRuntime::new(
        handler,
        sidecar,
        client,
        runtime_config.direct_registry.clone(),
        runtime_config.registry_client.clone(),
    );
    if let Some(cache_registry) = runtime_config.cache_registry.as_ref() {
        let cache: Arc<dyn RuntimeCache> = runtime.cache();
        cache_registry.register_arc(TOKEN_CACHE_NAME, cache);
    }
    Ok(Some(runtime))
}

pub(crate) fn load_client_config(
    runtime_config: &RuntimeConfig,
) -> Result<ClientTokenConfig, RuntimeError> {
    if let Some(client) = runtime_config.client.as_ref() {
        return Ok(client.clone());
    }

    let client = runtime_config
        .module_registry
        .load_config::<ClientTokenConfig>(runtime_config, CLIENT_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        CLIENT_TOKEN_MODULE_ID,
        CLIENT_TOKEN_CONFIG_NAME,
        ModuleKind::Core,
        &client,
        client_config_masks(),
        true,
        Some(true),
        true,
    )?;
    Ok(client)
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
    let service_url = request_header(session, SERVICE_URL_HEADER);
    if !runtime.sidecar.allows_token(
        service_id.as_deref(),
        service_url.as_deref(),
        session_request_is_http(session),
    ) {
        return Ok(());
    }
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

#[derive(Debug)]
struct CachedToken {
    token: Option<String>,
    expires_at_millis: u64,
    scope: Option<String>,
    renewing: bool,
    expired_retry_timeout: u64,
    early_retry_timeout: u64,
}

impl CachedToken {
    fn empty() -> Self {
        Self {
            token: None,
            expires_at_millis: 0,
            scope: None,
            renewing: false,
            expired_retry_timeout: 0,
            early_retry_timeout: 0,
        }
    }

    fn update(&mut self, fetched: FetchedToken) {
        self.token = Some(fetched.access_token);
        self.expires_at_millis = fetched.expires_at_millis;
        self.scope = fetched.scope;
        self.renewing = false;
        self.expired_retry_timeout = 0;
    }

    fn needs_refresh(&self, now_millis: u64, renew_before_millis: u64) -> bool {
        if self.token.is_none() {
            return true;
        }
        self.expires_at_millis.saturating_sub(renew_before_millis) <= now_millis
    }

    fn is_expired(&self, now_millis: u64) -> bool {
        self.token.is_none() || self.expires_at_millis <= now_millis
    }
}

pub struct TokenCache {
    capacity: usize,
    entries: Mutex<BTreeMap<TokenCacheKey, Arc<Mutex<CachedToken>>>>,
}

impl TokenCache {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            entries: tokio::sync::Mutex::new(BTreeMap::new()),
        }
    }

    async fn get_or_insert_empty(&self, key: TokenCacheKey) -> Arc<Mutex<CachedToken>> {
        if self.capacity == 0 {
            return Arc::new(Mutex::new(CachedToken::empty()));
        }

        let mut entries = self.entries.lock().await;
        if let Some(entry) = entries.get(&key) {
            return Arc::clone(entry);
        }

        if entries.len() >= self.capacity
            && let Some(evict_key) = evict_key(&entries)
        {
            entries.remove(&evict_key);
        }
        let entry = Arc::new(Mutex::new(CachedToken::empty()));
        entries.insert(key, Arc::clone(&entry));
        entry
    }

    #[cfg(test)]
    async fn insert(&self, key: TokenCacheKey, value: CachedToken) {
        if self.capacity == 0 {
            return;
        }
        let mut entries = self.entries.lock().await;
        if !entries.contains_key(&key)
            && entries.len() >= self.capacity
            && let Some(evict_key) = evict_key(&entries)
        {
            entries.remove(&evict_key);
        }
        entries.insert(key, Arc::new(Mutex::new(value)));
    }
}

#[async_trait]
impl RuntimeCache for TokenCache {
    async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    async fn entries_summary(&self) -> JsonValue {
        let entries = self
            .entries
            .lock()
            .await
            .iter()
            .map(|(key, token)| (key.clone(), Arc::clone(token)))
            .collect::<Vec<_>>();
        let mut summary = JsonMap::new();
        for (key, token) in entries {
            let token = token.lock().await;
            summary.insert(
                cache_key_to_string(&key),
                json!({
                    "expiresAtMillis": token.expires_at_millis,
                    "scope": token.scope,
                    "renewing": token.renewing,
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
    server_url: Option<String>,
    token_service_id: Option<String>,
    uri: String,
    client_id: String,
    client_secret: String,
    scope: Vec<String>,
    proxy_host: Option<String>,
    proxy_port: Option<u16>,
    enable_http2: bool,
    token_renew_before_expired: u64,
    expired_refresh_retry_delay: u64,
    early_refresh_retry_delay: u64,
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
    direct_registry: &DirectRegistryConfig,
    registry_client: Option<&PortalRegistryClient>,
) -> Result<FetchedToken, HandlerRejection> {
    let server_url = resolve_token_server_url(options, direct_registry, registry_client).await?;
    let url = token_endpoint_url(server_url.as_str(), options.uri.as_str())?;
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
        .and_then(non_empty);
    let token_service_id = auth_server
        .and_then(|auth| auth.service_id.clone())
        .or_else(|| token.service_id.clone())
        .and_then(non_empty);
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
        token_service_id,
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
        expired_refresh_retry_delay: auth_server
            .and_then(|auth| auth.expired_refresh_retry_delay)
            .unwrap_or(token.expired_refresh_retry_delay),
        early_refresh_retry_delay: auth_server
            .and_then(|auth| auth.early_refresh_retry_delay)
            .unwrap_or(token.early_refresh_retry_delay),
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
    ClientFactory::from_parts(request.clone(), tls.clone())
        .reqwest_client(EndpointOptions {
            proxy_host: options.proxy_host.clone(),
            proxy_port: options.proxy_port,
            enable_http2: Some(options.enable_http2),
            ..EndpointOptions::default()
        })
        .map_err(|error| {
            HandlerRejection::new(500, "ERR10056", format!("invalid token client: {error}"))
        })
}

async fn resolve_token_server_url(
    options: &TokenRequestOptions,
    direct_registry: &DirectRegistryConfig,
    registry_client: Option<&PortalRegistryClient>,
) -> Result<String, HandlerRejection> {
    if let Some(server_url) = options.server_url.as_ref() {
        return Ok(server_url.clone());
    }
    let service_id = options.token_service_id.as_deref().ok_or_else(|| {
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

fn session_request_is_http(session: &Session) -> bool {
    session
        .as_downstream()
        .digest()
        .and_then(|digest| digest.ssl_digest.as_ref())
        .is_none()
}

fn has_non_blank(value: Option<&str>) -> bool {
    value.is_some_and(|value| !value.trim().is_empty())
}

fn evict_key(entries: &BTreeMap<TokenCacheKey, Arc<Mutex<CachedToken>>>) -> Option<TokenCacheKey> {
    entries
        .iter()
        .filter_map(|(key, entry)| {
            entry
                .try_lock()
                .ok()
                .map(|token| (token.expires_at_millis, key.clone()))
        })
        .min_by_key(|(expires_at_millis, _)| *expires_at_millis)
        .map(|(_, key)| key)
        .or_else(|| entries.keys().next().cloned())
}

fn client_credentials_token_not_available() -> HandlerRejection {
    HandlerRejection::new(
        408,
        "ERR10009",
        "Could not get client credentials token in client module",
    )
}

fn default_sidecar_indicator() -> String {
    SIDECAR_MODE_HEADER.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration as TokioDuration, sleep};

    #[test]
    fn client_token_config_accepts_single_and_multiple_auth_server_shapes() {
        let config: ClientTokenConfig = serde_yaml::from_str(
            r#"
tls:
  verifyHostname: false
  caCertPath: config/ca.pem
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
    key:
      server_url: https://oauth.example.com
      uri: /oauth2/keys
      client_id: key-client
      client_secret: key-secret
      enableHttp2: false
pathPrefixServices: '{"/v1/pets": "petstore"}'
request:
  connectTimeout: 200
  timeout: 300
"#,
        )
        .expect("parse client token config");

        assert!(!config.tls.verify_hostname);
        assert_eq!(
            config.tls.ca_cert_path.as_deref(),
            Some(std::path::Path::new("config/ca.pem"))
        );
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
        assert_eq!(
            config.oauth.token.key.server_url.as_deref(),
            Some("https://oauth.example.com")
        );
        assert_eq!(config.oauth.token.key.uri, "/oauth2/keys");
        assert!(!config.oauth.token.key.enable_http2);
    }

    #[test]
    fn sidecar_config_defaults_to_header_mode_and_supports_protocol_mode() {
        let config = SidecarTrafficConfig::default();

        assert!(config.allows_token(Some("service"), None, false));
        assert!(config.allows_token(None, Some("http://api.example.com"), false));
        assert!(!config.allows_token(None, None, false));

        let config: SidecarTrafficConfig = serde_yaml::from_str(
            r#"
egressIngressIndicator: protocol
"#,
        )
        .expect("parse sidecar config");

        assert!(config.allows_token(None, None, true));
        assert!(!config.allows_token(Some("service"), None, false));
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
                    token: Some("secret-a".to_string()),
                    expires_at_millis: 200,
                    scope: None,
                    renewing: false,
                    expired_retry_timeout: 0,
                    early_retry_timeout: 0,
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
                    token: Some("secret-b".to_string()),
                    expires_at_millis: 300,
                    scope: Some("pet.r".to_string()),
                    renewing: false,
                    expired_retry_timeout: 0,
                    early_retry_timeout: 0,
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

    #[tokio::test]
    async fn expired_token_refresh_is_synchronized_for_same_cache_key() {
        let server = MockTokenServer::start(TokioDuration::from_millis(100)).await;
        let runtime = Arc::new(test_runtime(server.url("/oauth2/token"), 60_000));
        let mut tasks = Vec::new();

        for _ in 0..8 {
            let runtime = Arc::clone(&runtime);
            tasks.push(tokio::spawn(async move {
                runtime
                    .get_client_credentials_token("/v1/pets", Some("petstore".to_string()))
                    .await
            }));
        }

        for task in tasks {
            assert_eq!(task.await.expect("task").expect("token"), "token-1");
        }
        sleep(TokioDuration::from_millis(50)).await;
        assert_eq!(server.request_count(), 1);
        server.abort();
    }

    #[tokio::test]
    async fn early_refresh_returns_current_token_and_runs_once_in_background() {
        let server = MockTokenServer::start(TokioDuration::from_millis(25)).await;
        let runtime = Arc::new(test_runtime(server.url("/oauth2/token"), 60_000));
        let key = TokenCacheKey::new(Some("petstore".to_string()), vec!["pet.r".to_string()]);
        runtime
            .cache
            .insert(
                key.clone(),
                CachedToken {
                    token: Some("old-token".to_string()),
                    expires_at_millis: now_millis().saturating_add(30_000),
                    scope: Some("pet.r".to_string()),
                    renewing: false,
                    expired_retry_timeout: 0,
                    early_retry_timeout: 0,
                },
            )
            .await;

        let mut tasks = Vec::new();
        for _ in 0..5 {
            let runtime = Arc::clone(&runtime);
            tasks.push(tokio::spawn(async move {
                runtime
                    .get_client_credentials_token("/v1/pets", Some("petstore".to_string()))
                    .await
            }));
        }

        for task in tasks {
            assert_eq!(task.await.expect("task").expect("token"), "old-token");
        }

        let entry = runtime.cache.get_or_insert_empty(key).await;
        for _ in 0..20 {
            if server.request_count() == 1 && entry.lock().await.token.as_deref() == Some("token-1")
            {
                break;
            }
            sleep(TokioDuration::from_millis(20)).await;
        }
        assert_eq!(server.request_count(), 1);
        let cached = entry.lock().await;
        assert_eq!(cached.token.as_deref(), Some("token-1"));
        assert!(!cached.renewing);
        server.abort();
    }

    #[tokio::test]
    async fn token_service_id_uses_direct_registry_without_portal_registry() {
        let options = TokenRequestOptions {
            server_url: None,
            token_service_id: Some("com.networknt.light-oauth-1.0.0".to_string()),
            uri: "/oauth2/token".to_string(),
            client_id: "client".to_string(),
            client_secret: "secret".to_string(),
            scope: vec!["portal.r".to_string()],
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
            token_renew_before_expired: 60_000,
            expired_refresh_retry_delay: 1_000,
            early_refresh_retry_delay: 1_000,
            cache_service_id: None,
        };
        let direct_registry = DirectRegistryConfig {
            direct_urls: BTreeMap::from([(
                "com.networknt.light-oauth-1.0.0".to_string(),
                "https://light-oauth:6881".to_string(),
            )]),
        };

        let server_url = resolve_token_server_url(&options, &direct_registry, None)
            .await
            .expect("server url");

        assert_eq!(server_url, "https://light-oauth:6881");
    }

    fn test_runtime(server_url: String, token_renew_before_expired: u64) -> TokenRuntime {
        TokenRuntime::new(
            TokenHandlerConfig {
                enabled: true,
                applied_path_prefixes: vec!["/".to_string()],
            },
            SidecarTrafficConfig::default(),
            ClientTokenConfig {
                oauth: ClientOauthConfig {
                    multiple_auth_servers: false,
                    token: OAuthTokenConfig {
                        server_url: Some(server_url),
                        token_renew_before_expired,
                        client_credentials: OAuthClientCredentialsConfig {
                            uri: "/oauth2/token".to_string(),
                            client_id: "client".to_string(),
                            client_secret: "secret".to_string(),
                            scope: vec!["pet.r".to_string()],
                            service_id_auth_servers: BTreeMap::new(),
                        },
                        ..OAuthTokenConfig::default()
                    },
                    ..ClientOauthConfig::default()
                },
                ..ClientTokenConfig::default()
            },
            DirectRegistryConfig::default(),
            None,
        )
    }

    struct MockTokenServer {
        address: std::net::SocketAddr,
        count: Arc<AtomicUsize>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockTokenServer {
        async fn start(delay: TokioDuration) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind token server");
            let address = listener.local_addr().expect("token server address");
            let count = Arc::new(AtomicUsize::new(0));
            let handle_count = Arc::clone(&count);
            let handle = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let count = Arc::clone(&handle_count);
                    tokio::spawn(async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        let mut buffer = [0u8; 2048];
                        let _ = socket.read(&mut buffer).await;
                        sleep(delay).await;
                        let token_number = count.load(Ordering::SeqCst);
                        let body = format!(
                            r#"{{"access_token":"token-{token_number}","expires_in":300,"scope":"pet.r"}}"#
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self {
                address,
                count,
                handle,
            }
        }

        fn url(&self, _path: &str) -> String {
            format!("http://{}", self.address)
        }

        fn request_count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }

        fn abort(self) {
            self.handle.abort();
        }
    }
}
