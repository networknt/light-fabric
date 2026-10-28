use super::support::*;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

fn class(id: serverless_core::HandlerId) -> &'static str {
    serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == id)
        .unwrap()
        .class
}
fn activate(dir: &Path, enabled: bool, repeats: usize, selected: bool, detailed: bool) {
    use serverless_core::HandlerId;
    std::fs::write(dir.join("audit.yml"), if detailed {
        json!({"enabled":enabled,"auditOnError":false,"statusCode":true,"responseTime":true,"logLevelIsError":true,"auditStackTrace":true,"mask":true,"headers":["x-test-mask","x-credential-like"],"audit":["serviceId","client_id","user_id"],"timestampFormat":"'Bearer AUDIT-TIMESTAMP-PRIVATE'"})
    } else {
        json!({"enabled":enabled,"auditOnError":false,"statusCode":true,"responseTime":false,"mask":false,"headers":[],"audit":["serviceId"]})
    }.to_string()).unwrap();
    std::fs::write(
        dir.join("lambda-app.yml"),
        json!({"lambdaAppId":"trusted-audit-service"}).to_string(),
    )
    .unwrap();
    if detailed {
        std::fs::write(dir.join("mask.yml"),json!({"regex":{"requestHeader":{"x-test-mask":"id=(.+)"},"audit":{"client_id":"(.*)","user_id":"(.*)"}}}).to_string()).unwrap();
    }
    let occurrences: Vec<_> = (0..repeats).map(|_| "audit").collect();
    std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{}@jwt",class(HandlerId::Jwt)),format!("{}@audit",class(HandlerId::Audit)),format!("{}@proxy",class(HandlerId::Proxy))],"chains":{"nestedAudit":occurrences,"outerAudit":["nestedAudit"]},"defaultHandlers":if selected {vec!["jwt","outerAudit","proxy"]} else {vec!["jwt","proxy"]}}).to_string()).unwrap();
}
fn records(result: &super::support::Result) -> Vec<Value> {
    result
        .logs()
        .into_iter()
        .filter_map(|outer| serde_json::from_str(outer["fields"]["audit"].as_str()?).ok())
        .collect()
}
fn framework_finalizations(result: &super::support::Result, count: usize) {
    let rows: Vec<_> = result
        .audits()
        .into_iter()
        .filter(|v| v["fields"]["finalizations"].is_number())
        .collect();
    assert_eq!(rows.len(), count);
    assert!(rows.iter().all(|v| v["fields"]["finalizations"] == 1));
}

