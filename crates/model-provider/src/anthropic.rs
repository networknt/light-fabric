use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall, ToolSpec,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub struct AnthropicProvider {
    base_url: String,
    api_key: Option<String>,
    max_tokens: u32,
    client: Client,
}

const DEFAULT_ANTHROPIC_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Serialize)]
struct NativeChatRequest<'a> {
    model: String,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<SystemPrompt>,
    messages: Vec<NativeMessage>,
    temperature: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<NativeToolSpec<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: Vec<NativeContentOut>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum NativeContentOut {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    #[serde(rename = "image")]
    Image { source: ImageSource },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
}

#[derive(Debug, Serialize)]
struct ImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Debug, Serialize)]
struct NativeToolSpec<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Clone, Serialize)]
struct CacheControl {
    #[serde(rename = "type")]
    cache_type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemPrompt {
    String(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Serialize)]
struct SystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    #[serde(default)]
    content: Vec<NativeContentIn>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct NativeContentIn {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

impl AnthropicProvider {
    pub fn new(base_url: Option<&str>, api_key: Option<&str>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| {
                anyhow::anyhow!("Failed to build reqwest Client for AnthropicProvider: {e}")
            })?;

        Ok(Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.anthropic.com".to_string()),
            api_key: api_key.map(ToString::to_string),
            max_tokens: DEFAULT_ANTHROPIC_MAX_TOKENS,
            client,
        })
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn convert_tools<'a>(tools: Option<&'a [ToolSpec]>) -> Option<Vec<NativeToolSpec<'a>>> {
        let items = tools?;
        if items.is_empty() {
            return None;
        }
        let mut native_tools: Vec<NativeToolSpec<'a>> = items
            .iter()
            .map(|tool| NativeToolSpec {
                name: &tool.name,
                description: &tool.description,
                input_schema: &tool.parameters,
                cache_control: None,
            })
            .collect();

        if let Some(last_tool) = native_tools.last_mut() {
            last_tool.cache_control = Some(CacheControl::ephemeral());
        }

