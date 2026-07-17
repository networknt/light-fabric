//! Pure validation for the sessionless MCP HTTP profile.
//!
//! This module deliberately has no runtime, session-store, policy, cache, or
//! network dependency. It validates one already-classified request before the
//! stateless adapter is allowed to enter the application core.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map as JsonMap, Value as JsonValue};

pub(crate) const MCP_METHOD_HEADER: &str = "mcp-method";
pub(crate) const MCP_NAME_HEADER: &str = "mcp-name";
pub(crate) const CLIENT_INFO_META_KEY: &str = "io.modelcontextprotocol/clientInfo";
pub(crate) const CLIENT_CAPABILITIES_META_KEY: &str = "io.modelcontextprotocol/clientCapabilities";
pub(crate) const LOG_LEVEL_META_KEY: &str = "io.modelcontextprotocol/logLevel";
pub(crate) const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatelessRequestMetadata {
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExpectedParameterValue {
    String(String),
    Integer(i64),
    Boolean(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedParameterHeader {
    pub name: String,
    pub value: Option<ExpectedParameterValue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StatelessRequestError {
    pub status: u16,
    pub code: i64,
    pub message: String,
}

impl StatelessRequestError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: -32600,
            message: message.into(),
        }
    }

    fn header(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: -32020,
            message: message.into(),
        }
    }

    fn capability(message: impl Into<String>) -> Self {
        Self {
            status: 400,
            code: -32021,
            message: message.into(),
        }
    }
}

pub(crate) fn validate_stateless_request(
    headers: &[(String, String)],
    message: &JsonMap<String, JsonValue>,
) -> Result<StatelessRequestMetadata, StatelessRequestError> {
    require_json_content_type(headers)?;
    require_dual_accept(headers)?;
    if message.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
        return Err(StatelessRequestError::invalid(
            "invalid JSON-RPC version for stateless MCP request",
        ));
    }
    if message.contains_key("result") || message.contains_key("error") {
        return Err(StatelessRequestError::invalid(
            "client JSON-RPC responses are not allowed",
        ));
    }
    if !message.contains_key("id") {
        return Err(StatelessRequestError::invalid(
            "stateless MCP notification POSTs are not supported",
        ));
    }
    let method = message
        .get("method")
        .and_then(JsonValue::as_str)
        .filter(|method| !method.is_empty())
        .ok_or_else(|| StatelessRequestError::invalid("invalid JSON-RPC request"))?;
    let method_header = one_header(headers, MCP_METHOD_HEADER)?
        .ok_or_else(|| StatelessRequestError::header("missing Mcp-Method header"))?;
    let decoded_method = decode_header_value(method_header)?;
    if decoded_method != method {
        return Err(StatelessRequestError::header(
            "Mcp-Method header does not match JSON-RPC method",
        ));
    }

    let params = message
        .get("params")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| StatelessRequestError::invalid("params must be an object"))?;
    let meta = params
        .get("_meta")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| StatelessRequestError::invalid("params._meta must be an object"))?;
    if !meta
        .get(CLIENT_CAPABILITIES_META_KEY)
        .is_some_and(JsonValue::is_object)
    {
        return Err(StatelessRequestError::capability(
            "missing required client capabilities metadata",
        ));
    }
    if let Some(client_info) = meta.get(CLIENT_INFO_META_KEY) {
        let client_info = client_info.as_object().ok_or_else(|| {
            StatelessRequestError::invalid("clientInfo metadata must be an object")
        })?;
        for field in ["name", "version"] {
            if !client_info
                .get(field)
                .and_then(JsonValue::as_str)
                .is_some_and(|value| !value.is_empty() && value.len() <= 1024)
            {
                return Err(StatelessRequestError::invalid(format!(
                    "clientInfo.{field} must be a non-empty bounded string"
                )));
            }
        }
    }
    if let Some(log_level) = meta.get(LOG_LEVEL_META_KEY) {
        let log_level = log_level
            .as_str()
            .ok_or_else(|| StatelessRequestError::invalid("logLevel metadata must be a string"))?;
        if !matches!(
            log_level,
            "debug" | "info" | "notice" | "warning" | "error" | "critical" | "alert" | "emergency"
        ) {
            return Err(StatelessRequestError::invalid(
                "logLevel metadata is not supported",
            ));
        }
    }

    let name_header = one_header(headers, MCP_NAME_HEADER)?;
    if method == "tools/call" {
        let expected = params
            .get("name")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| StatelessRequestError::invalid("tools/call requires params.name"))?;
        let actual =
            name_header.ok_or_else(|| StatelessRequestError::header("missing Mcp-Name header"))?;
        if decode_header_value(actual)? != expected {
            return Err(StatelessRequestError::header(
                "Mcp-Name header does not match params.name",
            ));
        }
    } else if name_header.is_some() {
        return Err(StatelessRequestError::header(
            "Mcp-Name header is not applicable to this method",
        ));
    }
    if method != "tools/call"
        && headers
            .iter()
            .any(|(name, _)| name.to_ascii_lowercase().starts_with("mcp-param-"))
    {
        return Err(StatelessRequestError::header(
            "Mcp-Param headers are not applicable to this method",
        ));
    }

    Ok(StatelessRequestMetadata {
        method: method.to_string(),
    })
}

