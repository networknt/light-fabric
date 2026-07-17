mod access_control;
mod apikey;
mod basic_auth;
mod config_util;
mod correlation;
mod cors;
mod direct_registry;
mod handler;
mod header;
mod mcp;
mod metrics;
mod msal_auth;
mod msal_exchange;
mod pii_tokenization;
mod proxy;
mod rate_limit;
mod resource;
mod router;
mod security;
mod service;
mod spa_auth;
mod stateless_auth;
mod token;
mod unified_security;
mod websocket;

use async_trait::async_trait;
use light_runtime::{
    BoundTransport, ResolvedServerMetadata, RuntimeConfig, RuntimeError, ServerConfig,
    TransportRuntime,
};
use pingora::apps::HttpServerApp;
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::{HttpProxy, ProxyHttp};
use pingora::server::Server;
use pingora::server::configuration::ServerConf;
#[cfg(unix)]
use pingora::server::{RunArgs, ShutdownSignal, ShutdownSignalWatch};
use std::fs::File;
use std::io::BufReader;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::thread::JoinHandle;
#[cfg(unix)]
use tokio::sync::watch;

pub use access_control::{
    ACCESS_CONTROL_CONFIG_NAME, ACCESS_CONTROL_FILE, ACCESS_CONTROL_LEGACY_FILE,
    ACCESS_CONTROL_MODULE_ID, AccessControlConfig, AccessControlResponseFilterError,
    AccessControlRuntime, AccessDecision, RULE_CONFIG_NAME, RULE_FILE, RULE_LEGACY_FILE,
    RULE_MODULE_ID, RuleFileConfig, ToolVisibility, ToolsListAccessControlConfig,
    ToolsListAccessControlMode, ToolsListUnknownRuleFallback, load_access_control_runtime,
};
pub use apikey::{
    APIKEY_CONFIG_NAME, APIKEY_FILE, APIKEY_MODULE_ID, ApiKeyConfig, ApiKeyRule,
    load_api_key_config, verify_api_key, verify_required_api_key,
};
pub use basic_auth::{
    BASIC_AUTH_CONFIG_NAME, BASIC_AUTH_FILE, BASIC_AUTH_MODULE_ID, BasicAuthConfig, UserAuth,
    load_basic_auth_config, verify_basic_auth,
};
pub use correlation::{
    CORRELATION_CONFIG_NAME, CORRELATION_FILE, CORRELATION_ID_HEADER, CORRELATION_MODULE_ID,
    CorrelationConfig, CorrelationState, TRACEABILITY_ID_HEADER, apply_correlation_request,
    apply_correlation_response, correlation_id_for_upstream, load_correlation_config,
};
pub use cors::{
    CORS_CONFIG_NAME, CORS_FILE, CORS_MODULE_ID, CorsConfig, CorsRequestOutcome,
    CorsResponseHeaders, apply_cors_response, evaluate_cors_request, load_cors_config,
};
pub use handler::{
    ActiveHandlerSet, HANDLER_CONFIG_NAME, HANDLER_FILE, HANDLER_LEGACY_FILE, HANDLER_MODULE_ID,
    HandlerBuildContext, HandlerChain, HandlerConfig, HandlerMetricsLogLevel, HandlerModuleConfig,
    HandlerPath, PathMatch, PingoraHandler, PingoraHandlerDescriptor, PingoraHandlerFactory,
    PingoraHandlerKind, PingoraHandlerRegistry, ResolvedHandlerChain, load_active_handlers,
};
pub use header::{
    HEADER_CONFIG_NAME, HEADER_FILE, HEADER_MODULE_ID, HeaderConfig, HeaderMutation,
    HeaderPathPrefixConfig, apply_header_request, apply_header_response, load_header_config,
};
pub use mcp::{
    MCP_ROUTER_CONFIG_NAME, MCP_ROUTER_FILE, MCP_ROUTER_LEGACY_FILE, MCP_ROUTER_MODULE_ID,
    MCP_SESSION_ID_HEADER, McpDiscoveryResolver, McpHttpMethod, McpHttpRequest, McpHttpResponse,
    McpLegacyProtocolConfig, McpProtocolsConfig, McpRequestContext, McpRouterConfig,
    McpRouterRuntime, McpStatelessProtocolConfig, McpToolConfig, McpToolType,
    load_mcp_router_runtime,
};
pub use metrics::{
    METRICS_CONFIG_NAME, METRICS_FILE, METRICS_MODULE_ID, MetricCounts, MetricsConfig,
    MetricsEvent, MetricsRecorder, build_metrics_event, classify_status, load_metrics_config,
};
pub use msal_auth::{
    MSAL_AUTH_CONFIG_NAME, MSAL_AUTH_FILE, MSAL_AUTH_MODULE_ID, MsalAuthConfig, MsalAuthRuntime,
    load_msal_auth_config, load_msal_auth_runtime,
};
pub use msal_exchange::{
    MSAL_EXCHANGE_CONFIG_NAME, MSAL_EXCHANGE_FILE, MSAL_EXCHANGE_LEGACY_FILE,
    MSAL_EXCHANGE_MODULE_ID, MsalAuthorizationToken, MsalExchangeConfig, MsalExchangeOutcome,
    MsalExchangeRuntime, SECURITY_MSAL_CONFIG_NAME, SECURITY_MSAL_FILE, SECURITY_MSAL_MODULE_ID,
    load_msal_exchange_runtime,
};
pub use pii_tokenization::{
    PII_TOKENIZATION_CACHE_NAME, PII_TOKENIZATION_CONFIG_NAME, PII_TOKENIZATION_FILE,
    PII_TOKENIZATION_LEGACY_FILE, PII_TOKENIZATION_MODULE_ID, PiiDatabaseConfig, PiiFieldRule,
    PiiTokenCacheConfig, PiiTokenCryptoConfig, PiiTokenizationConfig, PiiTokenizationRule,
    PiiTokenizationRuntime, TokenScheme, load_pii_tokenization_runtime,
};
pub use proxy::{
    PROXY_CONFIG_NAME, PROXY_FILE, PROXY_MODULE_ID, ProxyConfig, ProxyRoute, ProxyTarget,
    load_proxy_route, parse_proxy_targets,
};
pub use rate_limit::{
    LIMIT_CONFIG_NAME, LIMIT_FILE, LIMIT_MODULE_ID, LimitConfig, LimitKey, LimitQuota,
    RateLimitHeaders, RateLimitRuntime, apply_rate_limit_headers, check_rate_limit,
    load_rate_limit_runtime,
};
pub use resource::{
    PATH_RESOURCE_CONFIG_NAME, PATH_RESOURCE_FILE, PATH_RESOURCE_LEGACY_FILE,
    PATH_RESOURCE_MODULE_ID, PathResourceConfig, StaticFile, StaticResolution, StaticResourceSet,
    StaticSite, VIRTUAL_HOST_CONFIG_NAME, VIRTUAL_HOST_FILE, VIRTUAL_HOST_LEGACY_FILE,
    VIRTUAL_HOST_MODULE_ID, VirtualHost, VirtualHostConfig, load_static_resources,
};
pub use router::{
    MethodRewriteRule, QueryHeaderRewriteRule, ROUTER_CONFIG_NAME, ROUTER_FILE, ROUTER_MODULE_ID,
    RouterConfig, RouterDecision, RouterRoute, UrlRewriteRule, apply_router_upstream_request,
    load_router_route, select_router_target,
};
pub use security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SECURITY_CONFIG_NAME, SECURITY_FILE,
    SECURITY_MODULE_ID, SecurityConfig, SecurityJwtConfig, SecurityRuntime, load_security_runtime,
    load_security_runtime_from_file, verify_jwt_request,
    verify_jwt_request_with_service_id_override, verify_jwt_request_with_service_ids,
    verify_jwt_token,
};
pub use service::{
    PATH_PREFIX_SERVICE_CONFIG_NAME, PATH_PREFIX_SERVICE_FILE, PATH_PREFIX_SERVICE_LEGACY_FILE,
    PATH_PREFIX_SERVICE_MODULE_ID, PathPrefixServiceConfig, apply_path_prefix_service,
    load_path_prefix_service_config, service_id_for_path,
};
pub use spa_auth::{
    ACCESS_TOKEN_COOKIE, CSRF_COOKIE, CookieSameSite, EID_COOKIE, EMAIL_COOKIE, HOST_COOKIE,
    REFRESH_TOKEN_COOKIE, ROLES_COOKIE, SpaAuthResponse, SpaCookieConfig, SpaSessionOutcome,
    SpaSessionRuntime, SpaTokenClient, TokenGrantResponse, USER_ID_COOKIE, USER_TYPE_COOKIE,
    generate_csrf, load_spa_token_client, merge_extra_response_headers,
};
pub use stateless_auth::{
    STATELESS_AUTH_CONFIG_NAME, STATELESS_AUTH_FILE, STATELESS_AUTH_LEGACY_FILE,
    STATELESS_AUTH_MODULE_ID, StatelessAuthConfig, StatelessAuthOutcome, StatelessAuthRuntime,
    load_stateless_auth_runtime,
};
pub use token::{
    CLIENT_FILE, CLIENT_TOKEN_CONFIG_NAME, CLIENT_TOKEN_MODULE_ID, ClientOauthConfig,
    ClientRequestConfig, ClientTlsConfig, ClientTokenConfig, OAuthAuthorizationCodeConfig,
    OAuthClientCredentialsConfig, OAuthRefreshTokenConfig, OAuthTokenCacheConfig, OAuthTokenConfig,
    OAuthTokenExchangeConfig, SCOPE_TOKEN_HEADER, SIDECAR_CONFIG_NAME, SIDECAR_FILE,
    SIDECAR_LEGACY_FILE, SIDECAR_MODULE_ID, SidecarTrafficConfig, TOKEN_CACHE_NAME,
    TOKEN_CONFIG_NAME, TOKEN_FILE, TOKEN_LEGACY_FILE, TOKEN_MODULE_ID, TokenHandlerConfig,
    TokenRuntime, apply_token_request, load_token_runtime,
};
pub use unified_security::{
    UNIFIED_SECURITY_CONFIG_NAME, UNIFIED_SECURITY_FILE, UNIFIED_SECURITY_MODULE_ID,
    UnifiedPathAuth, UnifiedSecurityConfig, load_unified_security_config, verify_unified_security,
};
pub use websocket::{
    CONTROLLER_MCP_CONNECT_ENDPOINT, CONTROLLER_MCP_PATH, WEBSOCKET_ROUTER_CONFIG_NAME,
    WEBSOCKET_ROUTER_FILE, WEBSOCKET_ROUTER_LEGACY_FILE, WEBSOCKET_ROUTER_MODULE_ID,
    WebSocketConnectionPermit, WebSocketDiscoveryResolver, WebSocketHandshake,
    WebSocketRouteDecision, WebSocketRouteError, WebSocketRouteSource, WebSocketRouterConfig,
    WebSocketRouterRuntime, WebSocketServiceTarget, apply_browser_websocket_upstream_credentials,
    apply_websocket_upstream_request, load_websocket_router_runtime,
    load_websocket_router_runtime_with_policy,
};

