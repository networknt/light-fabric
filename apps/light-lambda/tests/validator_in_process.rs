//! Real in-process engine, production Init/factories/service and owned backend.
//! No scripted validation, worker, AWS or claim of wire-level Invoke evidence.
use async_trait::async_trait;
use light_lambda_runtime::{
    init::{self, BackendConfiguration},
    validator_ipc::{accounting::MetadataLimits, in_process::Limits},
};
use serde_json::{Value, json};
use serverless_core::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
const SOURCE: &str = r##"{"openapi":"3.1.0","paths":{"/pets":{"post":{"requestBody":{"required":true,"content":{"application/json":{"schema":{"$ref":"#/components/schemas/Pet"}}}}}}},"components":{"schemas":{"Pet":{"type":"object","required":["name"],"properties":{"name":{"type":"string"},"price":{"type":"number","multipleOf":0.1}}}}}}"##;
fn limits() -> Limits {
    Limits {
        buffer_bytes: 64 << 20,
        body_depth: 128,
        metadata: MetadataLimits {
            max_wire_bytes: 2 << 20,
            max_peak_bytes: 32 << 20,
            max_retained_bytes: 16 << 20,
        },
    }
}
#[derive(Default)]
struct Observations {
    calls: Mutex<Vec<String>>,
}
#[async_trait]
impl Backend for Observations {
    async fn dispatch(
        &self,
        invocation: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.lock().unwrap().push(
            String::from_utf8_lossy(invocation.body.as_deref().unwrap_or_default()).into_owned(),
        );
        Ok(StatusError::new(200, "TEST", "TEST", "owned backend").response())
    }
}
fn fixture(source: &str) -> (tempfile::TempDir, init::InitInput, Arc<Observations>) {
    let dir = tempfile::tempdir().unwrap();
    for (name, value) in [
        (
            "values.yml",
            "light-lambda.stage: test-only\nlight-lambda.ingressTimeoutMs: 2000\nlight-lambda.responseReserveMs: 50\nlight-lambda.postReserveMs: 20\nlambda-proxy.region: us-east-1\nlambda-proxy.metricsInjection: false\n",
        ),
        ("openapi-handler.yml", "enabled: true\n"),
        ("openapi-validator.yml", "enabled: true\n"),
        ("openapi.json", source),
        (
            "lambda-proxy.yml",
            "functions: {'/pets@post': TestFn}\nregion: us-east-1\nmetricsInjection: false\n",
        ),
    ] {
        std::fs::write(dir.path().join(name), value).unwrap();
    }
    let class = |id| {
        compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@validator",class(HandlerId::Validator)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","validator","proxy"]}).to_string()).unwrap();
    let mut input = light_lambda::input_with_in_process_validator(
        Some(dir.path().into()),
        false,
        limits(),
        Duration::from_secs(5),
    )
    .unwrap();
    let backend = Arc::new(Observations::default());
    input.backend = BackendConfiguration::Injected(backend.clone());
    (dir, input, backend)
}
async fn invoke(snapshot: Arc<init::Snapshot>, body: &str) -> Value {
    let mut context = lambda_runtime::Context::default();
    context.request_id = "synthetic-local".into();
    context.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 3000;
    light_lambda_runtime::service::handle(snapshot,lambda_runtime::LambdaEvent::new(json!({"path":"/pets","httpMethod":"POST","multiValueHeaders":{"Content-Type":["application/json"]},"body":body,"isBase64Encoded":false,"requestContext":{"elb":{"targetGroupArn":"local-owned"}}}),context)).await.unwrap()
}
#[tokio::test]
async fn real_init_valid_invalid_warm_valid_exactly_two_dispatches() {
    let (dir, input, backend) = fixture(SOURCE);
    let snapshot = init::initialize(input).await.unwrap();
    // Compilation/catalogue capture must finish before snapshot publication;
    // warm requests never reload or recompile a subsequently changed source.
    std::fs::write(dir.path().join("openapi.json"), "invalid changed source").unwrap();
    let first = r#"{"name":"synthetic-first","price":0.3}"#;
    assert_eq!(invoke(snapshot.clone(), first).await["statusCode"], 200);
    let invalid = invoke(snapshot.clone(), "{}").await;
    assert_eq!(invalid["statusCode"], 400);
    assert!(invalid["body"].as_str().unwrap().contains("ERR11004"));
    assert_eq!(
        backend.calls.lock().unwrap().len(),
        1,
        "invalid request must not dispatch"
    );
    let warm = r#"{"name":"synthetic-warm","price":0.2}"#;
    assert_eq!(invoke(snapshot, warm).await["statusCode"], 200);
    assert_eq!(*backend.calls.lock().unwrap(), [first, warm]);
}
#[tokio::test]
async fn malformed_body_and_exact_numeric_rejection_preserve_warm_isolation() {
    let (_dir, input, backend) = fixture(SOURCE);
    let snapshot = init::initialize(input).await.unwrap();
    for body in [
        "{",
        r#"{"name":"synthetic","price":0.30000000000000000000001}"#,
    ] {
        assert_eq!(invoke(snapshot.clone(), body).await["statusCode"], 400);
        assert!(backend.calls.lock().unwrap().is_empty());
    }
    assert_eq!(
        invoke(snapshot, r#"{"name":"recovered","price":0.3}"#).await["statusCode"],
        200
    );
    assert_eq!(backend.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn invalid_schema_or_remote_reference_never_publishes_or_dispatches() {
    for source in [
        SOURCE.replace("\"multipleOf\":0.1", "\"multipleOf\":-1"),
        SOURCE.replace("#/components/schemas/Pet", "https://unowned.invalid/schema"),
    ] {
        let (_dir, input, backend) = fixture(&source);
        assert!(init::initialize(input).await.is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
#[tokio::test]
async fn offline_validation_stays_closed_and_does_not_compile_an_engine() {
    let (_dir, input, backend) = fixture(SOURCE);
    let report = init::validate_configuration(&input).await;
    assert!(!report.valid);
    assert!(backend.calls.lock().unwrap().is_empty());
}

fn specification_only(dir: &std::path::Path, keep_disabled_validator: bool) {
    std::fs::write(dir.join("openapi-validator.yml"), "enabled: false\n").unwrap();
    if !keep_disabled_validator {
        let class = |id| {
            compat::COMPATIBILITY
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .class
        };
        std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","proxy"]}).to_string()).unwrap();
    }
}
#[tokio::test]
async fn specification_only_offline_admission_retains_the_existing_provider() {
    let (dir, input, backend) = fixture(SOURCE);
    specification_only(dir.path(), false);
    let report = init::validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert!(backend.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn specification_only_real_init_preserves_requests_and_warm_snapshot() {
    let (dir, input, backend) = fixture(SOURCE);
    specification_only(dir.path(), false);
    let snapshot = init::initialize(input).await.unwrap();
    std::fs::write(dir.path().join("openapi.json"), "invalid changed source").unwrap();
    // This is explicitly Specification-only, not Validator qualification:
    // the original chain performs no request schema validation.
    assert_eq!(invoke(snapshot.clone(), "{}").await["statusCode"], 200);
    assert_eq!(
        invoke(snapshot, r#"{"name":"warm"}"#).await["statusCode"],
        200
    );
    assert_eq!(*backend.calls.lock().unwrap(), ["{}", r#"{"name":"warm"}"#]);
}
#[tokio::test]
async fn disabled_validator_binding_and_embedded_disabled_default_keep_specification_working() {
    for absent in [false, true] {
        let (dir, input, backend) = fixture(SOURCE);
        specification_only(dir.path(), true);
        if absent {
            std::fs::remove_file(dir.path().join("openapi-validator.yml")).unwrap();
        }
        assert!(init::validate_configuration(&input).await.valid);
        let snapshot = init::initialize(input).await.unwrap();
        assert_eq!(invoke(snapshot, "{}").await["statusCode"], 200);
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn malformed_validator_activation_cannot_select_the_specification_only_branch() {
    for invalid in [
        "enabled: invalid\n",
        "enabled: true\nskipBodyValidation: invalid\n",
    ] {
        let (dir, input, backend) = fixture(SOURCE);
        std::fs::write(dir.path().join("openapi-validator.yml"), invalid).unwrap();
        assert!(!init::validate_configuration(&input).await.valid);
        assert!(init::initialize(input).await.is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
