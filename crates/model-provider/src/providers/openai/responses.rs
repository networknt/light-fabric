use crate::inference::GenerateOutputItem;
use crate::inference::{
    ContentBlock, FinishReason, ImageSource, InferenceError, InferenceEvent, InferenceRequest,
    InferenceResponse, ItemStatus, Message, NormalizedUsage, ProviderContinuationState,
    ProviderEvidence, ProviderProtocol, ReasoningOptions, ResponseFormat, Role, SamplingOptions,
    StreamDecoder, TerminalState, TokenLimits, ToolCall, ToolCallDelta, ToolChoice, ToolDefinition,
    ToolResult,
};
use bytes::{Buf, BytesMut};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub const RESPONSES_CODEC_VERSION: &str = "openai-responses-v1";
pub const DEFAULT_MAX_RESPONSES_STREAM_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct OpenAiResponsesCodec;

impl OpenAiResponsesCodec {
    pub fn parse_client_request(
        &self,
        bytes: &[u8],
    ) -> Result<(InferenceRequest, bool), InferenceError> {
        let value: Value = serde_json::from_slice(bytes)
            .map_err(|error| InferenceError::invalid_request(format!("invalid JSON: {error}")))?;
        let object = value.as_object().ok_or_else(|| {
            InferenceError::invalid_request("Responses request must be a JSON object")
        })?;
        const ALLOWED: &[&str] = &[
            "model",
            "input",
            "instructions",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "text",
            "temperature",
            "top_p",
            "max_output_tokens",
            "reasoning",
            "stream",
            "store",
            "previous_response_id",
            "conversation",
            "background",
            "include",
            "metadata",
            "prompt",
            "truncation",
            "user",
        ];
        if let Some(field) = object
            .keys()
            .find(|field| !ALLOWED.contains(&field.as_str()))
        {
            return Err(InferenceError::unsupported(format!(
                "unsupported Responses field `{field}`"
            )));
        }
        reject_deferred(object)?;
        let model = required_nonempty_string(object, "model")?;
        let stream = optional_bool(object, "stream")?.unwrap_or(false);
        let mut messages = Vec::new();
        let mut provider_continuation = None;
        if let Some(instructions) = optional_string(object, "instructions")?
            && !instructions.is_empty()
        {
            messages.push(Message::text(Role::System, instructions));
        }
        parse_input(
            object
                .get("input")
                .ok_or_else(|| InferenceError::invalid_request("input is required"))?,
            &mut messages,
            &mut provider_continuation,
        )?;
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
        let response_format = object
            .get("text")
            .and_then(|value| value.get("format"))
            .filter(|value| !value.is_null())
            .map(parse_text_format)
            .transpose()?;
        let reasoning = object
            .get("reasoning")
            .filter(|value| !value.is_null())
            .map(parse_reasoning)
            .transpose()?;
        let parallel_tool_calls = optional_bool(object, "parallel_tool_calls")?.unwrap_or(false);
        Ok((
            InferenceRequest {
                model,
                messages,
                tools,
                tool_choice,
                response_format,
                parallel_tool_calls,
                reasoning,
                sampling: SamplingOptions {
                    temperature: optional_f64(object, "temperature")?,
                    top_p: optional_f64(object, "top_p")?,
                    stop: Vec::new(),
                },
                token_limits: TokenLimits {
                    max_output_tokens: optional_u32(object, "max_output_tokens")?,
                },
                extensions: BTreeMap::new(),
                provider_continuation,
            },
            stream,
        ))
    }

