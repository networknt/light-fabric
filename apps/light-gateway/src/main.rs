use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use light_pingora::{
    AccessControlRuntime, AccessDecision, ActiveHandlerSet, ApiKeyConfig, AuthPrincipal,
    BasicAuthConfig, CorrelationConfig, CorrelationState, CorsConfig, CorsRequestOutcome,
    CorsResponseHeaders, HandlerBuildContext, HandlerMetricsLogLevel, HandlerRejection,
    HeaderConfig, McpHttpRequest, McpHttpResponse, McpRequestContext, McpRouterRuntime,
    MetricsConfig, MetricsRecorder, MsalExchangeOutcome, MsalExchangeRuntime,
    PathPrefixServiceConfig, PiiTokenizationRuntime, PingoraApp, PingoraHandler,
    PingoraHandlerDescriptor, PingoraHandlerKind, PingoraHandlerRegistry, PingoraTransport,
    ProxyRoute, ProxyTarget, RateLimitHeaders, RateLimitRuntime, RouterDecision, RouterRoute,
    SecurityRuntime, SpaAuthResponse, StatelessAuthOutcome, StatelessAuthRuntime, StaticResolution,
    StaticResourceSet, TokenRuntime, UnifiedSecurityConfig, WebSocketConnectionPermit,
    WebSocketRouteDecision, WebSocketRouteError, WebSocketRouterRuntime, apply_correlation_request,
    apply_correlation_response, apply_cors_response, apply_header_request, apply_header_response,
    apply_path_prefix_service, apply_rate_limit_headers, apply_router_upstream_request,
    apply_token_request, apply_websocket_upstream_request, build_metrics_event, check_rate_limit,
    correlation_id_for_upstream, evaluate_cors_request, load_access_control_runtime,
    load_active_handlers, load_api_key_config, load_basic_auth_config, load_correlation_config,
    load_cors_config, load_header_config, load_mcp_router_runtime, load_metrics_config,
    load_msal_exchange_runtime, load_path_prefix_service_config, load_pii_tokenization_runtime,
    load_proxy_route, load_rate_limit_runtime, load_router_route, load_security_runtime,
    load_stateless_auth_runtime, load_static_resources, load_token_runtime,
    load_unified_security_config, load_websocket_router_runtime, merge_extra_response_headers,
    select_router_target, verify_api_key, verify_basic_auth, verify_jwt_request,
    verify_unified_security,
};
use light_runtime::{
    CacheRegistry, ConfigManager, LightRuntimeBuilder, ReloadContext, ReloadOutcome,
    ReloadableModule, RuntimeConfig, RuntimeError, TracingOptions, init_tracing,
};
use pingora::http::ResponseHeader;
use pingora::prelude::{HttpPeer, ProxyHttp, Session};
use pingora::utils::tls::CertKey;
use pingora::{Error, ErrorType};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::io::BufReader;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};
use url::form_urlencoded;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const HEALTH_PATH: &str = "/health";
const ACCESS_CONTROL_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

#[derive(Clone)]
struct GatewayApp;

impl PingoraApp for GatewayApp {
    type Proxy = GatewayProxy;

    fn proxy(&self, config: &RuntimeConfig) -> Result<Self::Proxy, RuntimeError> {
        GatewayProxy::from_runtime_config(config)
    }
}

struct GatewayProxy {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
    api_key_config: Arc<ConfigManager<Option<ApiKeyConfig>>>,
    basic_auth_config: Arc<ConfigManager<Option<BasicAuthConfig>>>,
    security_runtime: Arc<ConfigManager<Option<SecurityRuntime>>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
    metrics_recorder: Arc<MetricsRecorder>,
    proxy_route: Arc<ConfigManager<Option<ProxyRoute>>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
    static_resources: Arc<ConfigManager<StaticResourceSet>>,
    next_upstream: AtomicUsize,
    upstream_verify_hostname: bool,
    upstream_client_cert_key: Option<Arc<CertKey>>,
    upstream_connect_timeout: Option<Duration>,
    upstream_circuit_error_threshold: u32,
    upstream_circuit_reset_timeout: Duration,
    upstream_circuits: Mutex<BTreeMap<String, UpstreamCircuitState>>,
    server_scheme: String,
    server_port: u16,
}

