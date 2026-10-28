use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    let Some(mode) = f.get("blockingLeasePrerequisite") else {
        for step in f
            .get("steps")
            .and_then(Value::as_array)
            .map_or_else(|| vec![f], |steps| steps.iter().collect())
        {
            for expected in ["expect", "expectJava"] {
                ensure!(
                    step[expected].get("blockingLease").is_none(),
                    "lease observations require the named prerequisite"
                );
            }
        }
        return Ok(());
    };
    ensure!(
        mode == "response-300ms-capacity1-v1" || mode == "response-300ms-capacity1-two-callers-v1",
        "unknown lease recipe"
    );
    ensure!(
        f["ledger"]
            .as_array()
            .is_some_and(|a| a.iter().any(|r| r == "D-08c")),
        "lease requires D-08c"
    );
    ensure!(
        f.get("steps").is_none()
            && f.get("boundedWorkProbe").is_none()
            && f.get("requestFailureProbe").is_none(),
        "closed lease cannot combine probes or warm steps"
    );
    let handler = &f["config"]["files"]["handler"];
    ensure!(
        handler["paths"] == json!([])
            && handler["chains"]["response"] == json!([])
            && handler["defaultHandlers"] == json!(["request", "proxy", "response"]),
        "lease response stage must be reachable"
    );
    ensure!(
        f["context"]["remainingTimeMs"] == 100
            && f["config"]["values"]["light-lambda.responseReserveMs"] == 40
            && f["config"]["values"]["light-lambda.postReserveMs"] == 20,
        "lease budget/reserves are closed"
    );
    let done = json!({"started":1,"completed":1,"active":0,"permitsHeld":0});
    ensure!(
        f["expectJava"]["blockingLease"]
            == json!({"capacity":1,"atResponse":done,"afterReturn":done,"lateResultsDiscarded":0}),
        "exact Java synchronous lease assertions required"
    );
    ensure!(
        f["expect"]["blockingLease"]
            == json!({"capacity":1,"atResponse":{"started":1,"completed":0,"active":1,"permitsHeld":1},"afterReturn":done,"lateResultsDiscarded":1}),
        "exact Rust late-work lease assertions required"
    );
    let parallel = mode == "response-300ms-capacity1-two-callers-v1";
    if parallel {
        ensure!(
            f["concurrentCopies"] == 2,
            "lease saturation requires exactly two callers"
        );
        for (kind, status) in [("expect", 504), ("expectJava", 200)] {
            ensure!(
                f[kind]["outcome"] == "parallelResponses"
                    && f[kind]["responses"]
                        .as_array()
                        .is_some_and(|a| a.len() == 2 && a.iter().all(|r| r["status"] == status)),
                "closed two-caller lease response contrast required"
            );
        }
    } else {
        ensure!(
            f.get("concurrentCopies").is_none()
                && f["expect"]["status"] == 504
                && f["expectJava"]["status"] == 200,
            "lease timeout contrast required"
        );
    }
    for kind in ["expect", "expectJava"] {
        ensure!(
            f[kind]["backendCalls"]
                .as_array()
                .is_some_and(|a| a.len() == if parallel { 2 } else { 1 }
                    && a.iter().all(|call| call["target"] == "FnX")),
            "response work follows exact business dispatch"
        );
    }
    ensure!(
        f["differences"]
            .as_array()
            .is_some_and(|a| a.iter().any(|d| d == "blockingLease")),
        "lease contrast must be declared"
    );
    Ok(())
}
