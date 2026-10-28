use light_lambda_runtime::{
    transformer_adapter::{Fault, TransformerAdapter},
    transformer_program::Candidate,
};
use serde_json::{Value, json};
use serverless_core::*;
use std::{sync::Arc, time::Duration};
fn invocation(body: &str) -> Invocation {
    let now = tokio::time::Instant::now();
    serverless_aws::alb::decode(json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","pathParameters":{},"multiValueHeaders":{"x-remove":["before"],"test-rule":["not-an-int"]},"multiValueQueryStringParameters":{"before":["value"]},"body":body}),serverless_aws::alb::DecodeContext{request_id:"test-only".into(),deadline:now+Duration::from_secs(2),response_deadline:now+Duration::from_secs(3),cancel:tokio_util::sync::CancellationToken::new()},&serverless_aws::limits::Limits::default()).unwrap()
}
fn fixture(name: &str) -> Arc<Candidate> {
    let file = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1/t")
        .join(name);
    let value: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    Arc::new(Candidate::compile(&value["ruleConfigurationPair"]["rust"]["rule"]).unwrap())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn six_fixed_actions_use_real_transport_adapters_and_caught_context() {
    let state = ExchangeState::default();
    let mut inv = invocation("mutate");
    let adapter = TransformerAdapter::new(
        fixture("T-01-metadata-native-cel-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    assert!(matches!(
        adapter.request(&mut inv, &state).await.unwrap(),
        HandlerOutcome::Continue
    ));
    assert_eq!(inv.path, "/changed");
    assert_eq!(inv.query.0, [("added".into(), "7".into())]);
    assert!(!inv.headers.contains_key("x-remove"));
    assert_eq!(inv.headers["x-added"], "updated");
    assert_eq!(inv.body.unwrap(), "transformed");
    let mut inv = invocation("deny");
    let adapter = TransformerAdapter::new(
        fixture("T-01-deny-native-cel-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let HandlerOutcome::Reject(error) = adapter.request(&mut inv, &state).await.unwrap() else {
        panic!("expected policy denial")
    };
    assert_eq!(error.status_code, 403);
    assert_eq!(error.code, "ERR10075");
    let mut inv = invocation("local");
    let adapter = TransformerAdapter::new(
        fixture("T-01-local-native-cel-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let HandlerOutcome::ContinueWithInitialResponse(response) =
        adapter.request(&mut inv, &state).await.unwrap()
    else {
        panic!("expected initial response")
    };
    assert_eq!(response.status, 202);
    assert_eq!(response.body.unwrap(), "local-rule-body");
    let mut inv = invocation("mutate");
    let adapter = TransformerAdapter::new(
        fixture("T-02-actual-native-response-input-map-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    adapter.request(&mut inv, &state).await.unwrap();
    assert_eq!(inv.body.as_deref(), Some(b"request-transformed".as_slice()));
    let response = GatewayResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: Some(bytes::Bytes::from_static(b"before")),
        body_is_binary: false,
    };
    assert_eq!(
        adapter
            .response(&inv, &state, &response)
            .await
            .unwrap()
            .body
            .unwrap(),
        "response-transformed"
    );
    let adapter = TransformerAdapter::new(
        fixture("T-02-response-header-remove-update-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let mut inv = invocation("before");
    inv.method = http::Method::GET;
    let mut response = response.clone();
    response
        .headers
        .insert("x-remove", "before".parse().unwrap());
    let output = adapter.response(&inv, &state, &response).await.unwrap();
    assert!(!output.headers.contains_key("x-remove"));
    assert_eq!(output.headers["x-added"], "after");
    assert_eq!(output.body, response.body);
    let mut inv = invocation("unchanged");
    let adapter = TransformerAdapter::new(
        fixture("T-01-caught-native-cel-rule-error-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    adapter.request(&mut inv, &state).await.unwrap();
    assert_eq!(inv.body.as_deref(), Some(b"unchanged".as_slice()));
    let adapter = TransformerAdapter::new(
        fixture("T-02-caught-native-cel-rule-error-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    inv.method = http::Method::GET;
    assert_eq!(
        adapter
            .response(&inv, &state, &response)
            .await
            .unwrap()
            .body,
        response.body
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn response_input_profile_fault_is_typed_and_leaves_response_unchanged() {
    let adapter = TransformerAdapter::new(
        fixture("T-02-actual-native-response-input-map-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let mut inv = invocation("mutate");
    let state = ExchangeState::default();
    inv.body = Some(bytes::Bytes::from(vec![b'x'; 32768]));
    assert!(matches!(
        adapter.request(&mut inv, &state).await,
        Err(Fault::InputProfile)
    ));
    let inv = invocation("mutate");
    let response = GatewayResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: Some(bytes::Bytes::from(vec![b'x'; 32768])),
        body_is_binary: false,
    };
    assert!(matches!(
        adapter.response(&inv, &state, &response).await,
        Err(Fault::InputProfile)
    ));
    assert_eq!(response.body.as_ref().unwrap().len(), 32768);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_response_handler_boundary_aggregate_and_deadline_outcomes() {
    let adapter = TransformerAdapter::new(
        fixture("T-02-actual-native-response-input-map-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let mut inv = invocation("");
    let mut state = ExchangeState::default();
    let base = json!({"requestHeaders":{"x-remove":"before","test-rule":"not-an-int"},"responseHeaders":{"x-remove":"before","test-rule":"not-an-int"},"queryParameters":{"before":"value"},"pathParameters":{},"method":"POST","requestPath":"/x","auditInfo":null,"requestBody":"","responseBody":"","statusCode":200}).to_string().len();
    let response = |size| GatewayResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: Some(bytes::Bytes::from(vec![b'x'; size])),
        body_is_binary: false,
    };
    let mut exact = response(32768 - base);
    adapter
        .on_response(&inv, &mut state, &mut exact)
        .await
        .unwrap();
    assert_eq!(exact.status, 200);
    assert_eq!(
        exact.body.as_deref(),
        Some(b"response-transformed".as_slice())
    );
    let mut excess = response(32768 - base + 1);
    adapter
        .on_response(&inv, &mut state, &mut excess)
        .await
        .unwrap();
    assert_eq!(excess.status, 500);
    assert_eq!(excess.body.as_deref(), Some(br#"{"statusCode":500,"code":"ERR14032","message":"TRANSFORMER_OUTPUT_OUTSIDE_PROFILE","description":"Transformer output exceeds the admitted profile.","severity":"ERROR"}"#.as_slice()));
    // Neither input alone exceeds the cap; their complete response context does.
    state.audit.status = None;
    inv.body = Some(bytes::Bytes::from(vec![b'y'; 20000]));
    let mut combined = response(15000);
    adapter
        .on_response(&inv, &mut state, &mut combined)
        .await
        .unwrap();
    assert_eq!(combined.status, 500);
    assert_eq!(inv.body.as_ref().unwrap().len(), 20000);
    inv.response_deadline = tokio::time::Instant::now();
    let mut expired = response(32768);
    adapter
        .on_response(&inv, &mut state, &mut expired)
        .await
        .unwrap();
    assert_eq!(expired.status, 504);
    inv.response_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    inv.cancel.cancel();
    let mut cancelled = response(32768);
    adapter
        .on_response(&inv, &mut state, &mut cancelled)
        .await
        .unwrap();
    assert_eq!(cancelled.status, 504);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_input_boundary_and_output_rollback_use_distinct_reserved_errors() {
    use light_lambda_runtime::transformer_adapter::request_error;
    let state = ExchangeState::default();
    let mut inv = invocation("");
    let base=json!({"requestHeaders":{"x-remove":"before","test-rule":"not-an-int"},"queryParameters":{"before":"value"},"pathParameters":{},"method":"POST","requestPath":"/x","auditInfo":null,"requestBody":""}).to_string().len();
    let adapter = TransformerAdapter::new(
        fixture("T-01-metadata-native-cel-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    inv.body = Some(bytes::Bytes::from(vec![b'x'; 32768 - base]));
    assert!(adapter.request(&mut inv, &state).await.is_ok());
    inv.body = Some(bytes::Bytes::from(vec![b'x'; 32768 - base + 1]));
    let fault = adapter
        .request(&mut inv, &state)
        .await
        .err()
        .expect("profile fault");
    assert_eq!(fault, Fault::InputProfile);
    let error = request_error(fault);
    assert_eq!(
        error.to_bytes(),
        r#"{"statusCode":422,"code":"ERR14031","message":"TRANSFORMER_INPUT_OUTSIDE_PROFILE","description":"Request exceeds the admitted Transformer input profile.","severity":"ERROR"}"#
    );
    let adapter = TransformerAdapter::new(
        fixture("T-02-actual-native-response-input-map-multi-contract-v1.json"),
        vec!["/x".into()],
    );
    let mut inv = invocation("");
    inv.headers.clear();
    inv.query.0.clear();
    let base=json!({"requestHeaders":{"x-fill":""},"queryParameters":{},"pathParameters":{},"method":"POST","requestPath":"/x","auditInfo":null,"requestBody":""}).to_string().len();
    inv.headers.insert(
        "x-fill",
        http::HeaderValue::from_bytes(&vec![b'x'; 32768 - base]).unwrap(),
    );
    let fault = adapter
        .request(&mut inv, &state)
        .await
        .err()
        .expect("profile fault");
    assert_eq!(fault, Fault::OutputProfile);
    assert_eq!(inv.body.as_deref(), Some(b"".as_slice()));
    let error = request_error(fault);
    assert_eq!(error.status_code, 500);
    assert_eq!(error.code, "ERR14032");
    assert_eq!(request_error(Fault::Deadline).code, "ERR14017");
    assert_eq!(request_error(Fault::Internal).code, "ERR14004");
}
