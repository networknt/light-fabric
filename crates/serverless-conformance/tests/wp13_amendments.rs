use serde_json::{Value, json};
use serverless_conformance::{amendments, fixture_files, load};
use std::path::PathBuf;

fn fixtures() -> Vec<Value> {
    fixture_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures"),
    )
    .unwrap()
    .iter()
    .map(|p| load(p).unwrap())
    .collect()
}

#[test]
fn eleven_exact_approved_amendments_preserve_originals_and_java() {
    let package: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v1.json"
    ))
    .unwrap();
    assert_eq!(package["proposals"].as_array().unwrap().len(), 11);
    for row in package["proposals"].as_array().unwrap() {
        let f = fixtures()
            .into_iter()
            .find(|f| f["id"] == row["id"])
            .unwrap();
        let original = f.clone();
        let effective = amendments::effective(&f).unwrap();
        assert_eq!(f, original);
        let cases = |v: &Value| {
            v["steps"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| vec![v.clone()])
        };
        let old = cases(&f);
        let new = cases(&effective);
        for (old, new) in old.iter().zip(&new) {
            assert_eq!(
                new["expectJava"],
                old.get("expectJava").unwrap_or(&old["expect"]).clone()
            );
        }
        let actual = if row["proposedVersionedRustExpectation"].is_array() {
            json!(new.iter().map(|s| s["expect"].clone()).collect::<Vec<_>>())
        } else {
            effective["expect"].clone()
        };
        assert_eq!(actual, row["proposedVersionedRustExpectation"]);
        let mut changed = f.clone();
        changed["title"] = json!("stale");
        assert!(amendments::effective(&changed).is_err());
    }
}

#[test]
fn admission_does_not_qualify_handler_execution() {
    assert_eq!(
        amendments::coverage(&json!({"observations":[{"outcome":"initError"}]})),
        "admission-only"
    );
    assert_eq!(
        amendments::coverage(
            &json!({"observations":[{"outcome":"response","body":json!({"code":"ERR14010"}).to_string()}]})
        ),
        "admission-only"
    );
    assert_eq!(
        amendments::coverage(
            &json!({"observations":[{"outcome":"response","status":429,"body":"limited"}]})
        ),
        "service-execution"
    );
}

#[tokio::test]
async fn registered_transformer_pair_recipe_uses_real_preparation_and_service() {
    for id in [
        "T-01-T-02-native-cel-body-multi-contract-v1",
        "T-02-response-header-remove-update-multi-contract-v1",
    ] {
        let f = fixtures().into_iter().find(|f| f["id"] == id).unwrap();
        assert!(serverless_conformance::incremental::runner_gaps(&f).is_empty());
        let result = serverless_conformance::incremental::run_fixture(&f, &"0".repeat(40))
            .await
            .unwrap();
        assert_eq!(
            result["execution"], "passed",
            "{id}: {}",
            result["assertionErrors"]
        );
        assert_eq!(amendments::coverage(&result), "service-execution");
        assert!(
            result["observations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| !o["backendCalls"].as_array().unwrap().is_empty())
        );
        let mut wrong = f.clone();
        wrong["ruleConfigurationPair"]["rust"]["executor"] = json!("invented");
        assert!(!serverless_conformance::incremental::runner_gaps(&wrong).is_empty());
    }
}

#[test]
fn empty_http_probe_is_not_a_transport_difference() {
    let expectation = json!({"outcome":"initError","code":"InvalidConfiguration","backendCalls":[],"jwksFetches":0});
    let f = json!({"nondeterministic":[],"javaOnly":false,"rustOnly":false,"differences":[],"expect":expectation});
    let java = json!({"execution":"passed","observations":[expectation]});
    let mut rust = java.clone();
    rust["observations"][0]["httpCalls"] = json!([]);
    let mut effective = f.clone();
    effective["expect"]["httpCalls"] = json!([]);
    effective["expectJava"] = f["expect"].clone();
    serverless_conformance::incremental::compare(&effective, &java, &rust).unwrap();
    rust["observations"][0]["httpCalls"] = json!([{"target":"http://127.0.0.1:1/unexpected"}]);
    assert!(serverless_conformance::incremental::compare(&effective, &java, &rust).is_err());
}
