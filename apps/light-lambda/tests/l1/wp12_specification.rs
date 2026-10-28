use super::support::*;
use serde_json::json;

#[tokio::test]
async fn actual_app_local_specification_order_scopes_cache_and_post_init_mutation() {
    let keys = Keys::new("wp12-specification");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "openapi-handler.enabled", json!(true));
    update_values(
        dir.path(),
        "security.skipVerifyScopeWithoutSpec",
        json!(false),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"), json!({"handlers":[format!("{}@spec",class(serverless_core::HandlerId::Specification)),format!("{}@jwt",class(serverless_core::HandlerId::Jwt)),format!("{}@proxy",class(serverless_core::HandlerId::Proxy))],"defaultHandlers":["spec","jwt","proxy"]}).to_string()).unwrap();
    // Deliberately reverse lexical ordering: lexical-only read token must not
    // satisfy the first declared z scheme's write requirement.
    let source = r#"{"openapi":"3.1.0","servers":[{"url":"https://never-contact.invalid/api"}],"components":{"securitySchemes":{"z":{"type":"oauth2"},"a":{"type":"oauth2"}}},"paths":{"/write":{"get":{"security":[{"z":["write"],"a":["read"]}]}},"/read":{"get":{"security":[{"a":["read"]}]}}}}"#;
    std::fs::write(dir.path().join("openapi.yml"), source).unwrap();
    std::fs::write(
        dir.path().join("openapi.yaml"),
        "ignored-sensitive-marker: [",
    )
    .unwrap();
    let mut read = claims("source-user");
    read["scope"] = json!("read");
    let token = keys.token(read);
    let mut second = Invocation::new("spec-read", event("/api/read", "GET", Some(&token)));
    second.pause_ms = 500;
    let runtime = Runtime::start(
        vec![
            Invocation::new("spec-write", event("/api/write", "GET", Some(&token))),
            second,
            Invocation::new("spec-write-warm", event("/api/write", "GET", Some(&token))),
            Invocation::new("spec-invalid", event("/api/read", "GET", Some("invalid"))),
            Invocation::new(
                "spec-unmatched",
                event("/api/unmatched", "GET", Some(&token)),
            ),
            Invocation::new("spec-method", event("/api/read", "POST", Some(&token))),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    // Captured source/digest/scope decisions belong to Init, not a mutable file.
    std::fs::write(
        dir.path().join("openapi.yml"),
        "changed-sensitive-marker: [",
    )
    .unwrap();
    runtime.wait_posts(6).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 6);
    assert_audits(&result, 6);
    assert_eq!(invoke.count(), 1);
    assert_eq!(jwks.count(), 1);
    assert_eq!(
        runtime
            .records()
            .iter()
            .map(|r| r.json()["statusCode"].clone())
            .collect::<Vec<_>>(),
        vec![
            json!(403),
            json!(200),
            json!(403),
            json!(401),
            json!(404),
            json!(405)
        ]
    );
    let calls = invoke.records();
    assert_eq!(calls[0].json()["path"], "/api/read");
    assert_eq!(
        calls[0].json()["requestContext"]["authorizer"]["principalId"],
        "source-user"
    );
    assert!(!result.stdout.contains("sensitive-marker"));
    let digests = result
        .audits()
        .into_iter()
        .map(|v| v["fields"]["config_digest"].clone())
        .collect::<Vec<_>>();
    assert!(
        digests
            .iter()
            .all(|d| d.as_str().is_some_and(|s| s.len() == 64))
    );
    assert!(digests.iter().all(|d| d == &digests[0]));
    result.save(
        "WP12-specification-built-app",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}
