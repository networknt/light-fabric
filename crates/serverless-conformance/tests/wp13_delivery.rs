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
async fn four_native_encoder_profiles_and_path_only_health_use_real_application() {
    for id in [
        "V-03-json-golden-encoder-multi-contract-v1",
        "V-03-xml-1-1-golden-encoder-multi-contract-v1",
        "V-03-xml-1-1-content-golden-encoder-multi-contract-v1",
        "V-03-xml-1-1-attribute-golden-encoder-multi-contract-v1",
        "X-01-local-health-multi-contract-v1",
    ] {
        let r = incremental::run_fixture(&fixture(id), &"0".repeat(40))
            .await
            .unwrap();
        assert_eq!(r["execution"], "passed", "{id}: {}", r["assertionErrors"]);
    }
}

#[tokio::test]
async fn blank_basic_credentials_keep_the_recognized_scheme_and_header_conflict_visible() {
    let f = fixture("S-08-basic-failures-warm-multi-contract-v1");
    let r = incremental::run_fixture_original(&f, &"0".repeat(40))
        .await
        .unwrap();
    assert_eq!(
        r["observations"][2]["body"],
        f["steps"][2]["expect"]["body"]
    );
    assert_eq!(r["execution"], "failed"); // Preserve the original response-header failure.
    assert!(
        r["assertionErrors"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.as_str().unwrap().contains("header multimap"))
    );
    let amended = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(amended["execution"], "passed");
    assert_eq!(amended["observations"], r["observations"]);
}

#[tokio::test]
async fn init_rejections_are_observed_and_never_fabricated_from_expectations() {
    let mut f = fixture("R-06-duplicate-only-init-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed");
    assert_eq!(r["observations"][0]["outcome"], "initError");
    f["config"]["files"]["lambda-proxy"]["functions"] = json!({"/pets/fixed@get":"Literal"});
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "failed");
    assert_ne!(r["observations"][0]["outcome"], "initError");
}

#[tokio::test]
async fn fake_invoke_records_actual_sdk_settings_and_service_errors() {
    let mut f = fixture("D-04-log-type-multi-contract-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(
        r["observations"][0]["backendCalls"][0]["invoke"]["logType"],
        "None"
    );
    f["expect"]["backendCalls"][0]["invoke"]["logType"] = json!("Tail");
    assert_eq!(
        incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap()["execution"],
        "failed"
    );
    let r = incremental::run_fixture(
        &fixture("D-06-typed-sdk-1-multi-contract-v1"),
        &"0".repeat(40),
    )
    .await
    .unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(
        r["observations"][0]["backendCalls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn warm_fixture_changes_files_without_reinitializing_the_snapshot() {
    let r = incremental::run_fixture(
        &fixture("CFG-05-immutable-valid-snapshot-multi-contract-v1"),
        &"0".repeat(40),
    )
    .await
    .unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["observations"].as_array().unwrap().len(), 2);
    for observation in r["observations"].as_array().unwrap() {
        assert_eq!(observation["backendCalls"][0]["target"], "FnA");
    }
}

#[test]
fn reviewed_metadata_does_not_change_expectations_or_mask_observed_mismatches() {
    let f = fixture("V-03-malformed-json-multi-contract-v1");
    let j = json!({"execution":"passed","observations":[f["expectJava"]]});
    let mut r = json!({"execution":"passed","observations":[f["expect"]]});
    assert!(incremental::compare(&f, &j, &r).is_err());
    declarations::compare(&f, &j, &r).unwrap();
    r["observations"][0]["body"] = json!("wrong");
    assert!(declarations::compare(&f, &j, &r).is_err());
    let mut changed = f.clone();
    changed["expect"]["body"] = json!("changed contract");
    assert!(declarations::review(&changed).is_err());
    let reviews: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v1.json"
    ))
    .unwrap();
    assert_eq!(reviews["reviews"].as_array().unwrap().len(), 76);
    for review in reviews["reviews"].as_array().unwrap() {
        assert!(
            declarations::review(&fixture(review["id"].as_str().unwrap()))
                .unwrap()
                .is_some()
        );
    }
}

#[test]
fn selected_deferred_declaration_is_incomplete_and_distinct_from_unregistered() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("deferred");
    assert!(
        incremental::command(&[
            "run".into(),
            "--fixture".into(),
            "M-03-enabled-influx-deferred-init-contract-v1".into(),
            "--out".into(),
            out.to_string_lossy().into()
        ])
        .is_err()
    );
    let coverage = load(&out.join("coverage.json")).unwrap();
    assert_eq!(coverage["pass"], 0);
    assert_eq!(coverage["selectedDeferredHandlers"], 1);
    assert_eq!(coverage["selectedUnregisteredHandlers"], 0);
    assert_eq!(coverage["complete"], false);
}
