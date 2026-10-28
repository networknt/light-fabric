use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn response_rule_input_requires_execution_and_preserves_request_header_source() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let mut f =
        load(&root.join("fixtures/v1/t/T-01-T-02-native-cel-body-multi-contract-v1.json")).unwrap();
    f["ruleInputProbe"] = json!("native-response-map-v1");
    let observation = json!({"executions":1,"input":{"requestHeaders":{"x-input":"request-value"},"responseHeaders":{"x-input":"request-value"},"queryParameters":null,"pathParameters":null,"method":"POST","requestPath":"/x","requestBody":"request-transformed","responseBody":"ok","auditInfo":null,"statusCode":200}});
    f["steps"][0]["expectJava"]["ruleInputObservation"] = observation.clone();
    f["steps"][0]["expect"]["ruleInputObservation"] = observation;
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    assert!(schema_validate(&schema, &f).is_ok());
    assert!(cross_validate(&f, &ledger).is_ok());
    let mut invalid = f.clone();
    invalid["steps"][0]["expect"]["ruleInputObservation"]["input"]["responseHeaders"] =
        json!({"x-backend":"synthetic"});
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = f.clone();
    for kind in ["expect", "expectJava"] {
        invalid["steps"][0][kind]["ruleInputObservation"]["executions"] = json!(0);
    }
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = f;
    invalid["steps"][0]["expect"]["ruleInputObservation"]["input"]["unexpected"] = json!(true);
    assert!(schema_validate(&schema, &invalid).is_err());
}