    pub fn encode_request(
        &self,
        request: &InferenceRequest,
        stream: bool,
    ) -> Result<Value, InferenceError> {
        let mut object = Map::new();
        object.insert("model".into(), json!(request.model));
        object.insert("store".into(), Value::Bool(false));
        object.insert("stream".into(), Value::Bool(stream));
        let mut instructions = Vec::new();
        let mut input = Vec::new();
        for message in &request.messages {
            if message.role == Role::System {
                instructions.push(flatten_text(&message.content)?);
                continue;
            }
            encode_input_message(message, &mut input)?;
        }
        if !instructions.is_empty() {
            object.insert("instructions".into(), json!(instructions.join("\n\n")));
        }
        object.insert("input".into(), Value::Array(input));
        if !request.tools.is_empty() {
            object.insert(
                "tools".into(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| json!({"type":"function","name":tool.name,"description":tool.description,"parameters":tool.input_schema}))
                        .collect(),
                ),
            );
        }
        if let Some(choice) = &request.tool_choice {
            object.insert("tool_choice".into(), encode_tool_choice(choice));
        }
        if request.parallel_tool_calls {
            object.insert("parallel_tool_calls".into(), Value::Bool(true));
        }
        if let Some(format) = &request.response_format {
            object.insert("text".into(), json!({"format": encode_text_format(format)}));
        }
        if let Some(reasoning) = &request.reasoning {
            object.insert(
                "reasoning".into(),
                serde_json::to_value(reasoning).map_err(|_| {
                    InferenceError::invalid_request("reasoning controls are invalid")
                })?,
            );
        }
        if let Some(value) = request.sampling.temperature {
            object.insert("temperature".into(), json!(value));
        }
        if let Some(value) = request.sampling.top_p {
            object.insert("top_p".into(), json!(value));
        }
        if let Some(value) = request.token_limits.max_output_tokens {
            object.insert("max_output_tokens".into(), json!(value));
        }
        Ok(Value::Object(object))
    }

    pub fn decode_response(&self, value: &Value) -> Result<InferenceResponse, InferenceError> {
        let output = value
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                InferenceError::provider_protocol(Some(502), "Responses output is missing")
            })?;
        let mut normalized = Vec::with_capacity(output.len());
        for item in output {
            normalized.push(decode_output_item(item)?);
        }
        let raw_status = value
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("completed");
        let terminal_state = match raw_status {
            "completed" => TerminalState::Complete,
            "cancelled" => TerminalState::Cancelled,
            "incomplete" => TerminalState::Failed,
            "failed" => {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "Responses provider returned a failed terminal response",
                ));
            }
            other => {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    format!("Responses provider returned unknown status `{other}`"),
                ));
            }
        };
        let finish_reason = if value
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
            == Some("max_output_tokens")
        {
            FinishReason::Length
        } else if normalized
            .iter()
            .any(|item| matches!(item, GenerateOutputItem::FunctionCall { .. }))
        {
            FinishReason::ToolCalls
        } else if terminal_state == TerminalState::Complete {
            FinishReason::Stop
        } else {
            FinishReason::Error
        };
        Ok(InferenceResponse {
            output: normalized,
            finish_reason,
            usage: value.get("usage").map(decode_usage).transpose()?,
            evidence: ProviderEvidence {
                request_id: value.get("id").and_then(Value::as_str).map(str::to_string),
                physical_model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                api_version: None,
                raw_finish_reason: Some(raw_status.to_string()),
                continuation: None,
            },
            terminal_state,
        })
    }

    pub fn decode_error(
        &self,
        status: u16,
        retry_after: Option<&str>,
        body: &[u8],
    ) -> InferenceError {
        let detail = serde_json::from_slice::<Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .map(|value| value.chars().take(512).collect())
            })
            .unwrap_or_else(|| format!("OpenAI Responses provider returned HTTP {status}"));
        InferenceError::from_status(status, retry_after, detail)
    }
}

fn reject_deferred(object: &Map<String, Value>) -> Result<(), InferenceError> {
    match object.get("store") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(InferenceError::unsupported("store: true is not supported"));
        }
        Some(_) => return Err(InferenceError::invalid_request("store must be a boolean")),
    }
    for field in ["previous_response_id", "conversation", "prompt"] {
        if object.get(field).is_some_and(|value| !value.is_null()) {
            return Err(InferenceError::unsupported(format!(
                "{field} is not supported"
            )));
        }
    }
    match object.get("background") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => {
            return Err(InferenceError::unsupported(
                "background execution is not supported",
            ));
        }
        Some(_) => {
            return Err(InferenceError::invalid_request(
                "background must be a boolean",
            ));
        }
    }
    if let Some(include) = object.get("include").filter(|value| !value.is_null()) {
        let include = include
            .as_array()
            .ok_or_else(|| InferenceError::invalid_request("include must be an array"))?;
        if include
            .iter()
            .any(|value| value.as_str() != Some("reasoning.encrypted_content"))
        {
            return Err(InferenceError::unsupported(
                "include contains an unsupported value",
            ));
        }
    }
    if object.get("metadata").is_some_and(|value| {
        !value.is_null() && value.as_object().is_none_or(|values| !values.is_empty())
    }) {
        return Err(InferenceError::unsupported("metadata is not supported"));
    }
    if object
        .get("truncation")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some("disabled"))
    {
        return Err(InferenceError::unsupported(
            "automatic truncation is not supported",
        ));
    }
    if object.get("user").is_some_and(|value| !value.is_null()) {
        return Err(InferenceError::unsupported(
            "user is not supported by the stateless Responses profile",
        ));
    }
    if let Some(text) = object.get("text").filter(|value| !value.is_null()) {
        let text = text
            .as_object()
            .ok_or_else(|| InferenceError::invalid_request("text must be an object"))?;
        if text.keys().any(|field| field != "format") {
            return Err(InferenceError::unsupported(
                "text contains unsupported fields",
            ));
        }
    }
    Ok(())
}

