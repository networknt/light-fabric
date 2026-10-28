use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    if f.get("loopbackOauthPrerequisite").is_none() {
        return Ok(());
    }
    ensure!(
        f["loopbackOauthPrerequisite"] == "synthetic-client-token-cache-v1",
        "closed OAuth prerequisite name"
    );
    ensure!(
        f["ledger"]
            .as_array()
            .is_some_and(|r| r.iter().any(|x| x == "B-01")),
        "OAuth prerequisite is B-01 only"
    );
    ensure!(
        f.get("expectRustInit").is_none()
            && f.get("javaOnly") == Some(&json!(false))
            && f.get("rustOnly") == Some(&json!(false)),
        "OAuth readiness must reach both policies"
    );
    let files = &f["config"]["files"];
    ensure!(
        files.get("service").is_none() && files.get("cache").is_none(),
        "OAuth infrastructure is outside policy configuration"
    );
    ensure!(
        files["client"]["tls"]
            == json!({"loadKeyStore":false,"loadTrustStore":false,"verifyHostname":true}),
        "OAuth shared TLS exact settings"
    );
    ensure!(
        files["client"]["request"]
            == json!({"connectTimeout":1000,"timeout":1000,"maxRequestRetry":1}),
        "OAuth request infrastructure exact settings"
    );
    ensure!(
        files["client"]["oauth"]["token"]["server_url"] == "${dependency:http}",
        "OAuth logical dependency must be declared"
    );
    ensure!(
        f["dependencies"]["http"]["responses"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        "OAuth replies required"
    );
    let steps = f["steps"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("OAuth warm steps required"))?;
    for step in steps {
        for field in ["expect", "expectJava"] {
            ensure!(
                step[field]["outcome"] == "response",
                "OAuth prerequisite must not bypass policy through Init rejection"
            );
            ensure!(
                step[field]["httpCalls"].is_array() && step[field]["backendCalls"].is_array(),
                "OAuth exact dependency and business calls required"
            );
        }
    }
    let headers = json!({"authorization":"Basic dGVzdC1vbmx5LWNsaWVudDp0ZXN0LW9ubHktc2VjcmV0","content-type":"application/x-www-form-urlencoded","accept":"application/json"});
    for step in steps {
        for field in ["expect", "expectJava"] {
            for call in step[field]["httpCalls"].as_array().unwrap() {
                ensure!(
                    call["oauthRequest"] == headers,
                    "OAuth credentials and form HTTP fields must be asserted"
                );
            }
        }
    }
    for field in ["expect", "expectJava"] {
        ensure!(
            steps.iter().any(|s| s[field]["httpCalls"]
                .as_array()
                .is_some_and(|v| !v.is_empty())),
            "OAuth actual acquisition must be asserted for each runner"
        );
    }
    Ok(())
}
