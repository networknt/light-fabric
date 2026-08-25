use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use axum_server::Handle;
use light_runtime::{
    AdmissionGate, AdmissionKind, BoundTransport, LifecycleRegistrar, ResolvedServerMetadata,
    RuntimeConfig, RuntimeError, ShutdownContext, TransportRuntime,
};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ServerContext {
    pub runtime_config: Arc<RuntimeConfig>,
    pub lifecycle: LifecycleRegistrar,
    pub admission: AdmissionGate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlRouteKind {
    Liveness,
    Readiness,
    Metrics,
    ShutdownStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlRoute {
    pub method: &'static str,
    pub path: &'static str,
    pub kind: ControlRouteKind,
}

#[async_trait]
pub trait AxumApp: Send + Sync + 'static {
    async fn router(&self, context: ServerContext) -> Result<Router, RuntimeError>;

    /// Returns the complete controller-registration tag map after router
    /// construction has validated and initialized the application.
    fn registration_tags(&self) -> HashMap<String, String> {
        HashMap::new()
    }

    fn control_routes(&self) -> &'static [ControlRoute] {
        &[]
    }
}

#[derive(Clone)]
struct AdmissionState {
    admission: AdmissionGate,
    control_routes: &'static [ControlRoute],
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
    task: Option<JoinHandle<()>>,
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
        lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
        startup_cancel: CancellationToken,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        if startup_cancel.is_cancelled() {
            return Err(RuntimeError::StartupAborted);
        }
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

        let addr = bind_addr(config.server.ip.as_str(), desired_port)?;
        let handle = Handle::new();
        let context = ServerContext {
            runtime_config: Arc::new(config.clone()),
            lifecycle: lifecycle.clone(),
            admission: admission.clone(),
        };
        let admission_state = AdmissionState {
            admission: admission.clone(),
            control_routes: self.app.control_routes(),
        };
        let app = self
            .app
            .router(context)
            .await?
            .layer(middleware::from_fn_with_state(
                admission_state,
                admission_middleware,
            ));
        let server_handle = handle.clone();

        let listener = tokio::select! {
            biased;
            _ = startup_cancel.cancelled() => return Err(RuntimeError::StartupAborted),
            result = tokio::net::TcpListener::bind(addr) => result.map_err(RuntimeError::Io)?,
        };
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
            let tls = tokio::select! {
                biased;
                _ = startup_cancel.cancelled() => return Err(RuntimeError::StartupAborted),
                result = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert_path, key_path) =>
                    result.map_err(|e| RuntimeError::Unsupported(format!("invalid TLS config: {e}")))?,
            };
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
                task: Some(task),
            },
            metadata: ResolvedServerMetadata {
                protocol: protocol.to_string(),
                address: advertised_address,
                port: local_addr.port(),
                tags: self.app.registration_tags(),
            },
        })
    }

    async fn stop(
        &self,
        handle: &mut Self::Handle,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        let shutdown_budget = context.remaining();
        handle.shutdown.graceful_shutdown(Some(shutdown_budget));
        let Some(task) = handle.task.as_mut() else {
            return Ok(());
        };
        match tokio::time::timeout(shutdown_budget, task).await {
            Ok(result) => {
                handle.task.take();
                result.map_err(|e| {
                    RuntimeError::Unsupported(format!("server task join failed: {e}"))
                })?;
            }
            Err(_) => {
                if let Some(task) = handle.task.take() {
                    task.abort();
                    let _ = task.await;
                }
                return Err(RuntimeError::ShutdownDeadlineExceeded(shutdown_budget));
            }
        }
        Ok(())
    }
}

async fn admission_middleware(
    State(state): State<AdmissionState>,
    request: Request,
    next: Next,
) -> Response {
    let control_kind = state.control_routes.iter().find_map(|route| {
        (request.method().as_str() == route.method && request.uri().path() == route.path)
            .then_some(route.kind)
    });
    let admission_kind = if control_kind.is_some() {
        AdmissionKind::Control
    } else {
        AdmissionKind::Application
    };
    let permit = match state.admission.try_enter(admission_kind) {
        Ok(permit) => permit,
        Err(_) => {
            let mut response = StatusCode::SERVICE_UNAVAILABLE.into_response();
            response
                .headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
            return response;
        }
    };
    let mut response = next.run(request).await;
    if matches!(control_kind, Some(ControlRouteKind::Readiness)) && !state.admission.is_open() {
        *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("close"));
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
    }
    let (parts, body) = response.into_parts();
    Response::from_parts(parts, Body::new(AdmissionBody { body, permit }))
}

