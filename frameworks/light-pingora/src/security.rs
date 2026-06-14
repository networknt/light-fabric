use crate::config_util::{
    deserialize_string_list, deserialize_string_map, request_header, request_header as header_value,
};
use crate::direct_registry::direct_registry_match;
use crate::token::{
    CLIENT_FILE, ClientRequestConfig, ClientTlsConfig, ClientTokenConfig, load_client_config,
};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use light_client::{
    ClientFactory, EndpointOptions, OAuthProviderError, OAuthProviderResolver, ResolvedKeyProvider,
};
use light_runtime::{
    DirectRegistryConfig, DiscoveryNode, DiscoverySubscription, MaskSpec, ModuleKind,
    PortalRegistryClient, RuntimeConfig, RuntimeError,
};
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub const SECURITY_FILE: &str = "security.yml";
pub const SECURITY_MODULE_ID: &str = "light-pingora/security";
pub const SECURITY_CONFIG_NAME: &str = "security";

const AUTHORIZATION: &str = "authorization";
const SERVICE_ID_HEADER: &str = "service_id";
const WWW_AUTHENTICATE: &str = "www-authenticate";
const SCOPE_TOKEN: &str = "X-Scope-Token";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerRejection {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub headers: Vec<(String, String)>,
}

impl HandlerRejection {
    pub fn new(status: u16, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            headers: Vec::new(),
        }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(401, "ERR10002", message).with_header(WWW_AUTHENTICATE, "Bearer")
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "ERR10007", message)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuthPrincipal {
    pub client_id: Option<String>,
    pub user_id: Option<String>,
    pub issuer: Option<String>,
    pub email: Option<String>,
    pub host: Option<String>,
    pub role: Option<String>,
    pub claims: JsonValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityConfig {
    #[serde(default = "default_true")]
    pub enable_verify_jwt: bool,
    #[serde(default)]
    pub enable_verify_swt: bool,
    #[serde(default = "default_swt_client_id_header")]
    pub swt_client_id_header: String,
    #[serde(default = "default_swt_client_secret_header")]
    pub swt_client_secret_header: String,
    #[serde(default = "default_true")]
    pub enable_extract_scope_token: bool,
    #[serde(default = "default_true")]
    pub enable_verify_scope: bool,
    #[serde(default)]
    pub skip_verify_scope_without_spec: bool,
    #[serde(default)]
    pub ignore_jwt_expiry: bool,
    #[serde(default)]
    pub enable_h2c: bool,
    #[serde(default)]
    pub enable_mock_jwt: bool,
    #[serde(default)]
    pub enable_relaxed_key_validation: bool,
    #[serde(default)]
    pub jwt: SecurityJwtConfig,
    #[serde(default)]
    pub log_jwt_token: bool,
    #[serde(default)]
    pub log_client_user_scope: bool,
    #[serde(default = "default_true")]
    pub enable_jwt_cache: bool,
    #[serde(default = "default_jwt_cache_full_size")]
    pub jwt_cache_full_size: usize,
    #[serde(default)]
    pub bootstrap_from_key_service: bool,
    #[serde(default)]
    pub provider_id: String,
    #[serde(default)]
    pub issuer: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub audience: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub skip_path_prefixes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_map")]
    pub pass_through_claims: BTreeMap<String, String>,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            enable_verify_jwt: true,
            enable_verify_swt: false,
            swt_client_id_header: default_swt_client_id_header(),
            swt_client_secret_header: default_swt_client_secret_header(),
            enable_extract_scope_token: true,
            enable_verify_scope: true,
            skip_verify_scope_without_spec: false,
            ignore_jwt_expiry: false,
            enable_h2c: false,
            enable_mock_jwt: false,
            enable_relaxed_key_validation: false,
            jwt: SecurityJwtConfig::default(),
            log_jwt_token: false,
            log_client_user_scope: false,
            enable_jwt_cache: true,
            jwt_cache_full_size: default_jwt_cache_full_size(),
            bootstrap_from_key_service: false,
            provider_id: String::new(),
            issuer: String::new(),
            audience: Vec::new(),
            skip_path_prefixes: Vec::new(),
            pass_through_claims: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SecurityJwtConfig {
    #[serde(default = "default_clock_skew_seconds")]
    pub clock_skew_in_seconds: u64,
}

impl Default for SecurityJwtConfig {
    fn default() -> Self {
        Self {
            clock_skew_in_seconds: default_clock_skew_seconds(),
        }
    }
}

#[derive(Clone)]
pub struct SecurityRuntime {
    pub config: SecurityConfig,
    jwk_source: Option<Arc<JwkSource>>,
    jwks: Arc<RwLock<BTreeMap<String, Jwk>>>,
    cache: Arc<Mutex<BTreeMap<String, CachedPrincipal>>>,
}

impl SecurityRuntime {
    fn new(config: SecurityConfig, runtime_config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let jwk_source = load_jwk_source(runtime_config)?;
        Ok(Self {
            config,
            jwk_source,
            jwks: Arc::new(RwLock::new(BTreeMap::new())),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        if self.config.bootstrap_from_key_service {
            refresh_jwks_for_service(self, None).await?;
        }
        Ok(())
    }
}

#[derive(Clone)]
struct JwkSource {
    client: ClientTokenConfig,
    request: ClientRequestConfig,
    tls: ClientTlsConfig,
    direct_registry: DirectRegistryConfig,
    registry_client: Option<Arc<PortalRegistryClient>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum JwkResponse {
    Set(JwkSet),
    Single(Jwk),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JwtExpiryMode {
    Enforce,
    Ignore,
}

#[derive(Clone)]
struct CachedPrincipal {
    principal: AuthPrincipal,
    expires_at: Option<u64>,
}

pub fn load_security_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<SecurityRuntime>, RuntimeError> {
    load_security_runtime_from_file(
        runtime_config,
        active,
        SECURITY_FILE,
        SECURITY_MODULE_ID,
        SECURITY_CONFIG_NAME,
    )
}

pub fn load_security_runtime_from_file(
    runtime_config: &RuntimeConfig,
    active: bool,
    file_name: &str,
    module_id: &str,
    config_name: &str,
) -> Result<Option<SecurityRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<SecurityConfig>(runtime_config, file_name)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == file_name => SecurityConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        module_id,
        config_name,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("swtClientSecretHeader")],
        config.enable_verify_jwt || config.enable_verify_swt,
        Some(config.enable_verify_jwt || config.enable_verify_swt),
        true,
    )?;

    Ok(
        (config.enable_verify_jwt || config.enable_verify_swt || config.enable_mock_jwt)
            .then(|| SecurityRuntime::new(config, runtime_config))
            .transpose()?,
    )
}

pub async fn verify_jwt_token(
    runtime: &SecurityRuntime,
    token: &str,
    expiry_mode: JwtExpiryMode,
) -> Result<AuthPrincipal, HandlerRejection> {
    verify_jwt_token_for_service(runtime, token, expiry_mode, None).await
}

async fn verify_jwt_token_for_service(
    runtime: &SecurityRuntime,
    token: &str,
    expiry_mode: JwtExpiryMode,
    service_id: Option<&str>,
) -> Result<AuthPrincipal, HandlerRejection> {
    let token = token.trim();
    if token.is_empty() {
        return Err(HandlerRejection::unauthorized("missing bearer token"));
    }
    if service_id.is_none()
        && matches!(expiry_mode, JwtExpiryMode::Enforce)
        && let Some(principal) = cached_principal(runtime, token)
    {
        return Ok(principal);
    }

    let header = match decode_header(token) {
        Ok(header) => header,
        Err(error) => {
            tracing::debug!("JWT header decoding failed: {error}");
            return Err(HandlerRejection::unauthorized("invalid JWT header"));
        }
    };
    let key = match decoding_key_for_token(runtime, header.kid.as_deref(), header.alg, service_id)
        .await
    {
        Ok(key) => key,
        Err(error) => {
            tracing::debug!("JWT key resolution failed: {}", error.message);
            return Err(error);
        }
    };
    let validation = validation_for_mode(&runtime.config, header.alg, expiry_mode);
    let decoded = match decode::<JsonValue>(token, &key, &validation) {
        Ok(decoded) => decoded,
        Err(error) if runtime.jwk_source.is_some() => {
            refresh_jwks_for_service(runtime, service_id).await?;
            let key =
                decoding_key_for_token(runtime, header.kid.as_deref(), header.alg, service_id)
                    .await?;
            decode::<JsonValue>(token, &key, &validation).map_err(|_| {
                tracing::debug!("JWT validation failed after JWKS refresh: {error}");
                HandlerRejection::unauthorized("JWT validation failed")
            })?
        }
        Err(error) => {
            tracing::debug!("JWT validation failed: {error}");
            return Err(HandlerRejection::unauthorized("JWT validation failed"));
        }
    };
    if let Err(error) = validate_issuer_and_audience(&runtime.config, &decoded.claims) {
        tracing::debug!("JWT issuer/audience validation failed: {}", error.message);
        return Err(error);
    }
    if let Err(error) = validate_client_key_audience(runtime, service_id, &decoded.claims) {
        tracing::debug!(
            "JWT client key audience validation failed: {}",
            error.message
        );
        return Err(error);
    }
    let principal = principal_from_claims(decoded.claims);
    if service_id.is_none() && matches!(expiry_mode, JwtExpiryMode::Enforce) {
        cache_principal(runtime, token.to_string(), &principal);
    }
    Ok(principal)
}

pub async fn verify_jwt_request(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    let config = &runtime.config;
    if request_path_is_skipped(config, request_path) {
        return Ok(None);
    }
    if !config.enable_h2c && is_h2c_upgrade(session) {
        return Err(HandlerRejection::new(
            405,
            "ERR10048",
            "cleartext HTTP/2 upgrade is not allowed",
        ));
    }
    if config.enable_mock_jwt {
        return Ok(Some(mock_principal()));
    }
    if !config.enable_verify_jwt {
        return Ok(None);
    }

    let token = bearer_token(session).or_else(|| {
        config
            .enable_extract_scope_token
            .then(|| request_header(session, SCOPE_TOKEN))
            .flatten()
    });
    let token = token.ok_or_else(|| HandlerRejection::unauthorized("missing bearer token"))?;
    let jwk_service_id = jwk_service_id_for_request(runtime, session, request_path);

    if jwk_service_id.is_none()
        && let Some(principal) = cached_principal(runtime, token.as_str())
    {
        apply_pass_through_claims(session, config, &principal)?;
        return Ok(Some(principal));
    }

    let principal = verify_jwt_token_for_service(
        runtime,
        token.as_str(),
        JwtExpiryMode::Enforce,
        jwk_service_id.as_deref(),
    )
    .await?;
    apply_pass_through_claims(session, config, &principal)?;
    Ok(Some(principal))
}

fn validation_for_mode(
    config: &SecurityConfig,
    algorithm: Algorithm,
    expiry_mode: JwtExpiryMode,
) -> Validation {
    let mut validation = Validation::new(algorithm);
    validation.leeway = config.jwt.clock_skew_in_seconds;
    validation.validate_aud = false;
    if config.ignore_jwt_expiry || matches!(expiry_mode, JwtExpiryMode::Ignore) {
        validation.validate_exp = false;
        validation.required_spec_claims.clear();
    }
    validation
}

fn validate_issuer_and_audience(
    config: &SecurityConfig,
    claims: &JsonValue,
) -> Result<(), HandlerRejection> {
    let issuer = config.issuer.trim();
    if !issuer.is_empty() && claim_string(claims, "iss").as_deref() != Some(issuer) {
        return Err(HandlerRejection::unauthorized(
            "JWT issuer validation failed",
        ));
    }

    if !config.audience.is_empty() {
        let configured = config
            .audience
            .iter()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !configured.is_empty() && !claim_audience_matches(claims, &configured) {
            return Err(HandlerRejection::unauthorized(
                "JWT audience validation failed",
            ));
        }
    }
    Ok(())
}

fn claim_audience_matches(claims: &JsonValue, configured: &[&str]) -> bool {
    match claims.get("aud") {
        Some(JsonValue::String(value)) => configured.iter().any(|expected| value == expected),
        Some(JsonValue::Array(values)) => values.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|audience| configured.iter().any(|expected| audience == *expected))
        }),
        _ => false,
    }
}

