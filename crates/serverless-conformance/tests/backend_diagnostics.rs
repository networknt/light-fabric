use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn function_error_diagnostics_are_closed_and_declared() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original = load(&root.join("fixtures/v1/d/D-03-handled-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture
        .as_object_mut()
        .unwrap()
        .remove("observeBackendDiagnostics");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["ledger"] = json!(["E-10"]);
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["expect"]["backendDiagnostics"]["metadata"]["errorMessage"] = json!("forbidden");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["expect"]["backendDiagnostics"]["metadata"]["errorType"] = json!("bad error type!");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["differences"]
        .as_array_mut()
        .unwrap()
        .retain(|v| v != "backendDiagnostics");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original;
    fixture["backend"][0]["respond"]["functionError"] = json!(null);
    assert!(cross_validate(&fixture, &rows).is_err());
}
