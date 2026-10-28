use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

fn fixture(
    name: &str,
    area: &str,
) -> (
    serde_json::Value,
    serde_json::Value,
    std::collections::BTreeMap<String, String>,
) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let admitted = root.join(format!("fixtures/v1/{area}/{name}.json"));
    let draft = root.join(format!("drafts/v1/{area}/{name}.json"));
    (
        load(if admitted.exists() { &admitted } else { &draft }).unwrap(),
        load(&root.join("schema/fixture-v1.schema.json")).unwrap(),
        ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap(),
    )
}

#[test]
fn saturation_requires_two_callers_and_exact_held_lease() {
    let (mut f, schema, ledger) =
        fixture("D-08c-blocking-capacity-two-callers-multi-contract-v1", "d");
    schema_validate(&schema, &f).unwrap();
    cross_validate(&f, &ledger).unwrap();
    f["expect"]["blockingLease"]["atResponse"]["permitsHeld"] = json!(0);
    assert!(cross_validate(&f, &ledger).is_err());
    f["expect"]["blockingLease"]["atResponse"]["permitsHeld"] = json!(1);
    f["concurrentCopies"] = json!(3);
    assert!(cross_validate(&f, &ledger).is_err());
}

#[test]
fn rule_failure_requires_execution_and_closed_engine_input() {
    let (mut f, schema, ledger) =
        fixture("T-02-caught-native-cel-rule-error-multi-contract-v1", "t");
    schema_validate(&schema, &f).unwrap();
    cross_validate(&f, &ledger).unwrap();
    f["steps"][0]["expect"]["ruleFailuresCaught"] = json!(0);
    assert!(cross_validate(&f, &ledger).is_err());
    f["steps"][0]["expect"]["ruleFailuresCaught"] = json!(1);
    f["ruleConfigurationPair"]["rust"]["rule"]["ruleBodies"]["slow"]["expression"] = json!("true");
    assert!(schema_validate(&schema, &f).is_err());
    assert!(cross_validate(&f, &ledger).is_err());
}
