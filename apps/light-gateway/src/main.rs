use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use light_pingora::{
    ActiveHandlerSet, HandlerBuildContext, PingoraApp, PingoraHandler, PingoraHandlerDescriptor,
    PingoraHandlerKind, PingoraHandlerRegistry, PingoraTransport, ProxyRoute, ProxyTarget,
    StaticResolution, StaticResourceSet, load_active_handlers, load_proxy_route,
    load_static_resources,
};
use light_runtime::{
    ConfigManager, LightRuntimeBuilder, ReloadContext, ReloadOutcome, ReloadableModule,
    RuntimeConfig, RuntimeError,
};
use pingora::http::ResponseHeader;
use pingora::prelude::{HttpPeer, ProxyHttp, Session};
use pingora::{Error, ErrorType};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;
use tracing_subscriber::EnvFilter;

const CONFIG_DIR: &str = "config";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const HEALTH_PATH: &str = "/health";

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
    proxy_route: Arc<ConfigManager<Option<ProxyRoute>>>,
    static_resources: Arc<ConfigManager<StaticResourceSet>>,
    next_upstream: AtomicUsize,
}

impl GatewayProxy {
    fn from_runtime_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let active_handlers = load_active_handlers(config, &gateway_handler_registry())?;
        let proxy_route = load_proxy_route(config)?;
        let static_resources = load_static_resources(config)?;
        let active_handlers = Arc::new(ConfigManager::new(active_handlers));
        let proxy_route = Arc::new(ConfigManager::new(proxy_route));
        let static_resources = Arc::new(ConfigManager::new(static_resources));

        config.module_registry.register_reloader(
            light_pingora::HANDLER_MODULE_ID,
            Arc::new(HandlerReloader {
                active_handlers: Arc::clone(&active_handlers),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::PROXY_MODULE_ID,
            Arc::new(ProxyReloader {
                proxy_route: Arc::clone(&proxy_route),
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

        Ok(Self {
            active_handlers,
            proxy_route,
            static_resources,
            next_upstream: AtomicUsize::new(0),
        })
    }

    fn select_upstream(&self) -> Option<(ProxyTarget, bool)> {
        let route = self.proxy_route.load();
        let route = route.as_ref().as_ref()?;
        let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
        route
            .select(index)
            .map(|target| (target, route.rewrite_host_header()))
    }

    #[cfg(test)]
    fn current_proxy_route(&self) -> Arc<Option<ProxyRoute>> {
        self.proxy_route.load()
    }

    #[cfg(test)]
    fn current_static_resources(&self) -> Arc<StaticResourceSet> {
        self.static_resources.load()
    }

    #[cfg(test)]
    fn active_handler_ids(&self) -> Vec<String> {
        self.active_handlers.load().active_handler_ids().to_vec()
    }
}

struct HandlerReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
}

#[async_trait]
impl ReloadableModule for HandlerReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers =
            load_active_handlers(&ctx.runtime_config, &gateway_handler_registry())?;
        self.active_handlers.store(active_handlers);
        Ok(ReloadOutcome::success("handler.yml reloaded"))
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
        let request_path = session.req_header().uri.path().to_string();
        if request_path == HEALTH_PATH {
            return write_text_response(session, 200, "ok").await;
        }

        let method = session.req_header().method.as_str().to_string();
        let handler_ids = self
            .active_handlers
            .load()
            .resolve_handler_ids(&request_path, &method)
            .map_err(pingora_internal_error)?;

        if handler_ids.is_empty() {
            if let Some((target, rewrite_host_header)) = self.select_upstream() {
                ctx.proxy_target = Some(target);
                ctx.rewrite_host_header = rewrite_host_header;
                return Ok(false);
            }
            return write_text_response(session, 404, "not found").await;
        }

        for handler_id in handler_ids {
            match handler_id.as_str() {
                "health" => return write_text_response(session, 200, "ok").await,
                "virtual" => {
                    let host_header = session
                        .req_header()
                        .headers
                        .get("host")
                        .and_then(|value| value.to_str().ok());
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_virtual_host(host_header, &request_path);
                    return write_static_resolution(session, resolution).await;
                }
                "path-resource" | "resource" => {
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_path_resource(&request_path);
                    return write_static_resolution(session, resolution).await;
                }
                "proxy" => {
                    if let Some((target, rewrite_host_header)) = self.select_upstream() {
                        ctx.proxy_target = Some(target);
                        ctx.rewrite_host_header = rewrite_host_header;
                        return Ok(false);
                    }
                    return write_text_response(session, 502, "proxy is not configured").await;
                }
                "router" => {
                    return write_text_response(
                        session,
                        501,
                        "router handler is not implemented in phase 2",
                    )
                    .await;
                }
                _ => {}
            }
        }

        write_text_response(session, 404, "not found").await
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
        info!("proxying request to {}", upstream.address);
        Ok(Box::new(HttpPeer::new(
            upstream.address.as_str(),
            upstream.tls,
            upstream.sni.clone(),
        )))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if let Some(target) = ctx.proxy_target.as_ref() {
            if ctx.rewrite_host_header {
                upstream_request.insert_header("host", target.host_header.as_str())?;
            }
            if !target.path_prefix.is_empty() {
                rewrite_upstream_path(upstream_request, &target.path_prefix)?;
            }
        }
        upstream_request.insert_header("x-light-gateway", "light-pingora")?;
        Ok(())
    }
}

#[derive(Default)]
struct GatewayRequestContext {
    proxy_target: Option<ProxyTarget>,
    rewrite_host_header: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp))
        .with_config_dir(CONFIG_DIR)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
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

