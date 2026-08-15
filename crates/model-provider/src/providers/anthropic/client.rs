use crate::inference::{
    ContentBlock, FinishReason, GenerateOutputItem, ImageSource, InferenceError, InferenceRequest,
    InferenceResponse, Message, ProviderContinuationState, ProviderProtocol, Role, SamplingOptions,
    TokenLimits, ToolCall, ToolChoice, ToolDefinition, ToolResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

#[derive(Debug, Clone, Default)]
pub struct AnthropicClientCodec;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningBlock {
    ReasoningText {
        text: String,
        signature: Option<String>,
    },
    RedactedContent {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReasoningTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message_index: Option<usize>,
    pub(crate) blocks: Vec<ReasoningBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReasoningState {
    pub(crate) version: u8,
    pub(crate) turns: Vec<ReasoningTurn>,
}

impl AnthropicClientCodec {
    pub fn parse_request(&self, bytes: &[u8]) -> Result<(InferenceRequest, bool), InferenceError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|error| InferenceError::invalid_request(format!("invalid JSON: {error}")))?;
        let object = value.as_object().ok_or_else(|| {
            InferenceError::invalid_request("Anthropic request must be a JSON object")
        })?;
        const ALLOWED: &[&str] = &[
            "model",
            "max_tokens",
            "messages",
            "system",
            "tools",
            "tool_choice",
            "temperature",
            "top_p",
            "stop_sequences",
            "stream",
            "metadata",
        ];
        if let Some(field) = object
            .keys()
            .find(|field| !ALLOWED.contains(&field.as_str()))
        {
            return Err(InferenceError::unsupported(format!(
                "unsupported Anthropic field `{field}`"
            )));
        }
        if object.get("metadata").is_some_and(|value| {
            !value.is_null() && value.as_object().is_none_or(|values| !values.is_empty())
        }) {
            return Err(InferenceError::unsupported(
                "Anthropic metadata is not supported by the stateless profile",
            ));
        }
        let model = required_string(object, "model")?;
        let max_tokens = required_u32(object, "max_tokens")?;
        let stream = optional_bool(object, "stream")?.unwrap_or(false);
        let mut messages = Vec::new();
        if let Some(system) = object.get("system").filter(|value| !value.is_null()) {
            messages.push(Message {
                role: Role::System,
                content: parse_system(system)?,
            });
        }
        let source_messages = object
            .get("messages")
            .and_then(Value::as_array)
            .filter(|messages| !messages.is_empty())
            .ok_or_else(|| InferenceError::invalid_request("messages must be a non-empty array"))?;
        let mut reasoning = Vec::new();
        for source in source_messages {
            let message_index = messages.len();
            let (message, blocks) = parse_message(source)?;
            if !blocks.is_empty() {
                reasoning.push(ReasoningTurn {
                    message_index: Some(message_index),
                    blocks,
                });
            }
            messages.push(message);
        }
        let provider_continuation = (!reasoning.is_empty())
            .then(|| {
                serde_json::to_vec(&ReasoningState {
                    version: 1,
                    turns: reasoning,
                })
                .map(|payload| ProviderContinuationState {
                    protocol: ProviderProtocol::AnthropicMessages,
                    payload: Zeroizing::new(payload),
                })
            })
            .transpose()
            .map_err(|_| InferenceError::invalid_request("Anthropic reasoning state is invalid"))?;
        let tools = object
            .get("tools")
            .filter(|value| !value.is_null())
            .map(parse_tools)
            .transpose()?
            .unwrap_or_default();
        let tool_choice = object
            .get("tool_choice")
            .filter(|value| !value.is_null())
            .map(parse_tool_choice)
            .transpose()?;
        Ok((
            InferenceRequest {
                model,
                messages,
                tools,
                tool_choice,
                response_format: None,
                parallel_tool_calls: false,
                reasoning: None,
                sampling: SamplingOptions {
                    temperature: optional_f64(object, "temperature")?,
                    top_p: optional_f64(object, "top_p")?,
                    stop: object
                        .get("stop_sequences")
                        .filter(|value| !value.is_null())
                        .map(parse_strings)
                        .transpose()?
                        .unwrap_or_default(),
                },
                token_limits: TokenLimits {
                    max_output_tokens: Some(max_tokens),
                },
                extensions: BTreeMap::new(),
                provider_continuation,
            },
            stream,
        ))
    }

    pub fn render_response(
        &self,
        request_id: &str,
        alias: &str,
        response: InferenceResponse,
    ) -> Result<Value, InferenceError> {
        let mut content = Vec::new();
        if let Some(continuation) = response.evidence.continuation {
            if !matches!(
                continuation.protocol,
                ProviderProtocol::BedrockConverse | ProviderProtocol::AnthropicMessages
            ) {
                return Err(InferenceError::protocol(
                    "unsupported provider reasoning state for Anthropic facade",
                ));
            }
            let state: ReasoningState = serde_json::from_slice(&continuation.payload)
                .map_err(|_| InferenceError::protocol("invalid provider reasoning state"))?;
            if state.version != 1 {
                return Err(InferenceError::protocol(
                    "unsupported provider reasoning-state version",
                ));
            }
            for block in state.turns.into_iter().flat_map(|turn| turn.blocks) {
                content.push(match block {
                    ReasoningBlock::ReasoningText { text, signature } => {
                        json!({"type":"thinking","thinking":text,"signature":signature.unwrap_or_default()})
                    }
                    ReasoningBlock::RedactedContent { data } => {
                        json!({"type":"redacted_thinking","data":data})
                    }
                });
            }
        }
        for item in response.output {
            match item {
                GenerateOutputItem::Message {
                    content: blocks, ..
                } => {
                    for block in blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                content.push(json!({"type":"text","text":text}))
                            }
                            ContentBlock::Refusal { refusal } => {
                                content.push(json!({"type":"text","text":refusal}))
                            }
                            _ => {
                                return Err(InferenceError::protocol(
                                    "unsupported public Anthropic response content",
                                ));
                            }
                        }
                    }
                }
                GenerateOutputItem::FunctionCall {
                    call_id,
                    name,
                    arguments,
                    ..
                } => {
                    content.push(
                        json!({"type":"tool_use","id":call_id,"name":name,"input":arguments}),
                    );
                }
                GenerateOutputItem::ReasoningSummary { .. } => {}
            }
        }
        let usage = response.usage.unwrap_or_default();
        Ok(json!({
            "id":format!("msg_{request_id}"),
            "type":"message",
            "role":"assistant",
            "model":alias,
            "content":content,
            "stop_reason":stop_reason(response.finish_reason),
            "stop_sequence":null,
            "usage":{
                "input_tokens":usage.input_tokens.unwrap_or(0),
                "output_tokens":usage.output_tokens.unwrap_or(0),
                "cache_read_input_tokens":usage.cached_input_tokens.unwrap_or(0)
            }
        }))
    }
}

