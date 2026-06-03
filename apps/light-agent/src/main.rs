use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use hindsight_client::{HindsightMemory, PgHindsightClient};
use light_axum::{AxumApp, AxumTransport, ServerContext};
use light_runtime::{
    LightRuntimeBuilder, MaskSpec, ModuleKind, RuntimeConfig, RuntimeError, TracingOptions,
    config::{BootstrapConfig, ClientConfig, PortalRegistryConfig},
    init_tracing,
};
use mcp_client::{McpContent, McpGatewayClient, McpTool};
use model_provider::{
    AnthropicProvider, AzureOpenAiProvider, BedrockProvider, ChatMessage, ChatRequest,
    ChatResponse, ClaudeCodeProvider, CodexProvider, CompatibleProvider, CopilotProvider,
    GeminiCliProvider, GeminiProvider, GlmProvider, KiloCliProvider, OllamaProvider,
    OpenAiProvider, OpenRouterProvider, Provider, TelnyxProvider, ToolSpec,
};
use portal_registry::RegistryHandler;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const MODEL_PROVIDER_FILE: &str = "model-provider.yml";
const MAX_SESSION_MESSAGES: usize = 40;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaConfig {
    pub ollama_url: String,
    pub model: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub reasoning_enabled: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClientConfig {
    pub gateway_url: String,
    pub path: String,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderConfig {
    #[serde(default = "default_model_provider")]
    pub provider: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_model_temperature")]
    pub temperature: f64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiModelProviderConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AzureOpenAiConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default)]
    pub resource_name: Option<String>,
    #[serde(default)]
    pub deployment_name: Option<String>,
    #[serde(default)]
    pub api_version: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibleConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopilotConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub github_token: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GlmConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliModelProviderConfig {
    #[serde(default)]
    pub model: Option<String>,
}

struct ModelProviderSelection {
    provider: Box<dyn Provider>,
    model: String,
    temperature: f64,
}

fn default_model_provider() -> String {
    "ollama".to_string()
}

fn default_model_temperature() -> f64 {
    0.7
}

fn required_uuid_env_var(name: &str) -> anyhow::Result<Uuid> {
    let raw = std::env::var(name)
        .with_context(|| format!("Required environment variable {name} is not set"))?;
    Uuid::parse_str(&raw)
        .with_context(|| format!("Environment variable {name} must be a valid UUID"))
}

fn optional_uuid_env_var(names: &[&str]) -> anyhow::Result<Option<Uuid>> {
    for name in names {
        if let Ok(raw) = std::env::var(name) {
            if raw.trim().is_empty() {
                continue;
            }
            return Uuid::parse_str(raw.trim())
                .with_context(|| format!("Environment variable {name} must be a valid UUID"))
                .map(Some);
        }
    }
    Ok(None)
}

fn to_portal_query_url(portal_url: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(portal_url)
        .with_context(|| format!("Invalid portal query URL: {portal_url}"))?;
    url.set_path("/portal/query");
    url.set_query(None);
    Ok(url.to_string())
}

fn to_portal_command_url(portal_url: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(portal_url)
        .with_context(|| format!("Invalid portal command URL: {portal_url}"))?;
    url.set_path("/portal/command");
    url.set_query(None);
    Ok(url.to_string())
}

fn registry_token(config: &PortalRegistryConfig) -> Option<String> {
    std::env::var("LIGHT_PORTAL_AUTHORIZATION")
        .ok()
        .or_else(|| std::env::var("light_portal_authorization").ok())
        .filter(|value| !value.trim().is_empty())
        .map(|value| strip_bearer_prefix(&value))
        .or_else(|| {
            (!config.portal_token.trim().is_empty())
                .then(|| strip_bearer_prefix(&config.portal_token))
        })
}

fn strip_bearer_prefix(token: &str) -> String {
    token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token)
        .to_string()
}

fn portal_query_base_url(config: &PortalRegistryConfig) -> String {
    std::env::var("LIGHT_AGENT_PORTAL_QUERY_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            config
                .portal_query_url
                .clone()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| config.portal_url.clone())
}

#[derive(Clone)]
struct AgentCatalogCache {
    inner: Arc<RwLock<Option<EffectiveAgentCatalog>>>,
}

