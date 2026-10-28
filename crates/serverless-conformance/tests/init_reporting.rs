use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn input_property_names_do_not_become_reporting_declarations() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let mut fixture =
        load(&root.join("fixtures/v1/cfg/CFG-05-missing-stage-init-report-contract-v1.json"))
            .unwrap();
    fixture["rawEvent"]["initFailureReporting"] = json!({"ordinary":"input"});
    assert!(cross_validate(&fixture, &ledger).is_ok());
}

#[test]
fn results_require_rust_and_one_whole_process_report_representation() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/result-v1.schema.json")).unwrap();
    let report =
        load(&root.join("fixtures/v1/cfg/CFG-05-missing-stage-init-report-contract-v1.json"))
            .unwrap()["expect"]
            .clone();
    let original = json!({"schema":"light-lambda-conformance/result/v1","id":"CFG-05-test","runner":"rust",
        "runnerRevision":"0000000000000000000000000000000000000000","fixtureDigest":"0000000000000000000000000000000000000000000000000000000000000000",
        "execution":"passed","durationMs":1,"observations":[report.clone()],"assertionErrors":[]});
    assert!(schema_validate(&schema, &original).is_ok());
    let mut invalid = original.clone();
    invalid["runner"] = json!("java");
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid["observations"] = json!([report.clone(), report.clone()]);
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original;
    invalid["initialization"] = report;
    assert!(schema_validate(&schema, &invalid).is_err());
}

#[test]
fn reporting_is_closed_to_cfg05_rust_init_failure() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/cfg/CFG-05-missing-stage-init-report-contract-v1.json"))
            .unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    for (pointer, value) in [
        ("/expect/initFailureReporting/reportCount", json!(2)),
        ("/expect/initFailureReporting/exitCode", json!(0)),
        (
            "/expect/initFailureReporting/endpoint",
            json!("/invocation/error"),
        ),
        ("/ledger", json!(["E-09"])),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(cross_validate(&invalid, &ledger).is_err(), "{pointer}");
    }
    let mut invalid = original;
    invalid["expectJava"]["initFailureReporting"] =
        invalid["expect"]["initFailureReporting"].clone();
    assert!(cross_validate(&invalid, &ledger).is_err());
    assert!(schema_validate(&schema, &invalid).is_err());
}

#[test]
fn complete_reporting_fields_reject_weakened_assertions() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/cfg/CFG-05-missing-stage-init-report-contract-v1.json"))
            .unwrap();
    for (pointer, value) in [
        (
            "/expect/initFailureReporting/header/Lambda-Runtime-Function-Error-Type",
            json!("wrong"),
        ),
        (
            "/expect/initFailureReporting/body/errorMessage",
            json!("configuration failed"),
        ),
        (
            "/expect/initFailureReporting/body/errorType",
            json!("Other"),
        ),
        ("/expect/initFailureReporting/maxBodyBytes", json!(4096)),
        (
            "/expect/initFailureReporting/forbiddenBodyValues",
            json!([]),
        ),
        (
            "/expect/initFailureReporting/invocationLoopEntered",
            json!(true),
        ),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(schema_validate(&schema, &invalid).is_err(), "{pointer}");
        assert!(cross_validate(&invalid, &ledger).is_err(), "{pointer}");
    }
}

#[test]
fn panic_probe_is_closed_to_real_init_preparation_contract() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("drafts/v1/cfg/CFG-05-prepare-local-config-panic-init-contract-v1.json"))
            .unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    for (pointer, value) in [
        ("/rustOnly", json!(false)),
        ("/initPanicProbe", json!("arbitrary-panic")),
        (
            "/expect/initFailureReporting/body/errorType",
            json!("InvalidConfiguration"),
        ),
    ] {
        let mut invalid = original.clone();
        *invalid.pointer_mut(pointer).unwrap() = value;
        assert!(schema_validate(&schema, &invalid).is_err());
        assert!(cross_validate(&invalid, &ledger).is_err());
    }
}
