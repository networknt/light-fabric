use crate::inference::content::{ContentBlock, Message, Role, ToolCall};
use crate::inference::error::InferenceError;
use crate::inference::request::{InferenceRequest, ResponseFormat, ToolChoice};
use crate::inference::response::{
    FinishReason, InferenceResponse, NormalizedUsage, ProviderEvidence, TerminalState,
};
use crate::inference::stream::{InferenceEvent, StreamDecoder, ToolCallDelta};
use bytes::{Buf, BytesMut};
use serde_json::{Map, Value, json};

pub const CODEC_VERSION: &str = "openai-chat-completions-v1";
pub const DEFAULT_MAX_STREAM_BUFFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct OpenAiCodec;

impl OpenAiCodec {
    pub fn encode_request(
        &self,
        request: &InferenceRequest,
        stream: bool,
    ) -> Result<Value, InferenceError> {
        let mut object = Map::new();
        object.insert("model".to_string(), Value::String(request.model.clone()));
        object.insert(
            "messages".to_string(),
            Value::Array(
                request
                    .messages
                    .iter()
                    .map(encode_message)
                    .collect::<Result<Vec<_>, _>>()?,
            ),
        );
        object.insert("stream".to_string(), Value::Bool(stream));
        if stream {
            object.insert("stream_options".to_string(), json!({"include_usage":true}));
        }
        if let Some(temperature) = request.sampling.temperature {
            object.insert("temperature".to_string(), json!(temperature));
        }
        if let Some(top_p) = request.sampling.top_p {
            object.insert("top_p".to_string(), json!(top_p));
        }
        if !request.sampling.stop.is_empty() {
            object.insert("stop".to_string(), json!(request.sampling.stop));
        }
        if let Some(limit) = request.token_limits.max_output_tokens {
            object.insert("max_completion_tokens".to_string(), json!(limit));
        }
        if !request.tools.is_empty() {
            object.insert(
                "tools".to_string(),
                Value::Array(
                    request
                        .tools
                        .iter()
                        .map(|tool| {
                            json!({"type":"function","function":{
                                "name":tool.name,
                                "description":tool.description,
                                "parameters":tool.input_schema
                            }})
                        })
                        .collect(),
                ),
            );
        }
        if let Some(choice) = &request.tool_choice {
            object.insert("tool_choice".to_string(), encode_tool_choice(choice));
        }
        if let Some(format) = &request.response_format {
            object.insert(
                "response_format".to_string(),
                encode_response_format(format),
            );
        }
        for (name, value) in &request.extensions {
            if object.contains_key(name) {
                return Err(InferenceError::invalid_request(format!(
                    "extension `{name}` shadows an operated OpenAI field"
                )));
            }
            object.insert(name.clone(), value.clone());
        }
        Ok(Value::Object(object))
    }

    pub fn decode_response(&self, value: &Value) -> Result<InferenceResponse, InferenceError> {
        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
            .ok_or_else(|| {
                InferenceError::provider_protocol(Some(502), "OpenAI response has no choice")
            })?;
        let message = choice.get("message").ok_or_else(|| {
            InferenceError::provider_protocol(Some(502), "OpenAI choice has no message")
        })?;
        let mut content = Vec::new();
        if let Some(text) = message
            .get("content")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
        {
            content.push(ContentBlock::text(text));
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let id = required_string(call, "id", "OpenAI tool call id")?;
                let function = call.get("function").ok_or_else(|| {
                    InferenceError::provider_protocol(Some(502), "OpenAI tool call has no function")
                })?;
                let name = required_string(function, "name", "OpenAI tool name")?;
                let arguments_text =
                    required_string(function, "arguments", "OpenAI tool arguments")?;
                let arguments = serde_json::from_str(&arguments_text).map_err(|error| {
                    InferenceError::provider_protocol(
                        Some(502),
                        format!("OpenAI tool arguments drift: {error}"),
                    )
                })?;
                content.push(ContentBlock::ToolCall {
                    call: ToolCall {
                        id,
                        name,
                        arguments,
                    },
                });
            }
        }
        let raw_finish = choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        Ok(InferenceResponse {
            content,
            finish_reason: map_finish_reason(raw_finish.as_deref()),
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
                raw_finish_reason: raw_finish,
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
            .unwrap_or_else(|| format!("OpenAI provider returned HTTP {status}"));
        InferenceError::from_status(status, retry_after, detail)
    }
}

fn encode_message(message: &Message) -> Result<Value, InferenceError> {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let tool_results = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { result } => Some(result),
            _ => None,
        })
        .collect::<Vec<_>>();
    if message.role == Role::Tool {
        if tool_results.len() != 1 {
            return Err(InferenceError::invalid_request(
                "OpenAI tool message must contain exactly one tool result",
            ));
        }
        let result = tool_results[0];
        return Ok(json!({
            "role":"tool",
            "tool_call_id":result.tool_call_id,
            "content":flatten_text(&result.content)?
        }));
    }
    let mut text = String::new();
    let mut rich_content = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: value } => {
                text.push_str(value);
                rich_content.push(json!({"type":"text","text":value}));
            }
            ContentBlock::Image { source } => {
                rich_content.push(json!({"type":"image_url","image_url":{"url":source.url}}));
            }
            ContentBlock::ToolCall { call } => tool_calls.push(json!({
                "id":call.id,
                "type":"function",
                "function":{"name":call.name,"arguments":serde_json::to_string(&call.arguments).map_err(|error| InferenceError::invalid_request(error.to_string()))?}
            })),
            ContentBlock::ToolResult { .. } => {
                return Err(InferenceError::invalid_request("tool result appears outside tool role"));
            }
        }
    }
    let has_image = message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::Image { .. }));
    let content = if has_image {
        Value::Array(rich_content)
    } else if text.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    let mut encoded = json!({"role":role,"content":content});
    if !tool_calls.is_empty() {
        encoded["tool_calls"] = Value::Array(tool_calls);
    }
    Ok(encoded)
}