impl AgentCatalogCache {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
        }
    }

    async fn get(&self) -> Option<EffectiveAgentCatalog> {
        self.inner.read().await.clone()
    }

    async fn set(&self, catalog: EffectiveAgentCatalog) {
        *self.inner.write().await = Some(catalog);
    }

    async fn clear(&self) {
        *self.inner.write().await = None;
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct EffectiveAgentCatalog {
    catalog_hash: Option<String>,
    catalog_version: Option<u64>,
    stale: bool,
    skills: Vec<CatalogSkill>,
}

impl Default for EffectiveAgentCatalog {
    fn default() -> Self {
        Self {
            catalog_hash: None,
            catalog_version: None,
            stale: false,
            skills: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct CatalogSkill {
    name: String,
    description: Option<String>,
    content_markdown: Option<String>,
    priority: Option<i32>,
    sequence_id: Option<i32>,
    tags: Vec<String>,
    categories: Vec<String>,
    tools: Vec<CatalogTool>,
    policy_diagnostics: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct CatalogTool {
    name: String,
    description: Option<String>,
    routing_domain: Option<String>,
    semantic_namespace: Option<String>,
    sensitivity_tier: Option<String>,
    semantic_weight: Option<f32>,
    source_protocol: Option<String>,
    target_personas: Option<String>,
    read_only: Option<bool>,
    destructive: Option<bool>,
    requires_approval: Option<bool>,
    policy: Option<CatalogToolPolicy>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct CatalogToolPolicy {
    allowed: Option<bool>,
    reason: Option<String>,
    sensitivity_tier: Option<String>,
    max_sensitivity_tier: Option<String>,
    read_only: Option<bool>,
    destructive: Option<bool>,
    requires_approval: Option<bool>,
    approval_configured: Option<bool>,
}

#[derive(Debug, Clone)]
struct CatalogSelection {
    tool_names: HashSet<String>,
    context: Option<String>,
}

#[derive(Clone)]
struct PortalQueryClient {
    url: String,
    token: String,
    client: reqwest::Client,
}

#[derive(Clone)]
struct PortalCommandClient {
    url: String,
    token: String,
    client: reqwest::Client,
}

impl PortalCommandClient {
    fn with_options(
        portal_url: &str,
        token: String,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        accept_invalid_certs: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .connect_timeout(std::time::Duration::from_millis(timeout_ms));

        if let Some(pem) = ca_cert_pem {
            let certificates = light_client::parse_ca_cert_bundle(pem).context(
                "Invalid ca_cert_pem: failed to parse PEM-encoded CA certificate bundle",
            )?;
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }

        if !verify_hostname {
            builder = builder.danger_accept_invalid_hostnames(true);
        }
        if accept_invalid_certs {
            warn!(
                "TLS certificate validation is disabled for the portal command client; this should only be enabled in development environments"
            );
            builder = builder.danger_accept_invalid_certs(true);
        }

        Ok(Self {
            url: to_portal_command_url(portal_url)?,
            token,
            client: builder
                .build()
                .context("Failed to build portal command client")?,
        })
    }

    async fn call(&self, action: &str, data: serde_json::Value) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "host": "lightapi.net",
            "service": "genai",
            "action": action,
            "version": "0.1.0",
            "data": data
        });
        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await
            .context("HTTP request to portal command failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("portal command {action} returned HTTP {status}: {body}");
        }

        let value: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse portal command response")?;
        if value.get("statusCode").is_some() || value.get("code").is_some() {
            bail!("portal command {action} returned an error response: {value}");
        }
        Ok(value)
    }
}

impl PortalQueryClient {
    fn with_options(
        portal_url: &str,
        token: String,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        accept_invalid_certs: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .connect_timeout(std::time::Duration::from_millis(timeout_ms));

        if let Some(pem) = ca_cert_pem {
            let certificates = light_client::parse_ca_cert_bundle(pem).context(
                "Invalid ca_cert_pem: failed to parse PEM-encoded CA certificate bundle",
            )?;
            let certificate_count = certificates.len();
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
            info!(
                ca_cert_count = certificate_count,
                "loaded portal query CA certificate bundle"
            );
        }

        if !verify_hostname {
            builder = builder.danger_accept_invalid_hostnames(true);
        }
        if accept_invalid_certs {
            warn!(
                "TLS certificate validation is disabled for the portal query client; this should only be enabled in development environments"
            );
            builder = builder.danger_accept_invalid_certs(true);
        }

        Ok(Self {
            url: to_portal_query_url(portal_url)?,
            token,
            client: builder
                .build()
                .context("Failed to build portal query client")?,
        })
    }

    async fn get_effective_agent_catalog(
        &self,
        host_id: Uuid,
        agent_def_id: Uuid,
        service_id: &str,
        env_tag: Option<&str>,
    ) -> Result<EffectiveAgentCatalog> {
        let mut data = serde_json::json!({
            "hostId": host_id,
            "agentDefId": agent_def_id,
            "serviceId": service_id,
        });
        if let Some(env_tag) = env_tag {
            data["envTag"] = serde_json::json!(env_tag);
        }
        let request = serde_json::json!({
            "host": "lightapi.net",
            "service": "genai",
            "action": "getEffectiveAgentCatalog",
            "version": "0.1.0",
            "data": data
        });

        let response = self
            .client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await
            .context("HTTP request to portal query failed")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("portal query returned HTTP {status}: {body}");
        }

        let value: serde_json::Value = response
            .json()
            .await
            .context("Failed to parse portal query response")?;
        if value.get("statusCode").is_some() || value.get("code").is_some() {
            bail!("portal query returned an error response: {value}");
        }
        serde_json::from_value(value).context("Failed to parse effective agent catalog")
    }
}

#[async_trait]
trait MemoryStore: Send + Sync {
    async fn ensure_session_memory_bank(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<()>;
    async fn load_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<ChatMessage>>;
    async fn persist_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
        history: &[ChatMessage],
    ) -> Result<()>;
    async fn retain(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        content: &str,
        fact_type: &str,
        metadata: serde_json::Value,
    ) -> Result<Uuid>;
    async fn recall(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        query_embedding: Vec<f32>,
        limit: i32,
    ) -> Result<Vec<hindsight_client::MemoryUnit>>;
}

struct DirectPgMemoryStore {
    pool: PgPool,
    hindsight: PgHindsightClient,
}

impl DirectPgMemoryStore {
    fn new(pool: PgPool) -> Self {
        Self {
            hindsight: PgHindsightClient::new(pool.clone()),
            pool,
        }
    }
}

struct PortalCommandMemoryStore {
    pool: PgPool,
    hindsight: PgHindsightClient,
    command_client: PortalCommandClient,
}

impl PortalCommandMemoryStore {
    fn new(pool: PgPool, command_client: PortalCommandClient) -> Self {
        Self {
            hindsight: PgHindsightClient::new(pool.clone()),
            pool,
            command_client,
        }
    }
}

#[async_trait]
impl MemoryStore for DirectPgMemoryStore {
    async fn ensure_session_memory_bank(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<()> {
        insert_session_memory_bank(&self.pool, host_id, bank_id, session_id).await
    }

    async fn load_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<ChatMessage>> {
        load_session_history_from_db(&self.pool, host_id, bank_id, session_id).await
    }

    async fn persist_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
        history: &[ChatMessage],
    ) -> Result<()> {
        persist_session_history_to_db(&self.pool, host_id, bank_id, session_id, history).await
    }

    async fn retain(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        content: &str,
        fact_type: &str,
        metadata: serde_json::Value,
    ) -> Result<Uuid> {
        self.hindsight
            .retain(host_id, bank_id, content, fact_type, None, metadata)
            .await
    }

    async fn recall(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        query_embedding: Vec<f32>,
        limit: i32,
    ) -> Result<Vec<hindsight_client::MemoryUnit>> {
        self.hindsight
            .recall(host_id, bank_id, query_embedding, limit)
            .await
    }
}

#[async_trait]
impl MemoryStore for PortalCommandMemoryStore {
    async fn ensure_session_memory_bank(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<()> {
        self.command_client
            .call(
                "createAgentMemoryBank",
                serde_json::json!({
                    "hostId": host_id,
                    "bankId": bank_id,
                    "bankName": format!("session-{session_id}")
                }),
            )
            .await?;

        if !session_history_exists(&self.pool, host_id, bank_id, session_id).await? {
            self.command_client
                .call(
                    "createAgentSessionHistory",
                    serde_json::json!({
                        "hostId": host_id,
                        "bankId": bank_id,
                        "sessionId": session_id,
                        "messages": []
                    }),
                )
                .await?;
        }
        Ok(())
    }

    async fn load_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
    ) -> Result<Vec<ChatMessage>> {
        load_session_history_from_db(&self.pool, host_id, bank_id, session_id).await
    }

    async fn persist_session_history(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        session_id: Uuid,
        history: &[ChatMessage],
    ) -> Result<()> {
        self.command_client
            .call(
                "compactAgentSessionHistory",
                serde_json::json!({
                    "hostId": host_id,
                    "bankId": bank_id,
                    "sessionId": session_id,
                    "messages": history
                }),
            )
            .await?;
        Ok(())
    }

    async fn retain(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        content: &str,
        fact_type: &str,
        metadata: serde_json::Value,
    ) -> Result<Uuid> {
        let value = self
            .command_client
            .call(
                "retainAgentMemoryUnit",
                serde_json::json!({
                    "hostId": host_id,
                    "bankId": bank_id,
                    "content": content,
                    "factType": fact_type,
                    "metadata": metadata
                }),
            )
            .await?;
        let unit_id = value
            .get("unitId")
            .and_then(|value| value.as_str())
            .context("retainAgentMemoryUnit response did not include unitId")?;
        Uuid::parse_str(unit_id).context("retainAgentMemoryUnit returned invalid unitId")
    }

    async fn recall(
        &self,
        host_id: Uuid,
        bank_id: Uuid,
        query_embedding: Vec<f32>,
        limit: i32,
    ) -> Result<Vec<hindsight_client::MemoryUnit>> {
        self.hindsight
            .recall(host_id, bank_id, query_embedding, limit)
            .await
    }
}

struct AgentState {
    provider: Box<dyn Provider>,
    model: String,
    temperature: f64,
    mcp_client: McpGatewayClient,
    portal_query_client: Option<PortalQueryClient>,
    catalog_cache: AgentCatalogCache,
    memory: Arc<dyn MemoryStore>,
    host_id: Uuid,
    agent_def_id: Option<Uuid>,
    service_id: String,
    env_tag: Option<String>,
}

impl AgentState {
    async fn catalog_selection(&self, prompt: &str) -> Option<CatalogSelection> {
        let catalog = self.effective_catalog().await?;
        Some(select_catalog_tools(&catalog, prompt, 12))
    }

    async fn effective_catalog(&self) -> Option<EffectiveAgentCatalog> {
        if let Some(catalog) = self.catalog_cache.get().await {
            return Some(catalog);
        }

        let Some(client) = self.portal_query_client.as_ref() else {
            return None;
        };
        let Some(agent_def_id) = self.agent_def_id else {
            warn!(
                "LIGHT_AGENT_AGENT_DEF_ID is not configured; using gateway tools/list without portal catalog filtering"
            );
            return None;
        };

        match client
            .get_effective_agent_catalog(
                self.host_id,
                agent_def_id,
                &self.service_id,
                self.env_tag.as_deref(),
            )
            .await
        {
            Ok(catalog) => {
                self.catalog_cache.set(catalog.clone()).await;
                Some(catalog)
            }
            Err(err) => {
                warn!(
                    "Effective agent catalog lookup failed; using gateway tools/list fallback: {err}"
                );
                None
            }
        }
    }
}

#[derive(Clone)]
struct AgentApp {
    catalog_cache: AgentCatalogCache,
}

#[async_trait::async_trait]
impl AxumApp for AgentApp {
    async fn router(&self, context: ServerContext) -> Result<Router, RuntimeError> {
        let state = build_agent_state(&context.runtime_config, self.catalog_cache.clone()).await?;
        Ok(agent_router(state))
    }
}

fn agent_router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/diagnostics/tools", get(tool_diagnostics))
        .route("/chat", get(ws_handler))
        .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDiagnosticsResponse {
    catalog_available: bool,
    catalog_hash: Option<String>,
    catalog_version: Option<u64>,
    catalog_stale: bool,
    catalog_tools: Vec<String>,
    gateway_available: bool,
    gateway_tools: Vec<String>,
    missing_from_gateway: Vec<String>,
    extra_gateway_tools: Vec<String>,
    policy_blocked: Vec<serde_json::Value>,
    gateway_error: Option<String>,
}

async fn tool_diagnostics(
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> Json<ToolDiagnosticsResponse> {
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let catalog = state.effective_catalog().await;
    let (catalog_tools, policy_blocked) = catalog
        .as_ref()
        .map(|catalog| {
            (
                collect_catalog_tool_names(catalog),
                collect_policy_diagnostics(catalog),
            )
        })
        .unwrap_or_default();

    let gateway_result = state.mcp_client.list_tools(authorization.as_deref()).await;
    let (gateway_available, gateway_tools, gateway_error) = match gateway_result {
        Ok(tools) => {
            let mut names = tools
                .into_iter()
                .map(|tool| tool.name)
                .filter(|name| !name.trim().is_empty())
                .collect::<Vec<_>>();
            names.sort();
            names.dedup();
            (true, names, None)
        }
        Err(err) => (false, Vec::new(), Some(err.to_string())),
    };

    let missing_from_gateway = if gateway_available {
        sorted_difference(&catalog_tools, &gateway_tools)
    } else {
        Vec::new()
    };
    let extra_gateway_tools = if catalog.is_some() && gateway_available {
        sorted_difference(&gateway_tools, &catalog_tools)
    } else {
        Vec::new()
    };

    Json(ToolDiagnosticsResponse {
        catalog_available: catalog.is_some(),
        catalog_hash: catalog
            .as_ref()
            .and_then(|catalog| catalog.catalog_hash.clone()),
        catalog_version: catalog.as_ref().and_then(|catalog| catalog.catalog_version),
        catalog_stale: catalog.as_ref().is_some_and(|catalog| catalog.stale),
        catalog_tools,
        gateway_available,
        gateway_tools,
        missing_from_gateway,
        extra_gateway_tools,
        policy_blocked,
        gateway_error,
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> impl IntoResponse {
    let session_id = params.get("sessionId").cloned();
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id, authorization))
}

#[derive(Debug, Deserialize)]
struct ClientMessage {
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "session")]
    Session { session_id: String },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "error")]
    Error { message: String },
}

