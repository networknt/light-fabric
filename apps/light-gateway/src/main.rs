use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use config_loader::ConfigLoader;
use light_pingora::{PingoraApp, PingoraTransport};
use light_runtime::{LightRuntimeBuilder, RuntimeConfig, RuntimeError};
use pingora::http::ResponseHeader;
use pingora::prelude::{HttpPeer, ProxyHttp, Session};
use serde::Deserialize;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;
use tracing_subscriber::EnvFilter;

const VALUES_FILE: &str = "values.yml";
const GATEWAY_FILE: &str = "gateway.yml";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayConfig {
    upstreams: Vec<GatewayUpstream>,
    #[serde(default = "default_health_path")]
    health_path: String,
}

#[derive(Debug, Clone, Deserialize)]
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
    upstreams: Arc<Vec<GatewayUpstream>>,
    next_upstream: AtomicUsize,
    health_path: String,
}

impl GatewayProxy {
    fn from_runtime_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let gateway_config = load_gateway_config(config)?;
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

        Ok(Self {
            upstreams: Arc::new(gateway_config.upstreams),
            next_upstream: AtomicUsize::new(0),
            health_path: gateway_config.health_path,
        })
    }

    fn select_upstream(&self) -> &GatewayUpstream {
        let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
        &self.upstreams[index % self.upstreams.len()]
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
        if session.req_header().uri.path() == self.health_path {
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
        .with_config_dir("config")
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
    let values = load_values(&config.config_dir, &config.external_config_dir)?;
    let password = std::env::var("light_4j_config_password").ok();
    let loader = ConfigLoader::from_values(values, password.as_deref(), None)?;

    let mut paths = Vec::new();
    let base_path = config.config_dir.join(GATEWAY_FILE);
    if base_path.exists() {
        paths.push(base_path);
    }
    let external_path = config.external_config_dir.join(GATEWAY_FILE);
    if external_path.exists() && !paths.iter().any(|path| path == &external_path) {
        paths.push(external_path);
    }
    if paths.is_empty() {
        return Err(RuntimeError::MissingConfig(GATEWAY_FILE.to_string()));
    }

    let merged = loader.load_merged_files(paths.iter().map(PathBuf::as_path))?;
    serde_yaml::from_value(merged).map_err(RuntimeError::Yaml)
}

fn load_values(
    config_dir: &Path,
    external_config_dir: &Path,
) -> Result<std::collections::HashMap<String, serde_yaml::Value>, RuntimeError> {
    let mut values = std::collections::HashMap::new();
    for path in [
        config_dir.join(VALUES_FILE),
        external_config_dir.join(VALUES_FILE),
    ] {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let parsed: std::collections::HashMap<String, serde_yaml::Value> =
                serde_yaml::from_str(&content)?;
            values.extend(parsed);
        }
    }
    Ok(values)
}

fn default_health_path() -> String {
    "/health".to_string()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
