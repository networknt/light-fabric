use serde_json::{Value, json};
use serverless_core::unsupported::metrics::validate_configuration;
#[test]
fn all_metrics_keys_have_explicit_strict_redacted_dispositions_even_when_disabled() {
    let defaults = json!({"enabled":true,"enableJVMMonitor":false,"serverProtocol":"http","serverHost":"localhost","serverPath":"/apm/metricFeed","serverPort":8086,"serverName":"metrics","serverUser":"admin","serverPass":"admin","reportInMinutes":1,"productName":"http-sidecar","sendScopeClientId":false,"sendCallerId":false,"sendIssuer":false,"issuerRegex":""});
    assert_eq!(validate_configuration(&defaults).unwrap().len(), 14);
    assert_eq!(validate_configuration(&json!({})).unwrap().len(), 14);
    for key in defaults
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| key.as_str() != "enabled")
    {
        let mut settings = defaults.clone();
        settings["enabled"] = json!(false);
        settings[key] = json!("synthetic-secret-must-not-leak");
        let err = validate_configuration(&settings).unwrap_err();
        assert!(!format!("{err:?}").contains("synthetic-secret-must-not-leak"));
    }
    for invalid in [
        json!({"enabled":"true"}),
        json!({"enabled":null}),
        json!({"unknownOption":false}),
        Value::Null,
        json!([]),
    ] {
        assert!(validate_configuration(&invalid).is_err());
    }
}
