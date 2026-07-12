use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::{
    Json, Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
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
use light_security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SecurityRuntime, load_security_runtime,
    verify_jwt_token,
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
use sqlx::{PgPool, Row, postgres::PgListener};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use agent_core::{AgentSessionId, PolicySnapshot, sha256_digest};
use agent_delegation::{DelegationClaims, DelegationKind, DelegationSigner};
use light_agent::domain::{AgentRepository, SessionSpec, TurnRuntimeResolution};

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const MODEL_PROVIDER_FILE: &str = "model-provider.yml";
const MAX_SESSION_MESSAGES: usize = 40;
const DEFAULT_CATALOG_CACHE_TTL_SECONDS: u64 = 60;
const DEFAULT_CATALOG_STALE_ON_ERROR_SECONDS: u64 = 300;
const DEFAULT_CATALOG_SELECTION_LIMIT: usize = 12;
const DEFAULT_SEMANTIC_CATALOG_LIMIT: usize = 50;
const DEFAULT_MAX_TURN_SECONDS: u64 = 120;
const DEFAULT_MAX_MODEL_CALLS: usize = 10;
const DEFAULT_MAX_ACTION_CALLS: usize = 20;
const DEFAULT_MAX_USER_MESSAGE_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_TOOL_ARGUMENT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_GATEWAY_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_MAX_RESPONSE_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_OUTPUT_DEPTH: usize = 16;
const DEFAULT_MAX_OUTPUT_ITEMS: usize = 1024;
const DEFAULT_MAX_TURN_TOKENS: u64 = 64 * 1024;

#[derive(Debug, Clone)]
struct AgentLimits {
    turn_timeout: Duration,
    max_model_calls: usize,
    max_action_calls: usize,
    max_user_message_bytes: usize,
    max_tool_argument_bytes: usize,
    max_tool_output_bytes: usize,
    max_gateway_response_bytes: usize,
    max_response_bytes: usize,
    max_output_depth: usize,
    max_output_items: usize,
    max_turn_tokens: u64,
}

impl AgentLimits {
    fn from_env() -> Self {
        Self {
            turn_timeout: duration_from_env_seconds(
                "LIGHT_AGENT_MAX_TURN_SECONDS",
                DEFAULT_MAX_TURN_SECONDS,
            ),
            max_model_calls: usize_from_env(
                "LIGHT_AGENT_MAX_MODEL_CALLS",
                DEFAULT_MAX_MODEL_CALLS,
                100,
            ),
            max_action_calls: usize_from_env(
                "LIGHT_AGENT_MAX_ACTION_CALLS",
                DEFAULT_MAX_ACTION_CALLS,
                1_000,
            ),
            max_user_message_bytes: usize_from_env(
                "LIGHT_AGENT_MAX_USER_MESSAGE_BYTES",
                DEFAULT_MAX_USER_MESSAGE_BYTES,
                1024 * 1024,
            ),
            max_tool_argument_bytes: usize_from_env(
                "LIGHT_AGENT_MAX_TOOL_ARGUMENT_BYTES",
                DEFAULT_MAX_TOOL_ARGUMENT_BYTES,
                1024 * 1024,
            ),
            max_tool_output_bytes: usize_from_env(
                "LIGHT_AGENT_MAX_TOOL_OUTPUT_BYTES",
                DEFAULT_MAX_TOOL_OUTPUT_BYTES,
                4 * 1024 * 1024,
            ),
            max_gateway_response_bytes: usize_from_env(
                "LIGHT_AGENT_MAX_GATEWAY_RESPONSE_BYTES",
                DEFAULT_MAX_GATEWAY_RESPONSE_BYTES,
                8 * 1024 * 1024,
            ),
            max_response_bytes: usize_from_env(
                "LIGHT_AGENT_MAX_RESPONSE_BYTES",
                DEFAULT_MAX_RESPONSE_BYTES,
                1024 * 1024,
            ),
            max_output_depth: usize_from_env(
                "LIGHT_AGENT_MAX_OUTPUT_DEPTH",
                DEFAULT_MAX_OUTPUT_DEPTH,
                64,
            ),
            max_output_items: usize_from_env(
                "LIGHT_AGENT_MAX_OUTPUT_ITEMS",
                DEFAULT_MAX_OUTPUT_ITEMS,
                10_000,
            ),
            max_turn_tokens: u64_from_env(
                "LIGHT_AGENT_MAX_TURN_TOKENS",
                DEFAULT_MAX_TURN_TOKENS,
                10_000_000,
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionOwner {
    principal_id: Uuid,
    agent_def_id: Uuid,
}

#[derive(Debug, Clone)]
struct AuthenticatedRequest {
    authorization: String,
    owner: SessionOwner,
    caller_claims: serde_json::Value,
    caller_subject: String,
}

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

fn duration_from_env_seconds(name: &str, default_seconds: u64) -> Duration {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_seconds))
}

fn bool_from_env(name: &str, default_value: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default_value)
}

fn usize_from_env(name: &str, default_value: usize, max_value: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(max_value))
        .unwrap_or(default_value)
}

fn u64_from_env(name: &str, default_value: u64, max_value: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(max_value))
        .unwrap_or(default_value)
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
    inner: Arc<RwLock<HashMap<CatalogCacheKey, CachedAgentCatalog>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CatalogCacheKey {
    host_id: Uuid,
    agent_def_id: Uuid,
    definition_version: i64,
    policy_digest: String,
    service_id: String,
    env_tag: Option<String>,
}

#[derive(Debug, Clone)]
struct CachedAgentCatalog {
    catalog: EffectiveAgentCatalog,
    fetched_at: Instant,
}

impl AgentCatalogCache {
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn get_fresh(
        &self,
        key: &CatalogCacheKey,
        ttl: Duration,
    ) -> Option<EffectiveAgentCatalog> {
        self.get_with_max_age(key, ttl, false).await
    }

    async fn get_stale(
        &self,
        key: &CatalogCacheKey,
        max_age: Duration,
    ) -> Option<EffectiveAgentCatalog> {
        self.get_with_max_age(key, max_age, true).await
    }

    async fn get_with_max_age(
        &self,
        key: &CatalogCacheKey,
        max_age: Duration,
        mark_stale: bool,
    ) -> Option<EffectiveAgentCatalog> {
        let entry = self.inner.read().await.get(key).cloned()?;
        if entry.fetched_at.elapsed() > max_age {
            return None;
        }
        let mut catalog = entry.catalog;
        if mark_stale {
            catalog.stale = true;
        }
        Some(catalog)
    }

    async fn diagnostics(
        &self,
        ttl: Duration,
        stale_on_error: Duration,
    ) -> CatalogCacheDiagnostics {
        let entry = self
            .inner
            .read()
            .await
            .values()
            .max_by_key(|entry| entry.fetched_at)
            .cloned();
        let age_seconds = entry
            .as_ref()
            .map(|entry| entry.fetched_at.elapsed().as_secs());
        let fresh = entry
            .as_ref()
            .is_some_and(|entry| entry.fetched_at.elapsed() <= ttl);
        let usable_on_error = entry
            .as_ref()
            .is_some_and(|entry| entry.fetched_at.elapsed() <= stale_on_error);
        CatalogCacheDiagnostics {
            ttl_seconds: ttl.as_secs(),
            stale_on_error_seconds: stale_on_error.as_secs(),
            age_seconds,
            fresh,
            usable_on_error,
        }
    }

    async fn set(&self, key: CatalogCacheKey, catalog: EffectiveAgentCatalog) {
        self.inner.write().await.insert(
            key,
            CachedAgentCatalog {
                catalog,
                fetched_at: Instant::now(),
            },
        );
    }

    async fn clear(&self) {
        self.inner.write().await.clear();
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogCacheDiagnostics {
    ttl_seconds: u64,
    stale_on_error_seconds: u64,
    age_seconds: Option<u64>,
    fresh: bool,
    usable_on_error: bool,
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
    tool_id: Option<Uuid>,
    stable_tool_ref: Option<Uuid>,
    execution_placement: Option<String>,
    model_alias: Option<String>,
    schema_digest: Option<String>,
    name: String,
    description: Option<String>,
    lifecycle_status: Option<String>,
    semantic_description: Option<String>,
    semantic_keywords: Vec<String>,
    routing_domain: Option<String>,
    semantic_namespace: Option<String>,
    sensitivity_tier: Option<String>,
    semantic_weight: Option<f32>,
    semantic_score: Option<f32>,
    vector_score: Option<f32>,
    keyword_score: Option<f32>,
    combined_score: Option<f32>,
    vector_distance: Option<f32>,
    semantic_rank: Option<u32>,
    source_protocol: Option<String>,
    target_personas: Option<String>,
    read_only: Option<bool>,
    idempotent: Option<bool>,
    destructive: Option<bool>,
    requires_approval: Option<bool>,
    cost_tier: Option<String>,
    estimated_latency_ms: Option<u64>,
    cache_ttl_seconds: Option<u64>,
    retry_policy: Option<serde_json::Value>,
    rate_limit: Option<serde_json::Value>,
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

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct CatalogToolDiagnostic {
    skill: String,
    tool_name: String,
    selected: bool,
    reason: String,
    score: Option<f32>,
    semantic_score: Option<f32>,
    vector_score: Option<f32>,
    keyword_score: Option<f32>,
    combined_score: Option<f32>,
    vector_distance: Option<f32>,
    semantic_rank: Option<u32>,
    lifecycle_status: Option<String>,
    sensitivity_tier: Option<String>,
    cost_tier: Option<String>,
    estimated_latency_ms: Option<u64>,
    cache_ttl_seconds: Option<u64>,
    retry_policy: Option<serde_json::Value>,
    rate_limit: Option<serde_json::Value>,
    source_protocol: Option<String>,
    routing_domain: Option<String>,
    semantic_namespace: Option<String>,
    read_only: Option<bool>,
    idempotent: Option<bool>,
    destructive: Option<bool>,
    requires_approval: Option<bool>,
    approval_configured: Option<bool>,
}

#[derive(Debug, Clone)]
struct CatalogSelection {
    tool_names: HashSet<String>,
    tool_refs: HashMap<String, Uuid>,
    context: Option<String>,
    selected_tools: Vec<CatalogToolDiagnostic>,
    hidden_tools: Vec<CatalogToolDiagnostic>,
}

#[derive(Clone)]
struct PortalQueryClient {
    url: String,
    token: String,
    client: reqwest::Client,
}

fn build_effective_catalog_data(
    host_id: Uuid,
    agent_def_id: Uuid,
    definition_version: i64,
    policy_digest: String,
    service_id: &str,
    env_tag: Option<&str>,
    semantic_query: Option<&str>,
    semantic_limit: Option<usize>,
) -> serde_json::Value {
    let mut data = serde_json::json!({
        "hostId": host_id,
        "agentDefId": agent_def_id,
        "definitionVersion": definition_version,
        "policyDigest": policy_digest,
        "serviceId": service_id,
    });
    if let Some(env_tag) = env_tag.filter(|value| !value.trim().is_empty()) {
        data["envTag"] = serde_json::json!(env_tag);
    }
    if let Some(query) = semantic_query
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        data["semanticQuery"] = serde_json::json!(query);
    }
    if let Some(limit) = semantic_limit.filter(|value| *value > 0) {
        data["semanticLimit"] = serde_json::json!(limit);
    }
    data
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
        definition_version: i64,
        policy_digest: &str,
        service_id: &str,
        env_tag: Option<&str>,
        semantic_query: Option<&str>,
        semantic_limit: Option<usize>,
    ) -> Result<EffectiveAgentCatalog> {
        let data = build_effective_catalog_data(
            host_id,
            agent_def_id,
            definition_version,
            policy_digest.to_string(),
            service_id,
            env_tag,
            semantic_query,
            semantic_limit,
        );
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
        owner: SessionOwner,
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
        owner: SessionOwner,
    ) -> Result<()> {
        insert_session_memory_bank(&self.pool, host_id, bank_id, session_id, owner).await
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
        owner: SessionOwner,
    ) -> Result<()> {
        if let Some(existing) = load_session_owner(&self.pool, host_id, bank_id).await? {
            return validate_session_owner(existing, owner);
        }

        self.command_client
            .call(
                "createAgentMemoryBank",
                serde_json::json!({
                    "hostId": host_id,
                    "bankId": bank_id,
                    "agentDefId": owner.agent_def_id,
                    "userId": owner.principal_id,
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
        let persisted = load_session_owner(&self.pool, host_id, bank_id)
            .await?
            .context("created session memory bank is not visible")?;
        validate_session_owner(persisted, owner)
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
    runtime_config: RuntimeConfig,
    default_temperature: f64,
    mcp_client: McpGatewayClient,
    portal_query_client: Option<PortalQueryClient>,
    catalog_cache: AgentCatalogCache,
    memory: Arc<dyn MemoryStore>,
    domain: AgentRepository,
    turn_dispatch: TurnDispatchCoordinator,
    delegation_signer: Option<Arc<DelegationSigner>>,
    security: Arc<SecurityRuntime>,
    limits: AgentLimits,
    host_id: Uuid,
    agent_def_id: Uuid,
    definition_version: i64,
    policy_digest: String,
    service_id: String,
    env_tag: Option<String>,
    catalog_cache_ttl: Duration,
    catalog_stale_on_error: Duration,
    catalog_semantic_search_enabled: bool,
    catalog_semantic_limit: usize,
}

#[derive(Clone)]
struct TurnDispatchCoordinator {
    domain: AgentRepository,
    waiters: Arc<RwLock<HashMap<Uuid, Arc<Notify>>>>,
}

impl TurnDispatchCoordinator {
    fn new(domain: AgentRepository) -> Self {
        Self {
            domain,
            waiters: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn register(&self, turn_id: Uuid) -> Arc<Notify> {
        let notify = Arc::new(Notify::new());
        self.waiters
            .write()
            .await
            .insert(turn_id, Arc::clone(&notify));
        notify
    }

    async fn remove(&self, turn_id: Uuid) {
        self.waiters.write().await.remove(&turn_id);
    }

    async fn wake(&self, turn_id: Uuid) {
        if let Some(waiter) = self.waiters.read().await.get(&turn_id).cloned() {
            waiter.notify_waiters();
        }
    }

    fn spawn(&self, host_id: Uuid) {
        let coordinator = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = coordinator.listen_and_dispatch(host_id).await {
                    warn!(%error, "agent fair-dispatch listener disconnected");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        });
    }

    async fn listen_and_dispatch(&self, host_id: Uuid) -> Result<()> {
        let mut listener = PgListener::connect_with(&self.domain.pool()).await?;
        listener.listen("agent_turn_queue_v1").await?;
        listener.listen("agent_turn_capacity_v1").await?;
        listener.listen("agent_turn_activated_v1").await?;
        // LISTEN first, then catch up, so a commit in the handoff window is
        // either visible to the scan or queued on this connection.
        self.dispatch_available(host_id).await?;
        self.reconcile_local_waiters(host_id).await?;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), listener.recv()).await {
                Ok(Ok(notification)) if notification.channel() == "agent_turn_activated_v1" => {
                    if let Ok(turn_id) = Uuid::parse_str(notification.payload()) {
                        self.wake(turn_id).await;
                    }
                }
                Ok(Ok(notification)) => {
                    if notification.payload() == host_id.to_string() {
                        self.dispatch_available(host_id).await?;
                    }
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => {
                    self.dispatch_available(host_id).await?;
                    self.reconcile_local_waiters(host_id).await?;
                }
            }
        }
    }

    async fn dispatch_available(&self, host_id: Uuid) -> Result<()> {
        // Bound each wake pass; another notification or the five-second
        // catch-up continues large queues without monopolizing the listener.
        for _ in 0..256 {
            if self
                .domain
                .dispatch_next_turn_fair(host_id)
                .await?
                .is_none()
            {
                break;
            }
        }
        Ok(())
    }

    async fn reconcile_local_waiters(&self, host_id: Uuid) -> Result<()> {
        let turn_ids = self
            .waiters
            .read()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        for turn_id in self.domain.active_turn_ids(host_id, &turn_ids).await? {
            self.wake(turn_id).await;
        }
        Ok(())
    }
}

impl AgentState {
    fn catalog_cache_key(&self) -> CatalogCacheKey {
        CatalogCacheKey {
            host_id: self.host_id,
            agent_def_id: self.agent_def_id,
            definition_version: self.definition_version,
            policy_digest: self.policy_digest.clone(),
            service_id: self.service_id.clone(),
            env_tag: self.env_tag.clone(),
        }
    }
    fn turn_catalog_cache_key(&self, turn: &TurnRuntimeResolution) -> CatalogCacheKey {
        CatalogCacheKey {
            host_id: turn.host_id,
            agent_def_id: turn.agent_def_id,
            definition_version: turn.definition_version,
            policy_digest: turn.policy_digest.clone(),
            service_id: self.service_id.clone(),
            env_tag: self.env_tag.clone(),
        }
    }
    async fn catalog_selection_for_turn(
        &self,
        turn: &TurnRuntimeResolution,
        prompt: &str,
    ) -> Option<CatalogSelection> {
        let catalog = if self.catalog_semantic_search_enabled && !prompt.trim().is_empty() {
            match self
                .fetch_turn_catalog(turn, Some(prompt), Some(self.catalog_semantic_limit))
                .await
            {
                Ok(Some(catalog)) => Some(catalog),
                Ok(None) => self.effective_catalog_for_turn(turn).await,
                Err(error) => {
                    warn!(%error, turn_id=%turn.turn_id.0, "semantic turn catalog refresh failed");
                    self.effective_catalog_for_turn(turn).await
                }
            }
        } else {
            self.effective_catalog_for_turn(turn).await
        }?;
        Some(select_catalog_tools(
            &catalog,
            prompt,
            DEFAULT_CATALOG_SELECTION_LIMIT,
        ))
    }
    async fn effective_catalog_for_turn(
        &self,
        turn: &TurnRuntimeResolution,
    ) -> Option<EffectiveAgentCatalog> {
        let key = self.turn_catalog_cache_key(turn);
        if let Some(catalog) = self
            .catalog_cache
            .get_fresh(&key, self.catalog_cache_ttl)
            .await
        {
            return Some(catalog);
        }
        match self.fetch_turn_catalog(turn, None, None).await {
            Ok(Some(catalog)) => {
                self.catalog_cache.set(key, catalog.clone()).await;
                Some(catalog)
            }
            Ok(None) => None,
            Err(error) => {
                warn!(%error, turn_id=%turn.turn_id.0, "turn catalog refresh failed; using bounded stale entry");
                self.catalog_cache
                    .get_stale(&key, self.catalog_stale_on_error)
                    .await
            }
        }
    }
    async fn fetch_turn_catalog(
        &self,
        turn: &TurnRuntimeResolution,
        query: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Option<EffectiveAgentCatalog>> {
        let Some(client) = self.portal_query_client.as_ref() else {
            return Ok(None);
        };
        client
            .get_effective_agent_catalog(
                turn.host_id,
                turn.agent_def_id,
                turn.definition_version,
                &turn.policy_digest,
                &self.service_id,
                self.env_tag.as_deref(),
                query,
                limit,
            )
            .await
            .map(Some)
    }
    async fn effective_catalog(&self) -> Option<EffectiveAgentCatalog> {
        let key = self.catalog_cache_key();
        if let Some(catalog) = self
            .catalog_cache
            .get_fresh(&key, self.catalog_cache_ttl)
            .await
        {
            return Some(catalog);
        }

        match self.refresh_effective_catalog().await {
            Ok(catalog) => catalog,
            Err(err) => {
                warn!(
                    "Effective agent catalog refresh failed; trying bounded stale catalog fallback: {err}"
                );
                self.catalog_cache
                    .get_stale(&key, self.catalog_stale_on_error)
                    .await
            }
        }
    }

    async fn refresh_effective_catalog(&self) -> Result<Option<EffectiveAgentCatalog>> {
        let Some(client) = self.portal_query_client.as_ref() else {
            return Ok(None);
        };
        let catalog = client
            .get_effective_agent_catalog(
                self.host_id,
                self.agent_def_id,
                self.definition_version,
                &self.policy_digest,
                &self.service_id,
                self.env_tag.as_deref(),
                None,
                None,
            )
            .await?;
        self.catalog_cache
            .set(self.catalog_cache_key(), catalog.clone())
            .await;
        Ok(Some(catalog))
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
    catalog_cache: CatalogCacheDiagnostics,
    catalog_tools: Vec<String>,
    selected_tools: Vec<CatalogToolDiagnostic>,
    hidden_tools: Vec<CatalogToolDiagnostic>,
    gateway_available: bool,
    gateway_tools: Vec<String>,
    missing_from_gateway: Vec<String>,
    extra_gateway_tools: Vec<String>,
    policy_blocked: Vec<serde_json::Value>,
    catalog_error: Option<String>,
    gateway_error: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ToolDiagnosticsQuery {
    #[serde(default)]
    refresh: Option<bool>,
    #[serde(default, alias = "query")]
    prompt: Option<String>,
}

async fn tool_diagnostics(
    Query(params): Query<ToolDiagnosticsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> Response {
    let authenticated = match authenticate_request(&headers, &state).await {
        Ok(authenticated) => authenticated,
        Err(rejection) => return rejection_response(rejection),
    };
    let (catalog, catalog_error) = if params.refresh.unwrap_or(false) {
        match state.refresh_effective_catalog().await {
            Ok(catalog) => (catalog, None),
            Err(err) => (
                state
                    .catalog_cache
                    .get_stale(&state.catalog_cache_key(), state.catalog_stale_on_error)
                    .await,
                Some(err.to_string()),
            ),
        }
    } else {
        (state.effective_catalog().await, None)
    };
    let diagnostic_selection = catalog.as_ref().map(|catalog| {
        select_catalog_tools(
            catalog,
            params.prompt.as_deref().unwrap_or_default(),
            DEFAULT_CATALOG_SELECTION_LIMIT,
        )
    });
    let (catalog_tools, policy_blocked) = catalog
        .as_ref()
        .map(|catalog| {
            (
                collect_catalog_tool_names(catalog),
                collect_policy_diagnostics(catalog),
            )
        })
        .unwrap_or_default();

    let gateway_result = state
        .mcp_client
        .list_tools(Some(authenticated.authorization.as_str()))
        .await;
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
        catalog_cache: state
            .catalog_cache
            .diagnostics(state.catalog_cache_ttl, state.catalog_stale_on_error)
            .await,
        catalog_tools,
        selected_tools: diagnostic_selection
            .as_ref()
            .map(|selection| selection.selected_tools.clone())
            .unwrap_or_default(),
        hidden_tools: diagnostic_selection
            .map(|selection| selection.hidden_tools)
            .unwrap_or_default(),
        gateway_available,
        gateway_tools,
        missing_from_gateway,
        extra_gateway_tools,
        policy_blocked,
        catalog_error,
        gateway_error,
    })
    .into_response()
}

fn rejection_response(rejection: HandlerRejection) -> Response {
    let status = StatusCode::from_u16(rejection.status).unwrap_or(StatusCode::UNAUTHORIZED);
    let mut response = (
        status,
        Json(serde_json::json!({
            "code": rejection.code,
            "message": rejection.message
        })),
    )
        .into_response();
    for (name, value) in rejection.headers {
        if let (Ok(name), Ok(value)) = (
            name.parse::<axum::http::HeaderName>(),
            value.parse::<axum::http::HeaderValue>(),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

fn bearer_token(headers: &HeaderMap) -> Result<&str, HandlerRejection> {
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| HandlerRejection::unauthorized("missing bearer token"))?;
    let (scheme, token) = authorization
        .split_once(' ')
        .ok_or_else(|| HandlerRejection::unauthorized("invalid authorization header"))?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return Err(HandlerRejection::unauthorized(
            "authorization header must use Bearer",
        ));
    }
    Ok(token.trim())
}

fn claim_string<'a>(principal: &'a AuthPrincipal, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| {
            principal
                .claims
                .get(*name)
                .and_then(serde_json::Value::as_str)
        })
        .filter(|value| !value.trim().is_empty())
}

fn bind_authenticated_principal(
    principal: &AuthPrincipal,
    expected_host_id: Uuid,
    expected_service_id: &str,
    default_agent_def_id: Uuid,
) -> Result<SessionOwner, HandlerRejection> {
    let host_id = principal
        .host
        .as_deref()
        .or_else(|| claim_string(principal, &["host_id", "hostId"]))
        .ok_or_else(|| HandlerRejection::forbidden("token is not bound to a host"))?;
    let host_id = Uuid::parse_str(host_id)
        .map_err(|_| HandlerRejection::forbidden("token host is invalid"))?;
    if host_id != expected_host_id {
        return Err(HandlerRejection::forbidden(
            "token is not valid for this host",
        ));
    }

    let service_id = claim_string(principal, &["sid", "service_id", "serviceId"])
        .ok_or_else(|| HandlerRejection::forbidden("token is not bound to an agent service"))?;
    if service_id != expected_service_id {
        return Err(HandlerRejection::forbidden(
            "token is not valid for this agent service",
        ));
    }

    let principal_id = principal
        .user_id
        .as_deref()
        .or(principal.client_id.as_deref())
        .ok_or_else(|| HandlerRejection::forbidden("token has no principal identity"))?;
    let principal_id = Uuid::parse_str(principal_id)
        .map_err(|_| HandlerRejection::forbidden("token principal identity is invalid"))?;

    let agent_def_id = claim_string(principal, &["agent_def_id", "agentDefId"])
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|_| HandlerRejection::forbidden("token agent definition is invalid"))
        })
        .transpose()?
        .unwrap_or(default_agent_def_id);
    Ok(SessionOwner {
        principal_id,
        agent_def_id,
    })
}

async fn authenticate_request(
    headers: &HeaderMap,
    state: &AgentState,
) -> Result<AuthenticatedRequest, HandlerRejection> {
    let token = bearer_token(headers)?;
    let principal = verify_jwt_token(&state.security, token, JwtExpiryMode::Enforce).await?;
    let owner = bind_authenticated_principal(
        &principal,
        state.host_id,
        &state.service_id,
        state.agent_def_id,
    )?;
    Ok(AuthenticatedRequest {
        authorization: format!("Bearer {token}"),
        owner,
        caller_claims: principal.claims,
        caller_subject: principal
            .user_id
            .or(principal.client_id)
            .unwrap_or_default(),
    })
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> Response {
    let authenticated = match authenticate_request(&headers, &state).await {
        Ok(authenticated) => authenticated,
        Err(rejection) => return rejection_response(rejection),
    };
    let session_id = match params.get("sessionId") {
        Some(session_id) => match Uuid::parse_str(session_id) {
            Ok(session_id) => session_id,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": "INVALID_SESSION_ID",
                        "message": "sessionId must be a UUID"
                    })),
                )
                    .into_response();
            }
        },
        None => Uuid::new_v4(),
    };
    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id, authenticated))
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ClientMessage {
    pub text: String,
    #[serde(default)]
    pub client_message_id: Option<String>,
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

#[derive(Debug, Clone)]
struct ScoredCatalogTool {
    score: f32,
    sequence: usize,
    cost_rank: u8,
    latency_ms: u64,
    tool_name: String,
    tool_ref: Uuid,
    diagnostic: CatalogToolDiagnostic,
}

fn select_catalog_tools(
    catalog: &EffectiveAgentCatalog,
    prompt: &str,
    limit: usize,
) -> CatalogSelection {
    let query_terms = tokenize(prompt);
    let informational_prompt = prompt_is_informational(prompt);
    let mut scored_tools = Vec::new();
    let mut hidden_tools = Vec::new();

    for (skill_index, skill) in catalog.skills.iter().enumerate() {
        let skill_text = searchable_skill_text(skill);
        let skill_score = keyword_score(&query_terms, &skill_text);
        for tool in &skill.tools {
            if tool.name.trim().is_empty() {
                continue;
            }
            if let Some(reason) = catalog_tool_hidden_reason(tool) {
                hidden_tools.push(tool_diagnostic(skill, tool, false, reason, None));
                continue;
            }
            let tool_text = searchable_tool_text(tool);
            let routing_score = routing_score(&query_terms, tool);
            let priority = skill.priority.unwrap_or_default().max(0) as f32 / 10.0;
            let semantic_weight = tool.semantic_weight.unwrap_or(1.0).max(0.1);
            let base_score = ((skill_score * 0.75)
                + (keyword_score(&query_terms, &tool_text) * 1.5)
                + routing_score
                + priority)
                * semantic_weight;
            let portal_semantic_score = tool
                .combined_score
                .or(tool.semantic_score)
                .or(tool.vector_score)
                .map(|score| score.max(0.0))
                .unwrap_or(0.0);
            if base_score <= 0.0 && portal_semantic_score <= 0.0 {
                continue;
            }
            let mut score = base_score + portal_semantic_score;
            score += lifecycle_score_adjustment(tool);
            if informational_prompt {
                score += informational_safety_bonus(tool);
            }
            if score > 0.0 {
                scored_tools.push(ScoredCatalogTool {
                    score,
                    sequence: skill.sequence_id.unwrap_or(skill_index as i32).max(0) as usize,
                    cost_rank: cost_rank(tool.cost_tier.as_deref()),
                    latency_ms: tool.estimated_latency_ms.unwrap_or(u64::MAX),
                    tool_name: tool.name.clone(),
                    tool_ref: tool
                        .stable_tool_ref
                        .or(tool.tool_id)
                        .unwrap_or_else(Uuid::now_v7),
                    diagnostic: tool_diagnostic(
                        skill,
                        tool,
                        true,
                        "selected".to_string(),
                        Some(score),
                    ),
                });
            }
        }
    }

    scored_tools.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cost_rank.cmp(&b.cost_rank))
            .then_with(|| a.latency_ms.cmp(&b.latency_ms))
            .then_with(|| a.sequence.cmp(&b.sequence))
            .then_with(|| a.tool_name.cmp(&b.tool_name))
    });

    if scored_tools.is_empty() {
        for (skill_index, skill) in catalog.skills.iter().enumerate() {
            for tool in &skill.tools {
                if tool.name.trim().is_empty() {
                    continue;
                }
                if catalog_tool_hidden_reason(tool).is_some() {
                    continue;
                }
                let score = 0.1
                    + lifecycle_score_adjustment(tool)
                    + if informational_prompt {
                        informational_safety_bonus(tool)
                    } else {
                        0.0
                    };
                if score <= 0.0 {
                    continue;
                }
                scored_tools.push(ScoredCatalogTool {
                    score,
                    sequence: skill.sequence_id.unwrap_or(skill_index as i32).max(0) as usize,
                    cost_rank: cost_rank(tool.cost_tier.as_deref()),
                    latency_ms: tool.estimated_latency_ms.unwrap_or(u64::MAX),
                    tool_name: tool.name.clone(),
                    tool_ref: tool
                        .stable_tool_ref
                        .or(tool.tool_id)
                        .unwrap_or_else(Uuid::now_v7),
                    diagnostic: tool_diagnostic(
                        skill,
                        tool,
                        true,
                        "selected".to_string(),
                        Some(score),
                    ),
                });
            }
            if scored_tools.len() >= limit {
                break;
            }
        }
    }

    let mut tool_names = HashSet::new();
    let mut tool_refs = HashMap::new();
    let mut selected_tools = Vec::new();
    let mut selected_count = 0;
    for scored in scored_tools {
        if selected_count >= limit {
            let mut diagnostic = scored.diagnostic;
            diagnostic.selected = false;
            diagnostic.reason = "not_selected_ranked_below_limit".to_string();
            hidden_tools.push(diagnostic);
            continue;
        }
        if tool_names.insert(scored.tool_name.clone()) {
            tool_refs.insert(scored.tool_name.clone(), scored.tool_ref);
            selected_count += 1;
            selected_tools.push(scored.diagnostic);
        }
    }

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
        tool_refs,
        context,
        selected_tools,
        hidden_tools,
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
    catalog_tool_hidden_reason(tool).is_none()
}

