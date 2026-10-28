use serde_json::{Value, json};
use serverless_conformance::{fixture_files, gap_work, incremental, load};
use std::path::PathBuf;
fn fixture(id: &str) -> Value {
    fixture_files(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/serverless-conformance/fixtures"),
    )
    .unwrap()
    .iter()
    .map(|p| load(p).unwrap())
    .find(|f| f["id"] == id)
    .unwrap()
}
#[test]
fn all_fifty_one_work_classifications_are_digest_bound_and_disjoint() {
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-gap-work-v1.json"
    ))
    .unwrap();
    let mut categories = std::collections::BTreeMap::<String, usize>::new();
    for row in metadata["rows"].as_array().unwrap() {
        let f = fixture(row["id"].as_str().unwrap());
        assert_eq!(gap_work::review(&f).unwrap().unwrap(), *row);
        *categories
            .entry(row["category"].as_str().unwrap().into())
            .or_default() += 1;
        let mut changed = f.clone();
        changed["title"] = json!("changed");
        assert!(gap_work::review(&changed).is_err());
        if row["category"] == "explicit-deferral" {
            assert_ne!(row["authority"], Value::Null);
        }
    }
    assert_eq!(metadata["rows"].as_array().unwrap().len(), 51);
    assert_eq!(
        json!(categories),
        json!({"adapter-only":2,"explicit-deferral":45,"router-failed-state":1,"validator-integration":3})
    );
}
#[test]
fn stream_counterpart_comparison_rejects_native_output_and_skip_contract_errors() {
    let id = "C-07-valid-stream-no-output-component-v1";
    let f = fixture(id);
    assert!(incremental::runner_gaps(&f).is_empty());
    let java = json!({"execution":"passed","observations":[f["expect"]]});
    let rust = json!({"execution":"skipped","reason":"Java-only component","observations":[],"assertionErrors":[]});
    incremental::compare(&f, &java, &rust).unwrap();
    for (field, bad) in [
        ("chainExecutions", json!(0)),
        ("outputBytes", json!(1)),
        ("outputBase64", json!("eA==")),
    ] {
        let mut changed = java.clone();
        changed["observations"][0][field] = bad;
        assert!(
            incremental::compare(&f, &changed, &rust).is_err(),
            "{field}"
        );
    }
    let mut changed = rust.clone();
    changed["reason"] = json!("unimplemented");
    assert!(incremental::compare(&f, &java, &changed).is_err());
    let mut f = f;
    f["javaOnly"] = json!(false);
    assert!(!incremental::runner_gaps(&f).is_empty());
}
#[tokio::test]
async fn admitted_java_only_recipes_emit_no_rust_service_or_expectation_pass() {
    for id in [
        "C-07-malformed-stream-fallback-component-v1",
        "C-07-valid-stream-no-output-component-v1",
        "M-03-influx-null-status-cp1-e02-accepted",
    ] {
        let f = fixture(id);
        let result = incremental::run_fixture(&f, &"0".repeat(40)).await.unwrap();
        assert_eq!(result["execution"], "skipped");
        assert_eq!(result["reason"], "Java-only component");
        assert_eq!(result["observations"], json!([]));
    }
}
#[test]
fn deferred_influx_native_assertions_keep_calls_and_backend_response_exact() {
    let f = fixture("M-03-influx-null-status-cp1-e02-accepted");
    let mut observation = f["expect"].clone();
    observation["backendCalls"][0]["invoke"] = json!({"logType":"Tail","invocationType":null});
    let java = json!({"execution":"passed","observations":[observation]});
    let rust = json!({"execution":"skipped","reason":"Java-only component","observations":[],"assertionErrors":[]});
    incremental::compare(&f, &java, &rust).unwrap();
    for (field, bad) in [
        ("backendCalls", json!([])),
        ("body", json!("changed")),
        ("status", json!(500)),
    ] {
        let mut changed = java.clone();
        changed["observations"][0][field] = bad;
        assert!(
            incremental::compare(&f, &changed, &rust).is_err(),
            "{field}"
        );
    }
}

#[test]
fn executable_reports_native_only_completion_without_inventing_rust_execution() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("results");
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_serverless-conformance"))
        .args([
            "run",
            "--fixture",
            "C-07-malformed-stream-fallback-component-v1",
            "--fixture",
            "C-07-valid-stream-no-output-component-v1",
            "--fixture",
            "M-03-influx-null-status-cp1-e02-accepted",
            "--out",
        ])
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let coverage = load(&out.join("coverage.json")).unwrap();
    assert_eq!(coverage["expectationPasses"], 0);
    assert_eq!(coverage["singleRunnerPasses"], 3);
    assert_eq!(
        coverage["rustExecutionCategories"],
        json!({"service":0,"admission":0,"components":{},"componentTotal":0,"expectationTotal":0,"javaOnlyCounterpartSkips":3,"meaning":"Components are separate from service/admission; paired components contribute to differential passes, Rust-only components and counterpart skips use single-runner reporting"})
    );
    assert_eq!(coverage["reconciledGapCohort"]["present"], 51);
    assert_eq!(
        coverage["reconciledGapCohort"]["remainingAdapterGapsByCategory"]["adapter-only"],
        serde_json::Value::Null
    );
    let influx = coverage["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == "M-03-influx-null-status-cp1-e02-accepted")
        .unwrap();
    assert_eq!(influx["handlerSupport"]["deferred"], json!(["metrics"]));
    assert_eq!(influx["rustExecution"], "skipped");
    assert_eq!(influx["coverageKind"], "single-runner-java-component");
}
