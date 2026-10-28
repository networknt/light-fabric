use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn emf_is_closed_and_cannot_select_a_network_sink_or_hide_errors() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/m/M-01-per-request-emf-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut wrong = original.clone();
    wrong["expect"]["emf"]["duration"] = json!("ignore");
    assert!(schema_validate(&schema, &wrong).is_err());
    let mut wrong = original.clone();
    wrong["expect"]["emf"]["extra"] = json!(true);
    assert!(schema_validate(&schema, &wrong).is_err());
    let mut wrong = original.clone();
    wrong["expect"]["emf"]["errors"] = json!(1);
    assert!(cross_validate(&wrong, &rows).is_err());
    let mut wrong = original.clone();
    wrong["config"]["files"]["handler"]["paths"] =
        json!([{"path":"/pets/fixed","method":"get","exec":["proxy"]}]);
    assert!(cross_validate(&wrong, &rows).is_err());
    let mut wrong = original;
    wrong["config"]["files"]["metrics"]["serverHost"] = json!("127.0.0.1");
    assert!(cross_validate(&wrong, &rows).is_err());
}

#[test]
fn cache_prerequisite_is_fixed_and_cannot_override_shared_configuration() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/s/S-13b-active-refresh-join-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut wrong = original.clone();
    wrong["jwtCachePrerequisite"] = json!("arbitrary-manager");
    assert!(schema_validate(&schema, &wrong).is_err());
    let mut wrong = original;
    wrong["config"]["files"]["service"] = json!({});
    assert!(cross_validate(&wrong, &rows).is_err());
}
