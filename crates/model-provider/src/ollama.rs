use crate::multimodal;
use crate::traits::{
    ChatMessage, ChatResponse, Provider, ProviderCapabilities, TokenUsage, ToolCall,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub struct OllamaProvider {
    base_url: String,
    api_key: Option<String>,
    reasoning_enabled: Option<bool>,
    is_local: bool,
    client: Client,
}

// ─── Request Structures ───────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    stream: bool,
    options: Options,
    #[serde(skip_serializing_if = "Option::is_none")]
    think: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize)]
struct Message {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutgoingToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct OutgoingToolCall {
    #[serde(rename = "type")]
    kind: String,
    function: OutgoingFunction,
}

#[derive(Debug, Clone, Serialize)]
struct OutgoingFunction {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct Options {
    temperature: f64,
}

// ─── Response Structures ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ApiChatResponse {
    message: ResponseMessage,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
    /// Some models return a "thinking" field with internal reasoning
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OllamaToolCall {
    id: Option<String>,
    function: OllamaFunction,
}

#[derive(Debug, Deserialize)]
struct OllamaFunction {
    name: String,
    #[serde(default, deserialize_with = "deserialize_args")]
    arguments: serde_json::Value,
}

// ─── serde Helpers ───────────────────────────────────────────────────────────
fn deserialize_args<'de, D>(deserializer: D) -> Result<serde_json::Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;

    if let Some(s) = value.as_str() {
        match serde_json::from_str::<serde_json::Value>(s) {
            Ok(v) => Ok(v),
            Err(_) => Ok(serde_json::json!({})),
        }
    } else {
        Ok(value)
    }
}
// ─── Implementation ───────────────────────────────────────────────────────────

impl OllamaProvider {
    fn normalize_base_url(raw_url: &str) -> String {
        let trimmed = raw_url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return "http://localhost:11434".to_string();
        }

        trimmed
            .strip_suffix("/api/chat")
            .or_else(|| trimmed.strip_suffix("/api"))
            .unwrap_or(trimmed)
            .trim_end_matches('/')
            .to_string()
    }

    pub fn new(base_url: Option<&str>, api_key: Option<&str>) -> anyhow::Result<Self> {
        Self::new_with_reasoning(base_url, api_key, None)
    }

    pub fn new_with_reasoning(
        base_url: Option<&str>,
        api_key: Option<&str>,
        reasoning_enabled: Option<bool>,
    ) -> anyhow::Result<Self> {
        let api_key = api_key.and_then(|value| {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        });

        let base_url = Self::normalize_base_url(base_url.unwrap_or("http://localhost:11434"));
        let is_local = reqwest::Url::parse(&base_url)
            .ok()
            .and_then(|url| url.host_str().map(|host| host.to_string()))
            .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"));

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|error| {
                anyhow::anyhow!("Failed to build reqwest Client for OllamaProvider: {error}")
            })?;