fn jwk_service_id_for_request(
    runtime: &SecurityRuntime,
    session: &Session,
    request_path: &str,
) -> Option<String> {
    let source = runtime.jwk_source.as_ref()?;
    let explicit = request_header(session, SERVICE_ID_HEADER);
    OAuthProviderResolver::new(&source.client)
        .service_id_for_request(explicit.as_deref(), request_path)
}

fn validate_client_key_audience(
    runtime: &SecurityRuntime,
    service_id: Option<&str>,
    claims: &JsonValue,
) -> Result<(), HandlerRejection> {
    let Some(source) = runtime.jwk_source.as_ref() else {
        return Ok(());
    };
    let provider = match OAuthProviderResolver::new(&source.client).key_provider(service_id) {
        Ok(provider) => provider,
        Err(OAuthProviderError::MissingServiceId { .. })
        | Err(OAuthProviderError::MissingProvider { .. }) => return Ok(()),
        Err(error) => return Err(jwk_provider_error(error)),
    };
    let Some(audience) = non_empty(provider.audience.as_deref()) else {
        return Ok(());
    };
    if !claim_audience_matches(claims, &[audience]) {
        return Err(HandlerRejection::unauthorized(
            "JWT audience validation failed",
        ));
    }
    Ok(())
}

async fn decoding_key_for_token(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    _algorithm: Algorithm,
    service_id: Option<&str>,
) -> Result<DecodingKey, HandlerRejection> {
    if let Some(key) = decoding_key_from_jwk(runtime, kid, service_id).await? {
        return Ok(key);
    }
    Err(HandlerRejection::unauthorized("JWT key is not configured"))
}

