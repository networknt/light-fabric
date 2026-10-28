//! Actual built gateway + owned Runtime API/Invoke/JWKS servers. Validation is
//! in the gateway process; no precision child or scripted validation response.
use super::support::*;
use serde_json::{Value, json};
use serverless_core::{HandlerId, compat};

fn configure(dir: &std::path::Path) {
    std::fs::write(
        dir.join("openapi.json"),
        include_str!("data/validator-activation-openapi.json"),
    )
    .unwrap();
    std::fs::write(dir.join("openapi-handler.yml"), "enabled: true\n").unwrap();
    std::fs::write(dir.join("openapi-validator.yml"),"enabled: true\nskipBodyValidation: false\nhandleNullableField: true\nskipPathPrefixes: []\n").unwrap();
    update_values(dir, "security.enabled", json!(false));
    update_values(dir, "unified-security.enabled", json!(false));
    update_values(
        dir,
        "lambda-proxy.functions",
        json!({"/v1/decimal@post":"test-only-business","/v1/integer@post":"test-only-business","/v1/pets/{petId}@get":"test-only-business"}),
    );
    let class = |id| {
        compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@validator",class(HandlerId::Validator)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","validator","proxy"]}).to_string()).unwrap();
}
async fn run(cases: Vec<Value>, label: &str, petstore: bool) {
    let accepted: Vec<_> = cases.iter().filter(|c| c["status"] == 200).collect();
    let invoke = RawServer::start(accepted.iter().map(|_| Reply::ok()).collect()).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    configure(dir.path());
    if petstore {
        std::fs::remove_file(dir.path().join("openapi.json")).unwrap();
        std::fs::write(
            dir.path().join("openapi.yaml"),
            include_str!("../../migrations/native-petstore/openapi.yaml"),
        )
        .unwrap();
        update_values(
            dir.path(),
            "lambda-proxy.functions",
            json!({"/v1/pets@post":"test-only-business"}),
        );
    }
    let events = cases
        .iter()
        .map(|c| {
            let mut e = event(
                c["path"].as_str().unwrap(),
                c["method"].as_str().unwrap(),
                None,
            );
            e["multiValueHeaders"]["Content-Type"] =
                json!([c["contentType"].as_str().unwrap_or("application/json")]);
            if let Some(value) = c.get("queryLimit") {
                e["multiValueQueryStringParameters"]["limit"] = json!([value]);
            }
            if let Some(value) = c.get("requiredHeader") {
                e["multiValueHeaders"]["X-Required"] = json!([value]);
            }
            if let Some(body) = c.get("body") {
                e["body"] = body.clone();
            }
            e["multiValueHeaders"]["X-Traceability-Id"] =
                json!(["00000000-0000-4000-8000-000000000001"]);
            e["isBase64Encoded"] = json!(false);
            Invocation::new(c["id"].as_str().unwrap(), e)
        })
        .collect();
    let runtime = Runtime::start(events, false, false, 202).await;
    let base = command(dir.path(), Some(&runtime));
    let executable = std::env::var_os("VALIDATOR_IN_PROCESS_APPLICATION")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_validator-in-process").into());
    let mut cmd = tokio::process::Command::new(executable);
    cmd.env_clear()
        .envs(
            base.as_std()
                .get_envs()
                .filter_map(|(k, v)| v.map(|v| (k, v))),
        )
        .current_dir(dir.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let process = Process::spawn(cmd);
    runtime.wait_posts(cases.len()).await;
    let result = process.collect(true).await;
    // Preserve observations before assertions so a mismatch keeps its evidence.
    result.save(label, Some(&runtime), &invoke, &jwks);
    assert_posts(&runtime, cases.len());
    for sentinel in ["TrialPet", "WarmPet"] {
        assert!(!result.stdout.contains(sentinel));
        assert!(!result.stderr.contains(sentinel));
    }
    for (case, post) in cases.iter().zip(runtime.records()) {
        let response = post.json();
        assert_eq!(response["statusCode"], case["status"], "{}", case["id"]);
        if case["status"] != 200 {
            let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
            assert_eq!(body["code"], case["code"], "{}", case["id"]);
            assert_eq!(body["message"], case["message"], "{}", case["id"]);
        }
    }
    assert_eq!(invoke.count(), accepted.len());
    for (case, call) in accepted.iter().zip(invoke.records()) {
        let received = call.json();
        assert_eq!(received["path"], case["path"]);
        assert_eq!(received["httpMethod"], case["method"]);
        assert_eq!(received["requestContext"]["extendedRequestId"], case["id"]);
        if let Some(body) = case.get("body") {
            assert_eq!(&received["body"], body, "exact body bytes");
        }
    }
    assert_eq!(
        jwks.count(),
        0,
        "owned JWKS server actually observed zero calls"
    );
}
#[tokio::test]
async fn in_process_built_gateway_uses_existing_fifteen_case_semantic_matrix() {
    run(
        serde_json::from_str(include_str!("data/validator-activation-cases.json")).unwrap(),
        "validator-in-process-semantics",
        false,
    )
    .await;
}
#[tokio::test]
async fn in_process_built_gateway_valid_invalid_warm_valid_two_real_sdk_sends() {
    run(vec![
        json!({"id":"in-process-valid","path":"/v1/decimal","method":"POST","body":"{\"n\":0.3}","status":200}),
        json!({"id":"in-process-invalid","path":"/v1/decimal","method":"POST","body":"{}","status":400,"code":"ERR11004","message":"VALIDATOR_SCHEMA"}),
        json!({"id":"in-process-warm-valid","path":"/v1/decimal","method":"POST","body":"{\"n\":0.2}","status":200}),
    ],"validator-in-process-acceptance",false).await;
}

#[tokio::test]
async fn in_process_built_gateway_checked_in_petstore_valid_invalid_warm_valid() {
    run(vec![
        json!({"id":"petstore-valid","path":"/v1/pets","method":"POST","body":"{\"name\":\"TrialPet\"}","status":200}),
        json!({"id":"petstore-invalid","path":"/v1/pets","method":"POST","body":"{}","status":400,"code":"ERR11004","message":"VALIDATOR_SCHEMA"}),
        json!({"id":"petstore-warm","path":"/v1/pets","method":"POST","body":"{\"name\":\"WarmPet\"}","status":200}),
    ],"validator-in-process-petstore",true).await;
}
