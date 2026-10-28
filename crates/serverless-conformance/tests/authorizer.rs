use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn authorizer_claims_are_exactly_bound_to_the_declared_input_token() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/e/E-08-verified-authorizer-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &rows).is_ok());
    let mut fixture = original.clone();
    fixture["expect"]["backendCalls"][0]["eventAuthorizer"]["claimsFromToken"] =
        json!("actual-output");
    assert!(schema_validate(&schema, &fixture).is_err());
    fixture = original.clone();
    fixture["expect"]["backendCalls"][0]["eventAuthorizer"]["principalId"] = json!("forged");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["expect"]["backendCalls"][0]["eventAuthorizer"]["scope"] = json!("admin");
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original.clone();
    fixture["rawEvent"]["multiValueHeaders"]["authorization"] = json!(["Bearer forged"]);
    assert!(cross_validate(&fixture, &rows).is_err());
    fixture = original;
    fixture["ledger"]
        .as_array_mut()
        .unwrap()
        .retain(|id| id != "E-08");
    assert!(cross_validate(&fixture, &rows).is_err());
}