fn parse_input(
    value: &Value,
    messages: &mut Vec<Message>,
    provider_continuation: &mut Option<ProviderContinuationState>,
) -> Result<(), InferenceError> {
    match value {
        Value::String(text) => messages.push(Message::text(Role::User, text)),
        Value::Array(items) if items.is_empty() => {
            return Err(InferenceError::invalid_request("input must not be empty"));
        }
        Value::Array(items) => {
            for item in items {
                parse_input_item(item, messages, provider_continuation)?;
            }
        }
        _ => {
            return Err(InferenceError::invalid_request(
                "input must be a string or array",
            ));
        }
    }
    Ok(())
}

fn parse_input_item(
    value: &Value,
    messages: &mut Vec<Message>,
    provider_continuation: &mut Option<ProviderContinuationState>,
) -> Result<(), InferenceError> {
    let object = value
        .as_object()
        .ok_or_else(|| InferenceError::invalid_request("input item must be an object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("message");
    let allowed = match kind {
        "message" => &["type", "id", "status", "role", "content"][..],
        "function_call" => &["type", "id", "status", "call_id", "name", "arguments"][..],
        "function_call_output" => &["type", "id", "status", "call_id", "output"][..],
        "reasoning" => &["type", "id", "status", "summary", "encrypted_content"][..],
        _ => &[][..],
    };
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(InferenceError::unsupported(format!(
            "unsupported {kind} input field `{field}`"
        )));
    }
    match kind {
        "message" => {
            let role = match required_nonempty_string(object, "role")?.as_str() {
                "developer" | "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                _ => {
                    return Err(InferenceError::invalid_request(
                        "unsupported Responses message role",
                    ));
                }
            };
            let content =
                parse_content(object.get("content").ok_or_else(|| {
                    InferenceError::invalid_request("message content is required")
                })?)?;
            messages.push(Message { role, content });
        }
        "function_call" => {
            let arguments = object
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    InferenceError::invalid_request("function_call arguments must be a JSON string")
                })?;
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolCall {
                    call: ToolCall {
                        id: required_nonempty_string(object, "call_id")?,
                        name: required_nonempty_string(object, "name")?,
                        arguments: serde_json::from_str(arguments).map_err(|_| {
                            InferenceError::invalid_request("function_call arguments are not JSON")
                        })?,
                    },
                }],
            });
        }
        "function_call_output" => {
            let output = object
                .get("output")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    InferenceError::invalid_request("function_call_output output must be a string")
                })?;
            messages.push(Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    result: ToolResult {
                        tool_call_id: required_nonempty_string(object, "call_id")?,
                        content: vec![ContentBlock::text(output)],
                        is_error: false,
                    },
                }],
            });
        }
        "reasoning" => {
            let encrypted = object
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    InferenceError::invalid_request(
                        "reasoning encrypted_content is required for stateless continuation",
                    )
                })?;
            if provider_continuation.is_some() {
                return Err(InferenceError::invalid_request(
                    "only one reasoning continuation item is supported",
                ));
            }
            if object.get("summary").is_some_and(|value| {
                !value.is_null() && !value.as_array().is_some_and(Vec::is_empty)
            }) {
                return Err(InferenceError::unsupported(
                    "replayed reasoning summaries are not accepted",
                ));
            }
            *provider_continuation = Some(ProviderContinuationState {
                protocol: ProviderProtocol::OpenAiResponses,
                payload: zeroize::Zeroizing::new(encrypted.as_bytes().to_vec()),
            });
        }
        other => {
            return Err(InferenceError::unsupported(format!(
                "unsupported Responses input item `{other}`"
            )));
        }
    }
    Ok(())
}

