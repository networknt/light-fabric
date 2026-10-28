use super::support::*;
use serde_json::json;
use serverless_core::HandlerId;

#[tokio::test]
async fn actual_local_terminals_authenticate_return_exact_redacted_payloads_and_never_dispatch() {
    let keys = Keys::new("wp12-admin");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "health.enabled", json!(true));
    update_values(dir.path(), "info.enableServerInfo", json!(true));
    update_values(dir.path(), "logger.enabled", json!(true));
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@h",class(HandlerId::Health)),format!("{}@i",class(HandlerId::Info)),format!("{}@l",class(HandlerId::GetLogger)),format!("{}@auth",class(HandlerId::Jwt)),format!("{}@proxy",class(HandlerId::Proxy))],"paths":[{"path":"/health","method":"get","exec":["h"]},{"path":"/adm/health","method":"get","exec":["h"]},{"path":"/adm/server/info","method":"get","exec":["auth","i"]},{"path":"/adm/logger","method":"get","exec":["auth","l"]}],"defaultHandlers":["proxy"]}).to_string()).unwrap();
    let token = keys.token(claims("admin"));
    let events = [
        ("/health", "GET", None),
        ("/adm/health", "GET", None),
        ("/adm/server/info", "GET", None),
        ("/adm/server/info", "GET", Some(token.as_str())),
        ("/adm/logger", "GET", Some(token.as_str())),
        ("/adm/logger", "POST", Some(token.as_str())),
        ("/adm/cache/test", "GET", Some(token.as_str())),
        ("/health", "POST", None),
    ];
    let runtime = Runtime::start(
        events
            .iter()
            .enumerate()
            .map(|(i, (p, m, t))| Invocation::new(&format!("admin-{i}"), event(p, m, *t)))
            .collect(),
        false,
        false,
        202,
    )
    .await;
    let mut cmd = command(dir.path(), Some(&runtime));
    cmd.env("RUST_LOG", "info");
    let process = Process::spawn(cmd);
    runtime.wait_posts(events.len()).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, events.len());
    assert_audits(&result, events.len());
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 1);
    let posts = runtime.records();
    assert_eq!(
        posts
            .iter()
            .map(|r| r.json()["statusCode"].clone())
            .collect::<Vec<_>>(),
        vec![
            json!(200),
            json!(200),
            json!(401),
            json!(200),
            json!(200),
            json!(404),
            json!(404),
            json!(404)
        ]
    );
    assert_eq!(posts[0].json()["body"], "OK");
    assert_eq!(
        posts[0].json()["multiValueHeaders"]["content-type"],
        json!(["text/plain; charset=utf-8"])
    );
    assert_eq!(
        posts[3].json()["body"],
        r#"{"runtime":"rust","modules":["serverless-core","serverless-aws"]}"#
    );
    assert_eq!(posts[4].json()["body"], r#"{"filter":"info"}"#);
    result.save("WP12-admin-real-built-app", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn installed_filter_reports_fallback_and_skip_auth_cannot_expose_admin() {
    for filter in ["info", "["] {
        let keys = Keys::new("wp12-filter");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![Reply::ok()]).await;
        let dir = config(&invoke, &jwks);
        update_values(dir.path(), "logger.enabled", json!(true));
        update_values(dir.path(), "info.enableServerInfo", json!(true));
        update_values(
            dir.path(),
            "security.skipPathPrefixes",
            json!(["/adm/server/info"]),
        );
        let class = |id| {
            serverless_core::compat::COMPATIBILITY
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .class
        };
        std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@auth",class(HandlerId::Jwt)),format!("{}@logger",class(HandlerId::GetLogger)),format!("{}@info",class(HandlerId::Info)),format!("{}@proxy",class(HandlerId::Proxy))],"paths":[{"path":"/adm/logger","method":"get","exec":["auth","logger"]},{"path":"/adm/server/info","method":"get","exec":["auth","info"]}],"defaultHandlers":["proxy"]}).to_string()).unwrap();
        let token = keys.token(claims("filter-user"));
        let runtime = Runtime::start(
            vec![
                Invocation::new("skipped", event("/adm/server/info", "GET", None)),
                Invocation::new("filter", event("/adm/logger", "GET", Some(&token))),
            ],
            false,
            false,
            202,
        )
        .await;
        let mut cmd = command(dir.path(), Some(&runtime));
        cmd.env("RUST_LOG", filter);
        let process = Process::spawn(cmd);
        runtime.wait_posts(2).await;
        let result = process.collect(true).await;
        assert_eq!(runtime.records()[0].json()["statusCode"], 401);
        assert_eq!(invoke.count(), 0);
        assert_eq!(runtime.records()[1].json()["statusCode"], 200);
        assert_eq!(runtime.records()[1].json()["body"], r#"{"filter":"info"}"#);
        assert_eq!(jwks.count(), 1);
        result.save("WP12-admin-skipped-auth", Some(&runtime), &invoke, &jwks);
    }
}
