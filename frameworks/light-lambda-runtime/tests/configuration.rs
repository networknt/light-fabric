use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use light_lambda_runtime::{init::*, init_error::InitError, service::handle};
use serde_json::{Value, json};
use serverless_aws::target::GatewayIdentity;
use serverless_core::{
    compat::{COMPATIBILITY, Compatibility},
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;

static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: ${light-lambda.stage:test}\ningressTimeoutMs: 300\nresponseReserveMs: 100\npostReserveMs: 20\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /x@get: test-only-business\nregion: us-east-1\n",
    },
];
#[derive(Default)]
struct Fake {
    calls: AtomicUsize,
    remaining: Mutex<Vec<Duration>>,
    delay: Duration,
}
#[async_trait]
impl Backend for Fake {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.remaining.lock().unwrap().push(
            inv.deadline
                .saturating_duration_since(tokio::time::Instant::now()),
        );
        tokio::time::sleep(self.delay).await;
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: None,
            body_is_binary: false,
        })
    }
}
fn setup() -> (TempDir, InitInput, Arc<Fake>) {
    let dir = TempDir::new().unwrap();
    let fake = Arc::new(Fake::default());
    let class = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Proxy)
        .unwrap()
        .class;
    std::fs::write(dir.path().join("handler.yml"),serde_yaml::to_string(&json!({"enabled":true,"handlers":[format!("{class}@proxy")],"defaultHandlers":["proxy"]})).unwrap()).unwrap();
    let input = InitInput {
        embedded: EMBEDDED,
        local_dir: dir.path().join("missing-optional-local"),
        default_dir: None,
        external_dir: dir.path().into(),
        environment: Environment {
            region: Some("us-east-1".into()),
            function_name: "test-only-gateway".into(),
            function_version: "1".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: false,
        },
        gateway_identity: GatewayIdentity {
            function_name: "test-only-gateway".into(),
            region: "us-east-1".into(),
            account_id: "123456789012".into(),
        },
        handlers: vec![],
        finalizers: vec![],
        backend: BackendConfiguration::Injected(fake.clone()),
    };
    (dir, input, fake)
}
fn event() -> lambda_runtime::LambdaEvent<Value> {
    let mut c = lambda_runtime::Context::default();
    c.request_id = "test-request".into();
    c.deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    lambda_runtime::LambdaEvent::new(
        json!({"httpMethod":"GET","path":"/x","requestContext":{"elb":{"targetGroupArn":"test-only"}},"multiValueHeaders":{}}),
        c,
    )
}
#[tokio::test]
async fn status_flags_admitted_without_a_selected_policy_and_without_dispatch() {
    let (dir, input, fake) = setup();
    for value in [
        json!({}),
        json!({"showMetadata":true,"showMessage":true,"showDescription":true}),
        // Catalogue entries are a separate contract; do not mistake them for
        // handler settings or change the fixed StatusError serializer.
        json!({"ERR10007":{"statusCode":404,"code":"ERR10007","message":"INVALID_REQUEST_PATH","description":"Request path %s cannot be found in the spec."}}),
    ] {
        std::fs::write(dir.path().join("status.yml"), value.to_string()).unwrap();
        let report = validate_configuration(&input).await;
        assert!(report.valid, "{:?}", report.errors);
        for key in ["showMessage", "showDescription", "showMetadata"] {
            assert!(
                report
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(&format!("status.{key}:")))
            );
        }
    }
    for (key, nondefault) in [
        ("showMessage", false),
        ("showDescription", false),
        ("showMetadata", false),
    ] {
        for value in [
            json!(nondefault),
            json!("not-a-boolean"),
            Value::Null,
            json!(0),
            json!([]),
            json!({}),
        ] {
            std::fs::write(
                dir.path().join("status.yml"),
                json!({key:value}).to_string(),
            )
            .unwrap();
            let report = validate_configuration(&input).await;
            assert!(!report.valid, "{key}: {value}");
            assert!(report.errors.iter().any(|error| {
                error.contains("unknown, malformed or unsupported nondefault setting")
            }));
        }
    }
    std::fs::remove_file(dir.path().join("status.yml")).unwrap();
    let report = validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("status.showMessage:"))
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn status_stock_catalogue_values_precedence_and_redacted_rejections() {
    let (dir, mut input, fake) = setup();
    let stock: Value = serde_json::from_str(include_str!("../src/status_catalogue.json")).unwrap();
    assert_eq!(stock.as_object().unwrap().len(), 311);
    for value in [stock.clone(), json!({"ERR10007": stock["ERR10007"]})] {
        std::fs::write(dir.path().join("status.yml"), value.to_string()).unwrap();
        let report = validate_configuration(&input).await;
        assert!(report.valid, "{:?}", report.errors);
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning == "status.catalogue: stock entries ignored")
        );
    }
    for field in [
        "statusCode",
        "code",
        "message",
        "description",
        "metadata",
        "unknown",
    ] {
        let mut value = json!({"ERR10007":stock["ERR10007"]});
        value["ERR10007"][field] = json!("test-only-private-override");
        std::fs::write(dir.path().join("status.yml"), value.to_string()).unwrap();
        let report = validate_configuration(&input).await;
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .all(|error| !error.contains("test-only-private-override"))
        );
    }
    for value in [
        json!({"ERR999999":{}}),
        json!({"unknownSetting":true}),
        json!([]),
        Value::Null,
    ] {
        std::fs::write(dir.path().join("status.yml"), value.to_string()).unwrap();
        assert!(!validate_configuration(&input).await.valid);
    }
    std::fs::write(dir.path().join("status.yml"), "[malformed").unwrap();
    assert!(!validate_configuration(&input).await.valid);
    std::fs::create_dir(&input.local_dir).unwrap();
    let defaults = dir.path().join("defaults");
    std::fs::create_dir(&defaults).unwrap();
    input.default_dir = Some(defaults.clone());
    std::fs::write(defaults.join("values.yml"), "status.showMetadata: false\n").unwrap();
    std::fs::write(
        input.local_dir.join("values.yml"),
        "status.showMetadata: true\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("status.yml"),
        "showMetadata: ${status.showMetadata:true}\n",
    )
    .unwrap();
    assert!(validate_configuration(&input).await.valid);
    std::fs::write(
        dir.path().join("values.yml"),
        "status.showMetadata: false\n",
    )
    .unwrap();
    assert!(!validate_configuration(&input).await.valid);
    std::fs::write(dir.path().join("values.yml"), "status.showMetadata: true\n").unwrap();
    assert!(validate_configuration(&input).await.valid);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn local_precedence_immutable_snapshot_and_no_control_plane() {
    let (dir, mut input, fake) = setup();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    std::fs::create_dir(&input.local_dir).unwrap();
    std::fs::write(
        input.local_dir.join("startup.yml"),
        format!(
            "configServerUri: http://{}\n",
            listener.local_addr().unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("values.yml"),
        "light-lambda.stage: deployed-test\n",
    )
    .unwrap();
    input.default_dir = Some(dir.path().join("missing-default"));
    let snapshot = initialize(input).await.unwrap();
    let digest = snapshot.config_digest().to_owned();
    assert_eq!(digest.len(), 64);
    std::fs::write(
        dir.path().join("handler.yml"),
        "invalid: test-only-secret\n",
    )
    .unwrap();
    assert_eq!(
        handle(snapshot.clone(), event()).await.unwrap()["statusCode"],
        200
    );
    assert_eq!(snapshot.config_digest(), digest);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn missing_directory_invalid_settings_manifest_and_identity_fail_closed() {
    for case in [
        "missing",
        "stage",
        "budget",
        "limit",
        "manifest",
        "identity",
        "unimplemented",
        "registry",
    ] {
        let (dir, mut input, fake) = setup();
        match case {
            "missing"=>input.external_dir=dir.path().join("does-not-exist"),
            "stage"=>std::fs::write(dir.path().join("light-lambda.yml"),"stage: ''\n").unwrap(),
            "budget"=>std::fs::write(dir.path().join("light-lambda.yml"),"stage: test\npostReserveMs: 250\nresponseReserveMs: 250\n").unwrap(),
            "limit"=>std::fs::write(dir.path().join("light-lambda.yml"),"stage: test\nmaxResponseBytes: 4095\n").unwrap(),
            "manifest"=>std::fs::write(dir.path().join("light-lambda-layer.json"),json!({"schema":"light-lambda-layer/v1","files":[{"path":"handler.yml","sha256":"0".repeat(64)}],"configDigest":"a".repeat(64),"createdAt":"2026-10-12T00:00:00Z"}).to_string()).unwrap(),
            "identity"=>input.gateway_identity.function_name="wrong-function".into(),
            "unimplemented"=>{
                let class=COMPATIBILITY.iter().find(|c|c.id==HandlerId::Jwt).unwrap().class;
                std::fs::write(dir.path().join("handler.yml"),json!({"enabled":true,"handlers":[format!("{class}@jwt")],"defaultHandlers":["jwt"]}).to_string()).unwrap();
            },
            "registry"=>std::fs::write(dir.path().join("server.yml"),"enableRegistry: true\n").unwrap(),
            _=>unreachable!(),
        }
        assert!(initialize(input).await.is_err(), "{case}");
        assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    }
    let (dir, input, _) = setup();
    let bytes = std::fs::read(dir.path().join("handler.yml")).unwrap();
    std::fs::write(dir.path().join("light-lambda-layer.json"),json!({"schema":"light-lambda-layer/v1","files":[{"path":"handler.yml","sha256":format!("{:x}",Sha256::digest(bytes))}],"configDigest":"a".repeat(64),"createdAt":"2026-10-12T00:00:00Z"}).to_string()).unwrap();
    assert_eq!(
        initialize(input).await.unwrap().layer_identity(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
}
struct SlowPolicy;
#[async_trait]
impl RequestHandler for SlowPolicy {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        tokio::time::sleep(Duration::from_millis(140)).await;
        Ok(HandlerOutcome::Continue)
    }
}
struct PolicyProvider;
struct PolicyFactory;
impl HandlerFactoryProvider for PolicyProvider {
    fn create(
        &self,
        _: &light_runtime::RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(Arc::new(PolicyFactory))
    }
}
impl HandlerFactory for PolicyFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Request(
            Arc::new(SlowPolicy),
            RequestCapabilities::default(),
        ))
    }
}
#[tokio::test]
async fn declared_test_policy_consumes_the_same_dispatch_budget_and_disabled_unimplemented_is_allowed()
 {
    for enabled in [false, true] {
        let (dir, mut input, _) = setup();
        let fake = Arc::new(Fake {
            delay: Duration::from_millis(100),
            ..Default::default()
        });
        input.backend = BackendConfiguration::Injected(fake.clone());
        let proxy = COMPATIBILITY
            .iter()
            .find(|c| c.id == HandlerId::Proxy)
            .unwrap()
            .class;
        let policy = COMPATIBILITY
            .iter()
            .find(|c| c.id == HandlerId::Correlation)
            .unwrap()
            .class;
        std::fs::write(dir.path().join("handler.yml"),json!({"enabled":true,"handlers":[format!("{policy}@correlation"),format!("{proxy}@proxy")],"defaultHandlers":["correlation","proxy"]}).to_string()).unwrap();
        std::fs::write(
            dir.path().join("correlation.yml"),
            format!("enabled: {enabled}\n"),
        )
        .unwrap();
        input.handlers.push(HandlerRegistration {
            id: HandlerId::Correlation,
            config_file: "correlation.yml",
            factory: enabled.then(|| Arc::new(PolicyProvider) as Arc<dyn HandlerFactoryProvider>),
        });
        let snapshot = initialize(input).await.unwrap();
        let response = handle(snapshot, event()).await.unwrap();
        assert_eq!(response["statusCode"], if enabled { 504 } else { 200 });
        assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
        if enabled {
            assert!(fake.remaining.lock().unwrap()[0] < Duration::from_millis(100));
        }
    }
}