struct AdmissionBody {
    body: Body,
    permit: light_runtime::AdmissionPermit,
}

impl http_body::Body for AdmissionBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

impl Drop for AdmissionBody {
    fn drop(&mut self) {
        let _ = &self.permit;
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

fn bind_addr(ip: &str, port: u16) -> Result<SocketAddr, RuntimeError> {
    let parsed_ip = ip
        .parse::<IpAddr>()
        .map_err(|e| RuntimeError::Unsupported(format!("invalid server.ip `{ip}`: {e}")))?;
    Ok(SocketAddr::new(parsed_ip, port))
}

#[cfg(test)]
mod tests {
    use super::{
        AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext, bind_addr,
        resolve_advertised_address,
    };
    use async_trait::async_trait;
    use axum::{
        Router,
        body::{Body, Bytes},
        routing::get,
    };
    use futures_util::stream;
    use light_runtime::{
        AdmissionGate, AdmissionKind, BootstrapConfig, DirectRegistryConfig, LifecycleParticipant,
        LifecycleRegistry, LightRuntimeBuilder, ModuleRegistry, RuntimeConfig, RuntimeError,
        ServerConfig, ServiceIdentity, ShutdownContext, ShutdownMode, ShutdownReason,
        TransportRuntime,
    };
    use std::convert::Infallible;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    struct StreamingTestApp {
        finish: Arc<tokio::sync::Notify>,
    }

    struct StartupParticipant(Arc<AtomicUsize>);

    #[async_trait]
    impl LifecycleParticipant for StartupParticipant {
        fn name(&self) -> &'static str {
            "axum-startup-participant"
        }

        async fn shutdown(
            &self,
            _config: &RuntimeConfig,
            _context: &ShutdownContext,
        ) -> Result<(), RuntimeError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct RegisteringTestApp(Arc<AtomicUsize>);

    #[async_trait]
    impl AxumApp for RegisteringTestApp {
        async fn router(&self, context: ServerContext) -> Result<Router, RuntimeError> {
            context
                .lifecycle
                .register(Arc::new(StartupParticipant(Arc::clone(&self.0))))?;
            Ok(Router::new())
        }
    }

    #[async_trait]
    impl AxumApp for StreamingTestApp {
        async fn router(&self, _context: ServerContext) -> Result<Router, RuntimeError> {
            let finish = Arc::clone(&self.finish);
            Ok(Router::new()
                .route("/health", get(|| async { "healthy" }))
                .route("/ready", get(|| async { "ready" }))
                .route("/metrics", get(|| async { "metric 1" }))
                .route("/ok", get(|| async { "ok" }))
                .route(
                    "/stream",
                    get(move || {
                        let finish = Arc::clone(&finish);
                        async move {
                            Body::from_stream(stream::unfold(0_u8, move |state| {
                                let finish = Arc::clone(&finish);
                                async move {
                                    match state {
                                        0 => {
                                            Some((Ok::<_, Infallible>(Bytes::from_static(b"x")), 1))
                                        }
                                        _ => {
                                            finish.notified().await;
                                            None
                                        }
                                    }
                                }
                            }))
                        }
                    }),
                ))
        }

        fn control_routes(&self) -> &'static [ControlRoute] {
            &[
                ControlRoute {
                    method: "GET",
                    path: "/health",
                    kind: ControlRouteKind::Liveness,
                },
                ControlRoute {
                    method: "GET",
                    path: "/ready",
                    kind: ControlRouteKind::Readiness,
                },
                ControlRoute {
                    method: "GET",
                    path: "/metrics",
                    kind: ControlRouteKind::Metrics,
                },
            ]
        }
    }

    fn runtime_config() -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config"),
            resolved_values: Default::default(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn uses_explicit_advertised_address_when_present() {
        let mut config = runtime_config();
        config.server.advertised_address = Some("172.18.0.10".to_string());

        let address = resolve_advertised_address(&config, IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .expect("resolve advertised address");

        assert_eq!(address, "172.18.0.10");
    }

    #[test]
    fn falls_back_to_unspecified_bound_ip_without_failing() {
        let config = runtime_config();

        let address = resolve_advertised_address(&config, IpAddr::V4(Ipv4Addr::UNSPECIFIED))
            .expect("resolve advertised address");

        assert_eq!(address, "0.0.0.0");
    }

    #[test]
    fn builds_ipv4_bind_address() {
        let addr = bind_addr("0.0.0.0", 8080).expect("ipv4 bind address");

        assert_eq!(
            addr,
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8080)
        );
    }

    #[test]
    fn builds_ipv6_bind_address() {
        let addr = bind_addr("::", 8080).expect("ipv6 bind address");

        assert_eq!(
            addr,
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 8080)
        );
        assert_eq!(addr.to_string(), "[::]:8080");
    }

    #[test]
    fn rejects_invalid_bind_ip() {
        let error = bind_addr("not an ip", 8080).expect_err("invalid bind ip should fail");

        assert!(matches!(error, RuntimeError::Unsupported(_)));
    }

    #[test]
    fn rejects_empty_explicit_advertised_address() {
        let mut config = runtime_config();
        config.server.advertised_address = Some("   ".to_string());

        let error = resolve_advertised_address(&config, IpAddr::V4(Ipv4Addr::LOCALHOST))
            .expect_err("empty advertised address should fail");

        assert!(matches!(error, RuntimeError::Unsupported(_)));
    }

    #[tokio::test]
    async fn admission_rejects_new_requests_and_holds_stream_permit_until_body_finishes() {
        let finish = Arc::new(tokio::sync::Notify::new());
        let transport = AxumTransport::new(StreamingTestApp {
            finish: Arc::clone(&finish),
        });
        let mut config = runtime_config();
        config.server.ip = "127.0.0.1".to_string();
        config.server.enable_http = true;
        config.server.enable_https = false;
        config.server.dynamic_port = true;
        let lifecycle = LifecycleRegistry::default();
        let admission = AdmissionGate::default();
        let mut bound = transport
            .bind(
                &config,
                &lifecycle.registrar(),
                &admission,
                CancellationToken::new(),
            )
            .await
            .expect("bind test server");
        let base_url = format!("http://127.0.0.1:{}", bound.metadata.port);
        let client = reqwest::Client::new();

        let rejected = client
            .get(format!("{base_url}/ok"))
            .send()
            .await
            .expect("closed request");
        assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(rejected.headers()["connection"], "close");
        assert_eq!(rejected.headers()["retry-after"], "0");

        admission.open();
        let stream_response = client
            .get(format!("{base_url}/stream"))
            .send()
            .await
            .expect("stream request");
        assert_eq!(admission.active(AdmissionKind::Application), 1);

        admission.close();
        let rejected = client
            .get(format!("{base_url}/ok"))
            .send()
            .await
            .expect("quiescing request");
        assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(admission.active(AdmissionKind::Application), 1);

        let health = client
            .get(format!("{base_url}/health"))
            .send()
            .await
            .expect("health");
        assert_eq!(health.status(), reqwest::StatusCode::OK);
        let ready = client
            .get(format!("{base_url}/ready"))
            .send()
            .await
            .expect("ready");
        assert_eq!(ready.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
        let metrics = client
            .get(format!("{base_url}/metrics"))
            .send()
            .await
            .expect("metrics");
        assert_eq!(metrics.status(), reqwest::StatusCode::OK);

        finish.notify_one();
        assert_eq!(stream_response.bytes().await.expect("finish stream"), "x");
        tokio::task::yield_now().await;
        assert_eq!(admission.active(AdmissionKind::Application), 0);

        let context = ShutdownContext {
            reason: ShutdownReason::Programmatic,
            mode: ShutdownMode::Graceful,
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        };
        transport
            .stop(&mut bound.handle, &context)
            .await
            .expect("stop test server");
    }

    #[tokio::test]
    async fn listener_bind_failure_unwinds_participants_started_by_the_app() {
        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("occupy test port");
        let mut config = runtime_config();
        config.server.ip = "127.0.0.1".to_string();
        config.server.enable_http = true;
        config.server.enable_https = false;
        config.server.dynamic_port = false;
        config.server.http_port = occupied.local_addr().unwrap().port();
        let shutdowns = Arc::new(AtomicUsize::new(0));

        let result = LightRuntimeBuilder::new(AxumTransport::new(RegisteringTestApp(Arc::clone(
            &shutdowns,
        ))))
        .with_prepared_config(config)
        .build()
        .start()
        .await;

        assert!(matches!(result, Err(RuntimeError::Io(_))));
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    }
}
