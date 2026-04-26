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
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;
use tracing_subscriber::EnvFilter;

const GATEWAY_FILE: &str = "gateway.yml";
const CONFIG_DIR: &str = "config";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";

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
    let password = std::env::var("light_4j_config_password").ok();
    let loader =
        ConfigLoader::from_values(config.resolved_values.clone(), password.as_deref(), None)?;

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

fn default_health_path() -> String {
    "/health".to_string()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{BootstrapConfig, PortalRegistryConfig, ServerConfig, ServiceIdentity};
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
}
