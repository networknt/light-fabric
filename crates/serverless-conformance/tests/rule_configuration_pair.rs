use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn native_cel_pair_is_closed_and_cannot_override_other_configuration() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/t/T-01-T-02-native-cel-body-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture["ruleConfigurationPair"]["rust"]["rule"]["ruleBodies"]["request"]["expression"] =
        json!("arbitrary()");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["config"]["files"]["service"] = json!({});
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["ledger"] = json!(["T-03"]);
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original;
    fixture["rustOnly"] = json!(true);
    assert!(cross_validate(&fixture, &rows).is_err());
}