fn catalog_tool_hidden_reason(tool: &CatalogTool) -> Option<String> {
    if tool
        .lifecycle_status
        .as_deref()
        .is_some_and(|status| status.eq_ignore_ascii_case("retired"))
    {
        return Some("lifecycle_retired".to_string());
    }

    let policy = tool.policy.as_ref();
    if policy.and_then(|policy| policy.allowed) == Some(false) {
        return Some(
            policy
                .and_then(|policy| policy.reason.clone())
                .unwrap_or_else(|| "policy_denied".to_string()),
        );
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

    if (destructive || requires_approval) && !approval_configured {
        return Some("approval_required_missing_workflow".to_string());
    }

    None
}

fn tool_diagnostic(
    skill: &CatalogSkill,
    tool: &CatalogTool,
    selected: bool,
    reason: String,
    score: Option<f32>,
) -> CatalogToolDiagnostic {
    CatalogToolDiagnostic {
        skill: skill.name.clone(),
        tool_name: tool.name.clone(),
        selected,
        reason,
        score,
        semantic_score: tool.semantic_score.or(tool.combined_score),
        vector_score: tool.vector_score,
        keyword_score: tool.keyword_score,
        combined_score: tool.combined_score,
        vector_distance: tool.vector_distance,
        semantic_rank: tool.semantic_rank,
        lifecycle_status: tool.lifecycle_status.clone(),
        sensitivity_tier: effective_sensitivity_tier(tool),
        cost_tier: tool.cost_tier.clone(),
        estimated_latency_ms: tool.estimated_latency_ms,
        cache_ttl_seconds: tool.cache_ttl_seconds,
        retry_policy: tool.retry_policy.clone(),
        rate_limit: tool.rate_limit.clone(),
        source_protocol: tool.source_protocol.clone(),
        routing_domain: tool.routing_domain.clone(),
        semantic_namespace: tool.semantic_namespace.clone(),
        read_only: effective_read_only(tool),
        idempotent: effective_idempotent(tool),
        destructive: effective_destructive(tool),
        requires_approval: effective_requires_approval(tool),
        approval_configured: effective_approval_configured(tool),
    }
}

fn lifecycle_score_adjustment(tool: &CatalogTool) -> f32 {
    match tool
        .lifecycle_status
        .as_deref()
        .unwrap_or("active")
        .to_ascii_lowercase()
        .as_str()
    {
        "active" => 0.25,
        "deprecated" => -0.25,
        "retired" => -10.0,
        _ => 0.0,
    }
}

fn informational_safety_bonus(tool: &CatalogTool) -> f32 {
    let read_only = effective_read_only(tool).unwrap_or(false);
    let idempotent = effective_idempotent(tool).unwrap_or(false);
    match (read_only, idempotent) {
        (true, true) => 0.5,
        (true, false) | (false, true) => 0.25,
        (false, false) => 0.0,
    }
}

fn cost_rank(cost_tier: Option<&str>) -> u8 {
    match cost_tier.unwrap_or("medium").to_ascii_lowercase().as_str() {
        "free" | "none" | "low" => 0,
        "medium" => 1,
        "high" => 2,
        "premium" | "expensive" => 3,
        _ => 1,
    }
}

fn prompt_is_informational(prompt: &str) -> bool {
    let terms = tokenize(prompt);
    if terms.is_empty() {
        return true;
    }
    let mutating = [
        "add", "approve", "cancel", "change", "create", "delete", "modify", "record", "remove",
        "send", "submit", "update", "write",
    ];
    let informational = [
        "describe", "explain", "fetch", "find", "get", "how", "list", "lookup", "read", "search",
        "show", "what", "when", "where", "who",
    ];
    informational.iter().any(|term| terms.contains(*term))
        || !mutating.iter().any(|term| terms.contains(*term))
}

fn effective_read_only(tool: &CatalogTool) -> Option<bool> {
    tool.read_only
        .or_else(|| tool.policy.as_ref().and_then(|policy| policy.read_only))
}

fn effective_idempotent(tool: &CatalogTool) -> Option<bool> {
    tool.idempotent
}

fn effective_destructive(tool: &CatalogTool) -> Option<bool> {
    tool.destructive
        .or_else(|| tool.policy.as_ref().and_then(|policy| policy.destructive))
}

fn effective_requires_approval(tool: &CatalogTool) -> Option<bool> {
    tool.requires_approval.or_else(|| {
        tool.policy
            .as_ref()
            .and_then(|policy| policy.requires_approval)
    })
}

fn effective_approval_configured(tool: &CatalogTool) -> Option<bool> {
    tool.policy
        .as_ref()
        .and_then(|policy| policy.approval_configured)
}

fn effective_sensitivity_tier(tool: &CatalogTool) -> Option<String> {
    tool.policy
        .as_ref()
        .and_then(|policy| policy.sensitivity_tier.clone())
        .or_else(|| tool.sensitivity_tier.clone())
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
                "reason": catalog_tool_hidden_reason(tool)
                    .unwrap_or_else(|| "local_policy_guard".to_string()),
                "lifecycleStatus": tool.lifecycle_status.clone(),
                "sensitivityTier": effective_sensitivity_tier(tool),
                "maxSensitivityTier": tool
                    .policy
                    .as_ref()
                    .and_then(|policy| policy.max_sensitivity_tier.clone()),
                "readOnly": effective_read_only(tool),
                "idempotent": effective_idempotent(tool),
                "destructive": effective_destructive(tool),
                "requiresApproval": effective_requires_approval(tool),
                "approvalConfigured": effective_approval_configured(tool),
                "costTier": tool.cost_tier.clone(),
                "estimatedLatencyMs": tool.estimated_latency_ms,
                "cacheTtlSeconds": tool.cache_ttl_seconds,
                "retryPolicy": tool.retry_policy.clone(),
                "rateLimit": tool.rate_limit.clone(),
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
        warn!("Portal catalog is unavailable; failing closed instead of disclosing gateway tools");
        return Vec::new();
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
    let field_score = [
        tool.routing_domain.as_deref(),
        tool.semantic_namespace.as_deref(),
        tool.sensitivity_tier.as_deref(),
        tool.source_protocol.as_deref(),
        tool.lifecycle_status.as_deref(),
        tool.cost_tier.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(|value| keyword_score(query_terms, value))
    .sum::<f32>();
    let keyword_score = tool
        .semantic_keywords
        .iter()
        .map(|value| keyword_score(query_terms, value))
        .sum::<f32>();
    (field_score + keyword_score) * 2.0
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
        tool.semantic_description.as_deref().unwrap_or_default(),
    );
    append_search_text(&mut text, &tool.semantic_keywords.join(" "));
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
        tool.lifecycle_status.as_deref().unwrap_or_default(),
    );
    append_search_text(&mut text, tool.cost_tier.as_deref().unwrap_or_default());
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

    if is_local_cli_provider(&provider_id)
        && !bool_from_env("LIGHT_AGENT_ALLOW_LOCAL_CLI_PROVIDERS", false)
    {
        return Err(RuntimeError::Unsupported(format!(
            "local CLI model provider `{provider_id}` is disabled; run it only in an isolated runner profile or explicitly opt in for local development with LIGHT_AGENT_ALLOW_LOCAL_CLI_PROVIDERS=true"
        )));
    }

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

fn is_local_cli_provider(provider_id: &str) -> bool {
    matches!(
        provider_id,
        "claude-code" | "claudecode" | "gemini-cli" | "geminicli" | "kilo-cli" | "kilocli" | "kilo"
    )
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
    session_id: Uuid,
    authenticated: AuthenticatedRequest,
) {
    let (mut sender, mut receiver) = socket.split();
    let session_id_string = session_id.to_string();
    let policy_digest = |label: &str| {
        sha256_digest(
            format!(
                "{}:{}:{label}",
                state.host_id, authenticated.owner.agent_def_id
            )
            .as_bytes(),
        )
    };
    let durable_policy = PolicySnapshot {
        snapshot_id: session_id,
        definition_digest: policy_digest("definition"),
        product_profile_digest: policy_digest("enterprise"),
        model_digest: policy_digest("model"),
        catalog_digest: policy_digest("catalog"),
        memory_digest: policy_digest("memory"),
        execution_digest: policy_digest("execution"),
        channel_digest: policy_digest("channel"),
        data_boundary_digest: policy_digest(&authenticated.owner.principal_id.to_string()),
        tools: Default::default(),
    };
    if let Err(err) = state
        .domain
        .create_or_resume_session(&SessionSpec {
            host_id: state.host_id,
            session_id: AgentSessionId(session_id),
            principal_id: authenticated.owner.principal_id.to_string(),
            user_id: Some(authenticated.owner.principal_id),
            agent_def_id: authenticated.owner.agent_def_id,
            bank_id: None,
            policy: durable_policy,
            idle_expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            maximum_expires_at: chrono::Utc::now() + chrono::Duration::hours(24),
            resume_handle_digest: sha256_digest(
                format!("{}:{}", session_id, authenticated.owner.principal_id).as_bytes(),
            ),
        })
        .await
    {
        error!("Failed to create or resume durable session: {err}");
        return;
    }

    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::Session {
                session_id: session_id_string.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await;

    // 1. Load or Initialize Session
    let bank_id = session_id; // Using session as bank for simplicity
    if let Err(e) = state
        .memory
        .ensure_session_memory_bank(state.host_id, bank_id, session_id, authenticated.owner)
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

    let mut history = match state
        .memory
        .load_session_history(state.host_id, bank_id, session_id)
        .await
    {
        Ok(history) => history,
        Err(e) => {
            error!(
                "Failed to load session history for host_id={}, bank_id={}, session_id={}: {}",
                state.host_id, bank_id, session_id, e
            );
            if let Ok(payload) = serde_json::to_string(&ServerMessage::Error {
                message: "Failed to load session history".to_string(),
            }) {
                let _ = sender.send(Message::Text(payload.into())).await;
            }
            return;
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
            if client_msg.text.trim().is_empty()
                || client_msg.text.len() > state.limits.max_user_message_bytes
            {
                let message = if client_msg.text.trim().is_empty() {
                    "Message text must not be empty".to_string()
                } else {
                    format!(
                        "Message text exceeds {} bytes",
                        state.limits.max_user_message_bytes
                    )
                };
                if let Ok(payload) = serde_json::to_string(&ServerMessage::Error { message }) {
                    let _ = sender.send(Message::Text(payload.into())).await;
                }
                continue;
            }

            let user_text = client_msg.text.clone();
            let client_message_id = client_msg
                .client_message_id
                .unwrap_or_else(|| Uuid::now_v7().to_string());
            let admitted = match state
                .domain
                .admit_user_turn(
                    state.host_id,
                    AgentSessionId(session_id),
                    &client_message_id,
                    &user_text,
                )
                .await
            {
                Ok(admitted) if admitted.duplicate => {
                    if let Ok(payload) = serde_json::to_string(&ServerMessage::Error {
                        message: "Duplicate client message already admitted".into(),
                    }) {
                        let _ = sender.send(Message::Text(payload.into())).await;
                    }
                    continue;
                }
                Ok(admitted) => admitted,
                Err(err) => {
                    error!("Failed to durably admit agent turn: {err}");
                    if let Ok(payload) = serde_json::to_string(&ServerMessage::Error {
                        message: "Failed to admit turn".into(),
                    }) {
                        let _ = sender.send(Message::Text(payload.into())).await;
                    }
                    continue;
                }
            };
            let dispatch_deadline = tokio::time::Instant::now() + state.limits.turn_timeout;
            let waiter = state.turn_dispatch.register(admitted.turn_id.0).await;
            let turn_resolution = loop {
                // Register the notification future before checking PostgreSQL,
                // closing the activation/check race without query polling.
                let notified = waiter.notified();
                tokio::pin!(notified);
                if let Ok(resolution) = state
                    .domain
                    .resolve_turn_runtime(state.host_id, admitted.turn_id)
                    .await
                {
                    break Some(resolution);
                }
                if tokio::time::timeout_at(dispatch_deadline, &mut notified)
                    .await
                    .is_err()
                {
                    break None;
                }
            };
            state.turn_dispatch.remove(admitted.turn_id.0).await;
            let turn_resolution = match turn_resolution {
                Some(resolution) => resolution,
                None => {
                    let _ = state
                        .domain
                        .fail_turn(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            "turn remained queued past dispatch deadline",
                        )
                        .await;
                    warn!(turn_id=%admitted.turn_id.0, "turn dispatch deadline expired");
                    if let Ok(payload) = serde_json::to_string(&ServerMessage::Error {
                        message: "Turn could not acquire pool capacity".into(),
                    }) {
                        let _ = sender.send(Message::Text(payload.into())).await;
                    }
                    continue;
                }
            };
            let turn_provider_config = ModelProviderConfig {
                provider: turn_resolution.model_provider.clone(),
                model: Some(turn_resolution.model_name.clone()),
                temperature: state.default_temperature,
            };
            let turn_runtime =
                match build_model_provider(&state.runtime_config, &turn_provider_config) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = state
                            .domain
                            .fail_turn(
                                state.host_id,
                                AgentSessionId(session_id),
                                admitted.turn_id,
                                &error.to_string(),
                            )
                            .await;
                        let _ = sender
                            .send(Message::Text(
                                serde_json::to_string(&ServerMessage::Error {
                                    message: "Turn provider/runtime resolution failed".into(),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;
                        continue;
                    }
                };
            history.push(ChatMessage::user(user_text.clone()));
            trim_history(&mut history);

            let turn = run_agent_loop(
                &state,
                history.clone(),
                &authenticated,
                admitted.turn_id.0,
                &turn_resolution.policy_digest,
                &turn_resolution.data_boundary_digest,
                &session_id_string,
                bank_id,
                &turn_resolution,
                &turn_runtime,
            );
            match tokio::time::timeout(state.limits.turn_timeout, turn).await {
                Err(_) => {
                    let _ = state
                        .domain
                        .fail_turn(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            "turn deadline exceeded",
                        )
                        .await;
                    rollback_last_user_message(&mut history, &user_text);
                    let payload = serde_json::to_string(&ServerMessage::Error {
                        message: "Turn deadline exceeded".to_string(),
                    });
                    if let Ok(payload) = payload {
                        let _ = sender.send(Message::Text(payload.into())).await;
                    }
                }
                Ok(Ok((response, actual_tokens))) => {
                    if let Some(text) = response.text {
                        if let Err(err) = state
                            .domain
                            .complete_turn(
                                state.host_id,
                                AgentSessionId(session_id),
                                admitted.turn_id,
                                &text,
                                i64::try_from(actual_tokens).unwrap_or(i64::MAX),
                                0,
                            )
                            .await
                        {
                            error!("Failed to commit durable turn result: {err}");
                            continue;
                        }
                        history.push(ChatMessage::assistant(text.clone()));
                        trim_history(&mut history);

                        if let Err(e) = state
                            .memory
                            .persist_session_history(state.host_id, bank_id, session_id, &history)
                            .await
                        {
                            warn!("Failed to persist session history: {}", e);
                        }
                        if let Err(err) = state
                            .domain
                            .rebuild_history_projection(
                                state.host_id,
                                AgentSessionId(session_id),
                                bank_id,
                            )
                            .await
                        {
                            warn!("Failed to rebuild durable history projection: {err}");
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
                Ok(Err(e)) => {
                    error!("Agent loop error: {}", e);
                    let _ = state
                        .domain
                        .fail_turn(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            &e.to_string(),
                        )
                        .await;
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
    owner: SessionOwner,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO agent_memory_bank_t
         (host_id, bank_id, agent_def_id, user_id, bank_name)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (host_id, bank_id) DO NOTHING",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(owner.agent_def_id)
    .bind(owner.principal_id)
    .bind(format!("session-{session_id}"))
    .execute(db)
    .await
    .context("failed to create session memory bank")?;

    let persisted = load_session_owner(db, host_id, bank_id)
        .await?
        .context("created session memory bank is not visible")?;
    validate_session_owner(persisted, owner)
}

async fn load_session_owner(
    db: &PgPool,
    host_id: Uuid,
    bank_id: Uuid,
) -> Result<Option<SessionOwner>> {
    let row = sqlx::query(
        "SELECT agent_def_id, user_id, active
         FROM agent_memory_bank_t
         WHERE host_id = $1 AND bank_id = $2",
    )
    .bind(host_id)
    .bind(bank_id)
    .fetch_optional(db)
    .await
    .context("failed to load session memory bank owner")?;

    let Some(row) = row else {
        return Ok(None);
    };
    let active: bool = row
        .try_get("active")
        .context("session memory bank active flag is invalid")?;
    if !active {
        bail!("session memory bank is inactive");
    }
    let agent_def_id: Option<Uuid> = row
        .try_get("agent_def_id")
        .context("session memory bank agent owner is invalid")?;
    let principal_id: Option<Uuid> = row
        .try_get("user_id")
        .context("session memory bank principal owner is invalid")?;
    match (principal_id, agent_def_id) {
        (Some(principal_id), Some(agent_def_id)) => Ok(Some(SessionOwner {
            principal_id,
            agent_def_id,
        })),
        _ => bail!("session memory bank has no complete owner binding"),
    }
}

fn validate_session_owner(actual: SessionOwner, expected: SessionOwner) -> Result<()> {
    if actual != expected {
        bail!("session is not owned by the authenticated principal and agent definition");
    }
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
    serde_json::from_value::<Vec<ChatMessage>>(messages)
        .context("session history contains invalid messages")
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

fn validate_json_limits(
    value: &serde_json::Value,
    depth: usize,
    item_count: &mut usize,
    max_depth: usize,
    max_items: usize,
) -> Result<()> {
    if depth > max_depth {
        bail!("tool arguments exceed maximum nesting depth {max_depth}");
    }
    match value {
        serde_json::Value::Array(values) => {
            *item_count = item_count.saturating_add(values.len());
            if *item_count > max_items {
                bail!("tool arguments exceed maximum item count {max_items}");
            }
            for value in values {
                validate_json_limits(value, depth + 1, item_count, max_depth, max_items)?;
            }
        }
        serde_json::Value::Object(values) => {
            *item_count = item_count.saturating_add(values.len());
            if *item_count > max_items {
                bail!("tool arguments exceed maximum item count {max_items}");
            }
            for value in values.values() {
                validate_json_limits(value, depth + 1, item_count, max_depth, max_items)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_json_schema_subset(
    path: &str,
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    if let Some(enum_values) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !enum_values.iter().any(|candidate| candidate == value)
    {
        return Err(format!("{path} is not one of the allowed values"));
    }

    if let Some(schema_type) = schema.get("type").and_then(serde_json::Value::as_str) {
        let type_matches = match schema_type {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "null" => value.is_null(),
            unsupported => {
                return Err(format!("{path} uses unsupported schema type {unsupported}"));
            }
        };
        if !type_matches {
            return Err(format!("{path} must be {schema_type}"));
        }
    }

    if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
        let object = value
            .as_object()
            .ok_or_else(|| format!("{path} required fields need an object"))?;
        for field in required.iter().filter_map(serde_json::Value::as_str) {
            if !object.contains_key(field)
                || object.get(field).is_some_and(serde_json::Value::is_null)
            {
                return Err(format!("{path} is missing required field {field}"));
            }
        }
    }

    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        if schema
            .get("additionalProperties")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        {
            for field in object.keys() {
                if !properties.is_some_and(|properties| properties.contains_key(field)) {
                    return Err(format!("{path} contains unsupported field {field}"));
                }
            }
        }
        if let Some(properties) = properties {
            for (property, property_schema) in properties {
                if let Some(property_value) = object.get(property) {
                    validate_json_schema_subset(
                        &format!("{path}.{property}"),
                        property_schema,
                        property_value,
                    )?;
                }
            }
        }
    }

    if let (Some(items_schema), Some(values)) = (schema.get("items"), value.as_array()) {
        for (index, item) in values.iter().enumerate() {
            validate_json_schema_subset(&format!("{path}[{index}]"), items_schema, item)?;
        }
    }

    Ok(())
}

fn parse_tool_arguments(
    arguments: &str,
    schema: &serde_json::Value,
    limits: &AgentLimits,
) -> Result<serde_json::Value> {
    if arguments.len() > limits.max_tool_argument_bytes {
        bail!(
            "tool arguments exceed {} bytes",
            limits.max_tool_argument_bytes
        );
    }
    let arguments: serde_json::Value =
        serde_json::from_str(arguments).context("tool arguments are not valid JSON")?;
    if !arguments.is_object() {
        bail!("tool arguments must be a JSON object");
    }
    let mut item_count = 0;
    validate_json_limits(
        &arguments,
        0,
        &mut item_count,
        limits.max_output_depth,
        limits.max_output_items,
    )?;
    validate_json_schema_subset("$", schema, &arguments)
        .map_err(|message| anyhow!("tool arguments failed schema validation: {message}"))?;
    Ok(arguments)
}

fn sensitive_key(key: &str) -> bool {
    let key = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    matches!(
        key.as_str(),
        "authorization"
            | "accesstoken"
            | "refreshtoken"
            | "token"
            | "apikey"
            | "password"
            | "passwd"
            | "secret"
            | "clientsecret"
            | "privatekey"
            | "credential"
            | "cookie"
            | "setcookie"
    ) || key.ends_with("token")
        || key.ends_with("secret")
        || key.ends_with("password")
}

fn redact_and_bound_json(
    value: &serde_json::Value,
    depth: usize,
    item_count: &mut usize,
    limits: &AgentLimits,
    truncated: &mut bool,
) -> serde_json::Value {
    if depth > limits.max_output_depth {
        *truncated = true;
        return serde_json::Value::String("[TRUNCATED: maximum depth]".to_string());
    }
    match value {
        serde_json::Value::Array(values) => {
            let mut output = Vec::new();
            for value in values {
                if *item_count >= limits.max_output_items {
                    *truncated = true;
                    output.push(serde_json::Value::String(
                        "[TRUNCATED: maximum items]".to_string(),
                    ));
                    break;
                }
                *item_count += 1;
                output.push(redact_and_bound_json(
                    value,
                    depth + 1,
                    item_count,
                    limits,
                    truncated,
                ));
            }
            serde_json::Value::Array(output)
        }
        serde_json::Value::Object(values) => {
            let mut output = serde_json::Map::new();
            for (key, value) in values {
                if *item_count >= limits.max_output_items {
                    *truncated = true;
                    output.insert(
                        "_truncated".to_string(),
                        serde_json::Value::String("maximum items".to_string()),
                    );
                    break;
                }
                *item_count += 1;
                let value = if sensitive_key(key) {
                    serde_json::Value::String("<REDACTED>".to_string())
                } else {
                    redact_and_bound_json(value, depth + 1, item_count, limits, truncated)
                };
                output.insert(key.clone(), value);
            }
            serde_json::Value::Object(output)
        }
        value => value.clone(),
    }
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    const MARKER: &str = "\n[TRUNCATED]";
    if max_bytes <= MARKER.len() {
        return (MARKER[..max_bytes].to_string(), true);
    }
    let target = max_bytes.saturating_sub(MARKER.len());
    let mut end = target.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut output = value[..end].to_string();
    output.push_str(MARKER);
    (output, true)
}

fn redact_plain_text(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            let normalized = line.to_ascii_lowercase();
            if [
                "authorization",
                "access_token",
                "accesstoken",
                "refresh_token",
                "api_key",
                "apikey",
                "client_secret",
                "password",
                "private_key",
                "set-cookie",
                "bearer ",
            ]
            .iter()
            .any(|marker| normalized.contains(marker))
            {
                "<REDACTED>".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn bound_untrusted_text(value: &str, limits: &AgentLimits, max_bytes: usize) -> (String, bool) {
    let (redacted, mut truncated) = match serde_json::from_str::<serde_json::Value>(value) {
        Ok(value) => {
            let mut item_count = 0;
            let mut truncated = false;
            let value = redact_and_bound_json(&value, 0, &mut item_count, limits, &mut truncated);
            (
                serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
                truncated,
            )
        }
        Err(_) => (redact_plain_text(value), false),
    };
    let (redacted, size_truncated) = truncate_utf8(&redacted, max_bytes);
    truncated |= size_truncated;
    (redacted, truncated)
}

fn tool_result_message(
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
    is_error: bool,
    truncated: bool,
) -> ChatMessage {
    ChatMessage {
        role: "tool".into(),
        content: serde_json::to_string(&serde_json::json!({
            "tool_call_id": tool_call_id,
            "tool_name": tool_name,
            "content": content,
            "is_error": is_error,
            "truncated": truncated,
            "untrusted": true
        }))
        .unwrap_or_else(|_| "{\"is_error\":true}".to_string()),
    }
}

fn gateway_authorization(
    state: &AgentState,
    authenticated: &AuthenticatedRequest,
    session_id: Uuid,
    turn_id: Uuid,
    policy_digest: &str,
    data_boundary_digest: &str,
    action: Option<(Uuid, Uuid)>,
    tool_alias: Option<&str>,
) -> Result<String> {
    let Some(signer) = state.delegation_signer.as_ref() else {
        return Ok(authenticated.authorization.clone());
    };
    let now = chrono::Utc::now().timestamp();
    let token = signer.mint(DelegationClaims {
        token_id: Uuid::now_v7(),
        kind: if action.is_some() {
            DelegationKind::ToolCall
        } else {
            DelegationKind::ToolsList
        },
        issuer: String::new(),
        audience: "light-gateway".into(),
        caller_subject: authenticated.caller_subject.clone(),
        caller_claims: authenticated.caller_claims.clone(),
        agent_actor: state.service_id.clone(),
        host_id: state.host_id,
        session_id,
        turn_id,
        action_attempt_id: action.map(|value| value.0),
        tool_ref: action.map(|value| value.1),
        tool_alias: tool_alias.map(str::to_string),
        destination: Some("mcp".into()),
        data_boundary_digest: data_boundary_digest.to_string(),
        policy_digest: policy_digest.to_string(),
        replay_id: Uuid::now_v7(),
        issued_at: now,
        expires_at: now + 60,
    })?;
    Ok(format!("Bearer {token}"))
}

async fn run_agent_loop(
    state: &AgentState,
    mut messages: Vec<ChatMessage>,
    authenticated: &AuthenticatedRequest,
    turn_id: Uuid,
    policy_digest: &str,
    data_boundary_digest: &str,
    session_id: &str,
    bank_id: Uuid,
    turn_resolution: &TurnRuntimeResolution,
    turn_runtime: &ModelProviderSelection,
) -> Result<(ChatResponse, u64)> {
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
        let (context_msg, _) = bound_untrusted_text(
            &context_msg,
            &state.limits,
            state.limits.max_tool_output_bytes,
        );
        // Inject as a system hint or prefix to the user message
        if let Some(msg) = messages.last_mut() {
            msg.content = format!("{}\n\n{}", context_msg, msg.content);
        }
    }

    // 2. Discover executable tools from the gateway. The portal catalog only
    // narrows what we expose to the model; gateway remains the execution path.
    let catalog_selection = state
        .catalog_selection_for_turn(turn_resolution, &user_prompt)
        .await;
    if let Some(context) = catalog_selection
        .as_ref()
        .and_then(|selection| selection.context.as_ref())
    {
        if let Some(msg) = messages.last_mut() {
            msg.content = format!("{}\n\n{}", context, msg.content);
        }
    }

    let mut tool_specs: Vec<ToolSpec> = Vec::new();
    let mut accepted_tools = HashMap::new();
    let list_authorization = gateway_authorization(
        state,
        authenticated,
        Uuid::parse_str(session_id)?,
        turn_id,
        policy_digest,
        data_boundary_digest,
        None,
        None,
    )?;
    let mcp_tools = state
        .mcp_client
        .list_tools(Some(&list_authorization))
        .await
        .unwrap_or_else(|e| {
            warn!("Gateway tools/list failed: {}", e);
            Vec::new()
        });
    for t in filter_gateway_tools(mcp_tools, catalog_selection.as_ref()) {
        if t.name.trim().is_empty() || accepted_tools.contains_key(&t.name) {
            continue;
        }
        tool_specs.push(ToolSpec {
            name: t.name.clone(),
            description: t.description.clone(),
            parameters: t.input_schema.clone(),
        });
        accepted_tools.insert(t.name.clone(), t);
    }

    // 3. Main LLM Loop
    let mut final_response = None;
    let mut action_count = 0usize;
    let mut turn_tokens = 0u64;
    for _ in 0..state.limits.max_model_calls {
        let mut response = {
            let request = ChatRequest {
                messages: &messages,
                tools: if tool_specs.is_empty() {
                    None
                } else {
                    Some(&tool_specs)
                },
            };
            turn_runtime
                .provider
                .chat(request, &turn_runtime.model, turn_runtime.temperature)
                .await?
        };

        if let Some(usage) = response.usage.as_ref() {
            turn_tokens = turn_tokens
                .saturating_add(usage.input_tokens.unwrap_or_default())
                .saturating_add(usage.output_tokens.unwrap_or_default());
            if turn_tokens > state.limits.max_turn_tokens {
                bail!(
                    "turn token budget exceeded ({} > {})",
                    turn_tokens,
                    state.limits.max_turn_tokens
                );
            }
        }

        if response.tool_calls.is_empty() {
            if let Some(text) = response.text.take() {
                let (text, _) =
                    bound_untrusted_text(&text, &state.limits, state.limits.max_response_bytes);
                response.text = Some(text);
            }
            final_response = Some(response);
            break;
        }

        let serialized_tool_calls = serde_json::to_string(&response.tool_calls)
            .context("failed to serialize model tool calls")?;
        if serialized_tool_calls.len() > state.limits.max_response_bytes {
            bail!("model tool-call response exceeds configured response limit");
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
            action_count = action_count.saturating_add(1);
            if action_count > state.limits.max_action_calls {
                bail!("turn action limit exceeded");
            }
            let Some(tool) = accepted_tools.get(&tool_call.name) else {
                messages.push(tool_result_message(
                    &tool_call.id,
                    &tool_call.name,
                    "Model requested a tool that was not in the accepted tool set",
                    true,
                    false,
                ));
                continue;
            };
            let args =
                match parse_tool_arguments(&tool_call.arguments, &tool.input_schema, &state.limits)
                {
                    Ok(arguments) => arguments,
                    Err(error) => {
                        messages.push(tool_result_message(
                            &tool_call.id,
                            &tool_call.name,
                            &error.to_string(),
                            true,
                            false,
                        ));
                        continue;
                    }
                };
            let stable_tool_ref = catalog_selection
                .as_ref()
                .and_then(|selection| selection.tool_refs.get(&tool_call.name))
                .copied()
                .context("accepted gateway tool has no stable catalog reference")?;
            let (action_attempt_id, stable_tool_ref) = state
                .domain
                .propose_gateway_action(
                    state.host_id,
                    agent_core::AgentTurnId(turn_id),
                    stable_tool_ref,
                    &tool_call.name,
                    &tool_call.arguments,
                )
                .await?;
            let action_authorization = gateway_authorization(
                state,
                authenticated,
                Uuid::parse_str(session_id)?,
                turn_id,
                policy_digest,
                data_boundary_digest,
                Some((action_attempt_id, stable_tool_ref)),
                Some(&tool_call.name),
            )?;
            match state
                .mcp_client
                .call_tool(Some(&action_authorization), &tool_call.name, args)
                .await
            {
                Ok(result) => {
                    state
                        .domain
                        .accept_gateway_result(
                            state.host_id,
                            agent_core::AgentTurnId(turn_id),
                            action_attempt_id,
                            !result.is_error,
                            serde_json::to_value(&result)?,
                        )
                        .await?;
                    let mut text_result = String::new();
                    for content in result.content {
                        if let McpContent::Text { text } = content {
                            if !text_result.is_empty() {
                                text_result.push('\n');
                            }
                            text_result.push_str(&text);
                        }
                    }
                    let (text_result, truncated) = bound_untrusted_text(
                        &text_result,
                        &state.limits,
                        state.limits.max_tool_output_bytes,
                    );
                    messages.push(tool_result_message(
                        &tool_call.id,
                        &tool_call.name,
                        &text_result,
                        result.is_error,
                        truncated,
                    ));
                }
                Err(e) => {
                    warn!("Tool call failed: {}", e);
                    state
                        .domain
                        .accept_gateway_result(
                            state.host_id,
                            agent_core::AgentTurnId(turn_id),
                            action_attempt_id,
                            false,
                            serde_json::json!({"error": e.to_string()}),
                        )
                        .await?;
                    let (error, truncated) = bound_untrusted_text(
                        &format!("Error: {e}"),
                        &state.limits,
                        state.limits.max_tool_output_bytes,
                    );
                    messages.push(tool_result_message(
                        &tool_call.id,
                        &tool_call.name,
                        &error,
                        true,
                        truncated,
                    ));
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

    Ok((response, turn_tokens))
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
    let limits = AgentLimits::from_env();

    let ca_cert = read_agent_ca_cert_bundle(runtime_config)?;
    let verify_hostname: bool = runtime_config
        .client
        .as_ref()
        .map(|c| c.tls.verify_hostname)
        .unwrap_or(true);
    if !verify_hostname {
        warn!(
            "TLS hostname verification is disabled for the MCP gateway and portal query clients; this weakens server identity validation"
        );
    }

    let mcp_client = McpGatewayClient::with_tls_options_and_response_limit(
        &mcp_gateway_url,
        ca_cert.as_deref(),
        verify_hostname,
        mcp_config.timeout_ms,
        limits.max_gateway_response_bytes,
    )
    .map_err(|e| RuntimeError::Config(format!("failed to build MCP gateway client: {e}")))?;

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:secret@localhost:5432/configserver".to_string());
    let pool = PgPool::connect(&db_url)
        .await
        .map_err(|e| RuntimeError::Config(format!("failed to connect to database: {e}")))?;
    let allow_broad_gateway_token = bool_from_env("LIGHT_AGENT_ALLOW_BROAD_GATEWAY_TOKEN", false);
    let delegation_signer = match std::env::var("LIGHT_AGENT_DELEGATION_SECRET") {
        Ok(secret) if !secret.trim().is_empty() => Some(Arc::new(
            DelegationSigner::new(secret.as_bytes(), "light-agent")
                .map_err(|e| RuntimeError::Config(format!("invalid delegation configuration: {e}")))?,
        )),
        _ if allow_broad_gateway_token => {
            warn!("Broad caller bearer forwarding is enabled for the local compatibility profile");
            None
        }
        _ => return Err(RuntimeError::Config("LIGHT_AGENT_DELEGATION_SECRET is required unless LIGHT_AGENT_ALLOW_BROAD_GATEWAY_TOKEN=true is explicitly set for local compatibility".into())),
    };

    let host_id = required_uuid_env_var("LIGHT_AGENT_HOST_ID")
        .map_err(|e| RuntimeError::Config(e.to_string()))?;
    let agent_def_id =
        optional_uuid_env_var(&["LIGHT_AGENT_AGENT_DEF_ID", "LIGHT_AGENT_API_VERSION_ID"])
            .map_err(|e| RuntimeError::Config(e.to_string()))?
            .ok_or_else(|| {
                RuntimeError::Config(
                    "LIGHT_AGENT_AGENT_DEF_ID or LIGHT_AGENT_API_VERSION_ID is required"
                        .to_string(),
                )
            })?;
    let definition_identity:Option<(i64,String)>=sqlx::query_as("SELECT d.aggregate_version,
            COALESCE(p.policy_digest,'unresolved') FROM agent_definition_t d
            LEFT JOIN agent_policy_snapshot_t p ON p.host_id=d.host_id AND p.policy_snapshot_id=d.policy_snapshot_id
            WHERE d.host_id=$1 AND d.agent_def_id=$2")
        .bind(host_id).bind(agent_def_id).fetch_optional(&pool).await
        .map_err(|e|RuntimeError::Config(format!("failed to resolve Agent pool identity: {e}")))?;
    let (definition_version, policy_digest) =
        definition_identity.unwrap_or((0, "unresolved".into()));
    let security = load_security_runtime(runtime_config, true)?
        .ok_or_else(|| RuntimeError::Config("JWT verification must be enabled".to_string()))?;
    security.bootstrap().await.map_err(|rejection| {
        RuntimeError::Config(format!(
            "failed to bootstrap light-agent JWT verification: {}",
            rejection.message
        ))
    })?;
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
            mcp_config.timeout_ms,
        )
        .map_err(|e| RuntimeError::Config(format!("failed to build portal query client: {e}")))?,
    );

    let catalog_cache_ttl = duration_from_env_seconds(
        "LIGHT_AGENT_CATALOG_CACHE_TTL_SECONDS",
        DEFAULT_CATALOG_CACHE_TTL_SECONDS,
    );
    let mut catalog_stale_on_error = duration_from_env_seconds(
        "LIGHT_AGENT_CATALOG_STALE_ON_ERROR_SECONDS",
        DEFAULT_CATALOG_STALE_ON_ERROR_SECONDS,
    );
    if catalog_stale_on_error < catalog_cache_ttl {
        catalog_stale_on_error = catalog_cache_ttl;
    }
    let catalog_semantic_search_enabled =
        bool_from_env("LIGHT_AGENT_ENABLE_SEMANTIC_CATALOG_SEARCH", false);
    let catalog_semantic_limit = usize_from_env(
        "LIGHT_AGENT_SEMANTIC_CATALOG_LIMIT",
        DEFAULT_SEMANTIC_CATALOG_LIMIT,
        100,
    );

    let domain = AgentRepository::new(pool.clone());
    let turn_dispatch = TurnDispatchCoordinator::new(domain.clone());
    turn_dispatch.spawn(host_id);
    let state = Arc::new(AgentState {
        runtime_config: runtime_config.clone(),
        default_temperature: model_provider_config.temperature,
        mcp_client,
        portal_query_client,
        catalog_cache,
        memory,
        domain,
        turn_dispatch,
        delegation_signer,
        security: Arc::new(security),
        limits,
        host_id,
        agent_def_id,
        definition_version,
        policy_digest,
        service_id: runtime_config.service_identity.service_id.clone(),
        env_tag,
        catalog_cache_ttl,
        catalog_stale_on_error,
        catalog_semantic_search_enabled,
        catalog_semantic_limit,
    });
    state.domain.spawn_result_reconciler();

    if let Err(err) = state.refresh_effective_catalog().await {
        warn!(
            "Initial effective agent catalog refresh failed; continuing with lazy refresh: {err}"
        );
    }

    Ok(state)
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
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
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
        AgentCatalogCache, AgentLimits, CatalogCacheKey, CatalogSkill, CatalogTool,
        CatalogToolPolicy, ChatMessage, EffectiveAgentCatalog, MAX_SESSION_MESSAGES,
        ModelProviderConfig, SessionOwner, TurnDispatchCoordinator, agent_ca_cert_path_from_config,
        bind_authenticated_principal, bound_untrusted_text, build_effective_catalog_data,
        choose_model, collect_catalog_tool_names, collect_policy_diagnostics, filter_gateway_tools,
        is_local_cli_provider, normalize_provider_id, parse_tool_arguments,
        rollback_last_user_message, select_catalog_tools, trim_history, validate_session_owner,
    };
    use light_agent::domain::AgentRepository;
    use light_runtime::config::{BootstrapConfig, ClientConfig};
    use light_security::AuthPrincipal;
    use mcp_client::McpTool;
    use sqlx::postgres::PgPoolOptions;
    use std::path::PathBuf;
    use std::time::Duration;
    use uuid::Uuid;

    fn test_limits() -> AgentLimits {
        AgentLimits {
            turn_timeout: Duration::from_secs(1),
            max_model_calls: 2,
            max_action_calls: 2,
            max_user_message_bytes: 1024,
            max_tool_argument_bytes: 1024,
            max_tool_output_bytes: 128,
            max_gateway_response_bytes: 1024,
            max_response_bytes: 128,
            max_output_depth: 4,
            max_output_items: 8,
            max_turn_tokens: 100,
        }
    }

    #[tokio::test]
    async fn turn_dispatch_wakes_only_registered_waiter_without_database_polling() {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .unwrap();
        let coordinator = TurnDispatchCoordinator::new(AgentRepository::new(pool));
        let turn_id = Uuid::now_v7();
        let waiter = coordinator.register(turn_id).await;
        let notified = waiter.notified();
        tokio::pin!(notified);

        coordinator.wake(turn_id).await;

        tokio::time::timeout(Duration::from_millis(100), &mut notified)
            .await
            .unwrap();
        coordinator.remove(turn_id).await;
    }

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
    fn effective_catalog_request_data_adds_semantic_fields_only_when_enabled() {
        let host_id = Uuid::parse_str("019ec75c-72c5-702e-8e42-59dcf1e68cc2").unwrap();
        let agent_def_id = Uuid::parse_str("019ec75c-72c3-71fc-875b-b918d6277702").unwrap();

        let default_data = build_effective_catalog_data(
            host_id,
            agent_def_id,
            1,
            "policy-digest".to_string(),
            "com.networknt.agent-1.0.0",
            None,
            None,
            None,
        );

        assert!(default_data.get("semanticQuery").is_none());
        assert!(default_data.get("semanticLimit").is_none());

        let semantic_data = build_effective_catalog_data(
            host_id,
            agent_def_id,
            1,
            "policy-digest".to_string(),
            "com.networknt.agent-1.0.0",
            Some("dev"),
            Some("  find customer preferences  "),
            Some(25),
        );

        assert_eq!(semantic_data["envTag"], "dev");
        assert_eq!(semantic_data["semanticQuery"], "find customer preferences");
        assert_eq!(semantic_data["semanticLimit"], 25);
    }

    #[tokio::test]
    async fn catalog_cache_marks_stale_after_fresh_ttl() {
        let cache = AgentCatalogCache::new();
        let key = CatalogCacheKey {
            host_id: Uuid::nil(),
            agent_def_id: Uuid::new_v4(),
            definition_version: 1,
            policy_digest: "policy".into(),
            service_id: "agent".into(),
            env_tag: Some("dev".into()),
        };
        cache
            .set(
                key.clone(),
                EffectiveAgentCatalog {
                    catalog_hash: Some("abc".into()),
                    stale: false,
                    ..Default::default()
                },
            )
            .await;

        assert!(
            cache
                .get_fresh(&key, Duration::from_secs(60))
                .await
                .is_some()
        );
        let other = CatalogCacheKey {
            agent_def_id: Uuid::new_v4(),
            ..key.clone()
        };
        assert!(
            cache
                .get_fresh(&other, Duration::from_secs(60))
                .await
                .is_none()
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(
            cache
                .get_fresh(&key, Duration::from_secs(0))
                .await
                .is_none()
        );
        let stale = cache
            .get_stale(&key, Duration::from_secs(60))
            .await
            .expect("stale catalog");
        assert!(stale.stale);
        assert_eq!(stale.catalog_hash.as_deref(), Some("abc"));
    }

    #[test]
    fn provider_id_normalization_accepts_common_spellings() {
        assert_eq!(normalize_provider_id("Azure_OpenAI"), "azure-openai");
        assert_eq!(normalize_provider_id(" gemini cli "), "gemini-cli");
        assert!(is_local_cli_provider("gemini-cli"));
        assert!(!is_local_cli_provider("gemini"));
    }

    #[test]
    fn strict_tool_arguments_reject_malformed_hidden_and_extra_fields() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["accountId"],
            "additionalProperties": false,
            "properties": {
                "accountId": {"type": "string"}
            }
        });
        let limits = test_limits();

        assert!(parse_tool_arguments("not-json", &schema, &limits).is_err());
        assert!(parse_tool_arguments("{}", &schema, &limits).is_err());
        assert!(
            parse_tool_arguments(
                r#"{"accountId":"a","adminOverride":true}"#,
                &schema,
                &limits
            )
            .is_err()
        );
        assert_eq!(
            parse_tool_arguments(r#"{"accountId":"a"}"#, &schema, &limits).unwrap(),
            serde_json::json!({"accountId": "a"})
        );
    }

    #[test]
    fn untrusted_tool_output_is_redacted_and_bounded() {
        let limits = test_limits();
        let value = serde_json::json!({
            "accessToken": "secret-token",
            "result": "x".repeat(256)
        })
        .to_string();

        let (output, truncated) = bound_untrusted_text(&value, &limits, 96);

        assert!(truncated);
        assert!(output.contains("REDACTED"));
        assert!(!output.contains("secret-token"));
        assert!(output.len() <= 96);
    }

    #[test]
    fn principal_binding_rejects_host_or_service_substitution() {
        let host_id = Uuid::new_v4();
        let principal_id = Uuid::new_v4();
        let agent_def_id = Uuid::new_v4();
        let principal = AuthPrincipal {
            user_id: Some(principal_id.to_string()),
            host: Some(host_id.to_string()),
            claims: serde_json::json!({"sid": "com.networknt.agent.account-1.0.0"}),
            ..AuthPrincipal::default()
        };

        let owner = bind_authenticated_principal(
            &principal,
            host_id,
            "com.networknt.agent.account-1.0.0",
            agent_def_id,
        )
        .unwrap();
        assert_eq!(owner.principal_id, principal_id);
        assert_eq!(owner.agent_def_id, agent_def_id);
        assert!(
            bind_authenticated_principal(
                &principal,
                Uuid::new_v4(),
                "com.networknt.agent.account-1.0.0",
                agent_def_id
            )
            .is_err()
        );
        assert!(
            bind_authenticated_principal(&principal, host_id, "other-agent", agent_def_id).is_err()
        );
    }

    #[test]
    fn session_owner_must_match_principal_and_agent() {
        let owner = SessionOwner {
            principal_id: Uuid::new_v4(),
            agent_def_id: Uuid::new_v4(),
        };
        assert!(validate_session_owner(owner, owner).is_ok());
        assert!(
            validate_session_owner(
                SessionOwner {
                    principal_id: Uuid::new_v4(),
                    ..owner
                },
                owner
            )
            .is_err()
        );
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
    fn catalog_selection_excludes_retired_and_penalizes_deprecated_tools() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "offers".into(),
                tools: vec![
                    CatalogTool {
                        name: "old_offer_search".into(),
                        description: Some("Search active offers".into()),
                        lifecycle_status: Some("deprecated".into()),
                        cost_tier: Some("high".into()),
                        estimated_latency_ms: Some(2_000),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    },
                    CatalogTool {
                        name: "new_offer_search".into(),
                        description: Some("Search active offers".into()),
                        lifecycle_status: Some("active".into()),
                        cost_tier: Some("low".into()),
                        estimated_latency_ms: Some(50),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    },
                    CatalogTool {
                        name: "retired_offer_search".into(),
                        description: Some("Search active offers".into()),
                        lifecycle_status: Some("retired".into()),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let selection = select_catalog_tools(&catalog, "search offers", 2);

        assert_eq!(selection.selected_tools[0].tool_name, "new_offer_search");
        assert!(selection.tool_names.contains("old_offer_search"));
        assert!(!selection.tool_names.contains("retired_offer_search"));
        assert!(selection.hidden_tools.iter().any(|tool| {
            tool.tool_name == "retired_offer_search" && tool.reason == "lifecycle_retired"
        }));
    }

    #[test]
    fn catalog_selection_prefers_read_only_idempotent_for_informational_prompt() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "customer".into(),
                tools: vec![
                    CatalogTool {
                        name: "record_customer_lookup".into(),
                        description: Some("Customer lookup".into()),
                        read_only: Some(false),
                        idempotent: Some(false),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    },
                    CatalogTool {
                        name: "get_customer_lookup".into(),
                        description: Some("Customer lookup".into()),
                        read_only: Some(true),
                        idempotent: Some(true),
                        semantic_weight: Some(1.0),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let selection = select_catalog_tools(&catalog, "get customer lookup", 2);

        assert_eq!(selection.selected_tools[0].tool_name, "get_customer_lookup");
        assert_eq!(selection.selected_tools[0].read_only, Some(true));
        assert_eq!(selection.selected_tools[0].idempotent, Some(true));
    }

    #[test]
    fn catalog_selection_uses_portal_combined_score_when_present() {
        let catalog = EffectiveAgentCatalog {
            skills: vec![CatalogSkill {
                name: "operations".into(),
                tools: vec![
                    CatalogTool {
                        name: "semantic_match".into(),
                        description: Some("Profile data".into()),
                        combined_score: Some(0.95),
                        vector_score: Some(0.90),
                        keyword_score: Some(0.05),
                        vector_distance: Some(0.10),
                        semantic_rank: Some(1),
                        retry_policy: Some(serde_json::json!({
                            "enabled": true,
                            "maxAttempts": 2
                        })),
                        rate_limit: Some(serde_json::json!({
                            "bucket": "customer-read"
                        })),
                        ..Default::default()
                    },
                    CatalogTool {
                        name: "semantic_tail".into(),
                        description: Some("Profile data".into()),
                        combined_score: Some(0.05),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }],
            ..Default::default()
        };

        let selection = select_catalog_tools(&catalog, "find preference", 2);

        assert_eq!(selection.selected_tools[0].tool_name, "semantic_match");
        assert_eq!(selection.selected_tools[0].semantic_score, Some(0.95));
        assert_eq!(selection.selected_tools[0].vector_score, Some(0.90));
        assert_eq!(selection.selected_tools[0].keyword_score, Some(0.05));
        assert_eq!(selection.selected_tools[0].vector_distance, Some(0.10));
        assert_eq!(selection.selected_tools[0].semantic_rank, Some(1));
        assert_eq!(
            selection.selected_tools[0].retry_policy,
            Some(serde_json::json!({
                "enabled": true,
                "maxAttempts": 2
            }))
        );
        assert_eq!(
            selection.selected_tools[0].rate_limit,
            Some(serde_json::json!({
                "bucket": "customer-read"
            }))
        );
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
