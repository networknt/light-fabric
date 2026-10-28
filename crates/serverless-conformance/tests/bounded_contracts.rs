use serde_json::{Value, json};
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance")
}
fn fixture(area: &str, id: &str) -> Value {
    let admitted = root().join(format!("fixtures/v1/{area}/{id}.json"));
    let draft = root().join(format!("drafts/v1/{area}/{id}.json"));
    assert!(!(admitted.exists() && draft.exists()));
    load(if admitted.exists() { &admitted } else { &draft }).unwrap()
}
fn valid(f: &Value) -> bool {
    let ledger = ledger_rows(&fs::read_to_string(root().join("LEDGER.md")).unwrap()).unwrap();
    schema_validate(
        &load(&root().join("schema/fixture-v1.schema.json")).unwrap(),
        f,
    )
    .is_ok()
        && cross_validate(f, &ledger).is_ok()
}
#[test]
fn oauth_closed_infrastructure_and_acquisition_are_required() {
    let f = fixture(
        "b",
        "B-01-loopback-acquisition-cache-secondary-multi-contract-v1",
    );
    assert!(valid(&f));
    for pointer in [
        "/config/files/client/tls/verifyHostname",
        "/config/files/client/request/timeout",
    ] {
        let mut bad = f.clone();
        *bad.pointer_mut(pointer).unwrap() = json!(false);
        assert!(!valid(&bad));
    }
    let mut bad = f.clone();
    bad["config"]["files"]["service"] = json!({});
    assert!(!valid(&bad));
    let mut bad = f.clone();
    bad["steps"][0]["expect"]["httpCalls"] = json!([]);
    assert!(!valid(&bad));
    let mut bad = f;
    bad["expectRustInit"] = json!({"outcome":"initError","code":"InvalidConfiguration","backendCalls":[],"jwksFetches":0});
    assert!(!valid(&bad));
}
#[test]
fn response_counter_contracts_cover_short_circuit_timeout_and_replacement() {
    for (area, id) in [
        ("c", "C-09-response-policy-error-counter-multi-contract-v1"),
        (
            "d",
            "D-09-response-policy-headers-limit-counter-multi-contract-v1",
        ),
        (
            "d",
            "D-08e-response-timeout-emf-finalization-counter-multi-contract-v1",
        ),
    ] {
        let f = fixture(area, id);
        assert!(valid(&f), "{id}");
        let mut bad = f.clone();
        bad["expect"]["responsePolicyObservation"]["entries"] = json!(2);
        assert!(!valid(&bad));
        let mut bad = f.clone();
        bad["responsePolicyProbe"]["handler"] = json!("request0");
        assert!(!valid(&bad));
        let mut bad = f;
        bad["expect"]["responsePolicyObservation"]["finalizations"] = json!(0);
        assert!(!valid(&bad));
    }
}
#[test]
fn logging_schema_and_filter_contract_are_closed() {
    for mode in [
        "default-info-v1",
        "configured-debug-v1",
        "configured-error-v1",
    ] {
        let f = fixture("cfg", &format!("CFG-01-{mode}-contract-v1"));
        assert!(valid(&f));
        let mut bad = f.clone();
        bad["rustOnly"] = json!(false);
        assert!(!valid(&bad));
        let mut bad = f.clone();
        bad["expect"]["stdoutContract"]["records"][0]["fields"]["extra"] = json!("not allowed");
        assert!(!valid(&bad));
        let mut bad = f;
        bad["expect"]["stdoutContract"]["timestamp"] = json!("unrestricted");
        assert!(!valid(&bad));
    }
}
#[test]
fn failed_counter_results_preserve_actual_counts() {
    let schema = load(&root().join("schema/result-v1.schema.json")).unwrap();
    let result = json!({"schema":"light-lambda-conformance/result/v1","id":"test","runner":"java","runnerRevision":"0000000000000000000000000000000000000000","fixtureDigest":"0000000000000000000000000000000000000000000000000000000000000000","execution":"failed","durationMs":1,"assertionErrors":["actual double finalization"],"observations":[{"outcome":"response","status":200,"headers":[],"headersAbsent":[],"body":"ok","bodyBase64":false,"backendCalls":[],"jwksFetches":0,"responsePolicyObservation":{"handler":"response0","entries":2,"completions":2,"finalizations":2}}]});
    assert!(schema_validate(&schema, &result).is_ok());
}
