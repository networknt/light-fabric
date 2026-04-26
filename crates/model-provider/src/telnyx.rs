use crate::compatible::CompatibleProvider;
use crate::traits::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use async_trait::async_trait;

pub struct TelnyxProvider {
    inner: CompatibleProvider,
}

impl TelnyxProvider {
    pub const BASE_URL: &'static str = "https://api.telnyx.com/v2/ai";

    pub fn new(api_key: Option<&str>) -> anyhow::Result<Self> {
        let key = api_key
            .map(|s| s.to_string())
            .or_else(|| std::env::var("TELNYX_API_KEY").ok())
            .or_else(|| std::env::var("ZEROCLAW_API_KEY").ok());

        let inner = CompatibleProvider::new("Telnyx", Self::BASE_URL, key.as_deref())?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl Provider for TelnyxProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.inner
            .chat_with_system(system_prompt, message, model, temperature)
            .await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        self.inner
            .chat_with_history(messages, model, temperature)
            .await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.inner.chat(request, model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        self.inner
            .chat_with_tools(messages, tools, model, temperature)
            .await
    }
}
