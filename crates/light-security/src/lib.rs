use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use light_client::{
    ClientConfig as ClientTokenConfig, ClientFactory, ClientRequestConfig, ClientTlsConfig,
    EndpointOptions, OAuthProviderError, OAuthProviderResolver, ResolvedKeyProvider,
};
use light_runtime::{
    CLIENT_CONFIG_NAME, CLIENT_MODULE_ID, DirectRegistryConfig, DiscoveryNode,
    DiscoverySubscription, MaskSpec, ModuleKind, PortalRegistryClient, RuntimeConfig, RuntimeError,
    client_config_masks,
};
use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

pub const SECURITY_FILE: &str = "security.yml";
pub const SECURITY_MODULE_ID: &str = "light-pingora/security";
pub const SECURITY_CONFIG_NAME: &str = "security";

const WWW_AUTHENTICATE: &str = "www-authenticate";
const CLIENT_FILE: &str = "client.yml";

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

    pub fn service_id_for_request(
        &self,
        explicit_service_id: Option<&str>,
        request_path: &str,
    ) -> Option<String> {
        self.jwk_source.as_ref().and_then(|source| {
            OAuthProviderResolver::new(&source.client)
                .service_id_for_request(explicit_service_id, request_path)
        })
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
    verify_jwt_token_for_services(runtime, token, expiry_mode, &[]).await
}

