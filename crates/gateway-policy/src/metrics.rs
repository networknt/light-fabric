//! The approved closed LightLambda-per-request-v1 EMF contract.
use serde_json::{Value, json};
use std::time::Duration;

pub fn record(
    function_name: &str,
    route: &str,
    timestamp_ms: u64,
    duration: Duration,
    error: bool,
) -> Value {
    // Trusted route templates are admitted by the Metrics-aware Init compiler.
    // Preserve their exact bytes; do not invent an alternate dimension value.
    json!({
        "_aws":{"Timestamp":timestamp_ms,"CloudWatchMetrics":[{
            "Namespace":"LightLambda","Dimensions":[["FunctionName","Route"]],"Metrics":[
                {"Name":"Requests","Unit":"Count"},
                {"Name":"Errors","Unit":"Count"},
                {"Name":"Duration","Unit":"Milliseconds"}
            ]
        }]},
        "FunctionName":function_name,"Route":route,"Requests":1,"Errors":u8::from(error),
        "Duration":duration.as_secs_f64()*1000.0,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_fields_dimensions_units_and_error_counts() {
        for error in [false, true] {
            let r = record(
                "test-only-gateway",
                "/pets/fixed",
                123,
                Duration::from_micros(1500),
                error,
            );
            assert_eq!(
                r.as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                [
                    "Duration",
                    "Errors",
                    "FunctionName",
                    "Requests",
                    "Route",
                    "_aws"
                ]
            );
            assert_eq!(r["Requests"], 1);
            assert_eq!(r["Errors"], u8::from(error));
            assert_eq!(r["Duration"], 1.5);
            assert_eq!(r["_aws"]["Timestamp"], 123);
            let definition = &r["_aws"]["CloudWatchMetrics"][0];
            assert_eq!(definition["Namespace"], "LightLambda");
            assert_eq!(definition["Dimensions"], json!([["FunctionName", "Route"]]));
            assert_eq!(
                definition["Metrics"],
                json!([{"Name":"Requests","Unit":"Count"},{"Name":"Errors","Unit":"Count"},{"Name":"Duration","Unit":"Milliseconds"}])
            );
        }
    }
}
