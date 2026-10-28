use anyhow::{Result, ensure};
use serde_json::Value;

fn subset_matches(subset: &Value, exact: &Value) -> bool {
    match (subset, exact) {
        (Value::Object(fields), Value::Object(actual)) => fields.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|candidate| subset_matches(value, candidate))
        }),
        _ => subset == exact,
    }
}

/// Reject mutually contradictory authored assertions without pretending to execute a runner.
pub(crate) fn validate(fixture: &Value) -> Result<()> {
    let cases = fixture
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![fixture], |steps| steps.iter().collect());
    for (step, case) in cases.into_iter().enumerate() {
        for runner in ["expect", "expectJava"] {
            for (index, call) in case[runner]["backendCalls"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                if let Some(exact) = call["eventExact"].as_object() {
                    for (key, value) in exact {
                        if let Some(subset) = call["event"].get(key) {
                            ensure!(
                                subset_matches(subset, value),
                                "step {step} {runner} backend call {index}: event/{key} contradicts eventExact/{key}"
                            );
                        }
                    }
                }
                for pointer in call["eventAbsent"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    ensure!(
                        call["event"].pointer(pointer).is_none(),
                        "step {step} {runner} backend call {index}: event asserts absent pointer {pointer}"
                    );
                    ensure!(
                        call["eventExact"].pointer(pointer).is_none(),
                        "step {step} {runner} backend call {index}: eventExact asserts absent pointer {pointer}"
                    );
                }
            }
        }
    }
    Ok(())
}

/// Plan5.3's empty-map rule is an authored Rust contract, never Java normalization.
pub(crate) fn validate_rust_business_maps(fixture: &Value) -> Result<()> {
    if fixture["javaOnly"] == true {
        return Ok(());
    }
    let cases = fixture
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![fixture], |steps| steps.iter().collect());
    for case in cases {
        for call in case["expect"]["backendCalls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            for slot in ["event", "eventExact"] {
                for key in [
                    "headers",
                    "multiValueHeaders",
                    "queryStringParameters",
                    "multiValueQueryStringParameters",
                    "pathParameters",
                ] {
                    ensure!(
                        !call[slot][key].as_object().is_some_and(|v| v.is_empty()),
                        "Rust business-envelope {slot}/{key}: plan5.3 requires null when empty"
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn nested_subset_can_coexist_with_exact_extra_fields() {
        validate(&json!({"expect":{"backendCalls":[{"event":{"headers":{"x":"a"}},"eventExact":{"headers":{"x":"a","y":"b"}},"eventAbsent":["/body"]}]}})).unwrap();
    }
    #[test]
    fn contradictory_null_map_or_absence_is_rejected() {
        for call in [
            json!({"event":{"queryStringParameters":null},"eventExact":{"queryStringParameters":{"a":"2"}}}),
            json!({"event":{"body":null},"eventAbsent":["/body"]}),
            json!({"event":{},"eventExact":{"headers":{"x":"a"}},"eventAbsent":["/headers/x"]}),
        ] {
            assert!(validate(&json!({"steps":[{"expect":{"backendCalls":[call]}}]})).is_err());
        }
    }
}
