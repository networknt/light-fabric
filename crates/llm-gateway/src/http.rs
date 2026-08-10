use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use model_provider::inference::{
    ClientProtocol, ContentBlock, EmbeddingRequest, GenerateOutputItem, InferenceError,
    InferenceErrorCategory, InferenceRequest, InferenceResponse, OpenAiCompatibilityProfile,
    ProviderProtocol,
};
use model_provider::providers::openai::OpenAiResponsesCodec;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;

use crate::error::LlmGatewayError;
use crate::runtime::{
    EmbeddingMemoryPermit, EmbeddingSpaceExpectation, EmbeddingSpaceSelection, LlmRequestContext,
    LlmRuntime, LlmStreamExecution, ResponsesResponseMetadata,
};

#[derive(Debug, Clone)]
pub struct BufferedHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub principal_id: String,
    pub trusted_request_id: String,
}

#[derive(Debug)]
pub struct BufferedHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub lifecycle: Option<BufferedResponseLifecycle>,
}

#[derive(Debug)]
pub struct BufferedResponseLifecycle {
    pub memory_permit: EmbeddingMemoryPermit,
    pub write_timeout: Duration,
    pub minimum_drain_bytes_per_second: u64,
}

pub struct StreamingHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub stream: LlmStreamExecution,
}

pub enum LlmHttpResponse {
    Buffered(BufferedHttpResponse),
    Streaming(Box<StreamingHttpResponse>),
}

#[async_trait]
pub trait BodyAccessControl: Send + Sync {
    async fn authorize(
        &self,
        request: &BufferedHttpRequest,
        body: &[u8],
    ) -> Result<(), LlmGatewayError>;
}

/// Marker used only when an enclosing gateway chain has already completed
/// body-aware authorization over the exact captured bytes. Production callers
/// must carry independent proof of that decision before invoking this adapter.
pub struct PreauthorizedBodyAccessControl;

#[async_trait]
impl BodyAccessControl for PreauthorizedBodyAccessControl {
    async fn authorize(
        &self,
        _request: &BufferedHttpRequest,
        _body: &[u8],
    ) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

pub struct LlmBufferedHttp {
    runtime: Arc<LlmRuntime>,
    access: Arc<dyn BodyAccessControl>,
    max_body_bytes: usize,
    max_json_depth: usize,
    timeout: Duration,
    parser: OpenAiCompatibilityProfile,
}

impl LlmBufferedHttp {
    pub fn new(
        runtime: Arc<LlmRuntime>,
        access: Arc<dyn BodyAccessControl>,
        max_body_bytes: usize,
        max_json_depth: usize,
        timeout: Duration,
    ) -> Self {
        Self {
            runtime,
            access,
            max_body_bytes,
            max_json_depth,
            timeout,
            parser: OpenAiCompatibilityProfile::default(),
        }
    }

    pub fn with_openai_extension_allowlist(mut self, allowlist: BTreeSet<String>) -> Self {
        self.parser.extension_allowlist = allowlist;
        self
    }

    pub async fn handle(&self, request: BufferedHttpRequest) -> BufferedHttpResponse {
        let request_id = request.trusted_request_id.clone();
        match self.handle_route(request).await {
            LlmHttpResponse::Buffered(response) => response,
            LlmHttpResponse::Streaming(_) => public_error(
                LlmGatewayError::InvalidRequest(
                    "streaming response requires a streaming writer".to_string(),
                ),
                &request_id,
            ),
        }
    }

    pub async fn handle_route(&self, request: BufferedHttpRequest) -> LlmHttpResponse {
        self.handle_route_with_embedding_ingress(request, None)
            .await
    }

