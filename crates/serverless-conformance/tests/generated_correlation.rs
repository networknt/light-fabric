use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn generated_id_profile_preserves_exact_names_multiplicity_and_scope() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/q/Q-02-generated-correlation-multi-contract-v1.json"))
            .unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut invalid = original.clone();
    invalid["expect"]["backendCalls"][0]["eventExact"]["multiValueHeaders"]["x-correlation-id"] =
        json!(["${generated:correlation}", "extra"]);
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original.clone();
    invalid["expectJava"]["backendCalls"][0]["eventCorrelation"] = json!({"pointer":"/body"});
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid["expectJava"]["backendCalls"][0]["eventExact"]["headers"]["X-Correlation-Id"] =
        json!("fixed");
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["ledger"] = json!(["E-10"]);
    assert!(cross_validate(&invalid, &ledger).is_err());
}
