use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    let steps: Vec<&Value> = f
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![f], |a| a.iter().collect());
    if let Some(mode) = f.get("traceContextProbe") {
        ensure!(mode == "dispatch-v1", "unknown trace-context probe");
        ensure!(
            f["ledger"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r == "Q-01")),
            "trace context requires Q-01"
        );
        ensure!(
            f["config"]["files"]["traceability"]["enabled"] == true,
            "traceability must be enabled"
        );
        let handler = &f["config"]["files"]["handler"];
        ensure!(
            handler["paths"] == json!([])
                && handler["chains"]["request"] == json!(["trace"])
                && handler["chains"]["response"] == json!([])
                && handler["defaultHandlers"] == json!(["request", "proxy", "response"]),
            "closed trace chain cannot be bypassed or shadowed"
        );
        for step in steps {
            for kind in ["expect", "expectJava"] {
                ensure!(
                    step[kind]["traceContext"]["captures"] == 1
                        && step[kind]["backendCalls"]
                            .as_array()
                            .is_some_and(|a| a.len() == 1 && a[0]["target"] == "FnX"),
                    "trace context must be captured at actual sole dispatch"
                );
            }
        }
    } else {
        for step in steps {
            for kind in ["expect", "expectJava"] {
                ensure!(
                    step[kind].get("traceContext").is_none(),
                    "trace observation needs the closed probe"
                );
            }
        }
    }
    Ok(())
}
