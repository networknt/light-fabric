use crate::traits::{
    ChatMessage, ChatRequest as ProviderChatRequest, ChatResponse as ProviderChatResponse,
    Provider, ProviderCapabilities, TokenUsage, ToolCall as ProviderToolCall,
};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use hmac::{Hmac, Mac};
use chrono::Utc;

pub struct BedrockProvider {
    region: String,
    access_key_id: Option<String>,
    secret_access_key: Option<String>,
    session_token: Option<String>,
    max_tokens: u32,
    client: Client,
}

const DEFAULT_REGION: &str = "us-east-1";
const DEFAULT_MAX_TOKENS: u32 = 4096;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConverseRequest {
    messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<SystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ConverseMessage {
    role: String,
    content: Vec<ContentBlock>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum ContentBlock {
    Text(TextBlock),
    Image(ImageWrapper),
}

#[derive(Debug, Serialize, Deserialize)]
struct TextBlock {
    text: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageWrapper {
    image: ImageBlock,
}

#[derive(Debug, Serialize, Deserialize)]
struct ImageBlock {
    format: String,
    source: ImageSource,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImageSource {
    bytes: String,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum SystemBlock {
    Text(TextBlock),
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceConfig {
    max_tokens: u32,
    temperature: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConverseResponse {
    output: Option<ConverseOutput>,
    usage: Option<BedrockUsage>,
}

#[derive(Debug, Deserialize)]
struct ConverseOutput {
    message: Option<ConverseOutputMessage>,
}

#[derive(Debug, Deserialize)]
struct ConverseOutputMessage {
    content: Vec<ResponseContentBlock>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ResponseContentBlock {
    Text(TextBlock),
    Other(serde_json::Value),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

impl BedrockProvider {
    pub fn new(
        region: Option<&str>,
        access_key_id: Option<&str>,
        secret_access_key: Option<&str>,
        session_token: Option<&str>,
    ) -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build reqwest Client for BedrockProvider: {e}"))?;

        Ok(Self {
            region: region.unwrap_or(DEFAULT_REGION).to_string(),
            access_key_id: access_key_id.map(ToString::to_string),
            secret_access_key: secret_access_key.map(ToString::to_string),
            session_token: session_token.map(ToString::to_string),
            max_tokens: DEFAULT_MAX_TOKENS,
            client,
        })
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    fn convert_messages(messages: &[ChatMessage]) -> (Option<Vec<SystemBlock>>, Vec<ConverseMessage>) {
        let mut system = Vec::new();
        let mut converse_messages = Vec::new();

        for msg in messages {
            match msg.role.as_str() {
                "system" => {
                    system.push(SystemBlock::Text(TextBlock { text: msg.content.clone() }));
                }
                "user" | "assistant" => {
                    let (text, image_refs) = crate::multimodal::parse_image_markers(&msg.content);
                    let mut content = Vec::new();
                    if !text.trim().is_empty() {
                        content.push(ContentBlock::Text(TextBlock { text }));
                    }
                    for img_ref in image_refs {
                        if let Some(payload) = crate::multimodal::extract_gemini_image_payload(&img_ref) {
                            let format = match payload.media_type.as_str() {
                                "image/png" => "png",
                                "image/gif" => "gif",
                                "image/webp" => "webp",
                                _ => "jpeg",
                            };
                            content.push(ContentBlock::Image(ImageWrapper {
                                image: ImageBlock {
                                    format: format.to_string(),
                                    source: ImageSource { bytes: payload.data },
                                },
                            }));
                        }
                    }
                    if content.is_empty() {
                        content.push(ContentBlock::Text(TextBlock { text: msg.content.clone() }));
                    }
                    converse_messages.push(ConverseMessage {
                        role: if msg.role == "assistant" { "assistant".to_string() } else { "user".to_string() },
                        content,
                    });
                }
                _ => {}
            }
        }

        (if system.is_empty() { None } else { Some(system) }, converse_messages)
    }

    fn sign_request(
        &self,
        method: &str,
        url: &str,
        payload: &[u8],
    ) -> anyhow::Result<reqwest::header::HeaderMap> {
        let access_key = self.access_key_id.as_ref().ok_or_else(|| anyhow::anyhow!("AWS access key not set"))?;
        let secret_key = self.secret_access_key.as_ref().ok_or_else(|| anyhow::anyhow!("AWS secret key not set"))?;

        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        let url_parsed = reqwest::Url::parse(url)?;
        let host = url_parsed.host_str().ok_or_else(|| anyhow::anyhow!("Invalid URL host"))?;
        let path = url_parsed.path();

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("host", host.parse()?);
        headers.insert("x-amz-date", amz_date.parse()?);
        if let Some(token) = &self.session_token {
            headers.insert("x-amz-security-token", token.parse()?);
        }
        headers.insert("content-type", "application/json".parse()?);

        let mut signed_headers_list = vec!["content-type", "host", "x-amz-date"];
        if self.session_token.is_some() {
            signed_headers_list.push("x-amz-security-token");
        }
        signed_headers_list.sort();
        let signed_headers = signed_headers_list.join(";");

        let mut canonical_headers = String::new();
        for h in &signed_headers_list {
            let val = headers.get(*h).unwrap().to_str()?;
            canonical_headers.push_str(h);
            canonical_headers.push(':');
            canonical_headers.push_str(val);
            canonical_headers.push('\n');
        }

        let payload_hash = hex::encode(Sha256::digest(payload));
        let canonical_request = format!(
            "{}\n{}\n\n{}\n{}\n{}",
            method, path, canonical_headers, signed_headers, payload_hash
        );

        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, self.region, "bedrock");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let k_date = hmac_sha256(format!("AWS4{}", secret_key).as_bytes(), date_stamp.as_bytes());
        let k_region = hmac_sha256(&k_date, self.region.as_bytes());
        let k_service = hmac_sha256(&k_region, b"bedrock");
        let k_signing = hmac_sha256(&k_service, b"aws4_request");
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes()));

        let auth_header = format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            access_key, credential_scope, signed_headers, signature
        );
        headers.insert("Authorization", auth_header.parse()?);

        Ok(headers)
    }
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[async_trait]
impl Provider for BedrockProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            native_tool_calling: false, // Starting simple
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
        resp.text.ok_or_else(|| anyhow::anyhow!("No text response from Bedrock"))
    }

    async fn chat(
        &self,
        request: ProviderChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ProviderChatResponse> {
        let (system, messages) = Self::convert_messages(request.messages);
        let bedrock_request = ConverseRequest {
            messages,
            system,
            inference_config: Some(InferenceConfig {
                max_tokens: self.max_tokens,
                temperature,
            }),
        };

        let model_id = model.replace(':', "%3A");
        let url = format!(
            "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
            self.region, model_id
        );

        let payload = serde_json::to_vec(&bedrock_request)?;
        let headers = self.sign_request("POST", &url, &payload)?;

        let response = self.client
            .post(url)
            .headers(headers)
            .body(payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Bedrock API error ({}): {}", status, body);
        }

        let bedrock_response: ConverseResponse = response.json().await?;
        let usage = bedrock_response.usage.map(|u| TokenUsage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            cached_input_tokens: None,
        });

        let mut text_parts = Vec::new();
        if let Some(output) = bedrock_response.output && let Some(message) = output.message {
            for block in message.content {
                if let ResponseContentBlock::Text(tb) = block {
                    text_parts.push(tb.text);
                }
            }
        }

        Ok(ProviderChatResponse {
            text: (!text_parts.is_empty()).then(|| text_parts.join("")),
            tool_calls: Vec::new(),
            usage,
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
        anyhow::bail!("Bedrock tool calling not yet implemented in light-rs")
    }
}