pub(crate) fn validate_parameter_headers(
    headers: &[(String, String)],
    expected: &[ExpectedParameterHeader],
) -> Result<(), StatelessRequestError> {
    for parameter in expected {
        let actual = one_header(headers, parameter.name.as_str())?;
        match (&parameter.value, actual) {
            (None, None) => {}
            (None, Some(_)) => {
                return Err(StatelessRequestError::header(format!(
                    "{} header is not applicable when its argument is absent or null",
                    parameter.name
                )));
            }
            (Some(_), None) => {
                return Err(StatelessRequestError::header(format!(
                    "missing {} header",
                    parameter.name
                )));
            }
            (Some(expected), Some(actual)) => {
                let actual = decode_header_value(actual)?;
                let matches = match expected {
                    ExpectedParameterValue::String(expected) => actual == *expected,
                    ExpectedParameterValue::Integer(expected) => actual
                        .parse::<i64>()
                        .is_ok_and(|actual| actual == *expected),
                    ExpectedParameterValue::Boolean(expected) => match actual.as_str() {
                        "true" => *expected,
                        "false" => !*expected,
                        _ => false,
                    },
                };
                if !matches {
                    return Err(StatelessRequestError::header(format!(
                        "{} header does not match its tool argument",
                        parameter.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn require_json_content_type(headers: &[(String, String)]) -> Result<(), StatelessRequestError> {
    let content_type = one_header(headers, "content-type")?
        .ok_or_else(|| StatelessRequestError::invalid("Content-Type must be application/json"))?;
    if !content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(StatelessRequestError::invalid(
            "Content-Type must be application/json",
        ));
    }
    Ok(())
}

fn require_dual_accept(headers: &[(String, String)]) -> Result<(), StatelessRequestError> {
    let values = all_headers(headers, "accept");
    let mut json = false;
    let mut event_stream = false;
    for value in values {
        for item in value.split(',') {
            match item.split(';').next().unwrap_or_default().trim() {
                "application/json" => json = true,
                "text/event-stream" => event_stream = true,
                _ => {}
            }
        }
    }
    if !json || !event_stream {
        return Err(StatelessRequestError::invalid(
            "Accept must list application/json and text/event-stream",
        ));
    }
    Ok(())
}

fn one_header<'a>(
    headers: &'a [(String, String)],
    name: &str,
) -> Result<Option<&'a str>, StatelessRequestError> {
    let values = all_headers(headers, name);
    if values.len() > 1 {
        return Err(StatelessRequestError::header(format!(
            "multiple {name} headers are not allowed"
        )));
    }
    Ok(values.first().copied())
}

fn all_headers<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

pub(crate) fn decode_header_value(value: &str) -> Result<String, StatelessRequestError> {
    if let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|value| value.strip_suffix("?="))
    {
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| StatelessRequestError::header("malformed MCP Base64 header value"))?;
        return String::from_utf8(bytes)
            .map_err(|_| StatelessRequestError::header("MCP Base64 header value is not UTF-8"));
    }
    if value.starts_with("=?") || value.ends_with("?=") {
        return Err(StatelessRequestError::header(
            "unsupported encoded header sentinel",
        ));
    }
    if value.is_empty()
        || value.trim() != value
        || value.bytes().any(|byte| !(0x20..=0x7e).contains(&byte))
    {
        return Err(StatelessRequestError::header(
            "header value requires MCP Base64 encoding",
        ));
    }
    Ok(value.to_string())
}

/// Encodes an outbound semantic MCP header using the exact SEP-2243 sentinel.
/// Printable ASCII values are kept literal; all other UTF-8 values are Base64.
pub(crate) fn encode_header_value(value: &str) -> Result<String, StatelessRequestError> {
    if !value.is_empty()
        && value.trim() == value
        && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
        && !value.starts_with("=?")
        && !value.ends_with("?=")
    {
        return Ok(value.to_string());
    }
    if value.is_empty() {
        return Err(StatelessRequestError::header(
            "empty MCP semantic header values are not allowed",
        ));
    }
    Ok(format!("=?base64?{}?=", STANDARD.encode(value.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request() -> JsonMap<String, JsonValue> {
        json!({
            "jsonrpc":"2.0", "id":1, "method":"server/discover",
            "params":{"_meta":{
                "io.modelcontextprotocol/protocolVersion":"DRAFT-2026-v1",
                "io.modelcontextprotocol/clientInfo":{"name":"test","version":"1"},
                "io.modelcontextprotocol/clientCapabilities":{}
            }}
        })
        .as_object()
        .cloned()
        .expect("object")
    }

    fn headers() -> Vec<(String, String)> {
        vec![
            ("content-type".into(), "application/json".into()),
            (
                "accept".into(),
                "application/json, text/event-stream".into(),
            ),
            ("mcp-method".into(), "server/discover".into()),
        ]
    }

    #[test]
    fn accepts_exact_stateless_transport_contract() {
        assert_eq!(
            validate_stateless_request(&headers(), &request())
                .expect("valid")
                .method,
            "server/discover"
        );
    }

    #[test]
    fn requires_both_response_media_types_and_client_capabilities() {
        let mut headers = headers();
        headers[1].1 = "application/json".into();
        assert_eq!(
            validate_stateless_request(&headers, &request())
                .unwrap_err()
                .code,
            -32600
        );
        let mut request = request();
        request["params"]["_meta"]
            .as_object_mut()
            .expect("meta")
            .remove(CLIENT_CAPABILITIES_META_KEY);
        assert_eq!(
            validate_stateless_request(&super::tests::headers(), &request)
                .unwrap_err()
                .code,
            -32021
        );
        let mut request = super::tests::request();
        request["params"]["_meta"][CLIENT_INFO_META_KEY] = json!({"name":"missing-version"});
        assert_eq!(
            validate_stateless_request(&super::tests::headers(), &request)
                .unwrap_err()
                .code,
            -32600
        );
        let mut request = super::tests::request();
        request["params"]["_meta"][LOG_LEVEL_META_KEY] = json!("verbose");
        assert_eq!(
            validate_stateless_request(&super::tests::headers(), &request)
                .unwrap_err()
                .code,
            -32600
        );
    }

    #[test]
    fn rejects_notifications_responses_and_header_mismatches() {
        let mut notification = request();
        notification.remove("id");
        assert_eq!(
            validate_stateless_request(&headers(), &notification)
                .unwrap_err()
                .code,
            -32600
        );
        let mut response = request();
        response.remove("method");
        response.insert("result".into(), json!({}));
        assert_eq!(
            validate_stateless_request(&headers(), &response)
                .unwrap_err()
                .code,
            -32600
        );
        let mut mismatch = headers();
        mismatch[2].1 = "tools/list".into();
        assert_eq!(
            validate_stateless_request(&mismatch, &request())
                .unwrap_err()
                .code,
            -32020
        );
    }

    #[test]
    fn base64_sentinel_is_literal_and_not_mime_encoded_word() {
        assert_eq!(
            decode_header_value("=?base64?dMOpc3Q=?=").expect("decode"),
            "tést"
        );
        assert!(decode_header_value("=?utf-8?B?dMOpc3Q=?=").is_err());
        assert!(decode_header_value(" padded ").is_err());
    }

    #[test]
    fn parameter_headers_match_typed_body_values_and_null_omission() {
        let expected = vec![
            ExpectedParameterHeader {
                name: "Mcp-Param-Region".into(),
                value: Some(ExpectedParameterValue::String("Montréal".into())),
            },
            ExpectedParameterHeader {
                name: "Mcp-Param-Limit".into(),
                value: Some(ExpectedParameterValue::Integer(42)),
            },
            ExpectedParameterHeader {
                name: "Mcp-Param-Active".into(),
                value: Some(ExpectedParameterValue::Boolean(true)),
            },
            ExpectedParameterHeader {
                name: "Mcp-Param-Optional".into(),
                value: None,
            },
        ];
        let headers = vec![
            ("mcp-param-region".into(), "=?base64?TW9udHLDqWFs?=".into()),
            ("Mcp-Param-Limit".into(), "042".into()),
            ("Mcp-Param-Active".into(), "true".into()),
        ];
        validate_parameter_headers(&headers, &expected).expect("matching headers");

        let mut missing = headers.clone();
        missing.retain(|(name, _)| !name.eq_ignore_ascii_case("Mcp-Param-Limit"));
        assert_eq!(
            validate_parameter_headers(&missing, &expected)
                .unwrap_err()
                .code,
            -32020
        );
        let mut duplicate = headers;
        duplicate.push(("mcp-param-active".into(), "true".into()));
        assert_eq!(
            validate_parameter_headers(&duplicate, &expected)
                .unwrap_err()
                .code,
            -32020
        );
    }
}
