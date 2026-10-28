use super::*;

fn activate(dir: &std::path::Path, provider: &RawServer) {
    for (key, value) in [
        ("security.enabled", json!(false)),
        ("unified-security.enabled", json!(false)),
        ("token.enabled", json!(true)),
        ("token.appliedPathPrefixes", json!(["/x"])),
        ("client.tokenServerUrl", json!(provider.url)),
        ("client.tokenCcUri", json!("/token")),
        ("client.tokenCcClientId", json!("test-only-client")),
        ("client.tokenCcClientSecret", json!("TOKEN-SECRET-NO-LOG")),
        ("client.tokenEnableHttp2", json!(false)),
        ("client.tokenRenewBeforeExpired", json!(0)),
        ("client.expiredRefreshRetryDelay", json!(0)),
        ("client.maxRequestRetry", json!(0)),
        (
            "cache.caches",
            json!([{"cacheName":"token","maxSize":100,"expiryInMinutes":15}]),
        ),
    ] {
        update_values(dir, key, value);
    }
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{}@token",class(serverless_core::HandlerId::Token)),format!("{}@proxy",class(serverless_core::HandlerId::Proxy))],"defaultHandlers":["token","proxy"]}).to_string()).unwrap();
}
fn request(name: &str, secondary: bool) -> Invocation {
    let mut raw = event("/x", "GET", secondary.then_some("test-only-subject"));
    raw["multiValueHeaders"]["service_id"] = json!(["fixed-service"]);
    Invocation::new(name, raw)
}
#[tokio::test]
async fn token_builtin_provider_cache_and_secondary_headers() {
    let keys = Keys::new("token-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let provider = RawServer::start(vec![Reply::json(
        json!({"access_token":"TOKEN-VALUE-NO-LOG","expires_in":3600}),
    )])
    .await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(dir.path(), &provider);
    let runtime = Runtime::start(
        vec![request("token-first", false), request("token-cached", true)],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    assert!(
        runtime.records()[0].path.ends_with("/response"),
        "{}",
        runtime.records()[0].json()
    );
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 2);
    assert_audits(&result, 2);
    assert_eq!(provider.count(), 1);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let calls = invoke.records();
    assert_eq!(
        calls[0].json()["multiValueHeaders"]["authorization"],
        json!(["Bearer TOKEN-VALUE-NO-LOG"])
    );
    assert_eq!(
        calls[1].json()["multiValueHeaders"]["authorization"],
        json!(["Bearer test-only-subject"])
    );
    assert_eq!(
        calls[1].json()["multiValueHeaders"]["x-scope-token"],
        json!(["Bearer TOKEN-VALUE-NO-LOG"])
    );
    let records = provider.records();
    assert_eq!(records[0].path, "/token");
    assert_eq!(records[0].method, "POST");
    assert_eq!(records[0].body, b"grant_type=client_credentials");
    for secret in [
        "TOKEN-SECRET-NO-LOG",
        "TOKEN-VALUE-NO-LOG",
        "test-only-subject",
    ] {
        assert!(!result.stdout.contains(secret));
        assert!(!result.stderr.contains(secret));
    }
    result.save("WP12-token-built-app-cache", Some(&runtime), &invoke, &jwks);
}
#[tokio::test]
async fn token_builtin_denial_malformed_redirect_and_timeout_never_dispatch() {
    for (name, status, body, delay, expected, code) in [
        (
            "denial",
            401,
            json!({"statusCode":401,"code":"ERR10000","message":"INVALID_AUTH_TOKEN","severity":"ERROR","description":"TOKEN-SECRET-NO-LOG"}),
            0,
            401,
            "ERR10000",
        ),
        (
            "malformed",
            401,
            json!({"error":"invalid_client","error_description":"TOKEN-SECRET-NO-LOG"}),
            0,
            500,
            "ERR14004",
        ),
        (
            "redirect",
            302,
            json!({"token":"TOKEN-SECRET-NO-LOG"}),
            0,
            500,
            "ERR14004",
        ),
        (
            "timeout",
            200,
            json!({"access_token":"TOKEN-SECRET-NO-LOG","expires_in":3600}),
            400,
            504,
            "ERR14017",
        ),
    ] {
        let keys = Keys::new("token-unused");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let destination = RawServer::start(vec![Reply::json(
            json!({"access_token":"unexpected","expires_in":3600}),
        )])
        .await;
        let mut reply = Reply {
            status,
            delay: Duration::from_millis(delay),
            ..Reply::json(body)
        };
        reply.headers.insert(
            "location",
            format!("{}/token", destination.url).parse().unwrap(),
        );
        let provider = RawServer::start(vec![reply]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        activate(dir.path(), &provider);
        let mut invocation = request(&format!("token-{name}"), false);
        if delay > 0 {
            update_values(dir.path(), "light-lambda.responseReserveMs", json!(50));
            update_values(dir.path(), "light-lambda.postReserveMs", json!(25));
            invocation.budget_ms = 150;
        }
        let runtime = Runtime::start(vec![invocation], false, false, 202).await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        assert!(
            runtime.records()[0].path.ends_with("/response"),
            "{}",
            runtime.records()[0].json()
        );
        assert_posts(&runtime, 1);
        assert_audits(&result, 1);
        assert_eq!(provider.count(), 1);
        assert_eq!(destination.count(), 0);
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
        let response = runtime.records()[0].json();
        assert_eq!(response["statusCode"], expected);
        assert!(response["body"].as_str().unwrap().contains(code));
        assert!(response["multiValueHeaders"]["authorization"].is_null());
        assert!(response["multiValueHeaders"]["x-scope-token"].is_null());
        assert!(!result.stdout.contains("TOKEN-SECRET-NO-LOG"));
        assert!(!result.stderr.contains("TOKEN-SECRET-NO-LOG"));
        result.save(
            &format!("WP12-token-built-app-{name}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}
