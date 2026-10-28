use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn trace_probe_requires_actual_dispatch_and_cannot_be_shadowed() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/q/Q-01a-warm-trace-context-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    for (pointer, value) in [
        ("/traceContextProbe", json!("unrestricted")),
        ("/config/files/traceability/enabled", json!(false)),
        ("/config/files/handler/chains/request", json!([])),
        ("/config/files/handler/defaultHandlers", json!(["proxy"])),
        ("/steps/0/expect/traceContext/captures", json!(0)),
        ("/steps/1/expectJava/backendCalls", json!([])),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(cross_validate(&invalid, &ledger).is_err(), "{pointer}");
    }
    let mut invalid = original;
    invalid.as_object_mut().unwrap().remove("traceContextProbe");
    assert!(cross_validate(&invalid, &ledger).is_err());
}
