use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{
        Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use hindsight_client::{HindsightMemory, PgHindsightClient};
use knowledge_client::{
    KnowledgeClient, render_untrusted_evidence, render_untrusted_multi_evidence,
};
use knowledge_core::KnowledgeSearchResponse;
use knowledge_core::RetrieveRequest;
use light_axum::{AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext};
use light_runtime::{
    LifecycleParticipant, LightRuntimeBuilder, MaskSpec, ModuleKind, RuntimeConfig, RuntimeError,
    ShutdownContext, ShutdownWatcher, TracingOptions,
    config::{BootstrapConfig, ClientConfig, PortalRegistryConfig},
    init_tracing,
};
use light_security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SecurityRuntime, load_security_runtime,
    verify_jwt_token,
};
use mcp_client::{McpContent, McpGatewayClient, McpTool};
use model_provider::{
    ChatMessage, ChatRequest, ChatResponse, CompatibleProvider, Provider, ToolSpec,
};
use portal_registry::RegistryHandler;
use serde::{Deserialize, Serialize};
use sqlx::{
    PgPool, Row,
    postgres::{PgListener, PgPoolOptions},
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use url::Url;
use uuid::Uuid;

use a2a_core::{
    AuthorizedInvocation, Direction, InvocationAuthority, OutboundInvocationConstraints,
    TaskSnapshot, TaskState, sign_authorized_invocation, verify_authorized_invocation,
};
use a2a_protocol::{
    A2aOperation, EXTENSIONS_HEADER, ProtocolProfile, VERSION_HEADER, agent_card_etag,
    rewrite_agent_card_url,
};
use a2a_server::{OperationInput, parse_operation};
use agent_core::{AgentSessionId, AgentTurnId, PolicySnapshot, sha256_digest};
use agent_delegation::{DelegationClaims, DelegationKind, DelegationSigner};
use agent_materializer::{MaterializationManifest, ProductProfile};
use coding_agent_runtime::{CodingTurnSpec, ImmutableRepositoryInput};
use execution_client::ExecutionClient;
use light_agent::agent_config::{
    AGENT_CONFIG_FILE, AGENT_CONFIG_MODULE_ID, AgentConfig, AgentExecutionPolicy,
    CodingProfilePolicy,
};
use light_agent::domain::{
    AgentRepository, AgentRuntimeAuthority, EdgeActionSpec, PiCodingRuntime, SessionSpec,
    TurnRuntimeResolution,
};

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const MAX_SESSION_MESSAGES: usize = 40;
const DEFAULT_CATALOG_SELECTION_LIMIT: usize = 12;

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
    fn from_policy(policy: &AgentExecutionPolicy) -> Result<Self, RuntimeError> {
        let bounded = |name: &str, value: usize, maximum: usize| {
            if value == 0 || value > maximum {
                Err(RuntimeError::Config(format!(
                    "agentPolicy.execution.{name} must be between 1 and {maximum}"
                )))
            } else {
                Ok(value)
            }
        };
        if policy.maximum_turn_seconds == 0 {
            return Err(RuntimeError::Config(
                "agentPolicy.execution.maximumTurnSeconds must be positive".into(),
            ));
        }
        if policy.maximum_turn_tokens == 0 || policy.maximum_turn_tokens > 10_000_000 {
            return Err(RuntimeError::Config(
                "agentPolicy.execution.maximumTurnTokens must be between 1 and 10000000".into(),
            ));
        }
        Ok(Self {
            turn_timeout: Duration::from_secs(policy.maximum_turn_seconds),
            max_model_calls: bounded("maximumModelCalls", policy.maximum_model_calls, 100)?,
            max_action_calls: bounded("maximumActionCalls", policy.maximum_action_calls, 1_000)?,
            max_user_message_bytes: bounded(
                "maximumUserMessageBytes",
                policy.maximum_user_message_bytes,
                1024 * 1024,
            )?,
            max_tool_argument_bytes: bounded(
                "maximumToolArgumentBytes",
                policy.maximum_tool_argument_bytes,
                1024 * 1024,
            )?,
            max_tool_output_bytes: bounded(
                "maximumToolOutputBytes",
                policy.maximum_tool_output_bytes,
                4 * 1024 * 1024,
            )?,
            max_gateway_response_bytes: bounded(
                "maximumGatewayResponseBytes",
                policy.maximum_gateway_response_bytes,
                8 * 1024 * 1024,
            )?,
            max_response_bytes: bounded(
                "maximumResponseBytes",
                policy.maximum_response_bytes,
                1024 * 1024,
            )?,
            max_output_depth: bounded("maximumOutputDepth", policy.maximum_output_depth, 64)?,
            max_output_items: bounded("maximumOutputItems", policy.maximum_output_items, 10_000)?,
            max_turn_tokens: policy.maximum_turn_tokens,
        })
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
    subject_type: String,
    groups: Vec<String>,
    organizations: Vec<String>,
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

struct ModelProviderSelection {
    provider: Box<dyn Provider>,
    model: String,
    temperature: f64,
}

fn default_model_provider() -> String {
    "gateway".to_string()
}

fn default_model_temperature() -> f64 {
    0.7
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

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentKnowledgeBinding {
    agent_id: Uuid,
    knowledge_base_id: Uuid,
    #[serde(default)]
    evidence_required: bool,
    #[serde(default)]
    active: bool,
    #[serde(default = "default_knowledge_binding_priority")]
    priority: i32,
}

fn default_knowledge_binding_priority() -> i32 {
    50
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

struct EmbeddedMemoryStore {
    pool: PgPool,
    hindsight: PgHindsightClient,
}

impl EmbeddedMemoryStore {
    fn new(pool: PgPool) -> Self {
        Self {
            hindsight: PgHindsightClient::new(pool.clone()),
            pool,
        }
    }
}

#[async_trait]
impl MemoryStore for EmbeddedMemoryStore {
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

struct AgentState {
    agent_config: AgentConfig,
    system_prompt: String,
    llm_gateway_token: String,
    llm_gateway_client: reqwest::Client,
    policy_snapshot: PolicySnapshot,
    default_temperature: f64,
    mcp_client: McpGatewayClient,
    configured_catalog: Option<EffectiveAgentCatalog>,
    knowledge_bindings: Vec<AgentKnowledgeBinding>,
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
    coding_profile: Option<CodingProfileConfig>,
    personal_profile_digest: Option<String>,
    knowledge_client: Option<KnowledgeClient>,
    native_a2a: Option<NativeA2aRuntime>,
    outbound_a2a: Option<OutboundA2aRuntime>,
}

#[derive(Clone)]
struct OutboundA2aRuntime {
    authorization_key: Arc<Vec<u8>>,
    client: reqwest::Client,
    bindings_by_tool: HashMap<String, light_agent::agent_config::OutboundA2aBinding>,
}

#[derive(Clone)]
struct NativeA2aRuntime {
    repository: agent_store::NativeA2aRepository,
    authorization_key: Arc<Vec<u8>>,
    agent_ref: String,
    binding_id: Uuid,
    publication_id: Uuid,
    policy_digest: String,
    protocol_profile: ProtocolProfile,
    allowed_operations: BTreeSet<A2aOperation>,
    allowed_principal_prefixes: Vec<String>,
    public_url: String,
    agent_card: serde_json::Value,
    revocation_epoch: u64,
    public_skill_mapping: serde_json::Value,
    public_skill_mapping_digest: String,
    artifact_retention_days: u32,
    maximum_artifact_bytes: u64,
    artifact_root_directory: std::path::PathBuf,
}

#[derive(Clone)]
struct CodingProfileConfig {
    product_profile_digest: String,
    repository_uri_prefix: String,
    runtime: PiCodingRuntime,
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
    fn spawn_native_artifact_retention(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Some(native) = state.native_a2a.as_ref() {
                    let now = chrono::Utc::now();
                    match native
                        .repository
                        .expired_artifacts(state.host_id, now, 25)
                        .await
                    {
                        Ok(artifacts) => {
                            for artifact in artifacts {
                                let path = native
                                    .artifact_root_directory
                                    .join(&artifact.object_reference);
                                let deleted = match tokio::fs::remove_file(&path).await {
                                    Ok(()) => true,
                                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                        true
                                    }
                                    Err(error) => {
                                        warn!(artifact_id=%artifact.artifact_id,%error,"native A2A artifact deletion failed");
                                        false
                                    }
                                };
                                if deleted {
                                    let evidence = sha256_digest(
                                        format!(
                                            "native-a2a-delete:{}:{}",
                                            artifact.artifact_id, now
                                        )
                                        .as_bytes(),
                                    );
                                    if let Err(error) = native
                                        .repository
                                        .complete_artifact_deletion(
                                            state.host_id,
                                            artifact.artifact_id,
                                            now,
                                            &evidence,
                                        )
                                        .await
                                    {
                                        warn!(artifact_id=%artifact.artifact_id,%error,"native A2A artifact tombstone failed");
                                    }
                                }
                            }
                        }
                        Err(error) => warn!(%error,"native A2A artifact retention scan failed"),
                    }
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

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
        let catalog = self.effective_catalog_for_turn(turn).await?;
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
        match self.fetch_turn_catalog(turn).await {
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
    ) -> Result<Option<EffectiveAgentCatalog>> {
        if turn.host_id != self.host_id
            || turn.agent_def_id != self.agent_def_id
            || turn.definition_version != self.definition_version
            || turn.policy_digest != self.policy_digest
        {
            bail!("durable turn is not bound to the loaded Agent configuration");
        }
        Ok(self.configured_catalog.clone())
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
        let Some(catalog) = self.configured_catalog.clone() else {
            return Ok(None);
        };
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
        let state = build_agent_state(
            &context.runtime_config,
            self.catalog_cache.clone(),
            &context.lifecycle,
        )
        .await?;
        Ok(agent_router(state))
    }

    fn control_routes(&self) -> &'static [ControlRoute] {
        &[ControlRoute {
            method: "GET",
            path: "/health",
            kind: ControlRouteKind::Liveness,
        }]
    }
}

struct AgentDatabase(PgPool);

#[async_trait::async_trait]
impl LifecycleParticipant for AgentDatabase {
    fn name(&self) -> &'static str {
        "light-agent-database"
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        let budget = context.remaining();
        tokio::time::timeout(budget, self.0.close())
            .await
            .map_err(|_| RuntimeError::ShutdownDeadlineExceeded(budget))?;
        Ok(())
    }
}

fn agent_router(state: Arc<AgentState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/diagnostics/tools", get(tool_diagnostics))
        .route(
            "/knowledge/upload-delegation",
            post(knowledge_upload_delegation),
        )
        .route("/chat", get(ws_handler))
        .route("/a2a/{agent_ref}", post(native_a2a_request))
        .route(
            "/a2a/{agent_ref}/.well-known/agent-card.json",
            get(native_a2a_card),
        )
        .route(
            "/a2a/{agent_ref}/.well-known/agent.json",
            get(native_a2a_card),
        )
        .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeA2aRpcRequest {
    jsonrpc: String,
    id: serde_json::Value,
    #[serde(rename = "method")]
    _method: String,
    #[serde(default)]
    params: serde_json::Value,
}

async fn native_a2a_card(
    State(state): State<Arc<AgentState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(runtime) = state.native_a2a.as_ref() else {
        return native_a2a_error(
            serde_json::Value::Null,
            -32004,
            "Native A2A publication is disabled",
            StatusCode::NOT_FOUND,
        );
    };
    if agent_ref != runtime.agent_ref {
        return native_a2a_error(
            serde_json::Value::Null,
            -32004,
            "Native A2A publication is disabled",
            StatusCode::NOT_FOUND,
        );
    }
    let path = format!("/a2a/{agent_ref}/.well-known/agent-card.json");
    if let Err(error) = runtime.protocol_profile.classify(
        &Method::GET,
        &path,
        headers
            .get(VERSION_HEADER)
            .and_then(|value| value.to_str().ok()),
        headers
            .get(EXTENSIONS_HEADER)
            .and_then(|value| value.to_str().ok()),
        &[],
    ) {
        return Json(error.jsonrpc_response(serde_json::Value::Null)).into_response();
    }
    match rewrite_agent_card_url(&runtime.agent_card, &runtime.public_url) {
        Ok(card) => {
            let etag = agent_card_etag(&card, &runtime.policy_digest, runtime.revocation_epoch);
            if headers
                .get("if-none-match")
                .and_then(|value| value.to_str().ok())
                == Some(etag.as_str())
            {
                return Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header("etag", etag)
                    .body(Body::empty())
                    .expect("valid conditional Agent Card response");
            }
            let mut response = Json(card).into_response();
            response.headers_mut().insert(
                VERSION_HEADER,
                HeaderValue::from_static(runtime.protocol_profile.version.as_str()),
            );
            response.headers_mut().insert(
                "etag",
                HeaderValue::from_str(&etag).expect("SHA-256 ETag is a valid header"),
            );
            response.headers_mut().insert(
                "cache-control",
                HeaderValue::from_static("public, max-age=60, must-revalidate"),
            );
            response
        }
        Err(error) => Json(error.jsonrpc_response(serde_json::Value::Null)).into_response(),
    }
}

async fn native_a2a_request(
    State(state): State<Arc<AgentState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(runtime) = state.native_a2a.as_ref() else {
        return native_a2a_error(
            serde_json::Value::Null,
            -32004,
            "Native A2A publication is disabled",
            StatusCode::NOT_FOUND,
        );
    };
    let classified = match runtime.protocol_profile.classify(
        &Method::POST,
        "/",
        headers
            .get(VERSION_HEADER)
            .and_then(|value| value.to_str().ok()),
        headers
            .get(EXTENSIONS_HEADER)
            .and_then(|value| value.to_str().ok()),
        &body,
    ) {
        Ok(classified) => classified,
        Err(error) => {
            return Json(error.jsonrpc_response(serde_json::Value::Null)).into_response();
        }
    };
    let request = match serde_json::from_slice::<NativeA2aRpcRequest>(&body) {
        Ok(request) if request.jsonrpc == "2.0" => request,
        _ => {
            return native_a2a_error(
                serde_json::Value::Null,
                -32600,
                "Invalid Request",
                StatusCode::BAD_REQUEST,
            );
        }
    };
    let invocation = match verify_native_a2a_context(&headers, &body, runtime) {
        Ok(invocation) => invocation,
        Err(message) => {
            return native_a2a_error(request.id, -32001, message, StatusCode::UNAUTHORIZED);
        }
    };
    let authority = InvocationAuthority {
        binding_id: runtime.binding_id,
        publication_id: runtime.publication_id,
        policy_digest: runtime.policy_digest.clone(),
        directions: [Direction::Inbound].into_iter().collect(),
        operations: runtime.allowed_operations.clone(),
        principal_prefixes: runtime.allowed_principal_prefixes.clone(),
    };
    if agent_ref != runtime.agent_ref
        || invocation.target_agent_ref != runtime.agent_ref
        || invocation.direction != Direction::Inbound
        || invocation.host_id != state.host_id
        || invocation.request_digest != sha256_digest(&body)
        || authority
            .authorize(&invocation, classified.operation)
            .is_err()
    {
        return native_a2a_error(
            request.id,
            -32003,
            "Native A2A binding denied",
            StatusCode::FORBIDDEN,
        );
    }

    let operation_input = match parse_operation(
        classified.operation,
        classified.version,
        request.params,
        state.limits.max_user_message_bytes,
    ) {
        Ok(value) => value,
        Err(error) => {
            return native_a2a_error(request.id, -32602, &error.to_string(), StatusCode::OK);
        }
    };

    match (classified.operation, operation_input) {
        (
            A2aOperation::SendMessage | A2aOperation::SendStreamingMessage,
            OperationInput::Send(params),
        ) => {
            let selected_skill = params
                .metadata
                .get("skillId")
                .and_then(serde_json::Value::as_str);
            if selected_skill.is_some_and(|requested| {
                !runtime
                    .public_skill_mapping
                    .as_array()
                    .is_some_and(|mappings| {
                        mappings.iter().any(|mapping| {
                            mapping
                                .get("publicationAlias")
                                .and_then(serde_json::Value::as_str)
                                == Some(requested)
                        })
                    })
            }) {
                return native_a2a_error(
                    request.id,
                    -32003,
                    "Requested skill is not published for this native Agent",
                    StatusCode::FORBIDDEN,
                );
            }
            if params.task_id.is_some() {
                return native_a2a_error(
                    request.id,
                    -32004,
                    "Task continuation is not available for a terminal native Agent turn",
                    StatusCode::OK,
                );
            }
            let context_id = params.context_id.unwrap_or_else(|| {
                stable_native_a2a_id(
                    "context",
                    runtime.publication_id,
                    &invocation.principal_subject,
                    &params.message_id,
                )
            });
            let task_id = stable_native_a2a_id(
                "task",
                runtime.publication_id,
                &invocation.principal_subject,
                &params.message_id,
            );
            let principal_id = stable_native_a2a_id(
                "principal",
                runtime.publication_id,
                &invocation.principal_subject,
                &invocation.principal_subject,
            );
            let now = chrono::Utc::now();
            let session_policy = &state.agent_config.agent_policy.session;
            let idle_expires_at = now
                + chrono::Duration::seconds(
                    i64::try_from(session_policy.idle_seconds).unwrap_or(i64::MAX),
                );
            let maximum_expires_at = now
                + chrono::Duration::seconds(
                    i64::try_from(session_policy.maximum_seconds).unwrap_or(i64::MAX),
                );
            if let Err(error) = state
                .domain
                .create_or_resume_session(&SessionSpec {
                    host_id: state.host_id,
                    session_id: AgentSessionId(context_id),
                    principal_id: invocation.principal_subject.clone(),
                    user_id: Some(principal_id),
                    agent_def_id: state.agent_def_id,
                    definition_version: state.definition_version,
                    model_provider: state.agent_config.agent_policy.model.provider.clone(),
                    model_name: state.agent_config.agent_policy.model.alias.clone(),
                    maximum_active_sessions: session_policy.maximum_active_sessions,
                    bank_id: None,
                    policy: state.policy_snapshot.clone(),
                    idle_expires_at: idle_expires_at.min(maximum_expires_at),
                    maximum_expires_at,
                    resume_handle_digest: sha256_digest(
                        format!("a2a:{context_id}:{}", invocation.principal_subject).as_bytes(),
                    ),
                })
                .await
            {
                return native_a2a_error(
                    request.id,
                    -32010,
                    &error.to_string(),
                    StatusCode::CONFLICT,
                );
            }
            if let Err(error) = state
                .memory
                .ensure_session_memory_bank(
                    state.host_id,
                    context_id,
                    context_id,
                    SessionOwner {
                        principal_id,
                        agent_def_id: state.agent_def_id,
                    },
                )
                .await
            {
                return native_a2a_error(
                    request.id,
                    -32010,
                    &error.to_string(),
                    StatusCode::CONFLICT,
                );
            }
            let admitted = match state
                .domain
                .admit_user_turn(
                    state.host_id,
                    AgentSessionId(context_id),
                    &params.message_id,
                    &params.text,
                    &state.agent_config.agent_policy.model.provider,
                    &state.agent_config.agent_policy.model.alias,
                    session_policy.maximum_queued_turns,
                    state.agent_config.agent_policy.model.maximum_tokens,
                )
                .await
            {
                Ok(admitted) => admitted,
                Err(error) => {
                    return native_a2a_error(
                        request.id,
                        -32010,
                        &error.to_string(),
                        StatusCode::CONFLICT,
                    );
                }
            };
            let snapshot = match runtime
                .repository
                .bind(&agent_store::NativeTaskAdmission {
                    session_id: context_id,
                    turn_id: admitted.turn_id.0,
                    task_id,
                    context_id,
                    agent_def_id: state.agent_def_id,
                    message_id: params.message_id.clone(),
                    skill_mapping: runtime.public_skill_mapping.clone(),
                    skill_mapping_digest: runtime.public_skill_mapping_digest.clone(),
                    invocation: invocation.clone(),
                })
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return native_a2a_error(
                        request.id,
                        -32010,
                        &error.to_string(),
                        StatusCode::CONFLICT,
                    );
                }
            };
            if !admitted.duplicate {
                let execution_state = Arc::clone(&state);
                let execution_invocation = invocation.clone();
                tokio::spawn(async move {
                    if let Err(error) = execute_native_a2a_turn(
                        execution_state,
                        execution_invocation,
                        context_id,
                        admitted.turn_id,
                        task_id,
                        params.text,
                    )
                    .await
                    {
                        warn!(%error, %task_id, "native A2A Agent turn failed");
                    }
                });
            }
            let access = owned_native_task_access(&state, runtime, &invocation, task_id);
            if classified.operation == A2aOperation::SendStreamingMessage {
                native_a2a_task_stream(
                    request.id,
                    runtime.repository.clone(),
                    access,
                    classified.version,
                    Some(snapshot),
                    true,
                    state.limits.turn_timeout,
                    params.history_length,
                )
            } else {
                let snapshot = if params.return_immediately {
                    snapshot
                } else {
                    match wait_native_a2a_task(
                        &runtime.repository,
                        &access,
                        state.limits.turn_timeout + Duration::from_secs(5),
                    )
                    .await
                    {
                        Ok(snapshot) => snapshot,
                        Err(error) => {
                            return native_a2a_error(request.id, -32603, &error, StatusCode::OK);
                        }
                    }
                };
                native_a2a_result(
                    request.id,
                    a2a_server::send_result_with_history(
                        &snapshot,
                        classified.version,
                        chrono::Utc::now(),
                        params.history_length,
                    ),
                )
            }
        }
        (
            A2aOperation::GetTask | A2aOperation::CancelTask | A2aOperation::SubscribeToTask,
            OperationInput::Task(params),
        ) => {
            let owned = owned_native_task_access(&state, runtime, &invocation, params.task_id);
            let access = owned.as_borrowed();
            if classified.operation == A2aOperation::CancelTask {
                let (session_id, turn_id) = match runtime.repository.resolve_turn(&access).await {
                    Ok(ids) => ids,
                    Err(_) => {
                        return native_a2a_error(
                            request.id,
                            -32001,
                            "Task not found",
                            StatusCode::NOT_FOUND,
                        );
                    }
                };
                if let Err(_) = state
                    .domain
                    .cancel_turn(
                        state.host_id,
                        AgentSessionId(session_id),
                        AgentTurnId(turn_id),
                        &invocation.principal_subject,
                    )
                    .await
                {
                    return native_a2a_error(
                        request.id,
                        -32002,
                        "Task cannot be canceled",
                        StatusCode::CONFLICT,
                    );
                }
                match runtime.repository.mark_canceled(&access).await {
                    Ok(snapshot) => native_a2a_result(
                        request.id,
                        a2a_server::task_value(&snapshot, classified.version, chrono::Utc::now()),
                    ),
                    Err(_) => native_a2a_error(
                        request.id,
                        -32002,
                        "Task cannot be canceled",
                        StatusCode::CONFLICT,
                    ),
                }
            } else if classified.operation == A2aOperation::SubscribeToTask {
                match runtime.repository.get(&access).await {
                    Ok(snapshot) if snapshot.state.terminal() => native_a2a_error(
                        request.id,
                        -32004,
                        "This operation is not supported",
                        StatusCode::OK,
                    ),
                    Ok(snapshot) => native_a2a_task_stream(
                        request.id,
                        runtime.repository.clone(),
                        owned,
                        classified.version,
                        Some(snapshot),
                        false,
                        state.limits.turn_timeout,
                        None,
                    ),
                    Err(_) => native_a2a_error(
                        request.id,
                        -32001,
                        "Task not found",
                        StatusCode::NOT_FOUND,
                    ),
                }
            } else {
                match runtime.repository.get(&access).await {
                    Ok(snapshot) => native_a2a_result(
                        request.id,
                        a2a_server::task_value_with_history(
                            &snapshot,
                            classified.version,
                            chrono::Utc::now(),
                            params.history_length,
                        ),
                    ),
                    Err(_) => native_a2a_error(
                        request.id,
                        -32001,
                        "Task not found",
                        StatusCode::NOT_FOUND,
                    ),
                }
            }
        }
        (A2aOperation::ListTasks, OperationInput::List(params)) => {
            let cursor = match a2a_server::decode_page_token(params.page_token.as_deref()) {
                Ok(cursor) => cursor,
                Err(_) => {
                    return native_a2a_error(request.id, -32602, "Invalid params", StatusCode::OK);
                }
            };
            let page = match runtime
                .repository
                .list(&agent_store::NativeTaskListAccess {
                    host_id: state.host_id,
                    principal_subject: &invocation.principal_subject,
                    target_agent_id: state.agent_def_id,
                    publication_id: runtime.publication_id,
                    context_id: params.context_id,
                    status: params.status,
                    status_timestamp_after: params.status_timestamp_after,
                    cursor: cursor.map(|cursor| (cursor.created_at, cursor.task_id)),
                    limit: params.page_size,
                })
                .await
            {
                Ok(page) => page,
                Err(error) => {
                    return native_a2a_error(
                        request.id,
                        -32003,
                        &error.to_string(),
                        StatusCode::OK,
                    );
                }
            };
            let next_page_token = match a2a_server::encode_page_token(page.next_cursor.map(
                |(created_at, task_id)| a2a_server::PageCursor {
                    created_at,
                    task_id,
                },
            )) {
                Ok(token) => token,
                Err(_) => {
                    return native_a2a_error(request.id, -32603, "Internal error", StatusCode::OK);
                }
            };
            let total_size = page.total_size;
            let tasks = page.tasks;
            let tasks = if params.include_artifacts {
                tasks
            } else {
                tasks
                    .into_iter()
                    .map(|mut task| {
                        task.artifacts.clear();
                        task
                    })
                    .collect()
            };
            native_a2a_result(
                request.id,
                a2a_server::list_result_with_history(
                    &tasks,
                    classified.version,
                    params.page_size,
                    total_size,
                    next_page_token.as_deref(),
                    chrono::Utc::now(),
                    params.history_length,
                    params.include_artifacts,
                ),
            )
        }
        _ => native_a2a_error(request.id, -32601, "Method not found", StatusCode::OK),
    }
}

fn verify_native_a2a_context(
    headers: &HeaderMap,
    body: &[u8],
    runtime: &NativeA2aRuntime,
) -> Result<AuthorizedInvocation, &'static str> {
    let context = headers
        .get("x-light-a2a-context")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context")?;
    let signature = headers
        .get("x-light-a2a-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context signature")?;
    verify_authorized_invocation(
        context,
        signature,
        body,
        &runtime.authorization_key,
        "light-agent",
        chrono::Utc::now(),
    )
    .map_err(|_| "Authorized context rejected")
}

#[derive(Clone)]
struct NativeTaskAccessOwned {
    host_id: Uuid,
    task_id: Uuid,
    principal_subject: String,
    target_agent_id: Uuid,
    publication_id: Uuid,
}

impl NativeTaskAccessOwned {
    fn as_borrowed(&self) -> agent_store::NativeTaskAccess<'_> {
        agent_store::NativeTaskAccess {
            host_id: self.host_id,
            task_id: self.task_id,
            principal_subject: &self.principal_subject,
            target_agent_id: self.target_agent_id,
            publication_id: self.publication_id,
        }
    }
}

fn owned_native_task_access(
    state: &AgentState,
    runtime: &NativeA2aRuntime,
    invocation: &AuthorizedInvocation,
    task_id: Uuid,
) -> NativeTaskAccessOwned {
    NativeTaskAccessOwned {
        host_id: state.host_id,
        task_id,
        principal_subject: invocation.principal_subject.clone(),
        target_agent_id: state.agent_def_id,
        publication_id: runtime.publication_id,
    }
}

fn stable_native_a2a_id(
    kind: &str,
    publication_id: Uuid,
    principal_subject: &str,
    message_id: &str,
) -> Uuid {
    let digest = sha256_digest(
        format!("native-a2a:{kind}:{publication_id}:{principal_subject}:{message_id}").as_bytes(),
    );
    let hex = digest
        .strip_prefix("sha256:")
        .expect("SHA-256 digest prefix");
    let mut bytes = [0u8; 16];
    for (index, value) in bytes.iter_mut().enumerate() {
        *value = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16)
            .expect("SHA-256 digest is hexadecimal");
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn outbound_a2a_tool_name(agent_ref: &str) -> String {
    format!(
        "a2a__{}__send",
        agent_ref
            .chars()
            .map(|value| if value.is_ascii_alphanumeric() {
                value
            } else {
                '_'
            })
            .collect::<String>()
    )
}

async fn invoke_outbound_a2a(
    state: &AgentState,
    authenticated: &AuthenticatedRequest,
    binding: &light_agent::agent_config::OutboundA2aBinding,
    message: &str,
    skill_id: Option<&str>,
    message_id: &str,
    data_boundary_digest: &str,
) -> Result<serde_json::Value> {
    let runtime = state
        .outbound_a2a
        .as_ref()
        .context("outbound A2A runtime is unavailable")?;
    let part = if binding.protocol_version == "0.3" {
        serde_json::json!({"kind":"text","text":message})
    } else {
        serde_json::json!({"text":message})
    };
    let mut metadata = serde_json::Map::new();
    if let Some(skill_id) = skill_id {
        metadata.insert("skillId".into(), serde_json::Value::String(skill_id.into()));
    }
    let message_value = if binding.protocol_version == "0.3" {
        serde_json::json!({
            "kind":"message","role":"user","messageId":message_id,"parts":[part],
            "metadata":metadata
        })
    } else {
        serde_json::json!({
            "role":"ROLE_USER","messageId":message_id,"parts":[part],"metadata":metadata
        })
    };
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc":"2.0","id":message_id,"method":"message/send",
        "params":{"message":message_value}
    }))?;
    if body.len() as u64 > binding.maximum_budget_units {
        bail!("outbound A2A request exceeds its published budget");
    }
    let now = chrono::Utc::now();
    let expires_at = now + chrono::Duration::minutes(2);
    let caller_agent_ref = state.service_id.clone();
    let invocation = AuthorizedInvocation {
        host_id: state.host_id,
        audience: "light-a2a".into(),
        principal_subject: authenticated.caller_subject.clone(),
        caller_agent_ref: caller_agent_ref.clone(),
        target_agent_ref: binding.agent_ref.clone(),
        binding_id: binding.binding_id,
        policy_digest: binding.policy_digest.clone(),
        publication_id: binding.publication_id,
        direction: Direction::Outbound,
        idempotency_key: message_id.to_string(),
        request_digest: a2a_core::request_digest(&body),
        outbound: Some(OutboundInvocationConstraints {
            delegation_id: Uuid::now_v7(),
            environment: state.env_tag.clone().unwrap_or_default(),
            data_boundary_digest: data_boundary_digest.to_string(),
            delegation_depth: 1,
            maximum_delegation_depth: binding.maximum_delegation_depth,
            remaining_budget_units: binding.maximum_budget_units,
            deadline: expires_at,
            call_chain: vec![caller_agent_ref],
            skill_id: skill_id.map(str::to_owned),
        }),
        issued_at: now,
        expires_at,
    };
    invocation.validate("light-a2a", now)?;
    let (context, signature) =
        sign_authorized_invocation(&invocation, &body, &runtime.authorization_key)?;
    let response = runtime
        .client
        .post(&binding.gateway_uri)
        .bearer_auth(&state.llm_gateway_token)
        .header("content-type", "application/json")
        .header(VERSION_HEADER, &binding.protocol_version)
        .header("x-light-a2a-context", context)
        .header("x-light-a2a-signature", signature)
        .body(body)
        .send()
        .await
        .context("invoke governed outbound A2A binding")?;
    let status = response.status();
    let bytes = response.bytes().await?;
    if bytes.len() > state.limits.max_tool_output_bytes {
        bail!("outbound A2A response exceeds the Agent tool-output limit");
    }
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).context("outbound A2A response is not JSON")?;
    if !status.is_success() || value.get("error").is_some() {
        bail!("outbound A2A invocation failed: {value}");
    }
    Ok(value
        .get("result")
        .cloned()
        .unwrap_or(serde_json::Value::Null))
}

