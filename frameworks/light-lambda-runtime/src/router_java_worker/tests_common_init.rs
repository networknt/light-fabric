//! Preparatory composition only; no app registration or admitted limits.
use super::*;
use crate::init::*;
use crate::router_policy::{PreparedRouterResources, RouterPolicyProvider, RouterSettings};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{plan::*, *};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct JavaTerminal {
    worker: Arc<Session>,
    backend: Arc<dyn Backend>,
    http: Arc<crate::http_router::HttpRouterBackend>,
}
#[async_trait::async_trait]
impl Backend for JavaTerminal {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        if matches!(target, Target::Function(_)) {
            return self.backend.dispatch(inv, state, target).await;
        }
        self.http
            .dispatch_with_java(inv, target, &self.worker)
            .await
            .map_err(|error| match error {
                crate::http_router::JavaDispatchFailure::Transport(error) => error,
                crate::http_router::JavaDispatchFailure::Rewrite(error) => DispatchError(
                    error
                        .approved_status()
                        .expect("qualification must not invent outcomes for unadmitted categories"),
                ),
            })
    }
}
struct PreparedProvider {
    factory: Arc<dyn HandlerFactory>,
    resources: Arc<RouterSettings>,
}
impl HandlerFactoryProvider for PreparedProvider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, crate::init_error::InitError> {
        Ok(self.factory.clone())
    }
    fn prepare_router_resources(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedRouterResources, crate::init_error::InitError> {
        Ok(PreparedRouterResources {
            factory: self.factory.clone(),
            resources: Some(self.resources.clone()),
        })
    }
}
struct DenyProvider;
impl HandlerFactoryProvider for DenyProvider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, crate::init_error::InitError> {
        Ok(Arc::new(BindingFactory(Binding::Request(
            Arc::new(Deny),
            RequestCapabilities::default(),
        ))))
    }
}
struct FinalPath;
#[async_trait::async_trait]
impl RequestHandler for FinalPath {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        inv.path = "/x-final".into();
        Ok(HandlerOutcome::Continue)
    }
}
struct FinalPathProvider;
impl HandlerFactoryProvider for FinalPathProvider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, crate::init_error::InitError> {
        Ok(Arc::new(BindingFactory(Binding::Request(
            Arc::new(FinalPath),
            RequestCapabilities::default(),
        ))))
    }
}
struct OwnedHttp {
    url: String,
    requests: Arc<std::sync::Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for OwnedHttp {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl OwnedHttp {
    async fn start(reply: String, delay: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let saved = requests.clone();
        let task = tokio::spawn(async move {
            let mut children = tokio::task::JoinSet::new();
            while let Ok((mut socket, _)) = listener.accept().await {
                let saved = saved.clone();
                let reply = reply.clone();
                children.spawn(async move {
                    let mut request = Vec::new();
                    loop {
                        let mut bytes = [0; 1024];
                        let n = socket.read(&mut bytes).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&bytes[..n]);
                        if request.windows(4).any(|b| b == b"\r\n\r\n") {
                            break;
                        }
                    }
                    saved
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(request).unwrap());
                    tokio::time::sleep(delay).await;
                    let _ = socket.write_all(reply.as_bytes()).await;
                });
                while children.try_join_next().is_some() {}
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }
    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "owned Java + common Init + HTTP; requires WP12_ROUTER_ARTIFACTS"]
async fn common_init_repeated_router_trusted_http_and_late_rejection() {
    static EMBEDDED: &[config_loader::EmbeddedConfigFile] = &[
        config_loader::EmbeddedConfigFile {
            name: "server.yml",
            content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
        },
        config_loader::EmbeddedConfigFile {
            name: "light-lambda.yml",
            content: "stage: test-only\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 50\n",
        },
        config_loader::EmbeddedConfigFile {
            name: "lambda-proxy.yml",
            content: "functions:\n  /*@get: TestFn\nregion: us-east-1\n",
        },
    ];
    let sink = OwnedHttp::start(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        Duration::ZERO,
    )
    .await;
    for (deny, redirect, slow) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let reply = if redirect {
            format!(
                "HTTP/1.1 302 Found\r\nLocation: {}/untrusted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                sink.url
            )
        } else {
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
        };
        let server = OwnedHttp::start(
            reply,
            if slow {
                Duration::from_millis(300)
            } else {
                Duration::ZERO
            },
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, text: String| std::fs::write(dir.path().join(name), text).unwrap();
        write("router.yml","enabled: true\nhttpsEnabled: false\nmaxRequestTime: 100\nurlRewriteRules: ['/(x.*) /r-$1']\n".into());
        write("correlation.yml", "enabled: true\n".into());
        write(
            "direct-registry.yml",
            serde_yaml::to_string(&json!({"directUrls":{"service":format!("{}/base",server.url)}}))
                .unwrap(),
        );
        let class = |id| {
            compat::COMPATIBILITY
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .class
        };
        let chain = vec!["router", "router", "deny", "proxy"];
        write("handler.yml",serde_yaml::to_string(&json!({"enabled":true,"handlers":[format!("{}@router",class(HandlerId::Router)),format!("{}@deny",class(HandlerId::Correlation)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":chain})).unwrap());
        let counts = Arc::new(Counts {
            dispatch: AtomicUsize::new(0),
            complete: AtomicUsize::new(0),
            paths: std::sync::Mutex::new(Vec::new()),
        });
        let mut input = InitInput {
            embedded: EMBEDDED,
            local_dir: dir.path().join("absent"),
            default_dir: None,
            external_dir: dir.path().into(),
            environment: Environment {
                region: Some("us-east-1".into()),
                function_name: "test-only".into(),
                function_version: "1".into(),
                memory_mb: 128,
                allow_insecure_test_endpoints: false,
            },
            gateway_identity: serverless_aws::target::GatewayIdentity {
                region: "us-east-1".into(),
                function_name: "test-only".into(),
                account_id: "000000000000".into(),
            },
            handlers: Vec::new(),
            finalizers: vec![counts.clone()],
            backend: BackendConfiguration::Injected(counts.clone()),
            panic_during_preparation: false,
        };
        let sources = crate::router_policy::CapturedRouterSources::capture(
            input.embedded,
            input.default_dir.as_deref(),
            &input.local_dir,
            &input.external_dir,
            crate::router_policy::CaptureLimits {
                per_source: 1 << 20,
                aggregate: 4 << 20,
            },
        )
        .unwrap();
        let captured_identity = sources.identity().to_owned();
        // Mutate between early capture and ordinary runtime preparation. The
        // Router worker/catalogue must use its captured sources throughout.
        write("router.yml","enabled: true\nhttpsEnabled: false\nmaxRequestTime: 100\nurlRewriteRules: ['/x /stage-mutated']\n".into());
        write(
            "direct-registry.yml",
            serde_yaml::to_string(&json!({"directUrls":{"service":sink.url}})).unwrap(),
        );
        let config = prepare_local(&input).await.unwrap();
        assert_eq!(sources.identity(), captured_identity);
        let capacity = Arc::new(Semaphore::new(1));
        let manifest = artifacts();
        let prepared = RouterPolicyProvider
            .prepare_captured_with_java(
                &sources,
                Preparation {
                    artifacts: &manifest,
                    limits: limits(),
                    capacity: capacity.clone(),
                    epoch: 19,
                },
                Instant::now() + Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let worker = Arc::new(prepared.worker);
        let stage = Arc::new(serverless_core::handlers::router::RouterHandler::new([
            "service".into(),
        ]));
        let http = crate::router_policy::prepare_backend_from_resources(
            &config,
            &serverless_aws::limits::Limits::default(),
            &prepared.resources,
        )
        .unwrap();
        input.handlers = vec![
            HandlerRegistration {
                id: HandlerId::Router,
                config_file: "router.yml",
                factory: Some(Arc::new(PreparedProvider {
                    factory: Arc::new(BindingFactory(Binding::Request(
                        stage,
                        RequestCapabilities::default(),
                    ))),
                    resources: prepared.resources,
                })),
            },
            HandlerRegistration {
                id: HandlerId::Correlation,
                config_file: "correlation.yml",
                factory: Some(if deny {
                    Arc::new(DenyProvider)
                } else {
                    Arc::new(FinalPathProvider)
                }),
            },
        ];
        let snapshot = initialize(input).await.unwrap();
        let terminal = JavaTerminal {
            worker: worker.clone(),
            backend: snapshot.backend.clone(),
            http,
        };
        write(
            "direct-registry.yml",
            serde_yaml::to_string(&json!({"directUrls":{"service":sink.url}})).unwrap(),
        );
        write(
            "router.yml",
            "enabled: true\nurlRewriteRules: ['/x /mutated']\n".into(),
        );
        for _ in 0..2 {
            let mut inv = invocation("/x");
            inv.headers
                .insert("service_id", http::HeaderValue::from_static("service"));
            let pipeline = Pipeline::new(snapshot.plan.clone(), snapshot.finalizers.clone());
            let response = pipeline.execute(inv, &terminal).await;
            let expected = if deny {
                401
            } else if redirect {
                302
            } else if slow {
                400
            } else {
                200
            };
            assert_eq!(
                response.status.as_u16(),
                expected,
                "deny={deny}, redirect={redirect}, slow={slow}"
            );
        }
        assert_eq!(server.count(), if deny { 0 } else { 2 });
        assert_eq!(
            worker.serial.load(Ordering::SeqCst),
            if deny { 1 } else { 3 },
            "one terminal rewrite per accepted invocation; none after rejection"
        );
        assert_eq!(sink.count(), 0);
        assert_eq!(counts.dispatch.load(Ordering::SeqCst), 0);
        assert_eq!(counts.complete.load(Ordering::SeqCst), 2);
        for request in &*server.requests.lock().unwrap() {
            assert!(
                request.starts_with("GET /base/r-x-final HTTP/1.1"),
                "{request}"
            );
            assert!(!request.to_ascii_lowercase().contains("service_id"));
        }
        if !deny {
            let pipeline = Pipeline::new(snapshot.plan.clone(), snapshot.finalizers.clone());
            assert_eq!(
                pipeline.execute(invocation("/x"), &terminal).await.status,
                200
            );
            assert_eq!(counts.dispatch.load(Ordering::SeqCst), 1);
            assert_eq!(server.count(), 2);
        }
        drop(terminal);
        drop(snapshot);
        Arc::try_unwrap(worker)
            .ok()
            .expect("test bridge released")
            .shutdown()
            .await;
        assert_eq!(capacity.available_permits(), 1);
    }
}
