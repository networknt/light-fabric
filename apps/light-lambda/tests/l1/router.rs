use super::support::*;
use serde_json::{Value, json};

fn layer(invoke: &RawServer, jwks: &RawServer, http: &RawServer) -> tempfile::TempDir {
    let dir = config(invoke, jwks);
    for key in ["security.enabled", "unified-security.enabled"] {
        update_values(dir.path(), key, json!(false));
    }
    update_values(dir.path(), "router.enabled", json!(true));
    update_values(dir.path(), "router.httpsEnabled", json!(false));
    update_values(dir.path(), "router.urlRewriteRules", json!([]));
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        json!({"directUrls":{"http-service":format!("{}/base",http.url)}}).to_string(),
    )
    .unwrap();
    chain(dir.path(), &["router", "router", "proxy"]);
    dir
}
fn chain(dir: &std::path::Path, aliases: &[&str]) {
    let classes = serverless_core::compat::COMPATIBILITY;
    let declarations = ["router", "proxy", "basic", "requestHeader"]
        .into_iter()
        .map(|alias| {
            format!(
                "{}@{alias}",
                classes.iter().find(|c| c.alias == alias).unwrap().class
            )
        })
        .collect::<Vec<_>>();
    std::fs::write(
        dir.join("handler.yml"),
        json!({"enabled":true,"handlers":declarations,"defaultHandlers":aliases}).to_string(),
    )
    .unwrap();
}
fn routed(path: &str, method: &str, service: &str) -> Value {
    let mut e = event(path, method, None);
    e["multiValueHeaders"]["SeRvIcE_Id"] = json!([service]);
    e
}

#[tokio::test]
async fn application_router_forwarding_fallback_errors_and_warm_snapshot() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let http = RawServer::start(vec![Reply::json(json!({"from":"http"}))]).await;
    let dir = layer(&invoke, &jwks, &http);
    let mut post = routed("/two", "POST", "http-service");
    post["body"] = json!("business-body");
    let mut get = routed("/one", "GET", "http-service");
    get["body"] = json!("must-not-forward");
    let runtime = Runtime::start(
        vec![
            Invocation::new("http-get", get),
            Invocation {
                pause_ms: 250,
                ..Invocation::new("http-post", post)
            },
            Invocation::new("lambda-fallback", event("/fallback", "GET", None)),
            Invocation::new("unknown-service", routed("/one", "GET", "unknown")),
            Invocation::new("bad-method", routed("/one", "HEAD", "http-service")),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    std::fs::write(dir.path().join("direct-registry.yml"), "malformed: [").unwrap();
    runtime.wait_posts(5).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 5);
    let posts = runtime.records();
    assert_eq!(
        posts
            .iter()
            .map(|p| p.json()["statusCode"].clone())
            .collect::<Vec<_>>(),
        json!([200, 200, 200, 400, 400]).as_array().unwrap().clone()
    );
    assert_eq!(posts[0].json()["body"], json!({"from":"http"}).to_string());
    assert_eq!(
        http.count(),
        2,
        "repeated Router selects once and dispatches once"
    );
    assert_eq!(
        invoke.count(),
        1,
        "no HTTP request is also Lambda dispatched"
    );
    let calls = http.records();
    assert_eq!(calls[0].path, "/base/one");
    assert!(calls[0].body.is_empty());
    assert_eq!(calls[1].path, "/base/two");
    assert_eq!(calls[1].body, b"business-body");
    assert!(calls.iter().all(|c| !c.headers.contains_key("service_id")));
    assert_eq!(jwks.count(), 0);
    result.save(
        "router-application-forwarding",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn application_router_late_rejection_prevents_all_dispatch() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let http = RawServer::start(vec![Reply::json(json!({}))]).await;
    let dir = layer(&invoke, &jwks, &http);
    update_values(dir.path(), "basic-auth.enabled", json!(true));
    chain(dir.path(), &["router", "basic", "proxy"]);
    let runtime = Runtime::start(
        vec![Invocation::new(
            "denied-after-router",
            routed("/x", "GET", "http-service"),
        )],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    assert_eq!(runtime.records()[0].json()["statusCode"], 401);
    assert_eq!(http.count(), 0);
    assert_eq!(invoke.count(), 0);
    result.save(
        "router-application-late-rejection",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn application_router_transport_error_and_redirect_are_http_results() {
    for (name, reply, status) in [
        (
            "transport",
            Reply {
                disconnect: true,
                ..Reply::json(json!({}))
            },
            400,
        ),
        (
            "redirect",
            Reply {
                status: 302,
                headers: [(
                    http::header::LOCATION,
                    "http://127.0.0.1:1/untrusted".parse().unwrap(),
                )]
                .into_iter()
                .collect(),
                ..Reply::json(json!({}))
            },
            302,
        ),
    ] {
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
        let http = RawServer::start(vec![reply]).await;
        let dir = layer(&invoke, &jwks, &http);
        let runtime = Runtime::start(
            vec![Invocation::new(name, routed("/x", "GET", "http-service"))],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        assert_eq!(runtime.records()[0].json()["statusCode"], status);
        assert_eq!(http.count(), 1);
        assert_eq!(invoke.count(), 0);
        result.save(
            &format!("router-application-{name}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}

#[tokio::test]
async fn application_router_rewrite_profile_fails_init_without_a_worker() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let http = RawServer::start(vec![Reply::json(json!({}))]).await;
    let dir = layer(&invoke, &jwks, &http);
    update_values(
        dir.path(),
        "router.urlRewriteRules",
        json!(["/x /rewritten"]),
    );
    let runtime = Runtime::start(vec![], false, false, 202).await;
    let result = Process::spawn(command(dir.path(), Some(&runtime)))
        .collect(false)
        .await;
    assert_eq!(result.status.code(), Some(1));
    let posts = runtime.records();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].path.ends_with("/init/error"));
    assert!(
        posts[0].json()["errorMessage"]
            .as_str()
            .unwrap()
            .contains("Java rewrite execution is unavailable")
    );
    assert_eq!(http.count(), 0);
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    result.save(
        "router-application-unavailable-rewrite",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn approved_amendment_supplement_valid_router_input_reaches_http_transport() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let http = RawServer::start(vec![
        Reply::json(json!({"transport":"ok"})),
        Reply {
            disconnect: true,
            ..Reply::ok()
        },
    ])
    .await;
    let dir = layer(&invoke, &jwks, &http);
    let first = routed("/valid", "POST", "http-service");
    assert!(
        first
            .pointer("/requestContext/elb/targetGroupArn")
            .is_some()
    );
    let runtime = Runtime::start(
        vec![
            Invocation::new("valid-router", first),
            Invocation::new(
                "broken-transport",
                routed("/broken", "POST", "http-service"),
            ),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    assert_eq!(http.count(), 2);
    assert_eq!(invoke.count(), 0);
    assert_eq!(runtime.records()[0].json()["statusCode"], 200);
    assert_eq!(runtime.records()[1].json()["statusCode"], 400);
    assert_eq!(http.records()[0].path, "/base/valid");
    result.save(
        "WP13-valid-router-transport",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}
