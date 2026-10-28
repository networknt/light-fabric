//! The two owner-dispositioned mask keys and explicit responseBody Init contract.
use crate::{InitError, unsupported::strings};
use serde_json::{Value, json};
pub const RESPONSE_BODY_ERROR: &str = "audit.yml: responseBody selector is unsupported";
/// Local configuration parsing only; no clock or environment acquisition.
pub fn timestamp_format(
    value: &Value,
) -> Result<Option<gateway_policy::audit_timestamp::AuditTimestamp>, InitError> {
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("audit.yml: object required"))?;
    match map.get("timestampFormat") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(pattern)) if pattern.is_empty() => Ok(None),
        Some(Value::String(pattern)) => {
            gateway_policy::audit_timestamp::AuditTimestamp::parse(pattern)
                .map(Some)
                .map_err(InitError::Malformed)
        }
        _ => Err(InitError::Malformed("audit.yml: invalid timestamp format")),
    }
}
pub fn reject_response_body_selection(value: &Value) -> Result<(), InitError> {
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("audit.yml: object required"))?;
    let fields = strings(map.get("audit"))
        .map_err(|_| InitError::Malformed("audit.yml: invalid selector list"))?;
    if fields.iter().any(|field| field == "responseBody") {
        return Err(InitError::Malformed(RESPONSE_BODY_ERROR));
    }
    Ok(())
}
fn map_setting(value: Option<&Value>) -> Result<Option<Value>, InitError> {
    let value = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
        Some(Value::String(s)) => serde_json::from_str(s)
            .map_err(|_| InitError::Malformed("mask.yml: malformed map setting"))?,
        Some(value) => value.clone(),
    };
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("mask.yml: map required"))?;
    for fields in map.values() {
        let fields = fields
            .as_object()
            .ok_or(InitError::Malformed("mask.yml: field map required"))?;
        if fields.values().any(|v| !v.is_string()) {
            return Err(InitError::Malformed("mask.yml: string rule required"));
        }
    }
    Ok(Some(value))
}
pub fn validate_mask(value: &Value) -> Result<Vec<String>, InitError> {
    let map = value
        .as_object()
        .ok_or(InitError::Malformed("mask.yml: object required"))?;
    if map
        .keys()
        .any(|key| !["string", "json", "regex"].contains(&key.as_str()))
    {
        return Err(InitError::Malformed("mask.yml: unknown key"));
    }
    let string = map_setting(map.get("string"))?;
    if string.is_some_and(|v| v != json!({"uri":{"password=[^&]*":"password=******"}})) {
        return Err(InitError::Malformed(
            "mask.yml: mask.string unsupported non-default setting",
        ));
    }
    if map_setting(map.get("json"))?.is_some() {
        return Err(InitError::Malformed(
            "mask.yml: mask.json unsupported non-default setting",
        ));
    }
    let regex = map_setting(map.get("regex"))?.unwrap_or(Value::Null);
    gateway_policy::audit_mask::AuditMask::from_regex(&regex).map_err(InitError::Malformed)?;
    Ok(vec![
        "mask.string: default/absent unsupported Lambda setting".into(),
        "mask.json: default/absent unsupported Lambda setting".into(),
    ])
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_two_mask_keys_are_deferred_and_defaults_are_exact_and_redacted() {
        for value in [
            json!({}),
            json!({"string":null,"json":""}),
            json!({"string":{"uri":{"password=[^&]*":"password=******"}},"regex":{"requestHeader":{"x-test-header":"id=(.+)"}}}),
        ] {
            assert_eq!(validate_mask(&value).unwrap().len(), 2);
        }
        for value in [
            json!({"string":{}}),
            json!({"json":{}}),
            json!({"string":{"requestBody":{"secret":"secret-must-not-leak"}}}),
            json!({"json":{"responseBody":{"secret":"secret-must-not-leak"}}}),
            json!({"string":false}),
            json!({"json":false}),
            json!({"string":{"uri":{"pattern":false}}}),
            json!({"regex":{"requestHeader":{"x-test-header":false}}}),
        ] {
            let err = validate_mask(&value).unwrap_err();
            assert!(!format!("{err:?}").contains("secret-must-not-leak"));
        }
    }
    #[test]
    fn amendment_rejects_response_body_only_without_broadening_this_preflight() {
        for fields in [
            json!(["responseBody"]),
            json!("responseBody"),
            json!("[\"responseBody\"]"),
        ] {
            assert!(matches!(
                reject_response_body_selection(&json!({"enabled":false,"audit":fields})),
                Err(InitError::Malformed(RESPONSE_BODY_ERROR))
            ));
        }
        for fields in [
            json!(["requestBody"]),
            json!(["queryParameters", "pathParameters"]),
            json!(["unqualified-selector"]),
        ] {
            assert!(reject_response_body_selection(&json!({"audit":fields})).is_ok());
        }
    }
}
