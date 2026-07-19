use crate::inference::content::{ContentBlock, Role, ToolCall};
use crate::inference::error::InferenceError;
use crate::inference::request::{InferenceRequest, ResponseFormat, ToolChoice};
use crate::inference::response::{
    FinishReason, InferenceResponse, NormalizedUsage, ProviderEvidence, TerminalState,
};
use crate::inference::stream::{InferenceEvent, StreamDecoder, ToolCallDelta};
use bytes::{Buf, BytesMut};
use serde_json::{Value, json};

pub const CODEC_VERSION: &str = "anthropic-messages-v1";
pub const DEFAULT_MAX_STREAM_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct AnthropicCodec;

impl AnthropicCodec {
    pub fn encode_request(
        &self,
        request: &InferenceRequest,
        stream: bool,
    ) -> Result<Value, InferenceError> {
        if !request.extensions.is_empty() {
            return Err(InferenceError::unsupported(
                "OpenAI compatibility extensions cannot cross into Anthropic",
            ));
        }
        let mut system = Vec::new();
        let mut messages = Vec::new();
        for message in &request.messages {
            if message.role == Role::System {
                for block in &message.content {
                    match block {
                        ContentBlock::Text { text } => {
                            system.push(json!({"type":"text","text":text}))
                        }
                        _ => {
                            return Err(InferenceError::unsupported(
                                "Anthropic system content must be text",
                            ));
                        }
                    }
                }
                continue;
            }
            let role = match message.role {
                Role::Assistant => "assistant",
                Role::User | Role::Tool => "user",
                Role::System => unreachable!(),
            };
            let content = message
                .content
                .iter()
                .map(encode_content)
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(Value::Object(previous)) = messages.last_mut()
                && previous.get("role").and_then(Value::as_str) == Some(role)
                && role == "user"
            {
                previous
                    .get_mut("content")
                    .and_then(Value::as_array_mut)
                    .expect("constructed content array")
                    .extend(content);
            } else {
                messages.push(json!({"role":role,"content":content}));
            }
        }
        let max_tokens = request.token_limits.max_output_tokens.ok_or_else(|| {
            InferenceError::invalid_request("Anthropic requires max output tokens")
        })?;
        let mut object = serde_json::Map::new();
        object.insert("model".to_string(), json!(request.model));
        object.insert("max_tokens".to_string(), json!(max_tokens));
        object.insert("messages".to_string(), Value::Array(messages));
        object.insert("stream".to_string(), Value::Bool(stream));
        if !system.is_empty() {
            object.insert("system".to_string(), Value::Array(system));
        }
        if let Some(temperature) = request.sampling.temperature {
            object.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = request.sampling.top_p {
            object.insert("top_p".to_string(), json!(top_p));
        }
        if !request.sampling.stop.is_empty() {
            object.insert("stop_sequences".to_string(), json!(request.sampling.stop));
        }
        if !request.tools.is_empty() {
            object.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({"name":tool.name,"description":tool.description,"input_schema":tool.input_schema})
                        })
                        .collect(),
                ),
            );
        }
        if let Some(choice) = &request.tool_choice {
            object.insert("tool_choice".to_string(), encode_tool_choice(choice));
        }
        if let Some(format) = &request.response_format {
            match format {
                ResponseFormat::Text => {}
                ResponseFormat::JsonObject => {
                    let existing_system = object.remove("system");
                    object.insert(
                        "system".to_string(),
                        append_system(existing_system, "Return one valid JSON object."),
                    );
                }
                ResponseFormat::JsonSchema { .. } => {
                    return Err(InferenceError::unsupported(
                        "Anthropic JSON schema response format is not enabled in this profile",
                    ));
                }
            }
        }
        Ok(Value::Object(object))
    }

    pub fn decode_response(&self, value: &Value) -> Result<InferenceResponse, InferenceError> {
        let blocks = value
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                InferenceError::provider_protocol(
                    Some(502),
                    "Anthropic response content is missing",
                )
            })?;
        let mut content = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = block
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.is_empty())
                    {
                        content.push(ContentBlock::text(text));
                    }
                }
                Some("tool_use") => content.push(ContentBlock::ToolCall {
                    call: ToolCall {
                        id: required_string(block, "id", "Anthropic tool id")?,
                        name: required_string(block, "name", "Anthropic tool name")?,
                        arguments: block.get("input").cloned().unwrap_or_else(|| json!({})),
                    },
                }),
                Some("thinking") | Some("redacted_thinking") => {
                    // Provider reasoning is intentionally not exposed by the public contract.
                }
                Some(other) => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        format!("unknown Anthropic response block `{other}`"),
                    ));
                }
                None => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "Anthropic block has no type",
                    ));
                }
            }
        }
        let raw_stop = value
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        Ok(InferenceResponse {
            content,
            finish_reason: map_stop_reason(raw_stop.as_deref()),
            usage: value.get("usage").map(decode_usage).transpose()?,
            evidence: ProviderEvidence {
                request_id: value
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                physical_model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                api_version: None,
                raw_finish_reason: raw_stop,
            },
            terminal_state: TerminalState::Complete,
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
                    .map(sanitize)
            })
            .unwrap_or_else(|| format!("Anthropic provider returned HTTP {status}"));
        InferenceError::from_status(status, retry_after, detail)
    }
}