pub trait PingoraApp: Send + Sync + 'static {
    type Proxy: ProxyHttp + Send + Sync + 'static;

    fn proxy(&self, config: &RuntimeConfig) -> Result<Self::Proxy, RuntimeError>;
}

pub struct PingoraTransport<A>
where
    A: PingoraApp,
{
    app: A,
}

impl<A> PingoraTransport<A>
where
    A: PingoraApp,
{
    pub fn new(app: A) -> Self {
        Self { app }
    }
}

pub struct PingoraBoundHandle {
    #[cfg(unix)]
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

#[async_trait]
impl<A> TransportRuntime for PingoraTransport<A>
where
    A: PingoraApp,
    <A::Proxy as ProxyHttp>::CTX: Send + Sync,
    HttpProxy<A::Proxy>: HttpServerApp,
{
    type Handle = PingoraBoundHandle;

    async fn bind(
        &self,
        config: &RuntimeConfig,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        if config.server.dynamic_port {
            return Err(RuntimeError::Unsupported(
                "light-pingora does not support server.dynamicPort yet".to_string(),
            ));
        }
        if !config.server.enable_http && !config.server.enable_https {
            return Err(RuntimeError::Unsupported(
                "server must enable HTTP or HTTPS".to_string(),
            ));
        }

        let proxy = self.app.proxy(config)?;
        let mut server_conf = ServerConf::default();
        server_conf.threads = 1;
        server_conf.daemon = false;
        apply_client_request_config(config, &mut server_conf);
        server_conf.ca_file = upstream_ca_file(config)?;
        let shutdown_seconds = config.server.shutdown_graceful_period.div_ceil(1000);
        server_conf.grace_period_seconds = Some(0);
        server_conf.graceful_shutdown_timeout_seconds = Some(shutdown_seconds);

        let mut server = Server::new_with_opt_and_conf(None, server_conf);
        server.bootstrap();

        let mut service = pingora::proxy::http_proxy_service(&server.configuration, proxy);
        if config.server.enable_http {
            service.add_tcp(&listen_addr(config, config.server.http_port)?);
        }
        if let Some((cert_path, key_path)) = https_listener_tls_paths(&config.server)? {
            validate_https_listener_tls(&cert_path, &key_path)?;
            let mut tls = TlsSettings::intermediate(&cert_path, &key_path)
                .map_err(|e| RuntimeError::Unsupported(format!("invalid TLS config: {e}")))?;
            tls.enable_h2();
            service.add_tls_with_settings(
                &listen_addr(config, config.server.https_port)?,
                None,
                tls,
            );
        }
        server.add_service(service);

        #[cfg(unix)]
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = std::thread::Builder::new()
            .name("light-pingora".to_string())
            .spawn(move || {
                #[cfg(unix)]
                server.run(RunArgs {
                    shutdown_signal: Box::new(ControlledShutdown::new(shutdown_rx)),
                });
                #[cfg(not(unix))]
                server.run(Default::default());
            })
            .map_err(RuntimeError::Io)?;

        let metadata_port = if config.server.enable_https {
            config.server.https_port
        } else {
            config.server.http_port
        };
        let metadata = ResolvedServerMetadata {
            protocol: if config.server.enable_https {
                "https".to_string()
            } else {
                "http".to_string()
            },
            address: resolve_advertised_address(config)?,
            port: metadata_port,
            tags: Default::default(),
        };

        Ok(BoundTransport {
            handle: PingoraBoundHandle {
                #[cfg(unix)]
                shutdown: shutdown_tx,
                task: Some(task),
            },
            metadata,
        })
    }

    async fn stop(&self, handle: &mut Self::Handle) -> Result<(), RuntimeError> {
        #[cfg(unix)]
        {
            let _ = handle.shutdown.send(true);
        }
        if let Some(task) = handle.task.take() {
            tokio::task::spawn_blocking(move || task.join())
                .await
                .map_err(|e| RuntimeError::Unsupported(format!("pingora join failed: {e}")))?
                .map_err(|_| RuntimeError::Unsupported("pingora server panicked".to_string()))?;
        }
        Ok(())
    }
}

fn https_listener_tls_paths(
    server: &ServerConfig,
) -> Result<Option<(String, String)>, RuntimeError> {
    if !server.enable_https {
        return Ok(None);
    }

    let cert_path = required_non_empty_path(server.tls_cert_path.clone(), "server.tlsCertPath")?;
    let key_path = required_non_empty_path(server.tls_key_path.clone(), "server.tlsKeyPath")?;

    Ok(Some((
        cert_path.to_string_lossy().to_string(),
        key_path.to_string_lossy().to_string(),
    )))
}

fn validate_https_listener_tls(cert_path: &str, key_path: &str) -> Result<(), RuntimeError> {
    let cert_file = File::open(cert_path).map_err(|source| {
        RuntimeError::Unsupported(format!(
            "failed to read server.tlsCertPath `{cert_path}`: {source}"
        ))
    })?;
    let mut cert_reader = BufReader::new(cert_file);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| {
            RuntimeError::Unsupported(format!(
                "failed to parse server.tlsCertPath `{cert_path}`: {source}"
            ))
        })?;
    if certs.is_empty() {
        return Err(RuntimeError::Unsupported(format!(
            "server.tlsCertPath `{cert_path}` contains no certificates"
        )));
    }

