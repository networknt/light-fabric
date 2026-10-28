use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn closed_lease_requires_exact_lifecycle_and_reachable_response_stage() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/d/D-08c-blocking-response-lease-multi-contract-v1.json"))
            .unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    for (pointer, value) in [
        ("/expect/blockingLease/atResponse/permitsHeld", json!(0)),
        ("/expect/blockingLease/afterReturn/completed", json!(0)),
        ("/expect/blockingLease/lateResultsDiscarded", json!(0)),
        ("/expectJava/blockingLease/atResponse/active", json!(1)),
        ("/context/remainingTimeMs", json!(0)),
        (
            "/config/files/handler/defaultHandlers",
            json!(["request", "proxy"]),
        ),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(cross_validate(&invalid, &ledger).is_err(), "{pointer}");
    }
    let mut invalid = original.clone();
    invalid["expect"]["blockingLease"]["capacity"] = json!(2);
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original;
    invalid
        .as_object_mut()
        .unwrap()
        .remove("blockingLeasePrerequisite");
    assert!(cross_validate(&invalid, &ledger).is_err());
}
