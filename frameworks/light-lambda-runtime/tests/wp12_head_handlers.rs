//! Targeted approved fixture inputs through real Init/compiler/pipeline, not WP13.
use async_trait::async_trait;
use bytes::Bytes;
use config_loader::EmbeddedConfigFile;
use http::{HeaderMap, StatusCode};
use light_lambda_runtime::{head_policy::HeadPolicyProvider, init::*, service::handle};
use serde_json::{Value, json};
use serverless_aws::target::GatewayIdentity;
use serverless_core::*;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test-only\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 50\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /x@get: FnX\nregion: us-east-1\n",
    },
];
#[derive(Default)]
struct Captured {
    calls: AtomicUsize,
    headers: Mutex<Vec<HeaderMap>>,
    state: Mutex<Vec<(Option<String>, Option<String>)>>,
}
#[async_trait]
impl Backend for Captured {
    async fn dispatch(
        &self,
        inv: &Invocation,
        st: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.headers.lock().unwrap().push(inv.headers.clone());
        self.state
            .lock()
            .unwrap()
            .push((st.correlation_id.clone(), st.traceability_id.clone()));
        let mut headers = HeaderMap::new();
        headers.insert("x-backend", "synthetic".parse().unwrap());
        Ok(GatewayResponse {
            status: StatusCode::OK,
            headers,
            body: Some(Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
async fn execute_fixture(text: &str) {
    let fixture: Value = serde_json::from_str(text).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (name, value) in fixture["config"]["files"].as_object().unwrap() {
        // Only owned head/handler inputs are needed by these fixed cases. Other
        // fixture metadata/backend expectations are not interpreted as a runner.
        if ["handler", "header", "correlation", "traceability"].contains(&name.as_str()) {
            std::fs::write(
                dir.path().join(format!("{name}.yml")),
                serde_yaml::to_string(value).unwrap(),
            )
            .unwrap();
        }
    }
    let backend = Arc::new(Captured::default());
    let snapshot = initialize(InitInput {
        embedded: EMBEDDED,
        local_dir: dir.path().join("absent-local"),
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
            account_id: "000000000000".into(),
        },
        handlers: vec![
            HandlerRegistration {
                id: HandlerId::Correlation,
                config_file: "correlation.yml",
                factory: Some(Arc::new(HeadPolicyProvider::Correlation)),
            },
            HandlerRegistration {
                id: HandlerId::Traceability,
                config_file: "traceability.yml",
                factory: Some(Arc::new(HeadPolicyProvider::Traceability)),
            },
            HandlerRegistration {
                id: HandlerId::RequestHeader,
                config_file: "header.yml",
                factory: Some(Arc::new(HeadPolicyProvider::RequestHeader)),
            },
            HandlerRegistration {
                id: HandlerId::ResponseHeader,
                config_file: "header.yml",
                factory: Some(Arc::new(HeadPolicyProvider::ResponseHeader)),
            },
        ],
        finalizers: vec![],
        backend: BackendConfiguration::Injected(backend.clone()),
    })
    .await
    .unwrap();
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-only-head".into();
    context.deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    let result = handle(
        snapshot,
        lambda_runtime::LambdaEvent::new(fixture["rawEvent"].clone(), context),
    )
    .await
    .unwrap();
    assert_eq!(result["statusCode"], fixture["expect"]["status"]);
    assert_eq!(result["body"], fixture["expect"]["body"]);
    assert_eq!(
        result["multiValueHeaders"]["x-added"],
        json!(["response-prefix"])
    );
    assert!(result["multiValueHeaders"].get("x-backend").is_none());
    assert!(
        result["multiValueHeaders"]
            .get("x-correlation-id")
            .is_none()
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    let captured = backend.headers.lock().unwrap();
    let expected = &fixture["expect"]["backendCalls"][0]["eventExact"]["multiValueHeaders"];
    let actual = &captured[0];
    assert_eq!(actual.keys().count(), expected.as_object().unwrap().len());
    for (key, values) in expected.as_object().unwrap() {
        let actual_values: Vec<_> = actual
            .get_all(key)
            .iter()
            .map(|v| v.to_str().unwrap())
            .collect();
        let expected_values: Vec<_> = values
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            actual_values, expected_values,
            "approved Rust multimap contract for {key}"
        );
    }
    let metadata = backend.state.lock().unwrap();
    assert_eq!(
        metadata[0].0.as_deref(),
        actual.get("x-correlation-id").map(|v| v.to_str().unwrap())
    );
    assert_eq!(metadata[0].1.as_deref(), Some("test-only-trace"));
}

#[tokio::test]
async fn approved_prefix_correlation_and_traceability_input() {
    execute_fixture(include_str!("../../../tests/serverless-conformance/fixtures/v1/b/B-04-Q-02-header-prefix-correlation-multi-contract-v1.json")).await;
}
#[tokio::test]
async fn approved_empty_correlation_is_not_regenerated() {
    execute_fixture(include_str!("../../../tests/serverless-conformance/fixtures/v1/q/Q-02-empty-correlation-multi-contract-v1.json")).await;
}
#[tokio::test]
async fn approved_remove_update_prefix_collision_order() {
    execute_fixture(include_str!("../../../tests/serverless-conformance/fixtures/v1/b/B-04-remove-update-prefix-collision-multi-contract-v1.json")).await;
}

#[test]
fn strict_head_configuration_and_mdc_ownership() {
    use serverless_core::handlers::{
        correlation::CorrelationHandler, header::HeaderHandler, traceability::TraceabilityHandler,
    };
    assert!(
        CorrelationHandler::from_value(&json!({"enabled":true,"autogenCorrelationID":false}))
            .is_ok()
    );
    assert!(
        CorrelationHandler::from_value(
            &json!({"enabled":false,"correlationMdcField":"test-only-nondefault"})
        )
        .is_err()
    );
    assert!(
        CorrelationHandler::from_value(&json!({"enabled":false,"autogenCorrelationId":true}))
            .is_err()
    );
    assert!(CorrelationHandler::from_value(&json!({"enabled":"true"})).is_err());
    assert_eq!(
        CorrelationHandler::warnings(&json!({"enabled":false}))
            .unwrap()
            .len(),
        2
    );
    assert!(TraceabilityHandler::from_value(&json!({"enabled":true,"unknown":false})).is_err());
    assert!(
        HeaderHandler::from_yaml(
            serde_yaml::from_str("enabled: true\nunknown: {}\n").unwrap(),
            false
        )
        .is_err()
    );
}

#[test]
fn nested_unknown_header_settings_are_not_silently_ignored_when_disabled() {
    use serverless_core::handlers::header::HeaderHandler;
    for text in [
        "enabled: false\nrequest: {unknown: true}\n",
        "enabled: false\nresponse: {unknown: true}\n",
        "enabled: false\npathPrefixHeader: {'/x': {unknown: true}}\n",
        "enabled: false\npathPrefixHeader: {'/x': {request: {unknown: true}}}\n",
    ] {
        assert!(HeaderHandler::from_yaml(serde_yaml::from_str(text).unwrap(), false).is_err());
    }
}