impl GatewayProxy {
    fn from_runtime_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let active_handlers = load_active_handlers(config, &gateway_handler_registry())?;
        let correlation_config =
            load_correlation_config(config, active_handlers.is_handler_active("correlation"))?;
        let cors_config = load_cors_config(config, active_handlers.is_handler_active("cors"))?;
        let metrics_config =
            load_metrics_config(config, active_handlers.is_handler_active("metrics"))?;
        let header_config = load_header_config(
            config,
            handler_active(&active_handlers, &["header", "headers"]),
        )?;
        let api_key_config = load_api_key_config(
            config,
            handler_active(
                &active_handlers,
                &["api-key", "apikey", "unified-security", "unified"],
            ),
        )?;
        let basic_auth_config = load_basic_auth_config(
            config,
            handler_active(
                &active_handlers,
                &["basic-auth", "basic", "unified-security", "unified"],
            ),
        )?;
        let security_runtime = load_security_runtime(
            config,
            handler_active(
                &active_handlers,
                &["security", "jwt", "unified-security", "unified"],
            ),
        )?;
        let unified_security_config = load_unified_security_config(
            config,
            handler_active(&active_handlers, &["unified-security", "unified"]),
        )?;
        let rate_limit_runtime = load_rate_limit_runtime(
            config,
            handler_active(&active_handlers, &["limit", "rate-limit"]),
        )?;
        let path_prefix_service_config = load_path_prefix_service_config(
            config,
            handler_active(
                &active_handlers,
                &["prefix", "path-prefix-service", "pathPrefixService"],
            ),
        )?;
        let token_runtime = load_token_runtime(config, active_handlers.is_handler_active("token"))?;
        let stateless_auth = load_stateless_auth_runtime(
            config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange =
            load_msal_exchange_runtime(config, active_handlers.is_handler_active("msal-exchange"))?;
        let pii_tokenization = load_pii_tokenization_runtime(
            config,
            handler_active(&active_handlers, &["tokenize", "detokenize"]),
        )?;
        let access_control = load_access_control_runtime(
            config,
            active_handlers.is_handler_active("access-control"),
        )?;
        let mcp_router = load_mcp_router_runtime(config, active_handlers.is_handler_active("mcp"))?;
        let websocket_router =
            load_websocket_router_runtime(config, active_handlers.is_handler_active("websocket"))?;
        let router_route = load_router_route(config, active_handlers.is_handler_active("router"))?;
        let proxy_route = load_proxy_route(config)?;
        let static_resources = load_static_resources(config)?;
        let active_handlers = Arc::new(ConfigManager::new(active_handlers));
        let correlation_config = Arc::new(ConfigManager::new(correlation_config));
        let cors_config = Arc::new(ConfigManager::new(cors_config));
        let metrics_config = Arc::new(ConfigManager::new(metrics_config));
        let header_config = Arc::new(ConfigManager::new(header_config));
        let api_key_config = Arc::new(ConfigManager::new(api_key_config));
        let basic_auth_config = Arc::new(ConfigManager::new(basic_auth_config));
        let security_runtime = Arc::new(ConfigManager::new(security_runtime));
        let unified_security_config = Arc::new(ConfigManager::new(unified_security_config));
        let rate_limit_runtime = Arc::new(ConfigManager::new(rate_limit_runtime));
        let path_prefix_service_config = Arc::new(ConfigManager::new(path_prefix_service_config));
        let token_runtime = Arc::new(ConfigManager::new(token_runtime));
        let stateless_auth = Arc::new(ConfigManager::new(stateless_auth));
        let msal_exchange = Arc::new(ConfigManager::new(msal_exchange));
        let pii_tokenization = Arc::new(ConfigManager::new(pii_tokenization));
        let access_control = Arc::new(ConfigManager::new(access_control));
        let mcp_router = Arc::new(ConfigManager::new(mcp_router));
        let websocket_router = Arc::new(ConfigManager::new(websocket_router));
        let router_route = Arc::new(ConfigManager::new(router_route));
        let proxy_route = Arc::new(ConfigManager::new(proxy_route));
        let static_resources = Arc::new(ConfigManager::new(static_resources));
        let metrics_recorder = Arc::new(MetricsRecorder::default());

        config.module_registry.register_reloader(
            light_pingora::HANDLER_MODULE_ID,
            Arc::new(HandlerReloader {
                active_handlers: Arc::clone(&active_handlers),
                correlation_config: Arc::clone(&correlation_config),
                cors_config: Arc::clone(&cors_config),
                metrics_config: Arc::clone(&metrics_config),
                header_config: Arc::clone(&header_config),
                api_key_config: Arc::clone(&api_key_config),
                basic_auth_config: Arc::clone(&basic_auth_config),
                security_runtime: Arc::clone(&security_runtime),
                unified_security_config: Arc::clone(&unified_security_config),
                rate_limit_runtime: Arc::clone(&rate_limit_runtime),
                path_prefix_service_config: Arc::clone(&path_prefix_service_config),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
                pii_tokenization: Arc::clone(&pii_tokenization),
                access_control: Arc::clone(&access_control),
                mcp_router: Arc::clone(&mcp_router),
                websocket_router: Arc::clone(&websocket_router),
                router_route: Arc::clone(&router_route),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CORRELATION_MODULE_ID,
            Arc::new(CorrelationReloader {
                active_handlers: Arc::clone(&active_handlers),
                correlation_config: Arc::clone(&correlation_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CORS_MODULE_ID,
            Arc::new(CorsReloader {
                active_handlers: Arc::clone(&active_handlers),
                cors_config: Arc::clone(&cors_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::METRICS_MODULE_ID,
            Arc::new(MetricsReloader {
                active_handlers: Arc::clone(&active_handlers),
                metrics_config: Arc::clone(&metrics_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::HEADER_MODULE_ID,
            Arc::new(HeaderReloader {
                active_handlers: Arc::clone(&active_handlers),
                header_config: Arc::clone(&header_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::APIKEY_MODULE_ID,
            Arc::new(ApiKeyReloader {
                active_handlers: Arc::clone(&active_handlers),
                api_key_config: Arc::clone(&api_key_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::BASIC_AUTH_MODULE_ID,
            Arc::new(BasicAuthReloader {
                active_handlers: Arc::clone(&active_handlers),
                basic_auth_config: Arc::clone(&basic_auth_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SECURITY_MODULE_ID,
            Arc::new(SecurityReloader {
                active_handlers: Arc::clone(&active_handlers),
                security_runtime: Arc::clone(&security_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::UNIFIED_SECURITY_MODULE_ID,
            Arc::new(UnifiedSecurityReloader {
                active_handlers: Arc::clone(&active_handlers),
                unified_security_config: Arc::clone(&unified_security_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::LIMIT_MODULE_ID,
            Arc::new(RateLimitReloader {
                active_handlers: Arc::clone(&active_handlers),
                rate_limit_runtime: Arc::clone(&rate_limit_runtime),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::PATH_PREFIX_SERVICE_MODULE_ID,
            Arc::new(PathPrefixServiceReloader {
                active_handlers: Arc::clone(&active_handlers),
                path_prefix_service_config: Arc::clone(&path_prefix_service_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::TOKEN_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CLIENT_TOKEN_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SIDECAR_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::STATELESS_AUTH_MODULE_ID,
            Arc::new(StatelessAuthReloader {
                active_handlers: Arc::clone(&active_handlers),
                stateless_auth: Arc::clone(&stateless_auth),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::MSAL_EXCHANGE_MODULE_ID,
            Arc::new(MsalExchangeReloader {
                active_handlers: Arc::clone(&active_handlers),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SECURITY_MSAL_MODULE_ID,
            Arc::new(MsalExchangeReloader {
                active_handlers: Arc::clone(&active_handlers),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::PII_TOKENIZATION_MODULE_ID,
            Arc::new(PiiTokenizationReloader {
                active_handlers: Arc::clone(&active_handlers),
                pii_tokenization: Arc::clone(&pii_tokenization),
            }),
        );
        let mcp_reloader: Arc<dyn ReloadableModule> = Arc::new(McpRouterReloader {
            active_handlers: Arc::clone(&active_handlers),
            mcp_router: Arc::clone(&mcp_router),
        });
        config.module_registry.register_reloader(
            light_pingora::MCP_ROUTER_MODULE_ID,
            Arc::clone(&mcp_reloader),
        );
        config.module_registry.register_reloader(
            light_pingora::WEBSOCKET_ROUTER_MODULE_ID,
            Arc::new(WebSocketRouterReloader {
                active_handlers: Arc::clone(&active_handlers),
                websocket_router: Arc::clone(&websocket_router),
            }),
        );
        let access_control_reloader: Arc<dyn ReloadableModule> = Arc::new(AccessControlReloader {
            active_handlers: Arc::clone(&active_handlers),
            access_control: Arc::clone(&access_control),
            mcp_router: Arc::clone(&mcp_router),
            websocket_router: Arc::clone(&websocket_router),
        });
        config.module_registry.register_reloader(
            light_pingora::ACCESS_CONTROL_MODULE_ID,
            Arc::clone(&access_control_reloader),
        );
        config
            .module_registry
            .register_reloader(light_pingora::RULE_MODULE_ID, access_control_reloader);
        config.module_registry.register_reloader(
            light_pingora::PROXY_MODULE_ID,
            Arc::new(ProxyReloader {
                proxy_route: Arc::clone(&proxy_route),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::ROUTER_MODULE_ID,
            Arc::new(RouterReloader {
                active_handlers: Arc::clone(&active_handlers),
                router_route: Arc::clone(&router_route),
            }),
        );
        let static_reloader: Arc<dyn ReloadableModule> = Arc::new(StaticResourceReloader {
            static_resources: Arc::clone(&static_resources),
        });
        config.module_registry.register_reloader(
            light_pingora::PATH_RESOURCE_MODULE_ID,
            Arc::clone(&static_reloader),
        );
        config
            .module_registry
            .register_reloader(light_pingora::VIRTUAL_HOST_MODULE_ID, static_reloader);

        let (upstream_circuit_error_threshold, upstream_circuit_reset_timeout) =
            upstream_circuit_config(config);

        Ok(Self {
            active_handlers,
            correlation_config,
            cors_config,
            metrics_config,
            header_config,
            api_key_config,
            basic_auth_config,
            security_runtime,
            unified_security_config,
            rate_limit_runtime,
            path_prefix_service_config,
            token_runtime,
            stateless_auth,
            msal_exchange,
            pii_tokenization,
            access_control,
            mcp_router,
            websocket_router,
            metrics_recorder,
            proxy_route,
            router_route,
            static_resources,
            next_upstream: AtomicUsize::new(0),
            upstream_verify_hostname: upstream_verify_hostname(config),
            upstream_client_cert_key: upstream_client_cert_key(config)?,
            upstream_connect_timeout: upstream_connect_timeout(config),
            upstream_circuit_error_threshold,
            upstream_circuit_reset_timeout,
            upstream_circuits: Mutex::new(BTreeMap::new()),
            server_scheme: if config.server.enable_https {
                "https".to_string()
            } else {
                "http".to_string()
            },
            server_port: if config.server.enable_https {
                config.server.https_port
            } else {
                config.server.http_port
            },
        })
    }

    fn select_upstream(&self) -> Option<(ProxyTarget, bool, bool)> {
        let route = self.proxy_route.load();
        let route = route.as_ref().as_ref()?;
        let mut first_open_target = None;
        for _ in 0..route.targets.len() {
            let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
            let Some(target) = route.select(index) else {
                continue;
            };
            if self.is_upstream_circuit_open(&target) {
                first_open_target.get_or_insert(target);
                continue;
            }
            return Some((
                target,
                route.rewrite_host_header(),
                route.config.reuse_x_forwarded,
            ));
        }
        first_open_target.map(|target| {
            (
                target,
                route.rewrite_host_header(),
                route.config.reuse_x_forwarded,
            )
        })
    }

    fn is_upstream_circuit_open(&self, target: &ProxyTarget) -> bool {
        if self.upstream_circuit_error_threshold == 0 {
            return false;
        }
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(state) = circuits.get_mut(&key) else {
            return false;
        };
        let Some(opened_at) = state.opened_at else {
            return false;
        };
        if opened_at.elapsed() < self.upstream_circuit_reset_timeout {
            return true;
        }
        state.failures = 0;
        state.opened_at = None;
        false
    }

    fn record_upstream_success(&self, ctx: &GatewayRequestContext) {
        if self.upstream_circuit_error_threshold == 0 {
            return;
        }
        let Some(target) = ctx.proxy_target.as_ref() else {
            return;
        };
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        circuits.remove(&key);
    }

    fn record_upstream_failure(&self, ctx: &GatewayRequestContext) {
        if self.upstream_circuit_error_threshold == 0 {
            return;
        }
        let Some(target) = ctx.proxy_target.as_ref() else {
            return;
        };
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = circuits.entry(key).or_default();
        state.failures = state.failures.saturating_add(1);
        if state.failures >= self.upstream_circuit_error_threshold {
            state.opened_at = Some(Instant::now());
        }
    }

    async fn write_static_resolution(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        resolution: StaticResolution,
    ) -> pingora::Result<bool> {
        if !static_method_allowed(session) {
            return self
                .write_bytes_response_with_headers(
                    session,
                    ctx,
                    405,
                    "text/plain; charset=utf-8",
                    None,
                    Bytes::from_static(b"method not allowed"),
                    &[("allow".to_string(), "GET, HEAD".to_string())],
                )
                .await;
        }

        match resolution {
            StaticResolution::File(file) => {
                let metadata = tokio::fs::metadata(&file.path).await.map_err(|error| {
                    Error::because(
                        ErrorType::FileReadError,
                        format!("failed to stat static file `{}`", file.path.display()),
                        error,
                    )
                })?;
                let validators = static_file_validators(&metadata);
                if static_request_not_modified(session, &validators) {
                    return self
                        .write_static_not_modified(session, ctx, &file, &validators)
                        .await;
                }
                if should_stream_static_file(metadata.len(), file.transfer_min_size) {
                    self.write_streaming_static_file(session, ctx, &file, &metadata, &validators)
                        .await
                } else {
                    let body = tokio::fs::read(&file.path).await.map_err(|error| {
                        Error::because(
                            ErrorType::FileReadError,
                            format!("failed to read static file `{}`", file.path.display()),
                            error,
                        )
                    })?;
                    self.write_static_bytes_response(
                        session,
                        ctx,
                        &file,
                        &validators,
                        Bytes::from(body),
                    )
                    .await
                }
            }
            StaticResolution::Forbidden => {
                self.write_text_response(session, ctx, 403, "forbidden")
                    .await
            }
            StaticResolution::NotFound => {
                self.write_text_response(session, ctx, 404, "not found")
                    .await
            }
        }
    }

    async fn write_static_not_modified(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
    ) -> pingora::Result<bool> {
        let mut response = ResponseHeader::build(304, Some(8))?;
        response.insert_header("cache-control", file.cache_control.as_str())?;
        insert_static_validators(&mut response, validators)?;
        self.apply_response_headers(&mut response, ctx)?;
        session
            .write_response_header(Box::new(response), true)
            .await?;
        self.record_metrics(ctx, 304);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_static_bytes_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
        body: Bytes,
    ) -> pingora::Result<bool> {
        let is_head = is_head_request(session);
        let mut response = self.static_response_header(file, validators, body.len() as u64)?;
        self.apply_response_headers(&mut response, ctx)?;
        session
            .write_response_header(Box::new(response), is_head)
            .await?;
        if !is_head {
            session.write_response_body(Some(body), true).await?;
        }
        self.record_metrics(ctx, 200);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_streaming_static_file(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        metadata: &std::fs::Metadata,
        validators: &StaticFileValidators,
    ) -> pingora::Result<bool> {
        let is_head = is_head_request(session);
        let content_length = metadata.len();
        let mut response = self.static_response_header(file, validators, content_length)?;
        self.apply_response_headers(&mut response, ctx)?;
        let end_with_header = is_head || content_length == 0;
        session
            .write_response_header(Box::new(response), end_with_header)
            .await?;
        if end_with_header {
            self.record_metrics(ctx, 200);
            self.log_handler_durations(ctx);
            return Ok(true);
        }

        let mut file_handle = tokio::fs::File::open(&file.path).await.map_err(|error| {
            Error::because(
                ErrorType::FileReadError,
                format!("failed to open static file `{}`", file.path.display()),
                error,
            )
        })?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut sent = 0_u64;
        loop {
            let remaining = content_length.saturating_sub(sent);
            if remaining == 0 {
                break;
            }
            let max_read = buffer.len().min(remaining as usize);
            let read = file_handle
                .read(&mut buffer[..max_read])
                .await
                .map_err(|error| {
                    Error::because(
                        ErrorType::FileReadError,
                        format!("failed to stream static file `{}`", file.path.display()),
                        error,
                    )
                })?;
            if read == 0 {
                session
                    .write_response_body(Some(Bytes::new()), true)
                    .await?;
                break;
            }
            sent = sent.saturating_add(read as u64);
            let end = sent >= content_length;
            session
                .write_response_body(Some(Bytes::copy_from_slice(&buffer[..read])), end)
                .await?;
            if end {
                break;
            }
        }

        self.record_metrics(ctx, 200);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    fn static_response_header(
        &self,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
        content_length: u64,
    ) -> pingora::Result<ResponseHeader> {
        let content_length = usize::try_from(content_length).map_err(|_| {
            Error::explain(
                ErrorType::InternalError,
                "static file is too large to set content-length",
            )
        })?;
        let mut response = ResponseHeader::build(200, Some(12))?;
        response.insert_header("content-type", file.content_type.as_str())?;
        response.insert_header("cache-control", file.cache_control.as_str())?;
        insert_static_validators(&mut response, validators)?;
        response.set_content_length(content_length)?;
        Ok(response)
    }

    async fn write_empty_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::new(),
        )
        .await
    }

    async fn write_text_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        body: &'static str,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::from_static(body.as_bytes()),
        )
        .await
    }

    async fn write_string_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        body: String,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::from(body),
        )
        .await
    }

    async fn write_bytes_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        content_type: &str,
        cache_control: Option<&str>,
        body: Bytes,
    ) -> pingora::Result<bool> {
        self.write_bytes_response_with_headers(
            session,
            ctx,
            status,
            content_type,
            cache_control,
            body,
            &[],
        )
        .await
    }

    async fn write_bytes_response_with_headers(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        content_type: &str,
        cache_control: Option<&str>,
        body: Bytes,
        extra_headers: &[(String, String)],
    ) -> pingora::Result<bool> {
        let is_head = session
            .req_header()
            .method
            .as_str()
            .eq_ignore_ascii_case("HEAD");
        let mut response = ResponseHeader::build(status, Some(8 + extra_headers.len()))?;
        response.insert_header("content-type", content_type)?;
        if let Some(cache_control) = cache_control {
            response.insert_header("cache-control", cache_control)?;
        }
        self.apply_response_headers(&mut response, ctx)?;
        for (name, value) in extra_headers {
            response.append_header(name.to_string(), value.to_string())?;
        }
        response.set_content_length(body.len())?;
        session
            .write_response_header(Box::new(response), is_head)
            .await?;
        if !is_head {
            session.write_response_body(Some(body), true).await?;
        }
        self.record_metrics(ctx, status);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_rejection_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        rejection: HandlerRejection,
    ) -> pingora::Result<bool> {
        let body = Bytes::from(format!("{}: {}", rejection.code, rejection.message));
        self.write_bytes_response_with_headers(
            session,
            ctx,
            rejection.status,
            "text/plain; charset=utf-8",
            None,
            body,
            rejection.headers.as_slice(),
        )
        .await
    }

    async fn write_spa_auth_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: SpaAuthResponse,
    ) -> pingora::Result<bool> {
        self.write_bytes_response_with_headers(
            session,
            ctx,
            response.status,
            response.content_type.as_str(),
            None,
            Bytes::from(response.body),
            response.headers.as_slice(),
        )
        .await
    }

    async fn write_mcp_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: McpHttpResponse,
    ) -> pingora::Result<bool> {
        if response.streamed {
            return self
                .write_streaming_mcp_response(session, ctx, response)
                .await;
        }
        self.write_bytes_response_with_headers(
            session,
            ctx,
            response.status,
            response.content_type.as_str(),
            None,
            Bytes::from(response.body),
            response.headers.as_slice(),
        )
        .await
    }

    async fn write_streaming_mcp_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: McpHttpResponse,
    ) -> pingora::Result<bool> {
        let mut header = ResponseHeader::build(response.status, Some(8 + response.headers.len()))?;
        header.insert_header("content-type", response.content_type.as_str())?;
        self.apply_response_headers(&mut header, ctx)?;
        for (name, value) in &response.headers {
            header.append_header(name.to_string(), value.to_string())?;
        }
        let end_with_header = response.body.is_empty();
        session
            .write_response_header(Box::new(header), end_with_header)
            .await?;
        if !end_with_header {
            session
                .write_response_body(Some(Bytes::from(response.body)), true)
                .await?;
        }
        self.record_metrics(ctx, response.status);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    fn apply_response_headers(
        &self,
        response: &mut ResponseHeader,
        ctx: &GatewayRequestContext,
    ) -> pingora::Result<()> {
        apply_correlation_response(response, &ctx.correlation)?;
        if let Some(cors) = ctx.cors.as_ref() {
            apply_cors_response(response, cors)?;
        }
        if let Some(header_config) = self.header_config.load().as_ref().as_ref() {
            apply_header_response(response, header_config, ctx.request_path.as_str())?;
        }
        if let Some(rate_limit_headers) = ctx.rate_limit_headers.as_ref() {
            apply_rate_limit_headers(response, rate_limit_headers)?;
        }
        for (name, value) in &ctx.extra_response_headers {
            response.append_header(name.to_string(), value.to_string())?;
        }
        Ok(())
    }

    fn record_metrics(&self, ctx: &mut GatewayRequestContext, status: u16) {
        if ctx.metrics_recorded || !ctx.metrics_enabled {
            return;
        }
        let Some(config) = self.metrics_config.load().as_ref().as_ref().cloned() else {
            return;
        };

        let event = build_metrics_event(
            ctx.endpoint.as_str(),
            ctx.method.as_str(),
            status,
            ctx.request_start.elapsed(),
            ctx.correlation.correlation_id.clone(),
        );
        let counts = self.metrics_recorder.record(status);
        ctx.metrics_recorded = true;

        info!(
            target: "light_pingora::metrics",
            product = %config.product_name,
            endpoint = %event.endpoint,
            method = %event.method,
            status = event.status,
            statusClass = event.status_class,
            durationMs = event.duration_ms,
            correlationId = ?event.correlation_id,
            requestCount = counts.request,
            successCount = counts.success,
            authErrorCount = counts.auth_error,
            requestErrorCount = counts.request_error,
            serverErrorCount = counts.server_error,
            "request metrics"
        );
    }

    fn log_handler_durations(&self, ctx: &mut GatewayRequestContext) {
        if ctx.handler_timings_logged
            || ctx.handler_timings.is_empty()
            || !self.active_handlers.load().config().report_handler_duration
        {
            return;
        }

        let durations = ctx
            .handler_timings
            .iter()
            .map(|timing| format!("{}={}us", timing.handler_id, timing.duration.as_micros()))
            .collect::<Vec<_>>()
            .join(", ");

        match self
            .active_handlers
            .load()
            .config()
            .handler_metrics_log_level
        {
            HandlerMetricsLogLevel::Trace => {
                tracing::trace!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Debug => {
                tracing::debug!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Info => {
                tracing::info!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Warn => {
                tracing::warn!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Error => {
                tracing::error!(target: "light_pingora::handler", %durations, "handler durations")
            }
        }
        ctx.handler_timings_logged = true;
    }

    fn prepare_response_handlers(
        &self,
        ctx: &mut GatewayRequestContext,
        handler_ids: &[String],
        request_path: &str,
        method: &str,
    ) -> Result<(), HandlerRejection> {
        for handler_id in handler_ids {
            let started = Instant::now();
            if handler_id.as_str() == "detokenize" {
                let runtime = self.pii_tokenization.load();
                let Some(runtime) = runtime.as_ref().as_ref() else {
                    return Err(HandlerRejection::new(
                        502,
                        "ERR13021",
                        "pii tokenization is not configured",
                    ));
                };
                if runtime.has_response_rules(request_path, method) {
                    runtime.validate_auth(ctx.auth.as_ref())?;
                    ctx.detokenize_active = true;
                }
                ctx.record_handler_duration(handler_id, started.elapsed());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn current_proxy_route(&self) -> Arc<Option<ProxyRoute>> {
        self.proxy_route.load()
    }

    #[cfg(test)]
    fn current_router_route(&self) -> Arc<Option<RouterRoute>> {
        self.router_route.load()
    }

    #[cfg(test)]
    fn current_static_resources(&self) -> Arc<StaticResourceSet> {
        self.static_resources.load()
    }

    #[cfg(test)]
    fn current_path_prefix_service_config(&self) -> Arc<Option<PathPrefixServiceConfig>> {
        self.path_prefix_service_config.load()
    }

    #[cfg(test)]
    fn current_token_runtime(&self) -> Arc<Option<TokenRuntime>> {
        self.token_runtime.load()
    }

    #[cfg(test)]
    fn current_stateless_auth(&self) -> Arc<Option<StatelessAuthRuntime>> {
        self.stateless_auth.load()
    }

    #[cfg(test)]
    fn current_msal_exchange(&self) -> Arc<Option<MsalExchangeRuntime>> {
        self.msal_exchange.load()
    }

    #[cfg(test)]
    fn current_mcp_router(&self) -> Arc<Option<McpRouterRuntime>> {
        self.mcp_router.load()
    }

    #[cfg(test)]
    fn current_websocket_router(&self) -> Arc<Option<WebSocketRouterRuntime>> {
        self.websocket_router.load()
    }

    #[cfg(test)]
    fn active_handler_ids(&self) -> Vec<String> {
        self.active_handlers.load().active_handler_ids().to_vec()
    }
}

struct HandlerReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
    api_key_config: Arc<ConfigManager<Option<ApiKeyConfig>>>,
    basic_auth_config: Arc<ConfigManager<Option<BasicAuthConfig>>>,
    security_runtime: Arc<ConfigManager<Option<SecurityRuntime>>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
}

#[async_trait]
impl ReloadableModule for HandlerReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers =
            load_active_handlers(&ctx.runtime_config, &gateway_handler_registry())?;
        let correlation_config = load_correlation_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("correlation"),
        )?;
        let cors_config = load_cors_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("cors"),
        )?;
        let metrics_config = load_metrics_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("metrics"),
        )?;
        let header_config = load_header_config(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["header", "headers"]),
        )?;
        let api_key_config = load_api_key_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["api-key", "apikey", "unified-security", "unified"],
            ),
        )?;
        let basic_auth_config = load_basic_auth_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["basic-auth", "basic", "unified-security", "unified"],
            ),
        )?;
        let security_runtime = load_security_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["security", "jwt", "unified-security", "unified"],
            ),
        )?;
        let unified_security_config = load_unified_security_config(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["unified-security", "unified"]),
        )?;
        let rate_limit_runtime = load_rate_limit_runtime(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["limit", "rate-limit"]),
        )?;
        let path_prefix_service_config = load_path_prefix_service_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["prefix", "path-prefix-service", "pathPrefixService"],
            ),
        )?;
        let token_runtime = load_token_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("token"),
        )?;
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        let pii_tokenization = load_pii_tokenization_runtime(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["tokenize", "detokenize"]),
        )?;
        let access_control = load_access_control_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("access-control"),
        )?;
        let mcp_router = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("mcp"),
            &self.mcp_router,
        )?;
        let websocket_router = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("websocket"),
            &self.websocket_router,
        )?;
        let router_route = load_router_route(
            &ctx.runtime_config,
            active_handlers.is_handler_active("router"),
        )?;
        self.active_handlers.store(active_handlers);
        self.correlation_config.store(correlation_config);
        self.cors_config.store(cors_config);
        self.metrics_config.store(metrics_config);
        self.header_config.store(header_config);
        self.api_key_config.store(api_key_config);
        self.basic_auth_config.store(basic_auth_config);
        self.security_runtime.store(security_runtime);
        self.unified_security_config.store(unified_security_config);
        self.rate_limit_runtime.store(rate_limit_runtime);
        self.path_prefix_service_config
            .store(path_prefix_service_config);
        self.token_runtime.store(token_runtime);
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        self.pii_tokenization.store(pii_tokenization);
        self.access_control.store(access_control);
        self.mcp_router.store(mcp_router);
        self.websocket_router.store(websocket_router);
        self.router_route.store(router_route);
        Ok(ReloadOutcome::success("handler.yml reloaded"))
    }
}

struct CorrelationReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
}

#[async_trait]
impl ReloadableModule for CorrelationReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("correlation");
        let config = load_correlation_config(&ctx.runtime_config, active)?;
        self.correlation_config.store(config);
        Ok(ReloadOutcome::success("correlation.yml reloaded"))
    }
}

struct CorsReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
}

#[async_trait]
impl ReloadableModule for CorsReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("cors");
        let config = load_cors_config(&ctx.runtime_config, active)?;
        self.cors_config.store(config);
        Ok(ReloadOutcome::success("cors.yml reloaded"))
    }
}

struct MetricsReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
}

#[async_trait]
impl ReloadableModule for MetricsReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("metrics");
        let config = load_metrics_config(&ctx.runtime_config, active)?;
        self.metrics_config.store(config);
        Ok(ReloadOutcome::success("metrics.yml reloaded"))
    }
}

struct HeaderReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
}

#[async_trait]
impl ReloadableModule for HeaderReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["header", "headers"]);
        let config = load_header_config(&ctx.runtime_config, active)?;
        self.header_config.store(config);
        Ok(ReloadOutcome::success("header.yml reloaded"))
    }
}

struct ApiKeyReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    api_key_config: Arc<ConfigManager<Option<ApiKeyConfig>>>,
}

#[async_trait]
impl ReloadableModule for ApiKeyReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["api-key", "apikey", "unified-security", "unified"],
        );
        let config = load_api_key_config(&ctx.runtime_config, active)?;
        self.api_key_config.store(config);
        Ok(ReloadOutcome::success("apikey.yml reloaded"))
    }
}