async fn decoding_key_from_jwk(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_id: Option<&str>,
) -> Result<Option<DecodingKey>, HandlerRejection> {
    let Some(jwk) = jwk_for_token(runtime, kid, service_id).await? else {
        return Ok(None);
    };
    DecodingKey::from_jwk(&jwk)
        .map(Some)
        .map_err(|_| HandlerRejection::unauthorized("JWK cannot verify token algorithm"))
}

async fn jwk_for_token(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_id: Option<&str>,
) -> Result<Option<Jwk>, HandlerRejection> {
    if runtime.jwk_source.is_none() {
        return Ok(None);
    }
    if let Some(jwk) = cached_jwk_for_token(runtime, kid, service_id).await {
        return Ok(Some(jwk));
    }
    refresh_jwks_for_service(runtime, service_id).await?;
    Ok(cached_jwk_for_token(runtime, kid, service_id).await)
}

async fn cached_jwk_for_token(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_id: Option<&str>,
) -> Option<Jwk> {
    let jwks = runtime.jwks.read().await;
    if let Some(kid) = kid {
        if let Some(service_id) = service_id.and_then(|value| non_empty(Some(value))) {
            if let Some(jwk) = jwks.get(jwk_cache_key(kid, Some(service_id)).as_str()) {
                return Some(jwk.clone());
            }
        }
        if let Some(jwk) = jwks.get(kid) {
            return Some(jwk.clone());
        }
        if service_id.is_none() {
            let suffix = format!(":{kid}");
            let mut matched = jwks
                .iter()
                .filter(|(key, _)| key.ends_with(suffix.as_str()));
            if let Some((_, jwk)) = matched.next()
                && matched.next().is_none()
            {
                return Some(jwk.clone());
            }
        }
    }
    if let Some(service_id) = service_id.and_then(|value| non_empty(Some(value))) {
        let prefix = format!("{service_id}:");
        let mut matched = jwks
            .iter()
            .filter(|(key, _)| key.starts_with(prefix.as_str()));
        if let Some((_, jwk)) = matched.next()
            && matched.next().is_none()
        {
            return Some(jwk.clone());
        }
        return None;
    }
    if jwks.len() == 1 || (kid.is_none() && runtime.config.enable_relaxed_key_validation) {
        return jwks.values().next().cloned();
    }
    None
}