fn parse_system(value: &Value) -> Result<Vec<ContentBlock>, InferenceError> {
    if let Some(text) = value.as_str() {
        return Ok(vec![ContentBlock::text(text)]);
    }
    value
        .as_array()
        .ok_or_else(|| InferenceError::invalid_request("system must be text or an array"))?
        .iter()
        .map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                return Err(InferenceError::unsupported(
                    "Anthropic system blocks must be text",
                ));
            }
            Ok(ContentBlock::text(
                block.get("text").and_then(Value::as_str).ok_or_else(|| {
                    InferenceError::invalid_request("system text block is missing text")
                })?,
            ))
        })
        .collect()
}

fn parse_message(value: &Value) -> Result<(Message, Vec<ReasoningBlock>), InferenceError> {
    let object = value
        .as_object()
        .ok_or_else(|| InferenceError::invalid_request("message must be an object"))?;
    if let Some(field) = object
        .keys()
        .find(|field| !["role", "content"].contains(&field.as_str()))
    {
        return Err(InferenceError::unsupported(format!(
            "unsupported Anthropic message field `{field}`"
        )));
    }
    let role = match required_string(object, "role")?.as_str() {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => {
            return Err(InferenceError::invalid_request(
                "message role must be user or assistant",
            ));
        }
    };
    let content_value = object
        .get("content")
        .ok_or_else(|| InferenceError::invalid_request("message content is required"))?;
    if let Some(text) = content_value.as_str() {
        return Ok((
            Message {
                role,
                content: vec![ContentBlock::text(text)],
            },
            Vec::new(),
        ));
    }
    let blocks = content_value.as_array().ok_or_else(|| {
        InferenceError::invalid_request("message content must be text or an array")
    })?;
    let mut content = Vec::new();
    let mut reasoning = Vec::new();
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| InferenceError::invalid_request("content block has no type"))?;
        match kind {
            "text" => content.push(ContentBlock::text(
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| InferenceError::invalid_request("text block is missing text"))?,
            )),
            "image" => content.push(parse_image(block)?),
            "tool_use" if role == Role::Assistant => content.push(ContentBlock::ToolCall {
                call: ToolCall {
                    id: required_value_string(block, "id")?,
                    name: required_value_string(block, "name")?,
                    arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                },
            }),
            "tool_result" if role == Role::User => content.push(ContentBlock::ToolResult {
                result: ToolResult {
                    tool_call_id: required_value_string(block, "tool_use_id")?,
                    content: parse_tool_result_content(
                        block
                            .get("content")
                            .unwrap_or(&Value::String(String::new())),
                    )?,
                    is_error: block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
            }),
            "thinking" if role == Role::Assistant => {
                reasoning.push(ReasoningBlock::ReasoningText {
                    text: required_value_string(block, "thinking")?,
                    signature: block
                        .get("signature")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                })
            }
            "redacted_thinking" if role == Role::Assistant => {
                reasoning.push(ReasoningBlock::RedactedContent {
                    data: required_value_string(block, "data")?,
                })
            }
            other => {
                return Err(InferenceError::unsupported(format!(
                    "unsupported Anthropic content block `{other}`"
                )));
            }
        }
    }
    if content.is_empty() && reasoning.is_empty() {
        return Err(InferenceError::invalid_request(
            "message content must not be empty",
        ));
    }
    Ok((Message { role, content }, reasoning))
}