    pub async fn handle_route_with_embedding_ingress(
        &self,
        request: BufferedHttpRequest,
        mut ingress_permit: Option<OwnedSemaphorePermit>,
    ) -> LlmHttpResponse {
        let result = self.handle_inner(&request, &mut ingress_permit).await;
        match result {
            Ok(LlmHttpResponse::Buffered(mut response)) => {
                response
                    .headers
                    .insert("x-request-id".to_string(), request.trusted_request_id);
                LlmHttpResponse::Buffered(response)
            }
            Ok(LlmHttpResponse::Streaming(mut response)) => {
                response
                    .headers
                    .insert("x-request-id".to_string(), request.trusted_request_id);
                LlmHttpResponse::Streaming(response)
            }
            Err(error) => {
                LlmHttpResponse::Buffered(public_error(error, &request.trusted_request_id))
            }
        }
    }

    async fn handle_inner(
        &self,
        request: &BufferedHttpRequest,
        ingress_permit: &mut Option<OwnedSemaphorePermit>,
    ) -> Result<LlmHttpResponse, LlmGatewayError> {
        if request.path == "/v1/models" {
            if request.method != "GET" {
                return Err(LlmGatewayError::MethodNotAllowed);
            }
            let data = self
                .runtime
                .visible_models()
                .into_iter()
                .map(|id| json!({"id":id,"object":"model","owned_by":"light-gateway"}))
                .collect::<Vec<_>>();
            return json_response(200, json!({"object":"list","data":data}))
                .map(LlmHttpResponse::Buffered);
        }
        if let Some(alias) = request.path.strip_prefix("/v1/models/") {
            if request.method != "GET" {
                return Err(LlmGatewayError::MethodNotAllowed);
            }
            if alias.is_empty()
                || alias.contains('/')
                || !self
                    .runtime
                    .visible_models()
                    .iter()
                    .any(|model| model == alias)
            {
                return Err(LlmGatewayError::AliasNotFound);
            }
            return json_response(
                200,
                json!({"id":alias,"object":"model","owned_by":"light-gateway"}),
            )
            .map(LlmHttpResponse::Buffered);
        }
        let embedding_route = request.path == "/v1/embeddings";
        let responses_route = request.path == "/v1/responses";
        if request.path != "/v1/chat/completions" && !embedding_route && !responses_route {
            return Err(LlmGatewayError::RouteNotFound);
        }
        if request.method != "POST" {
            return Err(LlmGatewayError::MethodNotAllowed);
        }
        let content_type = request
            .headers
            .get("content-type")
            .map(String::as_str)
            .unwrap_or_default();
        if !content_type
            .split(';')
            .next()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
        {
            return Err(LlmGatewayError::UnsupportedMediaType);
        }
        if request.headers.contains_key("content-encoding") {
            return Err(LlmGatewayError::UnsupportedMediaType);
        }
        let root = self.runtime.snapshot();
        let max_body_bytes = if embedding_route {
            root.embedding_memory.max_request_body_bytes
        } else {
            self.max_body_bytes
        };
        if request
            .headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > max_body_bytes)
            || request.body.len() > max_body_bytes
        {
            return Err(LlmGatewayError::PayloadTooLarge);
        }