struct TestJwtProvider;
struct TestJwtFactory;
struct TestJwt;
impl HandlerFactoryProvider for TestJwtProvider {
    fn create(
        &self,
        _: &light_runtime::RuntimeConfig,
        security: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        assert!(security.is_some());
        Ok(Arc::new(TestJwtFactory))
    }
}
impl HandlerFactory for TestJwtFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Request(
            Arc::new(TestJwt),
            RequestCapabilities::default(),
        ))
    }
}
#[async_trait]
impl RequestHandler for TestJwt {
    fn id(&self) -> HandlerId {
        HandlerId::Jwt
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        Ok(HandlerOutcome::Continue)
    }
}
#[tokio::test]
async fn security_child() -> Result<(), InitError> {
    let Ok(endpoint) = std::env::var("WP9_JWKS_ENDPOINT") else {
        return Ok(());
    };
    light_lambda_runtime::telemetry::initialize().unwrap();
    let (dir, mut input, _) = setup();
    std::fs::write(dir.path().join("client.yml"),format!("oauth:\n  token:\n    key:\n      server_url: {endpoint}\n      uri: /keys\n      client_id: test-only-secret-sentinel\n")).unwrap();
    std::fs::write(
        dir.path().join("security.yml"),
        "enabled: true\nbootstrapFromKeyService: true\n",
    )
    .unwrap();
    let jwt = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Jwt)
        .unwrap()
        .class;
    let proxy = COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::Proxy)
        .unwrap()
        .class;
    std::fs::write(dir.path().join("handler.yml"),json!({"enabled":true,"handlers":[format!("{jwt}@jwt"),format!("{proxy}@proxy")],"defaultHandlers":["jwt","proxy"]}).to_string()).unwrap();
    input.handlers.push(HandlerRegistration {
        id: HandlerId::Jwt,
        config_file: "security.yml",
        factory: Some(Arc::new(TestJwtProvider)),
    });
    let result = initialize(input).await;
    if std::env::var("WP9_JWKS_FAILURE").is_ok() {
        assert!(matches!(result, Err(InitError::SecurityBootstrap)));
    } else {
        assert!(result?.security.is_some());
    }
    Ok(())
}
#[tokio::test]
async fn real_security_bootstrap_wiring_with_fake_jwks_not_authentication() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for status in [200, 503] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0; 4096];
            let n = s.read(&mut b).await.unwrap();
            assert!(String::from_utf8_lossy(&b[..n]).starts_with("GET /keys HTTP/1.1"));
            let body = if status == 200 {
                r#"{"keys":[{"kty":"RSA","kid":"test-only-bootstrap","n":"u1WabeaJBoGl","e":"AQAB"}]}"#
            } else {
                "test-only-secret-sentinel"
            };
            s.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let child = tokio::task::spawn_blocking(move || {
            let mut c = std::process::Command::new(std::env::current_exe().unwrap());
            c.args(["--exact", "security_child", "--nocapture"])
                .env_clear()
                .env("RUST_LOG", "trace")
                .env("WP9_JWKS_ENDPOINT", format!("http://{address}"));
            if status == 503 {
                c.env("WP9_JWKS_FAILURE", "1");
            }
            c.output().unwrap()
        });
        let output = tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains("test-only-secret-sentinel"));
        assert!(!String::from_utf8_lossy(&output.stderr).contains("test-only-secret-sentinel"));
        assert!(
            output.status.success(),
            "redacted child result: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        server.await.unwrap();
    }
}
#[tokio::test]
async fn initialized_sdk_backend_uses_only_injected_credentials_and_loopback() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut s, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut b = [0; 4096];
        let end = loop {
            let n = s.read(&mut b).await.unwrap();
            assert!(n > 0);
            bytes.extend_from_slice(&b[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let length = String::from_utf8_lossy(&bytes[..end])
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|v| v.parse::<usize>().ok())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    break end;
                }
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..end]);
        assert!(
            headers
                .starts_with("POST /2015-03-31/functions/test-only-business/invocations HTTP/1.1")
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-amz-invocation-type: requestresponse")
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-amz-log-type: none")
        );
        let payload: Value = serde_json::from_slice(&bytes[end + 4..]).unwrap();
        assert_eq!(payload["requestContext"]["requestId"], "test-request");
        assert!(payload["requestContext"]["accountId"].is_null());
        let body = r#"{"statusCode":200,"body":"","isBase64Encoded":false}"#;
        s.write_all(format!("HTTP/1.1 200 OK\r\nx-amzn-requestid: test-only-sdk\r\nx-amz-executed-version: 1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let (dir, mut input, _) = setup();
    input.environment.allow_insecure_test_endpoints = true;
    std::fs::write(dir.path().join("lambda-proxy.yml"),format!("region: us-east-1\nfunctions:\n  /x@get: test-only-business\nendpointOverride: http://{address}\n")).unwrap();
    input.backend = BackendConfiguration::Sdk(Box::new(
        aws_sdk_lambda::Config::builder()
            .behavior_version_latest()
            .region(aws_sdk_lambda::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_lambda::config::Credentials::new(
                "test-only-access",
                "test-only-secret",
                None,
                None,
                "wp9-test-only",
            ))
            .build(),
    ));
    let snapshot = initialize(input).await.unwrap();
    let response = handle(snapshot, event()).await.unwrap();
    assert_eq!(response["statusCode"], 200);
    assert_eq!(response["body"], "");
    server.await.unwrap();
}
