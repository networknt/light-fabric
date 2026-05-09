use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use light_pingora::{
    ActiveHandlerSet, HandlerBuildContext, PingoraApp, PingoraHandler, PingoraHandlerDescriptor,
    PingoraHandlerKind, PingoraHandlerRegistry, PingoraTransport, load_active_handlers,
};
use light_runtime::{
    ConfigManager, LightRuntimeBuilder, ModuleKind, ReloadContext, ReloadOutcome, ReloadableModule,
    RuntimeConfig, RuntimeError,
};
use pingora::http::ResponseHeader;
use pingora::prelude::{HttpPeer, ProxyHttp, Session};
use serde::{Deserialize, Serialize};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;
use tracing_subscriber::EnvFilter;

const GATEWAY_FILE: &str = "gateway.yml";
const GATEWAY_MODULE_ID: &str = "light-gateway/gateway";
const GATEWAY_CONFIG_NAME: &str = "gateway";
const CONFIG_DIR: &str = "config";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GatewayConfig {
    upstreams: Vec<GatewayUpstream>,
    #[serde(default = "default_health_path")]
    health_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct GatewayUpstream {
    address: String,
    #[serde(default)]
    tls: bool,
    #[serde(default)]
    sni: String,
    #[serde(default)]
    host_header: Option<String>,
}

#[derive(Clone)]
struct GatewayApp;

impl PingoraApp for GatewayApp {
    type Proxy = GatewayProxy;

    fn proxy(&self, config: &RuntimeConfig) -> Result<Self::Proxy, RuntimeError> {
        GatewayProxy::from_runtime_config(config)
    }
}

struct GatewayProxy {
    config: Arc<ConfigManager<GatewayConfig>>,
    _active_handlers: ActiveHandlerSet,
    next_upstream: AtomicUsize,
}

impl GatewayProxy {
    fn from_runtime_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let gateway_config = load_gateway_config(config)?;
        let active_handlers = load_active_handlers(config, &gateway_handler_registry())?;
        let config_manager = Arc::new(ConfigManager::new(gateway_config));
        config.module_registry.register_reloader(
            GATEWAY_MODULE_ID,
            Arc::new(GatewayReloader {
                config: Arc::clone(&config_manager),
            }),
        );

        Ok(Self {
            config: config_manager,
            _active_handlers: active_handlers,
            next_upstream: AtomicUsize::new(0),
        })
    }

    fn select_upstream(&self) -> GatewayUpstream {
        let config = self.config.load();
        let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
        config.upstreams[index % config.upstreams.len()].clone()
    }

    #[cfg(test)]
    fn current_config(&self) -> Arc<GatewayConfig> {
        self.config.load()
    }

    #[cfg(test)]
    fn active_handler_ids(&self) -> &[String] {
        self._active_handlers.active_handler_ids()
    }
}

struct GatewayReloader {
    config: Arc<ConfigManager<GatewayConfig>>,
}

#[async_trait]
impl ReloadableModule for GatewayReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let gateway_config = load_gateway_config(&ctx.runtime_config)?;
        self.config.store(gateway_config);
        Ok(ReloadOutcome::success("gateway.yml reloaded"))
    }
}

#[async_trait]
impl ProxyHttp for GatewayProxy {
    type CTX = Option<String>;

    fn new_ctx(&self) -> Self::CTX {
        None
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> pingora::Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        let config = self.config.load();
        if session.req_header().uri.path() == config.health_path {
            let body = Bytes::from_static(b"ok");
            let mut response = ResponseHeader::build(200, Some(2))?;
            response.insert_header("content-type", "text/plain")?;
            response.set_content_length(body.len())?;
            session
                .write_response_header(Box::new(response), false)
                .await?;
            session.write_response_body(Some(body), true).await?;
            return Ok(true);
        }

        Ok(false)
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let upstream = self.select_upstream();
        *ctx = upstream.host_header.clone();

        let sni = if upstream.sni.trim().is_empty() {
            upstream
                .address
                .split_once(':')
                .map(|(host, _)| host)
                .unwrap_or(upstream.address.as_str())
                .to_string()
        } else {
            upstream.sni.clone()
        };
        info!("proxying request to {}", upstream.address);
        Ok(Box::new(HttpPeer::new(
            upstream.address.as_str(),
            upstream.tls,
            sni,
        )))
    }

