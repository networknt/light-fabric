use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    fn expectation(
        value: &Value,
        allowed: bool,
        cfg05: bool,
        panic: bool,
        complete: bool,
    ) -> Result<()> {
        if let Some(report) = value.get("initFailureReporting") {
            ensure!(
                allowed && cfg05 && value["outcome"] == "initError",
                "Init reporting is closed to CFG-05 Rust whole-process Init failures"
            );
            if !complete {
                ensure!(
                    report
                        == &json!({"endpoint":"/2018-06-01/runtime/init/error","reportCount":1,"exitCode":1}),
                    "Other previously admitted Init reports retain their original assertions"
                );
                return Ok(());
            }
            let (error_type, message) = if panic {
                (
                    "Runtime.InitPanic",
                    "prepare_local_config: synthetic Init panic",
                )
            } else {
                (
                    "InvalidConfiguration",
                    "configuration: missing light-lambda.stage",
                )
            };
            let expected = json!({
                "endpoint":"/2018-06-01/runtime/init/error", "reportCount":1, "exitCode":1,
                "header":{"Lambda-Runtime-Function-Error-Type":"Runtime.InitError"},
                "body":{"errorType":error_type,"errorMessage":message}, "maxBodyBytes":1024,
                "forbiddenBodyValues":["test-only-secret-sentinel","test-only-password-sentinel"],
                "invocationLoopEntered":false
            });
            ensure!(
                report == &expected,
                "CFG-05 requires the complete approved closed report"
            );
            ensure!(
                value["code"] == error_type,
                "Init code must match report error type"
            );
        }
        Ok(())
    }
    let cfg05 = f["ledger"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row == "CFG-05"));
    let panic = f.get("initPanicProbe").is_some();
    let legacy_ids = [
        "S-14-test-override-does-not-relax-max-skew-init-contract-v1",
        "D-07-self-name-alias-init-contract-v1",
        "D-07-self-name-version-init-contract-v1",
        "D-07-self-arn-alias-init-contract-v1",
        "D-07-self-arn-version-init-contract-v1",
    ];
    let complete = !f["id"].as_str().is_some_and(|id| legacy_ids.contains(&id));
    if panic {
        ensure!(
            cfg05
                && f["rustOnly"] == true
                && f["initOnly"] == true
                && f["initPanicProbe"] == "prepare-local-config-panic-v1"
                && f["expect"].get("initFailureReporting").is_some(),
            "Init panic probe requires the closed Rust-only CFG-05 Init report"
        );
    }
    for key in [
        "expectRustInit",
        "expectInit",
        "expect",
        "expectJava",
        "expectJavaInit",
    ] {
        if let Some(value) = f.get(key) {
            let allowed = key == "expectRustInit"
                || (key == "expect" && (f["rustOnly"] == true || f.get("expectJava").is_some()))
                || (key == "expectInit"
                    && (f["rustOnly"] == true || f.get("expectJavaInit").is_some()));
            expectation(value, allowed, cfg05, panic, complete)?;
        }
    }
    if let Some(steps) = f["steps"].as_array() {
        for step in steps {
            for key in ["expect", "expectJava"] {
                if let Some(value) = step.get(key) {
                    expectation(value, false, cfg05, panic, complete)?;
                }
            }
        }
    }
    Ok(())
}
