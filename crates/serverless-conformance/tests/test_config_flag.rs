use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn test_flag_is_closed_and_cannot_be_an_unscoped_environment_override() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original = load(
        &root.join("fixtures/v1/s/S-14-ignoreJwtExpiry-explicit-test-override-contract-v1.json"),
    )
    .unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut invalid = original.clone();
    invalid["insecureTestConfigPrerequisite"] = json!({"env":{"OTHER":"1"}});
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid["ledger"] = json!(["E-10"]);
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["config"]["files"]["security"]["ignoreJwtExpiry"] = json!(false);
    assert!(cross_validate(&invalid, &ledger).is_err());
}
