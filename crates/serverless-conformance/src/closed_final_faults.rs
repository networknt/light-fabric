//! Authored contracts only: no reporter, gate, runtime or fixture execution.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    let profiles: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/review/closed-final-fault-contracts.json"
    ))?;
    for (key, component, row) in [
        (
            "initReportTransportProbe",
            "rustInitReportTransportCfg05",
            "CFG-05",
        ),
        ("jwksGatePanicProbe", "rustJwksGatePanicS13c", "S-13c"),
    ] {
        let Some(probe) = f.get(key) else {
            ensure!(
                f["component"] != component,
                "fault component requires its closed declaration"
            );
            continue;
        };
        ensure!(
            f["component"] == component
                && f["rustOnly"] == true
                && f["javaOnly"] == false
                && f["ledger"] == json!([row]),
            "faults are closed Rust-only components"
        );
        let profile = if key == "initReportTransportProbe" {
            ensure!(
                f["config"]["values"].get("light-lambda.stage").is_none(),
                "fault must be triggered by actual missing-stage Init failure"
            );
            &profiles[probe["fault"].as_str().unwrap_or("")]
        } else {
            ensure!(
                f["config"]["files"]["security"]["bootstrapFromKeyService"] == false
                    && f["config"]["files"]["security"]["jwksRefreshMinIntervalSecs"] == 1,
                "panic probe requires no bootstrap and real one-second cooldown"
            );
            ensure!(
                f["dependencies"]["jwks"] == json!({"publishedKeys":["k2"],"delayMs":0})
                    && f["tokens"]["gate-parallel"]["kid"] == "k2"
                    && f["tokens"]["gate-parallel"]["expOffsetSec"]
                        .as_i64()
                        .is_some_and(|s| s > 0),
                "recovery must use valid signed token and actual synthetic key service"
            );
            ensure!(
                f["config"]["files"]["client"]["tls"]
                    == json!({"loadKeyStore":false,"loadTrustStore":false,"verifyHostname":true}),
                "supported verified trust prerequisite required"
            );
            &profiles["shared-fetch-panic"]
        };
        ensure!(
            !profile.is_null() && probe == &profile["probe"] && f["expect"] == profile["expect"],
            "fault declarations and assertions must match the approved closed profile"
        );
    }
    Ok(())
}

pub(crate) fn validate_ingress_diagnostics(f: &Value) -> Result<()> {
    let has = f
        .get("expect")
        .is_some_and(|v| v.get("configurationDiagnostics").is_some())
        || f["steps"].as_array().is_some_and(|steps| {
            steps.iter().any(|s| {
                s["expect"].get("configurationDiagnostics").is_some()
                    || s["expectJava"].get("configurationDiagnostics").is_some()
            })
        });
    if !has {
        return Ok(());
    }
    ensure!(
        f["ledger"] == json!(["E-13"]) && f["javaOnly"] == false && f["rustOnly"] == false,
        "ingress diagnostics is closed to paired E-13"
    );
    let steps = f["steps"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("log-once requires warm steps"))?;
    ensure!(
        steps.len() == 2 && steps[0]["rawEvent"] == steps[1]["rawEvent"],
        "log-once requires two unchanged shared single-value events"
    );
    for (index, step) in steps.iter().enumerate() {
        ensure!(
            step["rawEvent"].get("multiValueHeaders").is_none()
                && step["expectJava"].get("configurationDiagnostics").is_none(),
            "not a Java counter or valid multimap policy request"
        );
        ensure!(
            step["expect"]["configurationDiagnostics"]
                == json!({"multiValueRequiredLogs":if index==0 {1} else {0}})
                && step["expect"]["status"] == 500
                && step["expect"]["backendCalls"] == json!([])
                && step["expect"]["body"]
                    .as_str()
                    .is_some_and(|b| b.contains("ERR14011")),
            "exact once-per-environment diagnostic and ingress-rejection assertions required"
        );
    }
    Ok(())
}

pub(crate) fn validate_configuration_warning(f: &Value) -> Result<()> {
    let Some(observed) = f["expect"].get("configurationWarningObservation") else {
        return Ok(());
    };
    ensure!(
        f["ledger"] == json!(["CFG-04"])
            && f["javaOnly"] == false
            && f["rustOnly"] == false
            && f["initOnly"] == true
            && f["expect"]["outcome"] == "initSuccess"
            && f["expectJava"]["outcome"] == "initSuccess",
        "configuration warning is closed to paired CFG04 Init"
    );
    let value = f["config"]["files"]["handler"].get("reportHandlerDuration");
    let mode = match value {
        None => "absent",
        Some(Value::Bool(false)) => "default-false",
        _ => anyhow::bail!("nondefault key cannot be admitted as a warning"),
    };
    ensure!(
        observed
            == &json!({"key":"handler.reportHandlerDuration","mode":mode,"warningCount":1,"ignored":true}),
        "exact KEY002 default/absent warning required"
    );
    Ok(())
}