fn trim_history(history: &mut Vec<ChatMessage>) {
    let excess = history.len().saturating_sub(MAX_SESSION_MESSAGES);
    if excess > 0 {
        history.drain(0..excess);
    }
}

fn rollback_last_user_message(history: &mut Vec<ChatMessage>, expected_text: &str) {
    if history
        .last()
        .is_some_and(|message| message.role == "user" && message.content == expected_text)
    {
        history.pop();
    }
}

fn select_catalog_tools(
    catalog: &EffectiveAgentCatalog,
    prompt: &str,
    limit: usize,
) -> CatalogSelection {
    let query_terms = tokenize(prompt);
    let mut scored_tools: Vec<(f32, usize, String)> = Vec::new();

    for (skill_index, skill) in catalog.skills.iter().enumerate() {
        let skill_text = searchable_skill_text(skill);
        let skill_score = keyword_score(&query_terms, &skill_text);
        for tool in &skill.tools {
            if tool.name.trim().is_empty() {
                continue;
            }
            if !catalog_tool_allowed(tool) {
                continue;
            }
            let tool_text = searchable_tool_text(tool);
            let routing_score = routing_score(&query_terms, tool);
            let priority = skill.priority.unwrap_or_default().max(0) as f32 / 10.0;
            let semantic_weight = tool.semantic_weight.unwrap_or(1.0).max(0.1);
            let score = ((skill_score * 0.75)
                + (keyword_score(&query_terms, &tool_text) * 1.5)
                + routing_score
                + priority)
                * semantic_weight;
            if score > 0.0 {
                scored_tools.push((
                    score,
                    skill.sequence_id.unwrap_or(skill_index as i32).max(0) as usize,
                    tool.name.clone(),
                ));
            }
        }
    }

    scored_tools.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });

    if scored_tools.is_empty() {
        for (skill_index, skill) in catalog.skills.iter().enumerate() {
            for tool in &skill.tools {
                if tool.name.trim().is_empty() {
                    continue;
                }
                if !catalog_tool_allowed(tool) {
                    continue;
                }
                scored_tools.push((
                    0.1,
                    skill.sequence_id.unwrap_or(skill_index as i32).max(0) as usize,
                    tool.name.clone(),
                ));
            }
            if scored_tools.len() >= limit {
                break;
            }
        }
    }

    let selected: Vec<_> = scored_tools.into_iter().take(limit).collect();
    let tool_names = selected
        .iter()
        .map(|(_, _, tool_name)| tool_name.clone())
        .collect::<HashSet<_>>();

    let context = if tool_names.is_empty() {
        None
    } else {
        let mut context = String::from("Relevant agent catalog skills and tools:\n");
        for skill in &catalog.skills {
            let selected_skill_tools = skill
                .tools
                .iter()
                .filter(|tool| tool_names.contains(&tool.name))
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>();
            if selected_skill_tools.is_empty() {
                continue;
            }
            let description = skill
                .description
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("no description");
            context.push_str(&format!("- {}: {}\n", skill.name, description));
            if let Some(instructions) = skill
                .content_markdown
                .as_deref()
                .filter(|value| !value.trim().is_empty())
            {
                context.push_str(&format!("  Instructions: {}\n", excerpt(instructions, 480)));
            }
            context.push_str(&format!("  Tools: {}\n", selected_skill_tools.join(", ")));
        }
        if let Some(hash) = &catalog.catalog_hash {
            context.push_str(&format!("Catalog hash: {hash}\n"));
        }
        if let Some(version) = catalog.catalog_version {
            context.push_str(&format!("Catalog version: {version}\n"));
        }
        if catalog.stale {
            context.push_str("Catalog status: stale\n");
        }
        Some(context)
    };

    CatalogSelection {
        tool_names,
        context,
    }
}

fn excerpt(value: &str, max_chars: usize) -> String {
    let value = value.trim().replace('\n', " ");
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut result = value.chars().take(max_chars).collect::<String>();
    result.push_str("...");
    result
}

fn catalog_tool_allowed(tool: &CatalogTool) -> bool {
    let policy = tool.policy.as_ref();
    if policy.and_then(|policy| policy.allowed) == Some(false) {
        return false;
    }

    let approval_configured = policy
        .and_then(|policy| policy.approval_configured)
        .unwrap_or(false);
    let destructive = tool
        .destructive
        .or_else(|| policy.and_then(|policy| policy.destructive))
        .unwrap_or(false);
    let requires_approval = tool
        .requires_approval
        .or_else(|| policy.and_then(|policy| policy.requires_approval))
        .unwrap_or(false);

    !(destructive || requires_approval) || approval_configured
}

