use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn bounded_work_requires_reachable_stage_and_valid_shared_reserves() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    for (row, stage) in [("D-08a", "request"), ("D-08b", "response")] {
        let original = load(&root.join(format!(
            "fixtures/v1/d/{row}-{stage}-bounded-work-multi-contract-v1.json"
        )))
        .unwrap();
        assert!(schema_validate(&schema, &original).is_ok());
        assert!(cross_validate(&original, &ledger).is_ok());
        for key in [
            "light-lambda.responseReserveMs",
            "light-lambda.postReserveMs",
        ] {
            let mut invalid = original.clone();
            invalid["config"]["values"]
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(cross_validate(&invalid, &ledger).is_err());
        }
        let mut invalid = original.clone();
        invalid["config"]["files"]["handler"]["paths"] =
            json!([{"path":"/pets/fixed","method":"get","exec":["proxy"]}]);
        assert!(cross_validate(&invalid, &ledger).is_err());
        let mut invalid = original.clone();
        invalid["boundedWorkProbe"] = json!({"delayMs":1000});
        assert!(schema_validate(&schema, &invalid).is_err());
        let mut invalid = original.clone();
        invalid["expect"]["boundedWorkProbeExecutions"] = json!(0);
        assert!(cross_validate(&invalid, &ledger).is_err());
        let mut invalid = original;
        invalid["context"]["remainingTimeMs"] = json!(0);
        assert!(cross_validate(&invalid, &ledger).is_err());
    }
}