        // Body-aware authorization is deliberately before JSON/alias parsing.
        if embedding_route {
            tokio::time::timeout(
                Duration::from_millis(root.embedding_memory.authorization_timeout_ms),
                self.access.authorize(request, &request.body),
            )
            .await
            .map_err(|_| LlmGatewayError::ProviderUnavailable)??;
            let expectation = parse_embedding_space_expectation(&request.headers)?;
            let maximum_billed_cost_micros = parse_embedding_cost_ceiling(&request.headers)?;
            let raw: Value = serde_json::from_slice(&request.body)
                .map_err(|_| LlmGatewayError::InvalidRequest("invalid JSON".to_string()))?;
            if json_depth(&raw) > self.max_json_depth {
                return Err(LlmGatewayError::InvalidRequest(
                    "JSON nesting limit exceeded".to_string(),
                ));
            }
            let probe = embedding_admission_probe(&raw)?;
            let selection = match self.runtime.probe_embedding_space(
                &root,
                &request.principal_id,
                probe.model,
                expectation.as_ref(),
                probe.dimensions,
            ) {
                Ok(selection) => selection,
                Err(error @ LlmGatewayError::UnsupportedCapability(_)) => {
                    self.runtime
                        .audit_embedding_space_rejection(
                            &root,
                            &request.principal_id,
                            probe.model,
                            expectation.as_ref(),
                        )
                        .await?;
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            if root.embedding_memory.admission_slots == 0 {
                drop(ingress_permit.take());
                return Err(LlmGatewayError::AliasNotFound);
            }
            let memory_permit = self.runtime.try_acquire_embedding_memory_slot(&root)?;
            drop(ingress_permit.take());
            return self
                .handle_embeddings(
                    request,
                    raw,
                    root,
                    memory_permit,
                    expectation,
                    selection,
                    maximum_billed_cost_micros,
                )
                .await
                .map(LlmHttpResponse::Buffered);
        }
        self.access.authorize(request, &request.body).await?;
        let mut raw: Value = serde_json::from_slice(&request.body)
            .map_err(|_| LlmGatewayError::InvalidRequest("invalid JSON".to_string()))?;
        if json_depth(&raw) > self.max_json_depth {
            return Err(LlmGatewayError::InvalidRequest(
                "JSON nesting limit exceeded".to_string(),
            ));
        }
        if raw.get("model").and_then(Value::as_str).is_none() {
            return Err(LlmGatewayError::InvalidRequest(
                "model is required".to_string(),
            ));
        }
        let streaming = match raw.get("stream") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            Some(_) => {
                return Err(LlmGatewayError::InvalidRequest(
                    "stream must be a boolean".to_string(),
                ));
            }
        };
        let client_include_usage = if streaming && !responses_route {
            match raw.get("stream_options") {
                None | Some(Value::Null) => false,
                Some(Value::Object(options))
                    if options.keys().all(|key| key.as_str() == "include_usage") =>
                {
                    match options.get("include_usage") {
                        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
                        Some(Value::Bool(true)) => true,
                        Some(_) => {
                            return Err(LlmGatewayError::InvalidRequest(
                                "stream_options.include_usage must be a boolean".to_string(),
                            ));
                        }
                    }
                }
                Some(_) => {
                    return Err(LlmGatewayError::InvalidRequest(
                        "stream_options contains unsupported fields".to_string(),
                    ));
                }
            }
        } else {
            false
        };
        if streaming && !responses_route {
            let object = raw.as_object_mut().ok_or_else(|| {
                LlmGatewayError::InvalidRequest("request must be a JSON object".to_string())
            })?;
            object.insert("stream".to_string(), Value::Bool(false));
            object.remove("stream_options");
        }
        let parse_body = if streaming && !responses_route {
            serde_json::to_vec(&raw)
                .map_err(|_| LlmGatewayError::InvalidRequest("invalid JSON".to_string()))?
        } else {
            request.body.clone()
        };
        let responses_metadata =
            responses_route.then(|| ResponsesResponseMetadata::from_validated_request(&raw));
        let mut canonical: InferenceRequest = if responses_route {
            let (canonical, parsed_stream) = OpenAiResponsesCodec
                .parse_client_request(&parse_body)
                .map_err(client_codec_error)?;
            if parsed_stream != streaming {
                return Err(LlmGatewayError::InvalidRequest(
                    "stream contains an inconsistent value".to_string(),
                ));
            }
            canonical
        } else {
            self.parser
                .parse_request(&parse_body, ProviderProtocol::OpenAiChat)
                .map_err(|error| LlmGatewayError::InvalidRequest(error.detail))?
        };
        let formats =
            self.runtime
                .eligible_formats(&root, &request.principal_id, &canonical, streaming)?;
        if !responses_route && formats.contains(&ProviderProtocol::AnthropicMessages) {
            canonical = self
                .parser
                .parse_request(&parse_body, ProviderProtocol::AnthropicMessages)
                .map_err(|error| LlmGatewayError::InvalidRequest(error.detail))?;
        }
        if canonical.messages.len() > 256 || canonical.tools.len() > 128 {
            return Err(LlmGatewayError::InvalidRequest(
                "message or tool count limit exceeded".to_string(),
            ));
        }
        let schema_bytes = canonical
            .tools
            .iter()
            .map(|tool| {
                serde_json::to_vec(&tool.input_schema).map_or(usize::MAX, |bytes| bytes.len())
            })
            .fold(0_usize, usize::saturating_add);
        if schema_bytes > 256 * 1024 {
            return Err(LlmGatewayError::InvalidRequest(
                "tool schema size limit exceeded".to_string(),
            ));
        }
        validate_images(&canonical)?;
        let context = LlmRequestContext {
            // Audit/request identity is always gateway-issued UUIDv7. The
            // independently trusted correlation ID remains the response
            // header and is never forced into the audit database UUID key.
            request_id: uuid::Uuid::now_v7().to_string(),
            principal_id: request.principal_id.clone(),
            deadline: std::time::Instant::now() + self.timeout,
        };
        if streaming {
            let stream = self
                .runtime
                .execute_stream_with_snapshot_protocol(
                    context,
                    root,
                    canonical,
                    if responses_route {
                        ClientProtocol::OpenAiResponses
                    } else {
                        ClientProtocol::OpenAiChat
                    },
                    client_include_usage,
                    responses_metadata.clone(),
                )
                .await?;
            return Ok(LlmHttpResponse::Streaming(Box::new(
                StreamingHttpResponse {
                    status: 200,
                    headers: BTreeMap::from([
                        ("content-type".to_string(), "text/event-stream".to_string()),
                        ("cache-control".to_string(), "no-cache".to_string()),
                        ("x-accel-buffering".to_string(), "no".to_string()),
                    ]),
                    stream,
                },
            )));
        }
        let execution = self
            .runtime
            .execute_with_snapshot(context, root, canonical)
            .await?;
        if responses_route {
            return render_responses_response(
                &execution.request_id,
                &execution.alias,
                execution.response,
                responses_metadata.as_ref(),
            )
            .map(LlmHttpResponse::Buffered);
        }
        let mut text = String::new();
        let mut refusal: Option<String> = None;
        let mut tool_calls = Vec::new();
        for item in execution.response.output {
            match item {
                GenerateOutputItem::Message { content, .. } => for block in content {
                    match block {
                        ContentBlock::Text { text: value } => text.push_str(&value),
                        ContentBlock::Refusal { refusal: value } => {
                            if let Some(existing) = &mut refusal { existing.push_str(&value); } else { refusal = Some(value); }
                        }
                        _ => {}
                    }
                },
                GenerateOutputItem::FunctionCall { call_id, name, arguments, .. } => tool_calls.push(json!({
                    "id":call_id,"type":"function","function":{"name":name,"arguments":serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string())}
                })),
                GenerateOutputItem::ReasoningSummary { .. } => {}
            }
        }
        let usage = execution.response.usage.unwrap_or_default();
        let total_tokens = usage
            .input_tokens
            .zip(usage.output_tokens)
            .map(|(input, output)| input.saturating_add(output));
        let mut message = json!({
            "role":"assistant",
            "content":text,
            "tool_calls":tool_calls
        });
        if let Some(refusal) = refusal {
            message["content"] = Value::Null;
            message["refusal"] = Value::String(refusal);
        }
        json_response(
            200,
            json!({
                "id":format!("chatcmpl-{}", execution.request_id), "object":"chat.completion",
                "model":execution.alias, "choices":[{"index":0,"message":message,"finish_reason":execution.response.finish_reason}],
                "usage":{"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":total_tokens}
            }),
        )
        .map(LlmHttpResponse::Buffered)
    }