fn parse_image(value: &Value) -> Result<ContentBlock, InferenceError> {
    let source = value
        .get("source")
        .and_then(Value::as_object)
        .ok_or_else(|| InferenceError::invalid_request("image source is required"))?;
    let url = match source.get("type").and_then(Value::as_str) {
        Some("base64") => format!(
            "data:{};base64,{}",
            required_string(source, "media_type")?,
            required_string(source, "data")?
        ),
        Some("url") => required_string(source, "url")?,
        _ => {
            return Err(InferenceError::unsupported(
                "unsupported Anthropic image source",
            ));
        }
    };
    Ok(ContentBlock::Image {
        source: ImageSource {
            url,
            media_type: source
                .get("media_type")
                .and_then(Value::as_str)
                .map(str::to_string),
        },
    })
}

fn parse_tool_result_content(value: &Value) -> Result<Vec<ContentBlock>, InferenceError> {
    if let Some(text) = value.as_str() {
        return Ok(vec![ContentBlock::text(text)]);
    }
    value
        .as_array()
        .ok_or_else(|| {
            InferenceError::invalid_request("tool result content must be text or an array")
        })?
        .iter()
        .map(|block| {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                return Err(InferenceError::unsupported(
                    "tool result supports text only",
                ));
            }
            Ok(ContentBlock::text(required_value_string(block, "text")?))
        })
        .collect()
}

fn parse_tools(value: &Value) -> Result<Vec<ToolDefinition>, InferenceError> {
    value
        .as_array()
        .ok_or_else(|| InferenceError::invalid_request("tools must be an array"))?
        .iter()
        .map(|tool| {
            let object = tool
                .as_object()
                .ok_or_else(|| InferenceError::invalid_request("tool must be an object"))?;
            if let Some(field) = object
                .keys()
                .find(|field| !["name", "description", "input_schema"].contains(&field.as_str()))
            {
                return Err(InferenceError::unsupported(format!(
                    "unsupported Anthropic tool field `{field}`"
                )));
            }
            Ok(ToolDefinition {
                name: required_string(object, "name")?,
                description: object
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input_schema: object.get("input_schema").cloned().ok_or_else(|| {
                    InferenceError::invalid_request("tool input_schema is required")
                })?,
            })
        })
        .collect()
}