async fn refresh_jwks_for_service(
    runtime: &SecurityRuntime,
    service_id: Option<&str>,
) -> Result<(), HandlerRejection> {
    let Some(source) = runtime.jwk_source.as_ref() else {
        return Ok(());
    };
    let providers = key_providers_for_service(source, service_id)?;
    let mut entries = BTreeMap::new();
    for provider in providers {
        entries.extend(fetch_jwks_for_provider(source, &provider).await?);
    }
    if entries.is_empty() {
        return Err(HandlerRejection::new(
            502,
            "ERR10056",
            "JWKS endpoint did not return any keys with kid",
        ));
    }
    let mut cached = runtime.jwks.write().await;
    if let Some(service_id) = service_id.and_then(|value| non_empty(Some(value))) {
        let prefix = format!("{service_id}:");
        cached.retain(|key, _| !key.starts_with(prefix.as_str()));
        cached.extend(entries);
    } else {
        *cached = entries;
    }
    Ok(())
}

fn key_providers_for_service(
    source: &JwkSource,
    service_id: Option<&str>,
) -> Result<Vec<ResolvedKeyProvider>, HandlerRejection> {
    let resolver = OAuthProviderResolver::new(&source.client);
    match service_id.and_then(|value| non_empty(Some(value))) {
        Some(service_id) => resolver
            .key_provider(Some(service_id))
            .map(|provider| vec![provider])
            .map_err(jwk_provider_error),
        None => resolver.key_providers().map_err(jwk_provider_error),
    }
}