    async fn handle_embeddings(
        &self,
        request: &BufferedHttpRequest,
        raw: Value,
        root: Arc<crate::runtime::LlmPublishedSnapshot>,
        memory_permit: EmbeddingMemoryPermit,
        expectation: Option<EmbeddingSpaceExpectation>,
        selection: EmbeddingSpaceSelection,
        maximum_billed_cost_micros: Option<u64>,
    ) -> Result<BufferedHttpResponse, LlmGatewayError> {
        let object = raw.as_object().ok_or_else(|| {
            LlmGatewayError::InvalidRequest("request must be a JSON object".to_string())
        })?;
        const ALLOWED: &[&str] = &["model", "input", "encoding_format", "dimensions", "user"];
        if let Some(field) = object
            .keys()
            .find(|field| !ALLOWED.contains(&field.as_str()))
        {
            return Err(LlmGatewayError::InvalidRequest(format!(
                "unsupported embeddings field `{field}`"
            )));
        }
        let model = object
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| LlmGatewayError::InvalidRequest("model is required".to_string()))?
            .to_string();
        if let Some(user) = object.get("user").filter(|value| !value.is_null()) {
            let user = user.as_str().ok_or_else(|| {
                LlmGatewayError::InvalidRequest("user must be a string".to_string())
            })?;
            if user.len() > 512 {
                return Err(LlmGatewayError::InvalidRequest(
                    "user exceeds the compatibility value limit".to_string(),
                ));
            }
        }
        let encoding = match object
            .get("encoding_format")
            .filter(|value| !value.is_null())
            .map(Value::as_str)
        {
            None | Some(Some("float")) => ClientEmbeddingEncoding::Float,
            Some(Some("base64")) => ClientEmbeddingEncoding::Base64,
            _ => {
                return Err(LlmGatewayError::InvalidRequest(
                    "encoding_format must be `float` or `base64`".to_string(),
                ));
            }
        };
        let mut dimensions = match object.get("dimensions").filter(|value| !value.is_null()) {
            None => None,
            Some(value) => Some(
                value
                    .as_u64()
                    .and_then(|value| u32::try_from(value).ok())
                    .filter(|value| *value > 0)
                    .ok_or_else(|| {
                        LlmGatewayError::InvalidRequest(
                            "dimensions must be a positive integer".to_string(),
                        )
                    })?,
            ),
        };
        if selection.required && dimensions.is_none() {
            dimensions = Some(selection.contract.dimension);
        }
        let input = object
            .get("input")
            .ok_or_else(|| LlmGatewayError::InvalidRequest("input is required".to_string()))?;
        let inputs = match input {
            Value::String(value) => vec![value.clone()],
            Value::Array(values) if values.is_empty() => {
                return Err(LlmGatewayError::InvalidRequest(
                    "input array must not be empty".to_string(),
                ));
            }
            Value::Array(values) if values.iter().all(Value::is_string) => values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            Value::Array(_) => {
                return Err(LlmGatewayError::UnsupportedCapability(
                    "token-array embedding inputs are not supported".to_string(),
                ));
            }
            _ => {
                return Err(LlmGatewayError::InvalidRequest(
                    "input must be a string or an array of strings".to_string(),
                ));
            }
        };
        if inputs.iter().any(String::is_empty) {
            return Err(LlmGatewayError::InvalidRequest(
                "embedding input strings must not be empty".to_string(),
            ));
        }
        if inputs
            .iter()
            .any(|value| value.len() > root.embedding_memory.max_input_bytes_per_item)
            || inputs
                .iter()
                .try_fold(0_usize, |total, value| total.checked_add(value.len()))
                .is_none_or(|total| total > root.embedding_memory.max_total_input_bytes)
        {
            return Err(LlmGatewayError::PayloadTooLarge);
        }
        let context = LlmRequestContext {
            request_id: request.trusted_request_id.clone(),
            principal_id: request.principal_id.clone(),
            deadline: std::time::Instant::now() + self.timeout,
        };
        let execution = self
            .runtime
            .execute_embedding_with_snapshot_expectation_and_budget(
                context,
                Arc::clone(&root),
                EmbeddingRequest {
                    model,
                    inputs,
                    dimensions,
                },
                expectation.clone(),
                maximum_billed_cost_micros,
            )
            .await?;
        let data = execution
            .response
            .vectors
            .iter()
            .map(|vector| {
                let embedding = match encoding {
                    ClientEmbeddingEncoding::Float => json!(vector.values),
                    ClientEmbeddingEncoding::Base64 => Value::String(
                        STANDARD.encode(
                            vector
                                .values
                                .iter()
                                .flat_map(|value| value.to_le_bytes())
                                .collect::<Vec<_>>(),
                        ),
                    ),
                };
                json!({"object":"embedding","embedding":embedding,"index":vector.index})
            })
            .collect::<Vec<_>>();
        let input_tokens = execution.response.usage.input_tokens;
        let body = serde_json::to_vec(&json!({
            "object":"list",
            "data":data,
            "model":execution.alias,
            "usage":{"prompt_tokens":input_tokens,"total_tokens":input_tokens}
        }))
        .map_err(|_| {
            LlmGatewayError::Invariant("embedding response rendering failed".to_string())
        })?;
        if body.len() > root.embedding_memory.max_rendered_response_bytes {
            return Err(LlmGatewayError::Invariant(
                "rendered embedding response exceeds the compiled bound".to_string(),
            ));
        }
        let mut headers =
            BTreeMap::from([("content-type".to_string(), "application/json".to_string())]);
        if expectation.is_some() || execution.selected_space.required {
            headers.insert(
                "x-light-embedding-space-id".to_string(),
                execution.selected_space.contract.space_id,
            );
            headers.insert(
                "x-light-embedding-space-revision".to_string(),
                execution.selected_space.contract.revision.to_string(),
            );
            headers.insert(
                "x-light-config-generation".to_string(),
                execution.generation.to_string(),
            );
        }
        headers.insert(
            "x-light-billed-cost-micros".to_string(),
            execution.usage.charged_micros.to_string(),
        );
        Ok(BufferedHttpResponse {
            status: 200,
            headers,
            body,
            lifecycle: Some(BufferedResponseLifecycle {
                memory_permit,
                write_timeout: Duration::from_millis(root.embedding_memory.write_timeout_ms),
                minimum_drain_bytes_per_second: root
                    .embedding_memory
                    .minimum_drain_bytes_per_second,
            }),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ClientEmbeddingEncoding {
    Float,
    Base64,
}

struct EmbeddingAdmissionProbe<'a> {
    model: &'a str,
    dimensions: Option<u32>,
}

fn embedding_admission_probe(raw: &Value) -> Result<EmbeddingAdmissionProbe<'_>, LlmGatewayError> {
    let object = raw.as_object().ok_or_else(|| {
        LlmGatewayError::InvalidRequest("request must be a JSON object".to_string())
    })?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| LlmGatewayError::InvalidRequest("model is required".to_string()))?;
    let dimensions = match object.get("dimensions").filter(|value| !value.is_null()) {
        None => None,
        Some(value) => Some(
            value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .filter(|value| *value > 0)
                .ok_or_else(|| {
                    LlmGatewayError::InvalidRequest(
                        "dimensions must be a positive integer".to_string(),
                    )
                })?,
        ),
    };
    Ok(EmbeddingAdmissionProbe { model, dimensions })
}