fn flatten_text(content: &[ContentBlock]) -> Result<String, InferenceError> {
    let mut text = String::new();
    for block in content {
        match block {
            ContentBlock::Text { text: value } => text.push_str(value),
            _ => return Err(InferenceError::unsupported("non-text OpenAI tool result")),
        }
    }
    Ok(text)
}

fn encode_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool { name } => json!({"type":"function","function":{"name":name}}),
    }
}

fn encode_response_format(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::Text => json!({"type":"text"}),
        ResponseFormat::JsonObject => json!({"type":"json_object"}),
        ResponseFormat::JsonSchema { name, schema } => {
            json!({"type":"json_schema","json_schema":{"name":name,"schema":schema}})
        }
    }
}

fn decode_usage(value: &Value) -> Result<NormalizedUsage, InferenceError> {
    if !value.is_object() {
        return Err(InferenceError::provider_protocol(
            Some(502),
            "OpenAI usage is not an object",
        ));
    }
    Ok(NormalizedUsage {
        input_tokens: value.get("prompt_tokens").and_then(Value::as_u64),
        output_tokens: value.get("completion_tokens").and_then(Value::as_u64),
        cached_input_tokens: value
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64),
        reasoning_tokens: value
            .pointer("/completion_tokens_details/reasoning_tokens")
            .and_then(Value::as_u64),
    })
}

fn required_string(value: &Value, field: &str, label: &str) -> Result<String, InferenceError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .ok_or_else(|| InferenceError::provider_protocol(Some(502), format!("{label} is missing")))
}

fn map_finish_reason(value: Option<&str>) -> FinishReason {
    match value {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("function_call") => FinishReason::ToolCalls,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(_) | None => FinishReason::Unknown,
    }
}

fn sanitize(value: &str) -> String {
    value.chars().take(512).collect()
}

#[derive(Debug)]
pub struct OpenAiStreamDecoder {
    buffer: BytesMut,
    done: bool,
    max_buffer_bytes: usize,
}

impl Default for OpenAiStreamDecoder {
    fn default() -> Self {
        Self::with_max_buffer_bytes(DEFAULT_MAX_STREAM_BUFFER_BYTES)
    }
}

impl OpenAiStreamDecoder {
    pub fn with_max_buffer_bytes(max_buffer_bytes: usize) -> Self {
        Self {
            buffer: BytesMut::new(),
            done: false,
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
                InferenceError::provider_protocol(Some(502), "OpenAI stream is not UTF-8")
            })?;
            for line in text.lines() {
                if self.done && !line.trim().is_empty() {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "OpenAI stream field after DONE",
                    ));
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    self.done = true;
                    continue;
                }
                if data.is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(data).map_err(|error| {
                    InferenceError::provider_protocol(
                        Some(502),
                        format!("malformed OpenAI stream event: {error}"),
                    )
                })?;
                decode_stream_value(&value, events)?;
            }
            if self.done {
                break;
            }
        }
        Ok(())
    }
}