fn collect_catalog_tool_names(catalog: &EffectiveAgentCatalog) -> Vec<String> {
    let mut names = catalog
        .skills
        .iter()
        .flat_map(|skill| skill.tools.iter())
        .filter(|tool| catalog_tool_allowed(tool))
        .map(|tool| tool.name.clone())
        .filter(|name| !name.trim().is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn collect_policy_diagnostics(catalog: &EffectiveAgentCatalog) -> Vec<serde_json::Value> {
    let mut diagnostics = Vec::new();
    for skill in &catalog.skills {
        diagnostics.extend(skill.policy_diagnostics.iter().cloned());
        for tool in &skill.tools {
            if catalog_tool_allowed(tool) {
                continue;
            }
            diagnostics.push(serde_json::json!({
                "skill": skill.name,
                "toolName": tool.name,
                "reason": tool
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.reason.clone())
                    .unwrap_or_else(|| "local_policy_guard".to_string()),
                "sensitivityTier": tool
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.sensitivity_tier.clone())
                    .or_else(|| tool.sensitivity_tier.clone()),
                "maxSensitivityTier": tool
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.max_sensitivity_tier.clone()),
                "readOnly": tool
                    .read_only
                    .or_else(|| tool.policy.as_ref().and_then(|policy| policy.read_only)),
                "destructive": tool
                    .destructive
                    .or_else(|| tool.policy.as_ref().and_then(|policy| policy.destructive)),
                "requiresApproval": tool
                    .requires_approval
                    .or_else(|| tool.policy.as_ref().and_then(|policy| policy.requires_approval)),
            }));
        }
    }
    diagnostics
}

fn sorted_difference(left: &[String], right: &[String]) -> Vec<String> {
    let right = right.iter().collect::<HashSet<_>>();
    let mut diff = left
        .iter()
        .filter(|item| !right.contains(item))
        .cloned()
        .collect::<Vec<_>>();
    diff.sort();
    diff
}

fn filter_gateway_tools(
    gateway_tools: Vec<McpTool>,
    selection: Option<&CatalogSelection>,
) -> Vec<McpTool> {
    let Some(selection) = selection else {
        return gateway_tools;
    };
    if selection.tool_names.is_empty() {
        return Vec::new();
    }

    let filtered = gateway_tools
        .iter()
        .filter(|tool| selection.tool_names.contains(&tool.name))
        .cloned()
        .collect::<Vec<_>>();

    if filtered.is_empty() {
        warn!(
            "Portal catalog selected tools that are not currently executable in gateway tools/list; hiding tools for this turn"
        );
        Vec::new()
    } else {
        filtered
    }
}

fn tokenize(value: &str) -> HashSet<String> {
    value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 2)
        .collect()
}

fn keyword_score(query_terms: &HashSet<String>, text: &str) -> f32 {
    if query_terms.is_empty() || text.is_empty() {
        return 0.0;
    }
    let text = text.to_ascii_lowercase();
    query_terms
        .iter()
        .filter(|term| text.contains(term.as_str()))
        .count() as f32
}