struct BasicAuthReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    basic_auth_config: Arc<ConfigManager<Option<BasicAuthConfig>>>,
}

#[async_trait]
impl ReloadableModule for BasicAuthReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["basic-auth", "basic", "unified-security", "unified"],
        );
        let config = load_basic_auth_config(&ctx.runtime_config, active)?;
        self.basic_auth_config.store(config);
        Ok(ReloadOutcome::success("basic-auth.yml reloaded"))
    }
}

struct SecurityReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    security_runtime: Arc<ConfigManager<Option<SecurityRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
}

#[async_trait]
impl ReloadableModule for SecurityReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["security", "jwt", "unified-security", "unified"],
        );
        let config = load_security_runtime(&ctx.runtime_config, active)?;
        if let Some(ref runtime) = config {
            if let Err(error) = runtime.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap JWT keys on security config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        if let Some(ref msal) = msal_exchange {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on security config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.security_runtime.store(config);
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        Ok(ReloadOutcome::success("security.yml reloaded"))
    }
}

struct UnifiedSecurityReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
}

#[async_trait]
impl ReloadableModule for UnifiedSecurityReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["unified-security", "unified"]);
        let config = load_unified_security_config(&ctx.runtime_config, active)?;
        self.unified_security_config.store(config);
        Ok(ReloadOutcome::success("unified-security.yml reloaded"))
    }
}

struct RateLimitReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
}

#[async_trait]
impl ReloadableModule for RateLimitReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["limit", "rate-limit"]);
        let config = load_rate_limit_runtime(&ctx.runtime_config, active)?;
        self.rate_limit_runtime.store(config);
        Ok(ReloadOutcome::success("limit.yml reloaded"))
    }
}

struct PathPrefixServiceReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
}

#[async_trait]
impl ReloadableModule for PathPrefixServiceReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["prefix", "path-prefix-service", "pathPrefixService"],
        );
        let config = load_path_prefix_service_config(&ctx.runtime_config, active)?;
        self.path_prefix_service_config.store(config);
        Ok(ReloadOutcome::success("pathPrefixService.yml reloaded"))
    }
}

struct TokenReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
}

struct StatelessAuthReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
}

#[async_trait]
impl ReloadableModule for StatelessAuthReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["stateless", "google", "facebook", "github"],
        );
        let runtime = load_stateless_auth_runtime(&ctx.runtime_config, active)?;
        self.stateless_auth.store(runtime);
        Ok(ReloadOutcome::success("statelessAuth.yml reloaded"))
    }
}

struct MsalExchangeReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
}

#[async_trait]
impl ReloadableModule for MsalExchangeReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self
            .active_handlers
            .load()
            .is_handler_active("msal-exchange");
        let runtime = load_msal_exchange_runtime(&ctx.runtime_config, active)?;
        if let Some(ref msal) = runtime {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on msal-exchange config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.msal_exchange.store(runtime);
        Ok(ReloadOutcome::success("msal-exchange.yml reloaded"))
    }
}

struct PiiTokenizationReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
}

#[async_trait]
impl ReloadableModule for PiiTokenizationReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["tokenize", "detokenize"]);
        let runtime = load_pii_tokenization_runtime(&ctx.runtime_config, active)?;
        self.pii_tokenization.store(runtime);
        Ok(ReloadOutcome::success("pii-tokenization.yml reloaded"))
    }
}

struct McpRouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for McpRouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("mcp");
        let runtime = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active,
            &self.mcp_router,
        )?;
        self.mcp_router.store(runtime);
        Ok(ReloadOutcome::success("mcp-router.yml reloaded"))
    }
}

struct WebSocketRouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for WebSocketRouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("websocket");
        let runtime = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active,
            &self.websocket_router,
        )?;
        self.websocket_router.store(runtime);
        Ok(ReloadOutcome::success("websocket-router.yml reloaded"))
    }
}

struct AccessControlReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for AccessControlReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let access_control = load_access_control_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("access-control"),
        )?;
        let mcp_router = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("mcp"),
            &self.mcp_router,
        )?;
        let websocket_router = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("websocket"),
            &self.websocket_router,
        )?;
        self.access_control.store(access_control);
        self.mcp_router.store(mcp_router);
        self.websocket_router.store(websocket_router);
        Ok(ReloadOutcome::success("access-control/rule.yml reloaded"))
    }
}

#[async_trait]
impl ReloadableModule for TokenReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = active_handlers.is_handler_active("token");
        let runtime = load_token_runtime(&ctx.runtime_config, active)?;
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        if let Some(ref msal) = msal_exchange {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on token config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.token_runtime.store(runtime);
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        Ok(ReloadOutcome::success("token/client/sidecar.yml reloaded"))
    }
}

struct ProxyReloader {
    proxy_route: Arc<ConfigManager<Option<ProxyRoute>>>,
}

#[async_trait]
impl ReloadableModule for ProxyReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let proxy_route = load_proxy_route(&ctx.runtime_config)?;
        self.proxy_route.store(proxy_route);
        Ok(ReloadOutcome::success("proxy.yml reloaded"))
    }
}

struct RouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
}

#[async_trait]
impl ReloadableModule for RouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("router");
        let router_route = load_router_route(&ctx.runtime_config, active)?;
        self.router_route.store(router_route);
        Ok(ReloadOutcome::success("router.yml reloaded"))
    }
}

struct StaticResourceReloader {
    static_resources: Arc<ConfigManager<StaticResourceSet>>,
}

#[async_trait]
impl ReloadableModule for StaticResourceReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let static_resources = load_static_resources(&ctx.runtime_config)?;
        self.static_resources.store(static_resources);
        Ok(ReloadOutcome::success(
            "static resource configuration reloaded",
        ))
    }
}

#[async_trait]
impl ProxyHttp for GatewayProxy {
    type CTX = GatewayRequestContext;

