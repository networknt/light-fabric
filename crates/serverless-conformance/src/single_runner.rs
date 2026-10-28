//! Existing result-v1 single-runner contracts; never handler parity.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
pub(crate) fn java_component(f: &Value) -> bool {
    f["javaOnly"] == true
        && f["rustOnly"] == false
        && matches!(
            f["id"].as_str(),
            Some(
                "C-07-stream-init-config-type-component-v1"
                    | "C-07-malformed-stream-fallback-component-v1"
                    | "C-07-valid-stream-no-output-component-v1"
                    | "M-03-influx-null-status-cp1-e02-accepted"
                    | "P-01a-java-bootstrap-component-v1"
                    | "K-01-loopback-create-remove-component-v1"
                    | "K-01-loopback-crud-component-v1"
            )
        )
}
pub(crate) fn supported(f: &Value) -> bool {
    java_component(f)
        || crate::panic_component::supported(f)
        || crate::reporting_component::supported(f)
        || crate::logging_component::supported(f)
        || crate::gate_component::supported(f)
}
pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    if crate::panic_component::supported(f) {
        crate::panic_component::run(f, revision).await
    } else if crate::reporting_component::supported(f) {
        crate::reporting_component::run(f, revision).await
    } else if crate::logging_component::supported(f) {
        crate::logging_component::run(f, revision).await
    } else {
        skip(f, revision)
    }
}
pub(crate) fn coverage(f: &Value) -> &'static str {
    if java_component(f) {
        "single-runner-java-component"
    } else {
        "single-runner-rust-component"
    }
}
pub(crate) fn skip(f: &Value, revision: &str) -> Result<Value> {
    ensure!(java_component(f), "unimplemented single-runner component");
    Ok(
        json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":"skipped","reason":"Java-only component","durationMs":0,"observations":[],"assertionErrors":[]}),
    )
}
pub(crate) fn compare(f: &Value, java: &Value, rust: &Value) -> Result<()> {
    if crate::logging_component::supported(f)
        || crate::gate_component::supported(f)
        || crate::reporting_component::supported(f)
        || crate::panic_component::supported(f)
    {
        ensure!(
            java["execution"] == "skipped"
                && java["reason"] == "rustOnly fixture; gateway not initialized"
                && java["observations"] == json!([]),
            "Rust-only native counterpart contract"
        );
        return assert_rust(f, rust);
    }
    ensure!(java_component(f), "unimplemented single-runner component");
    ensure!(
        rust["execution"] == "skipped"
            && rust["reason"] == "Java-only component"
            && rust["observations"] == json!([])
            && rust["assertionErrors"] == json!([]),
        "Java-only Rust counterpart contract"
    );
    ensure!(
        java["execution"] == "passed"
            && java["observations"]
                .as_array()
                .is_some_and(|a| a.len() == 1),
        "native component must actually pass"
    );
    // The native-only Influx fixture still uses the shared response/call
    // assertion rules. Actual SDK diagnostics are retained, not normalized away.
    if f["id"] == "M-03-influx-null-status-cp1-e02-accepted" {
        return crate::incremental::assert_observation(&f["expect"], &java["observations"][0]);
    }
    for (key, expected) in f["expect"]
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("component expectation"))?
    {
        ensure!(
            java["observations"][0].get(key) == Some(expected),
            "native component {key} differs"
        );
    }
    Ok(())
}

/// Reassert captured component observations for both comparison and retention.
pub(crate) fn assert_rust(f: &Value, rust: &Value) -> Result<()> {
    ensure!(
        supported(f) && !java_component(f),
        "not a Rust-only component"
    );
    ensure!(
        rust["execution"] == "passed"
            && rust["observations"]
                .as_array()
                .is_some_and(|a| a.len() == 1),
        "actual Rust component must pass"
    );
    if crate::panic_component::supported(f) || crate::reporting_component::supported(f) {
        crate::reporting_component::assert(&f["expect"], &rust["observations"][0])
    } else if crate::gate_component::supported(f) {
        crate::incremental::assert_observation(&f["expect"], &rust["observations"][0])
    } else {
        crate::logging_component::assert(&f["expect"], &rust["observations"][0])
    }
}
