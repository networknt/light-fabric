use crate::traits::{ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{info, warn};

pub struct ReliableProvider {
    providers: Vec<(String, Box<dyn Provider>)>,
    max_retries: u32,
    base_backoff_ms: u64,
    api_keys: Vec<String>,
    key_index: AtomicUsize,
    model_fallbacks: HashMap<String, Vec<String>>,
}

impl ReliableProvider {
    pub fn new(
        providers: Vec<(String, Box<dyn Provider>)>,
        max_retries: u32,
        base_backoff_ms: u64,
    ) -> Self {
        Self {
            providers,
            max_retries,
            base_backoff_ms: base_backoff_ms.max(50),
            api_keys: Vec::new(),
            key_index: AtomicUsize::new(0),
            model_fallbacks: HashMap::new(),
        }
    }

    pub fn with_api_keys(mut self, keys: Vec<String>) -> Self {
        self.api_keys = keys;
        self
    }

    pub fn with_model_fallbacks(mut self, fallbacks: HashMap<String, Vec<String>>) -> Self {
        self.model_fallbacks = fallbacks;
        self
    }

    fn model_chain<'a>(&'a self, model: &'a str) -> Vec<&'a str> {
        let mut chain = vec![model];
        if let Some(fallbacks) = self.model_fallbacks.get(model) {
            chain.extend(fallbacks.iter().map(|s| s.as_str()));
        }
        chain
    }

    fn is_retryable(err: &anyhow::Error) -> bool {
        let msg = err.to_string().to_lowercase();
        // 429 and 5xx are retryable. 4xx (except 429/408) are not.
        if msg.contains("429")
            || msg.contains("500")
            || msg.contains("502")
            || msg.contains("503")
            || msg.contains("504")
            || msg.contains("timeout")
        {
            return true;
        }
        // Heuristic for transient network issues
        if msg.contains("connection") || msg.contains("network") || msg.contains("reset") {
            return true;
        }
        false
    }
}

#[async_trait]
impl Provider for ReliableProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.providers
            .first()
            .map(|(_, p)| p.capabilities())
            .unwrap_or_default()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();

        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                let mut backoff = Duration::from_millis(self.base_backoff_ms);
                for attempt in 0..=self.max_retries {
                    match provider
                        .chat_with_system(system_prompt, message, current_model, temperature)
                        .await
                    {
                        Ok(resp) => {
                            if attempt > 0 || *current_model != model {
                                info!(
                                    provider = provider_name,
                                    model = *current_model,
                                    attempt,
                                    "ReliableProvider recovered"
                                );
                            }
                            return Ok(resp);
                        }
                        Err(e) => {
                            let retryable = Self::is_retryable(&e);
                            let err_msg = e.to_string();
                            failures.push(format!(
                                "{provider_name}/{current_model} (attempt {attempt}): {err_msg}"
                            ));

                            if !retryable || attempt == self.max_retries {
                                warn!(provider = provider_name, model = *current_model, attempt, error = %err_msg, "Attempt failed, moving to next");
                                break;
                            }

                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(10));
                        }
                    }
                }
            }
        }
        anyhow::bail!("All providers failed:\n{}", failures.join("\n"))
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let models = self.model_chain(model);
        let mut failures = Vec::new();

        for current_model in &models {
            for (provider_name, provider) in &self.providers {
                let mut backoff = Duration::from_millis(self.base_backoff_ms);
                for attempt in 0..=self.max_retries {
                    match provider
                        .chat_with_history(messages, current_model, temperature)
                        .await
                    {
                        Ok(resp) => return Ok(resp),
                        Err(e) => {
                            if !Self::is_retryable(&e) || attempt == self.max_retries {
                                break;
                            }
                            failures.push(format!("{provider_name}/{current_model}: {e}"));
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(10));
                        }
                    }
                }
            }
        }
        anyhow::bail!("All providers failed")
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let models = self.model_chain(model);
        for current_model in &models {
            for (_, provider) in &self.providers {
                let mut backoff = Duration::from_millis(self.base_backoff_ms);
                for attempt in 0..=self.max_retries {
                    match provider.chat(request, current_model, temperature).await {
                        Ok(resp) => return Ok(resp),
                        Err(e) => {
                            if !Self::is_retryable(&e) || attempt == self.max_retries {
                                break;
                            }
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(10));
                        }
                    }
                }
            }
        }
        anyhow::bail!("All providers failed")
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let models = self.model_chain(model);
        for current_model in &models {
            for (_, provider) in &self.providers {
                let mut backoff = Duration::from_millis(self.base_backoff_ms);
                for attempt in 0..=self.max_retries {
                    match provider
                        .chat_with_tools(messages, tools, current_model, temperature)
                        .await
                    {
                        Ok(resp) => return Ok(resp),
                        Err(e) => {
                            if !Self::is_retryable(&e) || attempt == self.max_retries {
                                break;
                            }
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(10));
                        }
                    }
                }
            }
        }
        anyhow::bail!("All providers failed")
    }
}