fn parse_embedding_space_expectation(
    headers: &BTreeMap<String, String>,
) -> Result<Option<EmbeddingSpaceExpectation>, LlmGatewayError> {
    let id = headers.get("x-light-expected-embedding-space-id");
    let revision = headers.get("x-light-expected-embedding-space-revision");
    match (id, revision) {
        (None, None) => Ok(None),
        (Some(id), Some(revision)) => {
            let id = id.trim();
            let revision = revision.parse::<u64>().ok().filter(|value| *value > 0);
            if id.is_empty() || id.len() > 255 || revision.is_none() {
                return Err(LlmGatewayError::InvalidRequest(
                    "expected embedding-space headers are malformed".to_string(),
                ));
            }
            Ok(Some(EmbeddingSpaceExpectation {
                space_id: id.to_string(),
                revision: revision.expect("checked above"),
            }))
        }
        _ => Err(LlmGatewayError::InvalidRequest(
            "expected embedding-space headers must be supplied together".to_string(),
        )),
    }
}

fn parse_embedding_cost_ceiling(
    headers: &BTreeMap<String, String>,
) -> Result<Option<u64>, LlmGatewayError> {
    headers
        .get("x-light-maximum-billed-cost-micros")
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                LlmGatewayError::InvalidRequest(
                    "maximum billed embedding cost header is malformed".to_string(),
                )
            })
        })
        .transpose()
}