fn parse_content(value: &Value) -> Result<Vec<ContentBlock>, InferenceError> {
    if let Some(text) = value.as_str() {
        return Ok(vec![ContentBlock::text(text)]);
    }
    let parts = value.as_array().ok_or_else(|| {
        InferenceError::invalid_request("message content must be a string or array")
    })?;
    parts
        .iter()
        .map(|part| {
            let object = part
                .as_object()
                .ok_or_else(|| InferenceError::invalid_request("content part must be an object"))?;
            match object.get("type").and_then(Value::as_str) {
                Some("input_text" | "output_text") => {
                    if let Some(field) = object.keys().find(|field| {
                        !["type", "text", "annotations", "logprobs"].contains(&field.as_str())
                    }) {
                        return Err(InferenceError::unsupported(format!(
                            "unsupported text content field `{field}`"
                        )));
                    }
                    for field in ["annotations", "logprobs"] {
                        if object
                            .get(field)
                            .filter(|value| !value.is_null())
                            .is_some_and(|value| !value.as_array().is_some_and(Vec::is_empty))
                        {
                            return Err(InferenceError::unsupported(format!(
                                "non-empty input {field} cannot be preserved"
                            )));
                        }
                    }
                    Ok(ContentBlock::text(
                        object.get("text").and_then(Value::as_str).ok_or_else(|| {
                            InferenceError::invalid_request("text part has no text")
                        })?,
                    ))
                }
                Some("input_image") => {
                    if let Some(field) = object
                        .keys()
                        .find(|field| !["type", "image_url", "detail"].contains(&field.as_str()))
                    {
                        return Err(InferenceError::unsupported(format!(
                            "unsupported input_image field `{field}`"
                        )));
                    }
                    if object
                        .get("detail")
                        .filter(|value| !value.is_null())
                        .is_some_and(|value| value.as_str() != Some("auto"))
                    {
                        return Err(InferenceError::unsupported(
                            "input_image detail other than auto cannot be preserved",
                        ));
                    }
                    Ok(ContentBlock::Image {
                        source: ImageSource {
                            url: object
                                .get("image_url")
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    InferenceError::invalid_request("input_image has no image_url")
                                })?
                                .to_string(),
                            media_type: None,
                        },
                    })
                }
                Some(other) => Err(InferenceError::unsupported(format!(
                    "unsupported Responses content part `{other}`"
                ))),
                None => Err(InferenceError::invalid_request("content part has no type")),
            }
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
            const ALLOWED: &[&str] = &["type", "name", "description", "parameters", "strict"];
            if let Some(field) = object
                .keys()
                .find(|field| !ALLOWED.contains(&field.as_str()))
            {
                return Err(InferenceError::unsupported(format!(
                    "unsupported function tool field `{field}`"
                )));
            }
            if tool.get("type").and_then(Value::as_str) != Some("function") {
                return Err(InferenceError::unsupported(
                    "provider-hosted tools are not supported",
                ));
            }
            if let Some(strict) = object.get("strict").filter(|value| !value.is_null()) {
                let strict = strict.as_bool().ok_or_else(|| {
                    InferenceError::invalid_request("function tool strict must be a boolean")
                })?;
                if strict {
                    return Err(InferenceError::unsupported(
                        "strict function tools are not supported by the portable profile",
                    ));
                }
            }
            Ok(ToolDefinition {
                name: tool
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .ok_or_else(|| {
                        InferenceError::invalid_request("function tool name is required")
                    })?
                    .to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                input_schema: tool
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"})),
            })
        })
        .collect()
}

fn parse_tool_choice(value: &Value) -> Result<ToolChoice, InferenceError> {
    if let Some(value) = value.as_str() {
        return match value {
            "auto" => Ok(ToolChoice::Auto),
            "none" => Ok(ToolChoice::None),
            "required" => Ok(ToolChoice::Required),
            _ => Err(InferenceError::invalid_request("invalid tool_choice")),
        };
    }
    if value.get("type").and_then(Value::as_str) != Some("function") {
        return Err(InferenceError::unsupported(
            "only function tool_choice is supported",
        ));
    }
    Ok(ToolChoice::Tool {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| InferenceError::invalid_request("function tool_choice has no name"))?
            .to_string(),
    })
}

fn parse_text_format(value: &Value) -> Result<ResponseFormat, InferenceError> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => Ok(ResponseFormat::Text),
        Some("json_object") => Ok(ResponseFormat::JsonObject),
        Some("json_schema") => Ok(ResponseFormat::JsonSchema {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("response")
                .to_string(),
            schema: value
                .get("schema")
                .cloned()
                .ok_or_else(|| InferenceError::invalid_request("json_schema has no schema"))?,
            strict: value
                .get("strict")
                .map(|value| {
                    value.as_bool().ok_or_else(|| {
                        InferenceError::invalid_request("json_schema strict must be a boolean")
                    })
                })
                .transpose()?,
        }),
        _ => Err(InferenceError::unsupported("unsupported text.format")),
    }
}

fn parse_reasoning(value: &Value) -> Result<ReasoningOptions, InferenceError> {
    let object = value
        .as_object()
        .ok_or_else(|| InferenceError::invalid_request("reasoning must be an object"))?;
    if object.keys().any(|key| key != "effort" && key != "summary") {
        return Err(InferenceError::unsupported(
            "reasoning contains unsupported fields",
        ));
    }
    Ok(ReasoningOptions {
        effort: optional_string(object, "effort")?,
        summary: optional_string(object, "summary")?,
    })
}