    fn new_ctx(&self) -> Self::CTX {
        GatewayRequestContext::default()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        ctx.begin_request();
        let request_path = session.req_header().uri.path().to_string();
        ctx.request_path = request_path.clone();
        if request_path == HEALTH_PATH {
            return self.write_text_response(session, ctx, 200, "ok").await;
        }

        let method = session.req_header().method.as_str().to_string();
        ctx.method = method.clone();
        let resolved = self
            .active_handlers
            .load()
            .resolve_handler_chain(&request_path, &method)
            .map_err(pingora_internal_error)?;
        ctx.handler_ids = resolved.handler_ids.clone();
        ctx.endpoint = resolved.endpoint(&request_path);
        ctx.path_params = resolved
            .path
            .as_ref()
            .map(|path| path.params.clone())
            .unwrap_or_default();

        if ctx.handler_ids.is_empty() {
            if let Some((target, rewrite_host_header, reuse_x_forwarded)) = self.select_upstream() {
                ctx.proxy_target = Some(target);
                ctx.rewrite_host_header = rewrite_host_header;
                ctx.reuse_x_forwarded = reuse_x_forwarded;
                return Ok(false);
            }
            return self
                .write_text_response(session, ctx, 404, "not found")
                .await;
        }

        let handler_ids = ctx.handler_ids.clone();
        for (handler_index, handler_id) in handler_ids.clone().into_iter().enumerate() {
            let started = Instant::now();
            match handler_id.as_str() {
                "correlation" => {
                    if let Some(config) = self.correlation_config.load().as_ref().as_ref() {
                        ctx.correlation = apply_correlation_request(session, config)?;
                    }
                }
                "cors" => {
                    if let Some(config) = self.cors_config.load().as_ref().as_ref() {
                        match evaluate_cors_request(
                            session,
                            config,
                            &request_path,
                            &self.server_scheme,
                            self.server_port,
                        ) {
                            CorsRequestOutcome::Continue(headers) => {
                                ctx.cors = headers;
                            }
                            CorsRequestOutcome::Respond { status, headers } => {
                                ctx.cors = Some(headers);
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_empty_response(session, ctx, status).await;
                            }
                        }
                    }
                }
                "metrics" => {
                    ctx.metrics_enabled = self.metrics_config.load().as_ref().is_some();
                }
                "header" | "headers" => {
                    if let Some(config) = self.header_config.load().as_ref().as_ref() {
                        apply_header_request(session, config, &request_path)?;
                    }
                }
                "api-key" | "apikey" => {
                    if let Some(config) = self.api_key_config.load().as_ref().as_ref() {
                        if let Err(rejection) = verify_api_key(session, config, &request_path) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                "basic-auth" | "basic" => {
                    if let Some(config) = self.basic_auth_config.load().as_ref().as_ref() {
                        if let Err(rejection) = verify_basic_auth(session, config, &request_path) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                "security" | "jwt" => {
                    if let Some(runtime) = self.security_runtime.load().as_ref().as_ref() {
                        match verify_jwt_request(session, runtime, &request_path).await {
                            Ok(auth) => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "unified-security" | "unified" => {
                    if let Some(config) = self.unified_security_config.load().as_ref().as_ref() {
                        let basic_config = self.basic_auth_config.load();
                        let api_key_config = self.api_key_config.load();
                        let security_runtime = self.security_runtime.load();
                        match verify_unified_security(
                            session,
                            config,
                            basic_config.as_ref().as_ref(),
                            api_key_config.as_ref().as_ref(),
                            security_runtime.as_ref().as_ref(),
                            &request_path,
                        )
                        .await
                        {
                            Ok(auth) => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "limit" | "rate-limit" => {
                    if let Some(runtime) = self.rate_limit_runtime.load().as_ref().as_ref() {
                        match check_rate_limit(session, runtime, ctx.auth.as_ref(), &request_path) {
                            Ok(headers) => {
                                ctx.rate_limit_headers = headers;
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "prefix" | "path-prefix-service" | "pathPrefixService" => {
                    if let Some(config) = self.path_prefix_service_config.load().as_ref().as_ref() {
                        apply_path_prefix_service(session, config, &request_path)?;
                    }
                }
                "token" => {
                    if let Some(runtime) = self.token_runtime.load().as_ref().as_ref()
                        && let Err(rejection) =
                            apply_token_request(session, runtime, &request_path).await
                    {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self.write_rejection_response(session, ctx, rejection).await;
                    }
                }
                "tokenize" => {
                    let runtime = self.pii_tokenization.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "pii tokenization is not configured",
                            )
                            .await;
                    };
                    if runtime.has_request_rules(&request_path, &method) {
                        if let Err(rejection) = runtime.validate_auth(ctx.auth.as_ref()) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        if request_header(session, "content-encoding").is_some() {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_rejection_response(
                                    session,
                                    ctx,
                                    HandlerRejection::new(
                                        415,
                                        "ERR13017",
                                        "tokenize handler does not support encoded request bodies",
                                    ),
                                )
                                .await;
                        }
                        session.req_header_mut().remove_header("content-length");
                        ctx.tokenize_active = true;
                    }
                }
                "detokenize" => {
                    let runtime = self.pii_tokenization.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "pii tokenization is not configured",
                            )
                            .await;
                    };
                    if runtime.has_response_rules(&request_path, &method) {
                        if let Err(rejection) = runtime.validate_auth(ctx.auth.as_ref()) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        ctx.detokenize_active = true;
                    }
                }
                "access-control" => {
                    let runtime = self.access_control.load();
                    let Some(runtime) = runtime
                        .as_ref()
                        .as_ref()
                        .filter(|runtime| runtime.authorization_enabled())
                    else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    if request_header(session, "content-encoding").is_some()
                        && method_has_request_body(&method)
                    {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_rejection_response(
                                session,
                                ctx,
                                HandlerRejection::new(
                                    415,
                                    "ERR13021",
                                    "access-control handler does not support encoded request bodies",
                                ),
                            )
                            .await;
                    }
                    if method_has_request_body(&method) {
                        ctx.access_control_active = true;
                    } else {
                        let exchange = access_control_exchange(
                            ctx.endpoint.as_str(),
                            ctx.request_path.as_str(),
                            session.req_header().uri.query(),
                            None,
                            ctx.auth.as_ref(),
                        )
                        .map_err(handler_rejection_error)?;
                        match runtime
                            .authorize_http_endpoint(
                                exchange.endpoint.as_str(),
                                &agent_headers(session),
                                ctx.auth.as_ref(),
                                &exchange.request_data,
                                ctx.correlation.correlation_id.as_deref(),
                            )
                            .await
                        {
                            AccessDecision::Allowed => {
                                let has_response_filter =
                                    runtime.has_response_filter(exchange.endpoint.as_str());
                                ctx.access_control_exchange = Some(exchange);
                                ctx.access_control_response_active = has_response_filter;
                            }
                            AccessDecision::Denied(message) => {
                                warn!(
                                    endpoint = exchange.endpoint.as_str(),
                                    request_path = ctx.request_path.as_str(),
                                    method = ctx.method.as_str(),
                                    client_id = ctx
                                        .auth
                                        .as_ref()
                                        .and_then(|auth| auth.client_id.as_deref())
                                        .unwrap_or(""),
                                    user_id = ctx
                                        .auth
                                        .as_ref()
                                        .and_then(|auth| auth.user_id.as_deref())
                                        .unwrap_or(""),
                                    correlation_id =
                                        ctx.correlation.correlation_id.as_deref().unwrap_or(""),
                                    reason = message.as_str(),
                                    "access-control denied request"
                                );
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_string_response(session, ctx, 403, message)
                                    .await;
                            }
                        }
                    }
                }
                "stateless" | "google" | "facebook" | "github" => {
                    let runtime = self.stateless_auth.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    match runtime.handle_request(session, handler_id.as_str()).await {
                        Err(rejection) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        Ok(outcome) => match outcome {
                            StatelessAuthOutcome::Continue {
                                auth,
                                response_headers,
                            } => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                                merge_extra_response_headers(
                                    &mut ctx.extra_response_headers,
                                    response_headers,
                                );
                            }
                            StatelessAuthOutcome::Respond(response) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_spa_auth_response(session, ctx, response).await;
                            }
                        },
                    }
                }
                "msal-exchange" => {
                    let runtime = self.msal_exchange.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    match runtime.handle_request(session).await {
                        Err(rejection) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        Ok(outcome) => match outcome {
                            MsalExchangeOutcome::Continue {
                                auth,
                                response_headers,
                            } => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                                merge_extra_response_headers(
                                    &mut ctx.extra_response_headers,
                                    response_headers,
                                );
                            }
                            MsalExchangeOutcome::Respond(response) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_spa_auth_response(session, ctx, response).await;
                            }
                        },
                    }
                }
                "websocket" => {
                    let runtime = self.websocket_router.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "websocket router is not configured",
                            )
                            .await;
                    };
                    if !is_websocket_upgrade(session) {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(session, ctx, 426, "upgrade required")
                            .await;
                    }
                    let headers = agent_headers(session);
                    let decision = match runtime.resolve(
                        &request_path,
                        session.req_header().uri.query(),
                        headers
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_str())),
                    ) {
                        Ok(decision) => decision,
                        Err(error) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_string_response(
                                    session,
                                    ctx,
                                    websocket_route_status(&error),
                                    error.to_string(),
                                )
                                .await;
                        }
                    };
                    match runtime
                        .authorize(
                            &decision,
                            ctx.endpoint.as_str(),
                            &headers,
                            ctx.auth.as_ref(),
                            ctx.correlation.correlation_id.as_deref(),
                        )
                        .await
                    {
                        AccessDecision::Allowed => {}
                        AccessDecision::Denied(message) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_string_response(session, ctx, 403, message).await;
                        }
                    }
                    if let Err(error) = runtime.check_upgrade_rate() {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_string_response(
                                session,
                                ctx,
                                websocket_route_status(&error),
                                error.to_string(),
                            )
                            .await;
                    }
                    let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
                    match runtime.select_target(&decision, index).await {
                        Ok(target) => {
                            let permit = match runtime.acquire_connection() {
                                Ok(permit) => permit,
                                Err(error) => {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    return self
                                        .write_string_response(
                                            session,
                                            ctx,
                                            websocket_route_status(&error),
                                            error.to_string(),
                                        )
                                        .await;
                                }
                            };
                            ctx.proxy_target = Some(target);
                            ctx.rewrite_host_header = true;
                            ctx.websocket_preserve_routing_headers =
                                runtime.config().preserve_routing_headers;
                            ctx.websocket_idle_timeout = runtime.idle_timeout();
                            ctx.websocket_max_connection_duration =
                                runtime.max_connection_duration();
                            ctx.websocket_permit = Some(permit);
                            let timeout = websocket_io_timeout(ctx);
                            session.as_downstream_mut().set_read_timeout(timeout);
                            session.as_downstream_mut().set_write_timeout(timeout);
                            ctx.websocket_decision = Some(decision);
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return Ok(false);
                        }
                        Err(error) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_string_response(
                                    session,
                                    ctx,
                                    websocket_route_status(&error),
                                    error.to_string(),
                                )
                                .await;
                        }
                    }
                }
                "mcp" => {
                    let runtime = self.mcp_router.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    if !runtime.matches_path(&request_path) {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    }
                    let path_with_query = match session.req_header().uri.query() {
                        Some(query) => format!("{request_path}?{query}"),
                        None => request_path.clone(),
                    };
                    let request = McpHttpRequest {
                        method: method.clone(),
                        path: path_with_query,
                        headers: agent_headers(session),
                        body: read_request_body(session).await?,
                    };
                    match runtime
                        .handle_request_with_context(
                            request,
                            McpRequestContext {
                                auth: ctx.auth.clone(),
                                correlation_id: ctx.correlation.correlation_id.clone(),
                            },
                        )
                        .await
                        .map_err(pingora_internal_error)?
                    {
                        Some(response) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_mcp_response(session, ctx, response).await;
                        }
                        None => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            continue;
                        }
                    }
                }
                "health" => {
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_text_response(session, ctx, 200, "ok").await;
                }
                "virtual" => {
                    let host_header = request_header(session, "host");
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_virtual_host(host_header.as_deref(), &request_path);
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_static_resolution(session, ctx, resolution).await;
                }
                "path-resource" | "resource" => {
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_path_resource(&request_path);
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_static_resolution(session, ctx, resolution).await;
                }
                "proxy" => {
                    if let Some((target, rewrite_host_header, reuse_x_forwarded)) =
                        self.select_upstream()
                    {
                        ctx.proxy_target = Some(target);
                        ctx.rewrite_host_header = rewrite_host_header;
                        ctx.reuse_x_forwarded = reuse_x_forwarded;
                        if let Err(rejection) = self.prepare_response_handlers(
                            ctx,
                            &handler_ids[handler_index + 1..],
                            &request_path,
                            &method,
                        ) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return Ok(false);
                    }
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self
                        .write_text_response(session, ctx, 502, "proxy is not configured")
                        .await;
                }
                "router" => {
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    let route = self.router_route.load();
                    let Some(route) = route.as_ref().as_ref() else {
                        return self
                            .write_text_response(session, ctx, 502, "router is not configured")
                            .await;
                    };
                    let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
                    match select_router_target(session, route, index).await {
                        Ok(decision) => {
                            ctx.proxy_target = Some(decision.target.clone());
                            ctx.rewrite_host_header = route.config.rewrite_host_header;
                            ctx.reuse_x_forwarded = route.config.reuse_x_forwarded;
                            ctx.router_decision = Some(decision);
                            if let Err(rejection) = self.prepare_response_handlers(
                                ctx,
                                &handler_ids[handler_index + 1..],
                                &request_path,
                                &method,
                            ) {
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                            return Ok(false);
                        }
                        Err(rejection) => {
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                _ => {}
            }
            ctx.record_handler_duration(&handler_id, started.elapsed());
        }

        self.write_text_response(session, ctx, 404, "not found")
            .await
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let upstream = ctx.proxy_target.as_ref().ok_or_else(|| {
            Error::explain(
                ErrorType::InternalError,
                "no proxy target selected by handler chain",
            )
        })?;
        if self.is_upstream_circuit_open(upstream) {
            return Err(Error::explain(
                ErrorType::HTTPStatus(503),
                format!("upstream circuit is open for {}", upstream.address),
            ));
        }
        debug!("proxying request to {}", upstream.address);
        let mut peer = if upstream.tls {
            if let Some(cert_key) = self.upstream_client_cert_key.as_ref() {
                HttpPeer::new_mtls(
                    upstream.address.as_str(),
                    upstream.sni.clone(),
                    Arc::clone(cert_key),
                )
            } else {
                HttpPeer::new(
                    upstream.address.as_str(),
                    upstream.tls,
                    upstream.sni.clone(),
                )
            }
        } else {
            HttpPeer::new(
                upstream.address.as_str(),
                upstream.tls,
                upstream.sni.clone(),
            )
        };
        if !self.upstream_verify_hostname {
            peer.options.verify_hostname = false;
        }
        if let Some(timeout) = self.upstream_connect_timeout {
            peer.options.connection_timeout = Some(timeout);
        }
        if ctx.websocket_decision.is_some()
            && let Some(timeout) = websocket_io_timeout(ctx)
        {
            peer.options.read_timeout = Some(timeout);
            peer.options.write_timeout = Some(timeout);
        }
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if let Some(target) = ctx.proxy_target.as_ref() {
            if ctx.rewrite_host_header {
                if let Some(original_host) = request_header(session, "host") {
                    upstream_request.insert_header("x-forwarded-host", original_host)?;
                }
                upstream_request.insert_header("host", target.host_header.clone())?;
            }
            apply_forwarded_headers(
                session,
                upstream_request,
                ctx.reuse_x_forwarded,
                self.server_scheme.as_str(),
                self.server_port,
            )?;
            if let Some(decision) = ctx.websocket_decision.as_ref() {
                apply_websocket_upstream_request(
                    upstream_request,
                    decision,
                    ctx.websocket_preserve_routing_headers,
                )?;
            } else if let Some(decision) = ctx.router_decision.as_ref() {
                let route = self.router_route.load();
                let route = route.as_ref().as_ref().ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "router target selected but router.yml is not loaded",
                    )
                })?;
                apply_router_upstream_request(upstream_request, route, decision, &ctx.endpoint)?;
            } else if !target.path_prefix.is_empty() {
                rewrite_upstream_path(upstream_request, &target.path_prefix)?;
            }
        }
        if ctx.access_control_active || ctx.access_control_response_active {
            upstream_request.remove_header("accept-encoding");
        }
        upstream_request.insert_header("x-light-gateway", "light-pingora")?;
        if let Some(correlation_id) = correlation_id_for_upstream(&ctx.correlation) {
            upstream_request.insert_header(light_pingora::CORRELATION_ID_HEADER, correlation_id)?;
        }
        if let Some(traceability_id) = ctx.correlation.traceability_id.as_deref() {
            upstream_request
                .insert_header(light_pingora::TRACEABILITY_ID_HEADER, traceability_id)?;
        }
        Ok(())
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&pingora::protocols::Digest>,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.websocket_decision.is_some() {
            let now = Instant::now();
            ctx.websocket_connected_at = Some(now);
            ctx.websocket_last_activity = Some(now);
        }
        Ok(())
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.websocket_decision.is_some() && session.was_upgraded() {
            enforce_websocket_tunnel_limits(ctx, body)?;
        }
        if ctx.tokenize_active {
            let runtime = self.pii_tokenization.load();
            let Some(runtime) = runtime.as_ref().as_ref() else {
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "pii tokenization is not configured",
                ));
            };
            buffer_body_chunk(
                &mut ctx.tokenize_request_body,
                body,
                runtime.max_body_size(),
                "request",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.tokenize_request_body);
                let transformed = runtime
                    .tokenize_request_body(
                        ctx.auth.as_ref(),
                        ctx.request_path.as_str(),
                        ctx.method.as_str(),
                        input.as_slice(),
                    )
                    .await
                    .map_err(handler_rejection_error)?;
                *body = Some(Bytes::from(transformed));
            } else {
                *body = None;
            }
        }
        if ctx.access_control_active {
            buffer_body_chunk(
                &mut ctx.access_control_request_body,
                body,
                ACCESS_CONTROL_MAX_BODY_SIZE,
                "access-control request",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.access_control_request_body);
                let runtime = self.access_control.load();
                let Some(runtime) = runtime
                    .as_ref()
                    .as_ref()
                    .filter(|runtime| runtime.authorization_enabled())
                else {
                    *body = Some(Bytes::from(input));
                    return Ok(());
                };
                let exchange = access_control_exchange(
                    ctx.endpoint.as_str(),
                    ctx.request_path.as_str(),
                    session.req_header().uri.query(),
                    Some(input.as_slice()),
                    ctx.auth.as_ref(),
                )
                .map_err(handler_rejection_error)?;
                match runtime
                    .authorize_http_endpoint(
                        exchange.endpoint.as_str(),
                        &agent_headers(session),
                        ctx.auth.as_ref(),
                        &exchange.request_data,
                        ctx.correlation.correlation_id.as_deref(),
                    )
                    .await
                {
                    AccessDecision::Allowed => {
                        let has_response_filter =
                            runtime.has_response_filter(exchange.endpoint.as_str());
                        ctx.access_control_exchange = Some(exchange);
                        ctx.access_control_response_active = has_response_filter;
                        *body = Some(Bytes::from(input));
                    }
                    AccessDecision::Denied(message) => {
                        warn!(
                            endpoint = exchange.endpoint.as_str(),
                            request_path = ctx.request_path.as_str(),
                            method = ctx.method.as_str(),
                            client_id = ctx
                                .auth
                                .as_ref()
                                .and_then(|auth| auth.client_id.as_deref())
                                .unwrap_or(""),
                            user_id = ctx
                                .auth
                                .as_ref()
                                .and_then(|auth| auth.user_id.as_deref())
                                .unwrap_or(""),
                            correlation_id =
                                ctx.correlation.correlation_id.as_deref().unwrap_or(""),
                            reason = message.as_str(),
                            "access-control denied request"
                        );
                        return Err(access_control_status_error(403, message));
                    }
                }
            } else {
                *body = None;
            }
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<Duration>>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.websocket_decision.is_some() && session.was_upgraded() {
            enforce_websocket_tunnel_limits(ctx, body)?;
        }
        if ctx.detokenize_active {
            let runtime = self.pii_tokenization.load();
            let Some(runtime) = runtime.as_ref().as_ref() else {
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "pii tokenization is not configured",
                ));
            };
            buffer_body_chunk(
                &mut ctx.detokenize_response_body,
                body,
                runtime.max_body_size(),
                "response",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.detokenize_response_body);
                let transformed = block_on_detokenize_response(
                    runtime,
                    ctx.auth.as_ref(),
                    ctx.request_path.as_str(),
                    ctx.method.as_str(),
                    input.as_slice(),
                )
                .map_err(handler_rejection_error)?;
                *body = Some(Bytes::from(transformed));
            } else {
                *body = None;
            }
        }
        if ctx.access_control_response_active {
            buffer_body_chunk(
                &mut ctx.access_control_response_body,
                body,
                ACCESS_CONTROL_MAX_BODY_SIZE,
                "access-control response",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.access_control_response_body);
                let Some(exchange) = ctx.access_control_exchange.as_ref() else {
                    *body = Some(Bytes::from(input));
                    return Ok(None);
                };
                let runtime = self.access_control.load();
                let Some(runtime) = runtime.as_ref().as_ref() else {
                    return Err(Error::explain(
                        ErrorType::InternalError,
                        "access-control is not configured",
                    ));
                };
                let transformed = block_on_access_control_response(
                    runtime,
                    exchange,
                    &agent_headers(session),
                    ctx.auth.as_ref(),
                    ctx.correlation.correlation_id.as_deref(),
                    ctx.upstream_status.unwrap_or(200),
                    input.as_slice(),
                );
                *body = Some(Bytes::from(transformed.unwrap_or(input)));
            } else {
                *body = None;
            }
        }
        Ok(None)
    }

    async fn response_filter(
        &self,
        session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        ctx.upstream_status = Some(upstream_response.status.as_u16());
        if ctx.detokenize_active {
            if upstream_response.headers.get("content-encoding").is_some() {
                return Err(handler_rejection_error(HandlerRejection::new(
                    415,
                    "ERR13018",
                    "detokenize handler does not support encoded response bodies",
                )));
            }
            upstream_response.remove_header("content-length");
            upstream_response.remove_header("etag");
            upstream_response.remove_header("last-modified");
        }
        if ctx.access_control_response_active {
            if upstream_response.headers.get("content-encoding").is_some() {
                return Err(handler_rejection_error(HandlerRejection::new(
                    415,
                    "ERR13022",
                    "access-control handler does not support encoded response bodies",
                )));
            }
            upstream_response.remove_header("content-length");
            upstream_response.remove_header("etag");
            upstream_response.remove_header("last-modified");
        }
        if ctx.websocket_decision.is_some()
            && upstream_response.status.as_u16() == 101
            && upstream_response
                .headers
                .get("sec-websocket-protocol")
                .is_none()
            && let Some(protocol) = selected_websocket_protocol(session)
        {
            upstream_response.insert_header("Sec-WebSocket-Protocol", protocol)?;
        }
        self.apply_response_headers(upstream_response, ctx)?;
        if upstream_response.status.as_u16() >= 500 {
            self.record_upstream_failure(ctx);
        } else {
            self.record_upstream_success(ctx);
        }
        self.record_metrics(ctx, upstream_response.status.as_u16());
        self.log_handler_durations(ctx);
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        e: Box<Error>,
    ) -> Box<Error> {
        self.record_upstream_failure(ctx);
        e
    }

    async fn logging(&self, _session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX)
    where
        Self::CTX: Send + Sync,
    {
        if error.is_some() {
            self.record_metrics(ctx, 500);
        }
        self.log_handler_durations(ctx);
    }
}

