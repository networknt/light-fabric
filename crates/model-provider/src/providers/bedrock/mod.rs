use crate::inference::capabilities::GenerationCapabilities;
use crate::inference::content::{ContentBlock as CanonicalContentBlock, Role};
use crate::inference::error::InferenceError;
use crate::inference::provider::{
    GenerationProvider, GenerationStream, ProviderProtocol, ProviderRequestContext,
};
use crate::inference::request::{InferenceRequest, ToolChoice as CanonicalToolChoice};
use crate::inference::response::{
    FinishReason, GenerateOutputItem, InferenceResponse, ItemStatus, NormalizedUsage,
    ProviderContinuationState, ProviderEvidence, TerminalState,
};
use crate::inference::stream::{InferenceEvent, ToolCallDelta};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_sdk_bedrockruntime::config::{Region, Token};
use aws_sdk_bedrockruntime::types::{
    ContentBlock, ContentBlockDelta, ContentBlockStart, ConversationRole, ConverseOutput,
    ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration, Message,
    ReasoningContentBlock, ReasoningTextBlock, StopReason, SystemContentBlock, Tool, ToolChoice,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolResultStatus,
    ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::error::metadata::ProvideErrorMetadata;
use aws_smithy_types::{Document, Number};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::OnceCell;
use zeroize::Zeroizing;

pub const CODEC_VERSION: &str = "bedrock-converse-v1";

#[derive(Debug, Clone)]
pub enum BedrockAuth {
    ApiKey(String),
    DefaultChain,
}

pub struct BedrockConverseProvider {
    region: String,
    endpoint_url: Option<String>,
    auth: BedrockAuth,
    capabilities: GenerationCapabilities,
    timeout: Duration,
    client: OnceCell<aws_sdk_bedrockruntime::Client>,
}

impl BedrockConverseProvider {
    pub fn new(
        region: impl Into<String>,
        endpoint_url: Option<String>,
        auth: BedrockAuth,
        capabilities: GenerationCapabilities,
        timeout: Duration,
    ) -> Result<Self, InferenceError> {
        let region = region.into();
        if region.trim().is_empty() {
            return Err(InferenceError::invalid_request(
                "Bedrock endpoint region must not be empty",
            ));
        }
        if matches!(&auth, BedrockAuth::ApiKey(token) if token.is_empty()) {
            return Err(InferenceError::security_invariant(
                "Bedrock API key resolved to empty material",
            ));
        }
        Ok(Self {
            region,
            endpoint_url,
            auth,
            capabilities,
            timeout,
            client: OnceCell::new(),
        })
    }

    async fn client(&self) -> Result<&aws_sdk_bedrockruntime::Client, InferenceError> {
        self.client
            .get_or_try_init(|| async {
                let region = Region::new(self.region.clone());
                let mut builder = match &self.auth {
                    BedrockAuth::ApiKey(token) => aws_sdk_bedrockruntime::Config::builder()
                        .behavior_version(BehaviorVersion::latest())
                        .region(region)
                        .bearer_token(Token::new(token.clone(), None)),
                    BedrockAuth::DefaultChain => {
                        let shared = aws_config::defaults(BehaviorVersion::latest())
                            .region(region)
                            .load()
                            .await;
                        aws_sdk_bedrockruntime::config::Builder::from(&shared)
                    }
                };
                if let Some(endpoint_url) = &self.endpoint_url {
                    builder = builder.endpoint_url(endpoint_url);
                }
                Ok(aws_sdk_bedrockruntime::Client::from_conf(builder.build()))
            })
            .await
    }
}

#[async_trait]
impl GenerationProvider for BedrockConverseProvider {
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::BedrockConverse
    }

    fn capabilities(&self) -> GenerationCapabilities {
        self.capabilities.clone()
    }

    async fn generate(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        context.check_active()?;
        let client = self.client().await?;
        let encoded = encode_request(&request)?;
        let operation = client
            .converse()
            .model_id(&request.model)
            .set_messages(Some(encoded.messages))
            .set_system(non_empty(encoded.system))
            .set_inference_config(encoded.inference_config)
            .set_tool_config(encoded.tool_config)
            .set_additional_model_request_fields(encoded.additional_model_request_fields);
        let remaining = context
            .remaining()
            .unwrap_or(self.timeout)
            .min(self.timeout);
        let sent = tokio::select! {
            _ = context.cancellation.cancelled() => return Err(InferenceError::cancelled()),
            result = tokio::time::timeout(remaining, operation.send()) => {
                result.map_err(|_| InferenceError::timeout_after_possible_acceptance())?
            }
        };
        decode_response(sent.map_err(map_sdk_error)?, &request.model)
    }

    async fn generate_stream(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
        context.check_active()?;
        let client = self.client().await?;
        let encoded = encode_request(&request)?;
        let operation = client
            .converse_stream()
            .model_id(&request.model)
            .set_messages(Some(encoded.messages))
            .set_system(non_empty(encoded.system))
            .set_inference_config(encoded.inference_config)
            .set_tool_config(encoded.tool_config)
            .set_additional_model_request_fields(encoded.additional_model_request_fields);
        let remaining = context
            .remaining()
            .unwrap_or(self.timeout)
            .min(self.timeout);
        let response = tokio::select! {
            _ = context.cancellation.cancelled() => return Err(InferenceError::cancelled()),
            result = tokio::time::timeout(remaining, operation.send()) => {
                result.map_err(|_| InferenceError::timeout_after_possible_acceptance())?
                    .map_err(map_sdk_error)?
            }
        };
        let mut receiver = response.stream;
        let cancellation = context.cancellation.clone();
        let deadline = context.deadline;
        let physical_model = request.model.clone();
        let stream = async_stream::try_stream! {
            yield InferenceEvent::MessageStart {
                evidence: ProviderEvidence {
                    physical_model: Some(physical_model),
                    api_version: Some(CODEC_VERSION.to_string()),
                    ..ProviderEvidence::default()
                },
            };
            let mut stream_state = BedrockStreamState::default();
            loop {
                let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
                    Err(InferenceError::timeout_after_possible_acceptance())?;
                    unreachable!();
                };
                let next = tokio::select! {
                    _ = cancellation.cancelled() => Err(InferenceError::cancelled()),
                    result = tokio::time::timeout(remaining, receiver.recv()) => match result {
                        Ok(result) => result.map_err(|error| InferenceError::network(sanitize(&error.to_string()))),
                        Err(_) => Err(InferenceError::timeout_after_possible_acceptance()),
                    },
                }?;
                let Some(event) = next else { break };
                for event in decode_stream_event(event, &mut stream_state)? {
                    yield event;
                }
            }
        };
        Ok(stream.boxed())
    }
}

