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
use std::thread::JoinHandle;
#[cfg(unix)]
use tokio::sync::watch;

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