fn encode_input_message(message: &Message, output: &mut Vec<Value>) -> Result<(), InferenceError> {
    let role = match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
        Role::System => unreachable!(),
    };
    let mut parts = Vec::new();
    let flush_message = |parts: &mut Vec<Value>, output: &mut Vec<Value>| {
        if !parts.is_empty() {
            output.push(json!({
                "type":"message",
                "role":role,
                "content":std::mem::take(parts)
            }));
        }
    };
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => parts.push(json!({"type":if role == "assistant" {"output_text"} else {"input_text"},"text":text})),
            ContentBlock::Refusal { .. } => return Err(InferenceError::unsupported("refusal content cannot be used as request input")),
            ContentBlock::Image { source } => parts.push(json!({"type":"input_image","image_url":source.url})),
            ContentBlock::ToolCall { call } => {
                flush_message(&mut parts, output);
                output.push(json!({"type":"function_call","call_id":call.id,"name":call.name,"arguments":serde_json::to_string(&call.arguments).map_err(|_| InferenceError::invalid_request("function arguments are invalid"))?}));
            }
            ContentBlock::ToolResult { result } => {
                flush_message(&mut parts, output);
                output.push(json!({"type":"function_call_output","call_id":result.tool_call_id,"output":flatten_text(&result.content)?}));
            }
        }
    }
    flush_message(&mut parts, output);
    Ok(())
}

fn decode_output_item(item: &Value) -> Result<GenerateOutputItem, InferenceError> {
    let id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("item")
        .to_string();
    let status = match item.get("status").and_then(Value::as_str) {
        Some("in_progress") => ItemStatus::InProgress,
        Some("incomplete") => ItemStatus::Incomplete,
        _ => ItemStatus::Completed,
    };
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
            let content = item
                .get("content")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    InferenceError::provider_protocol(
                        Some(502),
                        "Responses message content is missing",
                    )
                })?
                .iter()
                .map(|part| {
                    let object = part.as_object().ok_or_else(|| {
                        InferenceError::provider_protocol(
                            Some(502),
                            "Responses message content is not an object",
                        )
                    })?;
                    match object.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            if let Some(field) = object.keys().find(|field| {
                                !["type", "text", "annotations", "logprobs"]
                                    .contains(&field.as_str())
                            }) {
                                return Err(InferenceError::provider_protocol(
                                    Some(502),
                                    format!("unsupported Responses output_text field `{field}`"),
                                ));
                            }
                            for field in ["annotations", "logprobs"] {
                                if object
                                    .get(field)
                                    .filter(|value| !value.is_null())
                                    .is_some_and(|value| {
                                        !value.as_array().is_some_and(Vec::is_empty)
                                    })
                                {
                                    return Err(InferenceError::provider_protocol(
                                        Some(502),
                                        format!("non-empty Responses {field} cannot be preserved"),
                                    ));
                                }
                            }
                            object
                                .get("text")
                                .and_then(Value::as_str)
                                .map(ContentBlock::text)
                                .ok_or_else(|| {
                                    InferenceError::provider_protocol(
                                        Some(502),
                                        "Responses output_text has no text",
                                    )
                                })
                        }
                        Some("refusal") => {
                            if let Some(field) = object
                                .keys()
                                .find(|field| !["type", "refusal"].contains(&field.as_str()))
                            {
                                return Err(InferenceError::provider_protocol(
                                    Some(502),
                                    format!("unsupported Responses refusal field `{field}`"),
                                ));
                            }
                            object
                                .get("refusal")
                                .and_then(Value::as_str)
                                .map(|refusal| ContentBlock::Refusal {
                                    refusal: refusal.to_string(),
                                })
                                .ok_or_else(|| {
                                    InferenceError::provider_protocol(
                                        Some(502),
                                        "Responses refusal has no refusal text",
                                    )
                                })
                        }
                        Some(other) => Err(InferenceError::provider_protocol(
                            Some(502),
                            format!("unsupported Responses message content `{other}`"),
                        )),
                        None => Err(InferenceError::provider_protocol(
                            Some(502),
                            "Responses message content has no type",
                        )),
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(GenerateOutputItem::Message {
                id,
                role: Role::Assistant,
                content,
                status,
            })
        }
        Some("function_call") => {
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    InferenceError::provider_protocol(
                        Some(502),
                        "Responses function arguments are missing",
                    )
                })?;
            Ok(GenerateOutputItem::FunctionCall {
                id,
                call_id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        InferenceError::provider_protocol(Some(502), "Responses call_id is missing")
                    })?
                    .to_string(),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        InferenceError::provider_protocol(
                            Some(502),
                            "Responses function name is missing",
                        )
                    })?
                    .to_string(),
                arguments: serde_json::from_str(arguments).map_err(|_| {
                    InferenceError::provider_protocol(
                        Some(502),
                        "Responses function arguments are not JSON",
                    )
                })?,
                status,
            })
        }
        Some("reasoning") => {
            let summary = item
                .get("summary")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|entry| {
                    if entry.get("type").and_then(Value::as_str) != Some("summary_text") {
                        return Err(InferenceError::provider_protocol(
                            Some(502),
                            "unsupported Responses reasoning summary part",
                        ));
                    }
                    entry
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .ok_or_else(|| {
                            InferenceError::provider_protocol(
                                Some(502),
                                "Responses reasoning summary text is missing",
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(GenerateOutputItem::ReasoningSummary {
                id,
                summary,
                status,
            })
        }
        Some(other) => Err(InferenceError::provider_protocol(
            Some(502),
            format!("unsupported Responses output item `{other}`"),
        )),
        None => Err(InferenceError::provider_protocol(
            Some(502),
            "Responses output item has no type",
        )),
    }
}

fn decode_usage(value: &Value) -> Result<NormalizedUsage, InferenceError> {
    if !value.is_object() {
        return Err(InferenceError::provider_protocol(
            Some(502),
            "Responses usage is not an object",
        ));
    }
    Ok(NormalizedUsage {
        input_tokens: value.get("input_tokens").and_then(Value::as_u64),
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: value
            .pointer("/output_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    })
}

fn flatten_text(content: &[ContentBlock]) -> Result<String, InferenceError> {
    let mut output = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text } => output.push_str(text),
            ContentBlock::Refusal { .. } => {
                return Err(InferenceError::unsupported(
                    "refusal content cannot be used as tool output",
                ));
            }
            _ => {
                return Err(InferenceError::unsupported(
                    "non-text tool output cannot cross Responses",
                ));
            }
        }
    }
    Ok(output)
}

fn encode_tool_choice(value: &ToolChoice) -> Value {
    match value {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({"type":"function","name":name}),
    }
}
fn encode_text_format(value: &ResponseFormat) -> Value {
    match value {
        ResponseFormat::Text => json!({"type":"text"}),
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema {
            name,
            schema,
            strict,
        } => {
            let mut value = json!({"type":"json_schema","name":name,"schema":schema});
            if let Some(strict) = strict {
                value["strict"] = Value::Bool(*strict);
            }
            value
        }
    }
}
fn required_nonempty_string(
    object: &Map<String, Value>,
    field: &str,
) -> Result<String, InferenceError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= 512)
        .map(str::to_string)
        .ok_or_else(|| InferenceError::invalid_request(format!("{field} is required")))
}
fn optional_string(
    object: &Map<String, Value>,
    field: &str,
) -> Result<Option<String>, InferenceError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(InferenceError::invalid_request(format!(
            "{field} must be a string"
        ))),
    }
}
fn optional_bool(object: &Map<String, Value>, field: &str) -> Result<Option<bool>, InferenceError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(InferenceError::invalid_request(format!(
            "{field} must be a boolean"
        ))),
    }
}
fn optional_f64(object: &Map<String, Value>, field: &str) -> Result<Option<f64>, InferenceError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|v| v.is_finite())
            .map(Some)
            .ok_or_else(|| InferenceError::invalid_request(format!("{field} must be finite"))),
    }
}
fn optional_u32(object: &Map<String, Value>, field: &str) -> Result<Option<u32>, InferenceError> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .filter(|v| *v > 0)
            .map(Some)
            .ok_or_else(|| {
                InferenceError::invalid_request(format!("{field} must be a positive integer"))
            }),
    }
}

