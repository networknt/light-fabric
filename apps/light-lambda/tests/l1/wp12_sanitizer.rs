use super::support::*;
use serde_json::json;
use serverless_core::HandlerId;

#[tokio::test]
async fn real_sanitizer_decodes_base64_rejects_malformed_and_isolates_warm_requests() {
    let keys = Keys::new("sanitizer");
    let jwks = RawServer::start(vec![Reply::json(keys.jwks())]).await;
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "sanitizer.enabled", json!(true));
    update_values(dir.path(), "sanitizer.bodyEncoder", json!("html"));
    update_values(dir.path(), "sanitizer.headerEnabled", json!(false));
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"), json!({"handlers":[format!("{}@sanitizer",class(HandlerId::Sanitizer)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["sanitizer","proxy"]}).to_string()).unwrap();
    let mut encoded = event("/x", "POST", None);
    encoded["body"] = json!("eyJ2YWx1ZSI6IjxiPiJ9");
    encoded["isBase64Encoded"] = json!(true);
    let mut malformed = event("/x", "POST", None);
    malformed["body"] = json!("{bad");
    let mut plain = event("/x", "POST", None);
    plain["body"] = json!("plain <b>");
    let runtime = Runtime::start(
        vec![
            Invocation::new("encoded", encoded),
            Invocation::new("malformed", malformed),
            Invocation::new("plain", plain),
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
    assert_audits(&result, 3);
    assert_eq!(invoke.count(), 2);
    assert_eq!(jwks.count(), 0);
    let posts = runtime.records();
    assert_eq!(posts[0].json()["statusCode"], 200);
    assert_eq!(posts[1].json()["statusCode"], 400);
    assert!(
        posts[1].json()["body"]
            .as_str()
            .unwrap()
            .contains("ERR10015")
    );
    assert_eq!(posts[2].json()["statusCode"], 200);
    let requests = invoke.records();
    assert_eq!(requests[0].json()["body"], "eyJ2YWx1ZSI6IiZsdDtiJmd0OyJ9");
    assert_eq!(requests[0].json()["isBase64Encoded"], true);
    assert_eq!(requests[1].json()["body"], "plain <b>");
    result.save("WP12-sanitizer-built-app", Some(&runtime), &invoke, &jwks);
}
