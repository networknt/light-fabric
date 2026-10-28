use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};
#[test]
fn approved_unmatched_route_schema_is_narrow_and_closed() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let original =
        load(&root.join("fixtures/v1/m/M-01-gateway-error-emf-multi-contract-v1.json")).unwrap();
    assert_eq!(original["expect"]["emf"]["route"], "<unmatched>");
    assert!(schema_validate(&schema, &original).is_ok());
    for invalid in ["<other>", "/synthetic-sensitive-token"] {
        let mut wrong = original.clone();
        wrong["expect"]["emf"]["route"] = serde_json::json!(invalid);
        assert!(schema_validate(&schema, &wrong).is_err());
    }
    let mut wrong = original;
    wrong["expect"]["emf"]["extra"] = serde_json::json!(true);
    assert!(schema_validate(&schema, &wrong).is_err());
}
#[test]
fn approved_unmatched_missing_target_contract_requires_admission_validator_update() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/m/M-01-gateway-error-emf-multi-contract-v1.json")).unwrap();
    assert!(
        cross_validate(&original, &rows).is_ok(),
        "owner-amended trusted/unmatched Route contract must pass admission; source-reviewed HIGH adaptation is pending disposition"
    );
}

#[test]
fn unmatched_is_rejected_for_literal_template_wildcard_and_case_insensitive_method_matches() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/m/M-01-gateway-error-emf-multi-contract-v1.json")).unwrap();
    for mapping in [
        "/pets/fixed@get",
        "/pets/{id}@get",
        "/pets/*@get",
        "/*@GET",
        "/pets/fixed@GeT",
    ] {
        let mut wrong = original.clone();
        wrong["config"]["files"]["lambda-proxy"]["functions"] =
            serde_json::json!({mapping:"FnLiteral"});
        assert!(
            cross_validate(&wrong, &rows).is_err(),
            "matched recipe cannot claim unmatched"
        );
    }
    let mut mismatch = original.clone();
    mismatch["config"]["files"]["lambda-proxy"]["functions"] =
        serde_json::json!({"/pets/fixed@post":"FnLiteral"});
    assert!(cross_validate(&mismatch, &rows).is_ok());
    for mutate in 0..4 {
        let mut wrong = original.clone();
        match mutate {
            0 => wrong["expect"]["emf"]["errors"] = serde_json::json!(0),
            1 => wrong["expect"]["backendCalls"] = serde_json::json!([{"target":"FnLiteral"}]),
            2 => wrong["config"]["files"]["metrics"]["serverHost"] = serde_json::json!("127.0.0.1"),
            _ => wrong["id"] = serde_json::json!("unapproved-sentinel-recipe"),
        }
        assert!(cross_validate(&wrong, &rows).is_err());
    }
}

#[test]
fn matched_recipe_keeps_exact_route_and_rejects_untrusted_or_malformed_configuration() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let rows = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/m/M-01-per-request-emf-multi-contract-v1.json")).unwrap();
    assert!(cross_validate(&original, &rows).is_ok());
    let mut wrong = original.clone();
    wrong["expect"]["emf"]["route"] = serde_json::json!("<unmatched>");
    assert!(cross_validate(&wrong, &rows).is_err());
    for functions in [
        serde_json::json!({"/pets/**@get":"FnLiteral"}),
        serde_json::json!({"/pets/fixed@get":false}),
        serde_json::json!({"/pets/fixed@get_bad":"FnLiteral"}),
        serde_json::json!({"/pets/{a}@get":"FnLiteral","/pets/{b}@GET":"FnLiteral"}),
    ] {
        let mut wrong = original.clone();
        wrong["config"]["files"]["lambda-proxy"]["functions"] = functions;
        assert!(cross_validate(&wrong, &rows).is_err());
    }
    let mut sensitive = original;
    sensitive["rawEvent"]["path"] = serde_json::json!("/synthetic-sensitive-token");
    assert!(
        !cross_validate(&sensitive, &rows)
            .unwrap_err()
            .to_string()
            .contains("synthetic-sensitive-token")
    );
}
