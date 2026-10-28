//! Pure admission for the remaining WP12 policies, including inactive inputs.
use crate::{
    InitError,
    unsupported::{boolean, object, strings},
};
use serde_json::{Value, json};

fn invalid() -> InitError {
    InitError::Malformed(
        "policy configuration: unknown, malformed or unsupported nondefault setting",
    )
}
fn defaults(
    value: &Value,
    supported: &[&str],
    defaults: &[(&str, Value)],
    prefix: &str,
) -> Result<Vec<String>, InitError> {
    let map = object(value, "policy configuration: object required")?;
    if map
        .keys()
        .any(|k| !supported.contains(&k.as_str()) && !defaults.iter().any(|(name, _)| *name == k))
    {
        return Err(invalid());
    }
    let mut warnings = Vec::new();
    for (key, default) in defaults {
        if map.get(*key).is_some_and(|v| v != default) {
            return Err(invalid());
        }
        warnings.push(format!(
            "{prefix}.{key}: unsupported default/absent ignored"
        ));
    }
    Ok(warnings)
}
#[derive(Clone, Debug)]
pub struct ValidatorSettings {
    pub enabled: bool,
    pub skip_body_validation: bool,
    pub handle_nullable_field: bool,
    pub skip_path_prefixes: Vec<String>,
}
impl ValidatorSettings {
    pub fn from_value(value: &Value) -> Result<(Self, Vec<String>), InitError> {
        let warnings = defaults(
            value,
            &[
                "enabled",
                "skipBodyValidation",
                "handleNullableField",
                "skipPathPrefixes",
            ],
            &[
                ("logError", json!(true)),
                ("legacyPathType", json!(false)),
                ("validateResponse", json!(false)),
            ],
            "openapi-validator",
        )?;
        let map = object(value, "openapi-validator.yml: object required")?;
        let skip_path_prefixes = strings(map.get("skipPathPrefixes"))?;
        if skip_path_prefixes.iter().any(|p| !p.starts_with('/')) {
            return Err(invalid());
        }
        Ok((
            Self {
                enabled: boolean(map, "enabled", false)?,
                skip_body_validation: boolean(map, "skipBodyValidation", false)?,
                handle_nullable_field: boolean(map, "handleNullableField", true)?,
                skip_path_prefixes,
            },
            warnings,
        ))
    }
}
pub const ENCODERS: &[&str] = &[
    "html",
    "html-content",
    "html-attribute",
    "html-attribute-unquoted",
    "xml",
    "xml-content",
    "xml-attribute",
    "xml-comment",
    "xml-1.1",
    "xml-1.1-content",
    "xml-1.1-attribute",
    "json",
    "cdata",
    "css-string",
    "css-url",
    "java",
    "javascript",
    "javascript-attribute",
    "javascript-block",
    "javascript-source",
    "uri",
    "uri-component",
];
pub fn sanitizer(value: &Value) -> Result<Vec<String>, InitError> {
    let warnings = defaults(
        value,
        &[
            "enabled",
            "bodyEnabled",
            "bodyEncoder",
            "bodyAttributesToEncode",
            "bodyAttributesToIgnore",
            "headerEnabled",
            "headerEncoder",
            "headerAttributesToEncode",
            "headerAttributesToIgnore",
        ],
        &[],
        "sanitizer",
    )?;
    let map = object(value, "sanitizer.yml: object required")?;
    for key in ["enabled", "bodyEnabled", "headerEnabled"] {
        boolean(map, key, false)?;
    }
    for key in ["bodyEncoder", "headerEncoder"] {
        if map
            .get(key)
            .is_some_and(|v| !v.as_str().is_some_and(|s| ENCODERS.contains(&s)))
        {
            return Err(invalid());
        }
    }
    for key in [
        "bodyAttributesToEncode",
        "bodyAttributesToIgnore",
        "headerAttributesToEncode",
        "headerAttributesToIgnore",
    ] {
        strings(map.get(key))?;
    }
    Ok(warnings)
}
#[derive(Clone, Debug)]
pub struct HealthSettings {
    pub enabled: bool,
}
impl HealthSettings {
    pub fn from_value(value: &Value) -> Result<(Self, Vec<String>), InitError> {
        let warnings = defaults(
            value,
            &["enabled"],
            &[
                ("useJson", json!(false)),
                ("timeout", json!(2000)),
                ("downstreamEnabled", json!(false)),
                ("downstreamHost", json!("http://localhost:8081")),
                ("downstreamPath", json!("/health")),
            ],
            "health",
        )?;
        Ok((
            Self {
                enabled: boolean(
                    object(value, "health.yml: object required")?,
                    "enabled",
                    false,
                )?,
            },
            warnings,
        ))
    }
}
pub fn transformer(value: &Value, response: bool) -> Result<Vec<String>, InitError> {
    let prefix = if response {
        "response-transformer"
    } else {
        "request-transformer"
    };
    let map = object(value, "transformer: object required")?;
    boolean(map, "enabled", false)?;
    let prefixes = strings(map.get("appliedPathPrefixes"))?;
    if prefixes.iter().any(|p| !p.starts_with('/')) {
        return Err(invalid());
    }
    let mut normalized = value.clone();
    if let Some(map) = normalized.as_object_mut() {
        {
            let key = "pathPrefixEncoding";
            if map.get(key).is_some_and(|v| {
                v.is_null() || v.as_str() == Some("") || v.as_object().is_some_and(|m| m.is_empty())
            }) {
                map.remove(key);
            }
        }
    }
    defaults(
        &normalized,
        &["enabled", "appliedPathPrefixes"],
        &[
            ("requiredContent", json!(true)),
            ("defaultBodyEncoding", json!("UTF-8")),
            ("pathPrefixEncoding", Value::Null),
        ],
        prefix,
    )
}