pub async fn verify_jwt_token_for_services(
    runtime: &SecurityRuntime,
    token: &str,
    expiry_mode: JwtExpiryMode,
    service_ids: &[String],
) -> Result<AuthPrincipal, HandlerRejection> {
    let token = token.trim();
    if token.is_empty() {
        return Err(HandlerRejection::unauthorized("missing bearer token"));
    }
    let has_service_ids = has_service_ids(service_ids);
    if !has_service_ids
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
    let key = match decoding_key_for_token(runtime, header.kid.as_deref(), header.alg, service_ids)
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
            refresh_jwks_for_services(runtime, service_ids).await?;
            let key =
                decoding_key_for_token(runtime, header.kid.as_deref(), header.alg, service_ids)
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
    if let Err(error) = validate_client_key_audiences(runtime, service_ids, &decoded.claims) {
        tracing::debug!(
            "JWT client key audience validation failed: {}",
            error.message
        );
        return Err(error);
    }
    let principal = principal_from_claims(decoded.claims);
    if !has_service_ids && matches!(expiry_mode, JwtExpiryMode::Enforce) {
        cache_principal(runtime, token.to_string(), &principal);
    }
    Ok(principal)
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

fn validate_client_key_audiences(
    runtime: &SecurityRuntime,
    service_ids: &[String],
    claims: &JsonValue,
) -> Result<(), HandlerRejection> {
    let Some(source) = runtime.jwk_source.as_ref() else {
        return Ok(());
    };
    let resolver = OAuthProviderResolver::new(&source.client);
    let normalized = normalized_service_ids(service_ids);
    if normalized.is_empty() {
        let provider = match resolver.key_provider(None) {
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
        return Ok(());
    }

    let mut found_configured_audience = false;
    for service_id in &normalized {
        let provider = match resolver.key_provider(Some(service_id.as_str())) {
            Ok(provider) => provider,
            Err(OAuthProviderError::MissingServiceId { .. })
            | Err(OAuthProviderError::MissingProvider { .. }) => continue,
            Err(error) => return Err(jwk_provider_error(error)),
        };
        let Some(audience) = non_empty(provider.audience.as_deref()) else {
            continue;
        };
        found_configured_audience = true;
        if claim_audience_matches(claims, &[audience]) {
            return Ok(());
        }
    }
    if found_configured_audience {
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
    service_ids: &[String],
) -> Result<DecodingKey, HandlerRejection> {
    if let Some(key) = decoding_key_from_jwk(runtime, kid, service_ids).await? {
        return Ok(key);
    }
    Err(HandlerRejection::unauthorized("JWT key is not configured"))
}

async fn decoding_key_from_jwk(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_ids: &[String],
) -> Result<Option<DecodingKey>, HandlerRejection> {
    let Some(jwk) = jwk_for_services(runtime, kid, service_ids).await? else {
        return Ok(None);
    };
    DecodingKey::from_jwk(&jwk)
        .map(Some)
        .map_err(|_| HandlerRejection::unauthorized("JWK cannot verify token algorithm"))
}

async fn jwk_for_services(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_ids: &[String],
) -> Result<Option<Jwk>, HandlerRejection> {
    if runtime.jwk_source.is_none() {
        return Ok(None);
    }
    if let Some(jwk) = cached_jwk_for_services(runtime, kid, service_ids).await {
        return Ok(Some(jwk));
    }
    refresh_jwks_for_services(runtime, service_ids).await?;
    Ok(cached_jwk_for_services(runtime, kid, service_ids).await)
}

async fn cached_jwk_for_services(
    runtime: &SecurityRuntime,
    kid: Option<&str>,
    service_ids: &[String],
) -> Option<Jwk> {
    let service_ids = normalized_service_ids(service_ids);
    let jwks = runtime.jwks.read().await;
    if let Some(kid) = kid {
        if !service_ids.is_empty() {
            for service_id in &service_ids {
                if let Some(jwk) = jwks.get(jwk_cache_key(kid, Some(service_id)).as_str()) {
                    return Some(jwk.clone());
                }
            }
            if service_ids.len() == 1
                && let Some(jwk) = jwks.get(kid)
            {
                return Some(jwk.clone());
            }
        } else {
            if let Some(jwk) = jwks.get(kid) {
                return Some(jwk.clone());
            }
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
    if !service_ids.is_empty() {
        for service_id in &service_ids {
            let prefix = format!("{service_id}:");
            let mut matched = jwks
                .iter()
                .filter(|(key, _)| key.starts_with(prefix.as_str()));
            if let Some((_, jwk)) = matched.next()
                && matched.next().is_none()
            {
                return Some(jwk.clone());
            }
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
    let service_ids = service_id
        .and_then(|id| non_empty(Some(id)))
        .map(|id| vec![id.to_string()])
        .unwrap_or_default();
    refresh_jwks_for_services(runtime, &service_ids).await
}

async fn refresh_jwks_for_services(
    runtime: &SecurityRuntime,
    service_ids: &[String],
) -> Result<(), HandlerRejection> {
    let Some(source) = runtime.jwk_source.as_ref() else {
        return Ok(());
    };
    let service_ids = normalized_service_ids(service_ids);
    let providers = key_providers_for_services(source, &service_ids)?;
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
    if !service_ids.is_empty() {
        let prefixes = service_ids
            .iter()
            .map(|service_id| format!("{service_id}:"))
            .collect::<Vec<_>>();
        cached.retain(|key, _| {
            !prefixes
                .iter()
                .any(|prefix| key.starts_with(prefix.as_str()))
        });
        cached.extend(entries);
    } else {
        *cached = entries;
    }
    Ok(())
}

fn key_providers_for_services(
    source: &JwkSource,
    service_ids: &[String],
) -> Result<Vec<ResolvedKeyProvider>, HandlerRejection> {
    let resolver = OAuthProviderResolver::new(&source.client);
    let service_ids = normalized_service_ids(service_ids);
    if service_ids.is_empty() {
        return resolver.key_providers().map_err(jwk_provider_error);
    }

    let mut providers = Vec::with_capacity(service_ids.len());
    for service_id in &service_ids {
        providers.push(
            resolver
                .key_provider(Some(service_id.as_str()))
                .map_err(jwk_provider_error)?,
        );
    }
    Ok(providers)
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
    tracing::info!(
        "load_jwk_source resolved key config: server_url={:?}, service_id={:?}, uri={}, client_id={:?}, has_client_secret={}, service_id_auth_servers={:?}, audience={:?}",
        client.oauth.token.key.server_url,
        client.oauth.token.key.service_id,
        client.oauth.token.key.uri,
        client.oauth.token.key.client_id,
        !client.oauth.token.key.client_secret.is_empty(),
        client
            .oauth
            .token
            .key
            .service_id_auth_servers
            .keys()
            .collect::<Vec<_>>(),
        client.oauth.token.key.audience,
    );
    tracing::info!(
        "load_jwk_source has_key_providers: {}",
        resolver.has_key_providers()
    );
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

fn load_client_config(runtime_config: &RuntimeConfig) -> Result<ClientTokenConfig, RuntimeError> {
    if let Some(client) = runtime_config.client.as_ref() {
        return Ok(client.clone());
    }
    let client = runtime_config
        .module_registry
        .load_config::<ClientTokenConfig>(runtime_config, CLIENT_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        CLIENT_MODULE_ID,
        CLIENT_CONFIG_NAME,
        ModuleKind::Core,
        &client,
        client_config_masks(),
        true,
        Some(true),
        true,
    )?;
    Ok(client)
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
    if let Some(url) = source.direct_registry.direct_urls.get(service_id) {
        return Ok(url.trim().to_string());
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

fn has_service_ids(service_ids: &[String]) -> bool {
    service_ids
        .iter()
        .any(|service_id| !service_id.trim().is_empty())
}

fn normalized_service_ids(service_ids: &[String]) -> Vec<String> {
    service_ids
        .iter()
        .map(|service_id| service_id.trim())
        .filter(|service_id| !service_id.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringListVisitor;
    impl<'de> Visitor<'de> for StringListVisitor {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string, JSON/YAML string list, or list of strings")
        }
        fn visit_str<E: DeError>(self, value: &str) -> Result<Self::Value, E> {
            let value = value.trim();
            if value.is_empty() {
                return Ok(Vec::new());
            }
            if value.starts_with('[') {
                return serde_yaml::from_str(value).map_err(E::custom);
            }
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(ToOwned::to_owned)
                .collect())
        }
        fn visit_string<E: DeError>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_any(StringListVisitor)
}

fn deserialize_string_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringMapVisitor;
    impl<'de> Visitor<'de> for StringMapVisitor {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map, JSON/YAML string map, or key=value list")
        }
        fn visit_str<E: DeError>(self, value: &str) -> Result<Self::Value, E> {
            parse_string_map(value).map_err(E::custom)
        }
        fn visit_string<E: DeError>(self, value: String) -> Result<Self::Value, E> {
            self.visit_str(&value)
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, serde_yaml::Value>()? {
                values.insert(key, yaml_scalar_to_string(value).map_err(A::Error::custom)?);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_any(StringMapVisitor)
}

fn parse_string_map(value: &str) -> Result<BTreeMap<String, String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(BTreeMap::new());
    }
    if value.starts_with('{') {
        let value = serde_yaml::from_str::<serde_yaml::Value>(value).map_err(|e| e.to_string())?;
        let serde_yaml::Value::Mapping(mapping) = value else {
            return Err("expected map value".to_string());
        };
        return mapping
            .into_iter()
            .map(|(key, value)| {
                let key = key
                    .as_str()
                    .ok_or_else(|| "map key must be a string".to_string())?
                    .to_string();
                Ok((key, yaml_scalar_to_string(value)?))
            })
            .collect();
    }
    value
        .split([',', '&'])
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (key, value) = entry
                .split_once('=')
                .or_else(|| entry.split_once(':'))
                .ok_or_else(|| format!("invalid key/value entry `{entry}`"))?;
            let key = key.trim();
            if key.is_empty() {
                return Err("map key must not be empty".to_string());
            }
            Ok((key.to_string(), value.trim().to_string()))
        })
        .collect()
}

fn yaml_scalar_to_string(value: serde_yaml::Value) -> Result<String, String> {
    match value {
        serde_yaml::Value::Null => Ok(String::new()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::String(value) => Ok(value),
        other => Err(format!("expected scalar map value, got {other:?}")),
    }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

        let providers =
            key_providers_for_services(&source, &["orders".to_string()]).expect("providers");

        assert_eq!(providers.len(), 1);
        assert_eq!(
            providers[0].server_url.as_deref(),
            Some("https://orders-oauth")
        );
        assert_eq!(providers[0].audience.as_deref(), Some("orders-api"));
        assert_eq!(providers[0].cache_service_id.as_deref(), Some("orders"));
    }

    #[test]
    fn key_provider_selection_uses_all_service_ids() {
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
        client.oauth.token.key.service_id_auth_servers.insert(
            "billing".to_string(),
            AuthServerConfig {
                server_url: Some("https://billing-oauth".to_string()),
                audience: Some("billing-api".to_string()),
                ..AuthServerConfig::default()
            },
        );
        let source = test_jwk_source(client);

        let providers =
            key_providers_for_services(&source, &["orders".to_string(), "billing".to_string()])
                .expect("providers");

        assert_eq!(providers.len(), 2);
        assert_eq!(
            providers[0].server_url.as_deref(),
            Some("https://orders-oauth")
        );
        assert_eq!(
            providers[1].server_url.as_deref(),
            Some("https://billing-oauth")
        );
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

        validate_client_key_audiences(
            &runtime,
            &["orders".to_string()],
            &serde_json::json!({ "aud": "orders-api" }),
        )
        .expect("audience matches");
        let error = validate_client_key_audiences(
            &runtime,
            &["orders".to_string()],
            &serde_json::json!({ "aud": "other-api" }),
        )
        .expect_err("audience mismatch");

        assert_eq!(error.status, 401);
        assert_eq!(error.code, "ERR10002");
    }

    #[test]
    fn client_key_audience_accepts_any_service_provider_audience() {
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
        client.oauth.token.key.service_id_auth_servers.insert(
            "billing".to_string(),
            AuthServerConfig {
                server_url: Some("https://billing-oauth".to_string()),
                audience: Some("billing-api".to_string()),
                ..AuthServerConfig::default()
            },
        );
        let runtime = test_runtime_with_client(client);

        validate_client_key_audiences(
            &runtime,
            &["orders".to_string(), "billing".to_string()],
            &serde_json::json!({ "aud": "billing-api" }),
        )
        .expect("one configured audience matches");
        let error = validate_client_key_audiences(
            &runtime,
            &["orders".to_string(), "billing".to_string()],
            &serde_json::json!({ "aud": "other-api" }),
        )
        .expect_err("all configured audiences mismatch");

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

    #[tokio::test]
    async fn cached_jwk_prefers_exact_service_kid() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk1: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "global-n",
            "e": "AQAB"
        }"#,
        )
        .unwrap();
        let jwk2: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "service-n",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("key1".to_string(), jwk1.clone());
            jwks.insert("service1:key1".to_string(), jwk2.clone());
        }

        let found =
            cached_jwk_for_services(&runtime, Some("key1"), &["service1".to_string()]).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap(), jwk2);
    }

    #[tokio::test]
    async fn cached_jwk_uses_plain_kid_for_global_provider() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk1: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "global-n",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("key1".to_string(), jwk1.clone());
        }

        let found =
            cached_jwk_for_services(&runtime, Some("key1"), &["service1".to_string()]).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap(), jwk1);
    }

    #[tokio::test]
    async fn cached_jwk_does_not_cross_service_prefixes() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("service1:key1".to_string(), jwk.clone());
        }

        let found =
            cached_jwk_for_services(&runtime, Some("key1"), &["service2".to_string()]).await;
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn cached_jwk_uses_ordered_service_id_list() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk1: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val-1",
            "e": "AQAB"
        }"#,
        )
        .unwrap();
        let jwk2: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val-2",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("service1:key1".to_string(), jwk1.clone());
            jwks.insert("service2:key1".to_string(), jwk2.clone());
        }

        let found = cached_jwk_for_services(
            &runtime,
            Some("key1"),
            &["service2".to_string(), "service1".to_string()],
        )
        .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap(), jwk2);
    }

    #[tokio::test]
    async fn cached_jwk_no_service_suffix_match_requires_uniqueness() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("service1:key1".to_string(), jwk.clone());
        }

        let found = cached_jwk_for_services(&runtime, Some("key1"), &[]).await;
        assert!(found.is_some());
        assert_eq!(found.unwrap(), jwk);
    }

    #[tokio::test]
    async fn cached_jwk_no_service_suffix_match_fails_on_ambiguity() {
        let runtime = test_runtime_with_client(ClientTokenConfig::default());
        let jwk1: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val-1",
            "e": "AQAB"
        }"#,
        )
        .unwrap();
        let jwk2: Jwk = serde_json::from_str(
            r#"{
            "kty": "RSA",
            "kid": "key1",
            "n": "n-val-2",
            "e": "AQAB"
        }"#,
        )
        .unwrap();

        {
            let mut jwks = runtime.jwks.write().await;
            jwks.insert("service1:key1".to_string(), jwk1.clone());
            jwks.insert("service2:key1".to_string(), jwk2.clone());
        }

        let found = cached_jwk_for_services(&runtime, Some("key1"), &[]).await;
        assert!(found.is_none());
    }

    async fn spawn_mock_jwks_server(response: String) -> (String, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let address = listener.local_addr().expect("local addr");
        let call_count = Arc::new(Mutex::new(0));
        let count_clone = call_count.clone();
        tokio::spawn(async move {
            loop {
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buffer = vec![0_u8; 1024];
                    let _read = stream.read(&mut buffer).await;
                    *count_clone.lock().unwrap() += 1;
                    let _ = stream.write_all(response.as_bytes()).await;
                } else {
                    break;
                }
            }
        });
        (format!("http://{address}"), call_count)
    }

    #[tokio::test]
    async fn jwk_for_token_fetches_once_for_cached_kid() {
        let response_body = r#"{"keys":[{
            "kty": "RSA",
            "kid": "key_fetch_once",
            "n": "u1WabeaJBoGl",
            "e": "AQAB"
        }]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let (server_url, call_count) = spawn_mock_jwks_server(response).await;

        let mut client = ClientTokenConfig::default();
        client.oauth.token.key.server_url = Some(server_url.clone());
        client.oauth.token.key.uri = "/keys".to_string();

        let runtime = test_runtime_with_client(client);

        // First call should fetch
        let first = jwk_for_services(&runtime, Some("key_fetch_once"), &[])
            .await
            .unwrap();
        assert!(first.is_some());
        assert_eq!(
            first.unwrap().common.key_id,
            Some("key_fetch_once".to_string())
        );
        assert_eq!(*call_count.lock().unwrap(), 1);

        // Second call should hit the cache directly, call count stays at 1
        let second = jwk_for_services(&runtime, Some("key_fetch_once"), &[])
            .await
            .unwrap();
        assert!(second.is_some());
        assert_eq!(
            second.unwrap().common.key_id,
            Some("key_fetch_once".to_string())
        );
        assert_eq!(*call_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn bootstrap_caches_all_service_key_providers() {
        let response_body = r#"{"keys":[{
            "kty": "RSA",
            "kid": "key_boot",
            "n": "u1WabeaJBoGl",
            "e": "AQAB"
        }]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let (server_url, call_count) = spawn_mock_jwks_server(response).await;

        let mut client = ClientTokenConfig::default();
        client.oauth.multiple_auth_servers = true;
        client.oauth.token.key.server_url = Some(server_url.clone());
        client.oauth.token.key.uri = "/keys".to_string();
        client.oauth.token.key.service_id_auth_servers.insert(
            "service_boot1".to_string(),
            AuthServerConfig {
                server_url: Some(server_url.clone()),
                ..AuthServerConfig::default()
            },
        );
        client.oauth.token.key.service_id_auth_servers.insert(
            "service_boot2".to_string(),
            AuthServerConfig {
                server_url: Some(server_url.clone()),
                ..AuthServerConfig::default()
            },
        );

        let mut runtime = test_runtime_with_client(client);
        runtime.config.bootstrap_from_key_service = true;

        runtime.bootstrap().await.expect("bootstrap");

        // The mock server should have been hit for both providers (2 hits)
        assert_eq!(*call_count.lock().unwrap(), 2);

        // Verify keys are cached under serviceId:kid
        {
            let jwks = runtime.jwks.read().await;
            assert!(jwks.contains_key("service_boot1:key_boot"));
            assert!(jwks.contains_key("service_boot2:key_boot"));
        }

        // Subsequent requests with kid & serviceId must hit the cache directly
        let found =
            cached_jwk_for_services(&runtime, Some("key_boot"), &["service_boot1".to_string()])
                .await;
        assert!(found.is_some());
        assert_eq!(found.unwrap().common.key_id, Some("key_boot".to_string()));

        let resolved = jwk_for_services(&runtime, Some("key_boot"), &["service_boot1".to_string()])
            .await
            .unwrap();
        assert!(resolved.is_some());
        assert_eq!(*call_count.lock().unwrap(), 2); // No new fetch!
    }

    #[tokio::test]
    async fn bootstrap_caches_single_provider() {
        let response_body = r#"{"keys":[{
            "kty": "RSA",
            "kid": "key_boot_single",
            "n": "u1WabeaJBoGl",
            "e": "AQAB"
        }]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        let (server_url, call_count) = spawn_mock_jwks_server(response).await;

        let mut client = ClientTokenConfig::default();
        client.oauth.multiple_auth_servers = false;
        client.oauth.token.key.server_url = Some(server_url.clone());
        client.oauth.token.key.uri = "/keys".to_string();

        let mut runtime = test_runtime_with_client(client);
        runtime.config.bootstrap_from_key_service = true;

        runtime.bootstrap().await.expect("bootstrap");

        // The mock server should have been hit once
        assert_eq!(*call_count.lock().unwrap(), 1);

        // Verify key is cached under plain kid
        {
            let jwks = runtime.jwks.read().await;
            assert!(jwks.contains_key("key_boot_single"));
        }
    }
}
