//! Owned Runtime API/Lambda loopback proof of actual app registrations.
use super::support::*;
use serde_json::{Value, json};
use serverless_core::HandlerId;
use std::path::Path;

fn activate(dir: &Path, fixture: &str, response: bool) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1/t")
        .join(fixture);
    let pair: Value = serde_json::from_slice(&std::fs::read(source).unwrap()).unwrap();
    std::fs::write(
        dir.join("rule.yml"),
        pair["ruleConfigurationPair"]["rust"]["rule"].to_string(),
    )
    .unwrap();
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let mut handlers = vec![
        format!("{}@request", class(HandlerId::RequestTransformer)),
        format!("{}@proxy", class(HandlerId::Proxy)),
    ];
    let mut chain = vec!["request", "proxy"];
    if response {
        handlers.push(format!(
            "{}@response",
            class(HandlerId::ResponseTransformer)
        ));
        chain.push("response");
    }
    std::fs::write(
        dir.join("handler.yml"),
        json!({"handlers":handlers,"defaultHandlers":chain}).to_string(),
    )
    .unwrap();
    for file in ["request-transformer.yml", "response-transformer.yml"] {
        std::fs::write(
            dir.join(file),
            json!({"enabled":true,"appliedPathPrefixes":["/x"]}).to_string(),
        )
        .unwrap();
    }
    update_values(
        dir,
        "lambda-proxy.functions",
        json!({"/*@post":"test-only-business"}),
    );
}