pub fn router(value: &Value) -> Result<Vec<String>, InitError> {
    let map = object(value, "router.yml: object required")?;
    for key in ["enabled", "httpsEnabled", "metricsInjection"] {
        boolean(map, key, false)?;
    }
    if map
        .get("maxRequestTime")
        .is_some_and(|v| v.as_u64().is_none())
        || map.get("metricsName").is_some_and(|v| !v.is_string())
    {
        return Err(invalid());
    }
    if let Some(rules) = map.get("urlRewriteRules") {
        for rule in strings(Some(rules))? {
            if rule.split_whitespace().count() != 2 {
                return Err(invalid());
            }
        }
    }
    let mut normalized = value.clone();
    let normalized = normalized.as_object_mut().ok_or_else(invalid)?;
    for key in [
        "pathPrefixMaxRequestTime",
        "hostWhitelist",
        "urlRewriteRules",
        "methodRewriteRules",
        "queryParamRewriteRules",
        "headerRewriteRules",
        "streamPathPrefixes",
    ] {
        if let Some(v) = map.get(key) {
            let empty = v.is_null()
                || v.as_str() == Some("")
                || if [
                    "pathPrefixMaxRequestTime",
                    "queryParamRewriteRules",
                    "headerRewriteRules",
                ]
                .contains(&key)
                {
                    v.as_object().is_some_and(|v| v.is_empty())
                } else {
                    v.as_array().is_some_and(|v| v.is_empty())
                };
            if empty {
                normalized.remove(key);
            }
        }
    }
    defaults(
        &Value::Object(normalized.clone()),
        &[
            "enabled",
            "httpsEnabled",
            "maxRequestTime",
            "urlRewriteRules",
        ],
        &[
            ("metricsInjection", json!(false)),
            ("metricsName", json!("router-response")),
            ("http2Enabled", json!(true)),
            ("pathPrefixMaxRequestTime", Value::Null),
            ("streamResponseContentTypes", json!(["text/event-stream"])),
            ("streamRequestAcceptTypes", json!(["text/event-stream"])),
            ("streamPathPrefixes", Value::Null),
            ("streamMaxRequestTime", json!(0)),
            ("streamIdleTimeout", json!(0)),
            (
                "streamResponseHeaderOverwrite",
                json!([
                    "Content-Type",
                    "Cache-Control",
                    "Connection",
                    "Transfer-Encoding",
                    "Content-Encoding",
                    "Content-Length"
                ]),
            ),
            ("connectionsPerThread", json!(10)),
            ("maxQueueSize", json!(0)),
            ("softMaxConnectionsPerThread", json!(5)),
            ("rewriteHostHeader", json!(true)),
            ("reuseXForwarded", json!(false)),
            ("maxConnectionRetries", json!(3)),
            ("preResolveFQDN2IP", json!(false)),
            ("hostWhitelist", Value::Null),
            ("serviceIdQueryParameter", json!(false)),
            ("methodRewriteRules", Value::Null),
            ("queryParamRewriteRules", Value::Null),
            ("headerRewriteRules", Value::Null),
        ],
        "router",
    )
}

