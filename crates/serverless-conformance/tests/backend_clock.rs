use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn backend_clock_requires_closed_mode_ledger_and_relative_wall_clock() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/e/E-10-configured-metadata-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture["expect"]["backendCalls"][0]["eventClock"] = json!("ignore-time");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["ledger"]
        .as_array_mut()
        .unwrap()
        .retain(|id| id != "E-10g");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["clock"]["mode"] = json!("fixed");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original;
    fixture.as_object_mut().unwrap().remove("clock");
    assert!(cross_validate(&fixture, &rows).is_err());
}