async fn write_static_resolution(
    session: &mut Session,
    resolution: StaticResolution,
) -> pingora::Result<bool> {
    match resolution {
        StaticResolution::File(file) => {
            let body = tokio::fs::read(&file.path).await.map_err(|error| {
                Error::because(
                    ErrorType::FileReadError,
                    format!("failed to read static file `{}`", file.path.display()),
                    error,
                )
            })?;
            write_bytes_response(
                session,
                200,
                file.content_type.as_str(),
                Some(file.cache_control.as_str()),
                Bytes::from(body),
            )
            .await
        }
        StaticResolution::Forbidden => write_text_response(session, 403, "forbidden").await,
        StaticResolution::NotFound => write_text_response(session, 404, "not found").await,
    }
}

async fn write_text_response(
    session: &mut Session,
    status: u16,
    body: &'static str,
) -> pingora::Result<bool> {
    write_bytes_response(
        session,
        status,
        "text/plain; charset=utf-8",
        None,
        Bytes::from_static(body.as_bytes()),
    )
    .await
}

async fn write_bytes_response(
    session: &mut Session,
    status: u16,
    content_type: &str,
    cache_control: Option<&str>,
    body: Bytes,
) -> pingora::Result<bool> {
    let is_head = session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("HEAD");
    let mut response = ResponseHeader::build(status, Some(4))?;
    response.insert_header("content-type", content_type)?;
    if let Some(cache_control) = cache_control {
        response.insert_header("cache-control", cache_control)?;
    }
    response.set_content_length(body.len())?;
    session
        .write_response_header(Box::new(response), is_head)
        .await?;
    if !is_head {
        session.write_response_body(Some(body), true).await?;
    }
    Ok(true)
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

fn pingora_internal_error(error: RuntimeError) -> Box<Error> {
    Error::because(ErrorType::InternalError, error.to_string(), error)
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
    ("basic-auth", PingoraHandlerKind::Security),
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
    ("token", PingoraHandlerKind::Security),
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig, ServiceIdentity,
    };
    use std::collections::HashMap;
    use tempfile::TempDir;

    fn runtime_config(
        config_dir: &TempDir,
        external_config_dir: &TempDir,
        resolved_values: HashMap<String, serde_yaml::Value>,
    ) -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: external_config_dir.path().to_path_buf(),
            resolved_values,
            module_registry: Arc::new(ModuleRegistry::new()),
        }
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