async fn fetch_jwks_for_provider(
    source: &JwkSource,
    provider: &ResolvedKeyProvider,
) -> Result<BTreeMap<String, Jwk>, HandlerRejection> {
    let server_url = resolve_jwk_server_url(source, provider).await?;
    let url = jwk_endpoint_url(server_url.as_str(), provider.uri.as_str())?;
    let client = ClientFactory::from_parts(source.request.clone(), source.tls.clone())
        .reqwest_client(EndpointOptions {
            proxy_host: provider.proxy_host.clone(),
            proxy_port: provider.proxy_port,
            enable_http2: Some(provider.enable_http2),
            ..EndpointOptions::default()
        })
        .map_err(|error| {
            HandlerRejection::new(500, "ERR10056", format!("invalid JWK client: {error}"))
        })?;
    let mut request = client
        .get(url.as_str())
        .header("accept", "application/json");
    if let (Some(client_id), Some(client_secret)) = (
        non_empty(Some(provider.client_id.as_str())),
        non_empty(Some(provider.client_secret.as_str())),
    ) {
        request = request.basic_auth(client_id, Some(client_secret));
    }
    let response = request.send().await.map_err(|error| {
        HandlerRejection::new(502, "ERR10056", format!("failed to request JWKS: {error}"))
    })?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        HandlerRejection::new(
            502,
            "ERR10056",
            format!("failed to read JWKS response: {error}"),
        )
    })?;
    if !status.is_success() {
        return Err(HandlerRejection::new(
            502,
            "ERR10056",
            format!("JWKS endpoint returned {status}: {body}"),
        ));
    }
    let jwks = parse_jwks(body.as_str())?;
    let mut entries = BTreeMap::new();
    for jwk in jwks {
        if let Some(kid) = jwk
            .common
            .key_id
            .clone()
            .filter(|kid| !kid.trim().is_empty())
        {
            entries.insert(
                jwk_cache_key(kid.as_str(), provider.cache_service_id.as_deref()),
                jwk,
            );
        }
    }
    Ok(entries)
}

fn jwk_cache_key(kid: &str, service_id: Option<&str>) -> String {
    service_id
        .and_then(|value| non_empty(Some(value)))
        .map(|service_id| format!("{service_id}:{kid}"))
        .unwrap_or_else(|| kid.to_string())
}

fn jwk_provider_error(error: OAuthProviderError) -> HandlerRejection {
    HandlerRejection::new(500, "ERR10056", error.to_string())
}

fn load_jwk_source(runtime_config: &RuntimeConfig) -> Result<Option<Arc<JwkSource>>, RuntimeError> {
    let client = match load_client_config(runtime_config) {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == CLIENT_FILE => return Ok(None),
        Err(error) => return Err(error),
    };
    let resolver = OAuthProviderResolver::new(&client);
    if !resolver.has_key_providers() {
        return Ok(None);
    }
    Ok(Some(Arc::new(JwkSource {
        request: client.request.clone(),
        tls: client.tls.clone(),
        client,
        direct_registry: runtime_config.direct_registry.clone(),
        registry_client: runtime_config.registry_client.clone(),
    })))
}

fn parse_jwks(body: &str) -> Result<Vec<Jwk>, HandlerRejection> {
    let response = serde_json::from_str::<JwkResponse>(body).map_err(|error| {
        HandlerRejection::new(502, "ERR10056", format!("invalid JWKS response: {error}"))
    })?;
    Ok(match response {
        JwkResponse::Set(set) => set.keys,
        JwkResponse::Single(jwk) => vec![jwk],
    })
}

