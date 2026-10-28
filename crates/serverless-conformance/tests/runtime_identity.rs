use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn init_identity_is_closed_synthetic_and_bound_to_target_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original = load(&root.join("fixtures/v1/d/D-07-self-name-init-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    for declaration in [
        json!({"functionName":"production-name"}),
        json!({"functionName":"test-only-gateway","AWS_REGION":"us-east-1"}),
        json!({}),
    ] {
        let mut fixture = original.clone();
        fixture["runtimeIdentity"] = declaration;
        assert!(schema_validate(&schema, &fixture).is_err());
    }
    let mut unrelated = original;
    unrelated["ledger"] = json!(["E-13"]);
    assert!(cross_validate(&unrelated, &rows).is_err());
}