fn routing_score(query_terms: &HashSet<String>, tool: &CatalogTool) -> f32 {
    [
        tool.routing_domain.as_deref(),
        tool.semantic_namespace.as_deref(),
        tool.sensitivity_tier.as_deref(),
        tool.source_protocol.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(|value| keyword_score(query_terms, value))
    .sum::<f32>()
        * 2.0
}

fn searchable_skill_text(skill: &CatalogSkill) -> String {
    let mut text = String::new();
    append_search_text(&mut text, &skill.name);
    append_search_text(&mut text, skill.description.as_deref().unwrap_or_default());
    append_search_text(
        &mut text,
        skill.content_markdown.as_deref().unwrap_or_default(),
    );
    append_search_text(&mut text, &skill.tags.join(" "));
    append_search_text(&mut text, &skill.categories.join(" "));
    text
}

fn searchable_tool_text(tool: &CatalogTool) -> String {
    let mut text = String::new();
    append_search_text(&mut text, &tool.name);
    append_search_text(&mut text, tool.description.as_deref().unwrap_or_default());
    append_search_text(
        &mut text,
        tool.routing_domain.as_deref().unwrap_or_default(),
    );
    append_search_text(
        &mut text,
        tool.semantic_namespace.as_deref().unwrap_or_default(),
    );
    append_search_text(
        &mut text,
        tool.sensitivity_tier.as_deref().unwrap_or_default(),
    );
    append_search_text(
        &mut text,
        tool.source_protocol.as_deref().unwrap_or_default(),
    );
    append_search_text(
        &mut text,
        tool.target_personas.as_deref().unwrap_or_default(),
    );
    text
}

fn append_search_text(target: &mut String, value: &str) {
    if !value.trim().is_empty() {
        if !target.is_empty() {
            target.push(' ');
        }
        target.push_str(value);
    }
}

fn build_model_provider(
    runtime_config: &RuntimeConfig,
    config: &ModelProviderConfig,
) -> Result<ModelProviderSelection, RuntimeError> {
    let provider_id = normalize_provider_id(&config.provider);
    let temperature = config.temperature;

    let selection = match provider_id.as_str() {
        "ollama" => {
            let provider_config: OllamaConfig = load_agent_registered_config(
                runtime_config,
                "ollama.yml",
                "light-agent/ollama",
                "ollama",
                provider_secret_masks(),
            )?;
            let model = choose_model(config, Some(provider_config.model.as_str()), None, "ollama")?;
            let provider = OllamaProvider::new_with_reasoning(
                Some(&provider_config.ollama_url),
                optional_str(&provider_config.api_key),
                optional_bool(
                    &provider_config.reasoning_enabled,
                    "ollama.reasoningEnabled",
                )?,
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build Ollama provider: {e}")))?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "openai" | "open-ai" => {
            let provider_config: ApiModelProviderConfig =
                load_provider_config(runtime_config, "openai.yml", "openai")?;
            let model = choose_model(config, optional_str(&provider_config.model), None, "openai")?;
            let mut provider = OpenAiProvider::new(
                optional_str(&provider_config.base_url),
                optional_str(&provider_config.api_key),
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build OpenAI provider: {e}")))?;
            if let Some(max_tokens) = optional_u32(&provider_config.max_tokens, "openai.maxTokens")?
            {
                provider = provider.with_max_tokens(Some(max_tokens));
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "azure-openai" | "azure" | "azure-open-ai" => {
            let provider_config: AzureOpenAiConfig =
                load_provider_config(runtime_config, "azure-openai.yml", "azure-openai")?;
            let resource_name = required_config_value(
                optional_str(&provider_config.resource_name),
                "azure-openai.resourceName",
            )?;
            let deployment_name = required_config_value(
                optional_str(&provider_config.deployment_name),
                "azure-openai.deploymentName",
            )?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                Some(deployment_name),
                "azure-openai",
            )?;
            let provider = AzureOpenAiProvider::new(
                optional_str(&provider_config.credential),
                resource_name,
                deployment_name,
                optional_str(&provider_config.api_version),
            )
            .map_err(|e| {
                RuntimeError::Config(format!("failed to build Azure OpenAI provider: {e}"))
            })?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "anthropic" | "claude" => {
            let provider_config: ApiModelProviderConfig =
                load_provider_config(runtime_config, "anthropic.yml", "anthropic")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                None,
                "anthropic",
            )?;
            let mut provider = AnthropicProvider::new(
                optional_str(&provider_config.base_url),
                optional_str(&provider_config.api_key),
            )
            .map_err(|e| {
                RuntimeError::Config(format!("failed to build Anthropic provider: {e}"))
            })?;
            if let Some(max_tokens) =
                optional_u32(&provider_config.max_tokens, "anthropic.maxTokens")?
            {
                provider = provider.with_max_tokens(max_tokens);
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "bedrock" | "aws-bedrock" => {
            let provider_config: BedrockConfig =
                load_provider_config(runtime_config, "bedrock.yml", "bedrock")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                None,
                "bedrock",
            )?;
            let mut provider = BedrockProvider::new(
                optional_str(&provider_config.region),
                optional_str(&provider_config.access_key_id),
                optional_str(&provider_config.secret_access_key),
                optional_str(&provider_config.session_token),
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build Bedrock provider: {e}")))?;
            if let Some(max_tokens) =
                optional_u32(&provider_config.max_tokens, "bedrock.maxTokens")?
            {
                provider = provider.with_max_tokens(max_tokens);
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "codex" => {
            let provider_config: CodexConfig =
                load_provider_config(runtime_config, "codex.yml", "codex")?;
            let model = choose_model(config, optional_str(&provider_config.model), None, "codex")?;
            let provider = CodexProvider::new(
                optional_str(&provider_config.base_url),
                optional_str(&provider_config.api_key),
                optional_str(&provider_config.account_id),
                optional_str(&provider_config.reasoning_effort),
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build Codex provider: {e}")))?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "compatible" | "openai-compatible" | "open-ai-compatible" => {
            let provider_config: CompatibleConfig =
                load_provider_config(runtime_config, "compatible.yml", "compatible")?;
            let base_url = required_config_value(
                optional_str(&provider_config.base_url),
                "compatible.baseUrl",
            )?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                None,
                "compatible",
            )?;
            let name = optional_str(&provider_config.name).unwrap_or("compatible");
            let mut provider =
                CompatibleProvider::new(name, base_url, optional_str(&provider_config.api_key))
                    .map_err(|e| {
                        RuntimeError::Config(format!(
                            "failed to build OpenAI-compatible provider: {e}"
                        ))
                    })?;
            if let Some(max_tokens) =
                optional_u32(&provider_config.max_tokens, "compatible.maxTokens")?
            {
                provider = provider.with_max_tokens(Some(max_tokens));
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "gemini" | "google-gemini" => {
            let provider_config: ApiModelProviderConfig =
                load_provider_config(runtime_config, "gemini.yml", "gemini")?;
            let model = choose_model(config, optional_str(&provider_config.model), None, "gemini")?;
            let mut provider = GeminiProvider::new(
                optional_str(&provider_config.base_url),
                optional_str(&provider_config.api_key),
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build Gemini provider: {e}")))?;
            if let Some(max_tokens) = optional_u32(&provider_config.max_tokens, "gemini.maxTokens")?
            {
                provider = provider.with_max_tokens(max_tokens);
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "glm" | "zhipu" | "bigmodel" => {
            let provider_config: GlmConfig =
                load_provider_config(runtime_config, "glm.yml", "glm")?;
            let model = choose_model(config, optional_str(&provider_config.model), None, "glm")?;
            let provider = GlmProvider::new(
                optional_str(&provider_config.api_key),
                optional_str(&provider_config.base_url),
            )
            .map_err(|e| RuntimeError::Config(format!("failed to build GLM provider: {e}")))?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "openrouter" | "open-router" => {
            let provider_config: ApiModelProviderConfig =
                load_provider_config(runtime_config, "openrouter.yml", "openrouter")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                None,
                "openrouter",
            )?;
            let mut provider = OpenRouterProvider::new(
                optional_str(&provider_config.base_url),
                optional_str(&provider_config.api_key),
            )
            .map_err(|e| {
                RuntimeError::Config(format!("failed to build OpenRouter provider: {e}"))
            })?;
            if let Some(max_tokens) =
                optional_u32(&provider_config.max_tokens, "openrouter.maxTokens")?
            {
                provider = provider.with_max_tokens(Some(max_tokens));
            }
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "telnyx" => {
            let provider_config: ApiModelProviderConfig =
                load_provider_config(runtime_config, "telnyx.yml", "telnyx")?;
            let model = choose_model(config, optional_str(&provider_config.model), None, "telnyx")?;
            let provider =
                TelnyxProvider::new(optional_str(&provider_config.api_key)).map_err(|e| {
                    RuntimeError::Config(format!("failed to build Telnyx provider: {e}"))
                })?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "copilot" | "github-copilot" => {
            let provider_config: CopilotConfig =
                load_provider_config(runtime_config, "copilot.yml", "copilot")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                Some("gpt-4o"),
                "copilot",
            )?;
            let provider = CopilotProvider::new(optional_str(&provider_config.github_token))
                .map_err(|e| {
                    RuntimeError::Config(format!("failed to build Copilot provider: {e}"))
                })?;
            ModelProviderSelection {
                provider: Box::new(provider),
                model,
                temperature,
            }
        }
        "claude-code" | "claudecode" => {
            let provider_config: CliModelProviderConfig =
                load_provider_config(runtime_config, "claude-code.yml", "claude-code")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                Some("default"),
                "claude-code",
            )?;
            ModelProviderSelection {
                provider: Box::new(ClaudeCodeProvider::new()),
                model,
                temperature,
            }
        }
        "gemini-cli" | "geminicli" => {
            let provider_config: CliModelProviderConfig =
                load_provider_config(runtime_config, "gemini-cli.yml", "gemini-cli")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                Some("default"),
                "gemini-cli",
            )?;
            ModelProviderSelection {
                provider: Box::new(GeminiCliProvider::new()),
                model,
                temperature,
            }
        }
        "kilo-cli" | "kilocli" | "kilo" => {
            let provider_config: CliModelProviderConfig =
                load_provider_config(runtime_config, "kilo-cli.yml", "kilo-cli")?;
            let model = choose_model(
                config,
                optional_str(&provider_config.model),
                Some("default"),
                "kilo-cli",
            )?;
            ModelProviderSelection {
                provider: Box::new(KiloCliProvider::new()),
                model,
                temperature,
            }
        }
        other => {
            return Err(RuntimeError::Unsupported(format!(
                "unsupported model provider `{other}`"
            )));
        }
    };

    Ok(selection)
}

fn load_provider_config<T>(
    runtime_config: &RuntimeConfig,
    file_name: &str,
    config_name: &str,
) -> Result<T, RuntimeError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    load_agent_registered_config(
        runtime_config,
        file_name,
        format!("light-agent/{config_name}"),
        config_name,
        provider_secret_masks(),
    )
}

fn load_agent_registered_config<T>(
    runtime_config: &RuntimeConfig,
    file_name: &str,
    module_id: impl Into<String>,
    config_name: impl Into<String>,
    masks: impl IntoIterator<Item = MaskSpec>,
) -> Result<T, RuntimeError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    runtime_config.module_registry.load_registered(
        runtime_config,
        file_name,
        module_id,
        config_name,
        ModuleKind::Application,
        masks,
        Some(true),
        false,
    )
}

fn provider_secret_masks() -> Vec<MaskSpec> {
    vec![
        MaskSpec::key("accessKeyId"),
        MaskSpec::key("accountId"),
        MaskSpec::key("apiKey"),
        MaskSpec::key("credential"),
        MaskSpec::key("githubToken"),
        MaskSpec::key("secretAccessKey"),
        MaskSpec::key("sessionToken"),
    ]
}

fn choose_model(
    model_provider_config: &ModelProviderConfig,
    provider_model: Option<&str>,
    default_model: Option<&str>,
    provider_name: &str,
) -> Result<String, RuntimeError> {
    optional_str(&model_provider_config.model)
        .or(provider_model)
        .or(default_model)
        .map(ToString::to_string)
        .ok_or_else(|| {
            RuntimeError::Config(format!(
                "model-provider.model or {provider_name}.model is required"
            ))
        })
}

fn normalize_provider_id(provider: &str) -> String {
    provider
        .trim()
        .to_ascii_lowercase()
        .replace(['_', ' '], "-")
}

fn optional_str(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn required_config_value<'a>(value: Option<&'a str>, key: &str) -> Result<&'a str, RuntimeError> {
    value.ok_or_else(|| RuntimeError::Config(format!("{key} is required")))
}

fn optional_u32(value: &Option<String>, key: &str) -> Result<Option<u32>, RuntimeError> {
    let Some(value) = optional_str(value) else {
        return Ok(None);
    };
    value
        .parse::<u32>()
        .map(Some)
        .map_err(|e| RuntimeError::Config(format!("{key} must be an unsigned integer: {e}")))
}

fn optional_bool(value: &Option<String>, key: &str) -> Result<Option<bool>, RuntimeError> {
    let Some(value) = optional_str(value) else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(Some(true)),
        "false" | "0" | "no" | "off" => Ok(Some(false)),
        _ => Err(RuntimeError::Config(format!("{key} must be true or false"))),
    }
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AgentState>,
    initial_session_id: Option<String>,
    authorization: Option<String>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Immediate Session Initialization
    let session_id = initial_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::Session {
                session_id: session_id.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await;

    let current_session_id: String = session_id;

    // 1. Load or Initialize Session
    let session_uuid = Uuid::parse_str(&current_session_id).unwrap_or_else(|_| Uuid::new_v4());
    let bank_id = session_uuid; // Using session as bank for simplicity
    if let Err(e) = state
        .memory
        .ensure_session_memory_bank(state.host_id, bank_id, session_uuid)
        .await
    {
        error!("Failed to initialize session memory bank: {}", e);
        match serde_json::to_string(&ServerMessage::Error {
            message: "Failed to initialize session memory".to_string(),
        }) {
            Ok(payload) => {
                let _ = sender.send(Message::Text(payload.into())).await;
            }
            Err(serialize_err) => {
                error!(
                    "Failed to serialize session initialization error: {}",
                    serialize_err
                );
            }
        }
        return;
    }

    let mut history = state
        .memory
        .load_session_history(state.host_id, bank_id, session_uuid)
        .await
        .unwrap_or_else(|e| {
            warn!(
                "Failed to load session history for host_id={}, bank_id={}, session_id={}: {}",
                state.host_id, bank_id, session_uuid, e
            );
            Vec::new()
        });

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let client_msg: ClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    match serde_json::to_string(&ServerMessage::Error {
                        message: format!("Invalid message format: {}", e),
                    }) {
                        Ok(payload) => {
                            let _ = sender.send(Message::Text(payload.into())).await;
                        }
                        Err(serialize_err) => {
                            error!(
                                "Failed to serialize server error message: {}",
                                serialize_err
                            );
                        }
                    }
                    continue;
                }
            };

            let user_text = client_msg.text.clone();
            history.push(ChatMessage::user(user_text.clone()));
            trim_history(&mut history);

            match run_agent_loop(
                &state,
                history.clone(),
                authorization.as_deref(),
                &current_session_id,
                bank_id,
            )
            .await
            {
                Ok(response) => {
                    if let Some(text) = response.text {
                        history.push(ChatMessage::assistant(text.clone()));
                        trim_history(&mut history);

                        if let Err(e) = state
                            .memory
                            .persist_session_history(state.host_id, bank_id, session_uuid, &history)
                            .await
                        {
                            warn!("Failed to persist session history: {}", e);
                        }

                        match serde_json::to_string(&ServerMessage::Text { text }) {
                            Ok(payload) => {
                                let _ = sender.send(Message::Text(payload.into())).await;
                            }
                            Err(e) => {
                                error!("Failed to serialize server text message: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Agent loop error: {}", e);
                    rollback_last_user_message(&mut history, &user_text);
                    match serde_json::to_string(&ServerMessage::Error {
                        message: format!("Error: {}", e),
                    }) {
                        Ok(payload) => {
                            let _ = sender.send(Message::Text(payload.into())).await;
                        }
                        Err(serialize_err) => {
                            error!(
                                "Failed to serialize server error message: {}",
                                serialize_err
                            );
                        }
                    }
                }
            }
        }
    }
}

async fn insert_session_memory_bank(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
    session_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_memory_bank_t (host_id, bank_id, bank_name)
         VALUES ($1, $2, $3)
         ON CONFLICT (host_id, bank_id) DO NOTHING",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(format!("session-{session_id}"))
    .execute(db)
    .await
    .context("failed to create session memory bank")?;

    Ok(())
}

async fn session_history_exists(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
    session_id: Uuid,
) -> Result<bool> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM agent_session_history_t
            WHERE host_id = $1 AND bank_id = $2 AND session_id = $3
        )",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(session_id)
    .fetch_one(db)
    .await
    .context("failed to check session history existence")?;
    Ok(exists)
}

async fn load_session_history_from_db(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
    session_id: Uuid,
) -> Result<Vec<ChatMessage>> {
    let row = sqlx::query(
        "SELECT messages FROM agent_session_history_t
         WHERE host_id = $1 AND bank_id = $2 AND session_id = $3",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(session_id)
    .fetch_optional(db)
    .await
    .context("failed to load session history")?;

    let Some(row) = row else {
        return Ok(Vec::new());
    };
    let messages: serde_json::Value = row.get("messages");
    Ok(serde_json::from_value::<Vec<ChatMessage>>(messages).unwrap_or_default())
}

async fn persist_session_history_to_db(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
    session_id: Uuid,
    history: &[ChatMessage],
) -> Result<()> {
    let history_payload =
        serde_json::to_value(history).context("failed to serialize session history")?;
    sqlx::query(
        "INSERT INTO agent_session_history_t
         (host_id, bank_id, session_id, messages)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (host_id, bank_id, session_id)
         DO UPDATE SET messages = EXCLUDED.messages,
                       update_ts = CURRENT_TIMESTAMP",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(session_id)
    .bind(history_payload)
    .execute(db)
    .await
    .context("failed to persist session history")?;
    Ok(())
}

async fn run_agent_loop(
    state: &AgentState,
    mut messages: Vec<ChatMessage>,
    authorization: Option<&str>,
    session_id: &str,
    bank_id: Uuid,
) -> Result<ChatResponse> {
    let user_prompt = messages
        .last()
        .map(|m| m.content.clone())
        .unwrap_or_default();

    // 1. Recall Memory (Context Injection)
    // For now, we use a zero-vector since we don't have an embedding service yet.
    // In production, user_prompt would be embedded first.
    let relevant_memories = state
        .memory
        .recall(state.host_id, bank_id, vec![0.0; 384], 5)
        .await?;
    if !relevant_memories.is_empty() {
        let mut context_msg = String::from("Relevant context from your memory:\n");
        for mem in relevant_memories {
            context_msg.push_str(&format!("- {}\n", mem.content));
        }
        // Inject as a system hint or prefix to the user message
        if let Some(msg) = messages.last_mut() {
            msg.content = format!("{}\n\n{}", context_msg, msg.content);
        }
    }

    // 2. Discover executable tools from the gateway. The portal catalog only
    // narrows what we expose to the model; gateway remains the execution path.
    let catalog_selection = state.catalog_selection(&user_prompt).await;
    if let Some(context) = catalog_selection
        .as_ref()
        .and_then(|selection| selection.context.as_ref())
    {
        if let Some(msg) = messages.last_mut() {
            msg.content = format!("{}\n\n{}", context, msg.content);
        }
    }

    let mut tool_specs: Vec<ToolSpec> = Vec::new();
    let mcp_tools = state
        .mcp_client
        .list_tools(authorization)
        .await
        .unwrap_or_else(|e| {
            warn!("Gateway tools/list failed: {}", e);
            Vec::new()
        });
    for t in filter_gateway_tools(mcp_tools, catalog_selection.as_ref()) {
        tool_specs.push(ToolSpec {
            name: t.name,
            description: t.description,
            parameters: t.input_schema,
        });
    }

    // 3. Main LLM Loop
    let mut final_response = None;
    for _ in 0..10 {
        let response = {
            let request = ChatRequest {
                messages: &messages,
                tools: if tool_specs.is_empty() {
                    None
                } else {
                    Some(&tool_specs)
                },
            };
            state
                .provider
                .chat(request, &state.model, state.temperature)
                .await?
        };

        if response.tool_calls.is_empty() {
            final_response = Some(response);
            break;
        }

        // Add assistant message with tool calls
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: serde_json::to_string(
                &serde_json::json!({ "tool_calls": response.tool_calls }),
            )
            .unwrap(),
        });

        for tool_call in &response.tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tool_call.arguments).unwrap_or_default();
            match state
                .mcp_client
                .call_tool(authorization, &tool_call.name, args)
                .await
            {
                Ok(result) => {
                    let mut text_result = String::new();
                    for content in result.content {
                        if let McpContent::Text { text } = content {
                            text_result.push_str(&text);
                        }
                    }
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: serde_json::to_string(&serde_json::json!({
                            "tool_call_id": tool_call.id,
                            "tool_name": tool_call.name,
                            "content": text_result
                        }))
                        .unwrap(),
                    });
                }
                Err(e) => {
                    warn!("Tool call failed: {}", e);
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: serde_json::to_string(&serde_json::json!({
                            "tool_call_id": tool_call.id,
                            "tool_name": tool_call.name,
                            "content": format!("Error: {}", e)
                        }))
                        .unwrap(),
                    });
                }
            }
        }
    }

    let response = final_response.ok_or_else(|| anyhow!("Max iterations reached"))?;

    // 4. Retain Experience (Learning)
    if let Some(ref text) = response.text {
        let trajectory = format!("User: {}\nAssistant: {}", user_prompt, text);
        let _ = state
            .memory
            .retain(
                state.host_id,
                bank_id,
                &trajectory,
                "experience",
                serde_json::json!({ "session_id": session_id }),
            )
            .await
            .map_err(|e| warn!("Failed to retain memory: {}", e));
    }

    Ok(response)
}