    let key_file = File::open(key_path).map_err(|source| {
        RuntimeError::Unsupported(format!(
            "failed to read server.tlsKeyPath `{key_path}`: {source}"
        ))
    })?;
    let mut key_reader = BufReader::new(key_file);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|source| {
            RuntimeError::Unsupported(format!(
                "failed to parse server.tlsKeyPath `{key_path}`: {source}"
            ))
        })?
        .ok_or_else(|| {
            RuntimeError::Unsupported(format!(
                "server.tlsKeyPath `{key_path}` contains no private key"
            ))
        })?;

    // Preflight the same cert/key consistency check Pingora's Rustls listener
    // performs later. This temporary config does not define listener TLS policy.
    let builder = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])
    .map_err(|source| {
        RuntimeError::Unsupported(format!(
            "invalid server TLS protocol configuration: {source}"
        ))
    })?;

    builder.with_no_client_auth().with_single_cert(certs, key).map(|_| ()).map_err(|source| {
        RuntimeError::Unsupported(format!(
            "invalid server TLS certificate/key pair cert=`{cert_path}` key=`{key_path}`: {source}"
        ))
    })
}

fn required_non_empty_path(
    path: Option<PathBuf>,
    field: &'static str,
) -> Result<PathBuf, RuntimeError> {
    path.filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            RuntimeError::Unsupported(format!("https is enabled but {field} is missing or empty"))
        })
}

