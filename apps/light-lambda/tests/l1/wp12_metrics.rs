use super::*;

fn emf(result: &support::Result) -> Vec<Value> {
    result
        .logs()
        .into_iter()
        .filter(|record| record.get("_aws").is_some())
        .collect()
}

fn explicit_proxy(dir: &std::path::Path, invoke: &RawServer, template: &str) {
    // Keep long template keys out of YAML's implicit-key representation limit.
    std::fs::write(dir.join("lambda-proxy.yml"),format!("enabled: true\nregion: us-east-1\nendpointOverride: {}\nmetricsInjection: false\nlogType: None\nfunctions:\n  ? '{template}@get'\n  : test-only-business\n",invoke.url)).unwrap();
}
fn qualify(record: &Value, route: &str, error: bool, start: u64, end: u64) {
    assert_eq!(
        record
            .as_object()
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
    assert_eq!(record["FunctionName"], "light-lambda-test");
    assert_eq!(record["Route"], route);
    assert_eq!(record["Requests"], 1);
    assert_eq!(record["Errors"], u8::from(error));
    assert!(record["Duration"].as_f64().unwrap() >= 0.0);
    assert!(record["Duration"].as_f64().unwrap() < 5000.0);
    let timestamp = record["_aws"]["Timestamp"].as_u64().unwrap();
    assert!((start..=end).contains(&timestamp));
    let definition = &record["_aws"]["CloudWatchMetrics"][0];
    assert_eq!(definition["Namespace"], "LightLambda");
    assert_eq!(definition["Dimensions"], json!([["FunctionName", "Route"]]));
    assert_eq!(
        definition["Metrics"],
        json!([{"Name":"Requests","Unit":"Count"},{"Name":"Errors","Unit":"Count"},{"Name":"Duration","Unit":"Milliseconds"}])
    );
}

#[tokio::test]
async fn builtin_metrics_exact_boundary_success_denial_backend_error_and_unmatched() {
    let start = now_ms();
    let keys = Keys::new("wp12-metrics");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![
        Reply::ok(),
        Reply {
            status: 500,
            body: json!({"synthetic_business_error":"NO-TELEMETRY-BUSINESS-ERROR"}),
            ..Reply::ok()
        },
    ])
    .await;
    let dir = config(&invoke, &jwks);
    let template = format!("/pets/{{{}}}", "a".repeat(1016));
    assert_eq!(template.len(), 1024);
    explicit_proxy(dir.path(), &invoke, &template);
    let token = keys.token(claims("SENSITIVE-METRICS-PRINCIPAL"));
    let runtime = Runtime::start(
        vec![
            Invocation::new(
                "metrics-success",
                event("/pets/PRIVATE-PATH-ONE", "GET", Some(&token)),
            ),
            Invocation::new(
                "metrics-denial",
                event("/pets/PRIVATE-PATH-TWO", "GET", None),
            ),
            Invocation::new(
                "metrics-backend-error",
                event("/pets/PRIVATE-PATH-THREE", "GET", Some(&token)),
            ),
            Invocation::new(
                "metrics-unmatched",
                event("/UNMATCHED-PRIVATE-PATH", "GET", Some(&token)),
            ),
        ],
        true,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(4).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 4);
    assert_audits(&result, 4);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 1);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 200);
    assert_eq!(posts[1].json()["statusCode"], 401);
    assert!(posts[2].json()["statusCode"].as_u64().unwrap() >= 400);
    // Approved missing proxy-target outcome (distinct from reserved local404).
    assert_eq!(posts[3].json()["statusCode"], 400);
    let records = emf(&result);
    assert_eq!(records.len(), 4);
    for (index, record) in records.iter().enumerate() {
        qualify(
            record,
            if index == 3 { "<unmatched>" } else { &template },
            index != 0,
            start,
            now_ms(),
        );
    }
    for sensitive in [
        &token,
        "PRIVATE-PATH-ONE",
        "PRIVATE-PATH-TWO",
        "PRIVATE-PATH-THREE",
        "UNMATCHED-PRIVATE-PATH",
        "SENSITIVE-METRICS-PRINCIPAL",
        "NO-TELEMETRY-BUSINESS-ERROR",
    ] {
        assert!(
            !records
                .iter()
                .any(|record| record.to_string().contains(sensitive))
        );
        assert!(!result.stdout.contains(sensitive));
        assert!(!result.stderr.contains(sensitive));
    }
    result.save(
        "WP12-metrics-built-app-boundary-errors",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn builtin_metrics_disabled_or_unselected_emits_no_policy_record() {
    for selected in [true, false] {
        let keys = Keys::new("wp12-metrics-excluded");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        update_values(dir.path(), "metrics.enabled", json!(!selected));
        if !selected {
            update_values(
                dir.path(),
                "handler.defaultHandlers",
                json!(["security", "proxy"]),
            );
            // handler.yml owns defaultHandlers directly, so select explicitly.
            std::fs::write(dir.path().join("handler.yml"),json!({"handlers":["com.networknt.aws.lambda.handler.middleware.security.UnifiedSecurityMiddleware@security","com.networknt.aws.lambda.handler.middleware.metrics.APMMetricsMiddleware@metrics","com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware@proxy"],"defaultHandlers":["security","proxy"]}).to_string()).unwrap();
        }
        let token = keys.token(claims("excluded-test-only"));
        let runtime = Runtime::start(
            vec![Invocation::new(
                "excluded",
                event("/x", "GET", Some(&token)),
            )],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        assert_posts(&runtime, 1);
        assert_audits(&result, 1);
        assert_eq!(invoke.count(), 1);
        assert_eq!(jwks.count(), 1);
        assert!(emf(&result).is_empty());
        result.save(
            if selected {
                "WP12-metrics-built-app-disabled"
            } else {
                "WP12-metrics-built-app-unselected"
            },
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}

#[tokio::test]
async fn builtin_metrics_template_overflow_is_offline_init_rejection() {
    let keys = Keys::new("wp12-metrics-init");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let template = format!("/{{{}}}", "a".repeat(1022));
    assert_eq!(template.len(), 1025);
    explicit_proxy(dir.path(), &invoke, &template);
    let mut validate = command(dir.path(), None);
    validate.arg("validate-config");
    let offline = Process::spawn(validate).collect(false).await;
    assert!(!offline.status.success());
    assert!(
        offline
            .stdout
            .contains("handler.yml: invalid execution plan or unavailable handler")
    );
    assert!(!offline.stdout.contains(&template));
    assert!(!offline.stderr.contains(&template));
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    assert!(emf(&offline).is_empty());
    offline.save(
        "WP12-metrics-built-app-overflow-offline",
        None,
        &invoke,
        &jwks,
    );

    let runtime = Runtime::start(vec![], false, false, 202).await;
    let startup = Process::spawn(command(dir.path(), Some(&runtime)))
        .collect(false)
        .await;
    assert!(!startup.status.success());
    let records = runtime.records();
    assert_eq!(records.len(), 1);
    assert!(records[0].path.ends_with("/init/error"));
    assert_eq!(records[0].json()["errorType"], "InvalidConfiguration");
    assert!(!records[0].json().to_string().contains(&template));
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    assert!(emf(&startup).is_empty());
    startup.save(
        "WP12-metrics-built-app-overflow-startup",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn builtin_metrics_timeout_and_oversize_observe_final_posted_status_once() {
    for oversize in [false, true] {
        let start = now_ms();
        let keys = Keys::new("wp12-metrics-replacement");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let reply = if oversize {
            Reply::json(json!({"statusCode":200,"body":"x".repeat(1_100_000)}))
        } else {
            Reply::slow(2000, json!({"statusCode":200,"body":"late"}))
        };
        let invoke = RawServer::start(vec![reply]).await;
        let dir = config(&invoke, &jwks);
        let token = keys.token(claims("replacement-test-only"));
        let mut event = Invocation::new("metrics-replacement", event("/x", "GET", Some(&token)));
        if !oversize {
            event.budget_ms = 1000;
        }
        let runtime = Runtime::start(vec![event], false, false, 202).await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        assert_posts(&runtime, 1);
        assert_audits(&result, 1);
        assert_eq!(invoke.count(), 1);
        let records = emf(&result);
        assert_eq!(records.len(), 1);
        qualify(&records[0], "/*", true, start, now_ms());
        assert_eq!(
            runtime.records()[0].json()["statusCode"],
            if oversize { 502 } else { 504 }
        );
        assert!(!result.stdout.contains(&token));
        assert!(!result.stderr.contains(&token));
        result.save(
            if oversize {
                "WP12-metrics-built-app-oversize"
            } else {
                "WP12-metrics-built-app-timeout"
            },
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}