#[tokio::test]
async fn real_transformer_factories_pair_request_response_and_warm_input_rejection() {
    let keys = Keys::new("transformer-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(
        dir.path(),
        "T-02-actual-native-response-input-map-multi-contract-v1.json",
        true,
    );
    let mut normal = event("/x", "POST", None);
    normal["body"] = json!("mutate");
    let mut excess = normal.clone();
    excess["body"] = json!("x".repeat(32768));
    let runtime = Runtime::start(
        vec![
            Invocation::new("transform-first", normal.clone()),
            Invocation::new("transform-reject", excess),
            Invocation::new("transform-warm", normal),
        ],
        false,
        false,
        202,
    )
    .await;
    let result_process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(3).await;
    let result = result_process.collect(true).await;
    result.save(
        "WP12-transformer-pair-built-app",
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_posts(&runtime, 3);
    assert_audits(&result, 3);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    for index in [0, 2] {
        assert_eq!(posts[index].json()["statusCode"], 200);
        assert_eq!(posts[index].json()["body"], "response-transformed");
    }
    // Later response handlers still run: their current-request context is also
    // oversized, so the approved response-input outcome replaces the 422.
    assert_eq!(posts[1].json()["statusCode"], 500);
    assert_eq!(
        posts[1].json()["body"],
        r#"{"statusCode":500,"code":"ERR14032","message":"TRANSFORMER_OUTPUT_OUTSIDE_PROFILE","description":"Transformer output exceeds the admitted profile.","severity":"ERROR"}"#
    );
    for request in invoke.records() {
        assert_eq!(request.json()["body"], "request-transformed");
    }
}

#[tokio::test]
async fn request_only_transformer_rejects_with_exact_422_and_zero_dispatch() {
    let keys = Keys::new("transformer-request-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(
        dir.path(),
        "T-02-actual-native-response-input-map-multi-contract-v1.json",
        false,
    );
    let mut raw = event("/x", "POST", None);
    raw["body"] = json!("x".repeat(32768));
    let runtime = Runtime::start(
        vec![Invocation::new("transform-request-overflow", raw)],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    result.save(
        "WP12-transformer-request-overflow-built-app",
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_posts(&runtime, 1);
    assert_audits(&result, 1);
    assert_eq!(invoke.count(), 0);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 422);
    assert_eq!(
        posts[0].json()["body"],
        r#"{"statusCode":422,"code":"ERR14031","message":"TRANSFORMER_INPUT_OUTSIDE_PROFILE","description":"Request exceeds the admitted Transformer input profile.","severity":"ERROR"}"#
    );
}

#[tokio::test]
async fn real_response_transformer_oversize_replaces_once_without_redispatch() {
    let keys = Keys::new("transformer-overflow-unused");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![
        Reply::json(json!({"statusCode":200,"body":"x".repeat(32768)})),
        Reply::ok(),
    ])
    .await;
    let dir = config(&invoke, &jwks);
    activate(
        dir.path(),
        "T-02-actual-native-response-input-map-multi-contract-v1.json",
        true,
    );
    let mut raw = event("/x", "POST", None);
    raw["body"] = json!("mutate");
    let runtime = Runtime::start(
        vec![
            Invocation::new("transform-response-overflow", raw.clone()),
            Invocation::new("transform-response-warm", raw),
        ],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(2).await;
    let result = process.collect(true).await;
    result.save(
        "WP12-transformer-response-overflow-built-app",
        Some(&runtime),
        &invoke,
        &jwks,
    );
    assert_posts(&runtime, 2);
    assert_audits(&result, 2);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 500);
    assert_eq!(
        posts[0].json()["body"],
        r#"{"statusCode":500,"code":"ERR14032","message":"TRANSFORMER_OUTPUT_OUTSIDE_PROFILE","description":"Transformer output exceeds the admitted profile.","severity":"ERROR"}"#
    );
    assert_eq!(posts[1].json()["statusCode"], 200);
    assert_eq!(posts[1].json()["body"], "response-transformed");
}

#[tokio::test]
async fn actual_app_named_actions_and_both_caught_errors_use_preserved_pairs() {
    for (fixture, response) in [
        ("T-01-metadata-native-cel-multi-contract-v1.json", false),
        ("T-01-deny-native-cel-multi-contract-v1.json", false),
        ("T-01-local-native-cel-multi-contract-v1.json", false),
        (
            "T-02-response-header-remove-update-multi-contract-v1.json",
            true,
        ),
        (
            "T-01-caught-native-cel-rule-error-multi-contract-v1.json",
            false,
        ),
        (
            "T-02-caught-native-cel-rule-error-multi-contract-v1.json",
            true,
        ),
    ] {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures/v1/t")
            .join(fixture);
        let pair: Value = serde_json::from_slice(&std::fs::read(source).unwrap()).unwrap();
        let step = &pair["steps"][0];
        let payload = &step["backend"][0]["respond"]["payload"];
        let keys = Keys::new("transformer-actions-unused");
        let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
        let invoke = RawServer::start(vec![if payload.is_object() {
            Reply::json(payload.clone())
        } else {
            Reply::ok()
        }])
        .await;
        let dir = config(&invoke, &jwks);
        activate(dir.path(), fixture, response);
        update_values(
            dir.path(),
            "lambda-proxy.functions",
            json!({"/*@post":"test-only-business","/*@get":"test-only-business"}),
        );
        let runtime = Runtime::start(
            vec![Invocation::new(
                "transform-actions",
                step["rawEvent"].clone(),
            )],
            false,
            false,
            202,
        )
        .await;
        let process = Process::spawn(command(dir.path(), Some(&runtime)));
        runtime.wait_posts(1).await;
        let result = process.collect(true).await;
        result.save(
            &format!("WP12-transformer-{fixture}"),
            Some(&runtime),
            &invoke,
            &jwks,
        );
        assert_posts(&runtime, 1);
        assert_audits(&result, 1);
        assert_eq!(jwks.count(), 0);
        let expected = &step["expect"];
        assert_eq!(
            invoke.count(),
            expected["backendCalls"].as_array().unwrap().len(),
            "{fixture}"
        );
        let posts = runtime.records();
        let output = posts[0].json();
        assert_eq!(output["statusCode"], expected["status"], "{fixture}");
        assert_eq!(output["body"], expected["body"], "{fixture}");
        for header in expected["headers"].as_array().unwrap() {
            let name = header[0].as_str().unwrap().to_lowercase();
            assert_eq!(
                output["multiValueHeaders"][&name],
                json!([header[1].clone()]),
                "{fixture}: {name}"
            );
        }
        for header in expected["headersAbsent"].as_array().unwrap() {
            assert!(
                output["multiValueHeaders"]
                    .get(header.as_str().unwrap())
                    .is_none(),
                "{fixture}"
            );
        }
    }
}

#[tokio::test]
async fn approved_amendment_supplement_valid_transformer_input_executes_both_programs() {
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    activate(
        dir.path(),
        "T-02-actual-native-response-input-map-multi-contract-v1.json",
        true,
    );
    let mut raw = event("/x", "POST", None);
    raw["body"] = json!("mutate");
    assert!(raw.pointer("/requestContext/elb/targetGroupArn").is_some());
    let runtime = Runtime::start(
        vec![Invocation::new("valid-transform", raw)],
        false,
        false,
        202,
    )
    .await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    assert_eq!(invoke.count(), 1);
    assert_eq!(invoke.records()[0].json()["body"], "request-transformed");
    assert_eq!(runtime.records()[0].json()["statusCode"], 200);
    assert_eq!(runtime.records()[0].json()["body"], "response-transformed");
    result.save("WP13-valid-transformer", Some(&runtime), &invoke, &jwks);
}
