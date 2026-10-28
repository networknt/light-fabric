use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn paired_rule_clauses_reject_changed_recipe_and_missing_catch() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    for name in [
        "T-01-caught-native-cel-rule-error-multi-contract-v1",
        "T-02-response-header-remove-update-multi-contract-v1",
    ] {
        let mut fixture = load(&root.join(format!("fixtures/v1/t/{name}.json"))).unwrap();
        schema_validate(&schema, &fixture).unwrap();
        cross_validate(&fixture, &ledger).unwrap();
        if fixture.get("ruleFailureProbe").is_some() {
            fixture["steps"][0]["expectJava"]["ruleFailuresCaught"] = json!(0);
            assert!(cross_validate(&fixture, &ledger).is_err());
            fixture["steps"][0]["expectJava"]["ruleFailuresCaught"] = json!(1);
        }
        fixture["ruleConfigurationPair"]["rust"]["executor"] = json!("mock-rules");
        assert!(schema_validate(&schema, &fixture).is_err());
    }
}
