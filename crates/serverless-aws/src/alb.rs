use crate::limits::{Limits, status};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use serde::Deserialize;
use serde_json::{Value, json};
use serverless_core::{GatewayResponse, Invocation, MultiMap, SourceMetadata, StatusError};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IngressMode {
    Multi,
    Single,
}

/// Runtime supplies already computed absolute deadlines. This adapter never reserves time.
pub struct DecodeContext {
    pub request_id: String,
    pub deadline: Instant,
    pub response_deadline: Instant,
    pub cancel: CancellationToken,
}
#[derive(Debug)]
pub struct DecodeError {
    pub mode: IngressMode,
    pub status: StatusError,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    http_method: String,
    path: String,
    multi_value_headers: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    multi_value_query_string_parameters: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    is_base64_encoded: Option<bool>,
}
static SINGLE_WARNING: AtomicBool = AtomicBool::new(false);

pub fn decode(
    original: Value,
    context: DecodeContext,
    limits: &Limits,
) -> Result<Invocation, DecodeError> {
    decode_inner(original, context, limits, false).map(|(inv, _)| inv)
}
/// Same validated raw capture as decode; never uses caller-supplied proof fields.
pub fn decode_with_validator_media(
    original: Value,
    context: DecodeContext,
    limits: &Limits,
) -> Result<(Invocation, serverless_core::ValidatorMediaProof), DecodeError> {
    decode_inner(original, context, limits, true)
}
fn decode_inner(
    original: Value,
    context: DecodeContext,
    limits: &Limits,
    capture_media: bool,
) -> Result<(Invocation, serverless_core::ValidatorMediaProof), DecodeError> {
    let mode = if original.get("multiValueHeaders").is_some() {
        IngressMode::Multi
    } else if original.get("headers").is_some() {
        IngressMode::Single
    } else {
        IngressMode::Multi
    };
    let fail = |code| DecodeError {
        mode,
        status: status(code, limits.max_request_bytes),
    };
    if original
        .pointer("/requestContext/elb/targetGroupArn")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        tracing::warn!(event_family = "unknown");
        return Err(fail(14010));
    }
    if mode == IngressMode::Single {
        if !SINGLE_WARNING.swap(true, Ordering::Relaxed) {
            tracing::error!(
                event_family = "alb",
                configuration_error = "multi_value_required"
            );
        }
        return Err(fail(14011));
    }
    let event: Event = serde_json::from_value(original.clone()).map_err(|_| fail(14012))?;
    let method = Method::from_bytes(event.http_method.as_bytes()).map_err(|_| fail(14012))?;
    let path = percent_decode(&event.path).map_err(|_| fail(14012))?;
    if !path.starts_with('/') {
        return Err(fail(14012));
    }
    let mut headers = HeaderMap::new();
    let mut exact_content_type = None;
    for (key, values) in event.multi_value_headers {
        let exact = capture_media && key.eq_ignore_ascii_case("content-type");
        let key = HeaderName::from_bytes(key.as_bytes()).map_err(|_| fail(14012))?;
        for (index, value) in values.into_iter().enumerate() {
            let value = HeaderValue::from_str(&value).map_err(|_| fail(14012))?;
            if exact && index == 0 && exact_content_type.is_none() {
                exact_content_type = Some(value.clone());
            }
            headers.append(key.clone(), value);
        }
    }
    // Operational prerequisite: ALB append mode, no untrusted hop after the ALB.
    // Missing/malformed last address remains unknown; never fall back to an earlier hop.
    let source_ip = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.rsplit(',').next())
        .and_then(|s| s.trim().parse().ok());
    let mut query = MultiMap::default();
    let mut raw = Vec::new();
    // JSON objects have no wire-order guarantee. Key order is lexical; array order is retained.
    for (key, values) in event
        .multi_value_query_string_parameters
        .unwrap_or_default()
    {
        let decoded = percent_decode(&key).map_err(|_| fail(14012))?;
        for value in values {
            raw.push(format!("{key}={value}"));
            query.0.push((
                decoded.clone(),
                percent_decode(&value).map_err(|_| fail(14012))?,
            ));
        }
    }
    let encoded = event.is_base64_encoded.unwrap_or(false);
    let body = event
        .body
        .map(|b| {
            if encoded {
                STANDARD.decode(b).map(Bytes::from).map_err(|_| fail(14012))
            } else {
                Ok(Bytes::from(b))
            }
        })
        .transpose()?;
    if body
        .as_ref()
        .is_some_and(|b| b.len() > limits.max_request_bytes)
    {
        return Err(fail(14013));
    }
    let proof = if capture_media {
        serverless_core::ValidatorMediaProof::from_validated_ingress(exact_content_type)
    } else {
        serverless_core::ValidatorMediaProof::default()
    };
    Ok((
        Invocation {
            method,
            raw_path: event.path,
            path,
            raw_query: (!raw.is_empty()).then(|| raw.join("&")),
            query,
            headers,
            body,
            body_was_base64: encoded,
            source: SourceMetadata {
                source_ip,
                ingress_kind: "alb".into(),
            },
            request_id: context.request_id,
            deadline: context.deadline,
            response_deadline: context.response_deadline,
            cancel: context.cancel,
            path_params: BTreeMap::new(),
            route: None,
            original,
        },
        proof,
    ))
}