#[derive(Debug)]
pub struct OpenAiResponsesStreamDecoder {
    buffer: BytesMut,
    terminal: bool,
    started: bool,
    saw_tool_call: bool,
    max_buffer_bytes: usize,
}

impl Default for OpenAiResponsesStreamDecoder {
    fn default() -> Self {
        Self {
            buffer: BytesMut::new(),
            terminal: false,
            started: false,
            saw_tool_call: false,
            max_buffer_bytes: DEFAULT_MAX_RESPONSES_STREAM_BUFFER_BYTES,
        }
    }
}

impl StreamDecoder for OpenAiResponsesStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<InferenceEvent>, InferenceError> {
        self.buffer.extend_from_slice(chunk);
        if self.buffer.len() > self.max_buffer_bytes {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Responses stream frame exceeds buffer limit",
            ));
        }
        let mut output = Vec::new();
        while let Some(position) = find_frame(&self.buffer) {
            let frame = self.buffer.split_to(position);
            let delimiter = if self.buffer.starts_with(b"\r\n\r\n") {
                4
            } else {
                2
            };
            self.buffer.advance(delimiter);
            let text = std::str::from_utf8(&frame).map_err(|_| {
                InferenceError::provider_protocol(Some(502), "Responses stream is not UTF-8")
            })?;
            let data = text
                .lines()
                .filter_map(|line| line.strip_prefix("data:").map(str::trim))
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            if self.terminal {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "Responses stream data after terminal event",
                ));
            }
            let value: Value = serde_json::from_str(&data).map_err(|_| {
                InferenceError::provider_protocol(
                    Some(502),
                    "Responses stream data is invalid JSON",
                )
            })?;
            let kind = value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match kind {
                "response.created" | "response.in_progress" if !self.started => {
                    self.started = true;
                    output.push(InferenceEvent::MessageStart {
                        evidence: ProviderEvidence {
                            request_id: value
                                .pointer("/response/id")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            ..Default::default()
                        },
                    });
                }
                "response.output_text.delta" => output.push(InferenceEvent::TextDelta {
                    text: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                }),
                "response.refusal.delta" => output.push(InferenceEvent::RefusalDelta {
                    refusal: value
                        .get("delta")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            InferenceError::provider_protocol(
                                Some(502),
                                "Responses refusal delta is missing",
                            )
                        })?
                        .to_string(),
                }),
                "response.reasoning_summary_text.delta" => {
                    output.push(InferenceEvent::ReasoningSummaryDelta {
                        index: value
                            .get("output_index")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u32,
                        text: value
                            .get("delta")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                InferenceError::provider_protocol(
                                    Some(502),
                                    "Responses reasoning summary delta is missing",
                                )
                            })?
                            .to_string(),
                    })
                }
                "response.output_item.added"
                    if value.pointer("/item/type").and_then(Value::as_str)
                        == Some("function_call") =>
                {
                    self.saw_tool_call = true;
                    output.push(InferenceEvent::ToolCallDelta {
                        delta: ToolCallDelta {
                            index: value
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as u32,
                            id: value
                                .pointer("/item/call_id")
                                .and_then(Value::as_str)
                                .map(|value| bounded_provider_identifier(value, "call_id"))
                                .transpose()?,
                            name: value
                                .pointer("/item/name")
                                .and_then(Value::as_str)
                                .map(|value| bounded_provider_identifier(value, "function name"))
                                .transpose()?,
                            arguments_fragment: String::new(),
                        },
                    })
                }
                "response.function_call_arguments.delta" => {
                    output.push(InferenceEvent::ToolCallDelta {
                        delta: ToolCallDelta {
                            index: value
                                .get("output_index")
                                .and_then(Value::as_u64)
                                .unwrap_or(0) as u32,
                            id: None,
                            name: None,
                            arguments_fragment: value
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        },
                    })
                }
                "response.completed" | "response.incomplete" => {
                    if let Some(usage) = value.pointer("/response/usage") {
                        output.push(InferenceEvent::Usage {
                            usage: decode_usage(usage)?,
                        });
                    }
                    let reason = if kind == "response.incomplete" {
                        FinishReason::Length
                    } else if self.saw_tool_call {
                        FinishReason::ToolCalls
                    } else {
                        FinishReason::Stop
                    };
                    output.push(InferenceEvent::MessageEnd {
                        finish_reason: reason,
                        terminal_state: if kind == "response.completed" {
                            TerminalState::Complete
                        } else {
                            TerminalState::Failed
                        },
                    });
                    self.terminal = true;
                }
                "response.failed" | "error" => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "Responses provider stream failed",
                    ));
                }
                "response.output_item.added"
                | "response.created"
                | "response.queued"
                | "response.in_progress"
                | "response.output_item.done"
                | "response.content_part.added"
                | "response.content_part.done"
                | "response.output_text.done"
                | "response.refusal.done"
                | "response.reasoning_summary_part.added"
                | "response.reasoning_summary_part.done"
                | "response.reasoning_summary_text.done"
                | "response.function_call_arguments.done" => {}
                other => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        format!("unsupported Responses stream event `{other}`"),
                    ));
                }
            }
        }
        Ok(output)
    }

    fn finish(&mut self) -> Result<Vec<InferenceEvent>, InferenceError> {
        if !self.buffer.is_empty() {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Responses stream ended with a partial frame",
            ));
        }
        if !self.terminal {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Responses stream ended without a terminal event",
            ));
        }
        Ok(Vec::new())
    }
}