async fn build_agent_state(
    runtime_config: &RuntimeConfig,
    catalog_cache: AgentCatalogCache,
) -> Result<Arc<AgentState>, RuntimeError> {
    let model_provider_config: ModelProviderConfig = load_agent_registered_config(
        runtime_config,
        MODEL_PROVIDER_FILE,
        "light-agent/model-provider",
        "model-provider",
        [],
    )?;
    let model_provider = build_model_provider(runtime_config, &model_provider_config)?;

    let mcp_config: McpClientConfig = runtime_config.module_registry.load_registered(
        runtime_config,
        "mcp-client.yml",
        "light-agent/mcp-client",
        "mcp-client",
        ModuleKind::Application,
        [],
        Some(true),
        false,
    )?;

    let portal_registry_config = runtime_config
        .portal_registry
        .clone()
        .ok_or_else(|| RuntimeError::MissingConfig("portal-registry.yml".to_string()))?;

    let mcp_gateway_url = format!(
        "{}/{}",
        mcp_config.gateway_url.trim_end_matches('/'),
        mcp_config.path.trim_start_matches('/')
    );

    let ca_cert = read_agent_ca_cert_bundle(runtime_config)?;
    let verify_hostname: bool = runtime_config
        .client
        .as_ref()
        .map(|c| c.tls.verify_hostname)
        .unwrap_or(true);
    let accept_invalid_certs: bool = runtime_config
        .client
        .as_ref()
        .map(|c| c.tls.accept_invalid_certs)
        .unwrap_or(false);
    if !verify_hostname {
        warn!(
            "TLS hostname verification is disabled for the MCP gateway and portal query clients; this weakens server identity validation"
        );
    }

    let mcp_client = McpGatewayClient::with_tls_options(
        &mcp_gateway_url,
        ca_cert.as_deref(),
        verify_hostname,
        accept_invalid_certs,
        mcp_config.timeout_ms,
    )
    .map_err(|e| RuntimeError::Config(format!("failed to build MCP gateway client: {e}")))?;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:secret@localhost:5432/configserver".to_string());
    let pool = PgPool::connect(&db_url)
        .await
        .map_err(|e| RuntimeError::Config(format!("failed to connect to database: {e}")))?;

    let host_id = required_uuid_env_var("LIGHT_AGENT_HOST_ID")
        .map_err(|e| RuntimeError::Config(e.to_string()))?;
    let agent_def_id =
        optional_uuid_env_var(&["LIGHT_AGENT_AGENT_DEF_ID", "LIGHT_AGENT_API_VERSION_ID"])
            .map_err(|e| RuntimeError::Config(e.to_string()))?;
    let env_tag = runtime_config.service_identity.env_tag.clone();
    let portal_token =
        registry_token(&portal_registry_config).ok_or(RuntimeError::MissingPortalToken)?;
    let memory_write_mode = std::env::var("LIGHT_AGENT_MEMORY_WRITE_MODE")
        .unwrap_or_else(|_| "direct-pg".to_string())
        .to_ascii_lowercase();
    let memory: Arc<dyn MemoryStore> = match memory_write_mode.as_str() {
        "portal-command" => {
            let command_url = std::env::var("LIGHT_AGENT_PORTAL_COMMAND_URL")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| portal_query_base_url(&portal_registry_config));
            let command_client = PortalCommandClient::with_options(
                &command_url,
                portal_token.clone(),
                ca_cert.as_deref(),
                verify_hostname,
                accept_invalid_certs,
                mcp_config.timeout_ms,
            )
            .map_err(|e| {
                RuntimeError::Config(format!("failed to build portal command client: {e}"))
            })?;
            Arc::new(PortalCommandMemoryStore::new(pool.clone(), command_client))
        }
        "direct-pg" => Arc::new(DirectPgMemoryStore::new(pool.clone())),
        other => {
            return Err(RuntimeError::Config(format!(
                "LIGHT_AGENT_MEMORY_WRITE_MODE must be portal-command or direct-pg, got {other}"
            )));
        }
    };
    let portal_query_client = Some(
        PortalQueryClient::with_options(
            &portal_query_base_url(&portal_registry_config),
            portal_token.clone(),
            ca_cert.as_deref(),
            verify_hostname,
            accept_invalid_certs,
            mcp_config.timeout_ms,
        )
        .map_err(|e| RuntimeError::Config(format!("failed to build portal query client: {e}")))?,
    );

    Ok(Arc::new(AgentState {
        provider: model_provider.provider,
        model: model_provider.model,
        temperature: model_provider.temperature,
        mcp_client,
        portal_query_client,
        catalog_cache,
        memory,
        host_id,
        agent_def_id,
        service_id: runtime_config.service_identity.service_id.clone(),
        env_tag,
    }))
}

