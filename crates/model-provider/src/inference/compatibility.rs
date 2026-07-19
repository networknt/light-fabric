use super::content::{ContentBlock, Message, Role};
use super::error::InferenceError;
use super::provider::{InferenceProvider, ProviderFormat, ProviderRequestContext};
use super::request::{
    InferenceRequest, ResponseFormat, SamplingOptions, TokenLimits, ToolChoice, ToolDefinition,
};
use crate::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities as LegacyCapabilities,
    TokenUsage, ToolCall as LegacyToolCall, ToolSpec,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibilityProfile {
    pub extension_allowlist: BTreeSet<String>,
    pub max_extension_bytes: usize,
}

impl Default for OpenAiCompatibilityProfile {
    fn default() -> Self {
        Self {
            extension_allowlist: BTreeSet::new(),
            max_extension_bytes: 16 * 1024,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PublicRequest {
    model: String,
    messages: Vec<PublicMessage>,
    #[serde(default)]
    tools: Vec<PublicTool>,
    #[serde(default)]
    tool_choice: Option<Value>,
    #[serde(default)]
    response_format: Option<Value>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    stop: Option<StopValue>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_completion_tokens: Option<u32>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct PublicMessage {
    role: String,
    content: Value,
    #[serde(default)]
    tool_call_id: Option<String>,
    #[serde(default)]
    tool_calls: Vec<PublicToolCall>,
}

#[derive(Debug, Deserialize)]
struct PublicToolCall {
    id: String,
    function: PublicFunctionCall,
}

#[derive(Debug, Deserialize)]
struct PublicFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct PublicTool {
    function: PublicToolFunction,
}

#[derive(Debug, Deserialize)]
struct PublicToolFunction {
    name: String,
    #[serde(default)]
    description: String,
    parameters: Value,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StopValue {
    One(String),
    Many(Vec<String>),
}

impl OpenAiCompatibilityProfile {
    pub fn parse_request(
        &self,
        bytes: &[u8],
        provider_format: ProviderFormat,
    ) -> Result<InferenceRequest, InferenceError> {
        let public: PublicRequest = serde_json::from_slice(bytes)
            .map_err(|error| InferenceError::invalid_request(format!("invalid JSON: {error}")))?;
        let token_limits =
            TokenLimits::from_openai(public.max_tokens, public.max_completion_tokens)?;
        let messages = public
            .messages
            .into_iter()
            .map(convert_public_message)
            .collect::<Result<Vec<_>, _>>()?;
        let tools = public
            .tools
            .into_iter()
            .map(|tool| ToolDefinition {
                name: tool.function.name,
                description: tool.function.description,
                input_schema: tool.function.parameters,
            })
            .collect();
        let tool_choice = public.tool_choice.map(parse_tool_choice).transpose()?;
        let response_format = public
            .response_format
            .map(parse_response_format)
            .transpose()?;
        let stop = match public.stop {
            None => Vec::new(),
            Some(StopValue::One(value)) => vec![value],
            Some(StopValue::Many(values)) => values,
        };
        let mut extensions = BTreeMap::new();
        let mut extension_bytes = 0_usize;
        for (name, value) in public.extra {
            if is_supported_default(&name, &value) {
                continue;
            }
            if provider_format != ProviderFormat::OpenAi {
                if !value.is_null() {
                    return Err(InferenceError::unsupported(format!(
                        "field `{name}` cannot be forwarded across provider formats"
                    )));
                }
                continue;
            }
            if !self.extension_allowlist.contains(&name) {
                if value.is_null() {
                    continue;
                }
                return Err(InferenceError::unsupported(format!(
                    "unsupported non-default field `{name}`"
                )));
            }
            extension_bytes = extension_bytes
                .checked_add(name.len())
                .and_then(|size| {
                    serde_json::to_vec(&value)
                        .ok()
                        .and_then(|encoded| size.checked_add(encoded.len()))
                })
                .ok_or_else(|| InferenceError::invalid_request("extension size overflow"))?;
            if extension_bytes > self.max_extension_bytes {
                return Err(InferenceError::invalid_request(
                    "allowlisted extension envelope exceeds configured limit",
                ));
            }
            extensions.insert(name, value);
        }
        Ok(InferenceRequest {
            model: public.model,
            messages,
            tools,
            tool_choice,
            response_format,
            sampling: SamplingOptions {
                temperature: public.temperature,
                top_p: public.top_p,
                stop,
            },
            token_limits,
            extensions,
        })
    }
}

fn convert_public_message(message: PublicMessage) -> Result<Message, InferenceError> {
    let role = match message.role.as_str() {
        "system" => Role::System,
        "user" => Role::User,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        other => {
            return Err(InferenceError::invalid_request(format!(
                "unknown role `{other}`"
            )));
        }
    };
    let mut content = match message.content {
        Value::Null => Vec::new(),
        Value::String(text) => vec![ContentBlock::text(text)],
        Value::Array(blocks) => blocks
            .into_iter()
            .map(convert_public_content)
            .collect::<Result<Vec<_>, _>>()?,
        _ => {
            return Err(InferenceError::invalid_request(
                "message content has invalid shape",
            ));
        }
    };
    for call in message.tool_calls {
        let arguments = serde_json::from_str(&call.function.arguments).map_err(|error| {
            InferenceError::invalid_request(format!("tool arguments are not JSON: {error}"))
        })?;
        content.push(ContentBlock::ToolCall {
            call: super::content::ToolCall {
                id: call.id,
                name: call.function.name,
                arguments,
            },
        });
    }
    if role == Role::Tool {
        let tool_call_id = message
            .tool_call_id
            .ok_or_else(|| InferenceError::invalid_request("tool message has no tool_call_id"))?;
        content = vec![ContentBlock::ToolResult {
            result: super::content::ToolResult {
                tool_call_id,
                content,
                is_error: false,
            },
        }];
    }
    Ok(Message { role, content })
}

fn convert_public_content(value: Value) -> Result<ContentBlock, InferenceError> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "text" => Ok(ContentBlock::text(
            value
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        )),
        "image_url" => {
            let image = value.get("image_url").ok_or_else(|| {
                InferenceError::invalid_request("image_url block is missing image_url")
            })?;
            let url = image
                .as_str()
                .or_else(|| image.get("url").and_then(Value::as_str))
                .ok_or_else(|| InferenceError::invalid_request("image_url block is missing URL"))?;
            Ok(ContentBlock::Image {
                source: super::content::ImageSource {
                    url: url.to_string(),
                    media_type: None,
                },
            })
        }
        other => Err(InferenceError::unsupported(format!(
            "unsupported content block `{other}`"
        ))),
    }
}

fn parse_tool_choice(value: Value) -> Result<ToolChoice, InferenceError> {
    if let Some(choice) = value.as_str() {
        return match choice {
            "auto" => Ok(ToolChoice::Auto),
            "none" => Ok(ToolChoice::None),
            "required" => Ok(ToolChoice::Required),
            _ => Err(InferenceError::invalid_request("invalid tool_choice")),
        };
    }
    let name = value
        .pointer("/function/name")
        .and_then(Value::as_str)
        .ok_or_else(|| InferenceError::invalid_request("named tool_choice has no function name"))?;
    Ok(ToolChoice::Tool {
        name: name.to_string(),
    })
}

fn parse_response_format(value: Value) -> Result<ResponseFormat, InferenceError> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ResponseFormat::Text),
        Some("json_object") => Ok(ResponseFormat::JsonObject),
        Some("json_schema") => {
            let schema = value
                .get("json_schema")
                .cloned()
                .ok_or_else(|| InferenceError::invalid_request("json_schema payload is missing"))?;
            let name = schema
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_string();
            Ok(ResponseFormat::JsonSchema { name, schema })
        }
        _ => Err(InferenceError::unsupported("unsupported response_format")),
    }
}

