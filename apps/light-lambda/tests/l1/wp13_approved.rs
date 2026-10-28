use super::support::*;
use serde_json::json;
#[tokio::test]
async fn wp13_selected_dynamo_reports_init_error_before_poll_or_io() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    std::fs::write(dir.path().join("service.yml"),json!({"singletons":[{"com.networknt.cache.CacheManager":["com.networknt.aws.lambda.cache.DynamoDbCacheManager"]}]}).to_string()).unwrap();
    let runtime = Runtime::start(vec![], false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(false).await;
    assert_eq!(result.status.code(), Some(1));
    assert_eq!(runtime.records().len(), 1);
    assert_eq!(runtime.records()[0].path, "/2018-06-01/runtime/init/error");
    assert!(
        runtime.records()[0].json()["errorMessage"]
            .as_str()
            .unwrap()
            .contains("DynamoDbCacheManager unavailable")
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
        "WP13-selected-Dynamo-admission",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}

#[tokio::test]
async fn wp13_transformer_presence_maps_traverse_built_bootstrap_and_sdk() {
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok(), Reply::ok(), Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    let fixture:serde_json::Value=serde_json::from_str(include_str!("../../../../tests/serverless-conformance/fixtures/v1/t/T-02-actual-native-response-input-map-multi-contract-v1.json")).unwrap();
    std::fs::write(
        dir.path().join("rule.yml"),
        fixture["ruleConfigurationPair"]["rust"]["rule"].to_string(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("handler.yml"),
        fixture["config"]["files"]["handler"].to_string(),
    )
    .unwrap();
    for file in ["request-transformer.yml", "response-transformer.yml"] {
        std::fs::write(
            dir.path().join(file),
            json!({"enabled":true,"appliedPathPrefixes":["/x"]}).to_string(),
        )
        .unwrap();
    }
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/*@post":"test-only-business"}),
    );
    let mut events = vec![];
    for mode in ["absent", "null", "empty", "populated"] {
        let mut request = event("/x", "POST", Some("before"));
        for key in [
            "queryStringParameters",
            "multiValueQueryStringParameters",
            "pathParameters",
        ] {
            request.as_object_mut().unwrap().remove(key);
        }
        if mode != "absent" {
            for key in ["queryStringParameters", "pathParameters"] {
                request[key] = if mode == "null" {
                    serde_json::Value::Null
                } else if mode == "empty" {
                    json!({})
                } else {
                    json!({"id":"42"})
                };
            }
        }
        events.push(Invocation::new(mode, request));
    }
    let runtime = Runtime::start(events, false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(4).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 4);
    assert_eq!(invoke.count(), 4);
    assert_eq!(jwks.count(), 0);
    for post in runtime.records() {
        assert_eq!(post.json()["statusCode"], 200);
        assert_eq!(post.json()["body"], "response-transformed");
    }
    for call in invoke.records() {
        assert_eq!(call.json()["body"], "request-transformed");
    }
    result.save(
        "WP13-Transformer-map-presence",
        Some(&runtime),
        &invoke,
        &jwks,
    );
}
