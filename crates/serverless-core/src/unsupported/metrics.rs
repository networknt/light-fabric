//! Owner-approved five-key disposition preserving the closed M-01 EMF shape.
use crate::InitError;
use serde_json::{Value, json};

pub fn validate_configuration(value: &Value) -> Result<Vec<String>, InitError> {
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("metrics.yml: object required"))?;
    let defaults = [
        ("enableJVMMonitor", json!(false)),
        ("serverProtocol", json!("http")),
        ("serverHost", json!("localhost")),
        ("serverPath", json!("/apm/metricFeed")),
        ("serverPort", json!(8086)),
        ("serverName", json!("metrics")),
        ("serverUser", json!("admin")),
        ("serverPass", json!("admin")),
        ("reportInMinutes", json!(1)),
    ];
    if map.keys().any(|key| {
        ![
            "enabled",
            "productName",
            "sendScopeClientId",
            "sendCallerId",
            "sendIssuer",
            "issuerRegex",
        ]
        .contains(&key.as_str())
            && !defaults.iter().any(|(name, _)| name == key)
    }) {
        return Err(InitError::Malformed("metrics.yml: unknown key"));
    }
    if map.get("enabled").is_some_and(|v| !v.is_boolean()) {
        return Err(InitError::Malformed("metrics.yml: boolean required"));
    }
    let mut warnings = validate_emf_label_options(value)?;
    for (key, default) in defaults {
        if map.get(key).is_some_and(|v| *v != default) {
            return Err(InitError::Malformed(
                "metrics.yml: deferred sink option must retain default",
            ));
        }
        warnings.push(format!(
            "metrics.{key}: default/absent unsupported APM sink setting; local EMF only"
        ));
    }
    Ok(warnings)
}

pub fn validate_emf_label_options(value: &Value) -> Result<Vec<String>, InitError> {
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("metrics.yml: malformed configuration"))?;
    let mut warnings = Vec::new();
    for (key, default) in [
        ("productName", json!("http-sidecar")),
        ("sendScopeClientId", json!(false)),
        ("sendCallerId", json!(false)),
        ("sendIssuer", json!(false)),
    ] {
        if map.get(key).is_some_and(|value| *value != default) {
            return Err(InitError::Malformed(
                "metrics.yml: unsupported non-default APM label setting",
            ));
        }
        warnings.push(format!("metrics.{key}: default/absent unsupported APM label setting; closed Lambda EMF contract"));
    }
    if map
        .get("issuerRegex")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
    {
        return Err(InitError::Malformed(
            "metrics.yml: unsupported non-default APM label setting",
        ));
    }
    warnings.push("metrics.issuerRegex: default/absent unsupported APM label setting; closed Lambda EMF contract".into());
    Ok(warnings)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approved_five_keys_accept_only_defaults_and_never_expose_values() {
        for value in [
            json!({}),
            json!({"productName":"http-sidecar","sendScopeClientId":false,"sendCallerId":false,"sendIssuer":false,"issuerRegex":""}),
        ] {
            assert_eq!(validate_emf_label_options(&value).unwrap().len(), 5);
        }
        for (key, value) in [
            ("productName", json!("secret-must-not-leak")),
            ("sendScopeClientId", json!(true)),
            ("sendCallerId", json!(true)),
            ("sendIssuer", json!(true)),
            ("issuerRegex", json!("secret-must-not-leak")),
            ("sendIssuer", json!("false")),
        ] {
            let mut config = serde_json::Map::new();
            config.insert(key.into(), value);
            let error = validate_emf_label_options(&Value::Object(config)).unwrap_err();
            assert!(!format!("{error:?}").contains("secret-must-not-leak"));
        }
    }
}