fn is_supported_default(name: &str, value: &Value) -> bool {
    match name {
        "stream" => value == &Value::Bool(false) || value.is_null(),
        "n" => value.as_u64() == Some(1) || value.is_null(),
        "logprobs" => value == &Value::Bool(false) || value.is_null(),
        "top_logprobs" => value.as_u64() == Some(0) || value.is_null(),
        "presence_penalty" | "frequency_penalty" => value.as_f64() == Some(0.0) || value.is_null(),
        "seed" | "service_tier" => value.is_null(),
        _ => false,
    }
}

pub struct LegacyProviderAdapter<P> {
    provider: Arc<P>,
    timeout: Duration,
}

impl<P> LegacyProviderAdapter<P> {
    pub fn new(provider: Arc<P>, timeout: Duration) -> Self {
        Self { provider, timeout }
    }
}

#[async_trait]
impl<P> Provider for LegacyProviderAdapter<P>
where
    P: InferenceProvider + 'static,
{
    fn capabilities(&self) -> LegacyCapabilities {
        let capabilities = self.provider.capabilities();
        LegacyCapabilities {
            native_tool_calling: capabilities.content.tools,
            vision: capabilities.content.images,
            prompt_caching: false,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(system) = system_prompt {
            messages.push(ChatMessage::system(system));
        }
        messages.push(ChatMessage::user(message));
        self.chat_with_history(&messages, model, temperature).await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let response = self
            .chat(
                ChatRequest {
                    messages,
                    tools: None,
                },
                model,
                temperature,
            )
            .await?;
        Ok(response.text.unwrap_or_default())
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let canonical = legacy_request(request, model, temperature);
        let response = self
            .provider
            .infer(
                ProviderRequestContext::with_timeout("legacy-adapter", self.timeout),
                canonical,
            )
            .await?;
        let mut text = Vec::new();
        let mut tool_calls = Vec::new();
        for block in response.content {
            match block {
                ContentBlock::Text { text: value } => text.push(value),
                ContentBlock::ToolCall { call } => tool_calls.push(LegacyToolCall {
                    id: call.id,
                    name: call.name,
                    arguments: serde_json::to_string(&call.arguments)?,
                }),
                _ => {}
            }
        }
        Ok(ChatResponse {
            text: (!text.is_empty()).then(|| text.join("")),
            tool_calls,
            usage: response.usage.map(|usage| TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cached_input_tokens: usage.cached_input_tokens,
            }),
            reasoning_content: None,
        })
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let parsed = tools
            .iter()
            .map(parse_legacy_tool_spec)
            .collect::<Result<Vec<_>, _>>()?;
        self.chat(
            ChatRequest {
                messages,
                tools: Some(&parsed),
            },
            model,
            temperature,
        )
        .await
    }
}

