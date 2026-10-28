use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn security_observer_cannot_weaken_token_redaction_or_authorization() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let name = "v1/s/S-14-verified-jwt-no-secret-logs-multi-contract-v1.json";
    let admitted = root.join(format!("fixtures/{name}"));
    let draft = root.join(format!("drafts/{name}"));
    let mut f = load(if admitted.exists() { &admitted } else { &draft }).unwrap();
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    schema_validate(&schema, &f).unwrap();
    cross_validate(&f, &ledger).unwrap();
    f["steps"][0]["expect"]["securityLogObservation"]["fullTokenExposed"] = json!(true);
    assert!(cross_validate(&f, &ledger).is_err());
    f["steps"][0]["expect"]["securityLogObservation"]["fullTokenExposed"] = json!(false);
    f["steps"][0]["expectJava"]["securityLogObservation"]["sensitiveClaimExposed"] = json!(false);
    assert!(cross_validate(&f, &ledger).is_err());
    f["steps"][0]["expectJava"]["securityLogObservation"]["sensitiveClaimExposed"] = json!(true);
    f["steps"][0]["expect"]["securityLogObservation"]["unrestrictedMask"] = json!(true);
    assert!(schema_validate(&schema, &f).is_err());
}
