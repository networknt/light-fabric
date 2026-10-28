use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn invoke_dependency_is_finite_and_cannot_select_a_cloud_endpoint() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let mut fixture = load(&root.join("fixtures/v1/d/D-05-sdk-loopback-retry-v1.json")).unwrap();
    assert!(schema_validate(&schema, &fixture).is_ok());
    assert!(cross_validate(&fixture, &rows).is_ok());
    fixture["config"]["files"]["lambda-proxy"]["endpointOverride"] =
        json!("https://lambda.us-east-1.amazonaws.com");
    assert!(schema_validate(&schema, &fixture).is_err());
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture["config"]["files"]["lambda-proxy"]["endpointOverride"] = json!("${dependency:invoke}");
    fixture["dependencies"]["invoke"]["responses"] = json!([]);
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture["dependencies"]["invoke"]["responses"] =
        json!([{"status":200,"payload":{},"credentials":"forbidden"}]);
    assert!(schema_validate(&schema, &fixture).is_err());
}
