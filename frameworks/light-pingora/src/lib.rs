mod access_control;
mod apikey;
mod basic_auth;
mod config_util;
mod correlation;
mod cors;
mod handler;
mod header;
mod mcp;
mod metrics;
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
    BoundTransport, ResolvedServerMetadata, RuntimeConfig, RuntimeError, TransportRuntime,
};
use pingora::apps::HttpServerApp;
use pingora::listeners::tls::TlsSettings;
use pingora::proxy::{HttpProxy, ProxyHttp};
use pingora::server::Server;
use pingora::server::configuration::ServerConf;
#[cfg(unix)]
use pingora::server::{RunArgs, ShutdownSignal, ShutdownSignalWatch};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::thread::JoinHandle;
#[cfg(unix)]
use tokio::sync::watch;

pub use access_control::{
    ACCESS_CONTROL_CONFIG_NAME, ACCESS_CONTROL_FILE, ACCESS_CONTROL_LEGACY_FILE,
    ACCESS_CONTROL_MODULE_ID, AccessControlConfig, AccessControlRuntime, AccessDecision,
    RULE_CONFIG_NAME, RULE_FILE, RULE_LEGACY_FILE, RULE_MODULE_ID, RuleFileConfig,
    load_access_control_runtime,
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
    McpDiscoveryResolver, McpHttpMethod, McpHttpRequest, McpHttpResponse, McpRequestContext,
    McpRouterConfig, McpRouterRuntime, McpToolConfig, McpToolType, load_mcp_router_runtime,
};
pub use metrics::{
    METRICS_CONFIG_NAME, METRICS_FILE, METRICS_MODULE_ID, MetricCounts, MetricsConfig,
    MetricsEvent, MetricsRecorder, build_metrics_event, classify_status, load_metrics_config,
};
pub use msal_exchange::{
    MSAL_EXCHANGE_CONFIG_NAME, MSAL_EXCHANGE_FILE, MSAL_EXCHANGE_LEGACY_FILE,
    MSAL_EXCHANGE_MODULE_ID, MsalExchangeConfig, MsalExchangeOutcome, MsalExchangeRuntime,
    SECURITY_MSAL_CONFIG_NAME, SECURITY_MSAL_FILE, SECURITY_MSAL_MODULE_ID,
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
    load_security_runtime_from_file, verify_jwt_request, verify_jwt_token,
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
    WEBSOCKET_ROUTER_CONFIG_NAME, WEBSOCKET_ROUTER_FILE, WEBSOCKET_ROUTER_LEGACY_FILE,
    WEBSOCKET_ROUTER_MODULE_ID, WebSocketConnectionPermit, WebSocketDiscoveryResolver,
    WebSocketRouteDecision, WebSocketRouteError, WebSocketRouteSource, WebSocketRouterConfig,
    WebSocketRouterRuntime, WebSocketServiceTarget, apply_websocket_upstream_request,
    load_websocket_router_runtime,
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
        server_conf.ca_file = upstream_ca_file(config);
        let shutdown_seconds = config.server.shutdown_graceful_period.div_ceil(1000);
        server_conf.grace_period_seconds = Some(0);
        server_conf.graceful_shutdown_timeout_seconds = Some(shutdown_seconds);

        let mut server = Server::new_with_opt_and_conf(None, server_conf);
        server.bootstrap();

        let mut service = pingora::proxy::http_proxy_service(&server.configuration, proxy);
        if config.server.enable_http {
            service.add_tcp(&listen_addr(config, config.server.http_port)?);
        }
        if config.server.enable_https {
            let cert_path = config.server.tls_cert_path.clone().ok_or_else(|| {
                RuntimeError::Unsupported(
                    "https is enabled but server.tlsCertPath is missing".to_string(),
                )
            })?;
            let key_path = config.server.tls_key_path.clone().ok_or_else(|| {
                RuntimeError::Unsupported(
                    "https is enabled but server.tlsKeyPath is missing".to_string(),
                )
            })?;
            let cert_path = cert_path.to_string_lossy().to_string();
            let key_path = key_path.to_string_lossy().to_string();
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

fn listen_addr(config: &RuntimeConfig, port: u16) -> Result<String, RuntimeError> {
    let addr: SocketAddr = format!("{}:{port}", config.server.ip)
        .parse()
        .map_err(|e| RuntimeError::Unsupported(format!("invalid bind address: {e}")))?;
    Ok(addr.to_string())
}

fn upstream_ca_file(config: &RuntimeConfig) -> Option<String> {
    resolved_string(config, "client.caCertPath")
        .map(PathBuf::from)
        .or_else(|| config.bootstrap.bootstrap_ca_cert_path.clone())
        .filter(|path| path.exists())
        .map(|path| path.to_string_lossy().to_string())
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
