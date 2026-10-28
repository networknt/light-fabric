use async_trait::async_trait;
use light_client::{ClientFactory, config::ClientConfig};
use light_lambda_runtime::http_router::{ComposedBackend, HttpRouterBackend};
use serde_json::json;
use serverless_aws::{
    alb::{self, DecodeContext},
    limits::Limits,
};
use serverless_core::*;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new(response: String, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let r = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let r = r.clone();
                let response = response.clone();
                tokio::spawn(async move {
                    let mut input = Vec::new();
                    let mut b = [0u8; 1024];
                    loop {
                        let n = socket.read(&mut b).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        input.extend_from_slice(&b[..n]);
                        if let Some(end) = input.windows(4).position(|x| x == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&input[..end]);
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|s| s.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if input.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    r.lock().unwrap().push(input);
                    tokio::time::sleep(delay).await;
                    let _ = socket.write_all(response.as_bytes()).await;
                });
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
struct TlsProcess(std::process::Child);
impl Drop for TlsProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[tokio::test]
async fn owned_https_uses_configured_ca_rejects_untrusted_ca_and_never_follows_redirect() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let dir = tempfile::tempdir().unwrap();
    let command = |args: &[&str]| {
        let result = Command::new("openssl")
            .args(args)
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "owned certificate generation failed"
        );
    };
    command(&[
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "ca-key.pem",
        "-out",
        "ca.pem",
        "-days",
        "1",
        "-subj",
        "/CN=WP12-owned-CA",
        "-addext",
        "basicConstraints=critical,CA:TRUE",
        "-addext",
        "keyUsage=critical,keyCertSign,cRLSign",
    ]);
    command(&[
        "req",
        "-new",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-keyout",
        "server-key.pem",
        "-out",
        "server.csr",
        "-subj",
        "/CN=localhost",
    ]);
    std::fs::write(dir.path().join("extensions.cnf"),"subjectAltName=DNS:localhost\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n").unwrap();
    command(&[
        "x509",
        "-req",
        "-in",
        "server.csr",
        "-CA",
        "ca.pem",
        "-CAkey",
        "ca-key.pem",
        "-CAcreateserial",
        "-out",
        "server.pem",
        "-days",
        "1",
        "-extfile",
        "extensions.cnf",
    ]);
    let redirect = Server::new(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        Duration::ZERO,
    )
    .await;
    let script = r#"import http.server,ssl,sys
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  if self.path=='/redirect':
   self.send_response(302);self.send_header('Location',sys.argv[1]+'/untrusted');self.send_header('Content-Length','0');self.end_headers()
  else:
   self.send_response(200);self.send_header('Content-Length','6');self.end_headers();self.wfile.write(b'tls-ok')
server=http.server.HTTPServer(('127.0.0.1',0),Handler)
ctx=ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER);ctx.load_cert_chain('server.pem','server-key.pem')
server.socket=ctx.wrap_socket(server.socket,server_side=True)
print(server.server_port,flush=True)
server.serve_forever()
"#;
    let child = Command::new("python3")
        .args(["-u", "-c", script, &redirect.url])
        .current_dir(dir.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut process = TlsProcess(child);
    let mut ready = String::new();
    BufReader::new(process.0.stdout.take().unwrap())
        .read_line(&mut ready)
        .unwrap();
    let port: u16 = ready.trim().parse().expect("owned TLS listener ready");
    let destinations = BTreeMap::from([("service".into(), format!("https://localhost:{port}"))]);
    let mut config = ClientConfig::default();
    config.tls.ca_cert_path = Some(dir.path().join("ca.pem"));
    let trusted = HttpRouterBackend::new(
        destinations.clone(),
        &ClientFactory::from_config(&config),
        Duration::from_secs(1),
        &Limits::default(),
    )
    .unwrap();
    let target = Target::HttpService("service".into());
    let state = ExchangeState::default();
    let result = trusted
        .dispatch(&invocation("GET"), &state, &target)
        .await
        .unwrap();
    assert_eq!(result.status, 200);
    assert_eq!(result.body.unwrap(), "tls-ok");
    let untrusted = HttpRouterBackend::new(
        destinations,
        &ClientFactory::from_config(&ClientConfig::default()),
        Duration::from_secs(1),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        untrusted
            .dispatch(&invocation("GET"), &state, &target)
            .await
            .unwrap_err()
            .0
            .code,
        "ERR10089"
    );
    let mut inv = invocation("GET");
    inv.path = "/redirect".into();
    let result = trusted.dispatch(&inv, &state, &target).await.unwrap();
    assert_eq!(result.status, 302);
    assert_eq!(redirect.count(), 0);
    let wrong_host = HttpRouterBackend::new(
        BTreeMap::from([("service".into(), format!("https://127.0.0.1:{port}"))]),
        &ClientFactory::from_config(&config),
        Duration::from_secs(1),
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(
        wrong_host
            .dispatch(&invocation("GET"), &state, &target)
            .await
            .unwrap_err()
            .0
            .code,
        "ERR10089"
    );
}
fn invocation(method: &str) -> Invocation {
    alb::decode(json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":method,"path":"/x","multiValueHeaders":{"service_id":["service"],"x-forward":["yes"]},"multiValueQueryStringParameters":{"omitted":["as-java"]},"body":"synthetic-body"}),DecodeContext{request_id:"test-only".into(),deadline:Instant::now()+Duration::from_secs(2),response_deadline:Instant::now()+Duration::from_secs(3),cancel:CancellationToken::new()},&Limits::default()).unwrap()
}
fn backend(server: &Server) -> HttpRouterBackend {
    HttpRouterBackend::new(
        BTreeMap::from([("service".into(), server.url.clone())]),
        &ClientFactory::from_config(&ClientConfig::default()),
        Duration::from_secs(1),
        &Limits::default(),
    )
    .unwrap()
}
#[tokio::test]
async fn zero_timeout_and_lowercase_method_preserve_java_request_error_without_sending() {
    let server = Server::new(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
        Duration::ZERO,
    )
    .await;
    let zero = HttpRouterBackend::new(
        BTreeMap::from([("service".into(), server.url.clone())]),
        &ClientFactory::from_config(&ClientConfig::default()),
        Duration::ZERO,
        &Limits::default(),
    )
    .unwrap();
    for (backend, method) in [(&zero, "GET"), (&backend(&server), "get")] {
        let error = backend
            .dispatch(
                &invocation(method),
                &ExchangeState::default(),
                &Target::HttpService("service".into()),
            )
            .await
            .unwrap_err()
            .0;
        assert_eq!(error.status_code, 400);
        assert_eq!(error.code, "ERR10089");
        assert_eq!(
            error.description,
            format!(
                "Failed to invoke HTTP service with serviceId {}/x.",
                server.url
            )
        );
    }
    assert_eq!(server.count(), 0);
}
struct Lambda(Arc<AtomicUsize>);
struct InitLambda(Arc<AtomicUsize>);
#[async_trait]
impl Backend for InitLambda {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        assert!(matches!(target, Target::Function(_)));
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut headers = http::HeaderMap::new();
        headers.insert("x-backend", "synthetic".parse().unwrap());
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers,
            body: Some(bytes::Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
#[async_trait]
impl Backend for Lambda {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        assert!(matches!(target, Target::Function(_)));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(StatusError::missing_target().response())
    }
}
#[tokio::test]
async fn init_composes_real_http_after_selection_with_immutable_config_and_warm_lambda_fallback() {
    use light_lambda_runtime::{init::*, router_policy::RouterPolicyProvider, service::handle};
    static EMBEDDED: &[config_loader::EmbeddedConfigFile] = &[
        config_loader::EmbeddedConfigFile {
            name: "server.yml",
            content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
        },
        config_loader::EmbeddedConfigFile {
            name: "light-lambda.yml",
            content: "stage: test-only\ningressTimeoutMs: 2000\nresponseReserveMs: 200\npostReserveMs: 50\n",
        },
        config_loader::EmbeddedConfigFile {
            name: "lambda-proxy.yml",
            content: "functions:\n  /x@get: TestFn\nregion: us-east-1\n",
        },
    ];
    let server = Server::new(
        "HTTP/1.1 202 Accepted\r\nContent-Length: 7\r\nConnection: close\r\n\r\nhttp-ok".into(),
        Duration::ZERO,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("router.yml"),
        "enabled: true\nhttpsEnabled: false\nmaxRequestTime: 1000\nurlRewriteRules: ['/x /rewritten']\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        json!({"directUrls":{"file-only":"http://127.0.0.1:1"}}).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("service.yml"),json!({"singletons":[
        {"com.networknt.registry.URL":[{"com.networknt.registry.URLImpl":{"protocol":"http","host":"localhost","port":0,"path":"direct","parameters":{"service":format!("{}/first///?fixed=1,{}/second///?fixed=2",server.url,server.url),"tagged":format!("{}/tagged?environment=dev",server.url)}}}]},
        {"com.networknt.registry.Registry":["com.networknt.registry.support.DirectRegistry"]},
        {"com.networknt.balance.LoadBalance":["com.networknt.balance.RoundRobinLoadBalance"]},
        {"com.networknt.cluster.Cluster":["com.networknt.cluster.LightCluster"]}
    ]}).to_string()).unwrap();
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@router",class(HandlerId::Router)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["router","proxy"]}).to_string()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let input = InitInput {
        embedded: EMBEDDED,
        local_dir: dir.path().join("absent"),
        default_dir: None,
        external_dir: dir.path().into(),
        environment: Environment {
            region: Some("us-east-1".into()),
            function_name: "test-only-gateway".into(),
            function_version: "1".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: true,
        },
        gateway_identity: serverless_aws::target::GatewayIdentity {
            function_name: "test-only-gateway".into(),
            region: "us-east-1".into(),
            account_id: "000000000000".into(),
        },
        handlers: vec![HandlerRegistration {
            id: HandlerId::Router,
            config_file: "router.yml",
            factory: Some(Arc::new(RouterPolicyProvider)),
        }],
        finalizers: vec![],
        backend: BackendConfiguration::Injected(Arc::new(InitLambda(calls.clone()))),
    };
    let report = validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert_eq!(server.count(), 0);
    // The properties form passed offline qualification. Exercise the same
    // immutable catalogue through Java's explicit five-argument URLImpl form.
    let mut service: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("service.yml")).unwrap()).unwrap();
    let parameters =
        service["singletons"][0]["com.networknt.registry.URL"][0]["com.networknt.registry.URLImpl"]
            ["parameters"]
            .clone();
    service["singletons"][0]["com.networknt.registry.URL"][0]["com.networknt.registry.URLImpl"] = json!([
        {"java.lang.String":"http"},{"java.lang.String":"localhost"},{"int":0},{"java.lang.String":"direct"},{"java.util.Map":parameters}
    ]);
    std::fs::write(dir.path().join("service.yml"), service.to_string()).unwrap();
    let snapshot = initialize(input).await.unwrap();
    std::fs::write(dir.path().join("service.yml"), "singletons: []").unwrap();
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        json!({"directUrls":{"service":"http://127.0.0.1:1"}}).to_string(),
    )
    .unwrap();
    for (service, method, status) in [
        (Some("service"), "GET", 202),
        (Some("service"), "POST", 202),
        (Some("service"), "DELETE", 202),
        (Some("service"), "PUT", 202),
        (Some("service"), "PATCH", 202),
        (None, "GET", 200),
        (Some("unknown-other"), "GET", 400),
        (Some("file-only"), "GET", 400),
        (Some("tagged|dev"), "GET", 202),
        (Some("tagged"), "GET", 400),
    ] {
        let mut context = lambda_runtime::Context::default();
        context.request_id = "router-init".into();
        context.deadline = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
            + 5000;
        let headers = service.map_or_else(|| json!({}), |service| json!({"service_id":[service]}));
        let event = lambda_runtime::LambdaEvent::new(
            json!({"httpMethod":method,"path":"/x","requestContext":{"elb":{"targetGroupArn":"test-only"}},"multiValueHeaders":headers,"multiValueQueryStringParameters":{"omitted":["as-java"]},"body":"synthetic-body"}),
            context,
        );
        let response = handle(snapshot.clone(), event).await.unwrap();
        assert_eq!(response["statusCode"], status);
    }
    assert_eq!(server.count(), 6);
    for (i, raw) in server.requests.lock().unwrap().iter().enumerate() {
        let raw = String::from_utf8_lossy(raw);
        assert!(raw.starts_with(&format!(
            "{} /{}/rewritten HTTP/1.1",
            ["GET", "POST", "DELETE", "PUT", "PATCH", "GET"][i],
            if i == 5 {
                "tagged"
            } else if i % 2 == 0 {
                "first"
            } else {
                "second"
            }
        )));
        assert!(!raw.to_ascii_lowercase().contains("service_id"));
        assert!(!raw.contains("omitted"));
        assert_eq!(
            raw.split("\r\n\r\n").nth(1).unwrap(),
            if [0, 2, 5].contains(&i) {
                ""
            } else {
                "synthetic-body"
            }
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn trusted_http_component_forwards_supported_methods_strips_service_id_and_keeps_java_first_response_header()
 {
    let server=Server::new("HTTP/1.1 202 Accepted\r\nContent-Length: 7\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nConnection: close\r\n\r\nhttp-ok".into(),Duration::ZERO).await;
    let backend = backend(&server);
    for method in ["GET", "DELETE", "POST", "PUT", "PATCH"] {
        let inv = invocation(method);
        let response = backend
            .dispatch(
                &inv,
                &ExchangeState::default(),
                &Target::HttpService("service".into()),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 202);
        assert_eq!(response.body.as_deref(), Some(b"http-ok".as_slice()));
        assert_eq!(response.headers.get_all("set-cookie").iter().count(), 1);
        assert_eq!(response.headers["set-cookie"], "a=1");
        let requests = server.requests.lock().unwrap();
        let raw = String::from_utf8_lossy(requests.last().unwrap());
        assert!(raw.starts_with(&format!("{method} /x HTTP/1.1")));
        assert!(!raw.to_ascii_lowercase().contains("service_id"));
        assert!(raw.contains("x-forward: yes"));
        assert_eq!(
            raw.split("\r\n\r\n").nth(1).unwrap(),
            if matches!(method, "GET" | "DELETE") {
                ""
            } else {
                "synthetic-body"
            }
        );
    }
    assert_eq!(server.count(), 5);
    for target in [
        Target::HttpService("http://127.0.0.1:1".into()),
        Target::HttpService("unknown".into()),
    ] {
        assert_eq!(
            backend
                .dispatch(&invocation("GET"), &ExchangeState::default(), &target)
                .await
                .unwrap_err()
                .0
                .code,
            "ERR10089"
        );
    }
    assert!(
        backend
            .dispatch(
                &invocation("HEAD"),
                &ExchangeState::default(),
                &Target::HttpService("service".into())
            )
            .await
            .is_err()
    );
    assert_eq!(server.count(), 5);
}
#[tokio::test]
async fn redirect_deadline_and_composition_never_dispatch_to_an_untrusted_or_second_terminal() {
    let other = Server::new(
        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".into(),
        Duration::ZERO,
    )
    .await;
    let redirect=Server::new(format!("HTTP/1.1 302 Found\r\nContent-Length: 0\r\nLocation: {}/secret\r\nConnection: close\r\n\r\n",other.url),Duration::ZERO).await;
    let http = Arc::new(backend(&redirect));
    let calls = Arc::new(AtomicUsize::new(0));
    let composed = ComposedBackend {
        lambda: Arc::new(Lambda(calls.clone())),
        http,
    };
    assert_eq!(
        composed
            .dispatch(
                &invocation("GET"),
                &ExchangeState::default(),
                &Target::HttpService("service".into())
            )
            .await
            .unwrap()
            .status,
        302
    );
    assert_eq!(other.count(), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    composed
        .dispatch(
            &invocation("GET"),
            &ExchangeState::default(),
            &Target::Function("lambda-alias".into()),
        )
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(redirect.count(), 1);
    let slow = Server::new(
        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".into(),
        Duration::from_millis(100),
    )
    .await;
    // Construct the client before starting the measured request budget.
    let slow_backend = backend(&slow);
    let mut inv = invocation("GET");
    inv.deadline = Instant::now() + Duration::from_millis(20);
    let response = timeout(
        Duration::from_secs(1),
        slow_backend.dispatch(
            &inv,
            &ExchangeState::default(),
            &Target::HttpService("service".into()),
        ),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(response.0.code, "ERR14017");
    assert_eq!(slow.count(), 1);
    inv.cancel.cancel();
    assert!(
        slow_backend
            .dispatch(
                &inv,
                &ExchangeState::default(),
                &Target::HttpService("service".into())
            )
            .await
            .is_err()
    );
    assert_eq!(slow.count(), 1);
}
