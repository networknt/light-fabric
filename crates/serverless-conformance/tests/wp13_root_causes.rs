use serde_json::{Value, json};
use serverless_conformance::{declarations, fixture_files, incremental, load};
use std::path::PathBuf;

fn fixture(id: &str) -> Value {
    fixture_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures/v1"),
    )
    .unwrap()
    .iter()
    .map(|p| load(p).unwrap())
    .find(|f| f["id"] == id)
    .unwrap()
}

#[test]
fn reviewed_metadata_preserves_original_contracts_and_rejects_stale_sources() {
    for id in [
        "CFG-02-loopback-jks-tls-represented-v1",
        "CFG-02-sdk-netty-trust-boundary-represented-v1",
        "S-13b-active-refresh-join-multi-contract-v1",
        "S-17-controlled-cached-nbf-mutation-v1",
    ] {
        let f = fixture(id);
        let row = declarations::review(&f).unwrap().unwrap();
        assert_eq!(row["originalDeclared"], f["differences"]);
        let mut changed = f.clone();
        changed["config"]["values"]["light-lambda.stage"] = json!("different");
        assert!(declarations::review(&changed).is_err());
        if id.starts_with("S-17") {
            assert_eq!(row["add"], json!(["jwksFetches"]));
            assert_eq!(row["originalRustInit"], f["expectRustInit"]);
            assert_eq!(
                row["bootstrapProof"]["javaInitialization"]["jwksFetches"],
                1
            );
        }
    }
}

#[tokio::test]
async fn dispatch_span_is_bounded_and_does_not_leak_into_the_next_invocation() {
    let mut f = fixture("Q-01a-warm-trace-context-multi-contract-v1");
    let value = "t".repeat(1100);
    f["steps"][0]["rawEvent"]["headers"]["x-traceability-id"] = json!(value);
    if f["steps"][0]["rawEvent"]["multiValueHeaders"].is_object() {
        f["steps"][0]["rawEvent"]["multiValueHeaders"]["x-traceability-id"] = json!([value]);
    }
    f["steps"][0]["expect"]["traceContext"]["exchangeTraceabilityId"] = json!(value);
    f["steps"][0]["expect"]["traceContext"]["logContextTraceabilityId"] = json!("<truncated>");
    // Keep the original exact SDK-event assertion intact in the real fixture;
    // the control only selects the trace observation and service response.
    f["steps"][0]["expect"]["backendCalls"] = json!([{"target":"FnX","event":{}}]);
    let result = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(
        result["execution"], "passed",
        "{}",
        result["assertionErrors"]
    );
    assert_eq!(
        result["observations"][0]["traceContext"]["logContextTraceabilityId"],
        "<truncated>"
    );
    assert_eq!(
        result["observations"][1]["traceContext"]["logContextTraceabilityId"],
        Value::Null
    );
}
