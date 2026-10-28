use serde_json::{Value, json};
use serverless_conformance::{amendments, fixture_files, incremental, load};
fn fixture(id: &str) -> Value {
    fixture_files(
        &std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures/v1"),
    )
    .unwrap()
    .iter()
    .map(|p| load(p).unwrap())
    .find(|f| f["id"] == id)
    .unwrap()
}
#[test]
fn approved_versions_bind_exact_originals_and_reject_drift() {
    for id in [
        "C-03-completion-probe-multi-contract-v1",
        "CFG-05-prepare-local-config-panic-init-contract-v1",
        "S-13a-cancelled-shared-fetch-component-v1",
        "S-13c-shared-fetch-panic-recovery-component-v1",
    ] {
        let f = fixture(id);
        let row = amendments::review(&f).unwrap().unwrap();
        assert_eq!(row["originalExpected"], json!([f["expect"]]));
        let active = amendments::effective(&f).unwrap();
        assert_eq!(
            active["expectJava"],
            f.get("expectJava").unwrap_or(&f["expect"]).clone()
        );
        assert_ne!(active["expect"], f["expect"]);
        let mut changed = f.clone();
        changed["title"] = json!("changed");
        assert!(amendments::review(&changed).is_err());
    }
}
#[tokio::test]
async fn approved_components_and_service_contracts_execute_and_preserve_original_conflicts() {
    for id in [
        "C-03-completion-probe-multi-contract-v1",
        "CFG-05-prepare-local-config-panic-init-contract-v1",
        "S-13a-cancelled-shared-fetch-component-v1",
        "S-13c-shared-fetch-panic-recovery-component-v1",
    ] {
        let f = fixture(id);
        let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(r["execution"], "passed", "{id}: {}", r["assertionErrors"]);
        assert!(
            incremental::assert_observation(&f["expect"], &r["observations"][0]).is_err(),
            "original conflict lost: {id}"
        );
        if id.starts_with("S-13c") {
            let rec = &r["observations"][0]["recovery"];
            assert_eq!(rec["scopedPrincipalCachePublished"], false);
            assert_eq!(rec["jwksPublished"], true);
            assert_eq!(
                rec["reuse"],
                json!({"verifiedPrincipals":1,"taskStarts":0,"joinersAttached":0,"networkRequests":0,"residentKeyUnchanged":true})
            );
        }
    }
}
#[tokio::test]
async fn supported_zero_budget_is_service_execution_without_sdk_attempt() {
    let f = fixture("D-08-zero-remaining-supported-config-v1");
    let r = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
    assert_eq!(r["execution"], "passed", "{}", r["assertionErrors"]);
    assert_eq!(r["observations"][0]["outcome"], "response");
    assert_eq!(r["observations"][0]["status"], 504);
    assert_eq!(r["observations"][0]["backendCalls"], json!([]));
    assert_eq!(amendments::coverage(&r), "service-execution");
    let original = fixture("D-08-zero-remaining-multi-contract-v1");
    assert!(
        original["config"]["files"]["lambda-proxy"]
            .get("maxRetryAttempts")
            .is_some()
    );
    assert!(
        f["config"]["files"]["lambda-proxy"]
            .get("maxRetryAttempts")
            .is_none()
    );
    assert_eq!(f["expect"], original["expect"]);
    assert_eq!(f.get("expectJava"), original.get("expectJava"));
}

#[test]
fn principal_cache_absence_is_not_jwks_recovery_proof() {
    let f = fixture("S-13c-shared-fetch-panic-recovery-component-v1");
    let active = amendments::effective(&f).unwrap();
    let expected = &active["expect"];
    for (field, bad) in [
        ("jwksPublished", json!(false)),
        (
            "reuse",
            json!({"verifiedPrincipals":0,"taskStarts":1,"joinersAttached":0,"networkRequests":1,"residentKeyUnchanged":false}),
        ),
    ] {
        let mut actual = expected.clone();
        actual["recovery"][field] = bad;
        assert_eq!(actual["recovery"]["scopedPrincipalCachePublished"], false);
        assert!(
            incremental::assert_observation(expected, &actual).is_err(),
            "missing independent JWKS assertion: {field}"
        );
    }
}

#[test]
fn disposition_metadata_only_removes_equal_expected_fields() {
    for id in [
        "D-08-zero-remaining-supported-config-v1",
        "K-01-selected-dynamo-deferred-init-contract-v1",
    ] {
        let f = fixture(id);
        assert!(
            serverless_conformance::declarations::review(&f)
                .unwrap()
                .is_some()
        );
        let mut changed = f.clone();
        changed["expect"]["cacheActions"] = json!(["invented"]);
        assert!(serverless_conformance::declarations::review(&changed).is_err());
    }
}