/// Declaration admission only; no CEL/action execution or experimental limit
/// becomes supported because a disabled rule document passes these checks.
pub fn rule_declarations(value: &Value) -> Result<Vec<String>, InitError> {
    let root = object(value, "rule.yml: object required")?;
    if root
        .keys()
        .any(|k| !matches!(k.as_str(), "ruleBodies" | "endpointRules"))
    {
        return Err(invalid());
    }
    if let Some(endpoints) = root.get("endpointRules") {
        let endpoints = endpoints.as_object().ok_or_else(invalid)?;
        for endpoint in endpoints.values() {
            for names in endpoint.as_object().ok_or_else(invalid)?.values() {
                if !names
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string))
                {
                    return Err(invalid());
                }
            }
        }
    }
    if let Some(rules) = root.get("ruleBodies") {
        for rule in rules.as_object().ok_or_else(invalid)?.values() {
            let rule = rule.as_object().ok_or_else(invalid)?;
            if rule.keys().any(|k| {
                ![
                    "ruleId",
                    "ruleName",
                    "ruleType",
                    "common",
                    "hostId",
                    "ruleDesc",
                    "version",
                    "author",
                    "updatedAt",
                    "conditionLanguage",
                    "conditionSecurityProfile",
                    "accessControlEffect",
                    "expression",
                    "conditions",
                    "actions",
                ]
                .contains(&k.as_str())
            }) {
                return Err(invalid());
            }
            for key in ["ruleId", "ruleName", "ruleType", "common"] {
                if !rule.get(key).is_some_and(Value::is_string) {
                    return Err(invalid());
                }
            }
            for key in [
                "hostId",
                "ruleDesc",
                "version",
                "author",
                "updatedAt",
                "conditionLanguage",
                "conditionSecurityProfile",
                "accessControlEffect",
                "expression",
            ] {
                if rule
                    .get(key)
                    .is_some_and(|v| !v.is_null() && !v.is_string())
                {
                    return Err(invalid());
                }
            }
            if let Some(conditions) = rule.get("conditions").filter(|v| !v.is_null()) {
                for condition in conditions.as_array().ok_or_else(invalid)? {
                    if condition.as_object().ok_or_else(invalid)?.keys().any(|k| {
                        ![
                            "conditionId",
                            "conditionDesc",
                            "operator",
                            "operatorCode",
                            "operand",
                            "propertyPath",
                            "expected",
                            "joinCode",
                        ]
                        .contains(&k.as_str())
                    }) {
                        return Err(invalid());
                    }
                }
            }
            if let Some(actions) = rule.get("actions").filter(|v| !v.is_null()) {
                for action in actions.as_array().ok_or_else(invalid)? {
                    let action = action.as_object().ok_or_else(invalid)?;
                    if action.contains_key("actionClassName")
                        || action.keys().any(|k| {
                            !["actionId", "actionDesc", "actionRef", "actionValues"]
                                .contains(&k.as_str())
                        })
                    {
                        return Err(invalid());
                    }
                    if !action
                        .get("actionRef")
                        .and_then(Value::as_str)
                        .is_some_and(|name| {
                            [
                                "request-body-literal",
                                "response-body-literal",
                                "request-metadata-literal",
                                "request-local-response-literal",
                                "request-validation-error-literal",
                                "response-headers-literal",
                            ]
                            .contains(&name)
                        })
                    {
                        return Err(invalid());
                    }
                    if action
                        .get("actionValues")
                        .is_some_and(|v| !v.is_null() && !v.is_object())
                    {
                        return Err(invalid());
                    }
                    for key in ["actionId", "actionDesc"] {
                        if action
                            .get(key)
                            .is_some_and(|v| !v.is_null() && !v.is_string())
                        {
                            return Err(invalid());
                        }
                    }
                    if let Some(values) = action.get("actionValues").filter(|v| !v.is_null()) {
                        rule_literal_declaration(
                            action["actionRef"].as_str().expect("validated action"),
                            values,
                        )?;
                    }
                }
            }
        }
    }
    Ok(Vec::new())
}

fn rule_literal_declaration(action: &str, value: &Value) -> Result<(), InitError> {
    let values = value.as_object().ok_or_else(invalid)?;
    let allowed: &[&str] = match action {
        "request-body-literal" | "response-body-literal" => &["body"],
        "request-metadata-literal" => &[
            "requestPath",
            "queryString",
            "requestHeaders",
            "requestBody",
        ],
        "request-local-response-literal" => &["responseBody", "statusCode"],
        "request-validation-error-literal" => &["validationError", "errorMessage"],
        "response-headers-literal" => &["remove", "update"],
        _ => return Err(invalid()),
    };
    for (key, value) in values {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid());
        }
        match key.as_str() {
            "validationError" if value.is_boolean() => {}
            "statusCode" if value.as_i64().is_some() => {}
            "requestHeaders" => rule_literal_declaration("response-headers-literal", value)?,
            "remove"
                if value
                    .as_array()
                    .is_some_and(|a| a.iter().all(Value::is_string)) => {}
            "update"
                if value
                    .as_object()
                    .is_some_and(|m| m.values().all(Value::is_string)) => {}
            "body" | "requestPath" | "queryString" | "requestBody" | "responseBody"
            | "errorMessage"
                if value.is_string() => {}
            _ => return Err(invalid()),
        }
    }
    Ok(())
}