fn client_codec_error(error: InferenceError) -> LlmGatewayError {
    if error.category == InferenceErrorCategory::UnsupportedFeature {
        LlmGatewayError::UnsupportedCapability(error.detail)
    } else {
        LlmGatewayError::InvalidRequest(error.detail)
    }
}

fn render_responses_response(
    request_id: &str,
    alias: &str,
    response: InferenceResponse,
    metadata: Option<&ResponsesResponseMetadata>,
) -> Result<BufferedHttpResponse, LlmGatewayError> {
    let mut output = Vec::with_capacity(response.output.len());
    for (index, item) in response.output.into_iter().enumerate() {
        match item {
            GenerateOutputItem::Message {
                role,
                content,
                status,
                ..
            } => {
                let content = content
                    .into_iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(json!({
                            "type":"output_text","text":text,"annotations":[],"logprobs":[]
                        })),
                        ContentBlock::Refusal { refusal } => Some(json!({
                            "type":"refusal","refusal":refusal
                        })),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                output.push(json!({
                    "id":format!("msg_{request_id}_{index}"),"type":"message",
                    "role":role,"status":status,"content":content
                }));
                continue;
            }
            GenerateOutputItem::FunctionCall {
                call_id,
                name,
                arguments,
                status,
                ..
            } => {
                if call_id.is_empty() || call_id.len() > 512 || name.is_empty() || name.len() > 256
                {
                    return Err(LlmGatewayError::Invariant(
                        "provider returned an invalid Responses function identifier".to_string(),
                    ));
                }
                output.push(json!({
                    "id":format!("fc_{request_id}_{index}"),"type":"function_call",
                    "call_id":call_id,"name":name,
                    "arguments":serde_json::to_string(&arguments).map_err(|_| LlmGatewayError::Invariant("Responses function arguments failed to render".to_string()))?,
                    "status":status
                }));
                continue;
            }
            GenerateOutputItem::ReasoningSummary {
                summary, status, ..
            } => {
                output.push(json!({
                    "id":format!("rs_{request_id}_{index}"),"type":"reasoning","status":status,
                    "summary":summary.into_iter().map(|text| json!({"type":"summary_text","text":text})).collect::<Vec<_>>()
                }));
                continue;
            }
        }
    }
    let usage = response.usage.unwrap_or_default();
    let status = match response.terminal_state {
        model_provider::inference::TerminalState::Complete => "completed",
        model_provider::inference::TerminalState::Cancelled => "cancelled",
        model_provider::inference::TerminalState::Failed => "incomplete",
    };
    let incomplete_details = (response.finish_reason
        == model_provider::inference::FinishReason::Length)
        .then(|| json!({"reason":"max_output_tokens"}));
    let mut rendered = json!({
        "id":format!("resp_{request_id}"),"object":"response",
        "created_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |value| value.as_secs()),
        "status":status,"background":false,"error":null,"incomplete_details":incomplete_details,
        "instructions":null,"max_output_tokens":null,"model":alias,"output":output,
        "parallel_tool_calls":false,"previous_response_id":null,
        "reasoning":{"effort":null,"summary":null},"store":false,"temperature":null,
        "text":{"format":{"type":"text"}},"tool_choice":"auto","tools":[],"top_p":null,
        "truncation":"disabled","metadata":{},
        "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,
            "total_tokens":usage.input_tokens.zip(usage.output_tokens).map(|(input,output)| input.saturating_add(output)),
            "input_tokens_details":{"cached_tokens":usage.cached_input_tokens},
            "output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}}
    });
    metadata.cloned().unwrap_or_default().apply(&mut rendered);
    json_response(200, rendered)
}

