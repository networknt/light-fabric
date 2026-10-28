use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance")
}

#[test]
fn closed_fault_contracts_and_negative_controls() {
    let root = root();
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let names = [
        "cfg/CFG-05-runtime-api-unset-v1-contract-v1",
        "cfg/CFG-05-loopback-connection-refused-v1-contract-v1",
        "cfg/CFG-05-loopback-http-503-v1-contract-v1",
        "cfg/CFG-05-loopback-response-withheld-v1-contract-v1",
        "s/S-13c-shared-fetch-panic-recovery-component-v1",
    ];
    for name in names {
        let f = load(&root.join(format!("drafts/v1/{name}.json"))).unwrap();
        assert!(schema_validate(&schema, &f).is_ok(), "{name}");
        assert!(cross_validate(&f, &ledger).is_ok(), "{name}");
        let mut bad = f.clone();
        bad["rustOnly"] = json!(false);
        assert!(schema_validate(&schema, &bad).is_err());
        assert!(cross_validate(&bad, &ledger).is_err());
        let mut bad = f.clone();
        bad["ledger"] = json!(["E-13"]);
        assert!(schema_validate(&schema, &bad).is_err());
        assert!(cross_validate(&bad, &ledger).is_err());
        let mut bad = f.clone();
        bad["expect"]["unexpected"] = json!(true);
        assert!(schema_validate(&schema, &bad).is_err());
        assert!(cross_validate(&bad, &ledger).is_err());
        let mut bad = f.clone();
        if name.starts_with("cfg/") {
            bad["expect"]["reporting"]["receivedPosts"] = json!(9);
        } else {
            bad["expect"]["failure"]["verifiedPrincipals"] = json!(1);
        }
        assert!(schema_validate(&schema, &bad).is_err());
        assert!(cross_validate(&bad, &ledger).is_err());
    }
}

#[test]
fn faults_reject_changed_trigger_prerequisites() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let mut f =
        load(&root.join("drafts/v1/cfg/CFG-05-loopback-http-503-v1-contract-v1.json")).unwrap();
    f["config"]["values"]["light-lambda.stage"] = json!("valid");
    assert!(cross_validate(&f, &ledger).is_err());
    let mut f = load(&root.join("drafts/v1/s/S-13c-shared-fetch-panic-recovery-component-v1.json"))
        .unwrap();
    f["dependencies"]["jwks"]["publishedKeys"] = json!(["k1"]);
    assert!(cross_validate(&f, &ledger).is_err());
}

#[test]
fn result_schema_requires_actual_fault_evidence() {
    let root = root();
    let schema = load(&root.join("schema/result-v1.schema.json")).unwrap();
    let f =
        load(&root.join("drafts/v1/cfg/CFG-05-loopback-connection-refused-v1-contract-v1.json"))
            .unwrap();
    let mut result = json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":"0000000000000000000000000000000000000000","fixtureDigest":"0000000000000000000000000000000000000000000000000000000000000000","execution":"passed","durationMs":1,"observations":[f["expect"].clone()],"assertionErrors":[],"faultEvidence":{"diagnosticLines":["synthetic schema test only"],"elapsedAfterInitFailureMs":1}});
    assert!(schema_validate(&schema, &result).is_ok());
    result["faultEvidence"]["elapsedAfterInitFailureMs"] = json!(3001);
    assert!(schema_validate(&schema, &result).is_err());
    result["faultEvidence"]["elapsedAfterInitFailureMs"] = json!(1);
    result["runner"] = json!("java");
    assert!(schema_validate(&schema, &result).is_err());
    result["runner"] = json!("rust");
    result.as_object_mut().unwrap().remove("faultEvidence");
    assert!(schema_validate(&schema, &result).is_err());
}

#[test]
fn paired_dynamo_rejection_requires_real_declared_manager() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("drafts/v1/k/K-01-selected-dynamo-deferred-init-contract-v1.json"))
            .unwrap();
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut invalid = original.clone();
    invalid["config"]["files"]["service"] = json!({});
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["expect"]["outcome"] = json!("response");
    assert!(cross_validate(&invalid, &ledger).is_err());
}

#[test]
fn ingress_diagnostic_counter_is_warm_and_closed() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("drafts/v1/e/E-13-warm-single-value-config-log-once-contract-v1.json"))
            .unwrap();
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut f = original.clone();
    f["steps"][1]["expect"]["configurationDiagnostics"]["multiValueRequiredLogs"] = json!(1);
    assert!(cross_validate(&f, &ledger).is_err());
    let mut f = original;
    f["ledger"] = json!(["S-01"]);
    assert!(cross_validate(&f, &ledger).is_err());
}

#[test]
fn empty_business_maps_are_null_in_rust_but_java_is_preserved() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original = load(&root.join("fixtures/v1/r/R-01-literal-multi-v1.json")).unwrap();
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut f = original.clone();
    f["expect"]["backendCalls"][0]["eventExact"]["headers"] = json!({});
    assert!(cross_validate(&f, &ledger).is_err());
    let mut f = original;
    f["expectJava"]["backendCalls"][0]["eventExact"]["headers"] = json!({});
    f["expectJava"]["backendCalls"][0]["eventAbsent"] = json!([
        "/body",
        "/queryStringParameters",
        "/pathParameters",
        "/requestContext/stage"
    ]);
    assert!(cross_validate(&f, &ledger).is_ok());
}

#[test]
fn warning_contract_preserves_default_absent_nondefault_distinction() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original = load(
        &root.join("drafts/v1/cfg/CFG-04-handler-duration-default-false-init-contract-v1.json"),
    )
    .unwrap();
    assert!(cross_validate(&original, &ledger).is_ok());
    let mut f = original.clone();
    f["expect"]["configurationWarningObservation"]["warningCount"] = json!(0);
    assert!(cross_validate(&f, &ledger).is_err());
    let mut f = original;
    f["config"]["files"]["handler"]["reportHandlerDuration"] = json!(true);
    assert!(cross_validate(&f, &ledger).is_err());
}

#[test]
fn target_pair_is_exact_not_an_extra_call_allowance() {
    let root = root();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let mut f = load(&root.join("fixtures/v1/r/R-01-literal-multi-v1.json")).unwrap();
    f["expectJava"]["backendCalls"][0]["target"] = json!("FnOther");
    f["backend"][0]["match"] = json!({"targets":["FnLiteral","FnOther"]});
    assert!(schema_validate(&schema, &f).is_ok());
    assert!(cross_validate(&f, &ledger).is_ok());
    let mut bad = f.clone();
    bad["backend"][0]["match"]["targets"][1] = json!("FnUndeclared");
    assert!(cross_validate(&bad, &ledger).is_err());
    let mut bad = f.clone();
    bad["backend"][0]["match"]["targets"] = json!(["FnLiteral", "FnOther", "FnUndeclared"]);
    assert!(schema_validate(&schema, &bad).is_err());
    assert!(cross_validate(&bad, &ledger).is_err());
    let mut bad = f;
    bad["expectJava"]["backendCalls"] = json!([]);
    assert!(cross_validate(&bad, &ledger).is_err());
}
