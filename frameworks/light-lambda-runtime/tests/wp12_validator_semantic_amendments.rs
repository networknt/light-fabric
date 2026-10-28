//! Schema-direction proof only; does not claim OpenAPI/Java response-byte parity.
use jsonschema::Draft;
use serde_json::{Value, json};
fn engine(schema: &Value, value: &Value) -> bool {
    jsonschema::options()
        .with_draft(Draft::Draft202012)
        .build(schema)
        .unwrap()
        .is_valid(value)
}
#[test]
fn approved_exclusive_bounds_and_schema_siblings_have_separate_expectations() {
    let expectations: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/amendments/wp12-validator-dialects-v1.json"
    ))
    .unwrap();
    // OAS3.0 minimum:5,exclusiveMinimum:true lowers to a numeric exclusive bound;
    // OAS3.1 already supplies the numeric bound. No original Java fixture edit.
    for row in expectations["cases"].as_array().unwrap() {
        let id = row["observationId"].as_str().unwrap();
        let valid = if id.contains("exclusive-") {
            let number = id.rsplit('-').next().unwrap().parse::<i64>().unwrap();
            engine(
                &json!({"type":"number","exclusiveMinimum":5}),
                &json!(number),
            )
        } else {
            let mut schema = json!({"$defs":{"S":{"type":"string"}},"$ref":"#/$defs/S"});
            if id.contains("3.1.0") {
                schema["minLength"] = json!(3);
            }
            engine(&schema, &json!("a"))
        };
        assert_eq!(
            valid,
            row["approvedRustSchemaValid"].as_bool().unwrap(),
            "{id}"
        );
    }
    assert!(engine(
        &json!({"anyOf":[{"type":"number","multipleOf":0.25},{"type":"string"}]}),
        &json!(1.25)
    ));
    assert!(engine(
        &json!({"type":"string","format":"date"}),
        &json!("not-a-date")
    ));
    let recursive = json!({"$defs":{"Node":{"type":"object","properties":{"child":{"$ref":"#/$defs/Node"}}}},"$ref":"#/$defs/Node"});
    assert!(engine(&recursive, &json!({"child":{"child":{}}})));
    for (pattern, value) in [
        ("^a$", "a"),
        (".", "a"),
        ("a(?=b)", "ab"),
        (r"(a)\1", "aa"),
        ("é", "é"),
        (r"\p{L}+", "é"),
        ("a{33}", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
    ] {
        assert!(
            engine(&json!({"type":"string","pattern":pattern}), &json!(value)),
            "{pattern}"
        );
    }
    for nullable in [false, true] {
        // Configured Java nullable extension is retained for either version.
        let schema = if nullable {
            json!({"anyOf":[{"type":"string"},{"type":"null"}]})
        } else {
            json!({"type":"string"})
        };
        assert_eq!(engine(&schema, &Value::Null), nullable);
    }
}