#[derive(Debug)]
struct EncodedRequest {
    messages: Vec<Message>,
    system: Vec<SystemContentBlock>,
    inference_config: Option<InferenceConfiguration>,
    tool_config: Option<ToolConfiguration>,
    additional_model_request_fields: Option<Document>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum SerializedReasoningBlock {
    ReasoningText {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedContent {
        data: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SerializedReasoningState {
    version: u8,
    turns: Vec<SerializedReasoningTurn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SerializedReasoningTurn {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    message_index: Option<usize>,
    blocks: Vec<SerializedReasoningBlock>,
}

fn encode_request(request: &InferenceRequest) -> Result<EncodedRequest, InferenceError> {
    if !request.extensions.is_empty() {
        return Err(InferenceError::unsupported(
            "provider-specific extensions are not enabled for bedrock_converse_v1",
        ));
    }
    if request
        .response_format
        .as_ref()
        .is_some_and(|format| !matches!(format, crate::inference::ResponseFormat::Text))
    {
        return Err(InferenceError::unsupported(
            "structured output is not enabled in bedrock_converse_v1",
        ));
    }
    let mut system = Vec::new();
    let mut messages = Vec::<Message>::new();
    let mut message_positions = vec![None; request.messages.len()];
    for (source_index, message) in request.messages.iter().enumerate() {
        if message.role == Role::System {
            for block in &message.content {
                match block {
                    CanonicalContentBlock::Text { text } => {
                        system.push(SystemContentBlock::Text(text.clone()));
                    }
                    _ => {
                        return Err(InferenceError::unsupported(
                            "Bedrock system content must be text",
                        ));
                    }
                }
            }
            continue;
        }
        let role = match message.role {
            Role::Assistant => ConversationRole::Assistant,
            Role::User | Role::Tool => ConversationRole::User,
            Role::System => unreachable!(),
        };
        let mut content = Vec::new();
        for block in &message.content {
            content.push(encode_content(block)?);
        }
        if content.is_empty() {
            return Err(InferenceError::invalid_request(
                "Bedrock messages must contain at least one content block",
            ));
        }
        let previous_position = messages.len().checked_sub(1);
        if let Some(previous) = messages.last_mut()
            && previous.role == role
            && role == ConversationRole::User
        {
            message_positions[source_index] = previous_position;
            previous.content.extend(content);
        } else {
            messages.push(
                Message::builder()
                    .role(role)
                    .set_content(Some(content))
                    .build()
                    .map_err(build_error)?,
            );
            message_positions[source_index] = Some(messages.len() - 1);
        }
    }
    if messages.is_empty() {
        return Err(InferenceError::invalid_request(
            "Bedrock Converse requires at least one non-system message",
        ));
    }
    if let Some(continuation) = &request.provider_continuation {
        if !matches!(
            continuation.protocol,
            ProviderProtocol::BedrockConverse | ProviderProtocol::AnthropicMessages
        ) {
            return Err(InferenceError::invalid_request(
                "provider continuation protocol does not match Bedrock Converse",
            ));
        }
        let state: SerializedReasoningState = serde_json::from_slice(&continuation.payload)
            .map_err(|_| InferenceError::invalid_request("invalid Bedrock reasoning state"))?;
        if state.version != 1
            || state.turns.is_empty()
            || state.turns.iter().any(|turn| turn.blocks.is_empty())
        {
            return Err(InferenceError::invalid_request(
                "unsupported or empty Bedrock reasoning state",
            ));
        }
        for turn in state.turns {
            let target = if let Some(source_index) = turn.message_index {
                message_positions
                    .get(source_index)
                    .and_then(|position| *position)
                    .ok_or_else(|| {
                        InferenceError::invalid_request(
                            "Bedrock reasoning state references an invalid message",
                        )
                    })?
            } else {
                messages
                    .iter()
                    .rposition(|message| message.role == ConversationRole::Assistant)
                    .ok_or_else(|| {
                        InferenceError::invalid_request(
                            "Bedrock reasoning state requires prior assistant history",
                        )
                    })?
            };
            let assistant = messages.get_mut(target).ok_or_else(|| {
                InferenceError::invalid_request("Bedrock assistant content is invalid")
            })?;
            if assistant.role != ConversationRole::Assistant {
                return Err(InferenceError::invalid_request(
                    "Bedrock reasoning state must reference assistant history",
                ));
            }
            let mut reasoning = turn
                .blocks
                .into_iter()
                .map(decode_reasoning_input)
                .collect::<Result<Vec<_>, _>>()?;
            reasoning.append(&mut assistant.content);
            assistant.content = reasoning;
        }
    }
    let has_sampling = request.sampling.temperature.is_some()
        || request.sampling.top_p.is_some()
        || !request.sampling.stop.is_empty()
        || request.token_limits.max_output_tokens.is_some();
    let inference_config = has_sampling.then(|| {
        InferenceConfiguration::builder()
            .set_max_tokens(
                request
                    .token_limits
                    .max_output_tokens
                    .map(|value| value as i32),
            )
            .set_temperature(request.sampling.temperature.map(|value| value as f32))
            .set_top_p(request.sampling.top_p.map(|value| value as f32))
            .set_stop_sequences(
                (!request.sampling.stop.is_empty()).then(|| request.sampling.stop.clone()),
            )
            .build()
    });
    let tool_config = encode_tools(request)?;
    let additional_model_request_fields = encode_reasoning_controls(request)?;
    Ok(EncodedRequest {
        messages,
        system,
        inference_config,
        tool_config,
        additional_model_request_fields,
    })
}

fn encode_reasoning_controls(
    request: &InferenceRequest,
) -> Result<Option<Document>, InferenceError> {
    if request.reasoning.is_none() && request.provider_continuation.is_none() {
        return Ok(None);
    }
    let mut fields = serde_json::Map::new();
    fields.insert(
        "thinking".to_string(),
        serde_json::json!({"type":"adaptive"}),
    );
    if let Some(reasoning) = &request.reasoning {
        if reasoning.summary.is_some() {
            return Err(InferenceError::unsupported(
                "Bedrock Converse reasoning summaries are not enabled",
            ));
        }
        if let Some(effort) = reasoning.effort.as_deref() {
            if !matches!(effort, "low" | "medium" | "high" | "max") {
                return Err(InferenceError::invalid_request(
                    "reasoning effort must be low, medium, high, or max",
                ));
            }
            fields.insert(
                "output_config".to_string(),
                serde_json::json!({"effort":effort}),
            );
        }
    }
    json_to_document(&Value::Object(fields)).map(Some)
}

fn encode_content(block: &CanonicalContentBlock) -> Result<ContentBlock, InferenceError> {
    match block {
        CanonicalContentBlock::Text { text } => Ok(ContentBlock::Text(text.clone())),
        CanonicalContentBlock::Refusal { .. } => Err(InferenceError::unsupported(
            "refusal content cannot be sent to Bedrock",
        )),
        CanonicalContentBlock::Image { source } => {
            let (metadata, data) = source.url.split_once(',').ok_or_else(|| {
                InferenceError::unsupported("Bedrock image input requires an inline data URL")
            })?;
            let media_type = metadata
                .strip_prefix("data:")
                .and_then(|value| value.strip_suffix(";base64"))
                .ok_or_else(|| {
                    InferenceError::invalid_request(
                        "Bedrock image data URL must contain base64 media",
                    )
                })?;
            let format = match media_type {
                "image/gif" => ImageFormat::Gif,
                "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
                "image/png" => ImageFormat::Png,
                "image/webp" => ImageFormat::Webp,
                _ => {
                    return Err(InferenceError::unsupported(
                        "Bedrock image media type is not supported",
                    ));
                }
            };
            let bytes = STANDARD
                .decode(data)
                .map_err(|_| InferenceError::invalid_request("image data is not valid base64"))?;
            if bytes.is_empty() || bytes.len() > 20 * 1024 * 1024 {
                return Err(InferenceError::invalid_request(
                    "Bedrock image data exceeds the configured image bound",
                ));
            }
            Ok(ContentBlock::Image(
                ImageBlock::builder()
                    .format(format)
                    .source(ImageSource::Bytes(aws_smithy_types::Blob::new(bytes)))
                    .build()
                    .map_err(build_error)?,
            ))
        }
        CanonicalContentBlock::ToolCall { call } => Ok(ContentBlock::ToolUse(
            ToolUseBlock::builder()
                .tool_use_id(&call.id)
                .name(&call.name)
                .input(json_to_document(&call.arguments)?)
                .build()
                .map_err(build_error)?,
        )),
        CanonicalContentBlock::ToolResult { result } => {
            let content = result
                .content
                .iter()
                .map(|block| match block {
                    CanonicalContentBlock::Text { text } => {
                        Ok(ToolResultContentBlock::Text(text.clone()))
                    }
                    _ => Err(InferenceError::unsupported(
                        "Bedrock tool results support text in bedrock_converse_v1",
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ContentBlock::ToolResult(
                ToolResultBlock::builder()
                    .tool_use_id(&result.tool_call_id)
                    .set_content(Some(content))
                    .set_status(Some(if result.is_error {
                        ToolResultStatus::Error
                    } else {
                        ToolResultStatus::Success
                    }))
                    .build()
                    .map_err(build_error)?,
            ))
        }
    }
}

fn encode_tools(request: &InferenceRequest) -> Result<Option<ToolConfiguration>, InferenceError> {
    if request.tools.is_empty() {
        if request.tool_choice.is_some() {
            return Err(InferenceError::invalid_request(
                "tool choice requires at least one tool",
            ));
        }
        return Ok(None);
    }
    let tools = request
        .tools
        .iter()
        .map(|tool| {
            Ok(Tool::ToolSpec(
                ToolSpecification::builder()
                    .name(&tool.name)
                    .description(&tool.description)
                    .input_schema(ToolInputSchema::Json(json_to_document(&tool.input_schema)?))
                    .build()
                    .map_err(build_error)?,
            ))
        })
        .collect::<Result<Vec<_>, InferenceError>>()?;
    let choice = match request.tool_choice.as_ref() {
        None | Some(CanonicalToolChoice::Auto) => Some(ToolChoice::Auto(
            aws_sdk_bedrockruntime::types::AutoToolChoice::builder().build(),
        )),
        Some(CanonicalToolChoice::Required) => Some(ToolChoice::Any(
            aws_sdk_bedrockruntime::types::AnyToolChoice::builder().build(),
        )),
        Some(CanonicalToolChoice::Tool { name }) => Some(ToolChoice::Tool(
            aws_sdk_bedrockruntime::types::SpecificToolChoice::builder()
                .name(name)
                .build()
                .map_err(build_error)?,
        )),
        Some(CanonicalToolChoice::None) => {
            return Err(InferenceError::unsupported(
                "Bedrock Converse has no tool_choice=none representation; omit tools instead",
            ));
        }
    };
    Ok(Some(
        ToolConfiguration::builder()
            .set_tools(Some(tools))
            .set_tool_choice(choice)
            .build()
            .map_err(build_error)?,
    ))
}

fn decode_response(
    response: aws_sdk_bedrockruntime::operation::converse::ConverseOutput,
    physical_model: &str,
) -> Result<InferenceResponse, InferenceError> {
    if response.additional_model_response_fields().is_some() {
        return Err(InferenceError::provider_protocol(
            Some(502),
            "unrequested Bedrock additional model response fields",
        ));
    }
    let mut output = Vec::new();
    let mut message_content = Vec::new();
    let mut reasoning_blocks = Vec::new();
    let message = match response.output() {
        Some(ConverseOutput::Message(message)) => message,
        Some(_) => {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "unknown Bedrock Converse output variant",
            ));
        }
        None => {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "Bedrock Converse response is missing output",
            ));
        }
    };
    for block in message.content() {
        match block {
            ContentBlock::Text(text) => message_content.push(CanonicalContentBlock::text(text)),
            ContentBlock::ToolUse(tool) => output.push(GenerateOutputItem::FunctionCall {
                id: format!("function-{}", tool.tool_use_id()),
                call_id: tool.tool_use_id().to_string(),
                name: tool.name().to_string(),
                arguments: document_to_json(tool.input())?,
                status: ItemStatus::Completed,
            }),
            ContentBlock::ReasoningContent(block) => {
                reasoning_blocks.push(encode_reasoning_output(block)?);
            }
            _ => {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "unsupported Bedrock content block in generation response",
                ));
            }
        }
    }
    if !message_content.is_empty() {
        output.insert(
            0,
            GenerateOutputItem::Message {
                id: "message-0".to_string(),
                role: Role::Assistant,
                content: message_content,
                status: ItemStatus::Completed,
            },
        );
    }
    let raw_stop = response.stop_reason().as_str().to_string();
    let continuation = if reasoning_blocks.is_empty() {
        None
    } else {
        Some(ProviderContinuationState {
            protocol: ProviderProtocol::BedrockConverse,
            payload: Zeroizing::new(
                serde_json::to_vec(&SerializedReasoningState {
                    version: 1,
                    turns: vec![SerializedReasoningTurn {
                        message_index: None,
                        blocks: reasoning_blocks,
                    }],
                })
                .map_err(|_| {
                    InferenceError::protocol("Bedrock reasoning state did not serialize")
                })?,
            ),
        })
    };
    Ok(InferenceResponse {
        output,
        finish_reason: map_stop_reason(response.stop_reason()),
        usage: response.usage().map(normalize_usage),
        evidence: ProviderEvidence {
            physical_model: Some(physical_model.to_string()),
            api_version: Some(CODEC_VERSION.to_string()),
            raw_finish_reason: Some(raw_stop),
            continuation,
            ..ProviderEvidence::default()
        },
        terminal_state: TerminalState::Complete,
    })
}

fn decode_reasoning_input(block: SerializedReasoningBlock) -> Result<ContentBlock, InferenceError> {
    let block = match block {
        SerializedReasoningBlock::ReasoningText { text, signature } => {
            let mut builder = ReasoningTextBlock::builder().text(text);
            builder = builder.set_signature(signature);
            ReasoningContentBlock::ReasoningText(builder.build().map_err(build_error)?)
        }
        SerializedReasoningBlock::RedactedContent { data } => {
            ReasoningContentBlock::RedactedContent(aws_smithy_types::Blob::new(
                STANDARD.decode(data).map_err(|_| {
                    InferenceError::invalid_request("invalid redacted reasoning bytes")
                })?,
            ))
        }
    };
    Ok(ContentBlock::ReasoningContent(block))
}

fn encode_reasoning_output(
    block: &ReasoningContentBlock,
) -> Result<SerializedReasoningBlock, InferenceError> {
    match block {
        ReasoningContentBlock::ReasoningText(block) => {
            Ok(SerializedReasoningBlock::ReasoningText {
                text: block.text().to_string(),
                signature: block.signature().map(ToString::to_string),
            })
        }
        ReasoningContentBlock::RedactedContent(data) => {
            Ok(SerializedReasoningBlock::RedactedContent {
                data: STANDARD.encode(data.as_ref()),
            })
        }
        _ => Err(InferenceError::provider_protocol(
            Some(502),
            "unknown Bedrock reasoning content variant",
        )),
    }
}

fn decode_stream_event(
    event: ConverseStreamOutput,
    state: &mut BedrockStreamState,
) -> Result<Vec<InferenceEvent>, InferenceError> {
    match event {
        ConverseStreamOutput::MessageStart(_) => {
            if state.message_started || state.message_stopped {
                return Err(InferenceError::protocol("duplicate Bedrock message start"));
            }
            state.message_started = true;
            Ok(Vec::new())
        }
        ConverseStreamOutput::ContentBlockStop(event) => {
            require_open_message(state)?;
            let index = event.content_block_index();
            if state.closed_blocks.contains(&index) {
                return Err(InferenceError::protocol(
                    "duplicate Bedrock content-block stop",
                ));
            }
            if let Some(tool) = state.tools.remove(&index) {
                serde_json::from_str::<Value>(&tool.arguments).map_err(|_| {
                    InferenceError::protocol("Bedrock tool stream produced invalid JSON")
                })?;
            }
            state.closed_blocks.insert(index);
            Ok(Vec::new())
        }
        ConverseStreamOutput::ContentBlockStart(event) => {
            require_open_message(state)?;
            match event.start() {
                Some(ContentBlockStart::ToolUse(tool)) => {
                    let index = event.content_block_index();
                    if state.tools.contains_key(&index) || state.closed_blocks.contains(&index) {
                        return Err(InferenceError::protocol(
                            "duplicate Bedrock content-block start",
                        ));
                    }
                    state.tools.insert(
                        index,
                        StreamingToolBlock {
                            name: tool.name().to_string(),
                            arguments: String::new(),
                        },
                    );
                    Ok(vec![InferenceEvent::ToolCallDelta {
                        delta: ToolCallDelta {
                            index: index as u32,
                            id: Some(tool.tool_use_id().to_string()),
                            name: Some(tool.name().to_string()),
                            arguments_fragment: String::new(),
                        },
                    }])
                }
                Some(_) => Err(InferenceError::protocol(
                    "unsupported Bedrock stream content-block start",
                )),
                None => Err(InferenceError::protocol(
                    "Bedrock stream content-block start is missing payload",
                )),
            }
        }
        ConverseStreamOutput::ContentBlockDelta(event) => {
            require_open_message(state)?;
            if state.closed_blocks.contains(&event.content_block_index()) {
                return Err(InferenceError::protocol(
                    "Bedrock delta followed content-block stop",
                ));
            }
            match event.delta() {
                Some(ContentBlockDelta::Text(text)) => {
                    Ok(vec![InferenceEvent::TextDelta { text: text.clone() }])
                }
                Some(ContentBlockDelta::ToolUse(delta)) => {
                    Ok(vec![InferenceEvent::ToolCallDelta {
                        delta: {
                            let index = event.content_block_index();
                            let tool = state.tools.get_mut(&index).ok_or_else(|| {
                                InferenceError::protocol("Bedrock tool delta has no matching start")
                            })?;
                            tool.arguments.push_str(delta.input());
                            ToolCallDelta {
                                index: index as u32,
                                id: None,
                                name: Some(tool.name.clone()),
                                arguments_fragment: delta.input().to_string(),
                            }
                        },
                    }])
                }
                Some(ContentBlockDelta::ReasoningContent(delta)) => {
                    let block = state
                        .reasoning
                        .entry(event.content_block_index())
                        .or_default();
                    match delta {
                    aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta::Text(value) => {
                        if !block.redacted.is_empty() {
                            return Err(InferenceError::protocol(
                                "Bedrock reasoning block mixed text and redacted content",
                            ));
                        }
                        block.text.push_str(value);
                    }
                    aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta::Signature(value) => {
                        if !block.redacted.is_empty() {
                            return Err(InferenceError::protocol(
                                "Bedrock reasoning block mixed signature and redacted content",
                            ));
                        }
                        block.signature.get_or_insert_with(String::new).push_str(value);
                    }
                    aws_sdk_bedrockruntime::types::ReasoningContentBlockDelta::RedactedContent(value) => {
                        if !block.text.is_empty() || block.signature.is_some() {
                            return Err(InferenceError::protocol(
                                "Bedrock reasoning block mixed redacted and text content",
                            ));
                        }
                        block.redacted.extend_from_slice(value.as_ref());
                    }
                    _ => {
                        return Err(InferenceError::protocol(
                            "unknown Bedrock reasoning stream delta",
                        ));
                    }
                }
                    Ok(Vec::new())
                }
                Some(_) => Err(InferenceError::protocol(
                    "unsupported Bedrock stream delta in generation response",
                )),
                None => Err(InferenceError::protocol(
                    "Bedrock stream delta is missing payload",
                )),
            }
        }
        ConverseStreamOutput::Metadata(event) => {
            if !state.message_stopped {
                return Err(InferenceError::protocol(
                    "Bedrock metadata preceded message stop",
                ));
            }
            if event.trace().is_some() {
                return Err(InferenceError::protocol(
                    "Bedrock guardrail trace is not enabled for this profile",
                ));
            }
            Ok(event
                .usage()
                .map(|usage| InferenceEvent::Usage {
                    usage: normalize_usage(usage),
                })
                .into_iter()
                .collect())
        }
        ConverseStreamOutput::MessageStop(event) => {
            require_open_message(state)?;
            if event.additional_model_response_fields().is_some() {
                return Err(InferenceError::protocol(
                    "unrequested Bedrock additional model response fields",
                ));
            }
            if !state.tools.is_empty() {
                return Err(InferenceError::protocol(
                    "Bedrock message stopped with an open tool block",
                ));
            }
            state.message_stopped = true;
            let mut output = Vec::new();
            if !state.reasoning.is_empty() {
                let blocks = std::mem::take(&mut state.reasoning)
                    .into_iter()
                    .map(|(_, block)| block.into_serialized())
                    .collect::<Result<Vec<_>, _>>()?;
                output.push(InferenceEvent::ProviderContinuation {
                    state: ProviderContinuationState {
                        protocol: ProviderProtocol::BedrockConverse,
                        payload: Zeroizing::new(
                            serde_json::to_vec(&SerializedReasoningState {
                                version: 1,
                                turns: vec![SerializedReasoningTurn {
                                    message_index: None,
                                    blocks,
                                }],
                            })
                            .map_err(|_| {
                                InferenceError::protocol(
                                    "Bedrock streaming reasoning state did not serialize",
                                )
                            })?,
                        ),
                    },
                });
            }
            output.push(InferenceEvent::MessageEnd {
                finish_reason: map_stop_reason(event.stop_reason()),
                terminal_state: TerminalState::Complete,
            });
            Ok(output)
        }
        _ => Err(InferenceError::protocol("unknown Bedrock stream event")),
    }
}

#[derive(Default)]
struct BedrockStreamState {
    message_started: bool,
    message_stopped: bool,
    tools: HashMap<i32, StreamingToolBlock>,
    closed_blocks: std::collections::BTreeSet<i32>,
    reasoning: std::collections::BTreeMap<i32, StreamingReasoningBlock>,
}

fn require_open_message(state: &BedrockStreamState) -> Result<(), InferenceError> {
    if !state.message_started {
        return Err(InferenceError::protocol(
            "Bedrock content preceded message start",
        ));
    }
    if state.message_stopped {
        return Err(InferenceError::protocol(
            "Bedrock content followed message stop",
        ));
    }
    Ok(())
}

struct StreamingToolBlock {
    name: String,
    arguments: String,
}

#[derive(Default)]
struct StreamingReasoningBlock {
    text: String,
    signature: Option<String>,
    redacted: Vec<u8>,
}

impl StreamingReasoningBlock {
    fn into_serialized(self) -> Result<SerializedReasoningBlock, InferenceError> {
        if !self.redacted.is_empty() {
            return Ok(SerializedReasoningBlock::RedactedContent {
                data: STANDARD.encode(self.redacted),
            });
        }
        if self.text.is_empty() {
            return Err(InferenceError::protocol(
                "Bedrock reasoning stream produced an empty content block",
            ));
        }
        Ok(SerializedReasoningBlock::ReasoningText {
            text: self.text,
            signature: self.signature,
        })
    }
}

fn normalize_usage(usage: &aws_sdk_bedrockruntime::types::TokenUsage) -> NormalizedUsage {
    NormalizedUsage {
        input_tokens: Some(usage.input_tokens() as u64),
        output_tokens: Some(usage.output_tokens() as u64),
        cached_input_tokens: usage.cache_read_input_tokens().map(|value| value as u64),
        reasoning_tokens: None,
    }
}

fn map_stop_reason(reason: &StopReason) -> FinishReason {
    match reason.as_str() {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "guardrail_intervened" | "content_filtered" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn map_sdk_error<E, R>(error: aws_sdk_bedrockruntime::error::SdkError<E, R>) -> InferenceError
where
    E: std::fmt::Debug + ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    let detail = sanitize(&error.to_string());
    match &error {
        aws_sdk_bedrockruntime::error::SdkError::TimeoutError(_) => {
            InferenceError::timeout_after_possible_acceptance()
        }
        aws_sdk_bedrockruntime::error::SdkError::DispatchFailure(_) => {
            InferenceError::network(detail)
        }
        aws_sdk_bedrockruntime::error::SdkError::ServiceError(service) => {
            let status = match service.err().code() {
                Some("ValidationException" | "ResourceNotFoundException") => 400,
                Some("AccessDeniedException") => 403,
                Some("ThrottlingException" | "ModelNotReadyException") => 429,
                Some(
                    "ServiceUnavailableException"
                    | "InternalServerException"
                    | "ModelErrorException",
                ) => 503,
                _ => 502,
            };
            InferenceError::from_status(status, None, detail)
        }
        _ => InferenceError::protocol(detail),
    }
}

fn json_to_document(value: &Value) -> Result<Document, InferenceError> {
    Ok(match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Document::Number(Number::NegInt(value))
            } else if let Some(value) = value.as_u64() {
                Document::Number(Number::PosInt(value))
            } else if let Some(value) = value.as_f64() {
                Document::Number(Number::Float(value))
            } else {
                return Err(InferenceError::invalid_request("invalid JSON number"));
            }
        }
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(
            values
                .iter()
                .map(json_to_document)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Value::Object(values) => Document::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), json_to_document(value)?)))
                .collect::<Result<HashMap<_, _>, InferenceError>>()?,
        ),
    })
}

