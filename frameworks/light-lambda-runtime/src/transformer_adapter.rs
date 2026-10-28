//! Fixed six-action transport adapters. No runtime action/plugin registry.
use crate::transformer_program::{self as profile, Candidate};
use serde_json::{Map, Value};
use serverless_core::*;
use std::{collections::BTreeMap, sync::Arc};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    InputProfile,
    OutputProfile,
    Internal,
    Deadline,
}
pub fn request_error(fault: Fault) -> StatusError {
    match fault {
        Fault::InputProfile => StatusError::new(
            422,
            "ERR14031",
            "TRANSFORMER_INPUT_OUTSIDE_PROFILE",
            "Request exceeds the admitted Transformer input profile.",
        ),
        Fault::OutputProfile => StatusError::new(
            500,
            "ERR14032",
            "TRANSFORMER_OUTPUT_OUTSIDE_PROFILE",
            "Transformer output exceeds the admitted profile.",
        ),
        Fault::Internal => StatusError::handler_failure(),
        Fault::Deadline => StatusError::timeout(),
    }
}
/// Response context input and transformed output share the approved 500
/// outcome. A backend response outside this profile is not caller rejection.
pub fn response_error(fault: Fault) -> StatusError {
    request_error(if fault == Fault::InputProfile {
        Fault::OutputProfile
    } else {
        fault
    })
}
struct Budget {
    bytes: usize,
    nodes: usize,
}
impl Budget {
    fn add(&mut self, bytes: usize, nodes: usize) -> Result<(), Fault> {
        self.bytes = self.bytes.checked_add(bytes).ok_or(Fault::InputProfile)?;
        self.nodes += nodes;
        if self.bytes > profile::CONTEXT_BYTES || self.nodes > profile::CONTEXT_NODES {
            Err(Fault::InputProfile)
        } else {
            Ok(())
        }
    }
    fn string(&mut self, s: &str, node: bool) -> Result<(), Fault> {
        if s.len() > profile::CONTEXT_BYTES {
            return Err(Fault::InputProfile);
        }
        let bytes = 2 + s
            .chars()
            .map(|c| match c {
                '"' | '\\' | '\u{8}' | '\t' | '\n' | '\u{c}' | '\r' => 2,
                c if (c as u32) < 32 => 6,
                c => c.len_utf8(),
            })
            .sum::<usize>();
        self.add(bytes, usize::from(node))
    }
    fn object(&mut self, entries: &BTreeMap<&str, &str>) -> Result<(), Fault> {
        if entries.len() > 64 || entries.keys().any(|k| k.len() > 256) {
            return Err(Fault::InputProfile);
        }
        self.add(2 + entries.len().saturating_sub(1), 1)?;
        for (k, v) in entries {
            self.string(k, false)?;
            self.add(1, 0)?;
            self.string(v, true)?;
        }
        Ok(())
    }
}
#[async_trait::async_trait]
impl RequestHandler for TransformerAdapter {
    fn id(&self) -> HandlerId {
        HandlerId::RequestTransformer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        Ok(match self.request_with_media(inv, state).await {
            Ok((outcome, media)) => {
                if !matches!(&outcome, HandlerOutcome::Reject(_)) {
                    state.commit_validator_media(media);
                }
                outcome
            }
            Err(fault) => HandlerOutcome::Reject(request_error(fault)),
        })
    }
}
#[async_trait::async_trait]
impl ResponseHandler for TransformerAdapter {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseTransformer
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        state: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        match self.response(inv, state, response).await {
            Ok(candidate) => *response = candidate,
            Err(fault) => {
                let error = response_error(fault);
                *response = error.response();
                state.audit.status = Some(error);
            }
        }
        Ok(())
    }
}
fn owned(map: &BTreeMap<&str, &str>) -> Value {
    Value::Object(
        map.iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect(),
    )
}
fn context(
    inv: &Invocation,
    state: &ExchangeState,
    response: Option<&GatewayResponse>,
) -> Result<Value, Fault> {
    if inv.headers.keys().count() > 64 || inv.query.0.len() > 64 || inv.path_params.len() > 64 {
        return Err(Fault::InputProfile);
    }
    let headers = inv
        .headers
        .keys()
        .map(|key| {
            Ok((
                key.as_str(),
                std::str::from_utf8(inv.headers.get(key).expect("known key").as_bytes())
                    .map_err(|_| Fault::InputProfile)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, Fault>>()?;
    let mut query = BTreeMap::new();
    for (k, v) in &inv.query.0 {
        query.entry(k.as_str()).or_insert(v.as_str());
    }
    let path = inv
        .path_params
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect::<BTreeMap<_, _>>();
    let audit = gateway_policy::audit::SAFE_FIELDS
        .iter()
        .filter_map(|name| {
            state
                .audit
                .fields
                .get(name)
                .and_then(Value::as_str)
                .map(|value| (*name, value))
        })
        .collect::<BTreeMap<_, _>>();
    let request_body = if matches!(inv.method.as_str(), "POST" | "PUT" | "PATCH") {
        inv.body
            .as_ref()
            .map(|body| std::str::from_utf8(body).map_err(|_| Fault::InputProfile))
            .transpose()?
    } else {
        None
    };
    let response_body = response
        .and_then(|r| r.body.as_ref())
        .map(|body| std::str::from_utf8(body).map_err(|_| Fault::InputProfile))
        .transpose()?;
    // Preserve absent/null ingress maps without collapsing explicit empty maps.
    let query_absent = inv.query.0.is_empty()
        && !inv
            .original
            .get("queryStringParameters")
            .is_some_and(Value::is_object)
        && !inv
            .original
            .get("multiValueQueryStringParameters")
            .is_some_and(Value::is_object);
    let path_absent = inv.path_params.is_empty()
        && !inv
            .original
            .get("pathParameters")
            .is_some_and(Value::is_object);
    let absent = |name: &str| {
        (name == "queryParameters" && query_absent) || (name == "pathParameters" && path_absent)
    };
    let mut fields = vec![
        ("requestHeaders", &headers),
        ("queryParameters", &query),
        ("pathParameters", &path),
    ];
    if response.is_some() {
        fields.push(("responseHeaders", &headers));
    }
    let count = fields.len()
        + 3
        + usize::from(request_body.is_some())
        + usize::from(response_body.is_some())
        + usize::from(response.is_some());
    let mut budget = Budget {
        bytes: 2 + count.saturating_sub(1),
        nodes: 1,
    };
    for (name, map) in &fields {
        budget.string(name, false)?;
        budget.add(1, 0)?;
        if absent(name) {
            budget.add(4, 1)?;
        } else {
            budget.object(map)?;
        }
    }
    for (name, value) in [
        ("method", inv.method.as_str()),
        ("requestPath", inv.path.as_str()),
    ]
    .into_iter()
    .chain(request_body.map(|b| ("requestBody", b)))
    .chain(response_body.map(|b| ("responseBody", b)))
    {
        budget.string(name, false)?;
        budget.add(1, 0)?;
        budget.string(value, true)?;
    }
    budget.string("auditInfo", false)?;
    budget.add(1, 0)?;
    if audit.is_empty() {
        budget.add(4, 1)?;
    } else {
        budget.object(&audit)?;
    }
    if let Some(response) = response {
        budget.string("statusCode", false)?;
        budget.add(1 + response.status.as_u16().to_string().len(), 1)?;
    }
    // Only now copy caller data. Every map and string was borrowed and counted.
    let mut object = Map::new();
    for (name, map) in fields {
        object.insert(
            name.into(),
            if absent(name) {
                Value::Null
            } else {
                owned(map)
            },
        );
    }
    object.insert("method".into(), Value::String(inv.method.to_string()));
    object.insert("requestPath".into(), Value::String(inv.path.clone()));
    object.insert(
        "auditInfo".into(),
        if audit.is_empty() {
            Value::Null
        } else {
            owned(&audit)
        },
    );
    if let Some(body) = request_body {
        object.insert("requestBody".into(), Value::String(body.into()));
    }
    if let Some(body) = response_body {
        object.insert("responseBody".into(), Value::String(body.into()));
    }
    if let Some(response) = response {
        object.insert("statusCode".into(), Value::from(response.status.as_u16()));
    }
    let result = Value::Object(object);
    profile::context(&result).map_err(|_| Fault::InputProfile)?;
    Ok(result)
}
fn header_changes(headers: &mut http::HeaderMap, value: &Value) -> Result<(), Fault> {
    header_changes_with_media(headers, value, None)
}
fn header_changes_with_media(
    headers: &mut http::HeaderMap,
    value: &Value,
    mut media: Option<&mut ValidatorMediaProof>,
) -> Result<(), Fault> {
    if let Some(remove) = value.get("remove").and_then(Value::as_array) {
        for key in remove {
            let name = key.as_str().ok_or(Fault::Internal)?;
            headers.remove(name);
            if let Some(media) = &mut media {
                media.record_remove(name);
            }
        }
    }
    if let Some(update) = value.get("update").and_then(Value::as_object) {
        for (key, value) in update {
            let name = key;
            let key = http::HeaderName::try_from(key).map_err(|_| Fault::Internal)?;
            let value = http::HeaderValue::try_from(value.as_str().ok_or(Fault::Internal)?)
                .map_err(|_| Fault::Internal)?;
            if let Some(media) = &mut media {
                media.record_insert(name, &value, headers.contains_key(&key));
            }
            headers.insert(key, value);
        }
    }
    Ok(())
}
fn java_string(value: &Value) -> Result<String, Fault> {
    Ok(match value {
        Value::Null => return Err(Fault::Internal),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(java_nested_string)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| java_nested_string(v).map(|v| format!("{k}={v}")))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        ),
        _ => value.to_string(),
    })
}
fn java_nested_string(value: &Value) -> Result<String, Fault> {
    if value.is_null() {
        Ok("null".into())
    } else {
        java_string(value)
    }
}
pub struct TransformerAdapter {
    program: Arc<Candidate>,
    prefixes: Vec<String>,
}
impl TransformerAdapter {
    pub fn new(program: Arc<Candidate>, prefixes: Vec<String>) -> Self {
        Self { program, prefixes }
    }
    fn applies(&self, inv: &Invocation) -> bool {
        self.prefixes
            .iter()
            .any(|prefix| inv.path.starts_with(prefix))
    }
    async fn run(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        response: Option<&GatewayResponse>,
    ) -> Result<Value, Fault> {
        let deadline = if response.is_some() {
            inv.response_deadline
        } else {
            inv.deadline
        };
        if inv.cancel.is_cancelled() || tokio::time::Instant::now() >= deadline {
            return Err(Fault::Deadline);
        }
        let input = context(inv, state, response)?;
        let program = self.program.clone();
        let endpoint = format!("{}@{}", inv.path, inv.method.as_str().to_ascii_lowercase());
        let phase = if response.is_some() {
            "res-tra"
        } else {
            "req-tra"
        };
        profile::worker(
            input,
            if response.is_some() {
                inv.response_deadline
            } else {
                inv.deadline
            },
            inv.cancel.clone(),
            move |input| program.execute(input, &endpoint, phase),
        )
        .await
        .map_err(|error| match error.as_str() {
            "deadline" => Fault::Deadline,
            "candidate transformed output restriction" => Fault::OutputProfile,
            _ => Fault::Internal,
        })
    }
    pub async fn request(
        &self,
        inv: &mut Invocation,
        state: &ExchangeState,
    ) -> Result<HandlerOutcome, Fault> {
        self.request_with_media(inv, state)
            .await
            .map(|(outcome, _)| outcome)
    }
    async fn request_with_media(
        &self,
        inv: &mut Invocation,
        state: &ExchangeState,
    ) -> Result<(HandlerOutcome, ValidatorMediaProof), Fault> {
        let mut media = state.validator_media_snapshot();
        if !self.applies(inv) {
            return Ok((HandlerOutcome::Continue, media));
        }
        let output = self.run(inv, state, None).await?;
        // Stage transport changes separately. No partial invocation commit on
        // malformed internal result or out-of-profile output.
        let mut path = inv.path.clone();
        let mut query = inv.query.clone();
        let mut headers = inv.headers.clone();
        let mut body = inv.body.clone();
        if let Some(value) = output["requestPath"].as_str() {
            path = value.into();
        }
        if let Some(value) = output.get("requestBody") {
            body = value
                .as_str()
                .map(|value| bytes::Bytes::copy_from_slice(value.as_bytes()));
        }
        if let Some(raw) = output["queryString"].as_str() {
            let value: Value = serde_json::from_str(raw).map_err(|_| Fault::Internal)?;
            profile::tree(&value, profile::LITERAL_BYTES, profile::CONTEXT_NODES)
                .map_err(|_| Fault::OutputProfile)?;
            query = MultiMap(
                value
                    .as_object()
                    .ok_or(Fault::Internal)?
                    .iter()
                    .map(|(k, v)| java_string(v).map(|v| (k.clone(), v)))
                    .collect::<Result<_, _>>()?,
            );
        }
        header_changes_with_media(&mut headers, &output["requestHeaders"], Some(&mut media))?;
        if output.get("validationError").is_some() {
            let status: Value =
                serde_json::from_str(output["errorMessage"].as_str().ok_or(Fault::Internal)?)
                    .map_err(|_| Fault::Internal)?;
            let mut error = StatusError::new(
                status["statusCode"].as_u64().ok_or(Fault::Internal)? as u16,
                status["code"].as_str().ok_or(Fault::Internal)?,
                status["message"].as_str().ok_or(Fault::Internal)?,
                status["description"].as_str().ok_or(Fault::Internal)?,
            );
            error.severity = status["severity"].as_str().ok_or(Fault::Internal)?.into();
            if let Some(metadata) = status.get("metadata").and_then(Value::as_object) {
                error.metadata = Some(Box::new(
                    metadata
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ));
            }
            return Ok((
                HandlerOutcome::Reject(error),
                state.validator_media_snapshot(),
            ));
        }
        let staged = Invocation {
            method: inv.method.clone(),
            raw_path: String::new(),
            path,
            raw_query: None,
            query,
            headers,
            body,
            body_was_base64: inv.body_was_base64,
            source: SourceMetadata::default(),
            request_id: String::new(),
            deadline: inv.deadline,
            response_deadline: inv.response_deadline,
            cancel: inv.cancel.clone(),
            path_params: inv.path_params.clone(),
            route: None,
            // Keep only ingress map presence for staged budget validation.
            // Never clone the unbounded original event or its caller data.
            original: Value::Object(
                [
                    "queryStringParameters",
                    "multiValueQueryStringParameters",
                    "pathParameters",
                ]
                .into_iter()
                .filter(|name| inv.original.get(*name).is_some_and(Value::is_object))
                .map(|name| (name.into(), Value::Object(Map::new())))
                .collect(),
            ),
        };
        // Java query/header conversion changes the transport representation.
        // Count that representation too, before committing any invocation field.
        context(&staged, state, None).map_err(|_| Fault::OutputProfile)?;
        if inv.cancel.is_cancelled() || tokio::time::Instant::now() >= inv.deadline {
            return Err(Fault::Deadline);
        }
        inv.path = staged.path;
        inv.query = staged.query;
        inv.headers = staged.headers;
        inv.body = staged.body;
        if let Some(body) = output.get("responseBody").and_then(Value::as_str) {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            return Ok((
                HandlerOutcome::ContinueWithInitialResponse(GatewayResponse {
                    status: http::StatusCode::from_u16(
                        output["statusCode"].as_u64().unwrap_or(200) as u16,
                    )
                    .map_err(|_| Fault::Internal)?,
                    headers,
                    body: Some(bytes::Bytes::copy_from_slice(body.as_bytes())),
                    body_is_binary: false,
                }),
                media,
            ));
        }
        Ok((HandlerOutcome::Continue, media))
    }
    /// Staged response changes are committed only after complete evaluation.
    pub async fn response(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        response: &GatewayResponse,
    ) -> Result<GatewayResponse, Fault> {
        if !self.applies(inv) {
            return Ok(response.clone());
        }
        let output = self.run(inv, state, Some(response)).await?;
        let mut result = response.clone();
        header_changes(&mut result.headers, &output["responseHeaders"])?;
        if let Some(body) = output["responseBody"].as_str() {
            result.body = Some(bytes::Bytes::copy_from_slice(body.as_bytes()));
            result.body_is_binary = false;
        }
        if inv.cancel.is_cancelled() || tokio::time::Instant::now() >= inv.response_deadline {
            return Err(Fault::Deadline);
        }
        Ok(result)
    }
}

/// Read-only qualification view of the exact production response input builder.
/// This performs no evaluation or state mutation; ordinary handlers are unchanged.
pub fn observe_response_context(
    inv: &Invocation,
    state: &ExchangeState,
    response: &GatewayResponse,
) -> Result<Value, Fault> {
    context(inv, state, Some(response))
}