#[derive(Debug, Default)]
struct UpstreamCircuitState {
    failures: u32,
    opened_at: Option<Instant>,
}

struct GatewayRequestContext {
    proxy_target: Option<ProxyTarget>,
    rewrite_host_header: bool,
    reuse_x_forwarded: bool,
    router_decision: Option<RouterDecision>,
    websocket_decision: Option<WebSocketRouteDecision>,
    websocket_permit: Option<WebSocketConnectionPermit>,
    websocket_preserve_routing_headers: bool,
    websocket_idle_timeout: Option<Duration>,
    websocket_max_connection_duration: Option<Duration>,
    websocket_connected_at: Option<Instant>,
    websocket_last_activity: Option<Instant>,
    request_start: Instant,
    handler_ids: Vec<String>,
    request_path: String,
    endpoint: String,
    method: String,
    path_params: BTreeMap<String, String>,
    correlation: CorrelationState,
    cors: Option<CorsResponseHeaders>,
    auth: Option<AuthPrincipal>,
    tokenize_active: bool,
    detokenize_active: bool,
    access_control_active: bool,
    access_control_response_active: bool,
    tokenize_request_body: Vec<u8>,
    detokenize_response_body: Vec<u8>,
    access_control_request_body: Vec<u8>,
    access_control_response_body: Vec<u8>,
    access_control_exchange: Option<AccessControlExchange>,
    upstream_status: Option<u16>,
    rate_limit_headers: Option<RateLimitHeaders>,
    extra_response_headers: Vec<(String, String)>,
    metrics_enabled: bool,
    metrics_recorded: bool,
    handler_timings: Vec<HandlerTiming>,
    handler_timings_logged: bool,
}

impl Default for GatewayRequestContext {
    fn default() -> Self {
        Self {
            proxy_target: None,
            rewrite_host_header: false,
            reuse_x_forwarded: false,
            router_decision: None,
            websocket_decision: None,
            websocket_permit: None,
            websocket_preserve_routing_headers: false,
            websocket_idle_timeout: None,
            websocket_max_connection_duration: None,
            websocket_connected_at: None,
            websocket_last_activity: None,
            request_start: Instant::now(),
            handler_ids: Vec::new(),
            request_path: String::new(),
            endpoint: String::new(),
            method: String::new(),
            path_params: BTreeMap::new(),
            correlation: CorrelationState::default(),
            cors: None,
            auth: None,
            tokenize_active: false,
            detokenize_active: false,
            access_control_active: false,
            access_control_response_active: false,
            tokenize_request_body: Vec::new(),
            detokenize_response_body: Vec::new(),
            access_control_request_body: Vec::new(),
            access_control_response_body: Vec::new(),
            access_control_exchange: None,
            upstream_status: None,
            rate_limit_headers: None,
            extra_response_headers: Vec::new(),
            metrics_enabled: false,
            metrics_recorded: false,
            handler_timings: Vec::new(),
            handler_timings_logged: false,
        }
    }
}

impl GatewayRequestContext {
    fn begin_request(&mut self) {
        self.proxy_target = None;
        self.rewrite_host_header = false;
        self.reuse_x_forwarded = false;
        self.router_decision = None;
        self.websocket_decision = None;
        self.websocket_permit = None;
        self.websocket_preserve_routing_headers = false;
        self.websocket_idle_timeout = None;
        self.websocket_max_connection_duration = None;
        self.websocket_connected_at = None;
        self.websocket_last_activity = None;
        self.request_start = Instant::now();
        self.handler_ids.clear();
        self.request_path.clear();
        self.endpoint.clear();
        self.method.clear();
        self.path_params.clear();
        self.correlation = CorrelationState::default();
        self.cors = None;
        self.auth = None;
        self.tokenize_active = false;
        self.detokenize_active = false;
        self.access_control_active = false;
        self.access_control_response_active = false;
        self.tokenize_request_body.clear();
        self.detokenize_response_body.clear();
        self.access_control_request_body.clear();
        self.access_control_response_body.clear();
        self.access_control_exchange = None;
        self.upstream_status = None;
        self.rate_limit_headers = None;
        self.extra_response_headers.clear();
        self.metrics_enabled = false;
        self.metrics_recorded = false;
        self.handler_timings.clear();
        self.handler_timings_logged = false;
    }

    fn record_handler_duration(&mut self, handler_id: &str, duration: Duration) {
        self.handler_timings.push(HandlerTiming {
            handler_id: handler_id.to_string(),
            duration,
        });
    }
}

struct HandlerTiming {
    handler_id: String,
    duration: Duration,
}

#[derive(Debug, Clone)]
struct AccessControlExchange {
    endpoint: String,
    request_data: JsonValue,
}

#[tokio::main]
async fn main() -> Result<()> {
    let tracing_guard = init_tracing(
        TracingOptions::new("light-gateway").with_legacy_ansi_env("GATEWAY_LOG_ANSI"),
    )?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }

    let cache_registry = Arc::new(CacheRegistry::new());
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir(DEFAULT_CONFIG_DIR)
        .with_config_dir(CONFIG_DIR)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
        .with_cache_registry(cache_registry)
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start light-gateway runtime")?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    running
        .shutdown()
        .await
        .context("failed to shut down light-gateway")?;

    Ok(())
}

fn rewrite_upstream_path(
    upstream_request: &mut pingora::http::RequestHeader,
    path_prefix: &str,
) -> pingora::Result<()> {
    let original = upstream_request
        .uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or("/");
    let (path, query) = original
        .split_once('?')
        .map_or((original, None), |(path, query)| (path, Some(query)));
    let path = if path == "/" {
        path_prefix.to_string()
    } else {
        format!("{}{}", path_prefix.trim_end_matches('/'), path)
    };
    let path_and_query = query.map_or(path.clone(), |query| format!("{path}?{query}"));
    let uri = path_and_query.parse().map_err(|error| {
        Error::because(
            ErrorType::InvalidHTTPHeader,
            format!("invalid upstream URI `{path_and_query}`"),
            error,
        )
    })?;
    upstream_request.set_uri(uri);
    Ok(())
}

fn apply_forwarded_headers(
    session: &Session,
    upstream_request: &mut pingora::http::RequestHeader,
    reuse_x_forwarded: bool,
    server_scheme: &str,
    server_port: u16,
) -> pingora::Result<()> {
    let remote = client_ip(session).unwrap_or_else(|| "unknown".to_string());
    let forwarded_for = if reuse_x_forwarded {
        upstream_request
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(|value| format!("{value},{remote}"))
            .unwrap_or(remote)
    } else {
        remote
    };
    upstream_request.insert_header("x-forwarded-for", forwarded_for)?;

    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-proto") {
        upstream_request.insert_header("x-forwarded-proto", server_scheme.to_string())?;
    }
    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-port") {
        upstream_request.insert_header(
            "x-forwarded-port",
            host_port(session).unwrap_or(server_port).to_string(),
        )?;
    }
    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-server") {
        if let Some(host) = request_header(session, "host").and_then(|host| host_name(&host)) {
            upstream_request.insert_header("x-forwarded-server", host)?;
        }
    }
    Ok(())
}

fn request_header(session: &Session, name: &str) -> Option<String> {
    let header = session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if header.is_some() {
        return header;
    }
    if name.eq_ignore_ascii_case("host") {
        return session
            .req_header()
            .uri
            .authority()
            .map(|authority| authority.as_str().to_string());
    }
    None
}

fn selected_websocket_protocol(session: &Session) -> Option<String> {
    request_header(session, "Sec-WebSocket-Protocol")
        .as_deref()
        .and_then(first_websocket_protocol)
}

fn first_websocket_protocol(value: &str) -> Option<String> {
    value
        .split(',')
        .map(str::trim)
        .find(|protocol| !protocol.is_empty())
        .map(str::to_string)
}

fn agent_headers(session: &Session) -> Vec<(String, String)> {
    session
        .req_header()
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn method_has_request_body(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    )
}

fn access_control_exchange(
    endpoint: &str,
    request_path: &str,
    query: Option<&str>,
    body: Option<&[u8]>,
    _auth: Option<&AuthPrincipal>,
) -> Result<AccessControlExchange, HandlerRejection> {
    if is_portal_hybrid_path(request_path) {
        return portal_access_control_exchange(query, body);
    }
    let request_data = body
        .filter(|body| !body.is_empty())
        .and_then(|body| serde_json::from_slice::<JsonValue>(body).ok())
        .unwrap_or_else(|| JsonValue::Object(Default::default()));
    Ok(AccessControlExchange {
        endpoint: endpoint.to_string(),
        request_data: request_data.clone(),
    })
}

fn portal_access_control_exchange(
    query: Option<&str>,
    body: Option<&[u8]>,
) -> Result<AccessControlExchange, HandlerRejection> {
    let envelope = if let Some(body) = body.filter(|body| !body.is_empty()) {
        let parsed = serde_json::from_slice::<JsonValue>(body).map_err(|error| {
            HandlerRejection::new(
                400,
                "ERR13023",
                format!("invalid hybrid portal request body: {error}"),
            )
        })?;
        normalize_hybrid_body_envelope(parsed)?
    } else {
        hybrid_envelope_from_query(query)?
    };
    let host = required_text(&envelope, "host")?;
    let service = required_text(&envelope, "service")?;
    let action_name = required_text(&envelope, "action")?;
    let version = required_text(&envelope, "version")?;
    let endpoint = format!("{host}/{service}/{action_name}/{version}");
    let request_data = envelope
        .get("data")
        .cloned()
        .unwrap_or_else(|| JsonValue::Object(Default::default()));
    Ok(AccessControlExchange {
        endpoint: endpoint.clone(),
        request_data: request_data.clone(),
    })
}

fn normalize_hybrid_body_envelope(envelope: JsonValue) -> Result<JsonValue, HandlerRejection> {
    if envelope.get("host").and_then(JsonValue::as_str).is_some()
        && envelope
            .get("service")
            .and_then(JsonValue::as_str)
            .is_some()
        && envelope.get("action").and_then(JsonValue::as_str).is_some()
        && envelope
            .get("version")
            .and_then(JsonValue::as_str)
            .is_some()
    {
        return Ok(envelope);
    }

    let Some(method) = envelope.get("method").and_then(JsonValue::as_str) else {
        return Ok(envelope);
    };
    let parts: Vec<&str> = method.split('/').collect();
    if parts.len() != 4 || parts.iter().any(|part| part.trim().is_empty()) {
        return Ok(envelope);
    }

    Ok(json!({
        "host": parts[0],
        "service": parts[1],
        "action": parts[2],
        "version": parts[3],
        "data": envelope
            .get("params")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(Default::default()))
    }))
}

fn is_portal_hybrid_path(request_path: &str) -> bool {
    matches!(request_path, "/portal/query" | "/portal/command")
}

fn hybrid_envelope_from_query(query: Option<&str>) -> Result<JsonValue, HandlerRejection> {
    let mut envelope = serde_json::Map::new();
    let mut data = serde_json::Map::new();
    if let Some(query) = query {
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "cmd" => {
                    let parsed = serde_json::from_str::<JsonValue>(&value).map_err(|error| {
                        HandlerRejection::new(
                            400,
                            "ERR13023",
                            format!("invalid hybrid portal request cmd: {error}"),
                        )
                    })?;
                    if !parsed.is_object() {
                        return Err(HandlerRejection::new(
                            400,
                            "ERR13023",
                            "invalid hybrid portal request cmd: expected JSON object",
                        ));
                    }
                    return Ok(parsed);
                }
                "host" | "service" | "action" | "version" => {
                    envelope.insert(key.into_owned(), JsonValue::String(value.into_owned()));
                }
                "data" => {
                    if let Ok(value) = serde_json::from_str::<JsonValue>(&value) {
                        envelope.insert("data".to_string(), value);
                    }
                }
                _ => {
                    data.insert(key.into_owned(), JsonValue::String(value.into_owned()));
                }
            }
        }
    }
    if !envelope.contains_key("data") {
        envelope.insert("data".to_string(), JsonValue::Object(data));
    }
    Ok(JsonValue::Object(envelope))
}

