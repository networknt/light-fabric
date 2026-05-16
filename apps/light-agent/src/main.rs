use anyhow::{Context, Result, anyhow, bail};
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
use config_loader::ConfigLoader;
use futures_util::{SinkExt, StreamExt};
use hindsight_client::{HindsightMemory, PgHindsightClient};
use light_axum::{AxumApp, AxumTransport, ServerContext};
use light_runtime::{
    LightRuntimeBuilder,
    config::{BootstrapConfig, ClientConfig, PortalRegistryConfig, ServerConfig},
};
use light_runtime::{ModuleKind, ModuleRegistry};
use mcp_client::{McpContent, McpGatewayClient, McpTool};
use model_provider::{ChatMessage, ChatRequest, ChatResponse, OllamaProvider, Provider, ToolSpec};
use portal_registry::{PortalRegistryClient, RegistryHandler, ServiceRegistrationParams};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::services::ServeDir;
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;
use url::Url;
use uuid::Uuid;

const MAX_SESSION_MESSAGES: usize = 40;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaConfig {
    pub ollama_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClientConfig {
    pub gateway_url: String,
    pub path: String,
    pub timeout_ms: u64,
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

fn to_registry_ws_url(portal_url: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(portal_url)
        .with_context(|| format!("Invalid portal registry URL: {portal_url}"))?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("Unsupported portal registry URL scheme: {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow!("Failed to convert portal registry URL scheme"))?;
    url.set_path("/ws/microservice");
    url.set_query(None);
    Ok(url.to_string())
}

fn to_portal_query_url(portal_url: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(portal_url)
        .with_context(|| format!("Invalid portal query URL: {portal_url}"))?;
    url.set_path("/portal/query");
    url.set_query(None);
    Ok(url.to_string())
}

fn registry_advertised_address(server_config: &ServerConfig) -> String {
    server_config
        .advertised_address
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            if server_config.ip == "0.0.0.0" {
                "127.0.0.1".to_string()
            } else {
                server_config.ip.clone()
            }
        })
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

impl PortalQueryClient {
    fn with_options(
        portal_url: &str,
        token: String,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(timeout_ms))
            .connect_timeout(std::time::Duration::from_millis(timeout_ms));

        if let Some(pem) = ca_cert_pem {
            let cert = reqwest::Certificate::from_pem(pem)
                .context("Invalid ca_cert_pem: failed to parse PEM-encoded CA certificate")?;
            builder = builder.add_root_certificate(cert);
        }

        if !verify_hostname {
            builder = builder.danger_accept_invalid_hostnames(true);
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

struct AgentState {
    ollama_config: OllamaConfig,
    provider: OllamaProvider,
    mcp_client: McpGatewayClient,
    portal_query_client: Option<PortalQueryClient>,
    catalog_cache: AgentCatalogCache,
    memory: Arc<dyn HindsightMemory>,
    db: PgPool,
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
    state: Arc<AgentState>,
}

impl AxumApp for AgentApp {
    fn router(&self, _context: ServerContext) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/diagnostics/tools", get(tool_diagnostics))
            .route("/chat", get(ws_handler))
            .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
            .with_state(self.state.clone())
    }
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
    if let Err(e) =
        ensure_session_memory_bank(&state.db, state.host_id, bank_id, &current_session_id).await
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

    let mut history = match sqlx::query(
        "SELECT messages FROM agent_session_history_t
         WHERE host_id = $1 AND bank_id = $2 AND session_id = $3",
    )
    .bind(state.host_id)
    .bind(bank_id)
    .bind(session_uuid)
    .fetch_optional(&state.db)
    .await
    {
        Ok(Some(row)) => {
            let messages: serde_json::Value = row.get("messages");
            serde_json::from_value::<Vec<ChatMessage>>(messages).unwrap_or_default()
        }
        Ok(None) => Vec::new(),
        Err(e) => {
            warn!(
                "Failed to load session history for host_id={}, bank_id={}, session_id={}: {}",
                state.host_id, bank_id, session_uuid, e
            );
            Vec::new()
        }
    };

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

                        match serde_json::to_value(&history) {
                            Ok(history_payload) => {
                                if let Err(e) = sqlx::query(
                                    "INSERT INTO agent_session_history_t
                                    (host_id, bank_id, session_id, messages)
                                    VALUES ($1, $2, $3, $4)
                                    ON CONFLICT (host_id, bank_id, session_id)
                                    DO UPDATE SET messages = EXCLUDED.messages,
                                                  update_ts = CURRENT_TIMESTAMP",
                                )
                                .bind(state.host_id)
                                .bind(bank_id)
                                .bind(session_uuid)
                                .bind(history_payload)
                                .execute(&state.db)
                                .await
                                {
                                    warn!("Failed to persist session history: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Failed to serialize session history: {}", e);
                            }
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

async fn ensure_session_memory_bank(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
    session_id: &str,
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
                .chat(request, &state.ollama_config.model, 0.7)
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
                None,
                serde_json::json!({ "session_id": session_id }),
            )
            .await
            .map_err(|e| warn!("Failed to retain memory: {}", e));
    }

    Ok(response)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config_dir = PathBuf::from("config");
    let values_path = config_dir.join("values.yml");
    let values_yaml = std::fs::read_to_string(&values_path).unwrap_or_default();
    let loader = ConfigLoader::new(&values_yaml, None, None)?;
    let module_registry = Arc::new(ModuleRegistry::new());

    let ollama_config: OllamaConfig = loader.load_typed([config_dir.join("ollama.yml")])?;
    let mcp_config: McpClientConfig = loader.load_typed([config_dir.join("mcp-client.yml")])?;
    module_registry.register_loaded_config(
        "light-agent/ollama",
        "ollama",
        ModuleKind::Application,
        &ollama_config,
        [],
        true,
        Some(true),
        false,
    )?;
    module_registry.register_loaded_config(
        "light-agent/mcp-client",
        "mcp-client",
        ModuleKind::Application,
        &mcp_config,
        [],
        true,
        Some(true),
        false,
    )?;

    // Load startup.yml (for bootstrap_ca_cert_path) and client.yml (for verify_hostname).
    // This mirrors how the config-server and controller-rs clients are configured in light-runtime.
    let startup_config: BootstrapConfig = loader
        .load_typed([config_dir.join("startup.yml")])
        .unwrap_or_default();
    let client_config: Option<ClientConfig> =
        loader.load_typed([config_dir.join("client.yml")]).ok();
    let server_config: ServerConfig = loader
        .load_typed([config_dir.join("server.yml")])
        .context("Failed to load server.yml")?;
    let portal_registry_config: PortalRegistryConfig = loader
        .load_typed([config_dir.join("portal-registry.yml")])
        .context("Failed to load portal-registry.yml")?;

    let mcp_gateway_url = format!(
        "{}/{}",
        mcp_config.gateway_url.trim_end_matches('/'),
        mcp_config.path.trim_start_matches('/')
    );

    // Load TLS settings from the shared config files, consistent with how the
    // config-server and controller-rs clients are built by light-runtime.
    let ca_cert: Option<Vec<u8>> = startup_config
        .bootstrap_ca_cert_path
        .as_deref()
        .and_then(|path| std::fs::read(path).ok());
    let verify_hostname: bool = client_config
        .as_ref()
        .map(|c| c.tls.verify_hostname)
        .unwrap_or(true);
    if !verify_hostname {
        warn!(
            "TLS hostname verification is disabled for the MCP gateway and portal query clients; this weakens server identity validation"
        );
    }

    let mcp_client = McpGatewayClient::with_options(
        &mcp_gateway_url,
        ca_cert.as_deref(),
        verify_hostname,
        mcp_config.timeout_ms,
    )
    .context("Failed to build MCP gateway client")?;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:secret@localhost:5432/configserver".to_string());
    let pool = PgPool::connect(&db_url)
        .await
        .context("Failed to connect to database")?;

    let memory = Arc::new(PgHindsightClient::new(pool.clone()));
    let host_id = required_uuid_env_var("LIGHT_AGENT_HOST_ID")?;
    let agent_def_id =
        optional_uuid_env_var(&["LIGHT_AGENT_AGENT_DEF_ID", "LIGHT_AGENT_API_VERSION_ID"])?;
    let env_tag =
        (!server_config.environment.is_empty()).then(|| server_config.environment.clone());
    let portal_token = registry_token(&portal_registry_config)
        .context("Missing portal registry token; set light_portal_authorization or portalRegistry.portalToken")?;
    let portal_query_client = Some(
        PortalQueryClient::with_options(
            &portal_query_base_url(&portal_registry_config),
            portal_token.clone(),
            ca_cert.as_deref(),
            verify_hostname,
            mcp_config.timeout_ms,
        )
        .context("Failed to build portal query client")?,
    );
    let catalog_cache = AgentCatalogCache::new();

    // Registry Client Configuration
    let registry_handler = Arc::new(AgentRegistryHandler::new(catalog_cache.clone()));
    let registration_params = ServiceRegistrationParams {
        service_id: server_config.service_id.clone(),
        version: "0.1.0".to_string(),
        protocol: if server_config.enable_https {
            "https".to_string()
        } else {
            "http".to_string()
        },
        address: registry_advertised_address(&server_config),
        port: if server_config.enable_https {
            server_config.https_port
        } else {
            server_config.http_port
        },
        tags: HashMap::new(),
        env_tag: env_tag.clone(),
        jwt: portal_token,
    };

    let registry_url = to_registry_ws_url(&portal_registry_config.portal_url)?;
    let registry = Arc::new(PortalRegistryClient::new(
        &registry_url,
        registration_params,
        registry_handler,
    )?);

    let state = Arc::new(AgentState {
        provider: OllamaProvider::new(Some(&ollama_config.ollama_url), None)
            .context("Failed to build Ollama provider")?,
        mcp_client,
        portal_query_client,
        catalog_cache,
        ollama_config,
        memory,
        db: pool,
        host_id,
        agent_def_id,
        service_id: server_config.service_id,
        env_tag,
    });

    let app = AgentApp { state };

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_config_dir("config")
        .with_module_registry(module_registry)
        .with_registry_client(Arc::clone(&registry))
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

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let use_ansi = std::env::var("AGENT_LOG_ANSI")
        .ok()
        .map(|v| v.trim().to_lowercase())
        .map(|v| v == "true" || v == "1" || v == "yes" || v == "on");

    let subscriber = tracing_subscriber::fmt().with_env_filter(filter);

    match use_ansi {
        Some(use_ansi) => subscriber.with_ansi(use_ansi).init(),
        None => subscriber.init(),
    }
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
        MAX_SESSION_MESSAGES, collect_catalog_tool_names, collect_policy_diagnostics,
        filter_gateway_tools, rollback_last_user_message, select_catalog_tools, trim_history,
    };
    use mcp_client::McpTool;

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
