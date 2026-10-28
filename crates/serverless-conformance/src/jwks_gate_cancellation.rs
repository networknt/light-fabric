//! Contract only. No gate implementation, fixture execution, or mocked results.
use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub fn validate(f: &Value) -> Result<()> {
    if f.get("jwksGateCancellationProbe").is_none() {
        ensure!(
            f["component"] != "rustJwksGateCancellationS13a"
                && f["expect"]["outcome"] != "jwksGateCancellation",
            "cancellation result requires its closed probe"
        );
        return Ok(());
    }
    ensure!(
        f["component"] == "rustJwksGateCancellationS13a"
            && f["rustOnly"] == true
            && f["javaOnly"] == false
            && f["ledger"] == json!(["S-13a"]),
        "cancellation component has no Java asynchronous gate counterpart"
    );
    ensure!(
        f["jwksGateCancellationProbe"]
            == json!({"mode":"abort-started-fetch-v1","serviceIds":["test-provider"],"token":"gate-parallel","requestStartedWaitMs":1000}),
        "unknown cancellation probe"
    );
    ensure!(
        f["config"]["files"]["security"]["bootstrapFromKeyService"] == false
            && f["config"]["files"]["security"]["jwksRefreshMinIntervalSecs"] == 30,
        "cancellation requires no bootstrap and30s cooldown"
    );
    ensure!(
        f["config"]["files"]["client"]["tls"]
            == json!({"loadKeyStore":false,"loadTrustStore":false,"verifyHostname":true}),
        "component uses supported verified client settings"
    );
    ensure!(
        f["dependencies"]["jwks"] == json!({"publishedKeys":["k1"],"delayMs":500})
            && f["tokens"]["gate-parallel"]["kid"] == "k2"
            && f["tokens"]["gate-parallel"]["expOffsetSec"]
                .as_i64()
                .is_some_and(|v| v > 0),
        "cancellation must reach actual unknown-kid fetch with a valid relative token"
    );
    ensure!(
        f["expect"]["outcome"] == "jwksGateCancellation"
            && f["expect"]["jwksFetches"] == 1
            && f["expect"]["jwksCalls"] == json!([{"target":"jwks","method":"GET","uri":"/jwks"}]),
        "real request readiness and exact request required"
    );
    Ok(())
}