fn document_to_json(value: &Document) -> Result<Value, InferenceError> {
    Ok(match value {
        Document::Null => Value::Null,
        Document::Bool(value) => Value::Bool(*value),
        Document::Number(Number::NegInt(value)) => Value::Number((*value).into()),
        Document::Number(Number::PosInt(value)) => Value::Number((*value).into()),
        Document::Number(Number::Float(value)) => serde_json::Number::from_f64(*value)
            .map(Value::Number)
            .ok_or_else(|| InferenceError::protocol("Bedrock returned a non-finite number"))?,
        Document::String(value) => Value::String(value.clone()),
        Document::Array(values) => Value::Array(
            values
                .iter()
                .map(document_to_json)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Document::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| Ok((key.clone(), document_to_json(value)?)))
                .collect::<Result<serde_json::Map<_, _>, InferenceError>>()?,
        ),
    })
}

fn non_empty<T>(values: Vec<T>) -> Option<Vec<T>> {
    (!values.is_empty()).then_some(values)
}

fn build_error(error: impl std::fmt::Display) -> InferenceError {
    InferenceError::invalid_request(sanitize(&error.to_string()))
}

fn sanitize(detail: &str) -> String {
    let compact = detail.split_whitespace().collect::<Vec<_>>().join(" ");
    compact.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inference::content::{Message as CanonicalMessage, ToolCall, ToolResult};
    use crate::inference::request::ToolDefinition;
    use crate::providers::anthropic::AnthropicClientCodec;
    use serde_json::json;

    #[test]
    fn encodes_text_and_tools() {
        let mut request = InferenceRequest::text("us.anthropic.claude-sonnet-4-6", "weather");
        request.token_limits.max_output_tokens = Some(128);
        request.tools.push(ToolDefinition {
            name: "get_weather".to_string(),
            description: "Get weather".to_string(),
            input_schema: json!({"type":"object","properties":{"city":{"type":"string"}}}),
        });
        request.tool_choice = Some(CanonicalToolChoice::Required);
        let encoded = encode_request(&request).expect("request encodes");
        assert_eq!(encoded.messages.len(), 1);
        assert!(encoded.tool_config.is_some());
        assert_eq!(encoded.inference_config.unwrap().max_tokens(), Some(128));
    }

    #[test]
    fn merges_tool_result_into_user_turn() {
        let mut request = InferenceRequest::text("model", "weather");
        request.messages.push(CanonicalMessage {
            role: Role::Assistant,
            content: vec![CanonicalContentBlock::ToolCall {
                call: ToolCall {
                    id: "tool-1".to_string(),
                    name: "get_weather".to_string(),
                    arguments: json!({"city":"Toronto"}),
                },
            }],
        });
        request.messages.push(CanonicalMessage {
            role: Role::Tool,
            content: vec![CanonicalContentBlock::ToolResult {
                result: ToolResult {
                    tool_call_id: "tool-1".to_string(),
                    content: vec![CanonicalContentBlock::text("sunny")],
                    is_error: false,
                },
            }],
        });
        let encoded = encode_request(&request).expect("request encodes");
        assert_eq!(encoded.messages.len(), 3);
    }

    #[test]
    fn reattaches_each_anthropic_reasoning_turn_before_converse_dispatch() {
        let (request, _) = AnthropicClientCodec
            .parse_request(
                br#"{"model":"claude","max_tokens":32,"messages":[
                  {"role":"assistant","content":[{"type":"thinking","thinking":"first","signature":"one"},{"type":"text","text":"a"}]},
                  {"role":"user","content":"next"},
                  {"role":"assistant","content":[{"type":"thinking","thinking":"second","signature":"two"},{"type":"text","text":"b"}]},
                  {"role":"user","content":"finish"}
                ]}"#,
            )
            .unwrap();
        let encoded = encode_request(&request).unwrap();
        assert!(matches!(
            encoded.messages[0].content.first(),
            Some(ContentBlock::ReasoningContent(_))
        ));
        assert!(matches!(
            encoded.messages[2].content.first(),
            Some(ContentBlock::ReasoningContent(_))
        ));
    }

    #[test]
    fn rejects_unknown_extensions_before_dispatch() {
        let mut request = InferenceRequest::text("model", "hello");
        request.extensions.insert("beta".to_string(), json!(true));
        assert!(matches!(
            encode_request(&request).unwrap_err().category,
            crate::inference::error::InferenceErrorCategory::UnsupportedFeature
        ));
    }

    #[test]
    fn document_round_trip_preserves_json() {
        let value = json!({"a":[true, null, 4, -2, 1.5],"b":"text"});
        let document = json_to_document(&value).expect("document");
        assert_eq!(document_to_json(&document).expect("json"), value);
    }

    #[tokio::test]
    async fn api_key_client_sets_an_explicit_behavior_version() {
        let provider = BedrockConverseProvider::new(
            "us-east-1",
            Some("http://127.0.0.1:9".to_string()),
            BedrockAuth::ApiKey("test-api-key".to_string()),
            GenerationCapabilities::default(),
            Duration::from_millis(100),
        )
        .expect("provider");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            provider.generate(
                ProviderRequestContext::with_timeout("behavior-version", Duration::from_secs(1)),
                InferenceRequest::text("test-model", "hello"),
            ),
        )
        .await;

        assert!(result.is_err() || result.expect("completed request").is_err());
    }
}