fn encode_content(block: &ContentBlock) -> Result<Value, InferenceError> {
    match block {
        ContentBlock::Text { text } => Ok(json!({"type":"text","text":text})),
        ContentBlock::Image { source } => {
            if source.url.starts_with("data:") {
                let (header, data) = source.url.split_once(',').ok_or_else(|| {
                    InferenceError::invalid_request("image data URL has no payload")
                })?;
                let media_type = source
                    .media_type
                    .as_deref()
                    .or_else(|| {
                        header
                            .strip_prefix("data:")
                            .and_then(|value| value.split(';').next())
                    })
                    .unwrap_or("image/jpeg");
                Ok(
                    json!({"type":"image","source":{"type":"base64","media_type":media_type,"data":data}}),
                )
            } else {
                Ok(json!({"type":"image","source":{"type":"url","url":source.url}}))
            }
        }
        ContentBlock::ToolCall { call } => {
            Ok(json!({"type":"tool_use","id":call.id,"name":call.name,"input":call.arguments}))
        }
        ContentBlock::ToolResult { result } => {
            let content = result
                .content
                .iter()
                .map(encode_content)
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({
                "type":"tool_result",
                "tool_use_id":result.tool_call_id,
                "content":content,
                "is_error":result.is_error
            }))
        }
    }
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!({"type":"auto"}),
        ToolChoice::None => json!({"type":"none"}),
        ToolChoice::Required => json!({"type":"any"}),
        ToolChoice::Tool { name } => json!({"type":"tool","name":name}),
    }
}

fn append_system(existing: Option<Value>, text: &str) -> Value {
    let mut blocks = existing
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    blocks.push(json!({"type":"text","text":text}));
    Value::Array(blocks)
}

fn decode_usage(value: &Value) -> Result<NormalizedUsage, InferenceError> {
    if !value.is_object() {
        return Err(InferenceError::provider_protocol(
            Some(502),
            "Anthropic usage is not an object",
        ));
    }
    Ok(NormalizedUsage {
        input_tokens: value.get("input_tokens").and_then(Value::as_u64),
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        cached_input_tokens: value.get("cache_read_input_tokens").and_then(Value::as_u64),
        reasoning_tokens: None,
    })
}

fn map_stop_reason(value: Option<&str>) -> FinishReason {
    match value {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some(_) | None => FinishReason::Unknown,
    }
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, InferenceError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| InferenceError::provider_protocol(Some(502), format!("{label} is missing")))
}

fn sanitize(value: &str) -> String {
    value.chars().take(512).collect()
}

#[derive(Debug)]
pub struct AnthropicStreamDecoder {
    buffer: BytesMut,
    stopped: bool,
    max_buffer_bytes: usize,
}

