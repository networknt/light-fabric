use crate::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderCapabilities,
};
use async_trait::async_trait;
use std::collections::HashMap;
use tracing::{info, warn};

#[derive(Debug, Clone)]
pub struct Route {
    pub provider_name: String,
    pub model: String,
}

pub struct RouterProvider {
    routes: HashMap<String, (usize, String)>,
    providers: Vec<(String, Box<dyn Provider>)>,
    default_index: usize,
    default_model: String,
}

impl RouterProvider {
    pub fn new(
        providers: Vec<(String, Box<dyn Provider>)>,
        routes: Vec<(String, Route)>,
        default_model: String,
    ) -> Self {
        let name_to_index: HashMap<&str, usize> = providers
            .iter()
            .enumerate()
            .map(|(i, (name, _))| (name.as_str(), i))
            .collect();

        let resolved_routes: HashMap<String, (usize, String)> = routes
            .into_iter()
            .filter_map(|(hint, route)| {
                let index = name_to_index.get(route.provider_name.as_str()).copied();
                match index {
                    Some(i) => Some((hint, (i, route.model))),
                    None => {
                        warn!(hint = hint, provider = route.provider_name, "Route references unknown provider, skipping");
                        None
                    }
                }
            })
            .collect();

        Self {
            routes: resolved_routes,
            providers,
            default_index: 0,
            default_model,
        }
    }

    fn resolve(&self, model: &str) -> (usize, String) {
        if let Some(hint) = model.strip_prefix("hint:") {
            if let Some((idx, resolved_model)) = self.routes.get(hint) {
                return (*idx, resolved_model.clone());
            }
            warn!(hint = hint, "Unknown route hint, falling back to default provider");
        }
        (self.default_index, model.to_string())
    }
}

#[async_trait]
impl Provider for RouterProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        self.providers.get(self.default_index).map(|(_, p)| p.capabilities()).unwrap_or_default()
    }

    async fn chat_with_system(
        &self,
        system_prompt: Option<&str>,
        message: &str,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (idx, resolved_model) = self.resolve(model);
        let (name, provider) = &self.providers[idx];
        info!(provider = name, model = resolved_model, "Router dispatching request");
        provider.chat_with_system(system_prompt, message, &resolved_model, temperature).await
    }

    async fn chat_with_history(
        &self,
        messages: &[ChatMessage],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<String> {
        let (idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[idx];
        provider.chat_with_history(messages, &resolved_model, temperature).await
    }

    async fn chat(
        &self,
        request: ChatRequest<'_>,
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[idx];
        provider.chat(request, &resolved_model, temperature).await
    }

    async fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
        model: &str,
        temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let (idx, resolved_model) = self.resolve(model);
        let (_, provider) = &self.providers[idx];
        provider.chat_with_tools(messages, tools, &resolved_model, temperature).await
    }
}