impl StreamDecoder for OpenAiStreamDecoder {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<InferenceEvent>, InferenceError> {
        if self.done {
            if chunk.iter().all(|byte| byte.is_ascii_whitespace()) {
                return Ok(Vec::new());
            }
            return Err(InferenceError::provider_protocol(
                Some(502),
                "OpenAI stream data after DONE",
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
            if self.done {
                if !self.buffer.iter().all(|byte| byte.is_ascii_whitespace())
                    || !remaining.iter().all(|byte| byte.is_ascii_whitespace())
                {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "OpenAI stream data after DONE",
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
                "truncated OpenAI stream frame",
            ));
        }
        if !self.done {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "OpenAI stream ended without DONE",
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
        "OpenAI stream frame exceeds configured buffer limit",
    )
}

fn decode_stream_value(
    value: &Value,
    events: &mut Vec<InferenceEvent>,
) -> Result<(), InferenceError> {
    if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
        events.push(InferenceEvent::Usage {
            usage: decode_usage(usage)?,
        });
    }
    let Some(choice) = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    else {
        return Ok(());
    };
    let delta = choice.get("delta").unwrap_or(&Value::Null);
    if let Some(text) = delta
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        events.push(InferenceEvent::TextDelta {
            text: text.to_string(),
        });
    }
    if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            events.push(InferenceEvent::ToolCallDelta {
                delta: ToolCallDelta {
                    index: call
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or_default() as u32,
                    id: call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    name: call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    arguments_fragment: call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                },
            });
        }
    }
    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        events.push(InferenceEvent::MessageEnd {
            finish_reason: map_finish_reason(Some(reason)),
            terminal_state: TerminalState::Complete,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_tool_argument_fragments_across_chunks() {
        let mut decoder = OpenAiStreamDecoder::default();
        let first = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"lookup\",\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n";
        let second = b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n";
        let mut events = decoder.push(&first[..17]).unwrap();
        events.extend(decoder.push(&first[17..]).unwrap());
        events.extend(decoder.push(second).unwrap());
        decoder.finish().unwrap();
        let fragments = events
            .iter()
            .filter_map(|event| match event {
                InferenceEvent::ToolCallDelta { delta } => Some(delta.arguments_fragment.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(fragments, vec!["{\"a\":", "1}"]);
    }

    #[test]
    fn rejects_stream_frames_over_the_buffer_limit() {
        let mut decoder = OpenAiStreamDecoder::with_max_buffer_bytes(8);
        let error = decoder.push(b"data: oversized").unwrap_err();
        assert_eq!(
            error.category,
            crate::inference::error::InferenceErrorCategory::Protocol
        );
    }

    #[test]
    fn accepts_large_chunks_when_each_complete_frame_is_bounded() {
        let stream = b"data: {\"choices\":[]}\n\ndata: {\"choices\":[]}\n\ndata: [DONE]\n\n";
        assert!(stream.len() > 32);
        let mut decoder = OpenAiStreamDecoder::with_max_buffer_bytes(32);
        assert!(decoder.push(stream).is_ok());
        assert!(decoder.finish().is_ok());
    }

    #[test]
    fn rejects_data_after_done_in_the_same_push() {
        let mut decoder = OpenAiStreamDecoder::default();
        let error = decoder
            .push(b"data: [DONE]\n\ndata: {\"choices\":[]}\n\n")
            .unwrap_err();
        assert_eq!(
            error.category,
            crate::inference::error::InferenceErrorCategory::Protocol
        );
    }

    #[test]
    fn rejects_non_data_fields_after_done_in_the_same_frame() {
        let mut decoder = OpenAiStreamDecoder::default();
        let error = decoder
            .push(b"data: [DONE]\nid: unexpected\n\n")
            .unwrap_err();
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
        let stream = b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let mut whole = OpenAiStreamDecoder::default();
        let expected = whole.push(stream).unwrap();
        whole.finish().unwrap();
        for split in 0..=stream.len() {
            let mut decoder = OpenAiStreamDecoder::default();
            let mut actual = decoder.push(&stream[..split]).unwrap();
            actual.extend(decoder.push(&stream[split..]).unwrap());
            decoder.finish().unwrap();
            assert_eq!(actual, expected, "split at byte {split}");
        }
    }
}