fn read_agent_ca_cert_bundle(
    runtime_config: &RuntimeConfig,
) -> Result<Option<Vec<u8>>, RuntimeError> {
    let Some(path) = agent_ca_cert_path(runtime_config) else {
        return Ok(None);
    };
    let bundle = std::fs::read(&path)?;
    info!(
        ca_cert_path = %path.display(),
        ca_cert_configured = true,
        "loaded light-agent outbound CA certificate bundle"
    );
    Ok(Some(bundle))
}

fn agent_ca_cert_path(runtime_config: &RuntimeConfig) -> Option<PathBuf> {
    agent_ca_cert_path_from_config(&runtime_config.bootstrap, runtime_config.client.as_ref())
}

fn agent_ca_cert_path_from_config(
    bootstrap: &BootstrapConfig,
    client_config: Option<&ClientConfig>,
) -> Option<PathBuf> {
    client_config
        .and_then(|client| client.tls.ca_cert_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            bootstrap
                .bootstrap_ca_cert_path
                .clone()
                .filter(|path| !path.as_os_str().is_empty())
        })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tracing_guard =
        init_tracing(TracingOptions::new("light-agent").with_legacy_ansi_env("AGENT_LOG_ANSI"))?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }

    let catalog_cache = AgentCatalogCache::new();
    let registry_handler: Arc<dyn RegistryHandler> =
        Arc::new(AgentRegistryHandler::new(catalog_cache.clone()));
    let app = AgentApp { catalog_cache };

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir(DEFAULT_CONFIG_DIR)
        .with_config_dir(CONFIG_DIR)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
        .with_registry_handler(registry_handler)
        .with_logging_control(tracing_guard.logging_control())
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start agent runtime")?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    running
        .shutdown()
        .await
        .context("failed to shut down agent")?;

    Ok(())
}

