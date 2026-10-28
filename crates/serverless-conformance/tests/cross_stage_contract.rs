use serde_json::{Value, json};
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn closed_cross_stage_probe_requires_both_stages_and_one_shared_budget() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let mut fixture =
        load(&root.join("fixtures/v1/d/D-08b-response-bounded-work-multi-contract-v1.json"))
            .unwrap();
    fixture["boundedWorkProbe"] = json!("request-response-100ms-v1");
    fixture["context"]["remainingTimeMs"] = json!(150);
    fixture["expect"]["boundedWorkProbeExecutions"] = json!(2);
    fixture["expectJava"]["boundedWorkProbeExecutions"] = json!(2);
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let valid =
        |f: &Value| schema_validate(&schema, f).is_ok() && cross_validate(f, &ledger).is_ok();
    assert!(valid(&fixture));
    for pointer in [
        "/context/remainingTimeMs",
        "/expect/boundedWorkProbeExecutions",
        "/expectJava/boundedWorkProbeExecutions",
    ] {
        let mut invalid = fixture.clone();
        *invalid.pointer_mut(pointer).unwrap() = json!(1);
        assert!(!valid(&invalid), "accepted invalid {pointer}");
    }
    let mut invalid = fixture.clone();
    invalid["config"]["files"]["handler"]["chains"]["request"] = Value::Null;
    assert!(!valid(&invalid));
    let mut invalid = fixture;
    invalid["config"]["files"]["handler"]["defaultHandlers"] = json!(["request", "proxy"]);
    assert!(!valid(&invalid));
}