impl Default for AnthropicStreamDecoder {
    fn default() -> Self {
        Self::with_max_buffer_bytes(DEFAULT_MAX_STREAM_BUFFER_BYTES)
    }
}

impl AnthropicStreamDecoder {
    pub fn with_max_buffer_bytes(max_buffer_bytes: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            stopped: false,
            max_buffer_bytes: max_buffer_bytes.max(1),
        }
    }

    fn drain_frames(&mut self, events: &mut Vec<InferenceEvent>) -> Result<(), InferenceError> {
        while let Some(position) = find_frame(&self.buffer) {
            let frame = self.buffer.split_to(position);
            let delimiter = if self.buffer.starts_with(b"\r\n\r\n") {
                4
            } else {
                2
            };
            self.buffer.advance(delimiter);
            let text = std::str::from_utf8(&frame).map_err(|_| {
                InferenceError::provider_protocol(Some(502), "Anthropic stream is not UTF-8")
            })?;
            let data = text
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&data).map_err(|error| {
                InferenceError::provider_protocol(
                    Some(502),
                    format!("malformed Anthropic stream event: {error}"),
                )
            })?;
            decode_stream_value(&value, events, &mut self.stopped)?;
            if self.stopped {
                break;
            }
        }
        Ok(())
    }
}

impl StreamDecoder for AnthropicStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<InferenceEvent>, InferenceError> {
        if self.stopped {
            if chunk.iter().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(Vec::new());
            }
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Anthropic stream data after message_stop",
            ));
        }
        let mut events = Vec::new();
        let mut remaining = chunk;
        while !remaining.is_empty() {
            let available = self.max_buffer_bytes - self.buffer.len();
            if available == 0 {
                return Err(stream_buffer_limit_error());
            }
            let take = available.min(remaining.len());
            self.buffer.extend_from_slice(&remaining[..take]);
            remaining = &remaining[take..];
            self.drain_frames(&mut events)?;
            if self.stopped {
                if !self.buffer.iter().all(|byte| byte.is_ascii_whitespace())
                    || !remaining.iter().all(|byte| byte.is_ascii_whitespace())
                {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "Anthropic stream data after message_stop",
                    ));
                }
                self.buffer.clear();
                return Ok(events);
            }
            if self.buffer.len() == self.max_buffer_bytes && !remaining.is_empty() {
                return Err(stream_buffer_limit_error());
            }
        }
        Ok(events)
    }

    fn finish(&mut self) -> Result<Vec<InferenceEvent>, InferenceError> {
        if !self.buffer.iter().all(|byte| byte.is_ascii_whitespace()) {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "truncated Anthropic stream frame",
            ));
        }
        if !self.stopped {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Anthropic stream ended without message_stop",
            ));
        }
        Ok(Vec::new())
    }
}

fn find_frame(buffer: &[u8]) -> Option<usize> {
    let crlf = buffer.windows(4).position(|window| window == b"\r\n\r\n");
    let lf = buffer.windows(2).position(|window| window == b"\n\n");
    match (crlf, lf) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(position), None) | (None, Some(position)) => Some(position),
        (None, None) => None,
    }
}

fn stream_buffer_limit_error() -> InferenceError {
    InferenceError::provider_protocol(
        Some(502),
        "Anthropic stream frame exceeds configured buffer limit",
    )
}

