use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn failed_router_component_requires_actual_closed_handler_outcome() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let name = "v1/h/H-04-already-failed-router-component-v1.json";
    let admitted = root.join(format!("fixtures/{name}"));
    let draft = root.join(format!("drafts/{name}"));
    let original = load(if admitted.exists() { &admitted } else { &draft }).unwrap();
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    schema_validate(&schema, &original).unwrap();
    cross_validate(&original, &ledger).unwrap();
    for (field, value) in [
        ("failedState", json!(false)),
        ("status", json!(200)),
        ("httpCalls", json!([{}])),
        ("outcome", json!("response")),
    ] {
        let mut fixture = original.clone();
        fixture["expect"][field] = value;
        assert!(schema_validate(&schema, &fixture).is_err(), "{field}");
    }
    let mut fixture = original;
    fixture.as_object_mut().unwrap().remove("component");
    assert!(schema_validate(&schema, &fixture).is_err());
}