fn listen_addr(config: &RuntimeConfig, port: u16) -> Result<String, RuntimeError> {
    let ip = config.server.ip.parse::<IpAddr>().map_err(|e| {
        RuntimeError::Unsupported(format!("invalid server.ip `{}`: {e}", config.server.ip))
    })?;
    let addr = SocketAddr::new(ip, port);
    Ok(addr.to_string())
}

fn upstream_ca_file(config: &RuntimeConfig) -> Result<Option<String>, RuntimeError> {
    let Some(path) = config
        .client
        .as_ref()
        .and_then(|client| client.tls.ca_cert_path.clone())
        .or_else(|| resolved_string(config, "client.caCertPath").map(PathBuf::from))
        .or_else(|| config.bootstrap.bootstrap_ca_cert_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return Ok(None);
    };

    if !path.exists() {
        return Err(RuntimeError::Unsupported(format!(
            "upstream CA bundle `{}` does not exist",
            path.display()
        )));
    }

    let certificates = light_client::load_ca_cert_bundle(&path).map_err(|e| {
        RuntimeError::Unsupported(format!(
            "invalid upstream CA bundle `{}`: {e}",
            path.display()
        ))
    })?;
    tracing::info!(
        ca_cert_path = %path.display(),
        ca_cert_count = certificates.len(),
        "validated upstream CA certificate bundle"
    );

    Ok(Some(path.to_string_lossy().to_string()))
}

