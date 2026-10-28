use super::support::*;
use serde_json::json;

#[tokio::test]
async fn wp13_native_gcm_stage_traverses_built_bootstrap_and_real_sdk() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    for key in ["security.enabled", "unified-security.enabled"] {
        update_values(dir.path(), key, json!(false));
    }
    update_values(
        dir.path(),
        "light-lambda.stage",
        json!(
            "CRYPT:01010101010101010101010101010101:020202020202020202020202:384c3e9b2a30679cf57d653304f96af7599f065069b95682eb8275ee23e927a00d53a009e193590afa"
        ),
    );
    let runtime = Runtime::start(
        vec![Invocation::new("native-gcm", event("/x", "GET", None))],
        false,
        false,
        202,
    )
    .await;
    let mut child = command(dir.path(), Some(&runtime));
    child.env("light_4j_config_password", "test-only-master");
    let process = Process::spawn(child);
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 1);
    assert_eq!(runtime.records()[0].json()["statusCode"], 200);
    assert_eq!(invoke.count(), 1);
    assert_eq!(
        invoke.records()[0].json()["requestContext"]["stage"],
        "test-only-decrypted-value"
    );
    assert_eq!(jwks.count(), 0);
    for forbidden in ["test-only-master", "test-only-decrypted-value", "CRYPT:"] {
        assert!(!result.stdout.contains(forbidden));
        assert!(!result.stderr.contains(forbidden));
    }
    result.save("WP13-native-GCM-stage", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn wp13_path_only_health_plan_runs_in_the_built_bootstrap() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(dir.path(), "health.enabled", json!(true));
    let class = serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.alias == "health")
        .unwrap()
        .class;
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{class}@health")],"defaultHandlers":[],"paths":[{"path":"/health","method":"get","exec":["health"]}]}).to_string()).unwrap();
    let runtime = Runtime::start(
        vec![
            Invocation::new("path-health", event("/health", "GET", None)),
            Invocation::new("unmatched", event("/other", "GET", None)),
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
    assert_eq!(posts[0].json()["body"], "OK");
    assert_eq!(posts[1].json()["statusCode"], 400);
    assert!(
        posts[1].json()["body"]
            .as_str()
            .unwrap()
            .contains("ERR10086")
    );
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    result.save("WP13-path-only-health", Some(&runtime), &invoke, &jwks);
}

#[tokio::test]
async fn wp13_blank_basic_credentials_keep_exact_scheme_and_json_error_headers() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    std::fs::write(dir.path().join("basic-auth.yml"),json!({"enabled":true,"allowAnonymous":false,"allowBearerToken":false,"enableAD":false,"users":[]}).to_string()).unwrap();
    let class = |alias| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.alias == alias)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@basic",class("basic")),format!("{}@proxy",class("proxy"))],"defaultHandlers":["basic","proxy"]}).to_string()).unwrap();
    let mut request = event("/x", "GET", None);
    request["multiValueHeaders"]["authorization"] = json!(["Basic "]);
    let runtime = Runtime::start(
        vec![Invocation::new("blank-basic", request)],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 1);
    let reply = runtime.records()[0].json();
    assert_eq!(reply["statusCode"], 401);
    assert_eq!(
        reply["multiValueHeaders"]["content-type"],
        json!(["application/json"])
    );
    let status: serde_json::Value = serde_json::from_str(reply["body"].as_str().unwrap()).unwrap();
    assert_eq!(status["code"], "ERR12003");
    assert_eq!(status["description"], "Invalid authorization header BASIC.");
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    result.save("WP13-blank-basic", Some(&runtime), &invoke, &jwks);
}
