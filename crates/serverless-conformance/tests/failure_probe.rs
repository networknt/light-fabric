use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn failed_probe_observations_remain_valid_results_without_weakening_expectations() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/result-v1.schema.json")).unwrap();
    let mut result = json!({"schema":"light-lambda-conformance/result/v1","id":"C-04-failure","runner":"java","runnerRevision":"c7c5413a77c18c537a9a3baa24a44557c96721ae","fixtureDigest":"0000000000000000000000000000000000000000000000000000000000000000","execution":"failed","assertionErrors":["probe did not execute"],"durationMs":1,"observations":[{"outcome":"response","status":200,"headers":[],"headersAbsent":[],"body":"ok","bodyBase64":false,"backendCalls":[],"jwksFetches":0,"failureProbeResult":{"mode":null,"executions":0}}]});
    assert!(schema_validate(&schema, &result).is_ok());
    result["observations"][0]["failureProbeResult"]["executions"] = json!("hidden");
    assert!(schema_validate(&schema, &result).is_err());
}

#[test]
fn request_failure_probe_is_fixed_shared_and_asserted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/c/C-04-throw-test-exception-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture["requestFailureProbe"] = json!("arbitrary-code");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["expect"]["failureProbeResult"]["executions"] = json!(0);
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["expect"]["failureProbeResult"]["mode"] = json!("return-null-status");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["config"]["files"]["conformance-failure-probe"] = json!({});
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["config"]["files"]["handler"]["paths"] =
        json!([{"path":"/pets/fixed","method":"get","exec":["proxy"]}]);
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original;
    fixture["expectJava"]
        .as_object_mut()
        .unwrap()
        .remove("failureProbeResult");
    assert!(cross_validate(&fixture, &rows).is_err());
}