fn required_text(envelope: &JsonValue, field: &str) -> Result<String, HandlerRejection> {
    envelope
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            HandlerRejection::new(
                400,
                "ERR13024",
                format!("hybrid portal request is missing `{field}`"),
            )
        })
}



fn block_on_access_control_response(
    runtime: &AccessControlRuntime,
    exchange: &AccessControlExchange,
    headers: &[(String, String)],
    auth: Option<&AuthPrincipal>,
    correlation_id: Option<&str>,
    status_code: u16,
    body: &[u8],
) -> Option<Vec<u8>> {
    let future = runtime.filter_http_response(
        exchange.endpoint.as_str(),
        headers,
        auth,
        &exchange.request_data,
        correlation_id,
        status_code,
        body,
    );
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?
            .block_on(future)
    }
}

fn access_control_status_error(status: u16, message: String) -> Box<Error> {
    Error::explain(ErrorType::HTTPStatus(status), message)
}

async fn read_request_body(session: &mut Session) -> pingora::Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = session.read_request_body().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn static_method_allowed(session: &Session) -> bool {
    matches!(
        session.req_header().method.as_str(),
        method if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
    )
}

fn is_head_request(session: &Session) -> bool {
    session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("HEAD")
}

fn is_websocket_upgrade(session: &Session) -> bool {
    session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("GET")
        && header_contains_token(session, "connection", "upgrade")
        && header_contains_token(session, "upgrade", "websocket")
        && request_header(session, "sec-websocket-key")
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn header_contains_token(session: &Session, name: &str, token: &str) -> bool {
    session
        .req_header()
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
}

fn websocket_route_status(error: &WebSocketRouteError) -> u16 {
    match error {
        WebSocketRouteError::MissingTarget => 403,
        WebSocketRouteError::InvalidProtocol(_) => 400,
        WebSocketRouteError::UpgradeRateExceeded(_) => 429,
        WebSocketRouteError::TooManyActiveConnections(_) => 503,
        WebSocketRouteError::DiscoveryUnavailable(_)
        | WebSocketRouteError::DiscoveryFailed(_)
        | WebSocketRouteError::NoUsableEndpoint(_) => 502,
    }
}

fn websocket_io_timeout(ctx: &GatewayRequestContext) -> Option<Duration> {
    match (
        ctx.websocket_idle_timeout,
        ctx.websocket_max_connection_duration,
    ) {
        (Some(idle), Some(max_duration)) => Some(idle.min(max_duration)),
        (Some(idle), None) => Some(idle),
        (None, Some(max_duration)) => Some(max_duration),
        (None, None) => None,
    }
}

fn enforce_websocket_tunnel_limits(
    ctx: &mut GatewayRequestContext,
    body: &Option<Bytes>,
) -> pingora::Result<()> {
    let now = Instant::now();
    if let Some(max_duration) = ctx.websocket_max_connection_duration {
        let started = ctx.websocket_connected_at.unwrap_or(ctx.request_start);
        if now.duration_since(started) > max_duration {
            return Err(Error::explain(
                ErrorType::ReadTimedout,
                "websocket connection exceeded max duration",
            ));
        }
    }
    if let Some(idle_timeout) = ctx.websocket_idle_timeout
        && let Some(last_activity) = ctx.websocket_last_activity
        && now.duration_since(last_activity) > idle_timeout
    {
        return Err(Error::explain(
            ErrorType::ReadTimedout,
            "websocket connection exceeded idle timeout",
        ));
    }
    if body.as_ref().is_some_and(|body| !body.is_empty()) {
        ctx.websocket_last_activity = Some(now);
    }
    Ok(())
}

fn buffer_body_chunk(
    buffer: &mut Vec<u8>,
    body: &mut Option<Bytes>,
    max_body_size: usize,
    label: &str,
) -> pingora::Result<()> {
    if let Some(chunk) = body.take() {
        if buffer.len().saturating_add(chunk.len()) > max_body_size {
            return Err(handler_rejection_error(HandlerRejection::new(
                413,
                "ERR13019",
                format!("PII tokenization {label} body exceeds maxBodySize"),
            )));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(())
}

fn block_on_detokenize_response(
    runtime: &PiiTokenizationRuntime,
    auth: Option<&AuthPrincipal>,
    path: &str,
    method: &str,
    body: &[u8],
) -> Result<Vec<u8>, HandlerRejection> {
    let future = runtime.detokenize_response_body(auth, path, method, body);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| HandlerRejection::new(500, "ERR13020", "failed to create runtime"))?
            .block_on(future)
    }
}

#[derive(Debug, Clone)]
struct StaticFileValidators {
    etag: String,
    last_modified: Option<String>,
    last_modified_time: Option<SystemTime>,
}

fn static_file_validators(metadata: &std::fs::Metadata) -> StaticFileValidators {
    let modified = metadata.modified().ok();
    StaticFileValidators {
        etag: static_etag(metadata.len(), modified),
        last_modified: modified.map(format_http_date),
        last_modified_time: modified,
    }
}

fn static_etag(length: u64, modified: Option<SystemTime>) -> String {
    let (seconds, nanos) = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    format!("W/\"{length:x}-{seconds:x}-{nanos:x}\"")
}

fn format_http_date(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();
    datetime.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    let parsed = DateTime::parse_from_rfc2822(value).ok()?;
    let utc = parsed.with_timezone(&Utc);
    let seconds = u64::try_from(utc.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn static_request_not_modified(session: &Session, validators: &StaticFileValidators) -> bool {
    if let Some(if_none_match) = request_header(session, "if-none-match") {
        return etag_header_matches(if_none_match.as_str(), validators.etag.as_str());
    }

    let Some(modified) = validators.last_modified_time else {
        return false;
    };
    request_header(session, "if-modified-since")
        .as_deref()
        .and_then(parse_http_date)
        .is_some_and(|since| same_or_after_http_second(since, modified))
}

fn etag_header_matches(header: &str, etag: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || weak_etag_value(candidate) == weak_etag_value(etag)
    })
}

fn weak_etag_value(value: &str) -> &str {
    value.strip_prefix("W/").unwrap_or(value)
}

fn same_or_after_http_second(candidate: SystemTime, modified: SystemTime) -> bool {
    let Some(candidate_seconds) = unix_seconds(candidate) else {
        return false;
    };
    let Some(modified_seconds) = unix_seconds(modified) else {
        return false;
    };
    candidate_seconds >= modified_seconds
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn should_stream_static_file(file_size: u64, transfer_min_size: u64) -> bool {
    file_size >= transfer_min_size
}

fn insert_static_validators(
    response: &mut ResponseHeader,
    validators: &StaticFileValidators,
) -> pingora::Result<()> {
    response.insert_header("etag", validators.etag.as_str())?;
    if let Some(last_modified) = validators.last_modified.as_deref() {
        response.insert_header("last-modified", last_modified)?;
    }
    Ok(())
}

fn client_ip(session: &Session) -> Option<String> {
    session.as_downstream().client_addr().map(|address| {
        address
            .as_inet()
            .map(|address| address.ip().to_string())
            .unwrap_or_else(|| address.to_string())
    })
}

fn host_port(session: &Session) -> Option<u16> {
    request_header(session, "host").and_then(|host| {
        let host = host.split(',').next().unwrap_or(host.as_str()).trim();
        if host.starts_with('[') {
            return host
                .rsplit_once("]:")
                .and_then(|(_, port)| port.parse::<u16>().ok());
        }
        host.rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
    })
}

fn host_name(host: &str) -> Option<String> {
    let host = host.split(',').next().unwrap_or(host).trim();
    if host.is_empty() {
        return None;
    }
    if host.starts_with('[') {
        return host
            .strip_prefix('[')
            .and_then(|value| value.split_once(']'))
            .map(|(host, _)| host.to_string());
    }
    Some(
        host.rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(host)
            .to_string(),
    )
}

fn pingora_internal_error(error: RuntimeError) -> Box<Error> {
    Error::because(ErrorType::InternalError, error.to_string(), error)
}

fn handler_rejection_error(rejection: HandlerRejection) -> Box<Error> {
    Error::explain(
        ErrorType::InternalError,
        format!("{}: {}", rejection.code, rejection.message),
    )
}

fn handler_active(active_handlers: &ActiveHandlerSet, ids: &[&str]) -> bool {
    ids.iter().any(|id| active_handlers.is_handler_active(id))
}

fn upstream_verify_hostname(config: &RuntimeConfig) -> bool {
    config
        .client
        .as_ref()
        .map(|client| client.tls.verify_hostname)
        .unwrap_or(true)
}

fn upstream_connect_timeout(config: &RuntimeConfig) -> Option<Duration> {
    config
        .client
        .as_ref()
        .and_then(|client| duration_from_millis(client.request.connect_timeout))
}

fn duration_from_millis(value: u64) -> Option<Duration> {
    (value > 0).then(|| Duration::from_millis(value))
}

fn upstream_circuit_config(config: &RuntimeConfig) -> (u32, Duration) {
    config
        .client
        .as_ref()
        .map(|client| {
            (
                client.request.error_threshold,
                Duration::from_millis(client.request.reset_timeout),
            )
        })
        .unwrap_or((0, Duration::ZERO))
}

fn upstream_circuit_key(target: &ProxyTarget) -> String {
    target.address.clone()
}

fn upstream_client_cert_key(config: &RuntimeConfig) -> Result<Option<Arc<CertKey>>, RuntimeError> {
    let Some(client) = config.client.as_ref() else {
        return Ok(None);
    };
    let client_cert_path = client
        .tls
        .client_cert_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());
    let client_key_path = client
        .tls
        .client_key_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());

    let (Some(client_cert_path), Some(client_key_path)) = (client_cert_path, client_key_path)
    else {
        if client_cert_path.is_some() || client_key_path.is_some() {
            return Err(RuntimeError::Unsupported(
                "client TLS identity requires both tls.clientCertPath and tls.clientKeyPath"
                    .to_string(),
            ));
        }
        return Ok(None);
    };

    let cert_file = std::fs::File::open(client_cert_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|certificate| certificate.as_ref().to_vec())
        .collect::<Vec<_>>();
    if certificates.is_empty() {
        return Err(RuntimeError::Unsupported(format!(
            "client TLS certificate `{}` contains no certificates",
            client_cert_path.display()
        )));
    }

    let key_file = std::fs::File::open(client_key_path)?;
    let mut key_reader = BufReader::new(key_file);
    let Some(key) = rustls_pemfile::private_key(&mut key_reader)? else {
        return Err(RuntimeError::Unsupported(format!(
            "client TLS key `{}` contains no private key",
            client_key_path.display()
        )));
    };
    let key = key.secret_der().to_vec();
    let cert_key = std::panic::catch_unwind(|| CertKey::new(certificates, key)).map_err(|_| {
        RuntimeError::Unsupported(format!(
            "invalid client TLS identity cert=`{}` key=`{}`",
            client_cert_path.display(),
            client_key_path.display()
        ))
    })?;
    Ok(Some(Arc::new(cert_key)))
}

fn load_mcp_router_runtime_preserving_state(
    runtime_config: &RuntimeConfig,
    active: bool,
    current: &ConfigManager<Option<McpRouterRuntime>>,
) -> Result<Option<McpRouterRuntime>, RuntimeError> {
    let previous = current.load();
    let mut runtime = load_mcp_router_runtime(runtime_config, active)?;
    if let Some(runtime) = runtime.as_mut()
        && let Some(previous) = previous.as_ref().as_ref()
    {
        runtime.preserve_state_from(previous);
    }
    Ok(runtime)
}

fn load_websocket_router_runtime_preserving_state(
    runtime_config: &RuntimeConfig,
    active: bool,
    current: &ConfigManager<Option<WebSocketRouterRuntime>>,
) -> Result<Option<WebSocketRouterRuntime>, RuntimeError> {
    let previous = current.load();
    let mut runtime = load_websocket_router_runtime(runtime_config, active)?;
    if let Some(runtime) = runtime.as_mut()
        && let Some(previous) = previous.as_ref().as_ref()
    {
        runtime.preserve_state_from(previous);
    }
    Ok(runtime)
}

struct RegisteredGatewayHandler {
    id: &'static str,
}

impl PingoraHandler for RegisteredGatewayHandler {
    fn id(&self) -> &'static str {
        self.id
    }
}

fn gateway_handler_registry() -> PingoraHandlerRegistry {
    let mut registry = PingoraHandlerRegistry::new();
    for (id, kind) in GATEWAY_HANDLER_DESCRIPTORS {
        registry = registry.register(gateway_handler_descriptor(id, *kind));
    }
    registry
}

const GATEWAY_HANDLER_DESCRIPTORS: &[(&str, PingoraHandlerKind)] = &[
    ("exception", PingoraHandlerKind::Core),
    ("metrics", PingoraHandlerKind::Observability),
    ("correlation", PingoraHandlerKind::Observability),
    ("cors", PingoraHandlerKind::Traffic),
    ("specification", PingoraHandlerKind::Security),
    ("security", PingoraHandlerKind::Security),
    ("jwt", PingoraHandlerKind::Security),
    ("api-key", PingoraHandlerKind::Security),
    ("apikey", PingoraHandlerKind::Security),
    ("basic-auth", PingoraHandlerKind::Security),
    ("basic", PingoraHandlerKind::Security),
    ("unified-security", PingoraHandlerKind::Security),
    ("unified", PingoraHandlerKind::Security),
    ("access-control", PingoraHandlerKind::Security),
    ("body", PingoraHandlerKind::Traffic),
    ("audit", PingoraHandlerKind::Observability),
    ("sanitizer", PingoraHandlerKind::Security),
    ("validator", PingoraHandlerKind::Security),
    ("header", PingoraHandlerKind::Traffic),
    ("headers", PingoraHandlerKind::Traffic),
    ("limit", PingoraHandlerKind::Traffic),
    ("rate-limit", PingoraHandlerKind::Traffic),
    ("request-size-limit", PingoraHandlerKind::Traffic),
    ("prefix", PingoraHandlerKind::Traffic),
    ("path-prefix-service", PingoraHandlerKind::Traffic),
    ("pathPrefixService", PingoraHandlerKind::Traffic),
    ("token", PingoraHandlerKind::Security),
    ("tokenize", PingoraHandlerKind::Traffic),
    ("detokenize", PingoraHandlerKind::Traffic),
    ("router", PingoraHandlerKind::Traffic),
    ("proxy", PingoraHandlerKind::Traffic),
    ("proxyServerInfo", PingoraHandlerKind::Application),
    ("virtual", PingoraHandlerKind::Application),
    ("path-resource", PingoraHandlerKind::Application),
    ("resource", PingoraHandlerKind::Application),
    ("killapp", PingoraHandlerKind::Application),
    ("latency", PingoraHandlerKind::Application),
    ("memory", PingoraHandlerKind::Application),
    ("exchaos", PingoraHandlerKind::Application),
    ("chaosget", PingoraHandlerKind::Application),
    ("chaospost", PingoraHandlerKind::Application),
    ("health", PingoraHandlerKind::Application),
    ("info", PingoraHandlerKind::Application),
    ("getLogger", PingoraHandlerKind::Application),
    ("postLogger", PingoraHandlerKind::Application),
    ("getLogContents", PingoraHandlerKind::Application),
    ("modules", PingoraHandlerKind::Application),
    ("configReload", PingoraHandlerKind::Application),
    ("spec", PingoraHandlerKind::Application),
    ("swaggerui", PingoraHandlerKind::Application),
    ("favicon", PingoraHandlerKind::Application),
    ("oauth", PingoraHandlerKind::Application),
    ("getOauth", PingoraHandlerKind::Application),
    ("shutdown", PingoraHandlerKind::Application),
    ("stateless", PingoraHandlerKind::Security),
    ("google", PingoraHandlerKind::Security),
    ("facebook", PingoraHandlerKind::Security),
    ("github", PingoraHandlerKind::Security),
    ("msal-exchange", PingoraHandlerKind::Security),
    ("websocket", PingoraHandlerKind::Traffic),
    ("mcp", PingoraHandlerKind::Application),
];

