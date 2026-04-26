use crate::multimodal;
use crate::traits::{
    ChatMessage, ChatResponse, Provider, ProviderCapabilities, TokenUsage, ToolCall,
};
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

const DEFAULT_CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_CODEX_INSTRUCTIONS: &str = "You are Codex, a concise and helpful coding assistant.";

pub struct CodexProvider {
    responses_url: String,
    api_key: Option<String>,
    account_id: Option<String>,
    reasoning_effort: Option<String>,
    client: Client,
}

#[derive(Debug, Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<ResponsesInput>,
    instructions: String,
    store: bool,
    stream: bool,
    text: ResponsesTextOptions,
    reasoning: ResponsesReasoningOptions,
    include: Vec<String>,
    tool_choice: String,
    parallel_tool_calls: bool,
}

#[derive(Debug, Serialize)]
struct ResponsesInput {
    role: String,
    content: Vec<ResponsesInputContent>,
}

#[derive(Debug, Serialize)]
struct ResponsesInputContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct ResponsesTextOptions {
    verbosity: String,
}

#[derive(Debug, Serialize)]
struct ResponsesReasoningOptions {
    effort: String,
    summary: String,
}

#[derive(Debug, Deserialize)]
struct ResponsesResponse {
    #[serde(default)]
    output: Vec<ResponsesOutput>,
    #[serde(default)]
    output_text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ResponsesOutput {
    #[serde(default)]
    content: Vec<ResponsesContent>,
}

#[derive(Debug, Deserialize)]
struct ResponsesContent {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}

impl CodexProvider {
    fn normalize_responses_url(raw_url: &str) -> String {
        let candidate = raw_url.trim();
        if candidate.is_empty() {
            return DEFAULT_CODEX_RESPONSES_URL.to_string();
        }

        let mut parsed = match reqwest::Url::parse(candidate) {
            Ok(url) => url,
            Err(_) => return DEFAULT_CODEX_RESPONSES_URL.to_string(),
        };

        let path = parsed.path().trim_end_matches('/');
        if !path.ends_with("/responses") {
            let with_suffix = if path.is_empty() || path == "/" {
                "/responses".to_string()
            } else {
                format!("{path}/responses")
            };
            parsed.set_path(&with_suffix);
        }

        parsed.set_query(None);
        parsed.set_fragment(None);
        parsed.to_string()
    }

    pub fn new(
        base_url: Option<&str>,
        api_key: Option<&str>,
        account_id: Option<&str>,
        reasoning_effort: Option<&str>,
    ) -> anyhow::Result<Self> {
        let responses_url =
            Self::normalize_responses_url(base_url.unwrap_or(DEFAULT_CODEX_RESPONSES_URL));

        let client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| {
                anyhow::anyhow!("Failed to build reqwest Client for CodexProvider: {e}")
            })?;

        Ok(Self {
            responses_url,
            api_key: api_key.map(ToString::to_string),
            account_id: account_id.map(ToString::to_string),
            reasoning_effort: reasoning_effort.map(ToString::to_string),
            client,
        })
    }