async fn resolve_jwk_server_url(
    source: &JwkSource,
    provider: &ResolvedKeyProvider,
) -> Result<String, HandlerRejection> {
    if let Some(server_url) = non_empty(provider.server_url.as_deref()) {
        return Ok(server_url.to_string());
    }
    let service_id = non_empty(provider.key_service_id.as_deref()).ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            "client.yml oauth.token.key server_url or serviceId is required",
        )
    })?;
    if let Some(matched) = direct_registry_match(&source.direct_registry, service_id, None) {
        return Ok(matched.url.trim().to_string());
    }
    let registry_client = source.registry_client.as_deref().ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            "JWK serviceId discovery requires portal registry to be enabled",
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
                format!("failed to discover JWK service `{service_id}`: {error}"),
            )
        })?;
    let node = select_jwk_node(&snapshot.nodes).ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10056",
            format!("JWK service `{service_id}` has no usable discovery nodes"),
        )
    })?;
    Ok(discovery_node_base_url(node))
}

fn select_jwk_node(nodes: &[DiscoveryNode]) -> Option<&DiscoveryNode> {
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

fn jwk_endpoint_url(server_url: &str, uri: &str) -> Result<String, HandlerRejection> {
    let server_url = server_url.trim().trim_end_matches('/');
    if server_url.is_empty() {
        return Err(HandlerRejection::new(
            502,
            "ERR10056",
            "JWK server_url is empty",
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
            format!("invalid JWK endpoint URL `{url}`: {error}"),
        )
    })?;
    Ok(url)
}

fn bearer_token(session: &Session) -> Option<String> {
    let value = header_value(session, AUTHORIZATION)?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

fn principal_from_claims(claims: JsonValue) -> AuthPrincipal {
    AuthPrincipal {
        client_id: claim_string(&claims, "client_id").or_else(|| claim_string(&claims, "cid")),
        user_id: claim_string(&claims, "user_id")
            .or_else(|| claim_string(&claims, "uid"))
            .or_else(|| claim_string(&claims, "sub")),
        issuer: claim_string(&claims, "iss"),
        email: claim_string(&claims, "email").or_else(|| claim_string(&claims, "eml")),
        host: claim_string(&claims, "host"),
        role: claim_string(&claims, "role"),
        claims,
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

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn apply_pass_through_claims(
    session: &mut Session,
    config: &SecurityConfig,
    principal: &AuthPrincipal,
) -> Result<(), HandlerRejection> {
    for (claim_name, header_name) in &config.pass_through_claims {
        let Some(value) = claim_string(&principal.claims, claim_name) else {
            continue;
        };
        session
            .req_header_mut()
            .insert_header(header_name.to_string(), value)
            .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid pass-through header"))?;
    }
    Ok(())
}

fn cached_principal(runtime: &SecurityRuntime, token: &str) -> Option<AuthPrincipal> {
    if !runtime.config.enable_jwt_cache {
        return None;
    }
    let cached = runtime
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(token)
        .cloned()?;
    if runtime.config.ignore_jwt_expiry {
        return Some(cached.principal);
    }
    let Some(expires_at) = cached.expires_at else {
        return Some(cached.principal);
    };
    (now_epoch_seconds() <= expires_at.saturating_add(runtime.config.jwt.clock_skew_in_seconds))
        .then_some(cached.principal)
}

fn cache_principal(runtime: &SecurityRuntime, token: String, principal: &AuthPrincipal) {
    if !runtime.config.enable_jwt_cache {
        return;
    }
    let expires_at = principal.claims.get("exp").and_then(JsonValue::as_u64);
    let mut cache = runtime
        .cache
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if cache.len() >= runtime.config.jwt_cache_full_size {
        if let Some(first_key) = cache.keys().next().cloned() {
            cache.remove(&first_key);
        }
    }
    cache.insert(
        token,
        CachedPrincipal {
            principal: principal.clone(),
            expires_at,
        },
    );
}

fn request_path_is_skipped(config: &SecurityConfig, request_path: &str) -> bool {
    config
        .skip_path_prefixes
        .iter()
        .any(|prefix| request_path.starts_with(prefix.as_str()))
}

fn is_h2c_upgrade(session: &Session) -> bool {
    let Some(upgrade) = request_header(session, "upgrade") else {
        return false;
    };
    if !upgrade.eq_ignore_ascii_case("h2c") {
        return false;
    }
    request_header(session, "connection")
        .map(|value| value.to_ascii_lowercase().contains("upgrade"))
        .unwrap_or(false)
}

fn mock_principal() -> AuthPrincipal {
    AuthPrincipal {
        client_id: Some("mock-client".to_string()),
        user_id: Some("mock-user".to_string()),
        issuer: Some("mock".to_string()),
        claims: serde_json::json!({
            "client_id": "mock-client",
            "user_id": "mock-user",
            "iss": "mock"
        }),
        ..AuthPrincipal::default()
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn default_true() -> bool {
    true
}

fn default_swt_client_id_header() -> String {
    "swt-client-id".to_string()
}

fn default_swt_client_secret_header() -> String {
    "swt-client-secret".to_string()
}

fn default_jwt_cache_full_size() -> usize {
    1000
}

fn default_clock_skew_seconds() -> u64 {
    60
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_client::AuthServerConfig;

    #[test]
    fn security_config_accepts_java_style_maps_and_lists() {
        let config: SecurityConfig = serde_yaml::from_str(
            r#"
skipPathPrefixes: /health,/info
passThroughClaims: 'client_id=x-client-id,sub=x-user-id'
"#,
        )
        .expect("parse security config");

        assert_eq!(config.skip_path_prefixes, ["/health", "/info"]);
        assert_eq!(config.pass_through_claims["client_id"], "x-client-id");
    }

    #[test]
    fn principal_uses_light_claim_fallbacks() {
        let principal = principal_from_claims(serde_json::json!({
            "cid": "client",
            "uid": "user",
            "eml": "user@example.com"
        }));

        assert_eq!(principal.client_id.as_deref(), Some("client"));
        assert_eq!(principal.user_id.as_deref(), Some("user"));
        assert_eq!(principal.email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn key_provider_selection_uses_service_id_auth_servers() {
        let mut client = ClientTokenConfig::default();
        client.oauth.multiple_auth_servers = true;
        client.oauth.token.key.server_url = Some("https://global-oauth".to_string());
        client.oauth.token.key.service_id_auth_servers.insert(
            "orders".to_string(),
            AuthServerConfig {
                server_url: Some("https://orders-oauth".to_string()),
                audience: Some("orders-api".to_string()),
                ..AuthServerConfig::default()
            },
        );
        let source = test_jwk_source(client);

        let providers = key_providers_for_service(&source, Some("orders")).expect("providers");

        assert_eq!(providers.len(), 1);
        assert_eq!(
            providers[0].server_url.as_deref(),
            Some("https://orders-oauth")
        );
        assert_eq!(providers[0].audience.as_deref(), Some("orders-api"));
        assert_eq!(providers[0].cache_service_id.as_deref(), Some("orders"));
    }

    #[test]
    fn client_key_audience_uses_service_provider_audience() {
        let mut client = ClientTokenConfig::default();
        client.oauth.multiple_auth_servers = true;
        client.oauth.token.key.service_id_auth_servers.insert(
            "orders".to_string(),
            AuthServerConfig {
                server_url: Some("https://orders-oauth".to_string()),
                audience: Some("orders-api".to_string()),
                ..AuthServerConfig::default()
            },
        );
        let runtime = test_runtime_with_client(client);

        validate_client_key_audience(
            &runtime,
            Some("orders"),
            &serde_json::json!({ "aud": "orders-api" }),
        )
        .expect("audience matches");
        let error = validate_client_key_audience(
            &runtime,
            Some("orders"),
            &serde_json::json!({ "aud": "other-api" }),
        )
        .expect_err("audience mismatch");

        assert_eq!(error.status, 401);
        assert_eq!(error.code, "ERR10002");
    }

    fn test_runtime_with_client(client: ClientTokenConfig) -> SecurityRuntime {
        SecurityRuntime {
            config: SecurityConfig::default(),
            jwk_source: Some(Arc::new(test_jwk_source(client))),
            jwks: Arc::new(RwLock::new(BTreeMap::new())),
            cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn test_jwk_source(client: ClientTokenConfig) -> JwkSource {
        JwkSource {
            request: client.request.clone(),
            tls: client.tls.clone(),
            client,
            direct_registry: DirectRegistryConfig::default(),
            registry_client: None,
        }
    }
}