async fn wait_native_a2a_task(
    repository: &agent_store::NativeA2aRepository,
    access: &NativeTaskAccessOwned,
    maximum: Duration,
) -> Result<TaskSnapshot, String> {
    let deadline = tokio::time::Instant::now() + maximum;
    loop {
        let snapshot = repository
            .get(&access.as_borrowed())
            .await
            .map_err(|error| error.to_string())?;
        if snapshot.state.terminal()
            || matches!(
                snapshot.state,
                TaskState::InputRequired | TaskState::AuthRequired
            )
        {
            return Ok(snapshot);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(snapshot);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn native_a2a_task_stream(
    id: serde_json::Value,
    repository: agent_store::NativeA2aRepository,
    access: NativeTaskAccessOwned,
    version: a2a_protocol::ProtocolVersion,
    initial: Option<TaskSnapshot>,
    emit_initial_task: bool,
    maximum: Duration,
    history_length: Option<usize>,
) -> Response {
    struct StreamState {
        id: serde_json::Value,
        repository: agent_store::NativeA2aRepository,
        access: NativeTaskAccessOwned,
        version: a2a_protocol::ProtocolVersion,
        next: Option<TaskSnapshot>,
        last_state: Option<TaskState>,
        emit_initial_task: bool,
        deadline: tokio::time::Instant,
        done: bool,
        history_length: Option<usize>,
    }
    let stream = futures_util::stream::unfold(
        StreamState {
            id,
            repository,
            access,
            version,
            next: initial,
            last_state: None,
            emit_initial_task,
            deadline: tokio::time::Instant::now() + maximum + Duration::from_secs(5),
            done: false,
            history_length,
        },
        |mut state| async move {
            if state.done {
                return None;
            }
            loop {
                let snapshot = match state.next.take() {
                    Some(snapshot) => snapshot,
                    None => match state.repository.get(&state.access.as_borrowed()).await {
                        Ok(snapshot) => snapshot,
                        Err(_) => {
                            state.done = true;
                            let frame = serde_json::json!({
                                "jsonrpc":"2.0","id":state.id,
                                "error":{"code":-32001,"message":"Task not found"}
                            });
                            return Some((
                                Ok::<_, std::convert::Infallible>(Bytes::from(format!(
                                    "data: {frame}\n\n"
                                ))),
                                state,
                            ));
                        }
                    },
                };
                let first = state.last_state.is_none() && state.emit_initial_task;
                if first || state.last_state != Some(snapshot.state) {
                    state.last_state = Some(snapshot.state);
                    state.done = snapshot.state.terminal()
                        || matches!(
                            snapshot.state,
                            TaskState::InputRequired | TaskState::AuthRequired
                        );
                    let result = if first {
                        a2a_server::send_result_with_history(
                            &snapshot,
                            state.version,
                            chrono::Utc::now(),
                            state.history_length,
                        )
                    } else {
                        a2a_server::status_stream_result(
                            &snapshot,
                            state.version,
                            chrono::Utc::now(),
                        )
                    };
                    let frame = serde_json::json!({"jsonrpc":"2.0","id":state.id,"result":result});
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(format!(
                            "data: {frame}\n\n"
                        ))),
                        state,
                    ));
                }
                if tokio::time::Instant::now() >= state.deadline {
                    state.done = true;
                    let frame = serde_json::json!({
                        "jsonrpc":"2.0","id":state.id,
                        "error":{"code":-32010,"message":"native A2A stream deadline exceeded"}
                    });
                    return Some((
                        Ok::<_, std::convert::Infallible>(Bytes::from(format!(
                            "data: {frame}\n\n"
                        ))),
                        state,
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .expect("valid native A2A SSE response")
}

async fn execute_native_a2a_turn(
    state: Arc<AgentState>,
    invocation: AuthorizedInvocation,
    session_id: Uuid,
    turn_id: AgentTurnId,
    task_id: Uuid,
    text: String,
) -> Result<()> {
    let waiter = state.turn_dispatch.register(turn_id.0).await;
    let deadline = tokio::time::Instant::now() + state.limits.turn_timeout;
    let resolution = loop {
        let notified = waiter.notified();
        tokio::pin!(notified);
        if let Ok(value) = state
            .domain
            .resolve_turn_runtime(state.host_id, turn_id)
            .await
        {
            break value;
        }
        if tokio::time::timeout_at(deadline, &mut notified)
            .await
            .is_err()
        {
            state.turn_dispatch.remove(turn_id.0).await;
            state
                .domain
                .fail_turn(
                    state.host_id,
                    AgentSessionId(session_id),
                    turn_id,
                    "native A2A turn remained queued past dispatch deadline",
                )
                .await?;
            bail!("native A2A turn was not activated before its deadline");
        }
    };
    state.turn_dispatch.remove(turn_id.0).await;
    let provider_config = ModelProviderConfig {
        provider: resolution.model_provider.clone(),
        model: Some(resolution.model_name.clone()),
        temperature: state.default_temperature,
    };
    let runtime = build_model_provider(
        &state.agent_config,
        &provider_config,
        &state.llm_gateway_token,
        &state.llm_gateway_client,
    )?;
    let authenticated = AuthenticatedRequest {
        authorization: String::new(),
        owner: SessionOwner {
            principal_id: stable_native_a2a_id(
                "principal",
                invocation.publication_id,
                &invocation.principal_subject,
                &invocation.principal_subject,
            ),
            agent_def_id: state.agent_def_id,
        },
        caller_claims: serde_json::json!({"a2a":true}),
        caller_subject: invocation.principal_subject.clone(),
        subject_type: "a2a-principal".into(),
        groups: Vec::new(),
        organizations: Vec::new(),
    };
    let outcome = tokio::time::timeout(
        state.limits.turn_timeout,
        run_agent_loop(
            &state,
            vec![ChatMessage::user(text)],
            &authenticated,
            turn_id.0,
            &resolution.policy_digest,
            &resolution.data_boundary_digest,
            &session_id.to_string(),
            session_id,
            &resolution,
            &runtime,
        ),
    )
    .await;
    match outcome {
        Ok(Ok((response, usage, knowledge_evidence))) => {
            let text = response.text.unwrap_or_default();
            state
                .domain
                .complete_turn(
                    state.host_id,
                    AgentSessionId(session_id),
                    turn_id,
                    &text,
                    usage
                        .complete
                        .then(|| i64::try_from(usage.input_tokens).unwrap_or(i64::MAX)),
                    usage
                        .complete
                        .then(|| i64::try_from(usage.output_tokens).unwrap_or(i64::MAX)),
                    knowledge_evidence.as_ref(),
                )
                .await?;
            if !text.is_empty()
                && let Some(native) = state.native_a2a.as_ref()
                && text.len() as u64 <= native.maximum_artifact_bytes
            {
                let access = agent_store::NativeTaskAccess {
                    host_id: state.host_id,
                    task_id,
                    principal_subject: &invocation.principal_subject,
                    target_agent_id: state.agent_def_id,
                    publication_id: native.publication_id,
                };
                let snapshot = native.repository.get(&access).await?;
                if snapshot.state == TaskState::Completed {
                    let artifact_id = stable_native_a2a_id(
                        "artifact",
                        native.publication_id,
                        &invocation.principal_subject,
                        &task_id.to_string(),
                    );
                    let content_digest = sha256_digest(text.as_bytes());
                    let provenance_digest = sha256_digest(
                        format!("native-a2a-result:{session_id}:{}:{task_id}", turn_id.0)
                            .as_bytes(),
                    );
                    let object_reference = format!("{}/{}/{}", state.host_id, task_id, artifact_id);
                    let artifact_path = native.artifact_root_directory.join(&object_reference);
                    let parent = artifact_path
                        .parent()
                        .context("native A2A artifact path has no parent")?;
                    tokio::fs::create_dir_all(parent)
                        .await
                        .context("create native A2A artifact directory")?;
                    tokio::fs::write(&artifact_path, text.as_bytes())
                        .await
                        .context("write native A2A managed artifact")?;
                    native
                        .repository
                        .register_artifact(
                            &access,
                            &agent_store::NativeArtifactAdmission {
                                artifact_id,
                                logical_name: "agent-response.txt",
                                media_type: "text/plain",
                                size_bytes: text.len() as u64,
                                content_digest: &content_digest,
                                object_reference: &object_reference,
                                provenance_digest: &provenance_digest,
                                retain_until: chrono::Utc::now()
                                    + chrono::Duration::days(i64::from(
                                        native.artifact_retention_days,
                                    )),
                            },
                        )
                        .await
                        .context("persist native A2A result artifact")?;
                }
            }
            Ok(())
        }
        Ok(Err(error)) => {
            state
                .domain
                .fail_turn_after_model_dispatch(
                    state.host_id,
                    AgentSessionId(session_id),
                    turn_id,
                    &error.to_string(),
                )
                .await?;
            Err(error)
        }
        Err(_) => {
            state
                .domain
                .fail_turn_after_model_dispatch(
                    state.host_id,
                    AgentSessionId(session_id),
                    turn_id,
                    "native A2A turn deadline exceeded",
                )
                .await?;
            bail!("native A2A turn deadline exceeded")
        }
    }
}

fn native_a2a_result(id: serde_json::Value, result: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
        .into_response()
}

fn native_a2a_error(
    id: serde_json::Value,
    code: i64,
    message: &str,
    _status: StatusCode,
) -> Response {
    (
        StatusCode::OK,
        Json(serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct KnowledgeUploadDelegationResponse {
    token: String,
    expires_at: i64,
}

async fn knowledge_upload_delegation(
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> Response {
    let authenticated = match authenticate_request(&headers, &state).await {
        Ok(authenticated) => authenticated,
        Err(rejection) => return rejection_response(rejection),
    };
    let session_id = Uuid::now_v7();
    let turn_id = Uuid::now_v7();
    let boundary = sha256_digest(
        format!(
            "knowledge-upload|{}|{}|{}",
            state.host_id, authenticated.owner.agent_def_id, authenticated.owner.principal_id
        )
        .as_bytes(),
    );
    match knowledge_authorization(
        &state,
        &authenticated,
        DelegationKind::KnowledgeUpload,
        session_id,
        turn_id,
        &state.policy_digest,
        &boundary,
    ) {
        Ok(authorization) => {
            let now = chrono::Utc::now().timestamp();
            let token = authorization
                .strip_prefix("Bearer ")
                .unwrap_or(&authorization)
                .to_string();
            (
                StatusCode::OK,
                Json(KnowledgeUploadDelegationResponse {
                    token,
                    expires_at: now + 60,
                }),
            )
                .into_response()
        }
        Err(error) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "code": "KNOWLEDGE_DELEGATION_UNAVAILABLE",
                "message": error.to_string()
            })),
        )
            .into_response(),
    }
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
    if owner.agent_def_id != state.agent_def_id {
        return Err(HandlerRejection::forbidden(
            "token agent definition is not published to this Agent instance",
        ));
    }
    let caller_subject = principal
        .user_id
        .clone()
        .or_else(|| principal.client_id.clone())
        .unwrap_or_default();
    let subject_type = if principal.user_id.is_some() {
        "USER"
    } else {
        "WORKLOAD"
    };
    let groups = normalized_claim_values(&principal.claims, &["groups", "group"]);
    let organizations = normalized_claim_values(
        &principal.claims,
        &["organizations", "organization", "orgs"],
    );
    Ok(AuthenticatedRequest {
        authorization: format!("Bearer {token}"),
        owner,
        caller_claims: principal.claims,
        caller_subject,
        subject_type: subject_type.into(),
        groups,
        organizations,
    })
}

fn normalized_claim_values(claims: &serde_json::Value, names: &[&str]) -> Vec<String> {
    let Some(value) = names.iter().find_map(|name| claims.get(*name)) else {
        return Vec::new();
    };
    let mut values = match value {
        serde_json::Value::Array(values) => values
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect(),
        serde_json::Value::String(value) => value
            .split(|character: char| character.is_whitespace() || character == ',')
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    values.sort_unstable();
    values.dedup();
    values
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

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum RequestedProfile {
    Enterprise,
    Coding,
    PersonalAssistant,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CodingDispatchRequest {
    repository: ImmutableRepositoryInput,
    base_revision: String,
    workspace_root: String,
    #[serde(default)]
    writable_roots: BTreeSet<String>,
    allowed_tools: BTreeSet<String>,
    maximum_patch_bytes: u64,
    maximum_changed_files: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClientMessage {
    pub text: String,
    #[serde(default)]
    pub client_message_id: Option<String>,
    #[serde(default)]
    profile: Option<RequestedProfile>,
    #[serde(default)]
    coding: Option<CodingDispatchRequest>,
    #[serde(default)]
    edge_action: Option<EdgeActionSpec>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "session")]
    Session { session_id: String },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "executionAccepted")]
    ExecutionAccepted { profile: String, request_id: String },
    #[serde(rename = "error")]
    Error { message: String },
}

fn trim_history(history: &mut Vec<ChatMessage>) {
    let excess = history.len().saturating_sub(MAX_SESSION_MESSAGES);
    if excess > 0 {
        history.drain(0..excess);
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
    agent_config: &AgentConfig,
    config: &ModelProviderConfig,
    llm_gateway_token: &str,
    llm_gateway_client: &reqwest::Client,
) -> Result<ModelProviderSelection, RuntimeError> {
    let provider_id = normalize_provider_id(&config.provider);
    if provider_id != "gateway" && provider_id != "light-gateway" {
        return Err(RuntimeError::Unsupported(format!(
            "direct model provider {provider_id} is disabled; light-agent model calls must use llm-gateway"
        )));
    }
    let model = choose_model(config, None, None, "llm-gateway")?;
    if model != agent_config.agent_policy.model.alias {
        return Err(RuntimeError::Config(
            "turn model alias does not match the loaded immutable Agent policy".into(),
        ));
    }
    let gateway = &agent_config.agent_policy.model.gateway;
    let provider = CompatibleProvider::new_with_client(
        gateway.name.as_str(),
        gateway.base_url.as_str(),
        Some(llm_gateway_token),
        llm_gateway_client.clone(),
    )
    .with_max_tokens(Some(
        u32::try_from(agent_config.agent_policy.model.maximum_tokens).map_err(|_| {
            RuntimeError::Config("agentPolicy.model.maximumTokens exceeds u32".into())
        })?,
    ));
    Ok(ModelProviderSelection {
        provider: Box::new(provider),
        model,
        temperature: config.temperature,
    })
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
                "agentPolicy.model.alias or {provider_name}.model is required"
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

fn canonical_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn validate_repository_input_uri(value: &str, prefix: &str) -> Result<()> {
    let uri = Url::parse(value).context("repository input URI is invalid")?;
    let prefix_uri = Url::parse(prefix).context("repository input URI prefix is invalid")?;
    if uri.scheme() != "file"
        || prefix_uri.scheme() != "file"
        || uri.host_str().is_some_and(|host| !host.is_empty())
        || prefix_uri.host_str().is_some_and(|host| !host.is_empty())
        || uri.query().is_some()
        || uri.fragment().is_some()
        || !value.starts_with(prefix)
        || !prefix.ends_with('/')
        || uri.to_file_path().ok().is_none_or(|path| {
            path.components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        })
    {
        bail!("repository input is outside the configured immutable spool")
    }
    Ok(())
}

fn coding_profile_from_policy(
    policy: Option<&CodingProfilePolicy>,
) -> Result<Option<CodingProfileConfig>, RuntimeError> {
    let Some(policy) = policy else {
        return Ok(None);
    };
    for (name, value) in [
        (
            "productProfileDigest",
            policy.product_profile_digest.as_str(),
        ),
        ("compatibilityDigest", policy.compatibility_digest.as_str()),
        ("templateDigest", policy.template_digest.as_str()),
        ("binaryDigest", policy.binary_digest.as_str()),
    ] {
        if !canonical_sha256(value) {
            return Err(RuntimeError::Config(format!(
                "agentPolicy.execution.codingProfile.{name} must be canonical SHA-256"
            )));
        }
    }
    validate_repository_input_uri(
        &format!("{}probe", policy.repository_uri_prefix),
        &policy.repository_uri_prefix,
    )
    .map_err(|error| RuntimeError::Config(error.to_string()))?;
    Ok(Some(CodingProfileConfig {
        product_profile_digest: policy.product_profile_digest.clone(),
        repository_uri_prefix: policy.repository_uri_prefix.clone(),
        runtime: PiCodingRuntime {
            compatibility_digest: policy.compatibility_digest.clone(),
            template_digest: policy.template_digest.clone(),
            pi_digest: policy.binary_digest.clone(),
            provider: policy.provider.clone(),
            model: policy.model.clone(),
        },
    }))
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AgentState>,
    session_id: Uuid,
    authenticated: AuthenticatedRequest,
) {
    let (mut sender, mut receiver) = socket.split();
    let session_id_string = session_id.to_string();
    let durable_policy = state.policy_snapshot.clone();
    let session_policy = &state.agent_config.agent_policy.session;
    let now = chrono::Utc::now();
    let idle_seconds = i64::try_from(session_policy.idle_seconds).unwrap_or(i64::MAX);
    let maximum_seconds = i64::try_from(session_policy.maximum_seconds).unwrap_or(i64::MAX);
    let Some(idle_expires_at) = now.checked_add_signed(chrono::Duration::seconds(idle_seconds))
    else {
        error!("Configured Agent idle session lifetime overflows UTC time");
        return;
    };
    let Some(maximum_expires_at) =
        now.checked_add_signed(chrono::Duration::seconds(maximum_seconds))
    else {
        error!("Configured Agent maximum session lifetime overflows UTC time");
        return;
    };
    if let Err(err) = state
        .domain
        .create_or_resume_session(&SessionSpec {
            host_id: state.host_id,
            session_id: AgentSessionId(session_id),
            principal_id: authenticated.owner.principal_id.to_string(),
            user_id: Some(authenticated.owner.principal_id),
            agent_def_id: authenticated.owner.agent_def_id,
            definition_version: state.definition_version,
            model_provider: state.agent_config.agent_policy.model.provider.clone(),
            model_name: state.agent_config.agent_policy.model.alias.clone(),
            maximum_active_sessions: session_policy.maximum_active_sessions,
            bank_id: None,
            policy: durable_policy,
            idle_expires_at,
            maximum_expires_at,
            resume_handle_digest: sha256_digest(
                format!("{}:{}", session_id, authenticated.owner.principal_id).as_bytes(),
            ),
        })
        .await
    {
        error!("Failed to create or resume durable session: {err}");
        return;
    }

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
    if let Err(e) = state
        .domain
        .bind_session_memory_bank(state.host_id, AgentSessionId(session_id), bank_id)
        .await
    {
        error!("Failed to bind durable session to memory bank: {}", e);
        let payload = serde_json::to_string(&ServerMessage::Error {
            message: "Failed to bind session memory".to_string(),
        })
        .unwrap_or_else(|_| {
            "{\"type\":\"error\",\"message\":\"Session initialization failed\"}".to_string()
        });
        let _ = sender.send(Message::Text(payload.into())).await;
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

            let inferred_profile = if client_msg.coding.is_some() {
                RequestedProfile::Coding
            } else if client_msg.edge_action.is_some() {
                RequestedProfile::PersonalAssistant
            } else {
                RequestedProfile::Enterprise
            };
            let requested_profile = client_msg.profile.unwrap_or(inferred_profile);
            let profile_shape_valid = requested_profile == inferred_profile
                && !(client_msg.coding.is_some() && client_msg.edge_action.is_some())
                && (requested_profile != RequestedProfile::Coding || client_msg.coding.is_some());
            if !profile_shape_valid {
                let _ = sender
                    .send(Message::Text(
                        serde_json::to_string(&ServerMessage::Error {
                            message: "Profile and typed execution payload do not match".into(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await;
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
                    &state.agent_config.agent_policy.model.provider,
                    &state.agent_config.agent_policy.model.alias,
                    state.agent_config.agent_policy.session.maximum_queued_turns,
                    state.agent_config.agent_policy.model.maximum_tokens,
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

            // Admission and completion events are the conversation authority.
            // Refresh only after this turn owns the session activation fence so
            // a long-lived socket cannot run from the history it loaded before
            // another connection completed an earlier turn.
            if let Err(error) = state
                .domain
                .rebuild_history_projection(state.host_id, AgentSessionId(session_id), bank_id)
                .await
            {
                let _ = state
                    .domain
                    .fail_turn(
                        state.host_id,
                        AgentSessionId(session_id),
                        admitted.turn_id,
                        "event-backed history refresh failed",
                    )
                    .await;
                error!(turn_id=%admitted.turn_id.0, "Failed to refresh history projection after activation: {error}");
                let _ = sender
                    .send(Message::Text(
                        serde_json::to_string(&ServerMessage::Error {
                            message: "Failed to refresh session history".into(),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await;
                continue;
            }
            let mut history = match state
                .memory
                .load_session_history(state.host_id, bank_id, session_id)
                .await
            {
                Ok(history) => history,
                Err(error) => {
                    let _ = state
                        .domain
                        .fail_turn(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            "event-backed history load failed",
                        )
                        .await;
                    error!(turn_id=%admitted.turn_id.0, "Failed to load refreshed history projection: {error}");
                    let _ = sender
                        .send(Message::Text(
                            serde_json::to_string(&ServerMessage::Error {
                                message: "Failed to load refreshed session history".into(),
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await;
                    continue;
                }
            };
            trim_history(&mut history);

            if requested_profile == RequestedProfile::Coding {
                let outcome: Result<Uuid> = async {
                    let config = state
                        .coding_profile
                        .as_ref()
                        .context("coding profile is disabled")?;
                    if turn_resolution.product_profile_digest != config.product_profile_digest {
                        bail!("turn policy does not authorize the coding profile")
                    }
                    let request = client_msg
                        .coding
                        .as_ref()
                        .context("coding payload is required")?;
                    validate_repository_input_uri(
                        &request.repository.artifact_uri,
                        &config.repository_uri_prefix,
                    )?;
                    let writable_roots = if request.writable_roots.is_empty() {
                        BTreeSet::from([request.workspace_root.clone()])
                    } else {
                        request.writable_roots.clone()
                    };
                    let manifest = MaterializationManifest {
                        schema_version: 1,
                        materializer_id: "coding".into(),
                        materializer_version: 1,
                        product_profile: ProductProfile::Coding,
                        runtime_compatibility: config.runtime.compatibility_digest.clone(),
                        packages: Vec::new(),
                        effective_instructions: Vec::new(),
                        allowed_tools: BTreeSet::new(),
                        writable_roots: writable_roots.clone(),
                    };
                    let spec = CodingTurnSpec {
                        repository_digest: request.repository.digest.clone(),
                        base_revision: request.base_revision.clone(),
                        workspace_root: request.workspace_root.clone(),
                        prompt: user_text.clone(),
                        model_alias: format!(
                            "{}:{}",
                            config.runtime.provider, config.runtime.model
                        ),
                        materialization_manifest_digest: manifest.digest()?,
                        writable_roots,
                        allowed_tools: request.allowed_tools.clone(),
                        maximum_patch_bytes: request.maximum_patch_bytes,
                        maximum_changed_files: request.maximum_changed_files,
                    };
                    state
                        .domain
                        .schedule_pi_coding_turn(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            &state.service_id,
                            &manifest,
                            &spec,
                            &request.repository,
                            &config.runtime,
                        )
                        .await
                }
                .await;
                match outcome {
                    Ok(request_id) => {
                        let _ = sender
                            .send(Message::Text(
                                serde_json::to_string(&ServerMessage::ExecutionAccepted {
                                    profile: "coding".into(),
                                    request_id: request_id.to_string(),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;
                    }
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
                                    message: format!("Coding dispatch failed: {error}"),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;
                    }
                }
                continue;
            }

            if let Some(edge_action) = client_msg.edge_action.as_ref() {
                let outcome: Result<Uuid> = async {
                    let expected = state
                        .personal_profile_digest
                        .as_deref()
                        .context("personal-assistant profile is disabled")?;
                    if turn_resolution.product_profile_digest != expected {
                        bail!("turn policy does not authorize personal edge actions")
                    }
                    state
                        .domain
                        .schedule_edge_action(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            &state.service_id,
                            edge_action,
                        )
                        .await
                }
                .await;
                match outcome {
                    Ok(action_id) => {
                        let _ = sender
                            .send(Message::Text(
                                serde_json::to_string(&ServerMessage::ExecutionAccepted {
                                    profile: "personal-assistant".into(),
                                    request_id: action_id.to_string(),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;
                    }
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
                                    message: format!("Edge action dispatch failed: {error}"),
                                })
                                .unwrap()
                                .into(),
                            ))
                            .await;
                    }
                }
                continue;
            }
            let turn_provider_config = ModelProviderConfig {
                provider: turn_resolution.model_provider.clone(),
                model: Some(turn_resolution.model_name.clone()),
                temperature: state.default_temperature,
            };
            let turn_runtime = match build_model_provider(
                &state.agent_config,
                &turn_provider_config,
                &state.llm_gateway_token,
                &state.llm_gateway_client,
            ) {
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
                        .fail_turn_after_model_dispatch(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            "turn deadline exceeded",
                        )
                        .await;
                    let payload = serde_json::to_string(&ServerMessage::Error {
                        message: "Turn deadline exceeded".to_string(),
                    });
                    if let Ok(payload) = payload {
                        let _ = sender.send(Message::Text(payload.into())).await;
                    }
                }
                Ok(Ok((response, usage, knowledge_evidence))) => {
                    if let Some(text) = response.text {
                        if let Err(err) = state
                            .domain
                            .complete_turn(
                                state.host_id,
                                AgentSessionId(session_id),
                                admitted.turn_id,
                                &text,
                                usage
                                    .complete
                                    .then(|| i64::try_from(usage.input_tokens).unwrap_or(i64::MAX)),
                                usage.complete.then(|| {
                                    i64::try_from(usage.output_tokens).unwrap_or(i64::MAX)
                                }),
                                knowledge_evidence.as_ref(),
                            )
                            .await
                        {
                            error!("Failed to commit durable turn result: {err}");
                            continue;
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
                        .fail_turn_after_model_dispatch(
                            state.host_id,
                            AgentSessionId(session_id),
                            admitted.turn_id,
                            &e.to_string(),
                        )
                        .await;
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
         (host_id,bank_id,agent_def_id,user_id,agent_definition_version,
          agent_definition_digest,user_identity_digest,bank_name)
         SELECT s.host_id,$2,s.agent_def_id,s.user_id,s.agent_definition_version,
                s.agent_definition_digest,s.user_identity_digest,$5
           FROM agent_session_t s
          WHERE s.host_id=$1 AND s.session_id=$3 AND s.agent_def_id=$4
            AND s.user_id=$6 AND s.state='ACTIVE'
         ON CONFLICT (host_id, bank_id) DO NOTHING",
    )
    .bind(host_id)
    .bind(bank_id)
    .bind(session_id)
    .bind(owner.agent_def_id)
    .bind(format!("session-{session_id}"))
    .bind(owner.principal_id)
    .execute(db)
    .await
    .context("failed to create session memory bank")?;

    sqlx::query(
        "INSERT INTO operational_reference_evidence_t(
             host_id,reference_id,source_service,source_table,source_record_id,
             reference_kind,target_id,target_version,publication_id,content_digest,
             issuer,audience,state,accepted_ts,reconciled_ts)
         SELECT e.host_id,gen_random_uuid(),e.source_service,'agent_memory_bank_t',$1,
                e.reference_kind,e.target_id,e.target_version,e.publication_id,e.content_digest,
                e.issuer,e.audience,e.state,e.accepted_ts,now()
           FROM operational_reference_evidence_t e
          WHERE e.host_id=$2 AND e.source_table='agent_session_t'
            AND e.source_record_id=$3
            AND e.reference_kind IN ('HOST_SCOPE','AGENT_DEFINITION','USER_PRINCIPAL')
         ON CONFLICT(host_id,source_service,source_table,source_record_id,reference_kind)
         DO NOTHING",
    )
    .bind(bank_id)
    .bind(host_id)
    .bind(session_id)
    .execute(db)
    .await
    .context("failed to pin session memory bank reference evidence")?;

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
        subject_id: authenticated.caller_subject.clone(),
        subject_type: authenticated.subject_type.clone(),
        groups: Some(authenticated.groups.clone()),
        organizations: Some(authenticated.organizations.clone()),
        agent_actor: state.service_id.clone(),
        agent_def_id: Some(authenticated.owner.agent_def_id),
        agent_policy_version: state.definition_version,
        host_id: state.host_id,
        environment: state.env_tag.clone(),
        session_id,
        turn_id,
        action_attempt_id: action.map(|value| value.0),
        tool_ref: action.map(|value| value.1),
        tool_alias: tool_alias.map(str::to_string),
        destination: Some("mcp".into()),
        workflow_invocation_id: None,
        workflow_permit_depth: None,
        workflow_execution_class: None,
        workflow_budget_ledger_id: None,
        workflow_budget_generation: None,
        data_boundary_digest: data_boundary_digest.to_string(),
        policy_digest: policy_digest.to_string(),
        replay_id: Uuid::now_v7(),
        issued_at: now,
        expires_at: now + 60,
    })?;
    Ok(format!("Bearer {token}"))
}

fn knowledge_authorization(
    state: &AgentState,
    authenticated: &AuthenticatedRequest,
    kind: DelegationKind,
    session_id: Uuid,
    turn_id: Uuid,
    policy_digest: &str,
    data_boundary_digest: &str,
) -> Result<String> {
    let signer = state
        .delegation_signer
        .as_ref()
        .context("Knowledge access requires the delegated workload signer")?;
    let now = chrono::Utc::now().timestamp();
    let token = signer.mint(DelegationClaims {
        token_id: Uuid::now_v7(),
        kind,
        issuer: String::new(),
        audience: "light-knowledge".into(),
        caller_subject: authenticated.caller_subject.clone(),
        caller_claims: authenticated.caller_claims.clone(),
        subject_id: authenticated.caller_subject.clone(),
        subject_type: authenticated.subject_type.clone(),
        groups: Some(authenticated.groups.clone()),
        organizations: Some(authenticated.organizations.clone()),
        agent_actor: state.service_id.clone(),
        agent_def_id: Some(authenticated.owner.agent_def_id),
        agent_policy_version: state.definition_version,
        host_id: state.host_id,
        environment: state.env_tag.clone(),
        session_id,
        turn_id,
        action_attempt_id: None,
        tool_ref: None,
        tool_alias: None,
        destination: Some("knowledge".into()),
        workflow_invocation_id: None,
        workflow_permit_depth: None,
        workflow_execution_class: None,
        workflow_budget_ledger_id: None,
        workflow_budget_generation: None,
        data_boundary_digest: data_boundary_digest.to_string(),
        policy_digest: policy_digest.to_string(),
        replay_id: Uuid::now_v7(),
        issued_at: now,
        expires_at: now + 60,
    })?;
    Ok(format!("Bearer {token}"))
}

#[derive(Debug, Clone, Copy, Default)]
struct TrustedProviderUsage {
    input_tokens: u64,
    output_tokens: u64,
    complete: bool,
}

fn apply_authoritative_system_prompt(
    messages: &mut Vec<ChatMessage>,
    system_prompt: &str,
) -> Result<String> {
    let user_prompt = messages
        .last()
        .filter(|message| message.role == "user")
        .map(|message| message.content.clone())
        .context("agent turn requires a trailing user message")?;
    messages.retain(|message| message.role != "system");
    messages.insert(0, ChatMessage::system(system_prompt));
    Ok(user_prompt)
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
) -> Result<(
    ChatResponse,
    TrustedProviderUsage,
    Option<serde_json::Value>,
)> {
    // System instructions are compiled only from the immutable Portal
    // projection. Durable conversation history is never allowed to inject or
    // retain a competing system role.
    let user_prompt = apply_authoritative_system_prompt(&mut messages, &state.system_prompt)?;

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

    // Knowledge evidence remains separate from Hindsight. The immutable Agent
    // audience projection selects the bindings, and light-knowledge
    // independently authorizes the short-lived delegation against its view of
    // the same publication.
    let mut knowledge_evidence = None;
    if let Some(client) = state.knowledge_client.as_ref() {
        let environment = state
            .env_tag
            .as_deref()
            .context("Knowledge retrieval requires LIGHT_ENV_TAG")?;
        let bindings = state.knowledge_bindings.clone();
        if !bindings.is_empty() {
            let delegated = knowledge_authorization(
                state,
                authenticated,
                DelegationKind::KnowledgeRetrieve,
                Uuid::parse_str(session_id)?,
                turn_id,
                policy_digest,
                data_boundary_digest,
            )?;
            let request_id = Uuid::now_v7().to_string();
            let request = RetrieveRequest {
                knowledge_base_ids: bindings
                    .iter()
                    .map(|binding| binding.knowledge_base_id)
                    .collect(),
                environment: environment.to_string(),
                query: user_prompt.clone(),
                top_k: state.agent_config.agent_policy.knowledge.retrieval.top_k,
                token_budget: state
                    .agent_config
                    .agent_policy
                    .knowledge
                    .retrieval
                    .token_budget,
                filters: state
                    .agent_config
                    .agent_policy
                    .knowledge
                    .retrieval
                    .filters
                    .clone(),
            };
            match client.search(&request_id, &delegated, &request).await {
                Ok(KnowledgeSearchResponse::Single(response)) => {
                    let rendered =
                        render_untrusted_evidence(&response, state.limits.max_tool_output_bytes);
                    if !response.no_answer {
                        if let Some(message) = messages.last_mut() {
                            message.content = format!("{rendered}\n\n{}", message.content);
                        }
                    }
                    knowledge_evidence = Some(serde_json::json!({
                        "requestId": request_id,
                        "knowledgeBaseId": response.knowledge_base_id,
                        "generationId": response.generation_id,
                        "noAnswer": response.no_answer,
                        "citations": response.results.iter().map(|hit| serde_json::json!({
                            "chunkId": hit.chunk_id,
                            "documentId": hit.citation.document_id,
                            "documentVersionId": hit.citation.document_version_id,
                            "contentDigest": hit.citation.content_digest,
                            "canonicalUri": hit.citation.canonical_uri,
                            "sourceVersion": hit.citation.source_version
                        })).collect::<Vec<_>>()
                    }));
                }
                Ok(KnowledgeSearchResponse::Multi(response)) => {
                    let rendered = render_untrusted_multi_evidence(
                        &response,
                        state.limits.max_tool_output_bytes,
                    );
                    if !response.results.is_empty() {
                        if let Some(message) = messages.last_mut() {
                            message.content = format!("{rendered}\n\n{}", message.content);
                        }
                    }
                    knowledge_evidence = Some(serde_json::json!({
                        "requestId": request_id,
                        "status": response.status,
                        "disposition": response.disposition,
                        "knowledgeBaseIds": response.knowledge_base_ids,
                        "embeddingGroupCount": response.embedding_group_count,
                        "warnings": response.warnings,
                        "exclusions": response.exclusions,
                        "citations": response.results.iter().map(|result| serde_json::json!({
                            "knowledgeBaseId": result.knowledge_base_id,
                            "generationId": result.generation_id,
                            "chunkId": result.hit.chunk_id,
                            "documentId": result.hit.citation.document_id,
                            "documentVersionId": result.hit.citation.document_version_id,
                            "contentDigest": result.hit.citation.content_digest,
                            "canonicalUri": result.hit.citation.canonical_uri,
                            "sourceVersion": result.hit.citation.source_version
                        })).collect::<Vec<_>>()
                    }));
                }
                Err(error) if bindings.iter().any(|binding| binding.evidence_required) => {
                    bail!("required Knowledge Base evidence failed: {error}")
                }
                Err(error) => {
                    warn!(%error, "optional Knowledge Base evidence unavailable; continuing by policy");
                }
            }
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
    let outbound_tools = state
        .outbound_a2a
        .as_ref()
        .map(|runtime| runtime.bindings_by_tool.clone())
        .unwrap_or_default();
    for (name, binding) in &outbound_tools {
        if accepted_tools.contains_key(name) {
            bail!("outbound A2A tool collides with a Gateway tool: {name}");
        }
        tool_specs.push(ToolSpec {
            name: name.clone(),
            description: binding.description.clone(),
            parameters: serde_json::json!({
                "type":"object",
                "additionalProperties":false,
                "required":["message"],
                "properties":{
                    "message":{"type":"string","minLength":1},
                    "skillId":{"type":"string","enum":binding.allowed_skill_ids}
                }
            }),
        });
    }

    // 3. Main LLM Loop
    let mut final_response = None;
    let mut action_count = 0usize;
    let mut trusted_usage = TrustedProviderUsage {
        complete: true,
        ..TrustedProviderUsage::default()
    };
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

        if let Some(reported) = response.usage.as_ref() {
            if let (Some(input_tokens), Some(output_tokens)) =
                (reported.input_tokens, reported.output_tokens)
            {
                trusted_usage.input_tokens =
                    trusted_usage.input_tokens.saturating_add(input_tokens);
                trusted_usage.output_tokens =
                    trusted_usage.output_tokens.saturating_add(output_tokens);
                let turn_tokens = trusted_usage
                    .input_tokens
                    .saturating_add(trusted_usage.output_tokens);
                if turn_tokens > state.limits.max_turn_tokens {
                    bail!(
                        "turn token budget exceeded ({} > {})",
                        turn_tokens,
                        state.limits.max_turn_tokens
                    );
                }
            } else {
                trusted_usage.complete = false;
            }
        } else {
            trusted_usage.complete = false;
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
            if let Some(binding) = outbound_tools.get(&tool_call.name) {
                let arguments: serde_json::Value = serde_json::from_str(&tool_call.arguments)
                    .context("outbound A2A tool arguments are invalid JSON")?;
                let message = arguments
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("outbound A2A message is required"))?;
                let skill_id = arguments.get("skillId").and_then(serde_json::Value::as_str);
                if skill_id.is_some_and(|skill| {
                    !binding
                        .allowed_skill_ids
                        .iter()
                        .any(|allowed| allowed == skill)
                }) {
                    bail!("outbound A2A skill is not assigned to this Agent");
                }
                let (action_attempt_id, _) = state
                    .domain
                    .propose_gateway_action(
                        state.host_id,
                        agent_core::AgentTurnId(turn_id),
                        binding.catalog_tool_id,
                        &tool_call.name,
                        &tool_call.arguments,
                    )
                    .await?;
                let result = invoke_outbound_a2a(
                    state,
                    authenticated,
                    binding,
                    message,
                    skill_id,
                    &tool_call.id,
                    data_boundary_digest,
                )
                .await;
                let (succeeded, payload) = match result {
                    Ok(value) => (true, value),
                    Err(error) => (false, serde_json::json!({"error":error.to_string()})),
                };
                state
                    .domain
                    .accept_gateway_result(
                        state.host_id,
                        agent_core::AgentTurnId(turn_id),
                        action_attempt_id,
                        succeeded,
                        payload.clone(),
                    )
                    .await?;
                let rendered = serde_json::to_string(&payload)?;
                let (rendered, truncated) = bound_untrusted_text(
                    &rendered,
                    &state.limits,
                    state.limits.max_tool_output_bytes,
                );
                messages.push(tool_result_message(
                    &tool_call.id,
                    &tool_call.name,
                    &rendered,
                    !succeeded,
                    truncated,
                ));
                continue;
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

    Ok((response, trusted_usage, knowledge_evidence))
}

async fn build_agent_state(
    runtime_config: &RuntimeConfig,
    catalog_cache: AgentCatalogCache,
    lifecycle: &light_runtime::LifecycleRegistrar,
) -> Result<Arc<AgentState>, RuntimeError> {
    let agent_config: AgentConfig = load_agent_registered_config(
        runtime_config,
        AGENT_CONFIG_FILE,
        AGENT_CONFIG_MODULE_ID,
        "agent",
        [],
    )?;
    agent_config
        .validate(
            &runtime_config.bootstrap.host,
            &runtime_config.service_identity.service_id,
            runtime_config
                .service_identity
                .env_tag
                .as_deref()
                .ok_or_else(|| RuntimeError::Config("startup envTag is required".into()))?,
            chrono::Utc::now(),
        )
        .map_err(RuntimeError::Config)?;
    let model_provider_config = ModelProviderConfig {
        provider: agent_config.agent_policy.model.provider.clone(),
        model: Some(agent_config.agent_policy.model.alias.clone()),
        temperature: agent_config.agent_policy.model.temperature,
    };

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
    let limits = AgentLimits::from_policy(&agent_config.agent_policy.execution)?;

    let ca_cert = read_agent_ca_cert_bundle(runtime_config)?;
    let verify_hostname: bool = runtime_config
        .client
        .as_ref()
        .map(|c| c.tls.verify_hostname)
        .unwrap_or(true);
    if !verify_hostname {
        warn!(
            "TLS hostname verification is disabled for light-agent outbound clients; this weakens server identity validation"
        );
    }

    let llm_gateway_client = build_agent_http_client(
        ca_cert.as_deref(),
        verify_hostname,
        Duration::from_secs(300),
    )?;

    let mcp_client = McpGatewayClient::with_tls_options_and_response_limit(
        &mcp_gateway_url,
        ca_cert.as_deref(),
        verify_hostname,
        mcp_config.timeout_ms,
        limits.max_gateway_response_bytes,
    )
    .map_err(|e| RuntimeError::Config(format!("failed to build MCP gateway client: {e}")))?;

    let database_url_file = PathBuf::from(&agent_config.operational_store.database_url_file);
    let db_url = agent_store::read_database_url(&database_url_file)
        .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let pool = PgPoolOptions::new()
        .max_connections(20)
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET search_path TO agent_ops, operational_meta")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&db_url)
        .await
        .map_err(|e| RuntimeError::Config(format!("failed to connect to Agent store: {e}")))?;
    agent_store::validate(
        &pool,
        &agent_store::ExpectedBinding {
            binding_id: agent_config.operational_store.binding_id,
            binding_digest: &agent_config.operational_store.binding_digest,
            host_id: agent_config.operational_store.host_id,
            environment: &agent_config.operational_store.environment,
            minimum_schema_generation: agent_config.operational_store.minimum_schema_version,
        },
    )
    .await
    .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let native_a2a = if agent_config.a2a_policy.enabled {
        let key_path = PathBuf::from(&agent_config.a2a_policy.authorization_context_key_file);
        let metadata = std::fs::symlink_metadata(&key_path).map_err(|error| {
            RuntimeError::Config(format!("cannot inspect native A2A context key: {error}"))
        })?;
        #[cfg(unix)]
        let permissions_are_private = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o037 == 0
        };
        #[cfg(not(unix))]
        let permissions_are_private = true;
        if metadata.file_type().is_symlink() || !metadata.is_file() || !permissions_are_private {
            return Err(RuntimeError::Config(
                "native A2A context key must be a private regular non-symlink file".into(),
            ));
        }
        let key = std::fs::read(&key_path).map_err(|error| {
            RuntimeError::Config(format!("cannot read native A2A context key: {error}"))
        })?;
        if key.len() < 32 {
            return Err(RuntimeError::Config(
                "native A2A context key must contain at least 32 bytes".into(),
            ));
        }
        Some(NativeA2aRuntime {
            repository: agent_store::NativeA2aRepository::new(pool.clone()),
            authorization_key: Arc::new(key),
            agent_ref: agent_config.a2a_policy.agent_ref.clone(),
            binding_id: agent_config
                .a2a_policy
                .binding_id
                .expect("validated native A2A binding ID"),
            publication_id: agent_config
                .a2a_policy
                .publication_id
                .expect("validated native A2A publication ID"),
            policy_digest: agent_config.a2a_policy.policy_digest.clone(),
            protocol_profile: agent_config
                .a2a_policy
                .protocol_profile
                .clone()
                .expect("validated native A2A protocol profile"),
            allowed_operations: agent_config.a2a_policy.allowed_operations.clone(),
            allowed_principal_prefixes: agent_config.a2a_policy.allowed_principal_prefixes.clone(),
            public_url: agent_config.a2a_policy.public_url.clone(),
            agent_card: agent_config
                .a2a_policy
                .agent_card
                .clone()
                .expect("validated native Agent Card"),
            revocation_epoch: agent_config.runtime_policy.revocation_epoch,
            public_skill_mapping: serde_json::to_value(&agent_config.a2a_policy.public_skills)
                .map_err(|error| {
                    RuntimeError::Config(format!(
                        "cannot serialize native A2A public skill mapping: {error}"
                    ))
                })?,
            public_skill_mapping_digest: agent_config
                .a2a_skill_mapping_digest()
                .map_err(RuntimeError::Config)?,
            artifact_retention_days: agent_config
                .a2a_policy
                .artifact_retention
                .as_ref()
                .expect("validated native A2A artifact retention policy")
                .artifact_retention_days,
            maximum_artifact_bytes: agent_config
                .a2a_policy
                .artifact_retention
                .as_ref()
                .expect("validated native A2A artifact retention policy")
                .maximum_artifact_bytes,
            artifact_root_directory: agent_config.a2a_policy.artifact_root_directory.clone(),
        })
    } else {
        None
    };
    let outbound_a2a = if agent_config.a2a_outbound.enabled {
        let key_path = PathBuf::from(&agent_config.a2a_outbound.authorization_context_key_file);
        let metadata = std::fs::symlink_metadata(&key_path).map_err(|error| {
            RuntimeError::Config(format!("cannot inspect outbound A2A context key: {error}"))
        })?;
        #[cfg(unix)]
        let permissions_are_private = {
            use std::os::unix::fs::PermissionsExt;
            metadata.permissions().mode() & 0o037 == 0
        };
        #[cfg(not(unix))]
        let permissions_are_private = true;
        if metadata.file_type().is_symlink() || !metadata.is_file() || !permissions_are_private {
            return Err(RuntimeError::Config(
                "outbound A2A context key must be a private regular non-symlink file".into(),
            ));
        }
        let key = std::fs::read(&key_path).map_err(|error| {
            RuntimeError::Config(format!("cannot read outbound A2A context key: {error}"))
        })?;
        if key.len() < 32 {
            return Err(RuntimeError::Config(
                "outbound A2A context key must contain at least 32 bytes".into(),
            ));
        }
        let mut bindings_by_tool = HashMap::new();
        for binding in &agent_config.a2a_outbound.bindings {
            let name = outbound_a2a_tool_name(&binding.agent_ref);
            if bindings_by_tool.insert(name, binding.clone()).is_some() {
                return Err(RuntimeError::Config(
                    "outbound A2A agentRef values produce a duplicate model tool name".into(),
                ));
            }
        }
        Some(OutboundA2aRuntime {
            authorization_key: Arc::new(key),
            client: build_agent_http_client(
                ca_cert.as_deref(),
                verify_hostname,
                Duration::from_secs(120),
            )?,
            bindings_by_tool,
        })
    } else {
        None
    };
    lifecycle.register(Arc::new(AgentDatabase(pool.clone())))?;
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

    let host_id = agent_config.operational_store.host_id;
    let agent_def_id = agent_config.agent_policy.agent_def_id;
    let definition_version = agent_config.agent_policy.definition_version;
    let policy_digest = agent_config.runtime_policy.policy_digest.clone();
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
    let execution_client = ExecutionClient::new_with_bearer_token(
        &agent_config.agent_policy.execution.execution_api_url,
        &portal_token,
        Duration::from_millis(mcp_config.timeout_ms),
        ca_cert.as_deref(),
    )
    .map_err(|error| {
        RuntimeError::Config(format!(
            "failed to build Controller execution client: {error}"
        ))
    })?;
    let memory_write_mode = agent_config
        .agent_policy
        .memory
        .write_mode
        .trim()
        .to_ascii_lowercase();
    let memory: Arc<dyn MemoryStore> = match memory_write_mode.as_str() {
        "operational" => Arc::new(EmbeddedMemoryStore::new(pool.clone())),
        other => {
            return Err(RuntimeError::Config(format!(
                "agentPolicy.memory.writeMode must be operational after the Agent-store cutover, got {other}"
            )));
        }
    };
    let catalog_cache_ttl =
        Duration::from_secs(agent_config.agent_policy.catalog.cache_ttl_seconds.max(1));
    let mut catalog_stale_on_error = Duration::from_secs(
        agent_config
            .agent_policy
            .catalog
            .stale_on_error_seconds
            .max(1),
    );
    if catalog_stale_on_error < catalog_cache_ttl {
        catalog_stale_on_error = catalog_cache_ttl;
    }
    let coding_profile =
        coding_profile_from_policy(agent_config.agent_policy.execution.coding_profile.as_ref())?;
    let personal_profile_digest = agent_config
        .agent_policy
        .memory
        .personal_profile_digest
        .clone()
        .filter(|value| !value.trim().is_empty());
    if personal_profile_digest
        .as_deref()
        .is_some_and(|value| !canonical_sha256(value))
    {
        return Err(RuntimeError::Config(
            "agentPolicy.memory.personalProfileDigest must be canonical SHA-256".into(),
        ));
    }
    let knowledge_client = agent_config
        .agent_policy
        .knowledge
        .endpoint
        .clone()
        .filter(|value| !value.trim().is_empty())
        .map(|endpoint| {
            KnowledgeClient::new(
                &endpoint,
                Duration::from_millis(1_000),
                agent_config.agent_policy.knowledge.allow_private_plaintext,
            )
        })
        .transpose()
        .map_err(|error| {
            RuntimeError::Config(format!("failed to build Knowledge client: {error}"))
        })?;

    let configured_catalog = agent_config
        .agent_policy
        .catalog
        .effective_catalog
        .clone()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| {
            RuntimeError::Config(format!(
                "agentPolicy.catalog.effectiveCatalog is invalid: {error}"
            ))
        })?;
    let mut knowledge_bindings = agent_config
        .agent_policy
        .knowledge
        .bindings
        .iter()
        .cloned()
        .map(serde_json::from_value)
        .collect::<Result<Vec<AgentKnowledgeBinding>, _>>()
        .map_err(|error| {
            RuntimeError::Config(format!(
                "agentPolicy.knowledge.bindings are invalid: {error}"
            ))
        })?
        .into_iter()
        .filter(|binding| binding.active && binding.agent_id == agent_def_id)
        .collect::<Vec<_>>();
    knowledge_bindings.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.knowledge_base_id.cmp(&right.knowledge_base_id))
    });
    if knowledge_bindings.len() > 4 {
        return Err(RuntimeError::Config(
            "agentPolicy.knowledge.bindings permits at most four active bindings".into(),
        ));
    }

    let domain = AgentRepository::with_execution_authority(
        pool.clone(),
        AgentRuntimeAuthority {
            host_id,
            agent_def_id,
            definition_version,
            publication_id: agent_config.runtime_policy.publication_id,
            content_digest: agent_config.runtime_policy.content_digest.clone(),
            definition_digest: agent_config
                .agent_policy
                .policy_snapshot
                .definition_digest
                .clone(),
            environment: agent_config.runtime_policy.env_tag.clone(),
            service_id: agent_config.runtime_policy.service_id.clone(),
            instance_id: agent_config.portal_association.runtime_instance_id,
            policy_snapshot_id: agent_config.agent_policy.policy_snapshot.snapshot_id,
            policy_version: i64::try_from(agent_config.runtime_policy.policy_version).map_err(
                |_| RuntimeError::Config("runtimePolicy.policyVersion is too large".into()),
            )?,
            policy_digest: policy_digest.clone(),
            data_boundary_digest: agent_config
                .agent_policy
                .policy_snapshot
                .data_boundary_digest
                .clone(),
            model_provider: agent_config.agent_policy.model.provider.clone(),
            model_name: agent_config.agent_policy.model.alias.clone(),
            quota_policies: agent_config.agent_policy.execution.quota_policies.clone(),
            model_rates: agent_config.agent_policy.execution.model_rates.clone(),
            service_pools: agent_config.agent_policy.execution.service_pools.clone(),
            edge_runner_bindings: agent_config
                .agent_policy
                .execution
                .edge_runner_bindings
                .clone(),
        },
        execution_client,
    );
    let turn_dispatch = TurnDispatchCoordinator::new(domain.clone());
    turn_dispatch.spawn(host_id);
    let state = Arc::new(AgentState {
        policy_snapshot: agent_config.agent_policy.policy_snapshot.clone(),
        system_prompt: agent_config.compiled_system_prompt(),
        agent_config,
        llm_gateway_token: portal_token,
        llm_gateway_client,
        default_temperature: model_provider_config.temperature,
        mcp_client,
        configured_catalog,
        knowledge_bindings,
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
        coding_profile,
        personal_profile_digest,
        knowledge_client,
        native_a2a,
        outbound_a2a,
    });
    state.domain.spawn_result_reconciler();
    state.spawn_native_artifact_retention();

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

fn build_agent_http_client(
    ca_cert_pem: Option<&[u8]>,
    verify_hostname: bool,
    timeout: Duration,
) -> Result<reqwest::Client, RuntimeError> {
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(10));
    if let Some(pem) = ca_cert_pem {
        let certificates = light_client::parse_ca_cert_bundle(pem).map_err(|error| {
            RuntimeError::Config(format!("invalid outbound CA certificate bundle: {error}"))
        })?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if !verify_hostname {
        builder = builder.danger_accept_invalid_hostnames(true);
    }
    builder
        .build()
        .map_err(|error| RuntimeError::Config(format!("failed to build outbound client: {error}")))
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
    let watcher = ShutdownWatcher::install().context("failed to install shutdown handlers")?;
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

    runtime
        .run_until_shutdown(watcher)
        .await
        .context("agent lifecycle failed")?;

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
            portal_registry::unsupported_method_response(method)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AgentCatalogCache, AgentLimits, CatalogCacheKey, CatalogSkill, CatalogTool,
        CatalogToolPolicy, ChatMessage, EffectiveAgentCatalog, MAX_SESSION_MESSAGES,
        McpClientConfig, ModelProviderConfig, SessionOwner, TurnDispatchCoordinator,
        agent_ca_cert_path_from_config, apply_authoritative_system_prompt,
        bind_authenticated_principal, bound_untrusted_text, choose_model,
        collect_catalog_tool_names, collect_policy_diagnostics, filter_gateway_tools,
        normalize_provider_id, normalized_claim_values, parse_tool_arguments, select_catalog_tools,
        trim_history, validate_repository_input_uri, validate_session_owner,
    };
    use config_loader::{ConfigLoader, EmbeddedConfigFile};
    use light_agent::agent_config::AgentConfig;
    use light_agent::domain::AgentRepository;
    use light_runtime::config::{
        BootstrapConfig, ClientConfig, PortalRegistryConfig, ServerConfig,
    };
    use light_security::{AuthPrincipal, SecurityConfig};
    use mcp_client::McpTool;
    use serde::de::DeserializeOwned;
    use sqlx::postgres::PgPoolOptions;
    use std::path::PathBuf;

    #[test]
    fn native_a2a_application_errors_use_http_200() {
        let response = super::native_a2a_error(
            serde_json::Value::Null,
            -32003,
            "denied",
            axum::http::StatusCode::FORBIDDEN,
        );
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
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

    #[test]
    fn embedded_runtime_templates_resolve_and_match_typed_configs() {
        fn resolve<T: DeserializeOwned>(
            name: &'static str,
            content: &'static str,
            values: &str,
        ) -> T {
            let loader = ConfigLoader::new(values, None, None).expect("config loader");
            let mut value = loader
                .load_embedded_file(&EmbeddedConfigFile { name, content })
                .unwrap_or_else(|error| panic!("load {name}: {error}"));
            loader
                .resolve_value(&mut value)
                .unwrap_or_else(|error| panic!("resolve {name}: {error}"));
            serde_yaml::from_value(value)
                .unwrap_or_else(|error| panic!("deserialize {name}: {error}"))
        }

        let startup: BootstrapConfig =
            resolve("startup.yml", include_str!("../config/startup.yml"), "");
        let client: ClientConfig = resolve("client.yml", include_str!("../config/client.yml"), "");
        let server: ServerConfig = resolve("server.yml", include_str!("../config/server.yml"), "");
        let portal_registry: PortalRegistryConfig = resolve(
            "portal-registry.yml",
            include_str!("../config/portal-registry.yml"),
            "",
        );
        let security: SecurityConfig =
            resolve("security.yml", include_str!("../config/security.yml"), "");
        let mcp_client: McpClientConfig = resolve(
            "mcp-client.yml",
            include_str!("../config/mcp-client.yml"),
            "",
        );
        let agent: AgentConfig = resolve(
            "agent.yml",
            include_str!("../config/agent.yml"),
            r#"
operationalStore.bindingId: 00000000-0000-0000-0000-000000000010
operationalStore.scopeId: 00000000-0000-0000-0000-000000000003
operationalStore.hostId: 00000000-0000-0000-0000-000000000003
runtimePolicy.publicationId: 00000000-0000-0000-0000-000000000001
runtimePolicy.policySnapshotId: 00000000-0000-0000-0000-000000000002
runtimePolicy.host: dev.lightapi.net
runtimePolicy.serviceId: com.networknt.agent.account-1.0.0
runtimePolicy.envTag: dev
portalAssociation.runtimeInstanceId: 00000000-0000-0000-0000-000000000004
runtimePolicy.createdAt: 2026-08-26T12:00:00Z
runtimePolicy.validFrom: 2026-08-26T12:00:00Z
runtimePolicy.refreshAfter: 2026-08-26T12:30:00Z
runtimePolicy.expiresAt: 2026-08-26T13:00:00Z
agentPolicy.agentDefId: 00000000-0000-0000-0000-000000000005
agentPolicy.policySnapshot.snapshotId: 00000000-0000-0000-0000-000000000002
"#,
        );
        let override_values = r#"
client.caCertPath: config/customer-ca.pem
client.verifyHostname: false
server.tlsCertPath: config/server.pem
security.skipPathPrefixes: [/health]
"#;
        let overridden_client: ClientConfig = resolve(
            "client.yml",
            include_str!("../config/client.yml"),
            override_values,
        );
        let overridden_server: ServerConfig = resolve(
            "server.yml",
            include_str!("../config/server.yml"),
            override_values,
        );
        let overridden_security: SecurityConfig = resolve(
            "security.yml",
            include_str!("../config/security.yml"),
            override_values,
        );

        assert!(startup.external_config_dir.is_none());
        assert!(client.tls.verify_hostname);
        assert_eq!(client.request.timeout, 3_000);
        assert_eq!(client.oauth.token.early_refresh_retry_delay, 4_000);
        assert_eq!(client.oauth.sign.uri, "/oauth2/sign");
        assert_eq!(client.oauth.deref.uri, "/oauth2/deref");
        assert!(server.tls_cert_path.is_none());
        assert!(portal_registry.control_candidates.is_none());
        assert_eq!(security.swt_client_id_header, "swt-client-id");
        assert_eq!(security.jwt_cache_full_size, 1_000);
        assert_eq!(mcp_client.timeout_ms, 5_000);
        assert_eq!(
            agent.runtime_policy.publication_id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").expect("publication id")
        );
        assert!(agent.agent_policy.execution.coding_profile.is_none());
        assert_eq!(
            agent.agent_policy.catalog.effective_catalog,
            Some(serde_json::json!({}))
        );
        assert!(agent.agent_policy.knowledge.retrieval.filters.is_none());
        assert_eq!(
            overridden_client.tls.ca_cert_path,
            Some(PathBuf::from("config/customer-ca.pem"))
        );
        assert!(!overridden_client.tls.verify_hostname);
        assert_eq!(
            overridden_server.tls_cert_path,
            Some(PathBuf::from("config/server.pem"))
        );
        assert_eq!(overridden_security.skip_path_prefixes, ["/health"]);
    }

    #[test]
    fn coding_repository_input_is_confined_to_operator_spool() {
        let prefix = "file:///var/lib/light-agent/repositories/";
        assert!(
            validate_repository_input_uri(
                "file:///var/lib/light-agent/repositories/tenant/repo.bundle",
                prefix
            )
            .is_ok()
        );
        assert!(validate_repository_input_uri("file:///etc/shadow", prefix).is_err());
        assert!(
            validate_repository_input_uri("https://attacker.invalid/repository.bundle", prefix)
                .is_err()
        );
    }

    #[test]
    fn authoritative_system_prompt_requires_and_preserves_a_user_turn() {
        let mut empty = Vec::new();
        assert!(apply_authoritative_system_prompt(&mut empty, "policy").is_err());

        let mut messages = vec![
            ChatMessage::system("untrusted persisted system"),
            ChatMessage::user("hello"),
        ];
        assert_eq!(
            apply_authoritative_system_prompt(&mut messages, "published policy").unwrap(),
            "hello"
        );
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "published policy");
        assert_eq!(messages[1].role, "user");
        assert_eq!(messages[1].content, "hello");
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
    }

    #[test]
    fn delegated_identity_claims_are_normalized_and_deduplicated() {
        let claims = serde_json::json!({
            "groups": ["engineering", "reader", "engineering"],
            "organizations": "tenant-b,tenant-a tenant-b"
        });
        assert_eq!(
            normalized_claim_values(&claims, &["groups"]),
            vec!["engineering", "reader"]
        );
        assert_eq!(
            normalized_claim_values(&claims, &["organizations"]),
            vec!["tenant-a", "tenant-b"]
        );
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