#[tokio::test]
async fn actual_audit_values_only_configuration_reaches_stdout_with_no_offline_submission() {
    let keys = Keys::new("audit-values");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), true, 1, true, false);
    std::fs::remove_file(dir.path().join("audit.yml")).unwrap();
    std::fs::remove_file(dir.path().join("lambda-app.yml")).unwrap();
    for (key, value) in [
        ("audit.enabled", json!(true)),
        ("audit.auditOnError", json!(false)),
        ("audit.statusCode", json!(true)),
        ("audit.responseTime", json!(false)),
        ("audit.headers", json!(["x-test-mask"])),
        ("audit.audit", json!(["serviceId"])),
        ("audit.timestampFormat", json!("'safe-values-time'")),
        (
            "mask.regex",
            json!({"requestHeader":{"x-test-mask":"id=(.+)"}}),
        ),
        ("lambda-app.lambdaAppId", json!("trusted-values-service")),
    ] {
        update_values(dir.path(), key, value);
    }
    let mut validate = command(dir.path(), None);
    validate.arg("validate-config");
    let offline = Process::spawn(validate).collect(false).await;
    assert!(offline.status.success(), "{}", offline.stdout);
    assert!(records(&offline).is_empty());
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    let token = keys.token(claims("test-only-values"));
    let mut raw = event("/x", "GET", Some(&token));
    raw["multiValueHeaders"]["x-test-mask"] = json!(["id=VALUES-PRIVATE-MARKER"]);
    let runtime = Runtime::start(
        vec![Invocation::new("audit-values", raw)],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 1);
    framework_finalizations(&result, 1);
    assert_eq!(invoke.count(), 1);
    assert_eq!(jwks.count(), 1);
    let rows = records(&result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["serviceId"], "trusted-values-service");
    assert_eq!(rows[0]["timestamp"], "safe-values-time");
    assert_eq!(rows[0]["statusCode"], 200);
    assert!(
        rows[0]["x-test-mask"]
            .as_str()
            .unwrap()
            .starts_with("id=***")
    );
    assert_eq!(
        rows[0]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["serviceId", "statusCode", "timestamp", "x-test-mask"]
    );
    assert!(!result.stdout.contains("VALUES-PRIVATE-MARKER"));
    assert!(!result.stdout.contains(&token));
    offline.save("WP12-audit-built-app-values-offline", None, &invoke, &jwks);
    result.save(
        "WP12-audit-built-app-values",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn actual_audit_defaults_emit_only_errors_and_use_trusted_template_service_id() {
    let keys = Keys::new("audit-defaults");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![
        Reply::ok(),
        Reply {
            status: 500,
            body: json!({"private":"DEFAULT-ERROR-PRIVATE"}),
            ..Reply::ok()
        },
    ])
    .await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), true, 1, true, false);
    std::fs::write(dir.path().join("audit.yml"), "enabled: true\n").unwrap();
    std::fs::remove_file(dir.path().join("lambda-app.yml")).unwrap();
    let token = keys.token(claims("test-only-default"));
    let runtime = Runtime::start(
        vec![
            Invocation::new("audit-default-success", event("/x", "GET", Some(&token))),
            Invocation::new("audit-default-denial", event("/x", "GET", None)),
            Invocation::new("audit-default-error", event("/x", "GET", Some(&token))),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 3);
    framework_finalizations(&result, 3);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 1);
    let rows = records(&result);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["statusCode"], 401);
    assert_eq!(
        rows[1]["statusCode"],
        runtime.records()[2].json()["statusCode"]
    );
    for row in &rows {
        assert_eq!(row["serviceId"], "com.networknt.placeholder-1.0.0");
        assert!(row["timestamp"].is_u64());
        assert!(row["responseTime"].is_u64());
    }
    assert!(rows[0].get("user_id").is_none());
    assert_eq!(rows[1]["user_id"], "test-only-default");
    assert!(!result.stdout.contains("DEFAULT-ERROR-PRIVATE"));
    assert!(!result.stdout.contains(&token));
    result.save(
        "WP12-audit-built-app-defaults",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn actual_audit_oversize_reports_posted_replacement_once_without_redispatch() {
    let keys = Keys::new("audit-oversize");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::json(
        json!({"statusCode":200,"body":"x".repeat(1_100_000)}),
    )])
    .await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), true, 1, true, false);
    let token = keys.token(claims("test-only-oversize"));
    let runtime = Runtime::start(
        vec![Invocation::new(
            "audit-oversize",
            event("/x", "GET", Some(&token)),
        )],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 1);
    framework_finalizations(&result, 1);
    assert_eq!(invoke.count(), 1);
    assert_eq!(jwks.count(), 1);
    let posted = runtime.records()[0].json();
    assert_eq!(posted["statusCode"], 502);
    let rows = records(&result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["statusCode"], posted["statusCode"]);
    assert!(!result.stdout.contains(&token));
    result.save(
        "WP12-audit-built-app-oversize",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn actual_audit_stdout_success_error_auth_denial_deadlines_redaction_and_warm_isolation() {
    let keys = Keys::new("audit-app");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![
        Reply::ok(),
        Reply {
            status: 500,
            body: json!({"businessError":"AUDIT-BUSINESS-PRIVATE"}),
            ..Reply::ok()
        },
        Reply::slow(500, json!({"statusCode":200,"body":"late"})),
        Reply::ok(),
    ])
    .await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), true, 1, true, true);
    update_values(dir.path(), "light-lambda.responseReserveMs", json!(50));
    update_values(dir.path(), "light-lambda.postReserveMs", json!(25));
    let mut claim = claims("AUDIT-USER-PRIVATE");
    claim["client_id"] = json!("AUDIT-CLIENT-PRIVATE");
    let token = keys.token(claim);
    let request = |id: &str, credential: Option<&str>| {
        let mut raw = event("/x", "GET", credential);
        raw["multiValueHeaders"]["x-test-mask"] = json!(["id=AUDIT-HEADER-PRIVATE"]);
        raw["multiValueHeaders"]["x-credential-like"] = json!(["Bearer AUDIT-HEADER-CREDENTIAL"]);
        Invocation::new(id, raw)
    };
    let mut timeout = request("audit-timeout", Some(&token));
    timeout.budget_ms = 200;
    let mut expired = request("audit-initial-deadline", Some(&token));
    expired.budget_ms = 50;
    let mut denied = request("audit-denial", None);
    denied.pause_ms = 200;
    let runtime = Runtime::start(
        vec![
            request("audit-success", Some(&token)),
            denied,
            request("audit-error", Some(&token)),
            timeout,
            expired,
            request("audit-warm", Some(&token)),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    std::fs::write(
        dir.path().join("lambda-app.yml"),
        json!({"lambdaAppId":"changed-after-init"}).to_string(),
    )
    .unwrap();
    runtime.wait_posts(6).await;
    // Observation window only: queued telemetry has no delivery acknowledgment.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 6);
    framework_finalizations(&result, 6);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 200);
    assert_eq!(posts[1].json()["statusCode"], 401);
    assert!(posts[2].json()["statusCode"].as_u64().unwrap() >= 400);
    assert_eq!(posts[3].json()["statusCode"], 504);
    assert_eq!(posts[4].json()["statusCode"], 504);
    assert_eq!(posts[5].json()["statusCode"], 200);
    assert_eq!(invoke.count(), 4);
    assert_eq!(jwks.count(), 1);
    let rows = records(&result);
    assert_eq!(
        rows.len(),
        6,
        "best-effort delivery observed in this unsaturated run"
    );
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row["serviceId"], "trusted-audit-service");
        assert_eq!(row["timestamp"], "<redacted>");
        assert_eq!(row["statusCode"], posts[index].json()["statusCode"]);
        assert!(row["responseTime"].as_u64().unwrap() < 5000);
        if index == 1 || index == 4 {
            assert!(row.get("x-test-mask").is_none());
        } else {
            assert!(row["x-test-mask"].as_str().unwrap().starts_with("id=***"));
            assert_eq!(row["x-credential-like"], "<redacted>");
            assert!(row["client_id"].as_str().unwrap().chars().all(|c| c == '*'));
        }
    }
    let policy_outer: Vec<_> = result
        .logs()
        .into_iter()
        .filter(|v| v["fields"]["audit"].is_string())
        .collect();
    assert!(
        policy_outer
            .iter()
            .all(|v| v["level"] == "ERROR" && v["fields"]["stackTrace"] == "<redacted>")
    );
    for secret in [
        &token,
        "AUDIT-TIMESTAMP-PRIVATE",
        "AUDIT-BUSINESS-PRIVATE",
        "AUDIT-USER-PRIVATE",
        "AUDIT-CLIENT-PRIVATE",
        "AUDIT-HEADER-PRIVATE",
        "AUDIT-HEADER-CREDENTIAL",
        "changed-after-init",
    ] {
        assert!(!result.stdout.contains(secret));
        assert!(!result.stderr.contains(secret));
    }
    result.save(
        "WP12-audit-built-app-outcomes",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn actual_audit_stdout_disabled_unselected_and_nested_occurrence_counts() {
    for (enabled, selected, repeats, expected) in [
        (false, true, 65, 0),
        (true, false, 65, 0),
        (true, true, 2, 2),
        (true, true, 64, 64),
    ] {
        let keys = Keys::new("audit-occurrences");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        activate(dir.path(), enabled, repeats, selected, false);
        let token = keys.token(claims("test-only"));
        let runtime = Runtime::start(
            vec![Invocation::new(
                "audit-occurrences",
                event("/x", "GET", Some(&token)),
            )],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let result = process.collect(true).await;
        assert_posts(&runtime, 1);
        framework_finalizations(&result, 1);
        assert_eq!(runtime.records()[0].json()["statusCode"], 200);
        assert_eq!(invoke.count(), 1);
        assert_eq!(jwks.count(), 1);
        assert_eq!(records(&result).len(), expected);
        result.save(
            &format!("WP12-audit-built-app-occurrences-{enabled}-{selected}-{repeats}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}

#[tokio::test]
async fn actual_audit_overflow_and_malformed_config_reject_before_any_policy_record() {
    for overflow in [true, false] {
        let keys = Keys::new("audit-init");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        activate(dir.path(), true, if overflow { 65 } else { 1 }, true, false);
        if !overflow {
            std::fs::write(
                dir.path().join("audit.yml"),
                "enabled: false\nstatusCode: []\n",
            )
            .unwrap();
        }
        let mut validate = command(dir.path(), None);
        validate.arg("validate-config");
        let offline = Process::spawn(validate).collect(false).await;
        assert!(!offline.status.success());
        assert!(records(&offline).is_empty());
        let runtime = Runtime::start(vec![], false, false, 202).await;
        let startup = Process::spawn(command(dir.path(), Some(&runtime)))
            .collect(false)
            .await;
        assert!(!startup.status.success());
        assert!(records(&startup).is_empty());
        let posts = runtime.records();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].path.ends_with("/init/error"));
        assert_eq!(posts[0].json()["errorType"], "InvalidConfiguration");
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
        offline.save(
            &format!("WP12-audit-built-app-init-offline-{overflow}"),
            None,
            &invoke,
            &jwks,
        );
        startup.save(
            &format!("WP12-audit-built-app-init-startup-{overflow}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}

#[tokio::test]
async fn actual_audit_utf8_header_bounds_reach_sdk_and_finalize_successfully() {
    let keys = Keys::new("audit-utf8");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), true, 1, true, false);
    std::fs::write(dir.path().join("audit.yml"), json!({"enabled":true,"auditOnError":false,"statusCode":true,"responseTime":false,"mask":false,"headers":["x-test-header"],"audit":["serviceId"],"timestampFormat":"'synthetic-time'"}).to_string()).unwrap();
    let token = keys.token(claims("test-only-utf8"));
    let values = ["é".repeat(512), "é".repeat(513)];
    let invocations = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let mut raw = event("/x", "GET", Some(&token));
            raw["multiValueHeaders"]["x-test-header"] = json!([value]);
            Invocation::new(&format!("audit-utf8-{index}"), raw)
        })
        .collect();
    let runtime = Runtime::start(invocations, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 2);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 1);
    for (wire, value) in invoke.records().iter().zip(&values) {
        assert_eq!(wire.json()["headers"]["x-test-header"], *value);
        assert_eq!(
            wire.json()["multiValueHeaders"]["x-test-header"],
            json!([value])
        );
    }
    let audit = records(&result);
    assert_eq!(audit.len(), 2);
    assert!(audit.iter().all(|record| record["statusCode"] == 200));
    assert_eq!(audit[0]["x-test-header"], values[0]);
    assert_eq!(audit[1]["x-test-header"], "<truncated>");
    framework_finalizations(&result, 2);
    assert!(!result.stdout.contains(&token));
    result.save("WP13-audit-utf8-built-app", Some(&runtime), &invoke, &jwks);
}