fn gateway_handler_descriptor(
    id: &'static str,
    kind: PingoraHandlerKind,
) -> PingoraHandlerDescriptor {
    PingoraHandlerDescriptor {
        id,
        kind,
        factory: build_registered_gateway_handler,
    }
}

fn build_registered_gateway_handler(
    ctx: &HandlerBuildContext<'_>,
) -> Result<Arc<dyn PingoraHandler>, RuntimeError> {
    let id: &'static str = Box::leak(ctx.handler_id.to_string().into_boxed_str());
    Ok(Arc::new(RegisteredGatewayHandler { id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig,
        ServiceIdentity,
    };
    use portal_registry::{
        PortalRegistryClient, RegistrationState, RegistryHandler, ServiceRegistrationParams,
    };
    use serde_json::{Value as JsonValue, json};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tempfile::TempDir;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{Duration as TokioDuration, sleep, timeout};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as WsServerRequest, Response as WsServerResponse,
    };
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::protocol::Message;
    use tokio_tungstenite::{accept_async, accept_hdr_async, connect_async};

    fn runtime_config(
        config_dir: &TempDir,
        external_config_dir: &TempDir,
        resolved_values: HashMap<String, serde_yaml::Value>,
    ) -> RuntimeConfig {
        let client = [
            external_config_dir.path().join(light_pingora::CLIENT_FILE),
            config_dir.path().join(light_pingora::CLIENT_FILE),
        ]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|content| serde_yaml::from_str::<ClientConfig>(&content).ok())
        });

        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: external_config_dir.path().to_path_buf(),
            resolved_values,
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn portal_access_control_exchange_derives_logical_hybrid_endpoint() {
        let body = br#"{
            "host":"lightapi.net",
            "service":"service",
            "action":"getApi",
            "version":"0.1.0",
            "data":{"hostId":"host-1","apiId":"api-1"}
        }"#;
        let exchange = access_control_exchange(
            "/portal/query@post",
            "/portal/query",
            None,
            Some(body),
            None,
        )
        .expect("hybrid exchange");
        assert_eq!(exchange.endpoint, "lightapi.net/service/getApi/0.1.0");
        assert_eq!(exchange.request_data["hostId"], "host-1");
    }

    #[test]
    fn portal_access_control_exchange_derives_endpoint_from_get_cmd() {
        let cmd = r#"{"host":"lightapi.net","service":"user","action":"getUnreadPrivateMessageCount","version":"0.1.0","data":{"hostId":"01964b05-552a-7c4b-9184-6857e7f3dc5f","userId":"01964b05-5532-7c79-8cde-191dcbd421b8"}}"#;
        let query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(cmd.as_bytes()).collect::<String>()
        );

        let exchange = access_control_exchange(
            "/portal/query@get",
            "/portal/query",
            Some(&query),
            None,
            None,
        )
        .expect("hybrid exchange from cmd");

        assert_eq!(
            exchange.endpoint,
            "lightapi.net/user/getUnreadPrivateMessageCount/0.1.0"
        );
        assert_eq!(
            exchange.request_data["hostId"],
            "01964b05-552a-7c4b-9184-6857e7f3dc5f"
        );
    }

    #[test]
    fn portal_access_control_exchange_derives_endpoint_from_json_rpc_body() {
        let body = br#"{
            "jsonrpc":"2.0",
            "method":"lightapi.net/rule/createRule/0.1.0",
            "params":{"hostId":"host-1","ruleId":"rule-1"},
            "id":"request-1"
        }"#;
        let exchange = access_control_exchange(
            "/portal/command@post",
            "/portal/command",
            None,
            Some(body),
            None,
        )
        .expect("hybrid exchange from json-rpc body");

        assert_eq!(exchange.endpoint, "lightapi.net/rule/createRule/0.1.0");
        assert_eq!(exchange.request_data["hostId"], "host-1");
    }

    #[test]
    fn portal_access_control_exchange_rejects_non_object_get_cmd() {
        let cmd = r#"["lightapi.net","user"]"#;
        let query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(cmd.as_bytes()).collect::<String>()
        );

        let rejection = access_control_exchange(
            "/portal/query@get",
            "/portal/query",
            Some(&query),
            None,
            None,
        )
        .expect_err("non-object cmd should fail");

        assert_eq!(rejection.status, 400);
        assert_eq!(rejection.code, "ERR13023");
        assert_eq!(
            rejection.message,
            "invalid hybrid portal request cmd: expected JSON object"
        );
    }

    #[derive(Debug, Clone)]
    struct ObservedBackendHandshake {
        path_and_query: String,
        authorization: Option<String>,
        agent_header: Option<String>,
        service_id_header: Option<String>,
        subprotocol: Option<String>,
    }

    struct NoopRegistryHandler;

    #[async_trait]
    impl RegistryHandler for NoopRegistryHandler {}

    async fn spawn_websocket_echo_backend() -> (
        std::net::SocketAddr,
        Arc<Mutex<Option<ObservedBackendHandshake>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo backend");
        let address = listener.local_addr().expect("echo backend address");
        let observed = Arc::new(Mutex::new(None));
        let observed_for_task = Arc::clone(&observed);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept echo connection");
            let observed_for_callback = Arc::clone(&observed_for_task);
            let callback = move |request: &WsServerRequest, mut response: WsServerResponse| {
                let subprotocol = header_value(request, "sec-websocket-protocol");
                *observed_for_callback.lock().expect("observed lock") =
                    Some(ObservedBackendHandshake {
                        path_and_query: request
                            .uri()
                            .path_and_query()
                            .map(|value| value.as_str().to_string())
                            .unwrap_or_else(|| request.uri().path().to_string()),
                        authorization: header_value(request, "authorization"),
                        agent_header: header_value(request, "x-agent-test"),
                        service_id_header: header_value(request, "service_id")
                            .or_else(|| header_value(request, "serviceId"))
                            .or_else(|| header_value(request, "Service-Id")),
                        subprotocol: subprotocol.clone(),
                    });
                if subprotocol
                    .as_deref()
                    .is_some_and(|value| websocket_protocol_contains(value, "chat.v1"))
                {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        HeaderValue::from_static("chat.v1"),
                    );
                }
                Ok(response)
            };
            let mut websocket = accept_hdr_async(stream, callback)
                .await
                .expect("accept echo websocket");
            while let Some(message) = websocket.next().await {
                match message.expect("echo websocket message") {
                    Message::Text(text) => {
                        websocket
                            .send(Message::Text(format!("echo:{text}").into()))
                            .await
                            .expect("send text echo");
                    }
                    Message::Binary(bytes) => {
                        websocket
                            .send(Message::Binary(bytes))
                            .await
                            .expect("send binary echo");
                    }
                    Message::Close(_) => {
                        break;
                    }
                    Message::Ping(bytes) => {
                        websocket
                            .send(Message::Pong(bytes))
                            .await
                            .expect("send pong");
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        });
        (address, observed, task)
    }

    async fn spawn_fake_registry(
        backend_address: std::net::SocketAddr,
    ) -> (
        String,
        oneshot::Receiver<JsonValue>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake registry");
        let address = listener.local_addr().expect("registry address");
        let (lookup_tx, lookup_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept registry connection");
            let mut websocket = accept_async(stream)
                .await
                .expect("accept registry websocket");

            let register = websocket
                .next()
                .await
                .expect("registry register message")
                .expect("valid registry register frame")
                .into_text()
                .expect("register text");
            let register_json =
                serde_json::from_str::<JsonValue>(&register).expect("register json");
            assert_eq!(register_json["method"], "service/register");
            websocket
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": register_json["id"],
                        "result": {
                            "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39",
                            "status": "registered"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send register ack");

            let mut lookup_tx = Some(lookup_tx);
            while let Some(message) = websocket.next().await {
                let message = message.expect("valid registry frame");
                let Message::Text(text) = message else {
                    continue;
                };
                let lookup_json =
                    serde_json::from_str::<JsonValue>(&text).expect("registry request json");
                if lookup_json["method"] != "discovery/lookup" {
                    continue;
                }
                if let Some(sender) = lookup_tx.take() {
                    let _ = sender.send(lookup_json.clone());
                }
                websocket
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "id": lookup_json["id"],
                            "result": {
                                "serviceId": lookup_json["params"]["serviceId"],
                                "envTag": lookup_json["params"]["envTag"],
                                "protocol": lookup_json["params"]["protocol"],
                                "nodes": [{
                                    "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40",
                                    "serviceId": lookup_json["params"]["serviceId"],
                                    "envTag": lookup_json["params"]["envTag"],
                                    "environment": "dev",
                                    "version": "1.0.0",
                                    "protocol": "http",
                                    "address": backend_address.ip().to_string(),
                                    "port": backend_address.port(),
                                    "tags": {},
                                    "connectedAt": "2026-01-01T00:00:00Z",
                                    "lastSeenAt": "2026-01-01T00:00:01Z",
                                    "connected": true
                                }]
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("send discovery response");
            }
        });
        (format!("ws://{address}"), lookup_rx, task)
    }

    async fn wait_for_registry_registration(
        receiver: &mut tokio::sync::watch::Receiver<RegistrationState>,
    ) {
        timeout(TokioDuration::from_secs(5), async {
            loop {
                if matches!(
                    receiver.borrow().clone(),
                    RegistrationState::Registered { .. }
                ) {
                    break;
                }
                receiver.changed().await.expect("registration state change");
            }
        })
        .await
        .expect("registry registration");
    }

    async fn wait_for_tcp(address: std::net::SocketAddr) {
        timeout(TokioDuration::from_secs(5), async {
            loop {
                if TcpStream::connect(address).await.is_ok() {
                    break;
                }
                sleep(TokioDuration::from_millis(20)).await;
            }
        })
        .await
        .expect("tcp listener ready");
    }

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind free port")
            .local_addr()
            .expect("free port address")
            .port()
    }

    fn header_value(request: &WsServerRequest, name: &str) -> Option<String> {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    fn websocket_protocol_contains(header: &str, expected: &str) -> bool {
        header
            .split(',')
            .any(|value| value.trim().eq_ignore_ascii_case(expected))
    }

    #[test]
    fn proxy_config_uses_runtime_resolved_values() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "enabled: ${proxy.enabled:true}\nhosts: ${proxy.hosts}\nrewriteHostHeader: ${proxy.rewriteHostHeader:true}\n",
        )
        .expect("write proxy config");
        let values = serde_yaml::from_str(
            r#"
proxy.hosts: https://api.example.com/base
proxy.rewriteHostHeader: false
"#,
        )
        .expect("parse values");

        let config = runtime_config(&config_dir, &external_dir, values);
        let route = load_proxy_route(&config)
            .expect("load proxy config")
            .expect("proxy route");

        assert!(!route.config.rewrite_host_header);
        assert_eq!(route.targets[0].address, "api.example.com:443");
        assert_eq!(route.targets[0].path_prefix, "/base");
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::PROXY_MODULE_ID && entry.reloadable)
        );
    }

    #[test]
    fn external_proxy_config_overlays_base_file() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8081\n",
        )
        .expect("write base proxy config");
        std::fs::write(
            external_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8082\n",
        )
        .expect("write external proxy config");

        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let route = load_proxy_route(&config)
            .expect("load proxy config")
            .expect("proxy route");

        assert_eq!(route.targets[0].address, "127.0.0.1:8082");
    }

    #[test]
    fn gateway_external_config_dir_is_separate_from_base_config() {
        assert_ne!(CONFIG_DIR, EXTERNAL_CONFIG_DIR);
    }

    #[test]
    fn gateway_loads_active_handlers_from_handler_yml() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
enabled: ${handler.enabled:true}
reportHandlerDuration: ${handler.reportHandlerDuration:false}
handlerMetricsLogLevel: ${handler.handlerMetricsLogLevel:DEBUG}
basePath: ${handler.basePath:/}
handlers: ${handler.handlers:[]}
chains: ${handler.chains:{}}
paths: ${handler.paths:[]}
defaultHandlers: ${handler.defaultHandlers:[]}
"#,
        )
        .expect("write handler config");
        let values = serde_yaml::from_str(
            r#"
handler.handlers:
  - correlation
  - headers
  - jwt
handler.chains:
  api:
    exec:
      - correlation
      - headers
handler.paths:
  - path: /v1/test
    method: GET
    exec:
      - api
handler.defaultHandlers: []
"#,
        )
        .expect("parse handler values");
        let config = runtime_config(&config_dir, &external_dir, values);

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy.active_handler_ids(),
            vec!["correlation".to_string(), "headers".to_string()]
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::HANDLER_MODULE_ID && entry.active)
        );
    }

    #[test]
    fn gateway_loads_static_resources_for_virtual_hosts() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let dist = config_dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist");
        std::fs::write(dist.join("index.html"), "<html></html>").expect("write index");
        std::fs::write(
            config_dir.path().join(light_pingora::VIRTUAL_HOST_FILE),
            r#"
hosts:
  - domain: local.localhost
    path: /
    base: dist
"#,
        )
        .expect("write virtual host config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert!(
            proxy
                .current_static_resources()
                .virtual_hosts
                .contains_key("local.localhost")
        );
    }

    #[test]
    fn static_file_validators_emit_http_cache_headers() {
        let config_dir = TempDir::new().expect("config temp dir");
        let file = config_dir.path().join("app.js");
        std::fs::write(&file, "console.log(1);").expect("write static file");
        let metadata = std::fs::metadata(&file).expect("metadata");

        let validators = static_file_validators(&metadata);

        assert!(validators.etag.starts_with("W/\""));
        let last_modified = validators
            .last_modified
            .as_deref()
            .expect("last modified header");
        assert!(parse_http_date(last_modified).is_some());
        assert!(etag_header_matches(
            &format!("\"other\", {}", validators.etag),
            &validators.etag
        ));
    }

    #[test]
    fn static_file_streaming_uses_transfer_threshold() {
        assert!(!should_stream_static_file(1024, 2048));
        assert!(should_stream_static_file(2048, 2048));
        assert!(should_stream_static_file(1, 0));
    }

    #[test]
    fn gateway_loads_router_only_when_router_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - router
defaultHandlers:
  - router
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::ROUTER_FILE),
            r#"
serviceTargets:
  com.networknt.petstore-1.0.0:
    - https://api.example.com/base
"#,
        )
        .expect("write router config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let router = proxy.current_router_route();
        let router = router.as_ref().as_ref().expect("router route");
        assert_eq!(
            router.service_targets["com.networknt.petstore-1.0.0"][0].address,
            "api.example.com:443"
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::ROUTER_MODULE_ID && entry.active)
        );
    }

    #[test]
    fn gateway_loads_path_prefix_and_token_when_handlers_are_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - prefix
  - token
