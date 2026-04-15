use async_trait::async_trait;
use axum::Router;
use axum_server::Handle;
use light_runtime::{
    BoundTransport, ResolvedServerMetadata, RuntimeConfig, RuntimeError, TransportRuntime,
};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct ServerContext {
    pub runtime_config: Arc<RuntimeConfig>,
}

pub trait AxumApp: Send + Sync + 'static {
    fn router(&self, context: ServerContext) -> Router;
}

pub struct AxumTransport<A>
where
    A: AxumApp,
{
    app: Arc<A>,
}

impl<A> AxumTransport<A>
where
    A: AxumApp,
{
    pub fn new(app: A) -> Self {
        Self { app: Arc::new(app) }
    }
}

pub struct AxumBoundHandle {
    shutdown: Handle,
    task: JoinHandle<()>,
}

#[async_trait]
impl<A> TransportRuntime for AxumTransport<A>
where
    A: AxumApp,
{
    type Handle = AxumBoundHandle;

    async fn bind(
        &self,
        config: &RuntimeConfig,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        let protocol = if config.server.enable_https {
            "https"
        } else if config.server.enable_http {
            "http"
        } else {
            return Err(RuntimeError::Unsupported(
                "server must enable either HTTP or HTTPS".to_string(),
            ));
        };

        let desired_port = if config.server.dynamic_port {
            0
        } else if config.server.enable_https {
            config.server.https_port
        } else {
            config.server.http_port
        };

        let addr: SocketAddr = format!("{}:{desired_port}", config.server.ip)
            .parse()
            .map_err(|e| RuntimeError::Unsupported(format!("invalid bind address: {e}")))?;
        let handle = Handle::new();
        let context = ServerContext {
            runtime_config: Arc::new(config.clone()),
        };
        let app = self.app.router(context);
        let server_handle = handle.clone();

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(RuntimeError::Io)?;
        let local_addr = listener.local_addr().map_err(RuntimeError::Io)?;
        let advertised_address = resolve_advertised_address(config, local_addr.ip())?;
        let std_listener = listener.into_std().map_err(RuntimeError::Io)?;
        std_listener
            .set_nonblocking(true)
            .map_err(RuntimeError::Io)?;

        let task = if protocol == "https" {
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
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path)
                .await
                .map_err(|e| RuntimeError::Unsupported(format!("invalid TLS config: {e}")))?;
            tokio::spawn(async move {
                if let Err(error) = axum_server::from_tcp_rustls(std_listener, tls)
                    .handle(server_handle.clone())
                    .serve(app.into_make_service())
                    .await
                {
                    tracing::error!("axum server exited with error: {error}");
                }
            })
        } else {
            tokio::spawn(async move {
                if let Err(error) = axum_server::from_tcp(std_listener)
                    .handle(server_handle.clone())
                    .serve(app.into_make_service())
                    .await
                {
                    tracing::error!("axum server exited with error: {error}");
                }
            })
        };

        Ok(BoundTransport {
            handle: AxumBoundHandle {
                shutdown: handle,
                task,
            },
            metadata: ResolvedServerMetadata {
                protocol: protocol.to_string(),
                address: advertised_address,
                port: local_addr.port(),
                tags: Default::default(),
            },
        })
    }

    async fn stop(&self, handle: &mut Self::Handle) -> Result<(), RuntimeError> {
        handle.shutdown.graceful_shutdown(None);
        let task = std::mem::replace(&mut handle.task, tokio::spawn(async {}));
        task.await
            .map_err(|e| RuntimeError::Unsupported(format!("server task join failed: {e}")))?;
        Ok(())
    }
}

fn resolve_advertised_address(
    config: &RuntimeConfig,
    bound_ip: IpAddr,
) -> Result<String, RuntimeError> {
    if let Some(address) = config.server.advertised_address.as_deref() {
        let trimmed = address.trim();
        if trimmed.is_empty() {
            return Err(RuntimeError::Unsupported(
                "server.advertisedAddress must not be empty when provided".to_string(),
            ));
        }
        return Ok(trimmed.to_string());
    }

    Ok(bound_ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::resolve_advertised_address;
    use light_runtime::{BootstrapConfig, RuntimeConfig, RuntimeError, ServerConfig, ServiceIdentity};
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;

    fn runtime_config() -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            service_identity: ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config"),
        }
    }

    #[test]
    fn uses_explicit_advertised_address_when_present() {
        let mut config = runtime_config();
        config.server.advertised_address = Some("172.18.0.10".to_string());

        let address = resolve_advertised_address(
            &config,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        )
        .expect("resolve advertised address");

        assert_eq!(address, "172.18.0.10");
    }

    #[test]
    fn falls_back_to_unspecified_bound_ip_without_failing() {
        let config = runtime_config();

        let address = resolve_advertised_address(
            &config,
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        )
        .expect("resolve advertised address");

        assert_eq!(address, "0.0.0.0");
    }

    #[test]
    fn rejects_empty_explicit_advertised_address() {
        let mut config = runtime_config();
        config.server.advertised_address = Some("   ".to_string());

        let error = resolve_advertised_address(
            &config,
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        )
        .expect_err("empty advertised address should fail");

        assert!(matches!(error, RuntimeError::Unsupported(_)));
    }
}
