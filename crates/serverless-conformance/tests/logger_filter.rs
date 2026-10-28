use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn logger_filter_is_an_exact_closed_approved_get_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/x/X-03-current-filter-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut invalid = original.clone();
    invalid["loggerFilterPrerequisite"] = json!({"environment": {"RUST_LOG":"trace"}});
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid["expect"]["body"] = json!("{\"filter\":\"info\",\"extra\":true}");
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original.clone();
    invalid["rawEvent"]["httpMethod"] = json!("POST");
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["ledger"] = json!(["X-03", "E-06a"]);
    assert!(cross_validate(&invalid, &ledger).is_err());
}
