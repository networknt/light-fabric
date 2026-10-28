use serde_json::json;
use serverless_conformance::{incremental, load};
#[tokio::test]
async fn failed_request_reaches_actual_init_factory_router_and_never_transport() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures");
    let f = load(&root.join(
        "incremental-router-failed-v1/H-04-already-failed-router-forwarding-profile-v1.json",
    ))
    .unwrap();
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    let o = &r["observations"][0];
    assert_eq!(o["status"], 400);
    assert_eq!(o["code"], "ERR10087");
    assert_eq!(o["failedState"], true);
    assert_eq!(o["exchangeState"], (1 << 1) | (1 << 3));
    assert_eq!(o["backendCalls"], json!([]));
    assert_eq!(o["httpCalls"], json!([]));
    // Original rewriting/activation profile remains distinct and unchanged.
    let original = load(&root.join("v1/h/H-04-already-failed-router-component-v1.json")).unwrap();
    assert!(
        original["config"]["files"]["router"]
            .get("enabled")
            .is_none()
    );
    assert!(
        !original["config"]["files"]["router"]["urlRewriteRules"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(f["expect"], original["expect"]);
}

#[test]
fn failed_router_comparison_asserts_the_actual_status_state_and_zero_calls() {
    let f=load(&std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance/fixtures/incremental-router-failed-v1/H-04-already-failed-router-forwarding-profile-v1.json")).unwrap();
    let expected = json!({"execution":"passed","observations":[f["expect"]]});
    incremental::compare(&f, &expected, &expected).unwrap();
    for (field, bad) in [
        ("exchangeState", json!(2)),
        ("failedState", json!(false)),
        ("code", json!("ERR10000")),
        ("httpCalls", json!([{}])),
        ("backendCalls", json!([{}])),
    ] {
        let mut changed = expected.clone();
        changed["observations"][0][field] = bad;
        assert!(
            incremental::compare(&f, &expected, &changed).is_err(),
            "{field}"
        );
    }
}
