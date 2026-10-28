use async_trait::async_trait;
use light_lambda_runtime::{init::*, transformer_policy::RequestTransformerPolicyProvider};
use serde_json::{Value, json};
use serverless_core::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
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
        content: "functions:\n  /x@post: TestFn\nregion: us-east-1\n",
    },
];
struct BackendProbe(Arc<AtomicUsize>);
#[async_trait]
impl Backend for BackendProbe {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        assert_eq!(target, &Target::Function("function0".into()));
        assert_eq!(inv.body.as_deref(), Some(b"request-transformed".as_slice()));
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: inv.body.clone(),
            body_is_binary: false,
        })
    }
}
fn setup(dir: &std::path::Path, count: usize, enabled: bool) {
    let rule:Value=serde_json::from_str(include_str!("../../../tests/serverless-conformance/fixtures/v1/t/T-01-T-02-native-cel-body-multi-contract-v1.json")).unwrap();
    std::fs::write(
        dir.join("rule.yml"),
        rule["ruleConfigurationPair"]["rust"]["rule"].to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.join("request-transformer.yml"),
        json!({"enabled":enabled,"appliedPathPrefixes":["/x"]}).to_string(),
    )
    .unwrap();
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let mut selected = vec!["tr"; count];
    selected.push("proxy");
    std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{}@tr",class(HandlerId::RequestTransformer)),format!("{}@proxy",class(HandlerId::Proxy))],"chains":{"repeat":selected},"defaultHandlers":["repeat"]}).to_string()).unwrap();
}
fn input(dir: &std::path::Path, calls: Arc<AtomicUsize>) -> InitInput {
    InitInput {
        embedded: EMBEDDED,
        local_dir: dir.join("absent"),
        default_dir: None,
        external_dir: dir.into(),
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
            id: HandlerId::RequestTransformer,
            config_file: "request-transformer.yml",
            factory: Some(Arc::new(RequestTransformerPolicyProvider)),
        }],
        finalizers: vec![],
        backend: BackendConfiguration::Injected(Arc::new(BackendProbe(calls))),
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_factory_common_init_shared_program_aggregate_and_immutable_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    setup(dir.path(), 32, true);
    let report = validate_configuration(&input(dir.path(), calls.clone())).await;
    assert!(report.valid, "{:?}", report.errors);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let snapshot = initialize(input(dir.path(), calls.clone())).await.unwrap();
    std::fs::write(dir.path().join("rule.yml"), "not-valid-after-Init").unwrap();
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-only".into();
    context.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    let event = lambda_runtime::LambdaEvent::new(
        json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","multiValueHeaders":{},"multiValueQueryStringParameters":{},"body":"before"}),
        context,
    );
    let response = light_lambda_runtime::service::handle(snapshot, event)
        .await
        .unwrap();
    assert_eq!(response["statusCode"], 200);
    assert_eq!(response["body"], "request-transformed");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    setup(dir.path(), 33, true);
    assert!(
        !validate_configuration(&input(dir.path(), calls.clone()))
            .await
            .valid
    );
    // Common profile compilation also rejects non-profile CEL while disabled.
    setup(dir.path(), 0, false);
    let mut rule: Value =
        serde_json::from_slice(&std::fs::read(dir.path().join("rule.yml")).unwrap()).unwrap();
    rule["ruleBodies"]["request"]["expression"] = json!("requestBody.matches('.*')");
    std::fs::write(dir.path().join("rule.yml"), rule.to_string()).unwrap();
    assert!(
        !validate_configuration(&input(dir.path(), calls))
            .await
            .valid
    );
}

struct LargeResponse(Arc<AtomicUsize>);
struct CompletionProbe(Arc<AtomicUsize>);
impl Finalizer for CompletionProbe {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, response: &GatewayResponse) {
        assert_eq!(response.status, 500);
        assert_eq!(response.headers["x-after-transformer"], "visited");
        assert!(
            std::str::from_utf8(response.body.as_ref().unwrap())
                .unwrap()
                .contains("ERR14032")
        );
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[async_trait]
impl Backend for LargeResponse {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from(vec![b'x'; 32768])),
            body_is_binary: false,
        })
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_response_factory_keeps_later_headers_one_dispatch_and_exact_error_bytes() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path(), 0, false);
    std::fs::write(
        dir.path().join("response-transformer.yml"),
        json!({"enabled":true,"appliedPathPrefixes":["/x"]}).to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("header.yml"),
        json!({"enabled":true,"response":{"update":{"x-after-transformer":"visited"}}}).to_string(),
    )
    .unwrap();
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@proxy",class(HandlerId::Proxy)),format!("{}@response",class(HandlerId::ResponseTransformer)),format!("{}@after",class(HandlerId::ResponseHeader))],"defaultHandlers":["proxy","response","after"]}).to_string()).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut options = input(dir.path(), calls.clone());
    let completions = Arc::new(AtomicUsize::new(0));
    options
        .finalizers
        .push(Arc::new(CompletionProbe(completions.clone())));
    options.handlers = vec![
        HandlerRegistration {
            id: HandlerId::ResponseTransformer,
            config_file: "response-transformer.yml",
            factory: Some(Arc::new(
                light_lambda_runtime::transformer_policy::ResponseTransformerPolicyProvider,
            )),
        },
        HandlerRegistration {
            id: HandlerId::ResponseHeader,
            config_file: "header.yml",
            factory: Some(Arc::new(
                light_lambda_runtime::head_policy::HeadPolicyProvider::ResponseHeader,
            )),
        },
    ];
    options.backend = BackendConfiguration::Injected(Arc::new(LargeResponse(calls.clone())));
    let snapshot = initialize(options).await.unwrap();
    let mut context = lambda_runtime::Context::default();
    context.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    let event = lambda_runtime::LambdaEvent::new(
        json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","multiValueHeaders":{},"multiValueQueryStringParameters":{},"body":"before"}),
        context,
    );
    let result = light_lambda_runtime::service::handle(snapshot, event)
        .await
        .unwrap();
    assert_eq!(result["statusCode"], 500);
    assert_eq!(
        result["body"],
        r#"{"statusCode":500,"code":"ERR14032","message":"TRANSFORMER_OUTPUT_OUTSIDE_PROFILE","description":"Transformer output exceeds the admitted profile.","severity":"ERROR"}"#
    );
    assert_eq!(
        result["multiValueHeaders"]["x-after-transformer"],
        json!(["visited"])
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(completions.load(Ordering::SeqCst), 1);
}
