//! Admission impact only; this is not a complete-request Validator adapter.
#[path = "profile_support/validator.rs"]
mod validator;
use serde_json::{Value, json};
#[test]
fn valid_java_requests_outside_runtime_profile_are_distinct_from_schema_violations() {
    let cases: Value = serde_json::from_str(include_str!(
        "data/wp12-validator-runtime-restrictions.json"
    ))
    .unwrap();
    for case in cases.as_array().unwrap() {
        assert_eq!(case["java"]["status"], 200);
        let candidate = validator::Candidate::compile(&case["schema"], &json!({}), true).unwrap();
        assert!(
            candidate.validate(&case["value"]).is_err(),
            "{}",
            case["id"]
        );
    }
    assert_eq!(cases.as_array().unwrap().len(), 9);
}
#[test]
fn actual_wp2_request_schemas_and_paired_dialects_record_candidate_admission() {
    let cases: Value =
        serde_json::from_str(include_str!("data/wp12-validator-compatibility.json")).unwrap();
    let mut rows = Vec::new();
    let features: Value =
        serde_json::from_str(include_str!("data/wp12-validator-feature-impact.json")).unwrap();
    for case in cases
        .as_array()
        .unwrap()
        .iter()
        .chain(features.as_array().unwrap())
    {
        let result = validator::Candidate::compile(
            &case["schema"],
            &case["resources"],
            case["oas30"] == true,
        );
        let admitted = result.is_ok();
        if let Ok(candidate) = result {
            // Exercise bounded instance admission and deny retrieval as well as
            // compilation. No operation/request-validation claim follows.
            let _valid = candidate.validate_bytes(b"{}").unwrap();
            assert_eq!(
                candidate
                    .retrievals
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        }
        rows.push(json!({"id":case["id"],"location":case["location"],"admitted":admitted}));
    }
    assert_eq!(rows.len(), 66);
    assert!(
        rows.iter()
            .filter(|r| r["id"].as_str().unwrap().starts_with("synthetic-regex-"))
            .all(|r| r["admitted"] == false)
    );
    assert!(rows.iter().any(|r| r["admitted"] == false));
    println!("VALIDATOR_ADMISSION {}", json!(rows));
}
