use serde_json::{Value, json};
use serverless_conformance::{declarations, fixture_files, incremental, load};
use std::path::PathBuf;

fn fixture(id: &str) -> Value {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/serverless-conformance/fixtures/v1");
    fixture_files(&root)
        .unwrap()
        .iter()
        .map(|p| load(p).unwrap())
        .find(|f| f["id"] == id)
        .unwrap()
}

#[tokio::test]
async fn actual_init_error_satisfies_the_existing_http_zero_call_assertion() {
    let mut f = fixture("H-04-loopback-test-slow-represented-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    let observed = &r["observations"][0];
    assert_eq!(observed["code"], "InvalidConfiguration");
    assert_eq!(observed["backendCalls"], json!([]));
    assert_eq!(observed["httpCalls"], json!([]));
    let mut wrong = observed.clone();
    wrong["httpCalls"] = json!([{"method":"GET","uri":"/unexpected"}]);
    assert!(incremental::assert_observation(&f["expect"], &wrong).is_err());
    f["expect"]["code"] = json!("WrongInitCode");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "failed");
    assert!(
        r["assertionErrors"][0]
            .as_str()
            .unwrap()
            .contains("Init code mismatch")
    );
}

#[test]
fn all_closure_reviews_preserve_original_expectations() {
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v2.json"
    ))
    .unwrap();
    assert_eq!(metadata["reviews"].as_array().unwrap().len(), 16);
    for row in metadata["reviews"].as_array().unwrap() {
        let f = fixture(row["id"].as_str().unwrap());
        declarations::review(&f).unwrap().unwrap();
        let mut changed = f.clone();
        changed["title"] = json!("changed fixture");
        assert!(declarations::review(&changed).is_err());
    }
}

#[tokio::test]
async fn init_response_declaration_review_does_not_mask_a_real_outcome_change() {
    let f = fixture("R-05-missing-route-0-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    let j = json!({"execution":"passed","observations":[f["expectJava"]]});
    declarations::compare(&f, &j, &r).unwrap();
    let mut bad = r.clone();
    bad["observations"][0]["outcome"] = json!("response");
    assert!(declarations::compare(&f, &j, &bad).is_err());
    // Init has no ALB header namespace; a malformed response expectation must
    // not acquire this allowance merely because its headers are absent.
    let response = json!({"outcome":"response","status":200,"body":"ok","bodyBase64":false,"backendCalls":[],"jwksFetches":0});
    assert!(incremental::assert_observation(&response, &response).is_err());
}