fn apply_client_request_config(config: &RuntimeConfig, server_conf: &mut ServerConf) {
    let Some(client) = config.client.as_ref() else {
        return;
    };
    server_conf.upstream_keepalive_pool_size = client.request.connection_pool_size as usize;
    server_conf.max_retries = client.request.max_request_retry as usize;
}

fn resolved_string(config: &RuntimeConfig, key: &str) -> Option<String> {
    let value = config.resolved_values.get(key)?;
    match value {
        serde_yaml::Value::String(value) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        _ => None,
    }
}

fn resolve_advertised_address(config: &RuntimeConfig) -> Result<String, RuntimeError> {
    if let Some(address) = config.server.advertised_address.as_deref() {
        let trimmed = address.trim();
        if trimmed.is_empty() {
            return Err(RuntimeError::Unsupported(
                "server.advertisedAddress must not be empty when provided".to_string(),
            ));
        }
        return Ok(trimmed.to_string());
    }

    let ip: IpAddr = config
        .server
        .ip
        .parse()
        .map_err(|e| RuntimeError::Unsupported(format!("invalid server.ip: {e}")))?;
    Ok(ip.to_string())
}

#[cfg(unix)]
struct ControlledShutdown {
    receiver: tokio::sync::Mutex<watch::Receiver<bool>>,
}

#[cfg(unix)]
impl ControlledShutdown {
    fn new(receiver: watch::Receiver<bool>) -> Self {
        Self {
            receiver: tokio::sync::Mutex::new(receiver),
        }
    }
}