defaultHandlers:
  - prefix
  - token
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir
                .path()
                .join(light_pingora::PATH_PREFIX_SERVICE_FILE),
            r#"
enabled: true
mapping:
  /v1/pets: com.networknt.petstore-1.0.0
"#,
        )
        .expect("write path prefix service config");
        std::fs::write(
            config_dir.path().join(light_pingora::TOKEN_FILE),
            r#"
enabled: true
appliedPathPrefixes:
  - /v1
"#,
        )
        .expect("write token config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  multipleAuthServers: false
  token:
    cache:
      capacity: 4
    server_url: http://localhost:6882
    client_credentials:
      uri: /oauth2/token
      client_id: client
      client_secret: secret
      scope:
        - petstore.r
pathPrefixServices:
  /v1/pets: com.networknt.petstore-1.0.0
request:
  connectTimeout: 100
  timeout: 200
"#,
        )
        .expect("write client config");
        let mut config = runtime_config(&config_dir, &external_dir, HashMap::new());
        config.cache_registry = Some(Arc::new(CacheRegistry::new()));

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_path_prefix_service_config()
                .as_ref()
                .as_ref()
                .expect("path prefix config")
                .mapping["/v1/pets"],
            "com.networknt.petstore-1.0.0"
        );
        let token_runtime = proxy.current_token_runtime();
        let token_runtime = token_runtime.as_ref().as_ref().expect("token runtime");
        assert_eq!(token_runtime.client_config().oauth.token.cache.capacity, 4);
        assert_eq!(
            token_runtime
                .handler_config()
                .applied_path_prefixes
                .as_slice(),
            ["/v1".to_string()]
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::TOKEN_MODULE_ID && entry.active)
        );
        assert!(
            config
                .cache_registry
                .as_ref()
                .expect("cache registry")
                .names()
                .contains(&light_pingora::TOKEN_CACHE_NAME.to_string())
        );
    }

    #[test]
    fn gateway_loads_stateless_auth_when_stateless_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - stateless
defaultHandlers:
  - stateless
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::STATELESS_AUTH_FILE),
            r#"
enabled: true
authPath: /authorization
logoutPath: /logout
cookieDomain: localhost
cookieSecure: true
"#,
        )
        .expect("write stateless config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  token:
    server_url: http://localhost:6882
    authorization_code:
      uri: /oauth2/token
      client_id: ac-client
      client_secret: ac-secret
    refresh_token:
      uri: /oauth2/token
      client_id: rt-client
      client_secret: rt-secret
"#,
        )
        .expect("write client config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let stateless = proxy.current_stateless_auth();
        let stateless = stateless.as_ref().as_ref().expect("stateless runtime");
        assert_eq!(stateless.config().auth_path, "/authorization");
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| {
                    entry.module_id == light_pingora::STATELESS_AUTH_MODULE_ID && entry.active
                })
        );
    }

    #[test]
    fn gateway_loads_msal_exchange_when_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - msal-exchange
paths:
  - path: /auth/ms/exchange
    method: POST
    exec:
      - msal-exchange
defaultHandlers:
  - msal-exchange
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_EXCHANGE_FILE),
            r#"
enabled: true
exchangePath: /auth/ms/exchange
logoutPath: /auth/ms/logout
subjectTokenType: urn:ietf:params:oauth:token-type:jwt
"#,
        )
        .expect("write msal config");
        std::fs::write(
            config_dir.path().join(light_pingora::SECURITY_MSAL_FILE),
            r#"
enableVerifyJwt: true
issuer: https://login.microsoftonline.com/tenant/v2.0
audience: spa-client
"#,
        )
        .expect("write security-msal config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  token:
    server_url: http://localhost:6882
    refresh_token:
      uri: /oauth2/token
      client_id: rt-client
      client_secret: rt-secret
    token_exchange:
      uri: /oauth2/token
      client_id: ex-client
      client_secret: ex-secret
"#,
        )
        .expect("write client config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let msal = proxy.current_msal_exchange();
        let msal = msal.as_ref().as_ref().expect("msal runtime");
        assert_eq!(msal.config().exchange_path, "/auth/ms/exchange");
        assert_eq!(
            msal.config().subject_token_type.as_deref(),
            Some("urn:ietf:params:oauth:token-type:jwt")
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| {
                    entry.module_id == light_pingora::MSAL_EXCHANGE_MODULE_ID && entry.active
                })
        );
    }

    #[test]
    fn gateway_loads_mcp_router_when_mcp_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    description: Get weather.
    targetHost: http://127.0.0.1:8080
    path: /weather
    method: GET
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let mcp = proxy.current_mcp_router();
        let mcp = mcp.as_ref().as_ref().expect("mcp runtime");
        assert!(mcp.matches_path("/mcp"));
        assert_eq!(mcp.config().tools[0].name, "weather");
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(
                    |entry| entry.module_id == light_pingora::MCP_ROUTER_MODULE_ID && entry.active
                )
        );
    }

    #[test]
    fn gateway_loads_websocket_router_when_websocket_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
defaultProtocol: https
defaultEnvTag: dev
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
"#,
        )
        .expect("write websocket config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let websocket = proxy.current_websocket_router();
        let websocket = websocket.as_ref().as_ref().expect("websocket runtime");
        assert_eq!(
            websocket.config().path_prefix_service["/chat"].service_id,
            "com.networknt.llmchat-1.0.0"
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(
                    |entry| entry.module_id == light_pingora::WEBSOCKET_ROUTER_MODULE_ID
                        && entry.active
                )
        );
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_mcp_router_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    targetHost: http://127.0.0.1:8080
    path: /weather
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_mcp_router()
                .as_ref()
                .as_ref()
                .expect("mcp runtime")
                .config()
                .tools[0]
                .name,
            "weather"
        );

        std::fs::write(
            external_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: forecast
    targetHost: http://127.0.0.1:8081
    path: /forecast
"#,
        )
        .expect("write external mcp config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::MCP_ROUTER_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![light_pingora::MCP_ROUTER_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_mcp_router()
                .as_ref()
                .as_ref()
                .expect("mcp runtime")
                .config()
                .tools[0]
                .name,
            "forecast"
        );
    }

    #[tokio::test]
    async fn gateway_mcp_session_survives_config_reload() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    targetHost: http://127.0.0.1:8080
    path: /weather
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        // --- Initialize a session before the reload ---
        let mcp = proxy.current_mcp_router();
        let mcp = mcp.as_ref().as_ref().expect("mcp runtime");
        let init_response = mcp
            .handle_request(light_pingora::McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![("accept".to_string(), "application/json".to_string())],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#
                    .to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(init_response.status, 200, "initialize must succeed");
        let session_id = init_response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(light_pingora::MCP_SESSION_ID_HEADER))
            .map(|(_, v)| v.clone())
            .expect("session id header after initialize");

        // --- Reload the MCP router config (simulates /reload endpoint) ---
        std::fs::write(
            external_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: forecast
    targetHost: http://127.0.0.1:8081
    path: /forecast
"#,
        )
        .expect("write updated mcp config");
        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::MCP_ROUTER_MODULE_ID.to_string()],
            )
            .await;
        assert!(result.failed.is_empty(), "reload must not fail");

        // --- Verify that the existing session survives the reload ---
        let mcp_after = proxy.current_mcp_router();
        let mcp_after = mcp_after
            .as_ref()
            .as_ref()
            .expect("mcp runtime after reload");
        // Config swap must have happened.
        assert_eq!(mcp_after.config().tools[0].name, "forecast");

        // tools/list with the original session ID must still succeed.
        let tools_response = mcp_after
            .handle_request(light_pingora::McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    ("accept".to_string(), "application/json".to_string()),
                    (
                        light_pingora::MCP_SESSION_ID_HEADER.to_string(),
                        session_id.clone(),
                    ),
                ],
                body: br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(
            tools_response.status, 200,
            "tools/list must succeed with pre-reload session after config reload"
        );
    }

    #[tokio::test]
    async fn gateway_client_config_reload() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");

        // Write initial client config with verifyHostname: false
        std::fs::write(
            config_dir.path().join("client.yml"),
            r#"
tls:
  verifyHostname: false
"#,
        )
        .expect("write client config");

        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        config
            .module_registry
            .register_runtime_configs(&config)
            .expect("register configs");
        let _proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        // Verify initial value in component configs
        let component_configs = config.module_registry.component_configs();
        assert_eq!(component_configs["client"]["tls"]["verifyHostname"], false);

        // Update client config on disk in external dir with verifyHostname: true
        std::fs::write(
            external_dir.path().join("client.yml"),
            r#"
tls:
  verifyHostname: true
"#,
        )
        .expect("write updated client config");

        // Reload the client module
        let reload_ctx = config.reload_context().await.expect("reload context");
        let result = config
            .module_registry
            .reload_modules(reload_ctx, &[light_runtime::CLIENT_MODULE_ID.to_string()])
            .await;

        assert_eq!(result.reloaded, vec![light_runtime::CLIENT_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());

        // Verify updated value in component configs
        let updated_configs = config.module_registry.component_configs();
        assert_eq!(updated_configs["client"]["tls"]["verifyHostname"], true);
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_websocket_router_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
"#,
        )
        .expect("write websocket config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_websocket_router()
                .as_ref()
                .as_ref()
                .expect("websocket runtime")
                .config()
                .path_prefix_service["/chat"]
                .service_id,
            "com.networknt.llmchat-1.0.0"
        );

        std::fs::write(
            external_dir
                .path()
                .join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
pathPrefixService:
  /chat: com.networknt.chat-v2-1.0.0
"#,
        )
        .expect("write external websocket config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::WEBSOCKET_ROUTER_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(
            result.reloaded,
            vec![light_pingora::WEBSOCKET_ROUTER_MODULE_ID]
        );
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_websocket_router()
                .as_ref()
                .as_ref()
                .expect("websocket runtime")
                .config()
                .path_prefix_service["/chat"]
                .service_id,
            "com.networknt.chat-v2-1.0.0"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_gateway_proxies_text_binary_close_subprotocol_and_headers() {
        let (backend_address, observed_backend, backend_task) =
            spawn_websocket_echo_backend().await;
        let (registry_url, lookup_rx, registry_task) = spawn_fake_registry(backend_address).await;
        let registry_client = Arc::new(
            PortalRegistryClient::new(
                registry_url.as_str(),
                ServiceRegistrationParams {
                    service_id: "light-gateway-test".to_string(),
                    version: "1.0.0".to_string(),
                    protocol: "http".to_string(),
                    address: "127.0.0.1".to_string(),
                    port: 0,
                    tags: HashMap::new(),
                    env_tag: Some("dev".to_string()),
                    jwt: "test-token".to_string(),
                },
                Arc::new(NoopRegistryHandler),
            )
            .expect("build registry client"),
        );
        let mut registration_rx = registry_client.subscribe_registration();
        let registry_client_task = tokio::spawn({
            let registry_client = Arc::clone(&registry_client);
            async move { registry_client.run().await }
        });
        wait_for_registry_registration(&mut registration_rx).await;

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                r#"
ip: 127.0.0.1
advertisedAddress: 127.0.0.1
httpPort: {gateway_port}
enableHttp: true
httpsPort: 8443
enableHttps: false
serviceId: com.networknt.light-gateway-1.0.0
enableRegistry: false
startOnRegistryFailure: true
dynamicPort: false
environment: dev
shutdownGracefulPeriod: 100
"#
            ),
        )
        .expect("write server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
defaultProtocol: http
defaultEnvTag: dev
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
    envTag: dev
"#,
        )
        .expect("write websocket config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .with_registry_client(Arc::clone(&registry_client))
            .build();
        let running = runtime.start().await.expect("start gateway");
        wait_for_tcp(gateway_address).await;

        let mut request = format!(
            "ws://127.0.0.1:{gateway_port}/chat?service_id=com.networknt.llmchat-1.0.0&protocol=http&env_tag=dev&room=one"
        )
        .into_client_request()
        .expect("websocket client request");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("chat.v1"),
        );
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_static("Bearer agent-token"),
        );
        request
            .headers_mut()
            .insert("x-agent-test", HeaderValue::from_static("present"));
        request.headers_mut().insert(
            "service_id",
            HeaderValue::from_static("com.networknt.llmchat-1.0.0"),
        );

        let (mut websocket, response) =
            timeout(TokioDuration::from_secs(5), connect_async(request))
                .await
                .expect("connect timeout")
                .expect("connect through gateway");
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("chat.v1")
        );

        let lookup = timeout(TokioDuration::from_secs(5), lookup_rx)
            .await
            .expect("lookup timeout")
            .expect("lookup payload");
        assert_eq!(lookup["method"], "discovery/lookup");
        assert_eq!(lookup["params"]["serviceId"], "com.networknt.llmchat-1.0.0");
        assert_eq!(lookup["params"]["envTag"], "dev");
        assert_eq!(lookup["params"]["protocol"], "http");

        let observed = observed_backend
            .lock()
            .expect("observed backend lock")
            .clone()
            .expect("backend handshake observed");
        assert_eq!(observed.path_and_query, "/chat?room=one");
        assert_eq!(
            observed.authorization.as_deref(),
            Some("Bearer agent-token")
        );
        assert_eq!(observed.agent_header.as_deref(), Some("present"));
        assert_eq!(observed.service_id_header, None);
        assert!(
            observed
                .subprotocol
                .as_deref()
                .is_some_and(|value| websocket_protocol_contains(value, "chat.v1"))
        );

        websocket
            .send(Message::Text("hello".into()))
            .await
            .expect("send text");
        let text = timeout(TokioDuration::from_secs(5), websocket.next())
            .await
            .expect("text timeout")
            .expect("text frame")
            .expect("valid text frame")
            .into_text()
            .expect("text payload");
        assert_eq!(text, "echo:hello");

        websocket
            .send(Message::Binary(vec![1_u8, 2, 3, 4].into()))
            .await
            .expect("send binary");
        let binary = timeout(TokioDuration::from_secs(5), websocket.next())
            .await
            .expect("binary timeout")
            .expect("binary frame")
            .expect("valid binary frame")
            .into_data();
        assert_eq!(binary.as_slice(), &[1_u8, 2, 3, 4]);

        websocket.close(None).await.expect("close websocket");
        timeout(TokioDuration::from_secs(5), async {
            while let Some(message) = websocket.next().await {
                match message {
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .await
        .expect("close timeout");
        timeout(TokioDuration::from_secs(5), backend_task)
            .await
            .expect("backend close timeout")
            .expect("backend task");

        running.shutdown().await.expect("shutdown gateway");
        registry_client_task.abort();
        registry_task.abort();
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_proxy_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8081\n",
        )
        .expect("write proxy config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_proxy_route()
                .as_ref()
                .as_ref()
                .expect("proxy route")
                .targets[0]
                .address,
            "127.0.0.1:8081"
        );

        std::fs::write(
            external_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8082\n",
        )
        .expect("write external proxy config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::PROXY_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![light_pingora::PROXY_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_proxy_route()
                .as_ref()
                .as_ref()
                .expect("proxy route")
                .targets[0]
                .address,
            "127.0.0.1:8082"
        );
    }
}