fn decode_stream_value(
    value: &Value,
    events: &mut Vec<InferenceEvent>,
    stopped: &mut bool,
) -> Result<(), InferenceError> {
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") => {
            let message = value.get("message").unwrap_or(&Value::Null);
            events.push(InferenceEvent::MessageStart {
                evidence: ProviderEvidence {
                    request_id: message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    physical_model: message
                        .get("model")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    ..ProviderEvidence::default()
                },
            });
            if let Some(usage) = message.get("usage") {
                events.push(InferenceEvent::Usage {
                    usage: decode_usage(usage)?,
                });
            }
        }
        Some("content_block_start") => {
            let block = value.get("content_block").unwrap_or(&Value::Null);
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                events.push(InferenceEvent::ToolCallDelta {
                    delta: ToolCallDelta {
                        index: value
                            .get("index")
                            .and_then(Value::as_u64)
                            .unwrap_or_default() as u32,
                        id: block
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        name: block
                            .get("name")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        arguments_fragment: String::new(),
                    },
                });
            }
        }
        Some("content_block_delta") => {
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta.get("type").and_then(Value::as_str) {
                Some("text_delta") => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        events.push(InferenceEvent::TextDelta {
                            text: text.to_string(),
                        });
                    }
                }
                Some("input_json_delta") => events.push(InferenceEvent::ToolCallDelta {
                    delta: ToolCallDelta {
                        index: value
                            .get("index")
                            .and_then(Value::as_u64)
                            .unwrap_or_default() as u32,
                        id: None,
                        name: None,
                        arguments_fragment: delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                }),
                Some("thinking_delta") | Some("signature_delta") => {}
                Some(other) => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        format!("unknown Anthropic delta `{other}`"),
                    ));
                }
                None => {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "Anthropic delta has no type",
                    ));
                }
            }
        }
        Some("message_delta") => {
            if let Some(usage) = value.get("usage") {
                events.push(InferenceEvent::Usage {
                    usage: decode_usage(usage)?,
                });
            }
            let reason = value.pointer("/delta/stop_reason").and_then(Value::as_str);
            if let Some(reason) = reason {
                events.push(InferenceEvent::MessageEnd {
                    finish_reason: map_stop_reason(Some(reason)),
                    terminal_state: TerminalState::Complete,
                });
            }
        }
        Some("message_stop") => *stopped = true,
        Some("ping") | Some("content_block_stop") => {}
        Some("error") => {
            let detail = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("Anthropic stream error");
            return Err(InferenceError::provider_protocol(
                Some(502),
                sanitize(detail),
            ));
        }
        Some(other) => {
            return Err(InferenceError::provider_protocol(
                Some(502),
                format!("unknown Anthropic stream event `{other}`"),
            ));
        }
        None => {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Anthropic stream event has no type",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_blocks_are_not_exposed() {
        let response = json!({
            "id":"msg-1","model":"claude-test","stop_reason":"end_turn",
            "content":[{"type":"thinking","thinking":"secret chain"},{"type":"text","text":"answer"}],
            "usage":{"input_tokens":2,"output_tokens":1}
        });
        let decoded = AnthropicCodec.decode_response(&response).unwrap();
        assert_eq!(decoded.content, vec![ContentBlock::text("answer")]);
        assert!(
            !serde_json::to_string(&decoded)
                .unwrap()
                .contains("secret chain")
        );
    }

    #[test]
    fn rejects_stream_frames_over_the_buffer_limit() {
        let mut decoder = AnthropicStreamDecoder::with_max_buffer_bytes(8);
        let error = decoder.push(b"data: oversized").unwrap_err();
        assert_eq!(
            error.category,
            crate::inference::error::InferenceErrorCategory::Protocol
        );
    }

    #[test]
    fn rejects_truncated_streams() {
        let mut decoder = AnthropicStreamDecoder::default();
        decoder.push(b"data: {\"type\":\"message_start\"").unwrap();
        let error = decoder.finish().unwrap_err();
        assert_eq!(
            error.category,
            crate::inference::error::InferenceErrorCategory::Protocol
        );
    }

    #[test]
    fn selects_the_earliest_mixed_line_ending_delimiter() {
        assert_eq!(find_frame(b"data: first\n\ndata: second\r\n\r\n"), Some(11));
    }

    #[test]
    fn decodes_the_same_events_at_every_chunk_split() {
        let stream = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let mut whole = AnthropicStreamDecoder::default();
        let expected = whole.push(stream).unwrap();
        whole.finish().unwrap();
        for split in 0..=stream.len() {
            let mut decoder = AnthropicStreamDecoder::default();
            let mut actual = decoder.push(&stream[..split]).unwrap();
            actual.extend(decoder.push(&stream[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(actual, expected, "split at byte {split}");
        }
    }
}