fn parse_tool_choice(value: &Value) -> Result<ToolChoice, InferenceError> {
    match value.get("type").and_then(Value::as_str) {
        Some("auto") => Ok(ToolChoice::Auto),
        Some("none") => Ok(ToolChoice::None),
        Some("any") => Ok(ToolChoice::Required),
        Some("tool") => Ok(ToolChoice::Tool {
            name: required_value_string(value, "name")?,
        }),
        _ => Err(InferenceError::invalid_request(
            "unsupported Anthropic tool_choice",
        )),
    }
}

fn parse_strings(value: &Value) -> Result<Vec<String>, InferenceError> {
    value
        .as_array()
        .ok_or_else(|| InferenceError::invalid_request("stop_sequences must be an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| InferenceError::invalid_request("stop sequence must be a string"))
        })
        .collect()
}

fn stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Stop => "end_turn",
        FinishReason::ContentFilter => "refusal",
        FinishReason::Cancelled | FinishReason::Error | FinishReason::Unknown => "end_turn",
    }
}

fn required_string(object: &Map<String, Value>, field: &str) -> Result<String, InferenceError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| InferenceError::invalid_request(format!("{field} is required")))
}

fn required_value_string(value: &Value, field: &str) -> Result<String, InferenceError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| InferenceError::invalid_request(format!("{field} is required")))
}

fn required_u32(object: &Map<String, Value>, field: &str) -> Result<u32, InferenceError> {
    object
        .get(field)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            InferenceError::invalid_request(format!("{field} must be a positive integer"))
        })
}

fn optional_bool(object: &Map<String, Value>, field: &str) -> Result<Option<bool>, InferenceError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value.as_bool().ok_or_else(|| {
                InferenceError::invalid_request(format!("{field} must be a boolean"))
            })
        })
        .transpose()
}

fn optional_f64(object: &Map<String, Value>, field: &str) -> Result<Option<f64>, InferenceError> {
    object
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| {
                    InferenceError::invalid_request(format!("{field} must be a finite number"))
                })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_tools_and_native_reasoning_without_provider_identity() {
        let (request, stream) = AnthropicClientCodec.parse_request(br#"{
          "model":"claude","max_tokens":256,"stream":true,
          "messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"opaque","signature":"sig"},{"type":"tool_use","id":"call-1","name":"weather","input":{"city":"Toronto"}}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"cold"}]}
          ],
          "tools":[{"name":"weather","description":"Weather","input_schema":{"type":"object"}}]
        }"#).unwrap();
        assert!(stream);
        assert_eq!(request.model, "claude");
        assert_eq!(request.tools.len(), 1);
        assert_eq!(
            request.provider_continuation.as_ref().unwrap().protocol,
            ProviderProtocol::AnthropicMessages
        );
    }

    #[test]
    fn preserves_reasoning_blocks_on_each_assistant_turn() {
        let (request, _) = AnthropicClientCodec
            .parse_request(
                br#"{
                  "model":"claude","max_tokens":256,
                  "messages":[
                    {"role":"assistant","content":[{"type":"thinking","thinking":"first","signature":"sig-1"},{"type":"tool_use","id":"call-1","name":"one","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"one"}]},
                    {"role":"assistant","content":[{"type":"thinking","thinking":"second","signature":"sig-2"},{"type":"tool_use","id":"call-2","name":"two","input":{}}]},
                    {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-2","content":"two"}]}
                  ]
                }"#,
            )
            .unwrap();
        let continuation = request.provider_continuation.unwrap();
        let state: ReasoningState = serde_json::from_slice(&continuation.payload).unwrap();
        assert_eq!(state.turns.len(), 2);
        assert_eq!(state.turns[0].message_index, Some(0));
        assert_eq!(state.turns[1].message_index, Some(2));
        assert!(matches!(
            &state.turns[0].blocks[0],
            ReasoningBlock::ReasoningText { text, .. } if text == "first"
        ));
        assert!(matches!(
            &state.turns[1].blocks[0],
            ReasoningBlock::ReasoningText { text, .. } if text == "second"
        ));
    }
}