        Ok(Self {
            base_url,
            api_key,
            reasoning_enabled,
            is_local,
            client,
        })
    }

    fn resolve_request_details(&self, model: &str) -> anyhow::Result<(String, bool)> {
        let requests_cloud = model.ends_with(":cloud");
        let normalized_model = model.strip_suffix(":cloud").unwrap_or(model).to_string();

        if requests_cloud && self.is_local {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but Ollama endpoint is local. Configure api_url with a remote Ollama endpoint.",
                model
            );
        }

        if requests_cloud && self.api_key.is_none() {
            anyhow::bail!(
                "Model '{}' requested cloud routing, but no API key is configured. Set OLLAMA_API_KEY or config api_key.",
                model
            );
        }

        let should_auth = self.api_key.is_some() && !self.is_local;

        Ok((normalized_model, should_auth))
    }

    fn parse_tool_arguments(arguments: &str) -> serde_json::Value {
        serde_json::from_str(arguments).unwrap_or_else(|_| serde_json::json!({}))
    }

    /// Extracts text and reasoning from model output, handling <think> tags.
    /// Prefers text/reasoning from the content field, falling back to the
    /// thinking field for models that separate reasoning (like Qwen with think: true).
    fn extract_text_and_reasoning(
        content: &str,
        thinking: Option<&str>,
    ) -> (Option<String>, Option<String>) {
        let (content_text, content_reasoning) = Self::parse_think_tags(content);

        if !content_text.trim().is_empty() {
            // Case 1: content has actual text (after stripping tags).
            return (Some(content_text.trim().to_string()), content_reasoning);
        }

        // Case 2: content was empty or only had reasoning. Check thinking field.
        if let Some(t) = thinking.map(str::trim).filter(|t| !t.is_empty()) {
            let (thinking_text, thinking_reasoning) = Self::parse_think_tags(t);

            // Prefer reasoning from thinking field if content reasoning is absent.
            let final_reasoning = content_reasoning.clone().or(thinking_reasoning);

            if !thinking_text.trim().is_empty() {
                return (Some(thinking_text.trim().to_string()), final_reasoning);
            } else if final_reasoning.is_some() {
                // Thinking field was also only reasoning.
                return (None, final_reasoning);
            }
        }

        // Case 3: No text found.
        (None, content_reasoning)
    }

    /// Strips <think> tags and captures reasoning content.
    fn parse_think_tags(s: &str) -> (String, Option<String>) {
        let mut text = String::with_capacity(s.len());
        let mut reasoning = String::new();
        let mut rest = s;
        loop {
            if let Some(start) = rest.find("<think>") {
                text.push_str(&rest[..start]);
                let remaining = &rest[start + "<think>".len()..];
                if let Some(end) = remaining.find("</think>") {
                    reasoning.push_str(&remaining[..end]);
                    rest = &remaining[end + "</think>".len()..];
                } else {
                    // Unclosed tag: treat the rest as reasoning.
                    reasoning.push_str(remaining);
                    break;
                }
            } else {
                text.push_str(rest);
                break;
            }
        }
        let reasoning = (!reasoning.trim().is_empty()).then(|| reasoning.trim().to_string());
        (text, reasoning)
    }

    fn fallback_text_for_empty_content(model: &str, thinking: Option<&str>) -> String {
        if let Some(thinking) = thinking.map(str::trim).filter(|value| !value.is_empty()) {
            let thinking_reply_excerpt: String = thinking.chars().take(200).collect();
            tracing::warn!(
                "Ollama returned empty content with only thinking for model '{}'. Model may have stopped prematurely.",
                model
            );
            return format!(
                "I was thinking about this: {}... but I didn't complete my response. Could you try asking again?",
                thinking_reply_excerpt
            );
        }

        tracing::warn!(
            "Ollama returned empty or whitespace content with no tool calls for model '{}'",
            model
        );
        "I couldn't get a complete response from Ollama. Please try again or switch to a different model."
            .to_string()
    }

    /// Build a chat request with an explicit `think` value.
    fn build_chat_request_with_think(
        &self,
        messages: Vec<Message>,
        model: &str,
        temperature: f64,
        tools: Option<&[serde_json::Value]>,
        think: Option<bool>,
    ) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages,
            stream: false,
            options: Options { temperature },
            think,
            tools: tools.map(|t| t.to_vec()),
        }
    }

    fn convert_user_message_content(&self, content: &str) -> (Option<String>, Option<Vec<String>>) {
        let (cleaned, image_refs) = multimodal::parse_image_markers(content);
        if image_refs.is_empty() {
            return (Some(content.to_string()), None);
        }

        let images: Vec<String> = image_refs
            .iter()
            .filter_map(|reference| multimodal::extract_ollama_image_payload(reference))
            .collect();

        if images.is_empty() {
            return (Some(cleaned.trim().to_string()), None);
        }

        let cleaned = cleaned.trim();
        let content = if cleaned.is_empty() {
            None
        } else {
            Some(cleaned.to_string())
        };

        (content, Some(images))
    }

    /// Convert internal chat history format to Ollama's native tool-call message schema.
    fn convert_messages(&self, messages: &[ChatMessage]) -> Vec<Message> {
        let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

        messages
            .iter()
            .map(|message| {
                if message.role == "assistant"
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content)
                    && let Some(tool_calls_value) = value.get("tool_calls")
                    && let Ok(parsed_calls) =
                        serde_json::from_value::<Vec<ToolCall>>(tool_calls_value.clone())
                {
                    let outgoing_calls: Vec<OutgoingToolCall> = parsed_calls
                        .into_iter()
                        .map(|call| {
                            tool_name_by_id.insert(call.id.clone(), call.name.clone());
                            OutgoingToolCall {
                                kind: "function".to_string(),
                                function: OutgoingFunction {
                                    name: call.name,
                                    arguments: Self::parse_tool_arguments(&call.arguments),
                                },
                            }
                        })
                        .collect();
                    let content = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                    return Message {
                        role: "assistant".to_string(),
                        content,
                        images: None,
                        tool_calls: Some(outgoing_calls),
                        tool_name: None,
                    };
                }

                if message.role == "tool"
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(&message.content)
                {
                    let tool_name = value
                        .get("tool_name")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| {
                            value
                                .get("tool_call_id")
                                .and_then(serde_json::Value::as_str)
                                .and_then(|id| tool_name_by_id.get(id))
                                .cloned()
                        });
                    let content = value
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                        .or_else(|| {
                            (!message.content.trim().is_empty()).then_some(message.content.clone())
                        });

                    return Message {
                        role: "tool".to_string(),
                        content,
                        images: None,
                        tool_calls: None,
                        tool_name,
                    };
                }

                if message.role == "user" {
                    let (content, images) = self.convert_user_message_content(&message.content);
                    return Message {
                        role: "user".to_string(),
                        content,
                        images,
                        tool_calls: None,
                        tool_name: None,
                    };
                }

                Message {
                    role: message.role.clone(),
                    content: Some(message.content.clone()),
                    images: None,
                    tool_calls: None,
                    tool_name: None,
                }
            })
            .collect()
    }

    /// Send a single HTTP request to Ollama and parse the response.
    async fn send_request_inner(
        &self,
        messages: &[Message],
        model: &str,
        temperature: f64,
        should_auth: bool,
        tools: Option<&[serde_json::Value]>,
        think: Option<bool>,
    ) -> anyhow::Result<ApiChatResponse> {
        let request =
            self.build_chat_request_with_think(messages.to_vec(), model, temperature, tools, think);

        let url = format!("{}/api/chat", self.base_url);

        let mut request_builder = self.client.post(&url).json(&request);

        if should_auth && let Some(key) = self.api_key.as_ref() {
            request_builder = request_builder.bearer_auth(key);
        }

        let response = request_builder.send().await?;
        let status = response.status();

        let body = response.bytes().await?;

        if !status.is_success() {
            let raw = String::from_utf8_lossy(&body);
            anyhow::bail!("Ollama API error ({}): {}", status, raw);
        }

        let chat_response: ApiChatResponse = serde_json::from_slice(&body)?;

        Ok(chat_response)
    }

    async fn send_request(
        &self,
        messages: Vec<Message>,
        model: &str,
        temperature: f64,
        should_auth: bool,
        tools: Option<&[serde_json::Value]>,
    ) -> anyhow::Result<ApiChatResponse> {
        let result = self
            .send_request_inner(
                &messages,
                model,
                temperature,
                should_auth,
                tools,
                self.reasoning_enabled,
            )
            .await;

        match result {
            Ok(resp) => Ok(resp),
            Err(first_err) if self.reasoning_enabled == Some(true) => {
                tracing::warn!(
                    model = model,
                    error = %first_err,
                    "Ollama request failed with think=true; retrying without reasoning (model may not support it)"
                );
                self.send_request_inner(&messages, model, temperature, should_auth, tools, None)
                    .await
                    .map_err(|retry_err| {
                        anyhow::anyhow!(
                            "Ollama request failed with think=true, and retry without reasoning also failed. \
                            initial error: {}; retry error: {}",
                            first_err,
                            retry_err
                        )
                    })
            }
            Err(e) => Err(e),
        }
    }

    fn format_tool_calls_for_loop(&self, tool_calls: &[OllamaToolCall]) -> String {
        let formatted_calls: Vec<serde_json::Value> = tool_calls
            .iter()
            .map(|tc| {
                let (tool_name, tool_args) = self.extract_tool_name_and_args(tc);
                let args_str =
                    serde_json::to_string(&tool_args).unwrap_or_else(|_| "{}".to_string());

                serde_json::json!({
                    "id": tc.id.clone().unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": args_str
                    }
                })
            })
            .collect();

        serde_json::json!({
            "content": "",
            "tool_calls": formatted_calls
        })
        .to_string()
    }

    fn extract_tool_name_and_args(&self, tc: &OllamaToolCall) -> (String, serde_json::Value) {
        let name = &tc.function.name;
        let args = &tc.function.arguments;

        if (name == "tool_call"
            || name == "tool.call"
            || name.starts_with("tool_call>")
            || name.starts_with("tool_call<"))
            && let Some(nested_name) = args.get("name").and_then(|v| v.as_str())
        {
            let nested_args = args
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            return (nested_name.to_string(), nested_args);
        }

        if let Some(stripped) = name.strip_prefix("tool.") {
            return (stripped.to_string(), args.clone());
        }

        (name.clone(), args.clone())
    }
}