    fn build_responses_input(&self, messages: &[ChatMessage]) -> (String, Vec<ResponsesInput>) {
        let mut system_parts: Vec<&str> = Vec::new();
        let mut input: Vec<ResponsesInput> = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => system_parts.push(&msg.content),
                "user" => {
                    let (cleaned_text, image_refs) = multimodal::parse_image_markers(&msg.content);
                    let mut content_items = Vec::new();

                    if !cleaned_text.trim().is_empty() {
                        content_items.push(ResponsesInputContent {
                            kind: "input_text".to_string(),
                            text: Some(cleaned_text),
                            image_url: None,
                        });
                    }

                    for image_ref in image_refs {
                        content_items.push(ResponsesInputContent {
                            kind: "input_image".to_string(),
                            text: None,
                            image_url: Some(image_ref),
                        });
                    }

                    if content_items.is_empty() {
                        content_items.push(ResponsesInputContent {
                            kind: "input_text".to_string(),
                            text: Some(String::new()),
                            image_url: None,
                        });
                    }

                    input.push(ResponsesInput {
                        role: "user".to_string(),
                        content: content_items,
                    });
                }
                "assistant" => {
                    input.push(ResponsesInput {
                        role: "assistant".to_string(),
                        content: vec![ResponsesInputContent {
                            kind: "output_text".to_string(),
                            text: Some(msg.content.clone()),
                            image_url: None,
                        }],
                    });
                }
                _ => {}
            }
        }

        let instructions = if system_parts.is_empty() {
            DEFAULT_CODEX_INSTRUCTIONS.to_string()
        } else {
            system_parts.join("\n\n")
        };

        (instructions, input)
    }

    fn resolve_reasoning_effort(&self, model_id: &str) -> String {
        let effort = self
            .reasoning_effort
            .as_deref()
            .unwrap_or("xhigh")
            .to_ascii_lowercase();
        let id = model_id.rsplit('/').next().unwrap_or(model_id);

        if id == "gpt-5-codex" {
            return match effort.as_str() {
                "low" | "medium" | "high" => effort,
                "minimal" => "low".to_string(),
                _ => "high".to_string(),
            };
        }
        effort
    }

    async fn send_request(
        &self,
        messages: &[ChatMessage],
        model: &str,
    ) -> anyhow::Result<ChatResponse> {
        let (instructions, input) = self.build_responses_input(messages);
        let normalized_model = model.rsplit('/').next().unwrap_or(model);

        let request = ResponsesRequest {
            model: normalized_model.to_string(),
            input,
            instructions,
            store: false,
            stream: true,
            text: ResponsesTextOptions {
                verbosity: "medium".to_string(),
            },
            reasoning: ResponsesReasoningOptions {
                effort: self.resolve_reasoning_effort(normalized_model),
                summary: "auto".to_string(),
            },
            include: vec!["reasoning.encrypted_content".to_string()],
            tool_choice: "auto".to_string(),
            parallel_tool_calls: true,
        };

        let mut request_builder = self
            .client
            .post(&self.responses_url)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "pi")
            .header("accept", "text/event-stream")
            .header("Content-Type", "application/json");

        if let Some(key) = &self.api_key {
            request_builder = request_builder.header("Authorization", format!("Bearer {key}"));
        }
        if let Some(account_id) = &self.account_id {
            request_builder = request_builder.header("chatgpt-account-id", account_id);
        }

        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Codex API error ({}): {}", status, body);
        }

        self.decode_responses_body(response).await
    }

    async fn decode_responses_body(
        &self,
        response: reqwest::Response,
    ) -> anyhow::Result<ChatResponse> {
        let mut body = String::new();
        let mut pending_utf8 = Vec::new();
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| anyhow::anyhow!("Codex stream error: {e}"))?;
            self.append_utf8_chunk(&mut body, &mut pending_utf8, &bytes)?;
        }

        self.parse_responses_payload(&body)
    }

    fn append_utf8_chunk(
        &self,
        body: &mut String,
        pending: &mut Vec<u8>,
        chunk: &[u8],
    ) -> anyhow::Result<()> {
        pending.extend_from_slice(chunk);
        match std::str::from_utf8(pending) {
            Ok(text) => {
                body.push_str(text);
                pending.clear();
                Ok(())
            }
            Err(e) => {
                let valid_up_to = e.valid_up_to();
                if valid_up_to > 0 {
                    let s = std::str::from_utf8(&pending[..valid_up_to]).unwrap();
                    body.push_str(s);
                    pending.drain(..valid_up_to);
                }
                if e.error_len().is_some() {
                    anyhow::bail!("Invalid UTF-8 in Codex response");
                }
                Ok(())
            }
        }
    }

    fn parse_responses_payload(&self, payload: &str) -> anyhow::Result<ChatResponse> {
        let mut text_accumulator = String::new();
        let reasoning_accumulator = String::new();
        let mut saw_delta = false;

        for line in payload.lines() {
            let line = line.trim();
            if !line.starts_with("data:") {
                continue;
            }
            let data = line[5..].trim();
            if data == "[DONE]" {
                break;
            }

            if let Ok(event) = serde_json::from_str::<Value>(data) {
                if let Some(msg) = self.extract_error(&event) {
                    anyhow::bail!("Codex stream error: {msg}");
                }

                let kind = event.get("type").and_then(Value::as_str);
                match kind {
                    Some("response.output_text.delta") => {
                        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                            text_accumulator.push_str(delta);
                            saw_delta = true;
                        }
                    }
                    Some("response.output_text.done") if !saw_delta => {
                        if let Some(text) = event.get("text").and_then(Value::as_str) {
                            text_accumulator.push_str(text);
                        }
                    }
                    // Extract reasoning if present in the event
                    _ => {
                        // Logic for reasoning extraction could go here if Codex returns it in deltas
                    }
                }
            }
        }

        if text_accumulator.is_empty() && !saw_delta {
            // Fallback: try parsing as a single JSON response
            if let Ok(resp) = serde_json::from_str::<ResponsesResponse>(payload) {
                if let Some(t) = resp.output_text {
                    text_accumulator = t;
                } else if let Some(output) = resp.output.first() {
                    for content in &output.content {
                        if content.kind.as_deref() == Some("output_text") {
                            if let Some(t) = &content.text {
                                text_accumulator.push_str(t);
                            }
                        }
                    }
                }
            }
        }

        Ok(ChatResponse {
            text: (!text_accumulator.is_empty()).then(|| text_accumulator),
            tool_calls: vec![],
            usage: None,
            reasoning_content: (!reasoning_accumulator.is_empty()).then(|| reasoning_accumulator),
        })
    }

    fn extract_error(&self, event: &Value) -> Option<String> {
        let kind = event.get("type").and_then(Value::as_str);
        if kind == Some("error") {
            return event
                .get("message")
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        if kind == Some("response.failed") {
            return event
                .get("response")
                .and_then(|r| r.get("error"))
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
        }
        None
    }
}

#[async_trait]
impl Provider for CodexProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: true,
            prompt_caching: false,
        }
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let mut messages = Vec::new();
        if let Some(sys) = system_prompt {
            messages.push(ChatMessage::system(sys));
        }
        messages.push(ChatMessage::user(message));

        let response = self
            .chat_with_history(&messages, model, _temperature)
            .await?;
        Ok(response)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        let response = self
            .chat(
                crate::traits::ChatRequest {
                    messages,
                    tools: None,
                },
                model,
                _temperature,
            )
            .await?;
        response
            .text
            .ok_or_else(|| anyhow::anyhow!("No text response from Codex"))
    }

    async fn chat(
        &self,
        request: crate::traits::ChatRequest<'_>,
        model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.send_request(request.messages, model).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        // Codex experimental responses API technically supports tools,
        // but for now we follow the existing native_tool_calling: false capability.
        self.chat(
            crate::traits::ChatRequest {
                messages,
                tools: None,
            },
            model,
            temperature,
        )
        .await
    }
}
