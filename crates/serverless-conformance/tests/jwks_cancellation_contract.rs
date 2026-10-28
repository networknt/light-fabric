use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn cancellation_contract_cannot_be_an_http_failure_or_hidden_request() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let name = "v1/s/S-13a-cancelled-shared-fetch-component-v1.json";
    let admitted = root.join(format!("fixtures/{name}"));
    let draft = root.join(format!("drafts/{name}"));
    let mut f = load(if admitted.exists() { &admitted } else { &draft }).unwrap();
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    schema_validate(&schema, &f).unwrap();
    cross_validate(&f, &ledger).unwrap();
    f["expect"]["gate"]["fetchCancelled"] = json!(0);
    assert!(schema_validate(&schema, &f).is_err());
    f["expect"]["gate"]["fetchCancelled"] = json!(1);
    f["dependencies"]["jwks"]["delayMs"] = json!(0);
    assert!(cross_validate(&f, &ledger).is_err());
    f["dependencies"]["jwks"]["delayMs"] = json!(500);
    f["rustOnly"] = json!(false);
    assert!(cross_validate(&f, &ledger).is_err());
}
