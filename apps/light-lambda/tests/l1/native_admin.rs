//! Existing admitted factories and deferred-profile admission only.
//! This does not implement or qualify LoggerSet or CacheExplorer execution.
use super::support::*;
use light_lambda_runtime::init;
use serde_json::json;
use serverless_core::HandlerId;

fn class(id: HandlerId) -> &'static str {
    serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|entry| entry.id == id)
        .unwrap()
        .class
}

#[tokio::test]
async fn selected_logger_set_rejects_real_init_even_when_get_is_disabled() {
    for enabled in [false, true] {
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let jwks = RawServer::start(vec![Reply::json(json!({"keys": []}))]).await;
        let dir = config(&invoke, &jwks);
        update_values(dir.path(), "security.enabled", json!(false));
        update_values(dir.path(), "unified-security.enabled", json!(false));
        update_values(dir.path(), "logger.enabled", json!(enabled));
        std::fs::write(dir.path().join("handler.yml"), json!({
            "handlers": [format!("{}@setLogger", class(HandlerId::SetLogger)), format!("{}@proxy", class(HandlerId::Proxy))],
            "chains": {"adminSetter": ["setLogger"]},
            "paths": [{"path": "/adm/logger", "method": "post", "exec": ["adminSetter"]}],
            "defaultHandlers": ["proxy"]
        }).to_string()).unwrap();
        let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
        let report = init::validate_configuration(&input).await;
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("logger SET unsupported")),
            "{:?}",
            report.errors
        );
        assert!(init::initialize(input).await.is_err());
        let runtime = Runtime::start(vec![], false, false, 202).await;
        let result = Process::spawn(command(dir.path(), Some(&runtime)))
            .collect(false)
            .await;
        assert_eq!(result.status.code(), Some(1));
        assert!(!result.stopped_by_harness);
        let posts = runtime.records();
        assert_eq!(posts.len(), 1);
        assert!(posts[0].path.ends_with("/init/error"));
        assert_eq!(posts[0].json()["errorType"], "InvalidConfiguration");
        assert!(
            posts[0].json()["errorMessage"]
                .as_str()
                .unwrap()
                .contains("logger SET unsupported")
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
            &format!("native-admin-set-selection-get-enabled-{enabled}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
}

#[tokio::test]
async fn selected_cache_explorer_rejects_real_init_without_exposing_cache_data() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys": []}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    std::fs::write(
        dir.path().join("cache.yml"),
        json!({"enabled": true, "caches": []}).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("handler.yml"), json!({
        "handlers": [format!("{}@cache", class(HandlerId::Cache)), format!("{}@proxy", class(HandlerId::Proxy))],
        "paths": [{"path": "/adm/cache/{name}", "method": "get", "exec": ["cache"]}],
        "defaultHandlers": ["proxy"]
    }).to_string()).unwrap();
    let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    let report = init::validate_configuration(&input).await;
    assert!(!report.valid);
    assert!(
        report.errors.iter().any(|e| e.contains(
            "token.yml/client.yml/cache.yml: unsupported, invalid or insecure token configuration"
        )),
        "{:?}",
        report.errors
    );
    assert!(init::initialize(input).await.is_err());
    let runtime = Runtime::start(vec![], false, false, 202).await;
    let result = Process::spawn(command(dir.path(), Some(&runtime)))
        .collect(false)
        .await;
    assert_eq!(result.status.code(), Some(1));
    assert!(!result.stopped_by_harness);
    let posts = runtime.records();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].path.ends_with("/init/error"));
    assert!(posts[0].json()["errorMessage"].as_str().unwrap().contains(
        "token.yml/client.yml/cache.yml: unsupported, invalid or insecure token configuration"
    ));
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
        "native-admin-cache-selected",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn deferred_mutation_attempt_does_not_change_real_getter_or_warm_identity() {
    let keys = Keys::new("native-admin-warm");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "logger.enabled", json!(true));
    update_values(dir.path(), "health.enabled", json!(true));
    update_values(dir.path(), "info.enableServerInfo", json!(true));
    // Isolated already-admitted profile, not a rewrite of the native sample.
    // A dormant declaration is permitted; an executable setter binding is not.
    std::fs::write(dir.path().join("handler.yml"), json!({
        "handlers": [format!("{}@getLogger", class(HandlerId::GetLogger)), format!("{}@setLogger", class(HandlerId::SetLogger)),
            format!("{}@health", class(HandlerId::Health)), format!("{}@info", class(HandlerId::Info)),
            format!("{}@jwt", class(HandlerId::Jwt)), format!("{}@proxy", class(HandlerId::Proxy))],
        "paths": [{"path": "/adm/logger", "method": "get", "exec": ["jwt", "getLogger"]},
                  {"path": "/health", "method": "get", "exec": ["health"]},
                  {"path": "/adm/server/info", "method": "get", "exec": ["jwt", "info"]}],
        "defaultHandlers": ["proxy"]
    }).to_string()).unwrap();
    let token = keys.token(claims("test-admin"));
    let mut mutation = event("/adm/logger", "POST", Some(&token));
    mutation["body"] = json!(r#"[{"name":"ROOT","level":"DEBUG"}]"#);
    let events = vec![
        event("/health", "GET", None),
        event("/adm/logger", "GET", None),
        event("/adm/logger", "GET", Some(&token)),
        mutation,
        event("/adm/logger", "GET", Some(&token)),
        event("/adm/cache/jwk", "GET", Some(&token)),
        event("/adm/server/info", "GET", Some(&token)),
        event("/adm/logger", "GET", None),
        event("/health", "GET", None),
    ];
    let runtime = Runtime::start(
        events
            .into_iter()
            .enumerate()
            .map(|(i, e)| Invocation::new(&format!("admin-warm-{i}"), e))
            .collect(),
        false,
        false,
        202,
    )
    .await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.env("RUST_LOG", "info");
    let process = Process::spawn(cmd);
    runtime.wait_posts(9).await;
    let result = process.collect(true).await;
    result.save(
        "native-admin-warm-no-mutation",
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_posts(&runtime, 9);
    assert_audits(&result, 9);
    let posts = runtime.records();
    assert_eq!(
        posts
            .iter()
            .map(|p| p.json()["statusCode"].clone())
            .collect::<Vec<_>>(),
        vec![
            json!(200),
            json!(401),
            json!(200),
            json!(404),
            json!(200),
            json!(404),
            json!(200),
            json!(401),
            json!(200)
        ]
    );
    assert_eq!(posts[2].json()["body"], r#"{"filter":"info"}"#);
    assert_eq!(posts[4].json()["body"], posts[2].json()["body"]);
    assert_eq!(
        posts[6].json()["body"],
        r#"{"runtime":"rust","modules":["serverless-core","serverless-aws"]}"#
    );
    assert_eq!(posts[0].json()["body"], "OK");
    assert_eq!(posts[8].json()["body"], "OK");
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 1);
}