#[cfg(unix)]
#[async_trait]
impl ShutdownSignalWatch for ControlledShutdown {
    async fn recv(&self) -> ShutdownSignal {
        let mut receiver = self.receiver.lock().await;
        while !*receiver.borrow() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
        ShutdownSignal::GracefulTerminate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, PortalRegistryConfig,
        ServiceIdentity,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::sync::Arc;

    const TEST_CERT_1: &str = r#"-----BEGIN CERTIFICATE-----
MIIDCTCCAfGgAwIBAgIUKbn3AHRPATPzdLlwrGlzg/gN2powDQYJKoZIhvcNAQEL
BQAwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MDcwNzE0NTExM1oXDTI2MDcw
ODE0NTExM1owFDESMBAGA1UEAwwJbG9jYWxob3N0MIIBIjANBgkqhkiG9w0BAQEF
AAOCAQ8AMIIBCgKCAQEAjZO9WLGi3Mcb1CW6VeV3fhulxYxPWIwB86BsM45CY5xv
Uksa8UOXeaU/vXOWRs5O4lWLj5IPhQz3s0lvznzDBYTy6Dw1MahH4T9oqTWAsWK2
4g8aJLZCSYCkfEOxliAOszYLmJGd8G7n0kYLY2PmtfVNC9e1bzpKABCax/8F7R0a
ef3ZxubjJzQnZmqlEBFh3Ge1RSNwqGlORVNC0VYqTb4lE2ud/OoMoK2akeSZOsPT
+0wXPE+hRphLXXGjdnGOh5bCk4hzq3ZznQ8OAzi76RV80UJmQ9h2uXSE/QFrweXo
vmqu3uyE9oNYQY6GIimqE6iv7Kisy1PAQwgulCaWRwIDAQABo1MwUTAdBgNVHQ4E
FgQUlrniLoesAvzVg+7g8gzIKXrEmrswHwYDVR0jBBgwFoAUlrniLoesAvzVg+7g
8gzIKXrEmrswDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOCAQEAdZ4t
XGoY8Cik5OdcEpKDmWWNe26HIPGmcGMiqrHTarkh0JqXKd8+IdnqI1hxVvfIsUFU
yH11pZ5sEJNoMRRdWRgcw2764M/FRBHpHv8c8GXJ1fkoYESblAkyOnJrhAkwVQ39
8seMKnriIjH7+VnkDMZhJDdLTNAnLdwSlVDeTbmT/KwsPUtGPax20VqHn5Gp5eY8
LJVBrDowPwVRSx4sOYF2N74ETd9IUha+FMugv/aYs+b/xhPDyf6ELMUcKGVs6PcI
2y3mjK5e7xzXtjRgw0xsQvzbCT8jzPIBJ2Uhhox9TjwhDe4BRaUcUoJJINPlrN4o
TqQCN+AqCe94f2E2tg==
-----END CERTIFICATE-----
"#;

    const TEST_KEY_1: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCNk71YsaLcxxvU
JbpV5Xd+G6XFjE9YjAHzoGwzjkJjnG9SSxrxQ5d5pT+9c5ZGzk7iVYuPkg+FDPez
SW/OfMMFhPLoPDUxqEfhP2ipNYCxYrbiDxoktkJJgKR8Q7GWIA6zNguYkZ3wbufS
RgtjY+a19U0L17VvOkoAEJrH/wXtHRp5/dnG5uMnNCdmaqUQEWHcZ7VFI3CoaU5F
U0LRVipNviUTa5386gygrZqR5Jk6w9P7TBc8T6FGmEtdcaN2cY6HlsKTiHOrdnOd
Dw4DOLvpFXzRQmZD2Ha5dIT9AWvB5ei+aq7e7IT2g1hBjoYiKaoTqK/sqKzLU8BD
CC6UJpZHAgMBAAECggEAFMFM59zFexz2wnIe0ArfgAL+RBNev89pVdWgfH+60dmU
JE9TDX2S45111l3vRkVmdnMuDjJ5HftEW5SaKdOhJNpUYKFloaMBmU7va/xQ99r4
jp7CKWb4GXMc1K7OhlYAHFusSttGI9ejxUV0KUedrJ01L1WZ3qAqoGR5ccpm4azD
uo3EmuBXEaeJnF7rzC6+wNImwzqTeOdXl8v4LO1f2fe8DwCiHVFZkJOB99XX790Q
JjV7BwqstFbsRE9z1G+qIB7WHMVASctcXD6nXJI2YrIOifjAl99KFu8YWqguJvcH
inUAmesQh5m32B2JAXqyknnS8qnYY8daj/9ebyYgkQKBgQDEZsl6eUegPSTYi4zm
3yxOK7Ra1r3kXTKSN9T4NhwKCUYq9fMLDwruq4eniJTE64w+JdNJKoVpK546GGo4
hKSLV2BTgjNDhoPnEPyKKU3J+xZFM0AqOnF3RJ/se72CCDHpRFl0Ur1LcVJL9NVH
iqkoHoYqL5RyiJr95F33IP7xmQKBgQC4ifxAu4/8dIUMA1OL7RgxgA67O5DmLrwd
9kCgOIJOL6SbektBgtVivX4zv7aiIE/3KYe6nQLY7Xw9nDhckWAyC6f7PeabrxPe
kDO1OmBj4rSM9i3vh7Iadb/N/SSpvLECUuSa4bUR/w+dLxoCKODUUFcge2UPM+wv
RN14h9xy3wKBgAQaRZEqYWWmgVOIrrvP46QKY60WGUdg7wKA6hD5SGKpSO7yzk3n
1YmgyaelQb5PUVGnBp/bpIfK4nZCNk3R74H9pER6TsnVUIIOJ8hXDonulcuCQ4/e
QqqEI3cUKqRBuZEu3VOBuvSNfHObvKzO57Ov14ugDNDLq7ksAQ59gPXZAoGAVm5S
VmNCygQs+HZqYAQZK74FqE36zMSg2QuoMyKkbUhFOYjqzHEhzlBgVo55VK/7pBCw
gIffeIiqgxSzFTAFtQrej37rjolOrhQuE7iWwtHArLD0zNZqZZg20Jy62kEFSshW
R/Bk5VvoDT+tV8ubmfVTCWSh7Z/tBCql7Dj92FMCgYBZBLaiRHmB4yTIFyTfxAXW
Oqwwuywl1dVDFhUtzmDprSf6k/hyDxCl+lNcpLp2DnrTWCra0SmY9LBMQWND6l7F
xRqItVSXlV9A/bWJjdF487EAE1wVPpESsc/jvb5JN/ddhxydMu40H+3SAnvLzPfZ
PbCnOBiT3odV8W7MMwf/4A==
-----END PRIVATE KEY-----
"#;

    const TEST_KEY_2: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCz92FXZOAlYJYP
Q0VZt3TRHHEMr+ZRcLc1ldNJH9RAJaeO4tWkREShSVhM88LbjTQ8ZMVaNRVozEXg
W3IjO0qJJZMbOQCMp3bjINq4RauTK3SIX6UCCmOW5Y/FXCna63bdTaZkl6KDBe2j
gTYLBFO4sbDaS1S8WGuiQAaMBV6eWyfu4I38/dXhpA25DcDpIfFeg7ztV7aGiFvx
LKa7tITTPNMLmQN3r0kwceXOb1rQw+DNvxySWM4SJrR7vkXkqJh6tFZaVZ6UQYti
K6jmRX9f9uB1HkdRpuJeCIJQKvOr28qQ/x3HJCrMzJN4ys+qZoskW/87yxu1Bgbw
LItjIvxpAgMBAAECggEAAiTxEBpjuNJKK2+i4ocm8UxoVO0+HmuMRUtOF42VaPfB
47gUcVb+ZdkSwCT55gWMUSlmuBTQlt1zOjGAvkZ5NIHh+zWuSd6/cgSc0owC97eR
dYQFOm1fAyfkUwbOeV0rnwarNEDhxvOhwZxbJV21dSqJ18oEvhNEIgxm/5FbT6Hz
f+xt3bE3riPC8lUFX6iRywddX/yyFf677aUH4cgJcqh8dYKvodPvp/s0jTDkNras
04MC3csjkSNUfwodNzRvSBdxmnX1zV4TmYy8iJmEMNn7h8K50EsqLR6UWBA9jK8+
9+ynASobMl/uHVIPs+vKCcrNBBMcwM0jZg+R5dgUwQKBgQD4jfM54wwiTjLeuSy/
jN8mjYZnRITF7F15U+gGCzh9qn2g30jjWf7D4tI9JhHRffR4xWY7Tn7uLLG7ZsIc
8NZgt8p1di3Wvdc+aJq1AWQRE7EO9iCWAlfA85WgkcI5EaWkXIBHVjuP04cAdhnn
qXJ1OOJ9YoRBfjhfoADxuSAmKQKBgQC5W3WIMAu5cR7JQd3nxW7xeA2NmrQuYMXl
PF1UZ+UlyfuFR+08WK2I0EVzqft2Wz9Zi+d8+EhmyuUXHzGYhHiZh788mBOISypC
m0jiFKWBWxdtWA7rsw+KOwiNMmUV7gABg2x2CPKmX09+rzLi2JdSLJplFLI8D5SV
1N9uvuBsQQKBgQDtVBUnc81VQFfAZQ3+RNOaa04ncrxYhE3omJ6WjsY877sPDcT6
GSdzATR/4MbowpzZaJsqC9SVNSXr671zht8b8MInkFVKk3BgDd+S76YNzECnKYqJ
0ejau3tmm2bZuSjxnMV72DH9Lhvc6+fmVNyOY2eYE6Z3Jr9LR2s/Y+X3qQKBgElW
YHhT2i+zDCVBBFWRjkXH5ETksumurF34tkyRFt8OvY+MV9cKlw6MqQ4McUvw6m25
pwuRCMRy/pVZaDwaHcVRKl8FJKVGaCAWZI3e8WTu76P5tV2YaUud89I54Dj/A82V
fDJvc+JTz5YmJ5INdEG1GBlqSOLunzFxGj4tE4qBAoGBAMHC7kKGBIAQyjfciHm4
XcD1un6e3h5ch0RyHRAEicZiDXAjSzcCX1XcvUPBy/o4npyRzj5iCFLPCQREq7Ug
Les3kFc19cancBPV2fNXTbpZhSpuCADyjkaG/UA5cXAhm+hCeuRCtD+z96WSPa0D
iSPqLa2C/InN2hYeU+v8gzdT
-----END PRIVATE KEY-----
"#;

    fn server_config_with_https(
        tls_cert_path: Option<PathBuf>,
        tls_key_path: Option<PathBuf>,
    ) -> ServerConfig {
        ServerConfig {
            enable_https: true,
            tls_cert_path,
            tls_key_path,
            ..Default::default()
        }
    }

    fn unsupported_message(error: RuntimeError) -> String {
        match error {
            RuntimeError::Unsupported(message) => message,
            other => panic!("expected RuntimeError::Unsupported, got {other:?}"),
        }
    }

    fn runtime_config_with_ip(ip: &str) -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig {
                ip: ip.to_string(),
                ..Default::default()
            },
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config"),
            resolved_values: HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn https_listener_tls_paths_returns_none_when_https_disabled() {
        let server = ServerConfig::default();

        let paths = https_listener_tls_paths(&server).expect("https disabled");

        assert_eq!(paths, None);
    }

    #[test]
    fn https_listener_tls_paths_rejects_missing_cert_path() {
        let server = server_config_with_https(None, Some(PathBuf::from("server-key.pem")));

        let error = https_listener_tls_paths(&server).expect_err("missing cert path");

        assert_eq!(
            unsupported_message(error),
            "https is enabled but server.tlsCertPath is missing or empty"
        );
    }

    #[test]
    fn https_listener_tls_paths_rejects_empty_cert_path() {
        let server =
            server_config_with_https(Some(PathBuf::new()), Some(PathBuf::from("server-key.pem")));

        let error = https_listener_tls_paths(&server).expect_err("empty cert path");

        assert_eq!(
            unsupported_message(error),
            "https is enabled but server.tlsCertPath is missing or empty"
        );
    }

    #[test]
    fn https_listener_tls_paths_rejects_empty_key_path() {
        let server =
            server_config_with_https(Some(PathBuf::from("server.pem")), Some(PathBuf::new()));

        let error = https_listener_tls_paths(&server).expect_err("empty key path");

        assert_eq!(
            unsupported_message(error),
            "https is enabled but server.tlsKeyPath is missing or empty"
        );
    }

    #[test]
    fn https_listener_tls_paths_accepts_non_empty_paths() {
        let server = server_config_with_https(
            Some(PathBuf::from("server.pem")),
            Some(PathBuf::from("server-key.pem")),
        );

        let paths = https_listener_tls_paths(&server).expect("non-empty paths");

        assert_eq!(
            paths,
            Some(("server.pem".to_string(), "server-key.pem".to_string()))
        );
    }

    #[test]
    fn validate_https_listener_tls_accepts_matching_cert_and_key() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cert_path = temp_dir.path().join("server.pem");
        let key_path = temp_dir.path().join("server-key.pem");
        fs::write(&cert_path, TEST_CERT_1).expect("write cert");
        fs::write(&key_path, TEST_KEY_1).expect("write key");

        validate_https_listener_tls(
            cert_path.to_str().expect("cert path"),
            key_path.to_str().expect("key path"),
        )
        .expect("valid tls pair");
    }

