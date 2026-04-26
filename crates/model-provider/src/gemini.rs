use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub struct GeminiProvider {
    base_url: String,
    api_key: Option<String>,
    max_tokens: u32,
    client: Client,
}

const DEFAULT_GEMINI_MAX_TOKENS: u32 = 8192;

#[derive(Debug, Serialize)]
struct GenerateContentRequest {
    contents: Vec<Content>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<Content>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

#[derive(Debug, Serialize, Clone)]
struct Content {
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    parts: Vec<Part>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(untagged)]
enum Part {
    Text { text: String },
    Inline { inline_data: InlineData },
}

#[derive(Debug, Serialize, Clone)]
struct InlineData {
    mime_type: String,
    data: String,
}

#[derive(Debug, Serialize, Clone)]
struct GenerationConfig {
    temperature: f64,
    #[serde(rename = "maxOutputTokens")]
    max_output_tokens: u32,
}

#[derive(Debug, Deserialize)]
struct GenerateContentResponse {
    candidates: Option<Vec<Candidate>>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsageMetadata>,
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsageMetadata {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<CandidateContent>,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    parts: Vec<ResponsePart>,
}

#[derive(Debug, Deserialize)]
struct ResponsePart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: bool,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    message: String,
}

impl GeminiProvider {
    pub fn new(base_url: Option<&str>, api_key: Option<&str>) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| {
                anyhow::anyhow!("Failed to build reqwest Client for GeminiProvider: {e}")
            })?;

        Ok(Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string()),
            api_key: api_key.map(ToString::to_string),
            max_tokens: DEFAULT_GEMINI_MAX_TOKENS,
            client,
        })
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    async fn convert_messages(messages: &[ChatMessage]) -> (Option<Content>, Vec<Content>) {
        let mut system_instruction = None;
        let mut contents = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    system_instruction = Some(Content {
                        role: None,
                        parts: vec![Part::Text {
                            text: msg.content.clone(),
                        }],
                    });
                }
                "user" | "assistant" => {
                    let (text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
                    let mut parts = Vec::new();
                    if !text.trim().is_empty() {
                        parts.push(Part::Text { text });
                    }
                    for img_ref in image_refs {
                        if let Some(payload) =
                            crate::multimodal::extract_gemini_image_payload(&img_ref).await
                        {
                            parts.push(Part::Inline {
                                inline_data: InlineData {
                                    mime_type: payload.media_type,
                                    data: payload.data,
                                },
                            });
                        }
                    }
                    if parts.is_empty() {
                        parts.push(Part::Text {
                            text: msg.content.clone(),
                        });
                    }
                    contents.push(Content {
                        role: Some(if msg.role == "assistant" {
                            "model".to_string()
                        } else {
                            "user".to_string()
                        }),
                        parts,
                    });
                }
                _ => {}
            }
        }

        (system_instruction, contents)
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false, // Gemini supports it but it's complex, starting simple
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
            .ok_or_else(|| anyhow::anyhow!("No text response from Gemini"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let api_key = self
            .api_key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Gemini API key not set."))?;
        let (system_instruction, contents) = Self::convert_messages(request.messages).await;

        let gen_request = GenerateContentRequest {
            contents,
            system_instruction,
            generation_config: GenerationConfig {
                temperature,
                max_output_tokens: self.max_tokens,
            },
        };

        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url, model, api_key
        );
        let response = self.client.post(url).json(&gen_request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Gemini API error ({}): {}", status, body);
        }

        let gen_response: GenerateContentResponse = response.json().await?;
        if let Some(error) = gen_response.error {
            anyhow::bail!("Gemini API error: {}", error.message);
        }

        let usage = gen_response.usage_metadata.map(|u| TokenUsage {
            input_tokens: u.prompt_token_count,
            output_tokens: u.candidates_token_count,
            cached_input_tokens: None,
        });

        let mut text_parts = Vec::new();
        let mut reasoning_parts = Vec::new();

        if let Some(candidates) = gen_response.candidates
            && let Some(candidate) = candidates.first()
        {
            if let Some(content) = &candidate.content {
                for part in &content.parts {
                    if let Some(text) = &part.text {
                        if part.thought {
                            reasoning_parts.push(text.clone());
                        } else {
                            text_parts.push(text.clone());
                        }
                    }
                }
            }
        }

        Ok(ProviderChatResponse {
            text: (!text_parts.is_empty()).then(|| text_parts.join("")),
            tool_calls: Vec::new(),
            usage,
            reasoning_content: (!reasoning_parts.is_empty()).then(|| reasoning_parts.join("")),
        })
    }

    async fn chat_with_tools(
        &self,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        anyhow::bail!("Gemini tool calling not yet implemented in light-rs")
    }
}
