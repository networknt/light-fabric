use async_trait::async_trait;
use model_provider::inference::{
    ContentBlock, InferenceErrorCategory, InferenceRequest, OpenAiCompatibilityProfile,
    ProviderFormat,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use crate::error::LlmGatewayError;
use crate::runtime::{LlmRequestContext, LlmRuntime, LlmStreamExecution};

#[derive(Debug, Clone)]
pub struct BufferedHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub principal_id: String,
    pub trusted_request_id: String,
}

#[derive(Debug, Clone)]
pub struct BufferedHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub struct StreamingHttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub stream: LlmStreamExecution,
}

pub enum LlmHttpResponse {
    Buffered(BufferedHttpResponse),
    Streaming(StreamingHttpResponse),
}

#[async_trait]
pub trait BodyAccessControl: Send + Sync {
    async fn authorize(
        &self,
        request: &BufferedHttpRequest,
        body: &[u8],
    ) -> Result<(), LlmGatewayError>;
}

pub struct AllowBodyAccessControl;

#[async_trait]
impl BodyAccessControl for AllowBodyAccessControl {
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
        let result = self.handle_inner(&request).await;
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
        if request.path != "/v1/chat/completions" {
            return Err(LlmGatewayError::ModelUnavailable);
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
        if request
            .headers
            .get("content-length")
            .and_then(|value| value.parse::<usize>().ok())
            .is_some_and(|length| length > self.max_body_bytes)
            || request.body.len() > self.max_body_bytes
        {
            return Err(LlmGatewayError::PayloadTooLarge);
        }

        // Body-aware authorization is deliberately before JSON/alias parsing.
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
        if streaming {
            raw.as_object_mut()
                .ok_or_else(|| {
                    LlmGatewayError::InvalidRequest("request must be a JSON object".to_string())
                })?
                .insert("stream".to_string(), Value::Bool(false));
        }
        let parse_body = if streaming {
            serde_json::to_vec(&raw)
                .map_err(|_| LlmGatewayError::InvalidRequest("invalid JSON".to_string()))?
        } else {
            request.body.clone()
        };
        let root = self.runtime.snapshot();
        let mut canonical: InferenceRequest = self
            .parser
            .parse_request(&parse_body, ProviderFormat::OpenAi)
            .map_err(|error| LlmGatewayError::InvalidRequest(error.detail))?;
        let formats = self
            .runtime
            .eligible_formats(&root, &request.principal_id, &canonical)?;
        if formats.contains(&ProviderFormat::Anthropic) {
            canonical = self
                .parser
                .parse_request(&parse_body, ProviderFormat::Anthropic)
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
            request_id: request.trusted_request_id.clone(),
            principal_id: request.principal_id.clone(),
            deadline: std::time::Instant::now() + self.timeout,
        };
        if streaming {
            let stream = self
                .runtime
                .execute_stream_with_snapshot(context, root, canonical)
                .await?;
            return Ok(LlmHttpResponse::Streaming(StreamingHttpResponse {
                status: 200,
                headers: BTreeMap::from([
                    ("content-type".to_string(), "text/event-stream".to_string()),
                    ("cache-control".to_string(), "no-cache".to_string()),
                    ("x-accel-buffering".to_string(), "no".to_string()),
                ]),
                stream,
            }));
        }
        let execution = self
            .runtime
            .execute_with_snapshot(context, root, canonical)
            .await?;
        let mut text = String::new();
        let mut tool_calls = Vec::new();
        for block in execution.response.content {
            match block {
                ContentBlock::Text { text: value } => text.push_str(&value),
                ContentBlock::ToolCall { call } => tool_calls.push(json!({
                    "id":call.id,"type":"function","function":{"name":call.name,"arguments":serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_string())}
                })),
                _ => {}
            }
        }
        let usage = execution.response.usage.unwrap_or_default();
        let total_tokens = usage
            .input_tokens
            .zip(usage.output_tokens)
            .map(|(input, output)| input.saturating_add(output));
        json_response(
            200,
            json!({
                "id":format!("chatcmpl-{}", execution.request_id), "object":"chat.completion",
                "model":execution.alias, "choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":tool_calls},"finish_reason":execution.response.finish_reason}],
                "usage":{"prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,"total_tokens":total_tokens}
            }),
        )
        .map(LlmHttpResponse::Buffered)
    }
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
        LlmGatewayError::ModelUnavailable => "The requested model is not available",
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
    }
}
