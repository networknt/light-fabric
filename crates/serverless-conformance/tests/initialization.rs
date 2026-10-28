use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn initialization_is_not_an_http_response_and_bootstrap_calls_are_asserted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/s/S-13b-active-refresh-join-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut invalid = original.clone();
    invalid["expectInit"] = json!({"outcome":"response","status":200,"headers":[],"headersAbsent":[],"body":"ok","bodyBase64":false,"backendCalls":[],"jwksFetches":0});
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid.as_object_mut().unwrap().remove("expectJavaInit");
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original.clone();
    invalid["expectJavaInit"]["jwksCalls"] = json!([]);
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["expectJavaInit"] = json!({"outcome":"exception","phase":"invocation","class":"Test","message":"test","backendCalls":[],"jwksFetches":0,"jwksCalls":[]});
    assert!(schema_validate(&schema, &invalid).is_err());
}
