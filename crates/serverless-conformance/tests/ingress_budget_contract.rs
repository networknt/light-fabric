use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn fixed_ingress_probe_requires_exact_hard_limit_and_dispatch_assertions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let mut f = load(&root.join("fixtures/v1/d/D-08a-request-bounded-work-multi-contract-v1.json"))
        .unwrap();
    f["boundedWorkProbe"] = json!("request-ingress50-100ms-v1");
    f["context"]["remainingTimeMs"] = json!(150);
    f["config"]["values"]["light-lambda.ingressTimeoutMs"] = json!(50);
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    assert!(schema_validate(&schema, &f).is_ok());
    assert!(cross_validate(&f, &ledger).is_ok());
    for pointer in [
        "/context/remainingTimeMs",
        "/config/values/light-lambda.ingressTimeoutMs",
        "/expect/boundedWorkProbeExecutions",
    ] {
        let mut invalid = f.clone();
        *invalid.pointer_mut(pointer).unwrap() = json!(200);
        assert!(
            cross_validate(&invalid, &ledger).is_err(),
            "accepted invalid {pointer}"
        );
    }
    f["boundedWorkProbe"] = json!("request-ingress1-100ms-v1");
    assert!(schema_validate(&schema, &f).is_err());
    assert!(cross_validate(&f, &ledger).is_err());
}
