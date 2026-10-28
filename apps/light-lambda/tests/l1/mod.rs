mod baseline;
mod correlation_scalar;
mod logger_set;
mod native_admin;
mod router;
mod support;
mod validator_in_process;
mod validator_profile;
mod wp12_admin;
mod wp12_audit;
mod wp12_metrics;
mod wp12_sanitizer;
mod wp12_specification;
mod wp12_token;
mod wp12_transformer;
mod wp13_diagnostic;
mod wp13_repairs;
use serde_json::{Value, json};
use std::{sync::atomic::Ordering, time::Duration};
use support::*;
use tokio::time::timeout;

#[tokio::test]
async fn wp12_real_jwt_before_identity_limit_and_warm_counter_isolation() {
    let keys = Keys::new("wp12-limit");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "limit.enabled", json!(true));
    update_values(dir.path(), "limit.key", json!("client"));
    update_values(dir.path(), "limit.rateLimit", json!("1/m"));
    update_values(
        dir.path(),
        "limit.client",
        json!({"client-A":"1/m","client-B":"1/m"}),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[
        format!("{}@jwt",class(serverless_core::HandlerId::Jwt)),format!("{}@limit",class(serverless_core::HandlerId::Limit)),format!("{}@proxy",class(serverless_core::HandlerId::Proxy))],"defaultHandlers":["jwt","limit","proxy"]}).to_string()).unwrap();
    let mut a = claims("user-A");
    a["client_id"] = json!("client-A");
    let mut b = claims("user-B");
    b["client_id"] = json!("client-B");
    let a = keys.token(a);
    let b = keys.token(b);
    let events = [a.as_str(), "not-a-token", a.as_str(), b.as_str()]
        .iter()
        .enumerate()
        .map(|(i, token)| Invocation::new(&format!("limit-{i}"), event("/x", "GET", Some(token))))
        .collect();
    let runtime = Runtime::start(events, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(4).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 4);
    assert_audits(&result, 4);
    assert_eq!(invoke.count(), 2);
    let posts = runtime.records();
    assert_eq!(
        posts
            .iter()
            .map(|r| r.json()["statusCode"].clone())
            .collect::<Vec<_>>(),
        vec![json!(200), json!(401), json!(429), json!(200)]
    );
    let denied = posts[2].json();
    assert_eq!(
        denied["multiValueHeaders"]["ratelimit-limit"],
        json!(["1/MINUTES"])
    );
    assert_eq!(
        denied["multiValueHeaders"]["ratelimit-remaining"],
        json!(["0"])
    );
    let calls = invoke.records();
    assert_eq!(
        calls[0].json()["requestContext"]["authorizer"]["principalId"],
        "user-A"
    );
    assert_eq!(
        calls[1].json()["requestContext"]["authorizer"]["principalId"],
        "user-B"
    );
    result.save("WP12-limit-built-app", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn wp12_builtin_cors_preflight_denial_and_warm_isolation() {
    let keys = Keys::new("cors-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(dir.path(), "cors.enabled", json!(true));
    update_values(
        dir.path(),
        "cors.allowedOrigins",
        json!(["https://test.invalid"]),
    );
    update_values(dir.path(), "cors.allowedMethods", json!(["GET", "POST"]));
    update_values(
        dir.path(),
        "handler.defaultHandlers",
        json!(["requestCors", "proxy", "responseCors"]),
    );
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/x@get":"test-only-business","/x@options":"test-only-business"}),
    );
    let mut preflight = event("/x", "OPTIONS", None);
    preflight["multiValueHeaders"]["origin"] = json!(["https://test.invalid"]);
    preflight["multiValueHeaders"]["access-control-request-method"] = json!(["DELETE"]);
    let mut denied = event("/x", "GET", None);
    denied["multiValueHeaders"]["origin"] = json!(["https://denied.invalid"]);
    let runtime = Runtime::start(
        vec![
            Invocation::new("cors-preflight", preflight),
            Invocation::new("cors-denied", denied),
            Invocation::new("cors-ordinary", event("/x", "GET", None)),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(3).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 3);
    assert_audits(&result, 3);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    let preflight = posts[0].json();
    assert_eq!(preflight["statusCode"], 200);
    assert!(preflight["body"].is_null());
    assert_eq!(
        preflight["multiValueHeaders"]["access-control-allow-methods"],
        json!(["GET,POST"])
    );
    assert_eq!(
        preflight["multiValueHeaders"]["access-control-allow-origin"],
        json!(["https://test.invalid"])
    );
    let denial = posts[1].json();
    assert_eq!(denial["statusCode"], 403);
    assert!(
        denial["multiValueHeaders"]
            .get("access-control-allow-origin")
            .is_none()
    );
    let ordinary = posts[2].json();
    assert_eq!(ordinary["statusCode"], 200);
    assert_eq!(ordinary["body"], "ok");
    assert!(
        ordinary["multiValueHeaders"]
            .get("access-control-allow-origin")
            .is_none()
    );
    result.save("WP12-cors-built-app", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn wp12_builtin_basic_api_key_success_denial_and_zero_dispatch() {
    let keys = Keys::new("test-only-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "basic-auth.enabled", json!(true));
    update_values(dir.path(), "basic-auth.enableAD", json!(false));
    update_values(
        dir.path(),
        "basic-auth.users",
        json!([{"username":"test-user","password":"test-only-password","paths":["/b"]}]),
    );
    update_values(dir.path(), "apikey.enabled", json!(true));
    update_values(
        dir.path(),
        "apikey.pathPrefixAuths",
        json!([{"pathPrefix":"/a","headerName":"x-test-key","apiKey":"test-only-key"}]),
    );
    update_values(dir.path(), "unified-security.enabled", json!(true));
    update_values(
        dir.path(),
        "unified-security.pathPrefixAuths",
        json!([{"prefix":"/b","basic":true},{"prefix":"/a","apikey":true}]),
    );
    update_values(
        dir.path(),
        "handler.defaultHandlers",
        json!(["security", "proxy"]),
    );
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/*@get":"test-only-business"}),
    );
    let mut events = Vec::new();
    for (i, (path, auth, key)) in [
        ("/b", Some("Basic test-user:test-only-password"), None),
        ("/b", Some("Basic test-user:wrong"), None),
        ("/a", None, Some("test-only-key")),
        ("/a", None, Some("wrong")),
    ]
    .into_iter()
    .enumerate()
    {
        let mut value = event(path, "GET", None);
        if let Some(auth) = auth {
            value["multiValueHeaders"]["authorization"] = json!([auth]);
        }
        if let Some(key) = key {
            value["multiValueHeaders"]["x-test-key"] = json!([key]);
        }
        events.push(Invocation::new(&format!("wp12-factor-{i}"), value));
    }
    let runtime = Runtime::start(events, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(4).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 4);
    assert_audits(&result, 4);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let statuses: Vec<_> = runtime
        .records()
        .iter()
        .map(|r| r.json()["statusCode"].clone())
        .collect();
    assert_eq!(
        statuses,
        vec![json!(200), json!(401), json!(200), json!(401)]
    );
    let records = invoke.records();
    assert!(
        records[0]
            .json()
            .pointer("/requestContext/authorizer")
            .is_none()
    );
    assert!(
        records[1]
            .json()
            .pointer("/requestContext/authorizer")
            .is_none()
    );
    assert_eq!(
        records[1].json()["multiValueHeaders"]["x-test-key"],
        json!(["test-only-key"])
    );
    assert!(!result.stdout.contains("test-only-password"));
    assert!(!result.stderr.contains("test-only-password"));
    assert!(!result.stdout.contains("test-only-key"));
    assert!(!result.stderr.contains("test-only-key"));
    result.save("WP12-factors-built-app", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn l1_01_sdk_alb_route_multiplicity_cookies_bytes_and_presence() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke=RawServer::start(vec![Reply::json(json!({"statusCode":200,"body":null})),Reply::json(json!({"statusCode":200,"body":""})),Reply::json(json!({"statusCode":200,"body":"gAD/","isBase64Encoded":true,"headers":{"X-Overlap":"wrong"},"multiValueHeaders":{"x-overlap":["one","two"],"set-cookie":["a=1","b=2"]}}))]).await;
    let dir = config(&invoke, &jwks);
    use_jwt(dir.path());
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/x/{id}@get":"test-only-business"}),
    );
    let token = keys.token(claims("user-A"));
    let mut events = vec![];
    for (i, body) in [Value::Null, json!(""), json!("AAH/")]
        .into_iter()
        .enumerate()
    {
        let mut value = event("/x/a%20b", "GET", Some(&token));
        value["body"] = body;
        if i == 2 {
            value["isBase64Encoded"] = json!(true);
        }
        value["multiValueHeaders"]["x-repeat"] = json!(["first", "last"]);
        value["multiValueQueryStringParameters"] =
            json!({"q":["one%20two","three+four"],"id":["old"]});
        events.push(Invocation::new(&format!("l1-01-{i}"), value));
    }
    let runtime = Runtime::start(events, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(3).await;
    let result = process.collect(true).await;
    result.save("L1-01", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, 3);
    assert_audits(&result, 3);
    assert_eq!(invoke.count(), 3);
    let records = invoke.records();
    for r in &records {
        assert_eq!(r.method, "POST");
        assert!(
            r.path
                .starts_with("/2015-03-31/functions/test-only-business/invocations")
        );
        assert_eq!(r.headers["x-amz-invocation-type"], "RequestResponse");
        assert_eq!(r.headers["x-amz-log-type"], "None");
        let v = r.json();
        assert_eq!(v["path"], "/x/a b");
        assert_eq!(v["resource"], "/x/{id}");
        assert_eq!(v["queryStringParameters"]["id"], "a b");
        assert_eq!(
            v["multiValueQueryStringParameters"]["q"],
            json!(["one two", "three+four"])
        );
        assert_eq!(v["multiValueHeaders"]["x-repeat"], json!(["first", "last"]));
        assert_eq!(
            v.pointer("/requestContext/authorizer/principalId").unwrap(),
            "user-A"
        );
    }
    assert!(records[0].json()["body"].is_null());
    assert_eq!(records[1].json()["body"], "");
    assert_eq!(records[2].json()["body"], "AAH/");
    assert_eq!(records[2].json()["isBase64Encoded"], true);
    let posted = runtime.records();
    assert!(posted[0].json()["body"].is_null());
    assert_eq!(posted[1].json()["body"], "");
    assert_eq!(posted[2].json()["body"], "gAD/");
    assert_eq!(posted[2].json()["isBase64Encoded"], true);
    assert_eq!(
        posted[2].json()["multiValueHeaders"]["set-cookie"],
        json!(["a=1", "b=2"])
    );
    assert_eq!(
        posted[2].json()["multiValueHeaders"]["x-overlap"],
        json!(["one", "two"])
    );
}

async fn init_case(name: &str, unknown: bool, hang: bool, status: u16) {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let runtime = Runtime::start(vec![], false, hang, status).await;
    let mut command = command(dir.path(), Some(&runtime));
    if unknown {
        std::fs::write(
            dir.path().join("handler.yml"),
            json!({"handlers":["com.example.Unknown@bad"],"defaultHandlers":["bad"]}).to_string(),
        )
        .unwrap();
        update_values(dir.path(), "privateMarker", json!("SECRET-INIT-CONFIG"));
    } else {
        command.env("LIGHT_LAMBDA_CONFIG_DIR", dir.path().join("missing-layer"));
    }
    let result = Process::spawn(command).collect(false).await;
    result.save(name, Some(&runtime), &invoke, &jwks);
    assert_eq!(result.status.code(), Some(1));
    assert!(result.elapsed < Duration::from_millis(if hang { 3000 } else { 5000 }));
    assert_eq!(runtime.state.polls.load(Ordering::SeqCst), 0);
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].path, "/2018-06-01/runtime/init/error");
    assert_eq!(
        posts[0].headers["lambda-runtime-function-error-type"],
        "Runtime.InitError"
    );
    assert!(posts[0].body.len() <= 1024);
    let message = posts[0].json()["errorMessage"].as_str().unwrap().to_owned();
    if unknown {
        assert!(message.contains("handler.yml"));
        assert!(message.contains("com.example.Unknown"));
    } else {
        assert!(message.contains("missing-layer"));
    }
    assert!(!result.stdout.contains("SECRET-INIT-CONFIG"));
    assert!(!result.stderr.contains("SECRET-INIT-CONFIG"));
    assert!(!message.contains("SECRET-INIT-CONFIG"));
}
#[tokio::test]
async fn l1_02_missing_layer_reports_once_without_polling() {
    init_case("L1-02", false, false, 202).await;
}
#[tokio::test]
async fn l1_03_unknown_handler_reports_once_without_polling() {
    init_case("L1-03", true, false, 202).await;
}
#[tokio::test]
async fn l1_03b_hanging_init_transport_remains_bounded() {
    init_case("L1-03b", false, true, 202).await;
}
#[tokio::test]
async fn init_http_503_is_one_attempt_no_retry() {
    init_case("Init-HTTP503", false, false, 503).await;
}

#[tokio::test]
async fn init_unset_and_refused_runtime_transport_exit_without_polling() {
    for refused in [false, true] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        let mut cmd = command(dir.path(), None);
        cmd.env("LIGHT_LAMBDA_CONFIG_DIR", dir.path().join("missing-layer"));
        if refused {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            cmd.env("AWS_LAMBDA_RUNTIME_API", addr.to_string());
        }
        let result = Process::spawn(cmd).collect(false).await;
        result.save(
            if refused {
                "Init-refused"
            } else {
                "Init-unset"
            },
            None,
            &invoke,
            &jwks,
        );
        assert_eq!(result.status.code(), Some(1));
        assert!(result.elapsed < Duration::from_secs(3));
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
        assert!(result.stdout.contains(if refused {
            "TransportFailure"
        } else {
            "MissingEndpoint"
        }));
    }
}

async fn deadline_case(jwks_delay: bool) {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::slow(
        if jwks_delay { 700 } else { 0 },
        keys.jwks(),
    )])
    .await;
    let invoke = RawServer::start(vec![Reply::slow(
        500,
        json!({"statusCode":200,"body":"late"}),
    )])
    .await;
    let dir = config(&invoke, &jwks);
    use_jwt(dir.path());
    update_values(
        dir.path(),
        "light-lambda.responseReserveMs",
        json!(if jwks_delay { 200 } else { 30 }),
    );
    update_values(dir.path(), "light-lambda.postReserveMs", json!(10));
    if !jwks_delay {
        update_values(dir.path(), "security.bootstrapFromKeyService", json!(true));
    }
    let token = keys.token(claims("user-A"));
    let mut invocation = Invocation::new(
        if jwks_delay { "l1-04b" } else { "l1-04" },
        event("/x", "GET", Some(&token)),
    );
    invocation.budget_ms = if jwks_delay { 1200 } else { 100 };
    let runtime = Runtime::start(vec![invocation], false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    timeout(Duration::from_secs(1), async {
        loop {
            if invoke.records().first().is_some_and(|r| r.closed.is_some()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("SDK cancels the real Invoke connection");
    let result = process.collect(true).await;
    result.save(
        if jwks_delay { "L1-04b" } else { "L1-04" },
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_posts(&runtime, 1);
    assert_audits(&result, 1);
    assert_eq!(invoke.count(), 1);
    assert_eq!(jwks.count(), 1);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 504);
    assert!(
        posts[0].json()["body"]
            .as_str()
            .unwrap()
            .contains("ERR14017")
    );
    let delivered = runtime.state.delivered.lock().unwrap();
    let elapsed = posts[0].received.duration_since(delivered[0].1);
    assert!(elapsed < Duration::from_millis(if jwks_delay { 1200 } else { 250 }));
    let wire = &invoke.records()[0];
    let live = wire.closed.unwrap().duration_since(wire.received);
    if jwks_delay {
        assert!(
            wire.received.duration_since(jwks.records()[0].received) >= Duration::from_millis(650)
        );
        assert!(live < Duration::from_millis(400));
        assert!(elapsed >= Duration::from_millis(900));
    } else {
        assert!(live < Duration::from_millis(150));
    }
}
#[tokio::test]
async fn l1_04_slow_sdk_invoke_uses_actual_request_budget() {
    deadline_case(false).await;
}
#[tokio::test]
async fn l1_04b_real_jwks_and_invoke_share_one_budget() {
    deadline_case(true).await;
}

#[tokio::test]
async fn l1_05_real_jwt_denials_have_zero_dispatch() {
    for (case, code) in [
        ("missing", "ERR10002"),
        ("expired", "ERR10001"),
        ("audience", "ERR10000"),
        ("nbf", "ERR10000"),
    ] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        use_jwt(dir.path());
        let mut value = claims("denied-user");
        match case {
            "expired" => value["exp"] = json!(now_ms() / 1000 - 120),
            "audience" => value["aud"] = json!("wrong"),
            "nbf" => value["nbf"] = json!(now_ms() / 1000 + 120),
            _ => {}
        }
        let token = (case != "missing").then(|| keys.token(value));
        let runtime = Runtime::start(
            vec![Invocation::new(
                &format!("l1-05-{case}"),
                event("/x", "GET", token.as_deref()),
            )],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        result.save(&format!("L1-05-{case}"), Some(&runtime), &invoke, &jwks);
        assert_posts(&runtime, 1);
        assert_audits(&result, 1);
        assert_eq!(invoke.count(), 0);
        assert_eq!(runtime.records()[0].json()["statusCode"], 401);
        assert!(
            runtime.records()[0].json()["body"]
                .as_str()
                .unwrap()
                .contains(code)
        );
    }
}

#[tokio::test]
async fn l1_06_warm_users_anonymous_and_denied_requests_never_leak_state() {
    for unified in [false, true] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        if unified {
            update_values(
                dir.path(),
                "unified-security.anonymousPrefixes",
                json!(["/public"]),
            );
        } else {
            use_jwt(dir.path());
            update_values(dir.path(), "security.skipPathPrefixes", json!(["/public"]));
        }
        let a = keys.token(claims("user-A"));
        let b = keys.token(claims("user-B"));
        let mut events = vec![];
        for i in 0..20 {
            let (path, token) = match i % 4 {
                0 => ("/x", Some(a.as_str())),
                1 => ("/x", Some(b.as_str())),
                2 => ("/x", None),
                _ => ("/public", None),
            };
            events.push(Invocation::new(
                &format!("warm-{i}"),
                event(path, "GET", token),
            ));
        }
        let runtime = Runtime::start(events, false, false, 202).await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(20).await;
        let result = process.collect(true).await;
        result.save(
            if unified {
                "L1-06-unified"
            } else {
                "L1-06-jwt"
            },
            Some(&runtime),
            &invoke,
            &jwks,
        );
        assert_posts(&runtime, 20);
        assert_audits(&result, 20);
        assert_eq!(invoke.count(), 15);
        for (i, wire) in invoke.records().iter().enumerate() {
            let v = wire.json();
            match i % 3 {
                0 => assert_eq!(
                    v.pointer("/requestContext/authorizer/principalId").unwrap(),
                    "user-A"
                ),
                1 => assert_eq!(
                    v.pointer("/requestContext/authorizer/principalId").unwrap(),
                    "user-B"
                ),
                _ => assert!(v.pointer("/requestContext/authorizer").is_none()),
            };
        }
        for (i, wire) in runtime.records().iter().enumerate() {
            assert_eq!(
                wire.json()["statusCode"],
                if i % 4 == 2 { 401 } else { 200 }
            );
        }
        let audits = result.audits();
        assert_eq!(
            audits
                .iter()
                .filter(|v| v["fields"]["cold_start"] == true)
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn l1_08_http500_logs_and_resumes_polling_without_duplicate_dispatch_or_post() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let token = keys.token(claims("user-A"));
    let runtime = Runtime::start(
        vec![
            Invocation::new("first", event("/x", "GET", Some(&token))),
            Invocation::new("second", event("/x", "GET", Some(&token))),
        ],
        true,
        false,
        202,
    )
    .await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.arg("run");
    let process = Process::spawn(cmd);
    runtime.wait_posts(2).await;
    timeout(Duration::from_secs(1), async {
        loop {
            if runtime.state.polls.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let result = process.collect(true).await;
    result.save("L1-08", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, 2);
    assert_audits(&result, 2);
    assert_eq!(invoke.count(), 2);
    assert_eq!(
        runtime.records()[0].path,
        "/2018-06-01/runtime/invocation/first/response"
    );
    assert_eq!(
        runtime.records()[1].path,
        "/2018-06-01/runtime/invocation/second/response"
    );
    assert_eq!(
        invoke.records()[0].json()["requestContext"]["requestId"],
        "first"
    );
    assert_eq!(
        invoke.records()[1].json()["requestContext"]["requestId"],
        "second"
    );
    assert!(result.logs().iter().any(|v| {
        v["level"] == "ERROR"
            && v["target"]
                .as_str()
                .is_some_and(|t| t.starts_with("lambda_runtime"))
    }));
}

#[tokio::test]
async fn l1_09_validate_config_is_offline_for_valid_and_invalid_directories() {
    for invalid in [false, true] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        let runtime = Runtime::start(vec![], false, false, 202).await;
        update_values(dir.path(), "security.bootstrapFromKeyService", json!(true));
        update_values(
            dir.path(),
            "client.tokenCcClientSecret",
            json!("OFFLINE-SECRET-MARKER"),
        );
        if invalid {
            update_values(dir.path(), "light-lambda.stage", json!(""));
        }
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.args(["validate-config", "--config-dir"])
            .arg(dir.path())
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env("AWS_EC2_METADATA_DISABLED", "false")
            .env("AWS_EC2_METADATA_SERVICE_ENDPOINT", &invoke.url)
            .env("AWS_CONTAINER_CREDENTIALS_FULL_URI", &invoke.url)
            .env("HTTP_PROXY", &invoke.url)
            .env("HTTPS_PROXY", &invoke.url);
        let result = Process::spawn(cmd).collect(false).await;
        result.save(
            if invalid {
                "L1-09-invalid"
            } else {
                "L1-09-valid"
            },
            Some(&runtime),
            &invoke,
            &jwks,
        );
        assert_eq!(result.status.code(), Some(i32::from(invalid)));
        let report: Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(report["valid"], !invalid);
        assert_eq!(report.as_object().unwrap().len(), 6);
        assert!(!result.stdout.contains("OFFLINE-SECRET-MARKER"));
        assert!(!result.stderr.contains("OFFLINE-SECRET-MARKER"));
        assert_eq!(runtime.accepts.load(Ordering::SeqCst), 0);
        assert_eq!(invoke.accepts.load(Ordering::SeqCst), 0);
        assert_eq!(jwks.accepts.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.state.polls.load(Ordering::SeqCst), 0);
        if !invalid {
            assert_eq!(report["configDigest"].as_str().unwrap().len(), 64);
            assert!(!report["warnings"].as_array().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn l1_10_function_error_payload_never_leaks_at_trace() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke=RawServer::start(vec![Reply{function_error:true,..Reply::json(json!({"errorType":"BusinessFailure","errorMessage":"SECRET-MARKER-7f3a9c","stackTrace":["SECRET-MARKER-7f3a9c"]}))}]).await;
    let dir = config(&invoke, &jwks);
    let token = keys.token(claims("user-A"));
    let runtime = Runtime::start(
        vec![Invocation::new("trace", event("/x", "GET", Some(&token)))],
        false,
        false,
        202,
    )
    .await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.env("RUST_LOG", "trace");
    let process = Process::spawn(cmd);
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    result.save("L1-10", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, 1);
    assert_audits(&result, 1);
    assert_eq!(invoke.count(), 1);
    assert_eq!(runtime.records()[0].json()["statusCode"], 502);
    assert!(
        runtime.records()[0].json()["body"]
            .as_str()
            .unwrap()
            .contains("ERR14014")
    );
    assert!(!result.stdout.contains("SECRET-MARKER-7f3a9c"));
    assert!(!result.stderr.contains("SECRET-MARKER-7f3a9c"));
    assert!(
        !runtime.records()[0]
            .json()
            .to_string()
            .contains("SECRET-MARKER-7f3a9c")
    );
    assert!(result.logs().iter().any(|v| v["level"] == "TRACE"));
}

#[tokio::test]
async fn reserved_paths_all_methods_cannot_fall_through_to_catchall() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let methods = ["GET", "POST", "DELETE", "PATCH", "OPTIONS", "CUSTOM"];
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/*@get":"test-only-business","/*@post":"test-only-business","/*@delete":"test-only-business","/*@patch":"test-only-business","/*@options":"test-only-business","/*@custom":"test-only-business"}),
    );
    let token = keys.token(claims("user-A"));
    let mut events = vec![];
    let reserved = [
        "/health",
        "/adm/health",
        "/adm/server/info",
        "/adm/logger",
        "/adm/cache/item",
        "/%68ealth",
        "/adm/cache/a%20b",
    ];
    for path in reserved {
        for method in methods {
            events.push(Invocation::new(
                &format!("reserve-{}", events.len()),
                event(path, method, None),
            ));
        }
    }
    let denied = events.len();
    let unrelated = [
        "/healthz",
        "/health/",
        "/adm/cache",
        "/adm/cache/",
        "/adm/cache/a/b",
        "/adm/cache/a%2Fb",
        "/%2568ealth",
    ];
    for path in unrelated {
        events.push(Invocation::new(
            &format!("normal-{}", events.len()),
            event(path, "GET", Some(&token)),
        ));
    }
    let runtime = Runtime::start(events, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(denied + unrelated.len()).await;
    let result = process.collect(true).await;
    result.save("Phase1-reservations", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, denied + unrelated.len());
    assert_audits(&result, denied + unrelated.len());
    assert_eq!(invoke.count(), unrelated.len());
    for (i, wire) in runtime.records().iter().enumerate() {
        assert_eq!(
            wire.json()["statusCode"],
            if i < denied { 404 } else { 200 }
        );
        if i < denied {
            assert_eq!(wire.json()["statusDescription"], "404 Not Found");
            assert_eq!(wire.json()["isBase64Encoded"], false);
            assert!(wire.json()["body"].as_str().unwrap().contains("ERR10007"));
        }
    }
    for audit in result.audits().iter().take(denied) {
        assert!(audit["fields"].get("target").is_none_or(Value::is_null));
        assert_eq!(audit["fields"]["status"], 404);
    }
    assert!(
        invoke
            .records()
            .iter()
            .all(|r| !light_lambda::reserved_path(r.json()["path"].as_str().unwrap()).is_some())
    );
}

#[tokio::test]
async fn unimplemented_policies_and_missing_terminals_still_fail_init() {
    for case in [
        "enabled-policy",
        "handler-disabled",
        "missing-terminal",
        "unsupported-factor",
        "invalid-reserve",
    ] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        match case {
            // Metrics/Audit are qualified; Validator remains unavailable here.
            "enabled-policy" => update_values(dir.path(), "openapi-validator.enabled", json!(true)),
            "handler-disabled" => update_values(dir.path(), "handler.enabled", json!(false)),
            "missing-terminal" => {
                std::fs::write(dir.path().join("handler.yml"),json!({"handlers":["com.networknt.aws.lambda.handler.health.HealthCheckHandler@health"],"paths":[{"path":"/health","method":"get","exec":["health"]}]}).to_string()).unwrap();
            }
            "unsupported-factor" => update_values(
                dir.path(),
                "unified-security.pathPrefixAuths",
                json!([{"prefix":"/","basic":true}]),
            ),
            _ => update_values(dir.path(), "light-lambda.postReserveMs", json!(250)),
        }
        let runtime = Runtime::start(vec![], false, false, 202).await;
        let result = Process::spawn(command(dir.path(), Some(&runtime)))
            .collect(false)
            .await;
        result.save(&format!("Init-{case}"), Some(&runtime), &invoke, &jwks);
        assert_eq!(result.status.code(), Some(1));
        assert_eq!(runtime.records().len(), 1);
        assert_eq!(runtime.state.polls.load(Ordering::SeqCst), 0);
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
    }
}

#[tokio::test]
async fn embedded_cli_and_malformed_arguments_never_contact_runtime() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let runtime = Runtime::start(vec![], false, false, 202).await;
    for (i, args) in [
        vec!["unknown"],
        vec!["run", "extra"],
        vec!["validate-config", "--config-dir"],
        vec!["--dump-embedded-configs"],
        vec!["--print-embedded-config"],
    ]
    .into_iter()
    .enumerate()
    {
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.args(args);
        let result = Process::spawn(cmd).collect(false).await;
        result.save(&format!("CLI-{i}"), Some(&runtime), &invoke, &jwks);
        assert_eq!(result.status.code(), Some(if i == 4 { 0 } else { 1 }));
    }
    let dumped = dir.path().join("dump");
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.arg("--dump-embedded-configs").arg(&dumped);
    let result = Process::spawn(cmd).collect(false).await;
    result.save("CLI-dump", Some(&runtime), &invoke, &jwks);
    assert_eq!(result.status.code(), Some(0));
    for file in light_lambda::embedded_config::FILES {
        assert_eq!(
            std::fs::read_to_string(dumped.join(file.name)).unwrap(),
            file.content
        );
    }
    assert_eq!(runtime.accepts.load(Ordering::SeqCst), 0);
    assert_eq!(invoke.accepts.load(Ordering::SeqCst), 0);
    assert_eq!(jwks.accepts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn real_handler_integrates_wp6_refresh_coordination_in_a_warm_process() {
    let initial = Keys::new("initial");
    let rotated = Keys::new("rotated");
    let jwks = RawServer::start(vec![
        Reply::json(initial.jwks()),
        Reply::json(rotated.jwks()),
    ])
    .await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    use_jwt(dir.path());
    update_values(dir.path(), "security.bootstrapFromKeyService", json!(true));
    let a = initial.token(claims("user-A"));
    let b = rotated.token(claims("user-B"));
    let unknown = Keys::new("unknown").token(claims("denied"));
    update_values(dir.path(), "security.jwksRefreshMinIntervalSecs", json!(1));
    let mut rotation = Invocation::new("rotated", event("/x", "GET", Some(&b)));
    rotation.pause_ms = 1100;
    let runtime = Runtime::start(
        vec![
            Invocation::new("known", event("/x", "GET", Some(&a))),
            rotation,
            Invocation::new("unknown1", event("/x", "GET", Some(&unknown))),
            Invocation::new("unknown2", event("/x", "GET", Some(&unknown))),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(4).await;
    let result = process.collect(true).await;
    result.save("WP6-real-handler-refresh", Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, 4);
    assert_audits(&result, 4);
    assert_eq!(jwks.count(), 2);
    assert_eq!(invoke.count(), 2);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 200);
    assert_eq!(posts[1].json()["statusCode"], 200);
    assert_eq!(posts[2].json()["statusCode"], 401);
    assert_eq!(posts[3].json()["statusCode"], 401);
}

#[tokio::test]
async fn sdk_response_connection_loss_propagates_without_retry_or_redispatch() {
    let keys = Keys::new("initial");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    let token = keys.token(claims("user-A"));
    let mut next = Reply::json(event("/x", "GET", Some(&token)));
    for (key, value) in [
        ("lambda-runtime-aws-request-id", "transport-loss".into()),
        ("lambda-runtime-deadline-ms", (now_ms() + 10000).to_string()),
        ("lambda-runtime-invoked-function-arn", ARN.into()),
        (
            "lambda-runtime-trace-id",
            "Root=1-00000000-000000000000000000000001;Parent=0000000000000001;Sampled=0".into(),
        ),
    ] {
        next.headers
            .insert(http::HeaderName::from_static(key), value.parse().unwrap());
    }
    let raw_runtime = RawServer::start(vec![
        next,
        Reply {
            disconnect: true,
            ..Reply::json(json!({}))
        },
    ])
    .await;
    let mut cmd = command(dir.path(), None);
    cmd.env(
        "AWS_LAMBDA_RUNTIME_API",
        raw_runtime.url.strip_prefix("http://").unwrap(),
    );
    let result = Process::spawn(cmd).collect(false).await;
    result.save("Runtime-response-connection-loss", None, &invoke, &jwks);
    if let Some(root) = std::env::var_os("WP10_L1_EVIDENCE_DIR") {
        std::fs::write(
            std::path::PathBuf::from(root)
                .join("Runtime-response-connection-loss/runtime-records.json"),
            serde_json::to_vec_pretty(
                &raw_runtime
                    .records()
                    .iter()
                    .map(Wire::sanitized)
                    .collect::<Vec<_>>(),
            )
            .unwrap(),
        )
        .unwrap();
    }
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(raw_runtime.count(), 2);
    assert_eq!(invoke.count(), 1);
    assert_audits(&result, 1);
    assert_eq!(
        raw_runtime.records()[0].path,
        "/2018-06-01/runtime/invocation/next"
    );
    assert_eq!(
        raw_runtime.records()[1].path,
        "/2018-06-01/runtime/invocation/transport-loss/response"
    );
    assert!(result.stdout.contains("runtime transport terminated"));
}

#[tokio::test]
async fn offline_local_artifact_syntax_and_handler_failures_never_use_network() {
    for case in ["artifact", "syntax", "handler", "missing-directory"] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        let runtime = Runtime::start(vec![], false, false, 202).await;
        match case {
            "artifact" => update_values(
                dir.path(),
                "client.caCertPath",
                json!(dir.path().join("SECRET-PATH-MISSING.pem")),
            ),
            "syntax" => {
                std::fs::write(dir.path().join("handler.yml"), "[invalid yaml:").unwrap();
            }
            "handler" => {
                std::fs::write(
                    dir.path().join("handler.yml"),
                    json!({"handlers":["com.example.Unknown@bad"],"defaultHandlers":["bad"]})
                        .to_string(),
                )
                .unwrap();
            }
            _ => {}
        }
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.args(["validate-config", "--config-dir"])
            .arg(if case == "missing-directory" {
                dir.path().join("missing")
            } else {
                dir.path().into()
            });
        cmd.env("RUST_LOG", "trace")
            .env_remove("AWS_ACCESS_KEY_ID")
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env("AWS_CONTAINER_CREDENTIALS_FULL_URI", &invoke.url);
        let result = Process::spawn(cmd).collect(false).await;
        result.save(&format!("Offline-{case}"), Some(&runtime), &invoke, &jwks);
        assert_eq!(result.status.code(), Some(1));
        let report: Value = serde_json::from_str(&result.stdout).unwrap();
        assert_eq!(report["valid"], false);
        assert!(!result.stdout.contains("SECRET-PATH-MISSING"));
        assert!(!result.stderr.contains("SECRET-PATH-MISSING"));
        assert_eq!(runtime.accepts.load(Ordering::SeqCst), 0);
        assert_eq!(invoke.accepts.load(Ordering::SeqCst), 0);
        assert_eq!(jwks.accepts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cfg04_handler_duration_absent_false_warn_once_true_fails_init() {
    for mode in ["absent", "default-false", "nondefault-true"] {
        let keys = Keys::new("initial");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        use_jwt(dir.path());
        let file = dir.path().join("handler.yml");
        let mut handler: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        if mode != "absent" {
            handler["reportHandlerDuration"] = json!(mode == "nondefault-true");
        }
        std::fs::write(file, handler.to_string()).unwrap();
        let runtime = Runtime::start(
            vec![Invocation::new("duration", event("/x", "GET", None))],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        let result = if mode == "nondefault-true" {
            process.collect(false).await
        } else {
            runtime.wait_posts(1).await;
            process.collect(true).await
        };
        result.save(&format!("CFG04-{mode}"), Some(&runtime), &invoke, &jwks);
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
        if mode == "nondefault-true" {
            assert_eq!(result.status.code(), Some(1));
            assert_eq!(runtime.state.polls.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.records()[0].path, "/2018-06-01/runtime/init/error");
        } else {
            assert_posts(&runtime, 1);
            let warnings = result
                .logs()
                .into_iter()
                .filter(|v| v["fields"]["configuration_key"] == "handler.reportHandlerDuration")
                .collect::<Vec<_>>();
            assert_eq!(warnings.len(), 1);
            assert_eq!(warnings[0]["fields"]["mode"], mode);
            assert_eq!(warnings[0]["fields"]["ignored"], true);
        }
    }
}

#[tokio::test]
async fn approved_amendment_supplement_valid_limiter_input_consumes_warm_quota() {
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    for key in ["security.enabled", "unified-security.enabled"] {
        update_values(dir.path(), key, json!(false));
    }
    update_values(dir.path(), "limit.enabled", json!(true));
    update_values(dir.path(), "limit.key", json!("address"));
    update_values(dir.path(), "limit.rateLimit", json!("1/h"));
    update_values(dir.path(), "limit.address", json!({"198.51.100.7":"1/h"}));
    let classes = serverless_core::compat::COMPATIBILITY;
    let names = ["limit", "proxy"].map(|alias| {
        format!(
            "{}@{alias}",
            classes.iter().find(|c| c.alias == alias).unwrap().class
        )
    });
    std::fs::write(
        dir.path().join("handler.yml"),
        json!({"handlers":names,"defaultHandlers":["limit","proxy"]}).to_string(),
    )
    .unwrap();
    let raw = event("/x", "GET", None);
    assert!(raw.pointer("/requestContext/elb/targetGroupArn").is_some());
    let runtime = Runtime::start(
        vec![
            Invocation::new("limit-accepted", raw.clone()),
            Invocation::new("limit-denied", raw),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    assert_eq!(runtime.records()[0].json()["statusCode"], 200);
    assert_eq!(runtime.records()[1].json()["statusCode"], 429);
    assert_eq!(invoke.count(), 1);
    result.save("WP13-valid-limiter", Some(&runtime), &invoke, &jwks);
}

mod wp13_approved;
