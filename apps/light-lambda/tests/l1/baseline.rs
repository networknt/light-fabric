use super::support::*;
use serde_json::json;

#[tokio::test]
async fn gateway_errors_preserve_route_identity_and_plain_ingress_json() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(dir.path(), "lambda-app.encodeBase64Response", json!(true));
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/x@get":"test-only-business"}),
    );
    let mut ingress = event("/x", "GET", None);
    ingress.as_object_mut().unwrap().remove("multiValueHeaders");
    ingress["headers"] = json!({});
    let runtime = Runtime::start(
        vec![
            Invocation::new("ingress", ingress),
            Invocation::new("route-miss", event("/unknown", "GET", None)),
            Invocation::new("success", event("/x", "GET", None)),
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
    let posts = runtime.records();
    let ingress = posts[0].json();
    assert_eq!(ingress["isBase64Encoded"], false);
    assert_eq!(ingress["statusCode"], 500);
    assert!(serde_json::from_str::<serde_json::Value>(ingress["body"].as_str().unwrap()).is_ok());
    // Business responses still honor configured forced encoding.
    assert_eq!(posts[2].json()["isBase64Encoded"], true);
    assert_eq!(posts[2].json()["body"], "b2s=");
    use base64::Engine;
    let route = posts[1].json();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(route["body"].as_str().unwrap())
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(body["code"], "ERR10086");
    assert_eq!(
        body["description"],
        "Failed to invoke lambda function /unknown@get."
    );
    assert_eq!(invoke.count(), 1);
    assert_eq!(jwks.count(), 0);
    result.save("WP13-gateway-error-repairs", Some(&runtime), &invoke, &jwks);
}

/// Runnable baseline: real bootstrap/Init/SDK Invoke, no JWT or worker dependency.
#[tokio::test]
async fn local_baseline_success_and_business_error() {
    let invoke = RawServer::start(vec![
        Reply::json(json!({"statusCode":200,"body":"hello from business","headers":{"x-business":"yes"},"isBase64Encoded":false})),
        Reply { function_error: true, ..Reply::json(json!({"errorMessage":"synthetic business failure"})) },
    ]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = tempfile::tempdir().unwrap();
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/local-baseline");
    for name in ["handler.yml", "values.yml"] {
        std::fs::copy(source.join(name), dir.path().join(name)).unwrap();
    }
    // The test-owned endpoint is the only runtime override to the checked-in layer.
    std::fs::write(dir.path().join("lambda-proxy.yml"),json!({"enabled":true,"region":"us-east-1","endpointOverride":invoke.url,"functions":{"/hello@get":"test-only-business"},"metricsInjection":false}).to_string()).unwrap();
    let runtime = Runtime::start(
        vec![
            Invocation::new("baseline-ok", event("/hello", "GET", None)),
            Invocation::new("baseline-error", event("/hello", "GET", None)),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 2);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 200);
    assert_eq!(posts[0].json()["body"], "hello from business");
    assert_eq!(
        posts[0].json()["multiValueHeaders"]["x-business"],
        json!(["yes"])
    );
    assert_eq!(posts[1].json()["statusCode"], 502);
    assert!(
        posts[1].json()["body"]
            .as_str()
            .unwrap()
            .contains("ERR14014")
    );
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    for call in invoke.records() {
        assert!(call.path.contains("test-only-business/invocations"));
        assert_eq!(call.json()["path"], "/hello");
        assert_eq!(call.json()["requestContext"]["stage"], "local-baseline");
    }
    result.save("milestone-1-baseline", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn local_baseline_enabled_unimplemented_handlers_fail_init() {
    for (key, file) in [("openapi-validator.enabled", "openapi-validator.yml")] {
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
        let dir = config(&invoke, &jwks);
        update_values(dir.path(), "security.enabled", json!(false));
        update_values(dir.path(), "unified-security.enabled", json!(false));
        // Explicit files avoid alias ambiguity in configuration placeholders.
        std::fs::write(dir.path().join(file), json!({"enabled":true}).to_string()).unwrap();
        let runtime = Runtime::start(vec![], false, false, 202).await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        let result = process.collect(false).await;
        assert_eq!(result.status.code(), Some(1), "{key}");
        assert!(!result.stopped_by_harness);
        let posts = runtime.records();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].path.ends_with("/init/error"));
        assert_eq!(posts[0].json()["errorType"], "InvalidConfiguration");
        assert!(
            posts[0].json()["errorMessage"]
                .as_str()
                .unwrap()
                .contains("enabled handler not supported")
        );
        assert_eq!(
            runtime
                .state
                .polls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(invoke.count(), 0);
        assert_eq!(jwks.count(), 0);
        result.save(
            &format!("milestone-1-unavailable-{file}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}