fn validate_images(request: &InferenceRequest) -> Result<(), LlmGatewayError> {
    for source in request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::Image { source } => Some(source),
            _ => None,
        })
    {
        let valid = source.url.starts_with("https://") || source.url.starts_with("data:image/");
        if !valid {
            return Err(LlmGatewayError::InvalidRequest(
                "image URL must use https or an image data URL".to_string(),
            ));
        }
    }
    Ok(())
}

fn json_depth(value: &Value) -> usize {
    match value {
        Value::Array(values) => 1 + values.iter().map(json_depth).max().unwrap_or(0),
        Value::Object(values) => 1 + values.values().map(json_depth).max().unwrap_or(0),
        _ => 1,
    }
}

fn json_response(status: u16, value: Value) -> Result<BufferedHttpResponse, LlmGatewayError> {
    Ok(BufferedHttpResponse {
        status,
        headers: BTreeMap::from([("content-type".to_string(), "application/json".to_string())]),
        body: serde_json::to_vec(&value)
            .map_err(|error| LlmGatewayError::InvalidRequest(error.to_string()))?,
        lifecycle: None,
    })
}

fn public_error(error: LlmGatewayError, request_id: &str) -> BufferedHttpResponse {
    let status = error.public_status();
    let retry_after = match &error {
        LlmGatewayError::Provider(error)
            if error.category == InferenceErrorCategory::RateLimited =>
        {
            error
                .retry_after_ms
                .map(|milliseconds| milliseconds.saturating_add(999) / 1_000)
        }
        _ => None,
    };
    let message = match &error {
        LlmGatewayError::InvalidRequest(detail) => detail.as_str(),
        LlmGatewayError::MethodNotAllowed => "The method is not allowed",
        LlmGatewayError::UnsupportedMediaType => "The request media type is not supported",
        LlmGatewayError::PayloadTooLarge => "The request body is too large",
        LlmGatewayError::RouteNotFound => "The requested route is not available",
        LlmGatewayError::AliasNotFound => "The requested model is not available",
        LlmGatewayError::UnsupportedCapability(_) => "The requested capability is not supported",
        LlmGatewayError::NoReadyDeployment => "No deployment is currently ready",
        LlmGatewayError::Invariant(_) => "The gateway encountered an internal error",
        LlmGatewayError::Forbidden => "The request was denied",
        LlmGatewayError::Capacity | LlmGatewayError::Budget => "Request capacity is exhausted",
        LlmGatewayError::Provider(error)
            if matches!(
                error.category,
                InferenceErrorCategory::InvalidRequest | InferenceErrorCategory::UnsupportedFeature
            ) =>
        {
            "The request was rejected by the model provider"
        }
        _ => "The model provider is unavailable",
    };
    let body = serde_json::to_vec(
        &json!({"error":{"message":message,"type":error.public_code(),"code":error.public_code()}}),
    )
    .unwrap_or_default();
    let mut headers = BTreeMap::from([
        ("content-type".to_string(), "application/json".to_string()),
        ("x-request-id".to_string(), request_id.to_string()),
    ]);
    if let Some(seconds) = retry_after {
        headers.insert("retry-after".to_string(), seconds.to_string());
    }
    BufferedHttpResponse {
        status,
        headers,
        body,
        lifecycle: None,
    }
}
