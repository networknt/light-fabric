use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    let Some(probe) = f.get("responsePolicyProbe") else {
        return Ok(());
    };
    let name = probe["handler"].as_str().unwrap_or("");
    ensure!(
        f["ledger"].as_array().is_some_and(|r| r
            .iter()
            .any(|x| x == "C-09" || x == "D-09" || x == "D-08b" || x == "D-08e")),
        "response probe requires approved policy/deadline row"
    );
    ensure!(
        f.get("expectRustInit").is_none(),
        "response probe cannot credit Init rejection"
    );
    if name == "fixtureWork" {
        ensure!(
            f["boundedWorkProbe"] == "response-100ms-v1",
            "counter must name fixed response work probe"
        );
    } else {
        ensure!(name == "response0", "closed named response handler");
        let handler = &f["config"]["files"]["handler"];
        ensure!(handler["handlers"].as_array().is_some_and(|a| a.iter().filter(|x| *x == "com.networknt.aws.lambda.handler.middleware.header.ResponseHeaderMiddleware@response0").count() == 1), "counter requires exactly one unchanged response-header delegate");
        ensure!(
            handler["chains"]["response"].as_array().is_some_and(|a| a
                .iter()
                .filter(|x| *x == "response0")
                .count()
                == 1),
            "named handler must execute in response chain"
        );
    }
    let steps: Vec<&Value> = f["steps"]
        .as_array()
        .map_or_else(|| vec![f], |s| s.iter().collect());
    for step in steps {
        for field in ["expectJava", "expect"] {
            ensure!(
                step[field]["outcome"] == "response",
                "response counter requires actual response contract"
            );
            let short_circuit = field == "expectJava" && f["config"]["files"]["handler"]["handlers"].as_array().is_some_and(|a| a.iter().any(|x| x == "com.networknt.aws.lambda.handler.middleware.specification.OpenApiMiddleware@h0")) && step[field]["status"] == 404;
            let entry = if short_circuit { 0 } else { 1 };
            let completion = if short_circuit || (field == "expect" && name == "fixtureWork") {
                0
            } else {
                1
            };
            ensure!(
                step[field]["responsePolicyObservation"]
                    == json!({"handler":name,"entries":entry,"completions":completion,"finalizations":1}),
                "closed entry/completion/finalization assertion"
            );
            if field == "expect" && name == "fixtureWork" {
                ensure!(
                    step[field]["status"] == 504,
                    "incomplete cooperative work must time out"
                );
            }
        }
    }
    Ok(())
}
