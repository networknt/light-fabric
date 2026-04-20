use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct GlmProvider {
    api_key_id: String,
    api_key_secret: String,
    base_url: String,
    token_cache: Mutex<Option<(String, u64)>>,
    client: Client,
}

#[derive(Debug, Serialize)]
struct NativeChatRequest {
    model: String,
    messages: Vec<NativeMessage>,
    temperature: f64,
}

#[derive(Debug, Serialize)]
struct NativeMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct NativeChatResponse {
    choices: Vec<NativeChoice>,
}

#[derive(Debug, Deserialize)]
struct NativeChoice {
    message: NativeResponseMessage,
}

#[derive(Debug, Deserialize)]
struct NativeResponseMessage {
    content: String,
}

impl GlmProvider {
    pub fn new(api_key: Option<&str>, base_url: Option<&str>) -> anyhow::Result<Self> {
        let (id, secret) = api_key
            .and_then(|k| k.split_once('.'))
            .map(|(id, secret)| (id.to_string(), secret.to_string()))
            .unwrap_or_default();

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build reqwest Client for GlmProvider: {e}"))?;

        Ok(Self {
            api_key_id: id,
            api_key_secret: secret,
            base_url: base_url.map(|u| u.trim_end_matches('/').to_string()).unwrap_or_else(|| "https://open.bigmodel.cn/api/paas/v4".to_string()),
            token_cache: Mutex::new(None),
            client,
        })
    }

    fn generate_token(&self) -> anyhow::Result<String> {
        if self.api_key_id.is_empty() || self.api_key_secret.is_empty() {
            anyhow::bail!("GLM API key not set or invalid format. Expected 'id.secret'.");
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)?
            .as_millis() as u64;

        if let Ok(cache) = self.token_cache.lock() {
            if let Some((ref token, expiry)) = *cache {
                if now_ms < expiry {
                    return Ok(token.clone());
                }
            }
        }

        let exp_ms = now_ms + 210_000; // 3.5 minutes

        let header_json = r#"{"alg":"HS256","typ":"JWT","sign_type":"SIGN"}"#;
        let header_b64 = base64_url_encode(header_json.as_bytes());

        let payload_json = format!(
            r#"{{"api_key":"{}","exp":{},"timestamp":{}}}"#,
            self.api_key_id, exp_ms, now_ms
        );
        let payload_b64 = base64_url_encode(payload_json.as_bytes());

        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = Hmac::<Sha256>::new_from_slice(self.api_key_secret.as_bytes())?;
        mac.update(signing_input.as_bytes());
        let signature = mac.finalize().into_bytes();
        let sig_b64 = base64_url_encode(&signature);

        let token = format!("{signing_input}.{sig_b64}");

        if let Ok(mut cache) = self.token_cache.lock() {
            *cache = Some((token.clone(), now_ms + 180_000));
        }

        Ok(token)
    }
}

fn base64_url_encode(data: &[u8]) -> String {
    use base64::engine::{Engine, general_purpose::URL_SAFE_NO_PAD};
    URL_SAFE_NO_PAD.encode(data)
}

#[async_trait]
impl Provider for GlmProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false,
            vision: false,
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
        let resp = self.chat_with_history(&messages, model, temperature).await?;
        Ok(resp)
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let resp = self.chat(ProviderChatRequest { messages, tools: None }, model, temperature).await?;
        resp.text.ok_or_else(|| anyhow::anyhow!("No text response from GLM"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let token = self.generate_token()?;

        let native_messages: Vec<NativeMessage> = request.messages
            .iter()
            .map(|m| NativeMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();

        let native_request = NativeChatRequest {
            model: model.to_string(),
            messages: native_messages,
            temperature,
        };

        let url = format!("{}/chat/completions", self.base_url);
        let response = self.client
            .post(url)
            .header("Authorization", format!("Bearer {token}"))
            .json(&native_request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("GLM API error ({}): {}", status, body);
        }

        let native_response: NativeChatResponse = response.json().await?;
        let choice = native_response.choices.into_iter().next().ok_or_else(|| anyhow::anyhow!("No response from GLM"))?;

        Ok(ProviderChatResponse {
            text: Some(choice.message.content),
            tool_calls: Vec::new(),
            usage: None,
            reasoning_content: None,
        })
    }

    async fn chat_with_tools(
        &self,
        _messages: &[ChatMessage],
        _tools: &[serde_json::Value],
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        anyhow::bail!("GLM tool calling not yet implemented in light-rs")
    }
}