fn legacy_request(request: ChatRequest<'_>, model: &str, temperature: f64) -> InferenceRequest {
    InferenceRequest {
        model: model.to_string(),
        messages: request
            .messages
            .iter()
            .map(convert_legacy_message)
            .collect(),
        tools: request
            .tools
            .unwrap_or_default()
            .iter()
            .map(|tool| ToolDefinition {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.parameters.clone(),
            })
            .collect(),
        tool_choice: request.tools.map(|_| ToolChoice::Auto),
        response_format: None,
        sampling: SamplingOptions {
            temperature: Some(temperature),
            top_p: None,
            stop: Vec::new(),
        },
        token_limits: TokenLimits::default(),
        extensions: BTreeMap::new(),
    }
}

fn parse_legacy_tool_spec(value: &Value) -> Result<ToolSpec, serde_json::Error> {
    serde_json::from_value(value.clone()).or_else(|_| {
        serde_json::from_value(
            value
                .get("function")
                .cloned()
                .unwrap_or_else(|| value.clone()),
        )
    })
}

fn convert_legacy_message(message: &ChatMessage) -> Message {
    match message.role.as_str() {
        "system" => Message::text(Role::System, &message.content),
        "assistant" => {
            let Ok(envelope) = serde_json::from_str::<Value>(&message.content) else {
                return Message::text(Role::Assistant, &message.content);
            };
            let mut content = envelope
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(|text| vec![ContentBlock::text(text)])
                .unwrap_or_default();
            if let Some(calls) = envelope.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let Some(id) = call.get("id").and_then(Value::as_str) else {
                        continue;
                    };
                    let name = call
                        .get("name")
                        .or_else(|| call.pointer("/function/name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let arguments = call
                        .get("arguments")
                        .or_else(|| call.pointer("/function/arguments"))
                        .and_then(Value::as_str)
                        .and_then(|arguments| serde_json::from_str(arguments).ok())
                        .unwrap_or_else(|| serde_json::json!({}));
                    content.push(ContentBlock::ToolCall {
                        call: super::content::ToolCall {
                            id: id.to_string(),
                            name: name.to_string(),
                            arguments,
                        },
                    });
                }
            }
            Message {
                role: Role::Assistant,
                content,
            }
        }
        "tool" => {
            let envelope = serde_json::from_str::<Value>(&message.content).unwrap_or(Value::Null);
            let tool_call_id = envelope
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let text = envelope
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(&message.content)
                .to_string();
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: super::content::ToolResult {
                        tool_call_id,
                        content: vec![ContentBlock::text(text)],
                        is_error: false,
                    },
                }],
            }
        }
        _ => Message::text(Role::User, &message.content),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{anthropic::AnthropicCodec, openai::OpenAiCodec};

    #[test]
    fn conflicting_token_limits_are_rejected() {
        let error = OpenAiCompatibilityProfile::default()
            .parse_request(
                br#"{"model":"x","messages":[],"max_tokens":10,"max_completion_tokens":11}"#,
                ProviderFormat::OpenAi,
            )
            .unwrap_err();
        assert_eq!(
            error.category,
            super::super::error::InferenceErrorCategory::InvalidRequest
        );
    }

    #[test]
    fn non_default_unsupported_parameter_is_rejected() {
        let error = OpenAiCompatibilityProfile::default()
            .parse_request(
                br#"{"model":"x","messages":[],"logprobs":true}"#,
                ProviderFormat::OpenAi,
            )
            .unwrap_err();
        assert_eq!(
            error.category,
            super::super::error::InferenceErrorCategory::UnsupportedFeature
        );
    }

    #[test]
    fn one_public_request_is_encodable_for_openai_and_anthropic_before_output() {
        let public = br#"{
            "model":"public-alias",
            "messages":[{"role":"user","content":"hello"}],
            "max_completion_tokens":64
        }"#;
        let profile = OpenAiCompatibilityProfile::default();
        let openai_request = profile
            .parse_request(public, ProviderFormat::OpenAi)
            .unwrap();
        let anthropic_request = profile
            .parse_request(public, ProviderFormat::Anthropic)
            .unwrap();
        assert!(OpenAiCodec.encode_request(&openai_request, false).is_ok());
        assert!(
            AnthropicCodec
                .encode_request(&anthropic_request, false)
                .is_ok()
        );
    }

    #[test]
    fn legacy_adapter_preserves_assistant_tool_calls_and_tool_results() {
        let assistant = ChatMessage::assistant(
            r#"{"content":"checking","tool_calls":[{"id":"call-1","function":{"name":"lookup","arguments":"{\"city\":\"Toronto\"}"}}]}"#,
        );
        let converted = convert_legacy_message(&assistant);
        assert_eq!(converted.role, Role::Assistant);
        assert!(matches!(converted.content[0], ContentBlock::Text { .. }));
        assert!(matches!(
            converted.content[1],
            ContentBlock::ToolCall { .. }
        ));

        let tool = ChatMessage::tool(r#"{"tool_call_id":"call-1","content":"sunny"}"#);
        let converted = convert_legacy_message(&tool);
        assert_eq!(converted.role, Role::Tool);
        let ContentBlock::ToolResult { result } = &converted.content[0] else {
            panic!("expected canonical tool result");
        };
        assert_eq!(result.tool_call_id, "call-1");
        assert_eq!(result.content, vec![ContentBlock::text("sunny")]);
    }

    #[test]
    fn legacy_adapter_accepts_openai_wrapped_tool_specs() {
        let wrapped = serde_json::json!({
            "type":"function",
            "function":{
                "name":"lookup",
                "description":"Look up weather",
                "parameters":{"type":"object"}
            }
        });
        let parsed = parse_legacy_tool_spec(&wrapped).unwrap();
        assert_eq!(parsed.name, "lookup");
        assert_eq!(parsed.parameters, serde_json::json!({"type":"object"}));
    }
}
