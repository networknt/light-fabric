use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn loopback_jwks_infrastructure_is_closed_and_does_not_authorize_jks_for_rust() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    let original =
        load(&root.join("fixtures/v1/s/S-13b-active-refresh-join-multi-contract-v1.json")).unwrap();
    assert!(schema_validate(&schema, &original).is_ok());
    assert!(cross_validate(&original, &ledger).is_ok());
    for (key, value) in [
        ("verifyHostname", json!(false)),
        ("loadTrustStore", json!(true)),
        ("trustStore", json!("production.jks")),
        ("trustStorePass", json!("override")),
    ] {
        let mut invalid = original.clone();
        invalid["config"]["files"]["client"]["tls"][key] = value;
        assert!(cross_validate(&invalid, &ledger).is_err(), "{key}");
    }
    let mut invalid = original.clone();
    invalid["loopbackJwksPrerequisite"] = json!({"java": {"override": true}});
    assert!(schema_validate(&schema, &invalid).is_err());
    let mut invalid = original.clone();
    invalid["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("jwks");
    assert!(cross_validate(&invalid, &ledger).is_err());
    let mut invalid = original;
    invalid["config"]["files"]["security"]["enableVerifyJwt"] = json!(false);
    assert!(cross_validate(&invalid, &ledger).is_err());
}