    #[test]
    fn validate_https_listener_tls_rejects_mismatched_cert_and_key() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cert_path = temp_dir.path().join("server.pem");
        let key_path = temp_dir.path().join("server-key.pem");
        fs::write(&cert_path, TEST_CERT_1).expect("write cert");
        fs::write(&key_path, TEST_KEY_2).expect("write key");

        let error = validate_https_listener_tls(
            cert_path.to_str().expect("cert path"),
            key_path.to_str().expect("key path"),
        )
        .expect_err("mismatched tls pair");

        let message = unsupported_message(error);
        assert!(message.contains("invalid server TLS certificate/key pair"));
    }

    #[test]
    fn validate_https_listener_tls_rejects_empty_cert_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let cert_path = temp_dir.path().join("server.pem");
        let key_path = temp_dir.path().join("server-key.pem");
        fs::write(&cert_path, "").expect("write cert");
        fs::write(&key_path, TEST_KEY_1).expect("write key");

        let error = validate_https_listener_tls(
            cert_path.to_str().expect("cert path"),
            key_path.to_str().expect("key path"),
        )
        .expect_err("empty cert file");

        assert_eq!(
            unsupported_message(error),
            format!(
                "server.tlsCertPath `{}` contains no certificates",
                cert_path.display()
            )
        );
    }

    #[test]
    fn listen_addr_builds_ipv4_bind_address() {
        let config = runtime_config_with_ip("0.0.0.0");

        let address = listen_addr(&config, 8443).expect("ipv4 bind address");

        assert_eq!(address, "0.0.0.0:8443");
    }

    #[test]
    fn listen_addr_builds_ipv6_bind_address() {
        let config = runtime_config_with_ip("::");

        let address = listen_addr(&config, 8443).expect("ipv6 bind address");

        assert_eq!(address, "[::]:8443");
    }

    #[test]
    fn listen_addr_rejects_invalid_bind_ip() {
        let config = runtime_config_with_ip("not an ip");

        let error = listen_addr(&config, 8443).expect_err("invalid bind ip");

        assert_eq!(
            unsupported_message(error),
            "invalid server.ip `not an ip`: invalid IP address syntax"
        );
    }
}
