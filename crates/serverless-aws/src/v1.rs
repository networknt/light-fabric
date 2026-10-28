use crate::{alb::encoded_body, limits::status};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use serverless_core::{ExchangeState, GatewayResponse, Invocation, StatusError};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct EventConfig {
    stage: String,
    pub stage_variables: Option<BTreeMap<String, String>>,
    pub account_id: Option<String>,
    pub api_id: Option<String>,
    pub domain_name: Option<String>,
    pub resource_id: Option<String>,
    pub encode_base64_request: bool,
}
impl EventConfig {
    pub fn new(stage: String) -> Result<Self, &'static str> {
        if stage.trim().is_empty() {
            return Err("configuration: missing light-lambda.stage");
        }
        Ok(Self {
            stage,
            stage_variables: None,
            account_id: None,
            api_id: None,
            domain_name: None,
            resource_id: None,
            encode_base64_request: false,
        })
    }
}
fn nullable<T: serde::Serialize>(map: &BTreeMap<String, T>) -> Value {
    if map.is_empty() {
        Value::Null
    } else {
        json!(map)
    }
}
/// Clock is explicit: tests supply deterministic UTC, production supplies its clock.
/// Original ingress fields never override finalized request state or verified claims.
pub fn build_v1_event(
    inv: &Invocation,
    state: &ExchangeState,
    cfg: &EventConfig,
    now: DateTime<Utc>,
) -> Result<Value, StatusError> {
    let mut headers = BTreeMap::new();
    let mut multi_headers = BTreeMap::new();
    for key in inv.headers.keys() {
        let values: Vec<&str> = inv
            .headers
            .get_all(key)
            .iter()
            .map(|v| std::str::from_utf8(v.as_bytes()).map_err(|_| status(14012, "")))
            .collect::<Result<_, _>>()?;
        if let Some(last) = values.last() {
            headers.insert(key.to_string(), *last);
        }
        multi_headers.insert(key.to_string(), values);
    }
    // CorrelationMiddleware selects the first supplied value. Its selected
    // scalar is authoritative when the policy ran; preserve every original
    // multimap value and the existing scalar rule for unrelated headers.
    if let Some(correlation) = state.correlation_id.as_deref()
        && inv
            .headers
            .get("x-correlation-id")
            .and_then(|v| v.to_str().ok())
            == Some(correlation)
    {
        headers.insert("x-correlation-id".into(), correlation);
    }
    let mut query = BTreeMap::new();
    let mut multi_query: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (key, value) in &inv.query.0 {
        query.insert(key.clone(), value.as_str());
        multi_query.entry(key.clone()).or_default().push(value);
    }
    // R-03 copies captures into scalar query only; normalized multi-values survive.
    for (key, value) in &inv.path_params {
        query.insert(key.clone(), value);
    }
    let resource = inv
        .route
        .as_ref()
        .map(|r| r.template.as_str())
        .unwrap_or(&inv.path);
    let method = inv.method.as_str().to_ascii_uppercase();
    let (body, encoded) = encoded_body(&inv.body, cfg.encode_base64_request || inv.body_was_base64);
    let mut context = json!({
        "requestId": inv.request_id, "extendedRequestId": inv.request_id,
        "resourcePath": resource, "httpMethod": method, "path": inv.path,
        "stage": cfg.stage, "requestTime": now.format("%d/%b/%Y:%H:%M:%S +0000").to_string(),
        "requestTimeEpoch": now.timestamp_millis(), "protocol": "HTTP/1.1",
        "accountId": cfg.account_id, "apiId": cfg.api_id, "domainName": cfg.domain_name, "resourceId": cfg.resource_id,
        "identity": {"sourceIp": inv.source.source_ip.map(|ip| ip.to_string()), "userAgent": headers.get("user-agent"),
          "accountId": null, "apiKey": null, "apiKeyId": null, "caller": null,
          "cognitoAuthenticationProvider": null, "cognitoAuthenticationType": null,
          "cognitoIdentityId": null, "cognitoIdentityPoolId": null, "user": null, "userArn": null,
          "accessKey": null, "clientCert": null}
    });
    if let Some(principal) = &state.principal {
        let principal_id = ["sub", "cid", "client_id", "user_id"]
            .into_iter()
            .find_map(|k| principal.claims().get(k).and_then(Value::as_str))
            .unwrap_or(principal.principal_id());
        let mut authorizer = json!({"principalId": principal_id, "claims": principal.claims()});
        if let Some(scope) = principal.claims().get("scope") {
            let scopes = match scope {
                Value::String(s) => Some(s.split_whitespace().collect::<Vec<_>>().join(" ")),
                Value::Array(a) => a
                    .iter()
                    .map(Value::as_str)
                    .collect::<Option<Vec<_>>>()
                    .map(|v| v.join(" ")),
                _ => None,
            };
            if let Some(scopes) = scopes {
                authorizer["scope"] = json!(scopes);
            }
        }
        context["authorizer"] = authorizer;
    }
    Ok(
        json!({"resource": resource, "path": inv.path, "httpMethod": method,
      "headers": nullable(&headers), "multiValueHeaders": nullable(&multi_headers),
      "queryStringParameters": nullable(&query), "multiValueQueryStringParameters": nullable(&multi_query),
      "pathParameters": nullable(&inv.path_params), "stageVariables": cfg.stage_variables.as_ref().map(nullable).unwrap_or(Value::Null),
      "body": body, "isBase64Encoded": encoded, "requestContext": context}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProxyResponse {
    status_code: u16,
    headers: Option<BTreeMap<String, String>>,
    multi_value_headers: Option<BTreeMap<String, Vec<String>>>,
    body: Option<String>,
    is_base64_encoded: Option<bool>,
}

/// `alias` is validated configuration, never request input. No Debug of SDK
/// responses/errors or payload values is permitted here.
pub fn parse_response(
    payload: &[u8],
    function_error: Option<&str>,
    executed_version: Option<&str>,
    request_id: &str,
    alias: &str,
) -> Result<GatewayResponse, StatusError> {
    if let Some(error) = function_error {
        let error = match error {
            "Handled" => "Handled",
            "Unhandled" => "Unhandled",
            _ => "<redacted>",
        };
        let parsed: Option<Value> = serde_json::from_slice(payload).ok();
        let error_type = parsed
            .as_ref()
            .and_then(|v| v.get("errorType"))
            .and_then(Value::as_str)
            .filter(|s| allowed_error_type(s))
            .unwrap_or("<redacted>");
        // Preserve absence as JSON null; present values retain the exact
        // existing bounded allowlist/redaction policy.
        let version = executed_version.map(|version| {
            if version.len() <= 128
                && version
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'$' || b == b'_' || b == b'-')
            {
                version
            } else {
                "<redacted>"
            }
        });
        let metadata = serde_json::json!({
            "requestId": request_id, "targetAlias": alias,
            "functionError": error, "executedVersion": version,
            "payloadBytes": payload.len(), "errorType": error_type
        });
        tracing::warn!(message = %metadata);
        return Err(status(14014, alias));
    }
    let invalid = || {
        tracing::warn!(payload_bytes = payload.len());
        status(14015, alias)
    };
    let value: Value = serde_json::from_slice(payload).map_err(|_| invalid())?;
    if !value.is_object() {
        return Err(invalid());
    }
    let proxy: ProxyResponse = serde_json::from_value(value).map_err(|_| invalid())?;
    if !(100..=599).contains(&proxy.status_code) {
        return Err(invalid());
    }
    let mut headers = HeaderMap::new();
    let mut multi_names = BTreeSet::new();
    for (key, values) in proxy.multi_value_headers.unwrap_or_default() {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| invalid())?;
        multi_names.insert(name.as_str().to_owned());
        for value in values {
            headers.append(name.clone(), {
                let value = HeaderValue::from_str(&value).map_err(|_| invalid())?;
                value.to_str().map_err(|_| invalid())?;
                value
            });
        }
    }
    for (key, value) in proxy.headers.unwrap_or_default() {
        let name = HeaderName::from_bytes(key.as_bytes()).map_err(|_| invalid())?;
        // Validate even discarded scalar values; they cannot smuggle invalid HTTP.
        let value = HeaderValue::from_str(&value).map_err(|_| invalid())?;
        value.to_str().map_err(|_| invalid())?;
        if !multi_names.contains(name.as_str()) {
            headers.append(name, value);
        }
    }
    let binary = proxy.is_base64_encoded.unwrap_or(false);
    let body = proxy
        .body
        .map(|body| {
            if binary {
                STANDARD
                    .decode(body)
                    .map(Bytes::from)
                    .map_err(|_| invalid())
            } else {
                Ok(Bytes::from(body))
            }
        })
        .transpose()?;
    Ok(GatewayResponse {
        status: StatusCode::from_u16(proxy.status_code).map_err(|_| invalid())?,
        headers,
        body,
        body_is_binary: binary,
    })
}
fn allowed_error_type(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.$:-".contains(&b))
}
