use super::support::*;
use serde_json::json;

#[tokio::test]
async fn correlation_selected_scalar_survives_real_sdk_invoke() {
    let invoke = RawServer::start(vec![Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(dir.path(), "correlation.enabled", json!(true));
    let classes = serverless_core::compat::COMPATIBILITY;
    let declarations = ["correlation", "proxy"].map(|alias| {
        format!(
            "{}@{alias}",
            classes.iter().find(|c| c.alias == alias).unwrap().class
        )
    });
    std::fs::write(
        dir.path().join("handler.yml"),
        json!({"handlers":declarations,"defaultHandlers":["correlation","proxy"]}).to_string(),
    )
    .unwrap();
    let mut request = event("/x", "GET", None);
    request["multiValueHeaders"]["X-Correlation-ID"] = json!(["first", "second"]);
    request["multiValueHeaders"]["x-unrelated"] = json!(["alpha", "omega"]);
    let runtime = Runtime::start(vec![Invocation::new("scalar", request)], false, false, 202).await;
    let process = Process::spawn(command(dir.path(), Some(&runtime)));
    runtime.wait_posts(1).await;
    let result = process.collect(true).await;
    assert_posts(&runtime, 1);
    assert_eq!(invoke.count(), 1);
    let business = invoke.records()[0].json();
    assert_eq!(business["headers"]["x-correlation-id"], "first");
    assert_eq!(
        business["multiValueHeaders"]["x-correlation-id"],
        json!(["first", "second"])
    );
    assert_eq!(business["headers"]["x-unrelated"], "omega");
    result.save("correlation-scalar-sdk", Some(&runtime), &invoke, &jwks);
}
