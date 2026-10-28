pub mod support;
use gateway_policy::observation::{RecordKind, RecordSink};
use serde_json::{Value, json};
use serverless_core::{handlers::audit::AuditHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;
#[derive(Default)]
struct Records(Mutex<Vec<(RecordKind, Value)>>);
impl RecordSink for Records {
    fn try_emit(&self, kind: RecordKind, record: Value) {
        self.0.lock().unwrap().push((kind, record));
    }
}
fn settings() -> Value {
    json!({"enabled":true,"statusCode":true,"responseTime":false,"auditOnError":false,"auditStackTrace":false,"mask":false,"requestBodyMaxSize":1000,"audit":["serviceId"],"headers":["x-test-header"],"timestampFormat":"'synthetic-time'"})
}
fn pipeline(
    records: &Arc<Records>,
    counts: &Arc<Counts>,
    settings: Value,
    denial: bool,
) -> Pipeline {
    let mut r = registry();
    r.register(
        HandlerId::Audit,
        Arc::new(
            AuditHandler::from_value(&settings, "test-only-gateway".into(), records.clone())
                .unwrap(),
        ),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    let mut ids = vec![
        (HandlerId::Audit, "audit"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let mut sequence = vec!["audit", "proxy", "response"];
    if denial {
        ids.insert(0, (HandlerId::RequestHeader, "deny"));
        sequence.insert(0, "deny");
        install(
            &mut r,
            HandlerId::RequestHeader,
            Binding::Request(
                Arc::new(Handler::new(
                    HandlerId::RequestHeader,
                    Action::Reject,
                    counts,
                )),
                RequestCapabilities::default(),
            ),
        );
    }
    Pipeline::new(compile(config(&ids, &sequence), &r), vec![counts.clone()])
}
#[tokio::test]
async fn audit_real_handler_matches_safe_fixture_fields_and_exact_utf8_byte_bounds() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = pipeline(&records, &counts, settings(), false);
    for input in [
        "synthetic-value".to_string(),
        "a".repeat(1024),
        "a".repeat(1025),
        "é".repeat(512),
        "é".repeat(513),
        "Bearer synthetic-secret".into(),
    ] {
        let mut inv = invocation();
        inv.headers.insert(
            "x-test-header",
            http::HeaderValue::from_bytes(input.as_bytes()).unwrap(),
        );
        let response = p.execute(inv, &FakeBackend::new(&counts)).await;
        assert_eq!(response.status, 200);
    }
    let data = records.0.lock().unwrap();
    assert_eq!(data.len(), 6);
    assert_eq!(
        data[0].1,
        json!({"timestamp":"synthetic-time","serviceId":"test-only-gateway","statusCode":200,"x-test-header":"synthetic-value"})
    );
    assert_eq!(data[1].1["x-test-header"], "a".repeat(1024));
    assert_eq!(data[2].1["x-test-header"], "<truncated>");
    assert_eq!(data[3].1["x-test-header"], "é".repeat(512));
    assert_eq!(data[4].1["x-test-header"], "<truncated>");
    assert_eq!(data[5].1["x-test-header"], "<redacted>");
    assert_eq!(n(&counts.backend), 6);
    assert_eq!(n(&counts.response_entries), 6);
    assert_eq!(n(&counts.finalized), 6);
}
#[tokio::test]
async fn audit_error_only_deadline_cancellation_and_preparation_error_finalize_once() {
    for mode in 0..5 {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let mut cfg = settings();
        cfg["auditOnError"] = json!(true);
        let p = pipeline(&records, &counts, cfg, mode == 1);
        let mut inv = invocation();
        if mode == 2 {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        if mode == 3 {
            inv.cancel.cancel();
        }
        let response = p
            .execute_prepared(inv, &FakeBackend::new(&counts), |response| {
                if mode == 4 {
                    Err(StatusError::handler_failure())
                } else {
                    Ok(response)
                }
            })
            .await;
        assert_eq!(n(&counts.backend), usize::from(mode == 0 || mode == 4));
        assert_eq!(n(&counts.response_entries), 1);
        assert_eq!(n(&counts.finalized), 1);
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), usize::from(mode != 0));
        if mode != 0 {
            assert_eq!(records[0].1["statusCode"], response.status.as_u16());
        }
    }
}
#[test]
fn unsafe_selectors_unknown_keys_and_invalid_types_fail_without_echoing_configuration() {
    let sink = Arc::new(Records::default());
    for (key, value) in [
        ("headers", json!(["authorization"])),
        ("headers", json!(["x-scope-token"])),
        ("audit", json!(["requestBody"])),
        ("audit", json!(["responseBody"])),
        ("audit", json!(["queryParameters"])),
        ("audit", json!(["pathParameters"])),
        ("audit", json!(["unknown-field"])),
        ("mask", json!("true")),
        ("enabled", Value::Null),
        ("unknown-key", json!("secret-must-not-leak")),
    ] {
        let mut cfg = settings();
        cfg[key] = value;
        let err = AuditHandler::from_value(&cfg, "test-only-gateway".into(), sink.clone())
            .err()
            .unwrap();
        assert!(!format!("{err:?}").contains("secret-must-not-leak"));
    }
}

#[tokio::test]
async fn real_audit_mask_action_retains_request_bytes_and_emits_bounded_selected_header() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let mut cfg = settings();
    cfg["mask"] = json!(true);
    let audit = AuditHandler::from_value(&cfg, "test-only-gateway".into(), records.clone())
        .unwrap()
        .with_mask_regex(&json!({"requestHeader":{"x-test-header":"id=(.+)"}}))
        .unwrap();
    let mut r = registry();
    r.register(HandlerId::Audit, Arc::new(audit));
    let p = Pipeline::new(
        compile(
            config(
                &[(HandlerId::Audit, "audit"), (HandlerId::Proxy, "proxy")],
                &["audit", "proxy"],
            ),
            &r,
        ),
        vec![counts.clone()],
    );
    let mut inv = invocation();
    inv.headers.insert(
        "x-test-header",
        http::HeaderValue::from_bytes("id=é😀".as_bytes()).unwrap(),
    );
    inv.body = Some(bytes::Bytes::from_static(b"synthetic-business-body"));
    let response = p.execute(inv, &FakeBackend::new(&counts)).await;
    assert_eq!(
        response.body,
        Some(bytes::Bytes::from_static(b"synthetic-business-body"))
    );
    let data = records.0.lock().unwrap();
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].1["x-test-header"], "id=***");
    assert!(!data[0].1.to_string().contains("synthetic-business-body"));
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.finalized), 1);
}

#[test]
fn response_body_fixture_configuration_follows_owner_amended_init_contract() {
    let path=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance/fixtures/v1/a/A-01-response-body-finalization-exception-sink-v1.json");
    let fixture: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(fixture["expectRustInit"]["outcome"], "initError");
    assert_eq!(fixture["expectRustInit"]["code"], "InvalidConfiguration");
    assert_eq!(fixture["steps"][0]["expect"]["outcome"], "notExecuted");
    let admitted = AuditHandler::from_value(
        &fixture["config"]["files"]["audit"],
        "test-only-gateway".into(),
        Arc::new(Records::default()),
    );
    assert!(
        matches!(
            admitted,
            Err(InitError::Malformed(
                serverless_core::unsupported::audit::RESPONSE_BODY_ERROR
            ))
        ),
        "owner-amended responseBody contract is an explicit redacted Init rejection"
    );
}