struct AgentRegistryHandler {
    catalog_cache: AgentCatalogCache,
}

impl AgentRegistryHandler {
    fn new(catalog_cache: AgentCatalogCache) -> Self {
        Self { catalog_cache }
    }

    fn is_catalog_invalidation(method: &str, params: &serde_json::Value) -> bool {
        let method = method.to_ascii_lowercase();
        if method.contains("catalog") || method.contains("cache") {
            return true;
        }
        let params = params.to_string().to_ascii_lowercase();
        params.contains("effective-agent-catalog")
            || params.contains("agent-skill")
            || params.contains("skill-tool")
            || params.contains("tool")
            || params.contains("workflow")
    }
}

#[async_trait::async_trait]
impl RegistryHandler for AgentRegistryHandler {
    async fn handle_notification(&self, method: &str, params: serde_json::Value) {
        if Self::is_catalog_invalidation(method, &params) {
            self.catalog_cache.clear().await;
        }
    }

    async fn handle_request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        if Self::is_catalog_invalidation(method, &params) {
            self.catalog_cache.clear().await;
            serde_json::json!({"status": "cleared", "cache": "effective-agent-catalog"})
        } else {
            serde_json::json!({"status": "received"})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CatalogSkill, CatalogTool, CatalogToolPolicy, ChatMessage, EffectiveAgentCatalog,
        MAX_SESSION_MESSAGES, ModelProviderConfig, agent_ca_cert_path_from_config, choose_model,
        collect_catalog_tool_names, collect_policy_diagnostics, filter_gateway_tools,
        normalize_provider_id, rollback_last_user_message, select_catalog_tools, trim_history,
    };
    use light_runtime::config::{BootstrapConfig, ClientConfig};
    use mcp_client::McpTool;
    use std::path::PathBuf;

    #[test]
    fn trim_history_keeps_recent_messages() {
        let mut history: Vec<ChatMessage> = (0..(MAX_SESSION_MESSAGES + 5))
            .map(|index| ChatMessage::user(format!("msg-{index}")))
            .collect();

        trim_history(&mut history);

        assert_eq!(history.len(), MAX_SESSION_MESSAGES);
        assert_eq!(history.first().unwrap().content, "msg-5");
        assert_eq!(
            history.last().unwrap().content,
            format!("msg-{}", MAX_SESSION_MESSAGES + 4)
        );
    }

    #[test]
    fn rollback_last_user_message_removes_failed_turn() {
        let mut history = vec![
            ChatMessage::assistant("existing reply"),
            ChatMessage::user("failed prompt"),
        ];

        rollback_last_user_message(&mut history, "failed prompt");

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "assistant");
    }

    #[test]
    fn rollback_last_user_message_leaves_other_entries_untouched() {
        let mut history = vec![
            ChatMessage::user("previous prompt"),
            ChatMessage::assistant("previous reply"),
        ];

        rollback_last_user_message(&mut history, "failed prompt");

        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "previous reply");
    }

    #[test]
    fn provider_id_normalization_accepts_common_spellings() {
        assert_eq!(normalize_provider_id("Azure_OpenAI"), "azure-openai");
        assert_eq!(normalize_provider_id(" gemini cli "), "gemini-cli");
    }

    #[test]
    fn agent_ca_cert_path_prefers_client_ca_path() {
        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(PathBuf::from("config/bootstrap-ca.pem")),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(PathBuf::from("config/client-ca-bundle.crt"));

        let ca_cert_path = agent_ca_cert_path_from_config(&bootstrap, Some(&client_config));

        assert_eq!(
            ca_cert_path,
            Some(PathBuf::from("config/client-ca-bundle.crt"))
        );
    }

    #[test]
    fn agent_ca_cert_path_falls_back_to_bootstrap_ca_when_client_ca_is_empty() {
        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(PathBuf::from("config/bootstrap-ca-bundle.crt")),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(PathBuf::new());

        let ca_cert_path = agent_ca_cert_path_from_config(&bootstrap, Some(&client_config));

        assert_eq!(
            ca_cert_path,
            Some(PathBuf::from("config/bootstrap-ca-bundle.crt"))
        );
    }

    #[test]
    fn choose_model_prefers_global_model_over_provider_default() {
        let config = ModelProviderConfig {
            provider: "openai".to_string(),
            model: Some("gpt-selected".to_string()),
            temperature: 0.4,
        };

        let model = choose_model(&config, Some("gpt-provider"), None, "openai").unwrap();

        assert_eq!(model, "gpt-selected");
    }

    #[test]
    fn catalog_selection_prefers_matching_skill_tools() {
        let catalog = EffectiveAgentCatalog {
            catalog_hash: Some("abc".into()),
            catalog_version: Some(42),
            stale: false,
            skills: vec![
                CatalogSkill {
                    name: "billing".into(),
                    description: Some("Invoice and account support".into()),
                    priority: Some(3),
                    tools: vec![CatalogTool {
                        name: "get_invoice".into(),
                        description: Some("Fetch invoice details".into()),
                        routing_domain: Some("billing".into()),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                CatalogSkill {
                    name: "profile".into(),
                    description: Some("Customer profile lookup".into()),
                    tools: vec![CatalogTool {
                        name: "get_profile".into(),
                        description: Some("Fetch profile details".into()),
                        routing_domain: Some("profile".into()),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
        };

        let selection = select_catalog_tools(&catalog, "please find the invoice", 4);

        assert!(selection.tool_names.contains("get_invoice"));
        assert!(!selection.tool_names.contains("get_profile"));
        let context = selection.context.unwrap();
        assert!(context.contains("billing"));
        assert!(context.contains("Tools: get_invoice"));
    }

    #[test]
    fn catalog_selection_omits_policy_blocked_tools() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "billing".into(),
                tools: vec![
                    CatalogTool {
                        name: "delete_invoice".into(),
                        description: Some("Delete invoice".into()),
                        destructive: Some(true),
                        policy: Some(CatalogToolPolicy {
                            allowed: Some(false),
                            reason: Some("destructive_tool_requires_approval_workflow".into()),
                            destructive: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    CatalogTool {
                        name: "get_invoice".into(),
                        description: Some("Fetch invoice details".into()),
                        policy: Some(CatalogToolPolicy {
                            allowed: Some(true),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let selection = select_catalog_tools(&catalog, "invoice", 4);

        assert!(selection.tool_names.contains("get_invoice"));
        assert!(!selection.tool_names.contains("delete_invoice"));
    }

    #[test]
    fn catalog_diagnostics_include_blocked_tools() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "admin".into(),
                policy_diagnostics: vec![serde_json::json!({
                    "toolName": "reset_account",
                    "reason": "approval_required_missing_workflow"
                })],
                tools: vec![CatalogTool {
                    name: "restricted_lookup".into(),
                    policy: Some(CatalogToolPolicy {
                        allowed: Some(false),
                        reason: Some("sensitivity_tier_exceeds_policy".into()),
                        sensitivity_tier: Some("restricted".into()),
                        max_sensitivity_tier: Some("internal".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };

        let tool_names = collect_catalog_tool_names(&catalog);
        let diagnostics = collect_policy_diagnostics(&catalog);

        assert!(tool_names.is_empty());
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .any(|item| item["toolName"] == "reset_account")
        );
        assert!(
            diagnostics
                .iter()
                .any(|item| item["toolName"] == "restricted_lookup")
        );
    }

    #[test]
    fn gateway_tools_are_filtered_by_catalog_selection() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "billing".into(),
                tools: vec![CatalogTool {
                    name: "get_invoice".into(),
                    description: Some("Fetch invoice details".into()),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let selection = select_catalog_tools(&catalog, "invoice", 4);
        let tools = vec![
            McpTool {
                name: "get_invoice".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
            McpTool {
                name: "get_profile".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        ];

        let filtered = filter_gateway_tools(tools, Some(&selection));

        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "get_invoice");
    }
}
