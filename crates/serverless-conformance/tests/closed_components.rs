use serde_json::json;
use serverless_conformance::{load, schema_validate};
use std::path::PathBuf;

#[test]
fn approved_components_cannot_become_a_general_scenario_language() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let mut bootstrap =
        load(&root.join("fixtures/v1/p/P-01a-java-bootstrap-component-v1.json")).unwrap();
    assert!(schema_validate(&schema, &bootstrap).is_ok());
    bootstrap["command"] = json!(["sh", "-c", "arbitrary"]);
    assert!(schema_validate(&schema, &bootstrap).is_err());
    bootstrap.as_object_mut().unwrap().remove("command");
    bootstrap["component"] = json!("arbitrarySubprocess");
    assert!(schema_validate(&schema, &bootstrap).is_err());

    let mut dynamo =
        load(&root.join("fixtures/v1/k/K-01-loopback-crud-component-v1.json")).unwrap();
    assert!(schema_validate(&schema, &dynamo).is_ok());
    dynamo["dynamoProbe"] = json!("executeArbitraryOperation");
    assert!(schema_validate(&schema, &dynamo).is_err());
    dynamo["dynamoProbe"] = json!("crud");
    dynamo["rawEvent"] = json!({});
    assert!(schema_validate(&schema, &dynamo).is_err());
}
