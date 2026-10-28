use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn completion_probe_cannot_select_callbacks_or_hide_repeated_execution() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/c/C-03-completion-probe-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture["completionProbe"] = json!("arbitrary-callback");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["ledger"] = json!(["E-10"]);
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture.as_object_mut().unwrap().remove("completionProbe");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["expect"]["completionProbeResult"]["responseRepeatRejected"] = json!(false);
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original;
    fixture["expect"]["completionProbeResult"]["countAfterRepeat"] = json!(5);
    assert!(cross_validate(&fixture, &rows).is_err());
}
