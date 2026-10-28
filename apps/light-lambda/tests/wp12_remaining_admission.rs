use async_trait::async_trait;
use light_lambda_runtime::init::{BackendConfiguration, initialize, validate_configuration};
use serde_json::{Value, json};
use serverless_core::*;
use std::sync::Arc;
struct NoDispatch;
#[async_trait]
impl Backend for NoDispatch {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        panic!("Init must not dispatch")
    }
}
async fn check(file: &str, value: Value, expected: bool) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("values.yml"),json!({"light-lambda.stage":"test-only","lambda-proxy.region":"us-east-1","security.enabled":false,"security.enableVerifyJwt":false,"security.enableVerifyScope":false,"unified-security.enabled":false}).to_string()).unwrap();
    std::fs::write(dir.path().join(file), value.to_string()).unwrap();
    let input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    let report = validate_configuration(&input).await;
    assert_eq!(report.valid, expected, "{file}: {:?}", report.errors);
    let mut startup = light_lambda::input(Some(dir.path().into()), false).unwrap();
    startup.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
    assert_eq!(
        initialize(startup).await.is_ok(),
        expected,
        "startup {file}"
    );
}
#[tokio::test]
async fn disabled_router_rejects_unimplemented_metric_injection_nondefaults() {
    check(
        "router.yml",
        json!({"enabled":false,"metricsInjection":false,"metricsName":"router-response"}),
        true,
    )
    .await;
    check(
        "router.yml",
        json!({"enabled":false,"metricsInjection":true}),
        false,
    )
    .await;
    check(
        "router.yml",
        json!({"enabled":false,"metricsName":"custom"}),
        false,
    )
    .await;
}
#[tokio::test]
async fn unselected_rules_reject_class_plugins_unbounded_actions_and_unknown_declarations() {
    let valid = json!({"endpointRules":{"/x@post":{"req-tra":["r"]}},"ruleBodies":{"r":{"ruleId":"r","ruleName":"owned","ruleType":"req-tra","common":"Y","conditionLanguage":"cel","conditionSecurityProfile":"standard","expression":"true","actions":[{"actionId":"a","actionRef":"request-body-literal","actionValues":{"body":"fixed"}}]}}});
    check("rule.yml", valid.clone(), true).await;
    for (action, values) in [
        (
            "request-metadata-literal",
            json!({"queryString":"not-json"}),
        ),
        (
            "request-metadata-literal",
            json!({"requestHeaders":{"update":{"invalid\nname":"value"}}}),
        ),
        (
            "request-validation-error-literal",
            json!({"validationError":true,"errorMessage":"not-json"}),
        ),
    ] {
        let mut malformed = valid.clone();
        malformed["ruleBodies"]["r"]["actions"][0]["actionRef"] = json!(action);
        malformed["ruleBodies"]["r"]["actions"][0]["actionValues"] = values;
        check("rule.yml", malformed, false).await;
    }
    let mut plugin = valid.clone();
    plugin["ruleBodies"]["r"]["actions"][0]["actionClassName"] = json!("com.example.SecretPlugin");
    check("rule.yml", plugin, false).await;
    let mut unbounded = valid.clone();
    unbounded["ruleBodies"]["r"]["actions"][0]["actionRef"] = json!("unbounded-network-call");
    check("rule.yml", unbounded, false).await;
    let mut unknown = valid.clone();
    unknown["unknownSetting"] = json!(true);
    check("rule.yml", unknown, false).await;
    let mut unknown = valid.clone();
    unknown["ruleBodies"]["r"]["unknownSetting"] = json!(true);
    check("rule.yml", unknown, false).await;
    let mut unknown_value = valid.clone();
    unknown_value["ruleBodies"]["r"]["actions"][0]["actionValues"]["unknownSetting"] = json!(true);
    check("rule.yml", unknown_value, false).await;
    let mut malformed = valid;
    malformed["ruleBodies"]["r"]["actions"] = json!("not-actions");
    check("rule.yml", malformed, false).await;
}
#[tokio::test]
async fn disabled_unselected_validator_rejects_malformed_unknown_and_nondefault_unsupported() {
    for (key, value) in [
        ("logError", json!(false)),
        ("legacyPathType", json!(true)),
        ("validateResponse", json!(true)),
        ("skipBodyValidation", json!("not-a-boolean")),
        ("handleNullableField", json!(null)),
        ("skipPathPrefixes", json!([7])),
        ("skipPathPrefixes", json!(["relative"])),
        ("unknownSetting", json!(true)),
    ] {
        let mut config = json!({"enabled":false});
        config[key] = value;
        check("openapi-validator.yml", config, false).await;
    }
    check("openapi-validator.yml",json!({"enabled":false,"logError":true,"legacyPathType":false,"validateResponse":false,"skipBodyValidation":true,"handleNullableField":false,"skipPathPrefixes":["/skip"]}),true).await;
}
#[tokio::test]
async fn disabled_sanitizer_validates_all_settings_and_does_not_admit_unknown_keys() {
    for (key, value) in [
        ("unknownSetting", json!(true)),
        ("bodyEnabled", json!("true")),
        ("headerEnabled", json!(null)),
        ("bodyEncoder", json!("not-an-encoder")),
        ("headerEncoder", json!(false)),
        ("bodyAttributesToEncode", json!([true])),
        ("headerAttributesToIgnore", json!({})),
    ] {
        let mut config = json!({"enabled":false});
        config[key] = value;
        check("sanitizer.yml", config, false).await;
    }
    check("sanitizer.yml",json!({"enabled":false,"bodyEnabled":true,"headerEnabled":true,"bodyEncoder":"html","headerEncoder":"javascript-source","bodyAttributesToEncode":["value"],"bodyAttributesToIgnore":["ignore"],"headerAttributesToEncode":"[\"x-test\"]","headerAttributesToIgnore":null}),true).await;
}
#[tokio::test]
async fn inactive_health_and_transformers_still_enforce_unsupported_defaults() {
    check(
        "health.yml",
        json!({"enabled":false,"downstreamEnabled":true}),
        false,
    )
    .await;
    for file in ["request-transformer.yml", "response-transformer.yml"] {
        check(
            file,
            json!({"enabled":false,"requiredContent":false}),
            false,
        )
        .await;
        check(
            file,
            json!({"enabled":false,"appliedPathPrefixes":[9]}),
            false,
        )
        .await;
        check(file,json!({"enabled":false,"requiredContent":true,"defaultBodyEncoding":"UTF-8","pathPrefixEncoding":{},"appliedPathPrefixes":["/x"]}),true).await;
    }
}

#[tokio::test]
async fn values_only_status_flags_reach_embedded_admission_without_selection() {
    let dir = tempfile::tempdir().unwrap();
    for key in ["showMetadata", "showMessage", "showDescription"] {
        for value in [json!(true), json!(false), json!("not-a-boolean")] {
            let mut values = json!({"light-lambda.stage":"test-only","lambda-proxy.region":"us-east-1","security.enabled":false,"security.enableVerifyJwt":false,"security.enableVerifyScope":false,"unified-security.enabled":false});
            values[format!("status.{key}")] = value.clone();
            std::fs::write(dir.path().join("values.yml"), values.to_string()).unwrap();
            let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
            input.backend = BackendConfiguration::Injected(Arc::new(NoDispatch));
            let report = validate_configuration(&input).await;
            assert_eq!(report.valid, value == true, "{key}: {:?}", report.errors);
            assert_eq!(initialize(input).await.is_ok(), value == true);
        }
    }
}