fn percent_decode(s: &str) -> Result<String, ()> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let a = *bytes.get(i + 1).ok_or(())?;
            let b = *bytes.get(i + 2).ok_or(())?;
            let hex = |c: u8| (c as char).to_digit(16).map(|n| n as u8).ok_or(());
            out.push(hex(a)? * 16 + hex(b)?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| ())
}

pub(crate) fn encoded_body(body: &Option<Bytes>, binary: bool) -> (Option<String>, bool) {
    match body {
        None => (None, false),
        Some(b) if !binary => match std::str::from_utf8(b) {
            Ok(s) => (Some(s.into()), false),
            Err(_) => (Some(STANDARD.encode(b)), true),
        },
        Some(b) => (Some(STANDARD.encode(b)), true),
    }
}

fn envelope(
    response: &GatewayResponse,
    mode: IngressMode,
    force_base64: bool,
) -> Result<Value, &'static str> {
    let (body, encoded) = encoded_body(&response.body, force_base64 || response.body_is_binary);
    let mut out = json!({"statusCode": response.status.as_u16(), "statusDescription": format!("{} {}", response.status.as_u16(), response.status.canonical_reason().unwrap_or("Unknown")), "body": body, "isBase64Encoded": encoded});
    let mut headers = serde_json::Map::new();
    for name in response.headers.keys() {
        let values: Vec<&str> = response
            .headers
            .get_all(name)
            .iter()
            .map(|v| v.to_str().map_err(|_| "non-UTF8 response header"))
            .collect::<Result<_, _>>()?;
        headers.insert(
            name.as_str().into(),
            match mode {
                IngressMode::Multi => json!(values),
                IngressMode::Single => json!(values.last()),
            },
        );
    }
    out[match mode {
        IngressMode::Multi => "multiValueHeaders",
        IngressMode::Single => "headers",
    }] = Value::Object(headers);
    Ok(out)
}

/// Called once, after core response policies/finalization. Returns the exact bytes
/// to post; WP9 must not reserialize using another encoding or rerun policies.
pub fn encode(
    response: &GatewayResponse,
    mode: IngressMode,
    force_base64: bool,
    limits: &Limits,
) -> Result<Vec<u8>, &'static str> {
    prepare_response(response.clone(), mode, force_base64, limits)
        .map(|prepared| prepared.bytes)
        .map_err(|error| {
            if error == "prepared response exceeds response limit" {
                "replacement exceeds response limit"
            } else {
                error
            }
        })
}

/// Exact provider bytes and the response completion observers must see.
/// Constructed before policy callbacks; the framework posts these frozen bytes.
pub struct PreparedResponse {
    pub response: GatewayResponse,
    pub bytes: Vec<u8>,
}

/// Preserve the legacy encoder's exact envelope and oversize-replacement bytes,
/// while making its final HTTP outcome available before sole finalization.
pub fn prepare_response(
    response: GatewayResponse,
    mode: IngressMode,
    force_base64: bool,
    limits: &Limits,
) -> Result<PreparedResponse, &'static str> {
    limits.validate()?;
    let bytes = encode_final(&response, mode, force_base64, limits);
    match bytes {
        Ok(bytes) => Ok(PreparedResponse { response, bytes }),
        Err("prepared response exceeds response limit") => {
            let mut replacement = status(14016, limits.max_response_bytes).response();
            if let Some(id) = response
                .headers
                .get("x-correlation-id")
                .filter(|v| v.as_bytes().len() <= 256)
            {
                replacement.headers.insert("x-correlation-id", id.clone());
            }
            let bytes = encode_final(&replacement, mode, false, limits)?;
            Ok(PreparedResponse {
                response: replacement,
                bytes,
            })
        }
        Err(error) => Err(error),
    }
}

/// Final encoding never substitutes another HTTP outcome. Used only for an
/// ingress error or a preparation/callback failure's bounded gateway response.
pub fn encode_final(
    response: &GatewayResponse,
    mode: IngressMode,
    force_base64: bool,
    limits: &Limits,
) -> Result<Vec<u8>, &'static str> {
    limits.validate()?;
    let bytes = serde_json::to_vec(&envelope(response, mode, force_base64)?)
        .map_err(|_| "ALB serialization failed")?;
    if bytes.len() > limits.max_response_bytes {
        return Err("prepared response exceeds response limit");
    }
    Ok(bytes)
}