        Some(native_tools)
    }

    async fn convert_messages(
        messages: &[ChatMessage],
    ) -> (Option<SystemPrompt>, Vec<NativeMessage>) {
        let mut system_text = None;
        let mut native_messages = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    if system_text.is_none() {
                        system_text = Some(msg.content.clone());
                    }
                }
                "assistant" => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content)
                        && let Some(tool_calls_value) = value.get("tool_calls")
                        && let Ok(parsed_calls) = serde_json::from_value::<Vec<ProviderToolCall>>(
                            tool_calls_value.clone(),
                        )
                    {
                        let mut blocks = Vec::new();
                        if let Some(text) = value
                            .get("content")
                            .and_then(Value::as_str)
                            .filter(|t| !t.is_empty())
                        {
                            blocks.push(NativeContentOut::Text {
                                text: text.to_string(),
                                cache_control: None,
                            });
                        }
                        for call in parsed_calls {
                            let input = serde_json::from_str(&call.arguments)
                                .unwrap_or_else(|_| serde_json::json!({}));
                            blocks.push(NativeContentOut::ToolUse {
                                id: call.id,
                                name: call.name,
                                input,
                            });
                        }
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: blocks,
                        });
                    } else if !msg.content.trim().is_empty() {
                        native_messages.push(NativeMessage {
                            role: "assistant".to_string(),
                            content: vec![NativeContentOut::Text {
                                text: msg.content.clone(),
                                cache_control: None,
                            }],
                        });
                    }
                }
                "tool" => {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content)
                        && let Some(tool_use_id) = value.get("tool_call_id").and_then(Value::as_str)
                    {
                        let content = value
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let tool_msg = NativeMessage {
                            role: "user".to_string(),
                            content: vec![NativeContentOut::ToolResult {
                                tool_use_id: tool_use_id.to_string(),
                                content,
                                cache_control: None,
                            }],
                        };
                        if native_messages.last().is_some_and(|m| m.role == "user") {
                            native_messages
                                .last_mut()
                                .unwrap()
                                .content
                                .extend(tool_msg.content);
                        } else {
                            native_messages.push(tool_msg);
                        }
                    }
                }
                "user" => {
                    let (text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
                    let mut content_blocks = Vec::new();

                    for img_ref in &image_refs {
                        if let Some(payload) =
                            crate::multimodal::extract_anthropic_image_payload(img_ref).await
                        {
                            content_blocks.push(NativeContentOut::Image {
                                source: ImageSource {
                                    source_type: "base64".to_string(),
                                    media_type: payload.media_type,
                                    data: payload.data,
                                },
                            });
                        }
                    }

                    if !text.trim().is_empty() {
                        content_blocks.push(NativeContentOut::Text {
                            text,
                            cache_control: None,
                        });
                    } else if content_blocks.is_empty() {
                        content_blocks.push(NativeContentOut::Text {
                            text: String::new(),
                            cache_control: None,
                        });
                    }

                    if native_messages.last().is_some_and(|m| m.role == "user") {
                        native_messages
                            .last_mut()
                            .unwrap()
                            .content
                            .extend(content_blocks);
                    } else {
                        native_messages.push(NativeMessage {
                            role: "user".to_string(),
                            content: content_blocks,
                        });
                    }
                }
                _ => {}
            }
        }

        let system_prompt = system_text.map(|text| {
            SystemPrompt::Blocks(vec![SystemBlock {
                block_type: "text".to_string(),
                text,
                cache_control: Some(CacheControl::ephemeral()),
            }])
        });

        if let Some(last_user_msg) = native_messages.iter_mut().rev().find(|m| m.role == "user") {
            if let Some(last_block) = last_user_msg.content.last_mut() {
                match last_block {
                    NativeContentOut::Text { cache_control, .. }
                    | NativeContentOut::ToolResult { cache_control, .. } => {
                        *cache_control = Some(CacheControl::ephemeral());
                    }
                    _ => {}
                }
            }
        }

        (system_prompt, native_messages)
    }

    async fn send_request(
        &self,
        system: Option<SystemPrompt>,
        messages: Vec<NativeMessage>,
        model: &str,
        temperature: f64,
        tools: Option<Vec<NativeToolSpec<'_>>>,
    ) -> anyhow::Result<ProviderChatResponse> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Anthropic API key not set."))?;

        let native_request = NativeChatRequest {
            model: model.to_string(),
            max_tokens: self.max_tokens,
            system,
            messages,
            temperature,
            tools,
            tool_choice: None,
        };

        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&native_request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Anthropic API error ({}): {}", status, body);
        }

        let native_response: NativeChatResponse = response.json().await?;
        let usage = native_response.usage.map(|u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_input_tokens: u.cache_read_input_tokens,
        });

        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();

        for block in native_response.content {
            match block.kind.as_str() {
                "text" => {
                    if let Some(t) = block.text {
                        text_parts.push(t);
                    }
                }
                "tool_use" => {
                    let name = block.name.unwrap_or_default();
                    let input = block.input.unwrap_or_else(|| serde_json::json!({}));
                    tool_calls.push(ProviderToolCall {
                        id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: input.to_string(),
                    });
                }
                _ => {}
            }
        }

        Ok(ProviderChatResponse {
            text: (!text_parts.is_empty()).then(|| text_parts.join("\n")),
            tool_calls,
            usage,
            reasoning_content: None,
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
            prompt_caching: true,
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
        if let Some(sys) = system_prompt {
            messages.push(ChatMessage::system(sys));
        }
        messages.push(ChatMessage::user(message));
        let resp = self
            .chat_with_history(&messages, model, temperature)
            .await?;
        Ok(resp)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let resp = self
            .chat(
                ProviderChatRequest {
                    messages,
                    tools: None,
                },
                model,
                temperature,
            )
            .await?;
        resp.text
            .ok_or_else(|| anyhow::anyhow!("No text response from Anthropic"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (system, messages) = Self::convert_messages(request.messages).await;
        let tools = Self::convert_tools(request.tools);
        self.send_request(system, messages, model, temperature, tools)
            .await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (system, messages) = Self::convert_messages(messages).await;
        let native_tools = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .iter()
                    .map(|t| {
                        let name = t.get("name").and_then(Value::as_str).unwrap_or("");
                        let description =
                            t.get("description").and_then(Value::as_str).unwrap_or("");
                        let parameters = t.get("parameters").unwrap_or(&Value::Null);
                        Ok(NativeToolSpec {
                            name,
                            description,
                            input_schema: parameters,
                            cache_control: None,
                        })
                    })
                    .collect::<Result<Vec<_>, anyhow::Error>>()?,
            )
        };
        self.send_request(system, messages, model, temperature, native_tools)
            .await
    }
}