    async fn upstream_request_filter(
        &self,
        _session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if let Some(host_header) = ctx.as_deref() {
            upstream_request.insert_header("host", host_header)?;
        }
        upstream_request.insert_header("x-light-gateway", "light-pingora")?;
        Ok(())
    }
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

fn load_gateway_config(config: &RuntimeConfig) -> Result<GatewayConfig, RuntimeError> {
    let gateway_config = config
        .module_registry
        .load_config::<GatewayConfig>(config, GATEWAY_FILE)?;
    validate_gateway_config(&gateway_config)?;
    config.module_registry.register_loaded_config(
        GATEWAY_MODULE_ID,
        GATEWAY_CONFIG_NAME,
        ModuleKind::Application,
        &gateway_config,
        [],
        true,
        Some(true),
        true,
    )?;
    Ok(gateway_config)
}

fn validate_gateway_config(gateway_config: &GatewayConfig) -> Result<(), RuntimeError> {
    if gateway_config.upstreams.is_empty() {
        return Err(RuntimeError::Unsupported(
            "gateway.upstreams must contain at least one upstream".to_string(),
        ));
    }

    for upstream in &gateway_config.upstreams {
        upstream
            .address
            .to_socket_addrs()
            .map_err(|e| {
                RuntimeError::Unsupported(format!(
                    "invalid gateway upstream `{}`: {e}",
                    upstream.address
                ))
            })?
            .next()
            .ok_or_else(|| {
                RuntimeError::Unsupported(format!(
                    "gateway upstream `{}` did not resolve to any socket address",
                    upstream.address
                ))
            })?;
    }
    Ok(())
}

fn default_health_path() -> String {
    "/health".to_string()
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
    PingoraHandlerRegistry::new()
        .register(gateway_handler_descriptor(
            "correlation",
            PingoraHandlerKind::Observability,
        ))
        .register(gateway_handler_descriptor(
            "headers",
            PingoraHandlerKind::Traffic,
        ))
        .register(gateway_handler_descriptor(
            "metrics",
            PingoraHandlerKind::Observability,
        ))
        .register(gateway_handler_descriptor(
            "cors",
            PingoraHandlerKind::Traffic,
        ))
        .register(gateway_handler_descriptor(
            "jwt",
            PingoraHandlerKind::Security,
        ))
        .register(gateway_handler_descriptor(
            "api-key",
            PingoraHandlerKind::Security,
        ))
        .register(gateway_handler_descriptor(
            "basic-auth",
            PingoraHandlerKind::Security,
        ))
        .register(gateway_handler_descriptor(
            "rate-limit",
            PingoraHandlerKind::Traffic,
        ))
        .register(gateway_handler_descriptor(
            "request-size-limit",
            PingoraHandlerKind::Traffic,
        ))
}

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
    let id = match ctx.declaration.id.as_str() {
        "correlation" => "correlation",
        "headers" => "headers",
        "metrics" => "metrics",
        "cors" => "cors",
        "jwt" => "jwt",
        "api-key" => "api-key",
        "basic-auth" => "basic-auth",
        "rate-limit" => "rate-limit",
        "request-size-limit" => "request-size-limit",
        other => {
            return Err(RuntimeError::Unsupported(format!(
                "handler `{other}` is not registered in light-gateway"
            )));
        }
    };
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
    fn gateway_config_uses_runtime_resolved_values() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(GATEWAY_FILE),
            "healthPath: ${gateway.healthPath:/health}\nupstreams: ${gateway.upstreams}\n",
        )
        .expect("write gateway config");
        let values = serde_yaml::from_str(
            r#"
gateway.healthPath: /ready
gateway.upstreams:
  - address: 127.0.0.1:8081
    hostHeader: example.com
"#,
        )
        .expect("parse values");

        let config = runtime_config(&config_dir, &external_dir, values);
        let gateway = load_gateway_config(&config).expect("load gateway config");

        assert_eq!(gateway.health_path, "/ready");
        assert_eq!(gateway.upstreams.len(), 1);
        assert_eq!(gateway.upstreams[0].address, "127.0.0.1:8081");
        assert_eq!(
            gateway.upstreams[0].host_header.as_deref(),
            Some("example.com")
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == GATEWAY_MODULE_ID && entry.reloadable)
        );
    }

    #[test]
    fn external_gateway_config_overlays_base_file() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(GATEWAY_FILE),
            "healthPath: /base\nupstreams: ${gateway.upstreams}\n",
        )
        .expect("write base gateway config");
        std::fs::write(
            external_dir.path().join(GATEWAY_FILE),
            "healthPath: /external\n",
        )
        .expect("write external gateway config");
        let values = serde_yaml::from_str(
            r#"
gateway.upstreams:
  - address: 127.0.0.1:8081
"#,
        )
        .expect("parse values");

        let config = runtime_config(&config_dir, &external_dir, values);
        let gateway = load_gateway_config(&config).expect("load gateway config");

        assert_eq!(gateway.health_path, "/external");
        assert_eq!(gateway.upstreams[0].address, "127.0.0.1:8081");
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
            config_dir.path().join(GATEWAY_FILE),
            r#"
healthPath: /ready
upstreams:
  - address: 127.0.0.1:8081
"#,
        )
        .expect("write gateway config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
enabled: true
handlers:
  - id: correlation
  - id: headers
  - id: jwt
chains:
  api:
    - correlation
    - headers
defaultHandlers:
  - api
"#,
        )
        .expect("write handler config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy.active_handler_ids(),
            &["correlation".to_string(), "headers".to_string()]
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::HANDLER_MODULE_ID && entry.active)
        );
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_proxy_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(GATEWAY_FILE),
            r#"
healthPath: /ready
upstreams:
  - address: 127.0.0.1:8081
"#,
        )
        .expect("write gateway config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(proxy.current_config().health_path, "/ready");
        assert_eq!(
            proxy.current_config().upstreams[0].address,
            "127.0.0.1:8081"
        );

        std::fs::write(
            external_dir.path().join(GATEWAY_FILE),
            r#"
healthPath: /live
upstreams:
  - address: 127.0.0.1:8082
    hostHeader: live.example
"#,
        )
        .expect("write external gateway config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[GATEWAY_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![GATEWAY_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(proxy.current_config().health_path, "/live");
        assert_eq!(
            proxy.current_config().upstreams[0].address,
            "127.0.0.1:8082"
        );
        assert_eq!(
            proxy.current_config().upstreams[0].host_header.as_deref(),
            Some("live.example")
        );
    }
}