fn bounded_provider_identifier(value: &str, field: &str) -> Result<String, InferenceError> {
    if value.is_empty() || value.len() > 512 {
        return Err(InferenceError::provider_protocol(
            Some(502),
            format!("Responses provider {field} exceeds the identifier bound"),
        ));
    }
    Ok(value.to_string())
}

fn find_frame(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_profile_defaults_store_to_false_and_round_trips_tools() {
        let (request, stream) = OpenAiResponsesCodec.parse_client_request(serde_json::to_string(&json!({
            "model":"public", "instructions":"be concise", "input":[
                {"role":"user","content":[{"type":"input_text","text":"hello"}]},
                {"type":"function_call","call_id":"call_1","name":"weather","arguments":"{\"city\":\"Toronto\"}"},
                {"type":"function_call_output","call_id":"call_1","output":"sunny"}
            ], "tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":false}]
        })).unwrap().as_bytes()).unwrap();
        assert!(!stream);
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.tools[0].name, "weather");
        assert!(OpenAiResponsesCodec.parse_client_request(br#"{"model":"m","input":"x","tools":[{"type":"function","name":"f","strict":true}]}"#).is_err());
        assert!(
            OpenAiResponsesCodec
                .parse_client_request(br#"{"model":"m","input":"x","store":true}"#)
                .is_err()
        );
    }

    #[test]
    fn provider_request_preserves_message_and_tool_item_order() {
        let mut request = InferenceRequest::text("physical", "placeholder");
        request.messages = vec![Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("before"),
                ContentBlock::ToolCall {
                    call: ToolCall {
                        id: "call_1".to_string(),
                        name: "weather".to_string(),
                        arguments: json!({"city":"Toronto"}),
                    },
                },
                ContentBlock::text("after"),
            ],
        }];
        let encoded = OpenAiResponsesCodec
            .encode_request(&request, false)
            .unwrap();
        let types = encoded["input"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["type"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(types, ["message", "function_call", "message"]);
        assert_eq!(encoded["input"][0]["content"][0]["text"], "before");
        assert_eq!(encoded["input"][2]["content"][0]["text"], "after");
    }

    #[test]
    fn provider_codec_preserves_typed_output_and_usage() {
        let response = OpenAiResponsesCodec.decode_response(&json!({"id":"resp_private","model":"physical","status":"completed","output":[
            {"type":"message","id":"msg_private","status":"completed","content":[{"type":"output_text","text":"ok"}]},
            {"type":"function_call","id":"fc_private","call_id":"call_1","name":"weather","arguments":"{}","status":"completed"}
        ],"usage":{"input_tokens":4,"output_tokens":2}})).unwrap();
        assert_eq!(response.output.len(), 2);
        assert_eq!(response.usage.unwrap().input_tokens, Some(4));
    }

    #[test]
    fn provider_codec_preserves_refusal_and_rejects_failed_terminal() {
        let response = OpenAiResponsesCodec.decode_response(&json!({
            "status":"completed","output":[{"type":"message","id":"m","status":"completed","content":[
                {"type":"refusal","refusal":"I cannot help with that."}
            ]}],"usage":{"input_tokens":1,"output_tokens":1}
        })).unwrap();
        assert!(matches!(
            &response.output[0],
            GenerateOutputItem::Message { content, .. }
                if matches!(&content[0], ContentBlock::Refusal { refusal } if refusal == "I cannot help with that.")
        ));
        assert!(
            OpenAiResponsesCodec
                .decode_response(&json!({
                    "status":"failed","output":[],"error":{"code":"server_error"}
                }))
                .is_err()
        );
    }

    #[test]
    fn client_rejects_lossy_image_detail_and_unknown_provider_content() {
        assert!(OpenAiResponsesCodec.parse_client_request(br#"{"model":"m","input":[{"role":"user","content":[{"type":"input_image","image_url":"https://example.test/a.png","detail":"high"}]}]}"#).is_err());
        assert!(OpenAiResponsesCodec.parse_client_request(br#"{"model":"m","input":[{"role":"user","content":[{"type":"input_text","text":"hello","annotations":[{"type":"file_citation"}]}]}]}"#).is_err());
        assert!(OpenAiResponsesCodec.decode_response(&json!({
            "status":"completed","output":[{"type":"message","content":[{"type":"future_content","value":"x"}]}]
        })).is_err());
    }

    #[test]
    fn stream_decoder_handles_arbitrary_fragmentation() {
        let bytes = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\nevent: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"hé🙂\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n".as_bytes();
        for split in 1..bytes.len() {
            let mut decoder = OpenAiResponsesStreamDecoder::default();
            let mut events = decoder.push(&bytes[..split]).unwrap();
            events.extend(decoder.push(&bytes[split..]).unwrap());
            decoder.finish().unwrap();
            assert!(events.iter().any(
                |event| matches!(event, InferenceEvent::TextDelta { text } if text == "hé🙂")
            ));
        }
    }

    #[test]
    fn stream_decoder_preserves_refusal_and_reasoning_and_rejects_unknown_events() {
        let mut decoder = OpenAiResponsesStreamDecoder::default();
        let events = decoder.push(concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n",
            "data: {\"type\":\"response.refusal.delta\",\"delta\":\"no\"}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":1,\"delta\":\"summary\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
        ).as_bytes()).unwrap();
        assert!(events.iter().any(
            |event| matches!(event, InferenceEvent::RefusalDelta { refusal } if refusal == "no")
        ));
        assert!(events.iter().any(|event| matches!(event, InferenceEvent::ReasoningSummaryDelta { index: 1, text } if text == "summary")));

        let mut decoder = OpenAiResponsesStreamDecoder::default();
        assert!(
            decoder
                .push(b"data: {\"type\":\"response.future_behavior.delta\",\"delta\":\"x\"}\n\n")
                .is_err()
        );
    }
}