#[async_trait]
impl Provider for OllamaProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: true,
            vision: true,
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
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let mut messages = Vec::new();

        if let Some(sys) = system_prompt {
            messages.push(Message {
                role: "system".to_string(),
                content: Some(sys.to_string()),
                images: None,
                tool_calls: None,
                tool_name: None,
            });
        }

        let (user_content, user_images) = self.convert_user_message_content(message);
        messages.push(Message {
            role: "user".to_string(),
            content: user_content,
            images: user_images,
            tool_calls: None,
            tool_name: None,
        });

        let response = self
            .send_request(messages, &normalized_model, temperature, should_auth, None)
            .await?;

        if !response.message.tool_calls.is_empty() {
            return Ok(self.format_tool_calls_for_loop(&response.message.tool_calls));
        }

        let (text, _reasoning) = Self::extract_text_and_reasoning(
            &response.message.content,
            response.message.thinking.as_deref(),
        );

        if let Some(content) = text {
            return Ok(content);
        }

        Ok(Self::fallback_text_for_empty_content(
            &normalized_model,
            response.message.thinking.as_deref(),
        ))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let api_messages = self.convert_messages(messages);

        let response = self
            .send_request(
                api_messages,
                &normalized_model,
                temperature,
                should_auth,
                None,
            )
            .await?;

        if !response.message.tool_calls.is_empty() {
            return Ok(self.format_tool_calls_for_loop(&response.message.tool_calls));
        }

        let (text, _reasoning) = Self::extract_text_and_reasoning(
            &response.message.content,
            response.message.thinking.as_deref(),
        );

        if let Some(content) = text {
            return Ok(content);
        }

        Ok(Self::fallback_text_for_empty_content(
            &normalized_model,
            response.message.thinking.as_deref(),
        ))
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (normalized_model, should_auth) = self.resolve_request_details(model)?;

        let api_messages = self.convert_messages(messages);
        let tools_opt = if tools.is_empty() { None } else { Some(tools) };

        let response = self
            .send_request(
                api_messages,
                &normalized_model,
                temperature,
                should_auth,
                tools_opt,
            )
            .await?;

        let usage = if response.prompt_eval_count.is_some() || response.eval_count.is_some() {
            Some(TokenUsage {
                input_tokens: response.prompt_eval_count,
                output_tokens: response.eval_count,
                cached_input_tokens: None,
            })
        } else {
            None
        };

        let (text, reasoning) = Self::extract_text_and_reasoning(
            &response.message.content,
            response.message.thinking.as_deref(),
        );

        if !response.message.tool_calls.is_empty() {
            let tool_calls: Vec<ToolCall> = response
                .message
                .tool_calls
                .iter()
                .map(|tc| {
                    let (name, args) = self.extract_tool_name_and_args(tc);
                    ToolCall {
                        id: tc
                            .id
                            .clone()
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                        name,
                        arguments: serde_json::to_string(&args)
                            .unwrap_or_else(|_| "{}".to_string()),
                    }
                })
                .collect();

            return Ok(ChatResponse {
                text,
                tool_calls,
                usage,
                reasoning_content: reasoning,
            });
        }

        let final_text = text.unwrap_or_else(|| {
            Self::fallback_text_for_empty_content(
                &normalized_model,
                response.message.thinking.as_deref(),
            )
        });

        Ok(ChatResponse {
            text: Some(final_text),
            tool_calls: vec![],
            usage,
            reasoning_content: reasoning,
        })
    }

    async fn chat(
        &self,
        request: crate::traits::ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let tools: Vec<serde_json::Value> = if let Some(specs) = request.tools {
            specs
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": &s.name,
                            "description": &s.description,
                            "parameters": &s.parameters
                        }
                    })
                })
                .collect()
        } else {
            vec![]
        };

        self.chat_with_tools(request.messages, &tools, model, temperature)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url() {
        let p = OllamaProvider::new(None, None).unwrap();
        assert_eq!(p.base_url, "http://localhost:11434");
    }

    #[test]
    fn custom_url_trailing_slash() {
        let p = OllamaProvider::new(Some("http://192.168.1.100:11434/"), None).unwrap();
        assert_eq!(p.base_url, "http://192.168.1.100:11434");
    }

    #[test]
    fn custom_url_no_trailing_slash() {
        let p = OllamaProvider::new(Some("http://myserver:11434"), None).unwrap();
        assert_eq!(p.base_url, "http://myserver:11434");
    }

    #[test]
    fn cloud_suffix_strips_model_name() {
        let p = OllamaProvider::new(Some("https://ollama.com"), Some("ollama-key")).unwrap();
        let (model, should_auth) = p.resolve_request_details("qwen3:cloud").unwrap();
        assert_eq!(model, "qwen3");
        assert!(should_auth);
    }

    #[test]
    fn response_deserializes() {
        let json = r#"{"message":{"role":"assistant","content":"Hello from Ollama!"}}"#;
        let resp: ApiChatResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.message.content, "Hello from Ollama!");
    }

    #[test]
    fn parse_think_tags_extracts_single_block() {
        let input = "<think>internal reasoning</think>Hello world";
        let (text, reasoning) = OllamaProvider::parse_think_tags(input);
        assert_eq!(text.trim(), "Hello world");
        assert_eq!(reasoning, Some("internal reasoning".to_string()));
    }

    #[test]
    fn extract_text_and_reasoning_prefers_content() {
        let (text, reasoning) = OllamaProvider::extract_text_and_reasoning(
            "<think>reasoning</think> hello",
            Some("more reasoning"),
        );
        assert_eq!(text, Some("hello".to_string()));
        assert_eq!(reasoning, Some("reasoning".to_string()));
    }

    #[test]
    fn extract_text_and_reasoning_handles_qwen_xml_in_thinking() {
        let thinking = "I need to check the date\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}\n</tool_call>";
        let (text, reasoning) = OllamaProvider::extract_text_and_reasoning("", Some(thinking));
        assert!(text.is_some());
        assert!(text.unwrap().contains("<tool_call>"));
        assert!(reasoning.is_none());
    }
}
