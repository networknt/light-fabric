use serde_json::json;
use serverless_conformance::incremental;

#[test]
fn wired_validator_factory_does_not_claim_application_activation_or_fixture_support() {
    let class = serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == serverless_core::HandlerId::Validator)
        .unwrap()
        .class;
    let fixture =
        json!({"config":{"files":{"handler":{"handlers":[format!("{class}@validator")]}}}});
    let support = incremental::handler_support(&fixture);
    assert_eq!(support["registered"], json!([]));
    assert_eq!(support["unregistered"], json!(["validator"]));
    assert_eq!(
        support["factoryWiredActivationClosed"],
        json!(["validator"])
    );
}
