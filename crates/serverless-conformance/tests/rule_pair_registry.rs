use serde_json::json;
use serverless_conformance::{cross_validate, ledger_rows, load, schema_validate};
use std::{fs, path::PathBuf};

#[test]
fn native_cel_registry_is_closed_and_matches_java_resources() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/serverless-conformance");
    let java = root.join("../../../light-lambda-native-conformance/src/test/resources/conformance");
    let schema = load(&root.join("schema/fixture-v1.schema.json")).unwrap();
    let ledger = ledger_rows(&fs::read_to_string(root.join("LEDGER.md")).unwrap()).unwrap();
    for name in ["metadata", "local", "deny"] {
        let path = format!("v1/t/T-01-{name}-native-cel-multi-contract-v1.json");
        let admitted = root.join(format!("fixtures/{path}"));
        let draft = root.join(format!("drafts/{path}"));
        let f = load(if admitted.exists() { &admitted } else { &draft }).unwrap();
        assert!(schema_validate(&schema, &f).is_ok());
        assert!(cross_validate(&f, &ledger).is_ok());
        // A local sibling worktree is available in WP3 qualification. Avoid
        // imposing that filesystem layout on future independent crate users.
        let resource = java.join(format!("rule-configuration-pair-{name}-v1.json"));
        if resource.exists() {
            assert_eq!(load(&resource).unwrap(), f["ruleConfigurationPair"]);
        }
        let mut invalid = f;
        invalid["ruleConfigurationPair"]["rust"]["rule"]["ruleBodies"]["test-edge"]["actions"][0]
            ["actionRef"] = json!("unreviewed-action");
        assert!(schema_validate(&schema, &invalid).is_err());
    }
}
