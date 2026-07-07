use crate::access_control::{
    AccessControlRuntime, AccessDecision, ToolVisibility, ToolsListAccessControlConfig,
    ToolsListAccessControlMode, load_access_control_runtime,
};
use crate::config_util::deserialize_typed_list;
use crate::direct_registry::{direct_registry_match, validate_direct_registry_protocol};
use crate::security::AuthPrincipal;
use crate::token::{CLIENT_FILE, load_client_config};
use async_trait::async_trait;
use light_client::{ClientConfig, ClientFactory, EndpointOptions};
use light_runtime::{
    DirectRegistryConfig, DiscoveryNode, DiscoverySnapshot, DiscoverySubscription, ModuleKind,
    PortalRegistryClient, RuntimeConfig, RuntimeError,
};
use regex::Regex;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value as JsonValue, json};
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;
use url::{Url, form_urlencoded};

pub const MCP_ROUTER_FILE: &str = "mcp-router.yml";
pub const MCP_ROUTER_LEGACY_FILE: &str = "mcp-router.yaml";
pub const MCP_ROUTER_MODULE_ID: &str = "light-pingora/mcp-router";
pub const MCP_ROUTER_CONFIG_NAME: &str = "mcp-router";

const DEFAULT_MCP_PATH: &str = "/mcp";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MCP_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const MCP_SESSION_PURGE_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_MCP_MAX_FRONTEND_SESSIONS: usize = 10_000;
const DEFAULT_MCP_MAX_FRONTEND_SESSIONS_PER_CLIENT: usize = 100;
const JSON_CONTENT_TYPE: &str = "application/json";
const EVENT_STREAM_CONTENT_TYPE: &str = "text/event-stream";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRouterConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_mcp_path")]
    pub path: String,
    #[serde(default = "default_max_frontend_sessions", alias = "maxSessions")]
    pub max_sessions: usize,
    #[serde(
        default = "default_max_frontend_sessions_per_client",
        alias = "maxSessionsPerClient"
    )]
    pub max_sessions_per_client: usize,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub tools: Vec<McpToolConfig>,
}

impl Default for McpRouterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: default_mcp_path(),
            max_sessions: default_max_frontend_sessions(),
            max_sessions_per_client: default_max_frontend_sessions_per_client(),
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolConfig {
    pub name: String,
    /// The real tool name understood by the backend MCP server (operationId).
    /// When present this is used in `tools/call` requests forwarded to the backend,
    /// while `name` is the gateway-facing identifier exposed to agents.
    #[serde(default, alias = "endpointName")]
    pub endpoint_name: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default, alias = "serviceId")]
    pub service_id: Option<String>,
    #[serde(default, alias = "envTag")]
    pub env_tag: Option<String>,
    #[serde(default, alias = "targetHost")]
    pub target_host: Option<String>,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub method: McpHttpMethod,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default, alias = "apiType")]
    pub api_type: McpToolType,
    #[serde(default = "default_input_schema", alias = "inputSchema")]
    pub input_schema: JsonValue,
    #[serde(skip)]
    pub input_schema_configured: bool,
    #[serde(
        default = "default_object",
        alias = "toolMetadata",
        deserialize_with = "deserialize_json_value"
    )]
    pub tool_metadata: JsonValue,
}

impl<'de> Deserialize<'de> for McpToolConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawMcpToolConfig {
            name: String,
            #[serde(default, alias = "endpointName")]
            endpoint_name: Option<String>,
            #[serde(default)]
            description: String,
            #[serde(default)]
            protocol: Option<String>,
            #[serde(default, alias = "serviceId")]
            service_id: Option<String>,
            #[serde(default, alias = "envTag")]
            env_tag: Option<String>,
            #[serde(default, alias = "targetHost")]
            target_host: Option<String>,
            #[serde(default)]
            path: String,
            #[serde(default)]
            method: McpHttpMethod,
            #[serde(default)]
            endpoint: Option<String>,
            #[serde(default, alias = "apiType")]
            api_type: McpToolType,
            #[serde(
                default,
                alias = "inputSchema",
                deserialize_with = "deserialize_optional_json_value"
            )]
            input_schema: Option<JsonValue>,
            #[serde(
                default = "default_object",
                alias = "toolMetadata",
                deserialize_with = "deserialize_json_value"
            )]
            tool_metadata: JsonValue,
        }

        let raw = RawMcpToolConfig::deserialize(deserializer)?;
        let input_schema_configured = raw.input_schema.is_some();
        Ok(Self {
            name: raw.name,
            endpoint_name: raw.endpoint_name,
            description: raw.description,
            protocol: raw.protocol,
            service_id: raw.service_id,
            env_tag: raw.env_tag,
            target_host: raw.target_host,
            path: raw.path,
            method: raw.method,
            endpoint: raw.endpoint,
            api_type: raw.api_type,
            input_schema: raw.input_schema.unwrap_or_else(default_input_schema),
            input_schema_configured,
            tool_metadata: raw.tool_metadata,
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpHttpMethod {
    #[default]
    Get,
    Post,
    Put,
    Patch,
    Delete,
    Head,
    Options,
    Call,
}

impl McpHttpMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Call => "CALL",
        }
    }

    fn as_reqwest(self) -> reqwest::Method {
        match self {
            Self::Get => reqwest::Method::GET,
            Self::Post => reqwest::Method::POST,
            Self::Put => reqwest::Method::PUT,
            Self::Patch => reqwest::Method::PATCH,
            Self::Delete => reqwest::Method::DELETE,
            Self::Head => reqwest::Method::HEAD,
            Self::Options => reqwest::Method::OPTIONS,
            Self::Call => reqwest::Method::POST,
        }
    }

    fn sends_json_body(self) -> bool {
        !matches!(self, Self::Get | Self::Head)
    }
}

impl Serialize for McpHttpMethod {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for McpHttpMethod {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_uppercase().as_str() {
            "GET" => Ok(Self::Get),
            "POST" => Ok(Self::Post),
            "PUT" => Ok(Self::Put),
            "PATCH" => Ok(Self::Patch),
            "DELETE" => Ok(Self::Delete),
            "HEAD" => Ok(Self::Head),
            "OPTIONS" => Ok(Self::Options),
            "CALL" => Ok(Self::Call),
            method => Err(D::Error::custom(format!(
                "unsupported MCP tool HTTP method `{method}`"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum McpToolType {
    #[default]
    Http,
    Mcp,
}

impl McpToolType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Mcp => "mcp",
        }
    }
}

impl Serialize for McpToolType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for McpToolType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "http" | "rest" | "openapi" => Ok(Self::Http),
            "mcp" => Ok(Self::Mcp),
            api_type => Err(D::Error::custom(format!(
                "unsupported MCP tool apiType `{api_type}`"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpHttpRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpHttpResponse {
    pub status: u16,
    pub content_type: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub streamed: bool,
}

#[derive(Debug, Clone, Default)]
pub struct McpRequestContext {
    pub auth: Option<AuthPrincipal>,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone)]
struct McpGatewaySession {
    protocol_version: String,
    client_key: String,
    last_accessed: Instant,
    backend_sessions: BTreeMap<String, McpBackendSession>,
}

impl McpGatewaySession {
    fn new(protocol_version: String, client_key: String) -> Self {
        Self {
            protocol_version,
            client_key,
            last_accessed: Instant::now(),
            backend_sessions: BTreeMap::new(),
        }
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_accessed) >= MCP_SESSION_IDLE_TIMEOUT
    }

    fn touch(&mut self, now: Instant) {
        self.last_accessed = now;
    }
}

#[derive(Debug, Default)]
struct McpSessionStore {
    sessions: BTreeMap<String, McpGatewaySession>,
    client_session_counts: BTreeMap<String, usize>,
}

impl McpSessionStore {
    fn len(&self) -> usize {
        self.sessions.len()
    }

    fn get(&self, session_id: &str) -> Option<&McpGatewaySession> {
        self.sessions.get(session_id)
    }

    fn get_mut(&mut self, session_id: &str) -> Option<&mut McpGatewaySession> {
        self.sessions.get_mut(session_id)
    }

    #[cfg(test)]
    fn contains_key(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    fn client_session_count(&self, client_key: &str) -> usize {
        self.client_session_counts
            .get(client_key)
            .copied()
            .unwrap_or_default()
    }

    fn insert(
        &mut self,
        session_id: String,
        session: McpGatewaySession,
    ) -> Option<McpGatewaySession> {
        let client_key = session.client_key.clone();
        let replaced = self.sessions.insert(session_id, session);
        if let Some(replaced) = replaced.as_ref() {
            self.decrement_client_session_count(replaced.client_key.as_str());
        }
        *self.client_session_counts.entry(client_key).or_insert(0) += 1;
        replaced
    }

    fn remove(&mut self, session_id: &str) -> Option<McpGatewaySession> {
        let removed = self.sessions.remove(session_id)?;
        self.decrement_client_session_count(removed.client_key.as_str());
        Some(removed)
    }

    fn remove_expired(&mut self, now: Instant) -> Vec<McpBackendSession> {
        let expired_ids = self
            .sessions
            .iter()
            .filter_map(|(session_id, session)| session.is_expired(now).then(|| session_id.clone()))
            .collect::<Vec<_>>();
        expired_ids
            .into_iter()
            .filter_map(|session_id| self.remove(session_id.as_str()))
            .flat_map(|session| session.backend_sessions.into_values())
            .collect()
    }

    fn decrement_client_session_count(&mut self, client_key: &str) {
        let remove_entry = if let Some(count) = self.client_session_counts.get_mut(client_key) {
            *count = count.saturating_sub(1);
            *count == 0
        } else {
            false
        };
        if remove_entry {
            self.client_session_counts.remove(client_key);
        }
    }
}

#[derive(Debug, Clone)]
struct McpBackendSession {
    target_url: String,
    session_id: Option<String>,
    protocol_version: String,
    agent_headers: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
struct McpFrontendSession {
    id: String,
    protocol_version: String,
}

struct RemovedMcpSession {
    protocol_version: String,
    backend_sessions: Vec<McpBackendSession>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpResponseMode {
    Json,
    EventStream,
}

#[derive(Debug, Clone, Default)]
struct ToolsListVisibilityCache {
    max_entries: usize,
    entries: BTreeMap<String, Vec<String>>,
    order: VecDeque<String>,
}

impl ToolsListVisibilityCache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&mut self, key: &str) -> Option<Vec<String>> {
        let value = self.entries.get(key).cloned()?;
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, value: Vec<String>) {
        if self.max_entries == 0 {
            return;
        }
        self.entries.insert(key.clone(), value);
        self.touch(key.as_str());
        while self.entries.len() > self.max_entries {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            if self.entries.remove(expired.as_str()).is_some() {
                continue;
            }
        }
    }

    fn touch(&mut self, key: &str) {
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.to_string());
    }
}

#[derive(Clone)]
pub struct McpRouterRuntime {
    config: McpRouterConfig,
    tools: BTreeMap<String, McpToolConfig>,
    client: reqwest::Client,
    direct_registry: DirectRegistryConfig,
    discovery: Option<Arc<dyn McpDiscoveryResolver>>,
    policy: Option<Arc<AccessControlRuntime>>,
    sessions: Arc<AsyncMutex<McpSessionStore>>,
    tools_list_cache: Arc<AsyncMutex<ToolsListVisibilityCache>>,
    last_session_purge: Arc<AsyncMutex<Option<Instant>>>,
    next_backend_request_id: Arc<AtomicU64>,
}

impl fmt::Debug for McpRouterRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpRouterRuntime")
            .field("path", &self.config.path)
            .field("tool_count", &self.tools.len())
            .field(
                "direct_registry_entries",
                &self.direct_registry.direct_urls.len(),
            )
            .field("discovery", &self.discovery.is_some())
            .field("policy", &self.policy.is_some())
            .field(
                "tools_list_cache_entries",
                &self
                    .tools_list_cache
                    .try_lock()
                    .ok()
                    .map(|cache| cache.len()),
            )
            .field(
                "session_count",
                &self.sessions.try_lock().ok().map(|store| store.len()),
            )
            .finish()
    }
}

#[async_trait]
pub trait McpDiscoveryResolver: Send + Sync {
    async fn lookup_discovery(
        &self,
        subscription: DiscoverySubscription,
    ) -> Result<DiscoverySnapshot, String>;
}

#[async_trait]
impl McpDiscoveryResolver for PortalRegistryClient {
    async fn lookup_discovery(
        &self,
        subscription: DiscoverySubscription,
    ) -> Result<DiscoverySnapshot, String> {
        PortalRegistryClient::lookup_discovery(self, subscription)
            .await
            .map_err(|error| error.to_string())
    }
}

impl McpRouterRuntime {
    pub fn new(config: McpRouterConfig) -> Result<Self, RuntimeError> {
        Self::new_with_discovery(config, None)
    }

    pub fn new_with_policy(
        config: McpRouterConfig,
        policy: Option<Arc<AccessControlRuntime>>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_and_policy(config, None, policy)
    }

    pub fn new_with_discovery(
        config: McpRouterConfig,
        discovery: Option<Arc<dyn McpDiscoveryResolver>>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_and_policy(config, discovery, None)
    }

    pub fn new_with_discovery_and_policy(
        config: McpRouterConfig,
        discovery: Option<Arc<dyn McpDiscoveryResolver>>,
        policy: Option<Arc<AccessControlRuntime>>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_policy_and_direct_registry(
            config,
            discovery,
            policy,
            DirectRegistryConfig::default(),
        )
    }

    pub fn new_with_discovery_policy_and_direct_registry(
        config: McpRouterConfig,
        discovery: Option<Arc<dyn McpDiscoveryResolver>>,
        policy: Option<Arc<AccessControlRuntime>>,
        direct_registry: DirectRegistryConfig,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_policy_direct_registry_and_client_config(
            config,
            discovery,
            policy,
            direct_registry,
            ClientConfig::default(),
        )
    }

    fn new_with_discovery_policy_direct_registry_and_client_config(
        config: McpRouterConfig,
        discovery: Option<Arc<dyn McpDiscoveryResolver>>,
        policy: Option<Arc<AccessControlRuntime>>,
        direct_registry: DirectRegistryConfig,
        client_config: ClientConfig,
    ) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        let tools = config
            .tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect::<BTreeMap<_, _>>();
        let client = ClientFactory::from_config(&client_config)
            .reqwest_client(EndpointOptions::default())
            .map_err(|error| {
                RuntimeError::Unsupported(format!("invalid MCP HTTP client: {error}"))
            })?;
        let tools_list_cache_entries = policy
            .as_ref()
            .map(|policy| policy.tools_list_access_control().max_cache_entries)
            .unwrap_or_else(|| ToolsListAccessControlConfig::default().max_cache_entries);
        Ok(Self {
            config,
            tools,
            client,
            direct_registry,
            discovery,
            policy,
            sessions: Arc::new(AsyncMutex::new(McpSessionStore::default())),
            tools_list_cache: Arc::new(AsyncMutex::new(ToolsListVisibilityCache::new(
                tools_list_cache_entries,
            ))),
            last_session_purge: Arc::new(AsyncMutex::new(None)),
            next_backend_request_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn config(&self) -> &McpRouterConfig {
        &self.config
    }

    fn next_backend_request_id(&self) -> u64 {
        self.next_backend_request_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn matches_path(&self, path: &str) -> bool {
        if !self.config.enabled {
            return false;
        }
        // Strip any query string so the caller can pass the full URI.
        let path_only = path.split('?').next().unwrap_or(path);
        path_only == self.config.path
    }

    /// Shares the live session store and purge timestamp with a freshly loaded
    /// runtime, so active client sessions are not lost when configuration is
    /// reloaded.  Mirrors `WebSocketRouterRuntime::preserve_state_from`.
    pub fn preserve_state_from(&mut self, previous: &Self) {
        self.sessions = Arc::clone(&previous.sessions);
        self.last_session_purge = Arc::clone(&previous.last_session_purge);
        self.next_backend_request_id = Arc::clone(&previous.next_backend_request_id);
    }

    pub async fn handle_request(
        &self,
        request: McpHttpRequest,
    ) -> Result<Option<McpHttpResponse>, RuntimeError> {
        self.handle_request_with_context(request, McpRequestContext::default())
            .await
    }

    pub async fn handle_request_with_context(
        &self,
        request: McpHttpRequest,
        context: McpRequestContext,
    ) -> Result<Option<McpHttpResponse>, RuntimeError> {
        if !self.matches_path(request.path.as_str()) {
            return Ok(None);
        }

        let method = request.method.to_ascii_uppercase();
        match method.as_str() {
            "POST" => self.handle_post(request, &context).await.map(Some),
            "GET" => Ok(Some(method_not_allowed_response())),
            "DELETE" => self.handle_delete(request).await.map(Some),
            _ => Ok(Some(method_not_allowed_response())),
        }
    }

    async fn handle_post(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
    ) -> Result<McpHttpResponse, RuntimeError> {
        let Some(response_mode) = preferred_response_mode(&request.headers) else {
            return json_error_response(
                406,
                JsonValue::Null,
                -32600,
                "Accept header must allow application/json or text/event-stream",
            );
        };
        if let Some(version) = first_header(&request.headers, "mcp-protocol-version")
            && !protocol_version_supported(version.as_str())
        {
            return rpc_error_response(
                response_mode,
                400,
                JsonValue::Null,
                -32600,
                format!("unsupported MCP protocol version `{version}`"),
            );
        }
        self.purge_expired_sessions(false).await;

        let payload = match serde_json::from_slice::<JsonValue>(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return rpc_error_response(
                    response_mode,
                    400,
                    JsonValue::Null,
                    -32700,
                    format!("parse error: {error}"),
                );
            }
        };
        if payload.is_array() {
            return rpc_error_response(
                response_mode,
                400,
                JsonValue::Null,
                -32600,
                "JSON-RPC batch requests are not supported",
            );
        }
        let Some(message) = payload.as_object() else {
            return rpc_error_response(
                response_mode,
                400,
                JsonValue::Null,
                -32600,
                "invalid JSON-RPC request",
            );
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(JsonValue::as_str);
        if method.is_none() {
            return Ok(accepted_response());
        }
        let method = method.unwrap_or_default();
        let frontend_session = if method == "initialize" {
            None
        } else {
            match self
                .validate_frontend_session(request.path.as_str(), &request.headers)
                .await
            {
                Ok(session) => Some(session),
                Err(error) => {
                    return rpc_error_response(
                        response_mode,
                        error.status,
                        id.clone().unwrap_or(JsonValue::Null),
                        error.code,
                        error.message,
                    );
                }
            }
        };
        if id.is_none() {
            return Ok(accepted_response_with_protocol_version(
                frontend_session
                    .as_ref()
                    .map(|session| session.protocol_version.as_str()),
            ));
        }
        if message.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
            return response_with_protocol_version(
                rpc_error_response(
                    response_mode,
                    200,
                    id.unwrap_or(JsonValue::Null),
                    -32600,
                    "invalid JSON-RPC version",
                ),
                frontend_session
                    .as_ref()
                    .map(|session| session.protocol_version.as_str()),
            );
        }

        let id = id.unwrap_or(JsonValue::Null);
        match method {
            "initialize" => {
                let result = self.initialize_result(message);
                let session_id = match self.create_frontend_session(message, context).await {
                    Ok(session_id) => session_id,
                    Err(error) => {
                        return rpc_error_response(
                            response_mode,
                            error.status,
                            id,
                            error.code,
                            error.message,
                        );
                    }
                };
                initialize_response(response_mode, 200, id, result, session_id)
            }
            "tools/list" => response_with_protocol_version(
                rpc_result_response(
                    response_mode,
                    200,
                    id,
                    self.tools_list_result(message, &request.headers, context)
                        .await,
                ),
                frontend_session
                    .as_ref()
                    .map(|session| session.protocol_version.as_str()),
            ),
            "tools/call" => match self
                .handle_tool_call(
                    message,
                    &request.headers,
                    frontend_session
                        .as_ref()
                        .map(|session| session.id.as_str())
                        .unwrap_or_default(),
                    context,
                )
                .await
            {
                Ok(result) => response_with_protocol_version(
                    rpc_result_response(response_mode, 200, id, result),
                    frontend_session
                        .as_ref()
                        .map(|session| session.protocol_version.as_str()),
                ),
                Err(error) => response_with_protocol_version(
                    rpc_error_response(response_mode, 200, id, error.code, error.message),
                    frontend_session
                        .as_ref()
                        .map(|session| session.protocol_version.as_str()),
                ),
            },
            _ => response_with_protocol_version(
                rpc_error_response(
                    response_mode,
                    200,
                    id,
                    -32601,
                    format!("method `{method}` not found"),
                ),
                frontend_session
                    .as_ref()
                    .map(|session| session.protocol_version.as_str()),
            ),
        }
    }

    async fn handle_delete(
        &self,
        request: McpHttpRequest,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.purge_expired_sessions(false).await;
        let removed_session = match self
            .remove_frontend_session(request.path.as_str(), &request.headers)
            .await
        {
            Ok(session) => session,
            Err(error) => {
                return rpc_error_response(
                    McpResponseMode::Json,
                    error.status,
                    JsonValue::Null,
                    error.code,
                    error.message,
                );
            }
        };
        if !removed_session.backend_sessions.is_empty() {
            self.terminate_backend_sessions_in_background(removed_session.backend_sessions);
        }
        Ok(accepted_response_with_protocol_version(Some(
            removed_session.protocol_version.as_str(),
        )))
    }

    fn initialize_result(&self, message: &serde_json::Map<String, JsonValue>) -> JsonValue {
        let requested = requested_protocol_version(message);
        json!({
            "protocolVersion": requested.unwrap_or(DEFAULT_PROTOCOL_VERSION),
            "capabilities": {
                "tools": {
                    "listChanged": true
                }
            },
            "serverInfo": {
                "name": "light-gateway-mcp",
                "version": env!("CARGO_PKG_VERSION")
            }
        })
    }

    async fn create_frontend_session(
        &self,
        message: &serde_json::Map<String, JsonValue>,
        context: &McpRequestContext,
    ) -> Result<String, McpSessionError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let protocol_version = requested_protocol_version(message)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
            .to_string();
        let client_key = frontend_client_key(message, context);
        let mut forced_purge = false;
        loop {
            let mut store = self.sessions.lock().await;
            if store.len() >= self.config.max_sessions && !forced_purge {
                drop(store);
                self.purge_expired_sessions(true).await;
                forced_purge = true;
                continue;
            }
            if store.len() >= self.config.max_sessions {
                return Err(McpSessionError {
                    status: 503,
                    code: -32000,
                    message: "MCP session store is full".to_string(),
                });
            }
            let client_sessions = store.client_session_count(client_key.as_str());
            if client_sessions >= self.config.max_sessions_per_client && !forced_purge {
                drop(store);
                self.purge_expired_sessions(true).await;
                forced_purge = true;
                continue;
            }
            if client_sessions >= self.config.max_sessions_per_client {
                return Err(McpSessionError {
                    status: 429,
                    code: -32000,
                    message: "MCP client session limit exceeded".to_string(),
                });
            }
            store.insert(
                session_id.clone(),
                McpGatewaySession::new(protocol_version, client_key),
            );
            return Ok(session_id);
        }
    }

    async fn validate_frontend_session(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> Result<McpFrontendSession, McpSessionError> {
        let session_id =
            session_id_from_path_and_headers(path, headers).ok_or_else(|| McpSessionError {
                status: 400,
                code: -32600,
                message: "missing MCP session id".to_string(),
            })?;
        let now = Instant::now();
        let mut expired_backend_sessions = Vec::new();
        let result = {
            let mut store = self.sessions.lock().await;
            if store
                .get(session_id.as_str())
                .is_some_and(|session| session.is_expired(now))
            {
                if let Some(session) = store.remove(session_id.as_str()) {
                    expired_backend_sessions = session.backend_sessions.into_values().collect();
                }
                Err(McpSessionError {
                    status: 400,
                    code: -32000,
                    message: "unknown MCP session id".to_string(),
                })
            } else if let Some(session) = store.get_mut(session_id.as_str()) {
                session.touch(now);
                Ok(McpFrontendSession {
                    id: session_id,
                    protocol_version: session.protocol_version.clone(),
                })
            } else {
                Err(McpSessionError {
                    status: 400,
                    code: -32000,
                    message: "unknown MCP session id".to_string(),
                })
            }
        };
        if !expired_backend_sessions.is_empty() {
            self.terminate_backend_sessions_in_background(expired_backend_sessions);
        }
        result
    }

    async fn purge_expired_sessions(&self, force: bool) {
        let now = Instant::now();
        if !force {
            let mut last_purge = self.last_session_purge.lock().await;
            if last_purge.is_some_and(|last| {
                now.saturating_duration_since(last) < MCP_SESSION_PURGE_INTERVAL
            }) {
                return;
            }
            *last_purge = Some(now);
        } else {
            *self.last_session_purge.lock().await = Some(now);
        }
        let mut store = self.sessions.lock().await;
        let backend_sessions = store.remove_expired(now);
        drop(store);
        if !backend_sessions.is_empty() {
            self.terminate_backend_sessions_in_background(backend_sessions);
        }
    }

    async fn remove_frontend_session(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> Result<RemovedMcpSession, McpSessionError> {
        let session_id =
            session_id_from_path_and_headers(path, headers).ok_or_else(|| McpSessionError {
                status: 400,
                code: -32600,
                message: "missing MCP session id".to_string(),
            })?;
        let mut store = self.sessions.lock().await;
        let session = store
            .remove(session_id.as_str())
            .ok_or_else(|| McpSessionError {
                status: 400,
                code: -32000,
                message: "unknown MCP session id".to_string(),
            })?;
        Ok(RemovedMcpSession {
            protocol_version: session.protocol_version,
            backend_sessions: session.backend_sessions.into_values().collect(),
        })
    }

    async fn tools_list_result(
        &self,
        message: &serde_json::Map<String, JsonValue>,
        agent_headers: &[(String, String)],
        context: &McpRequestContext,
    ) -> JsonValue {
        let query = message
            .get("params")
            .and_then(|params| params.get("query").or_else(|| params.get("intent")))
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_lowercase());
        if let Some(cache_key) = self.tools_list_cache_key(query.as_deref(), agent_headers, context)
            && let Some(tool_names) = self.tools_list_cache.lock().await.get(cache_key.as_str())
        {
            return self.tools_list_response_from_names(tool_names);
        }
        let candidates = self
            .tools
            .values()
            .filter(|tool| {
                query.as_deref().is_none_or(|query| {
                    tool.name.to_ascii_lowercase().contains(query)
                        || tool.description.to_ascii_lowercase().contains(query)
                })
            })
            .collect::<Vec<_>>();
        let mut visible_tools = Vec::new();
        for (index, tool) in candidates.into_iter().enumerate() {
            if self
                .tool_visible_for_list(tool, index, agent_headers, context)
                .await
            {
                visible_tools.push(tool);
            }
        }
        let visible_tool_names = visible_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        if let Some(cache_key) = self.tools_list_cache_key(query.as_deref(), agent_headers, context)
        {
            self.tools_list_cache
                .lock()
                .await
                .insert(cache_key, visible_tool_names.clone());
        }
        self.tools_list_response_from_names(visible_tool_names)
    }

    fn tools_list_response_from_names(&self, tool_names: Vec<String>) -> JsonValue {
        let tools = tool_names
            .into_iter()
            .filter_map(|tool_name| self.tools.get(tool_name.as_str()))
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema
                })
            })
            .collect::<Vec<_>>();
        json!({ "tools": tools })
    }

    fn tools_list_cache_key(
        &self,
        query: Option<&str>,
        agent_headers: &[(String, String)],
        context: &McpRequestContext,
    ) -> Option<String> {
        let policy = self.policy.as_ref()?;
        let tools_list_config = policy.tools_list_access_control();
        if tools_list_config.mode == ToolsListAccessControlMode::None
            || tools_list_config.max_cache_entries == 0
        {
            return None;
        }
        let claims = policy.normalized_claims_for_visibility(context.auth.as_ref());
        let subject = json!({
            "clientId": context.auth.as_ref().and_then(|auth| auth.client_id.as_deref()),
            "userId": context.auth.as_ref().and_then(|auth| auth.user_id.as_deref()),
            "issuer": context.auth.as_ref().and_then(|auth| auth.issuer.as_deref()),
            "claims": claims,
            "headers": normalized_header_pairs(agent_headers),
        });
        Some(format!(
            "{:?}|{}|{}",
            tools_list_config.mode,
            query.unwrap_or_default(),
            stable_json_hash(&subject)
        ))
    }

    async fn tool_visible_for_list(
        &self,
        tool: &McpToolConfig,
        index: usize,
        agent_headers: &[(String, String)],
        context: &McpRequestContext,
    ) -> bool {
        let Some(policy) = self.policy.as_ref() else {
            return true;
        };
        let endpoint = tool_endpoint(tool);
        let tools_list_config = policy.tools_list_access_control();
        match tools_list_config.mode {
            ToolsListAccessControlMode::None => true,
            ToolsListAccessControlMode::Permission => matches!(
                policy.tool_visible(tool.name.as_str(), endpoint.as_str(), context.auth.as_ref()),
                ToolVisibility::Visible
            ),
            ToolsListAccessControlMode::Cel => {
                if index >= tools_list_config.max_cel_evaluations {
                    tracing::warn!(
                        target: "light_pingora::mcp",
                        toolName = %tool.name,
                        endpoint = %endpoint,
                        maxCelEvaluations = tools_list_config.max_cel_evaluations,
                        "mcp tools/list access-control skipped tool after maxCelEvaluations limit"
                    );
                    return false;
                }
                matches!(
                    policy
                        .authorize_tool(
                            tool.name.as_str(),
                            endpoint.as_str(),
                            agent_headers,
                            context.auth.as_ref(),
                            &json!({}),
                            context.correlation_id.as_deref(),
                        )
                        .await,
                    AccessDecision::Allowed
                )
            }
        }
    }

    async fn handle_tool_call(
        &self,
        message: &serde_json::Map<String, JsonValue>,
        agent_headers: &[(String, String)],
        frontend_session_id: &str,
        context: &McpRequestContext,
    ) -> Result<JsonValue, McpExecutionError> {
        let params = message
            .get("params")
            .and_then(JsonValue::as_object)
            .ok_or_else(|| {
                McpExecutionError::invalid_params("tools/call requires object params")
            })?;
        let name = params
            .get("name")
            .and_then(JsonValue::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| McpExecutionError::invalid_params("tools/call requires params.name"))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return Err(McpExecutionError::invalid_params(
                "tools/call params.arguments must be an object",
            ));
        }
        let tool = self.get_tool(name).ok_or_else(|| McpExecutionError {
            code: -32601,
            message: format!("tool `{name}` not found"),
        })?;
        let endpoint = tool_endpoint(tool);
        let started = Instant::now();
        let mut policy_outcome = "not_configured";

        if let Some(policy) = self.policy.as_ref() {
            match policy
                .authorize_tool(
                    tool.name.as_str(),
                    endpoint.as_str(),
                    agent_headers,
                    context.auth.as_ref(),
                    &arguments,
                    context.correlation_id.as_deref(),
                )
                .await
            {
                AccessDecision::Allowed => {
                    policy_outcome = "allowed";
                }
                AccessDecision::Denied(message) => {
                    policy_outcome = "denied";
                    log_mcp_tool_call(
                        tool,
                        endpoint.as_str(),
                        started,
                        "denied",
                        policy_outcome,
                        context,
                    );
                    return Err(McpExecutionError {
                        code: -32001,
                        message,
                    });
                }
            }
        }

        let masked_arguments = mask_tool_arguments(tool, &arguments);

        let execution = match tool.api_type {
            McpToolType::Http => {
                self.execute_http_tool(tool, &masked_arguments, agent_headers)
                    .await
            }
            McpToolType::Mcp => {
                self.execute_mcp_proxy_tool(
                    tool,
                    &masked_arguments,
                    agent_headers,
                    frontend_session_id,
                )
                .await
            }
        };
        let result = match execution {
            Ok(result) => result,
            Err(error) => {
                log_mcp_tool_call(
                    tool,
                    endpoint.as_str(),
                    started,
                    "error",
                    policy_outcome,
                    context,
                );
                return Err(error);
            }
        };

        let backend_result_is_error = is_mcp_error_result(&result);
        let (result, status) = if let Some(policy) = self.policy.as_ref() {
            let filtered = policy
                .filter_mcp_response(
                    tool.name.as_str(),
                    endpoint.as_str(),
                    agent_headers,
                    context.auth.as_ref(),
                    &masked_arguments,
                    context.correlation_id.as_deref(),
                    result,
                )
                .await;
            if !backend_result_is_error && is_mcp_error_result(&filtered) {
                tracing::warn!(
                    target: "light_pingora::mcp",
                    toolName = %tool.name,
                    endpoint = %endpoint,
                    policyOutcome = %policy_outcome,
                    correlationId = ?context.correlation_id,
                    error = %mcp_error_result_text(&filtered),
                    "mcp response filter returned error result"
                );
                (filtered, "filter_error")
            } else {
                (filtered, "success")
            }
        } else {
            (result, "success")
        };
        log_mcp_tool_call(
            tool,
            endpoint.as_str(),
            started,
            status,
            policy_outcome,
            context,
        );
        Ok(result)
    }

    async fn execute_http_tool(
        &self,
        tool: &McpToolConfig,
        arguments: &JsonValue,
        agent_headers: &[(String, String)],
    ) -> Result<JsonValue, McpExecutionError> {
        let method = effective_http_method(tool);
        self.execute_http_tool_with_method(tool, arguments, agent_headers, method)
            .await
    }

    async fn execute_http_tool_with_method(
        &self,
        tool: &McpToolConfig,
        arguments: &JsonValue,
        agent_headers: &[(String, String)],
        method: McpHttpMethod,
    ) -> Result<JsonValue, McpExecutionError> {
        let mut url = self.tool_target_url(tool).await?;

        let mut path_params = std::collections::HashMap::new();
        let mut query_params = std::collections::HashMap::new();
        let mut header_params = std::collections::HashMap::new();
        let mut cookie_params = std::collections::HashMap::new();
        let mut body_val: Option<&JsonValue> = None;

        let mut has_mapping = false;
        let mapping = tool
            .tool_metadata
            .get("routing")
            .and_then(|r| r.get("parameters"))
            .and_then(|p| p.as_object());

        if let Some(mapping) = mapping {
            has_mapping = true;
            if let Some(args_obj) = arguments.as_object() {
                for (key, val) in args_obj {
                    if let Some(loc) = mapping.get(key).and_then(|l| l.as_str()) {
                        match loc {
                            "path" => {
                                path_params.insert(key.clone(), val);
                            }
                            "query" => {
                                query_params.insert(key.clone(), val);
                            }
                            "header" => {
                                header_params.insert(key.clone(), val);
                            }
                            "cookie" => {
                                cookie_params.insert(key.clone(), val);
                            }
                            "body" => {
                                body_val = Some(val);
                            }
                            _ => {}
                        }
                    } else {
                        if key == "body" {
                            body_val = Some(val);
                        } else if matches!(method, McpHttpMethod::Get | McpHttpMethod::Head) {
                            query_params.insert(key.clone(), val);
                        }
                    }
                }
            }
        }

        // 1. Substitute path parameters in URL path
        if has_mapping && !path_params.is_empty() {
            let mut path = url.path().to_string();
            for (key, val) in path_params {
                let val_str = match val {
                    JsonValue::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let encoded_val = Self::percent_encode_path_segment(&val_str);
                let placeholder1 = format!("{{{}}}", key);
                path = path.replace(&placeholder1, &encoded_val);
                let placeholder2 = format!("%7B{}%7D", key);
                path = path.replace(&placeholder2, &encoded_val);
            }
            url.set_path(&path);
        }

        // 2. Query parameters
        if has_mapping {
            if !query_params.is_empty() {
                let mut query_pairs = url.query_pairs_mut();
                for (key, val) in query_params {
                    if val.is_null() {
                        continue;
                    }
                    query_pairs.append_pair(&key, &json_value_to_query(val));
                }
            }
        } else if matches!(method, McpHttpMethod::Get | McpHttpMethod::Head) {
            append_query_arguments(&mut url, arguments);
        }

        let request_url = url.to_string();
        let request_method = method.as_reqwest();

        // 3. Headers & Cookies
        let mut request_headers = outbound_headers(agent_headers)?;
        if has_mapping {
            for (key, val) in header_params {
                if val.is_null() {
                    continue;
                }
                let val_str = json_value_to_query(val);
                if let Ok(header_name) = reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
                    if let Ok(header_val) = reqwest::header::HeaderValue::from_str(&val_str) {
                        request_headers.insert(header_name, header_val);
                    }
                }
            }
            if !cookie_params.is_empty() {
                let mut cookies = Vec::new();
                for (key, val) in cookie_params {
                    if val.is_null() {
                        continue;
                    }
                    let val_str = json_value_to_query(val);
                    cookies.push(format!("{}={}", key, val_str));
                }
                if !cookies.is_empty() {
                    let cookie_header_val = cookies.join("; ");
                    if let Ok(header_val) =
                        reqwest::header::HeaderValue::from_str(&cookie_header_val)
                    {
                        request_headers.insert(reqwest::header::COOKIE, header_val);
                    }
                }
            }
        }

        let mut request = self
            .client
            .request(request_method.clone(), url)
            .headers(request_headers);

        // 4. Request Body
        if method.sends_json_body() {
            let mut final_body = None;
            if has_mapping {
                if let Some(body) = body_val {
                    final_body = Some(body.clone());
                } else if let Some(mapping) = mapping {
                    let mut body_obj = serde_json::Map::new();
                    if let Some(args_obj) = arguments.as_object() {
                        for (key, val) in args_obj {
                            let mapped_loc = mapping.get(key).and_then(|l| l.as_str());
                            if !matches!(mapped_loc, Some("path" | "query" | "header" | "cookie")) {
                                body_obj.insert(key.clone(), val.clone());
                            }
                        }
                    }
                    if !body_obj.is_empty() {
                        final_body = Some(JsonValue::Object(body_obj));
                    }
                }
            } else {
                final_body = Some(arguments.clone());
            }

            if let Some(body) = final_body {
                request = request.json(&body);
            }
        }

        let response = request.send().await.map_err(|error| {
            let detail = error_chain(&error);
            tracing::warn!(
                target: "light_pingora::mcp",
                toolName = %tool.name,
                method = %request_method,
                url = %request_url,
                error = %detail,
                "mcp backend request failed"
            );
            McpExecutionError::execution_failed(detail)
        })?;
        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .await
            .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
        if !status.is_success() {
            return Err(McpExecutionError::execution_failed(format!(
                "tool `{}` returned HTTP {}: {}",
                tool.name,
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }
        if body.is_empty() {
            return Ok(mcp_text_result("success"));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
            && let Ok(value) = serde_json::from_slice::<JsonValue>(&body)
        {
            return Ok(mcp_json_result(value));
        }
        if let Ok(value) = serde_json::from_slice::<JsonValue>(&body) {
            return Ok(mcp_json_result(value));
        }
        Ok(mcp_text_result(String::from_utf8_lossy(&body).to_string()))
    }

    fn percent_encode_path_segment(input: &str) -> String {
        input
            .bytes()
            .flat_map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    vec![byte as char]
                }
                _ => format!("%{byte:02X}").chars().collect(),
            })
            .collect()
    }

    async fn execute_mcp_proxy_tool(
        &self,
        tool: &McpToolConfig,
        arguments: &JsonValue,
        agent_headers: &[(String, String)],
        frontend_session_id: &str,
    ) -> Result<JsonValue, McpExecutionError> {
        let method = effective_http_method(tool);
        if matches!(tool.method, McpHttpMethod::Call)
            && matches!(method, McpHttpMethod::Get | McpHttpMethod::Head)
        {
            return self
                .execute_http_tool_with_method(tool, arguments, agent_headers, method)
                .await;
        }

        let url = self.tool_target_url(tool).await?;
        let backend_session = self
            .ensure_backend_session(frontend_session_id, &url, agent_headers)
            .await?;
        // Use `endpoint_name` (the raw operationId registered on the backend MCP server) when
        // available, falling back to `name` for configs that predate this field.
        let backend_tool_name = tool
            .endpoint_name
            .as_deref()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or(tool.name.as_str());
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_backend_request_id(),
            "method": "tools/call",
            "params": {
                "name": backend_tool_name,
                "arguments": arguments
            }
        });
        let url_for_log = url.to_string();
        let response = self
            .client
            .post(url)
            .headers(backend_headers(
                agent_headers,
                backend_session.session_id.as_deref(),
                Some(backend_session.protocol_version.as_str()),
            )?)
            .json(&request)
            .send()
            .await
            .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
        let (status, content_type, _headers, body) =
            read_backend_mcp_response(response, "tools/call", &url_for_log).await?;
        if !status.is_success() {
            return Err(McpExecutionError::execution_failed(format!(
                "MCP tool `{}` returned HTTP {}: {}",
                tool.name,
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }
        let message = parse_mcp_backend_response(&body, content_type.as_deref())
            .map_err(McpExecutionError::execution_failed)?;
        if let Some(error) = message.get("error") {
            let message = error
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("MCP backend returned an error");
            return Err(McpExecutionError::execution_failed(message));
        }
        message.get("result").cloned().ok_or_else(|| {
            McpExecutionError::execution_failed("MCP backend response missing result")
        })
    }

    async fn ensure_backend_session(
        &self,
        frontend_session_id: &str,
        target_url: &Url,
        agent_headers: &[(String, String)],
    ) -> Result<McpBackendSession, McpExecutionError> {
        let target_key = target_url.as_str().to_string();
        let protocol_version = {
            let store = self.sessions.lock().await;
            let session = store
                .get(frontend_session_id)
                .ok_or_else(|| McpExecutionError::execution_failed("unknown MCP session id"))?;
            if let Some(backend_session) = session.backend_sessions.get(target_key.as_str()) {
                return Ok(backend_session.clone());
            }
            session.protocol_version.clone()
        };

        let initialized = self
            .initialize_backend_session(
                target_url.clone(),
                protocol_version.as_str(),
                agent_headers,
            )
            .await?;
        let insert_result = {
            let mut store = self.sessions.lock().await;
            if let Some(session) = store.get_mut(frontend_session_id) {
                if let Some(existing) = session.backend_sessions.get(target_key.as_str()) {
                    Ok(Some(existing.clone()))
                } else {
                    session
                        .backend_sessions
                        .insert(target_key, initialized.clone());
                    Ok(None)
                }
            } else {
                Err(McpExecutionError::execution_failed(
                    "unknown MCP session id",
                ))
            }
        };
        match insert_result {
            Ok(None) => Ok(initialized),
            Ok(Some(existing)) => {
                self.terminate_backend_session(initialized).await;
                Ok(existing)
            }
            Err(error) => {
                self.cleanup_initialized_backend_session(initialized, error)
                    .await
            }
        }
    }

    async fn cleanup_initialized_backend_session<T>(
        &self,
        backend_session: McpBackendSession,
        error: McpExecutionError,
    ) -> Result<T, McpExecutionError> {
        self.terminate_backend_session(backend_session).await;
        Err(error)
    }

    async fn initialize_backend_session(
        &self,
        target_url: Url,
        protocol_version: &str,
        agent_headers: &[(String, String)],
    ) -> Result<McpBackendSession, McpExecutionError> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": self.next_backend_request_id(),
            "method": "initialize",
            "params": {
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {
                    "name": "light-gateway-mcp",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        });
        let response = self
            .client
            .post(target_url.clone())
            .headers(backend_headers(
                agent_headers,
                None,
                Some(protocol_version),
            )?)
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                McpExecutionError::execution_failed(format!(
                    "backend MCP initialize failed: {error}"
                ))
            })?;
        let backend_session_id = response
            .headers()
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (status, content_type, _headers, body) =
            read_backend_mcp_response(response, "initialize", target_url.as_str()).await?;
        if !status.is_success() {
            return Err(McpExecutionError::execution_failed(format!(
                "backend MCP initialize returned HTTP {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }
        let message =
            parse_mcp_backend_response(&body, content_type.as_deref()).map_err(|error| {
                McpExecutionError::execution_failed(format!(
                    "invalid MCP backend initialize response: {error}"
                ))
            })?;
        if let Some(error) = message.get("error") {
            let message = error
                .get("message")
                .and_then(JsonValue::as_str)
                .unwrap_or("MCP backend initialize returned an error");
            return Err(McpExecutionError::execution_failed(message));
        }
        let result = message.get("result").ok_or_else(|| {
            McpExecutionError::execution_failed("MCP backend initialize response missing result")
        })?;
        let backend_protocol_version = result
            .get("protocolVersion")
            .and_then(JsonValue::as_str)
            .filter(|version| protocol_version_supported(version))
            .unwrap_or(protocol_version)
            .to_string();
        if result.is_null() {
            return Err(McpExecutionError::execution_failed(
                "MCP backend initialize response missing result",
            ));
        }
        let backend_session = McpBackendSession {
            target_url: target_url.to_string(),
            session_id: backend_session_id,
            protocol_version: backend_protocol_version,
            agent_headers: agent_headers.to_vec(),
        };
        if let Err(error) = self
            .send_backend_initialized(
                target_url.clone(),
                backend_session.protocol_version.as_str(),
                backend_session.session_id.as_deref(),
                agent_headers,
            )
            .await
        {
            self.terminate_backend_session(backend_session).await;
            return Err(error);
        }
        Ok(backend_session)
    }

    async fn send_backend_initialized(
        &self,
        target_url: Url,
        protocol_version: &str,
        backend_session_id: Option<&str>,
        agent_headers: &[(String, String)],
    ) -> Result<(), McpExecutionError> {
        let request = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let target_url_for_log = target_url.to_string();
        let response = self
            .client
            .post(target_url)
            .headers(backend_headers(
                agent_headers,
                backend_session_id,
                Some(protocol_version),
            )?)
            .json(&request)
            .send()
            .await
            .map_err(|error| {
                McpExecutionError::execution_failed(format!(
                    "backend MCP initialized notification failed: {error}"
                ))
            })?;
        let (status, _content_type, _headers, body) =
            read_backend_mcp_response(response, "notifications/initialized", &target_url_for_log)
                .await?;
        if !status.is_success() {
            return Err(McpExecutionError::execution_failed(format!(
                "backend MCP initialized notification returned HTTP {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }
        Ok(())
    }

    async fn terminate_backend_session(&self, backend_session: McpBackendSession) {
        let Some(session_id) = backend_session.session_id.as_deref() else {
            return;
        };
        let headers = match backend_headers(
            &backend_session.agent_headers,
            Some(session_id),
            Some(backend_session.protocol_version.as_str()),
        ) {
            Ok(headers) => headers,
            Err(error) => {
                tracing::warn!(
                    target: "light_pingora::mcp",
                    url = %backend_session.target_url,
                    error = %error.message,
                    "backend MCP session termination headers invalid"
                );
                return;
            }
        };
        if let Err(error) = self
            .client
            .delete(backend_session.target_url.as_str())
            .headers(headers)
            .send()
            .await
        {
            tracing::warn!(
                target: "light_pingora::mcp",
                url = %backend_session.target_url,
                error = %error,
                "backend MCP session termination failed"
            );
        }
    }

    fn terminate_backend_sessions_in_background(&self, backend_sessions: Vec<McpBackendSession>) {
        let runtime = self.clone();
        tokio::spawn(async move {
            for backend_session in backend_sessions {
                runtime.terminate_backend_session(backend_session).await;
            }
        });
    }

    async fn tool_target_url(&self, tool: &McpToolConfig) -> Result<Url, McpExecutionError> {
        let base = self.tool_base_url(tool).await?;
        Ok(apply_tool_path(base, tool))
    }

    async fn tool_base_url(&self, tool: &McpToolConfig) -> Result<Url, McpExecutionError> {
        if let Some(target_host) = tool
            .target_host
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            return parse_base_url(target_host, &tool.name);
        }

        let service_id = tool
            .service_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                McpExecutionError::execution_failed(format!(
                    "tool `{}` requires targetHost or serviceId",
                    tool.name
                ))
            })?;
        if let Some(matched) =
            direct_registry_match(&self.direct_registry, service_id, tool.env_tag.as_deref())
        {
            validate_direct_registry_protocol(matched, tool.protocol.as_deref())
                .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
            return parse_base_url(matched.url.trim(), &tool.name);
        }
        let discovery = self.discovery.as_ref().ok_or_else(|| {
            McpExecutionError::execution_failed(format!(
                "tool `{}` serviceId discovery requires portal registry to be enabled",
                tool.name
            ))
        })?;
        let snapshot = discovery
            .lookup_discovery(DiscoverySubscription {
                service_id: service_id.to_string(),
                env_tag: tool.env_tag.clone(),
                protocol: tool.protocol.clone(),
            })
            .await
            .map_err(|error| {
                McpExecutionError::execution_failed(format!(
                    "failed to discover MCP tool service `{service_id}`: {error}"
                ))
            })?;
        let node =
            select_discovery_node(&snapshot.nodes, tool.protocol.as_deref()).ok_or_else(|| {
                McpExecutionError::execution_failed(format!(
                    "MCP tool service `{service_id}` has no usable discovery nodes"
                ))
            })?;
        parse_base_url(discovery_node_base_url(node).as_str(), &tool.name)
    }

    fn get_tool(&self, requested_name: &str) -> Option<&McpToolConfig> {
        if let Some(tool) = self.tools.get(requested_name) {
            return Some(tool);
        }

        let requested_key = tool_name_alias_key(requested_name);
        if requested_key.is_empty() {
            return None;
        }

        let mut matched = None;
        for tool in self.tools.values() {
            if tool_name_alias_key(tool.name.as_str()) == requested_key {
                if matched.is_some() {
                    return None;
                }
                matched = Some(tool);
            }
        }
        matched
    }
}

fn tool_name_alias_key(name: &str) -> String {
    name.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn mcp_text_result(text: impl Into<String>) -> JsonValue {
    json!({
        "content": [
            {
                "type": "text",
                "text": text.into()
            }
        ]
    })
}

fn mcp_json_result(value: JsonValue) -> JsonValue {
    let structured_content = if value.is_array() {
        json!({ "items": value })
    } else {
        value
    };
    let text = serde_json::to_string(&structured_content)
        .unwrap_or_else(|_| structured_content.to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": structured_content
    })
}

#[derive(Debug, Clone)]
struct McpExecutionError {
    code: i64,
    message: String,
}

impl McpExecutionError {
    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn execution_failed(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
struct McpSessionError {
    status: u16,
    code: i64,
    message: String,
}

fn requested_protocol_version(message: &serde_json::Map<String, JsonValue>) -> Option<&str> {
    message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(JsonValue::as_str)
        .filter(|version| protocol_version_supported(version))
}

fn frontend_client_key(
    message: &serde_json::Map<String, JsonValue>,
    context: &McpRequestContext,
) -> String {
    if let Some(auth) = &context.auth {
        if let Some(client_id) = auth.client_id.as_deref().filter(|value| !value.is_empty()) {
            return format!(
                "auth:client:{}:{client_id}",
                auth.issuer.as_deref().unwrap_or_default()
            );
        }
        if let Some(user_id) = auth.user_id.as_deref().filter(|value| !value.is_empty()) {
            return format!(
                "auth:user:{}:{user_id}",
                auth.issuer.as_deref().unwrap_or_default()
            );
        }
        if let Some(email) = auth.email.as_deref().filter(|value| !value.is_empty()) {
            return format!(
                "auth:email:{}:{email}",
                auth.issuer.as_deref().unwrap_or_default()
            );
        }
        if let Some(host) = auth.host.as_deref().filter(|value| !value.is_empty()) {
            return format!("auth:host:{host}");
        }
    }

    let client_info = message
        .get("params")
        .and_then(|params| params.get("clientInfo"));
    let name = client_info
        .and_then(|client_info| client_info.get("name"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    let version = client_info
        .and_then(|client_info| client_info.get("version"))
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("unknown");
    format!("mcp-client:{name}:{version}")
}

pub fn load_mcp_router_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<McpRouterRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match load_mcp_router_config(runtime_config)? {
        Some(config) => config,
        None => McpRouterConfig::default(),
    };
    runtime_config.module_registry.register_loaded_config(
        MCP_ROUTER_MODULE_ID,
        MCP_ROUTER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;
    if !config.enabled {
        return Ok(None);
    }
    let policy = load_access_control_runtime(runtime_config, true)?.map(Arc::new);
    let client_config = match load_client_config(runtime_config) {
        Ok(client) => client,
        Err(RuntimeError::MissingConfig(file)) if file == CLIENT_FILE => ClientConfig::default(),
        Err(error) => return Err(error),
    };
    Ok(Some(
        McpRouterRuntime::new_with_discovery_policy_direct_registry_and_client_config(
            config,
            discovery_resolver(runtime_config.registry_client.clone()),
            policy,
            runtime_config.direct_registry.clone(),
            client_config,
        )?,
    ))
}

fn load_mcp_router_config(
    runtime_config: &RuntimeConfig,
) -> Result<Option<McpRouterConfig>, RuntimeError> {
    for file in [MCP_ROUTER_FILE, MCP_ROUTER_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<McpRouterConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(config)),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn validate_config(config: &McpRouterConfig) -> Result<(), RuntimeError> {
    if !config.path.starts_with('/') {
        return Err(RuntimeError::Unsupported(format!(
            "mcp-router.path `{}` must start with `/`",
            config.path
        )));
    }
    if config.max_sessions == 0 {
        return Err(RuntimeError::Unsupported(
            "mcp-router.maxSessions must be greater than 0".to_string(),
        ));
    }
    if config.max_sessions_per_client == 0 {
        return Err(RuntimeError::Unsupported(
            "mcp-router.maxSessionsPerClient must be greater than 0".to_string(),
        ));
    }
    let mut names = BTreeSet::new();
    for tool in &config.tools {
        let name = tool.name.trim();
        if name.is_empty() {
            return Err(RuntimeError::Unsupported(
                "mcp-router tool name must not be empty".to_string(),
            ));
        }
        if !names.insert(name.to_string()) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate mcp-router tool `{name}`"
            )));
        }
        if tool.path.trim().is_empty() || !tool.path.starts_with('/') {
            return Err(RuntimeError::Unsupported(format!(
                "mcp-router tool `{name}` path must start with `/`"
            )));
        }
        let has_target_host = tool
            .target_host
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        let has_service_id = tool
            .service_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
        if !has_target_host && !has_service_id {
            return Err(RuntimeError::Unsupported(format!(
                "mcp-router tool `{name}` requires targetHost or serviceId"
            )));
        }
    }
    Ok(())
}

fn tool_endpoint(tool: &McpToolConfig) -> String {
    tool.endpoint
        .as_deref()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "{}@{}",
                tool.path,
                tool.method.as_str().to_ascii_lowercase()
            )
        })
}

fn effective_http_method(tool: &McpToolConfig) -> McpHttpMethod {
    match tool.method {
        McpHttpMethod::Call if tool.input_schema_configured => McpHttpMethod::Post,
        McpHttpMethod::Call => McpHttpMethod::Get,
        method => method,
    }
}

fn log_mcp_tool_call(
    tool: &McpToolConfig,
    endpoint: &str,
    started: Instant,
    status: &str,
    policy_outcome: &str,
    context: &McpRequestContext,
) {
    tracing::info!(
        target: "light_pingora::mcp",
        toolName = %tool.name,
        endpoint = %endpoint,
        durationMs = started.elapsed().as_millis(),
        status = %status,
        policyOutcome = %policy_outcome,
        correlationId = ?context.correlation_id,
        "mcp tool call"
    );
}

fn is_mcp_error_result(result: &JsonValue) -> bool {
    result
        .get("isError")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

fn mcp_error_result_text(result: &JsonValue) -> String {
    result
        .get("content")
        .and_then(JsonValue::as_array)
        .and_then(|content| content.first())
        .and_then(|item| item.get("text"))
        .and_then(JsonValue::as_str)
        .unwrap_or("MCP error result")
        .to_string()
}

fn error_chain(error: &(dyn StdError + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}

fn mask_tool_arguments(tool: &McpToolConfig, arguments: &JsonValue) -> JsonValue {
    let mut masked = arguments.clone();
    apply_schema_mask(&tool.input_schema, &mut masked);
    masked
}

fn apply_schema_mask(schema: &JsonValue, value: &mut JsonValue) {
    let Some(schema) = schema.as_object() else {
        return;
    };
    if schema
        .get("x-mask")
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
    {
        mask_json_value(
            value,
            schema.get("x-mask-pattern").and_then(JsonValue::as_str),
        );
        return;
    }

    if let Some(properties) = schema.get("properties").and_then(JsonValue::as_object)
        && let Some(values) = value.as_object_mut()
    {
        for (name, property_schema) in properties {
            if let Some(property_value) = values.get_mut(name) {
                apply_schema_mask(property_schema, property_value);
            }
        }
    }

    if let Some(items_schema) = schema.get("items")
        && let Some(values) = value.as_array_mut()
    {
        for item in values {
            apply_schema_mask(items_schema, item);
        }
    }
}

fn mask_json_value(value: &mut JsonValue, pattern: Option<&str>) {
    match value {
        JsonValue::String(value) => {
            *value = mask_string(value, pattern);
        }
        JsonValue::Array(values) => {
            for value in values {
                mask_json_value(value, pattern);
            }
        }
        JsonValue::Object(values) => {
            for value in values.values_mut() {
                mask_json_value(value, pattern);
            }
        }
        JsonValue::Number(_) | JsonValue::Bool(_) | JsonValue::Null => {
            *value = JsonValue::String("******".to_string());
        }
    }
}

fn mask_string(value: &str, pattern: Option<&str>) -> String {
    if let Some(pattern) = pattern
        && let Ok(regex) = Regex::new(pattern)
        && regex.is_match(value)
    {
        return regex.replace_all(value, "******").to_string();
    }
    "******".to_string()
}

fn discovery_resolver(
    registry_client: Option<Arc<PortalRegistryClient>>,
) -> Option<Arc<dyn McpDiscoveryResolver>> {
    registry_client.map(|client| {
        let resolver: Arc<dyn McpDiscoveryResolver> = client;
        resolver
    })
}

fn parse_base_url(base: &str, tool_name: &str) -> Result<Url, McpExecutionError> {
    let url = Url::parse(base).map_err(|error| {
        McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` target `{base}` is invalid: {error}"
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` target `{base}` must use http or https"
        )));
    }
    Ok(url)
}

fn apply_tool_path(mut url: Url, tool: &McpToolConfig) -> Url {
    let combined = combine_base_and_tool_path(url.path(), tool.path.as_str());
    url.set_path(combined.as_str());
    url
}

fn combine_base_and_tool_path(base_path: &str, tool_path: &str) -> String {
    let base_path = normalize_base_path(base_path);
    let tool_path = normalize_tool_path(tool_path);

    if base_path.is_empty() {
        return tool_path;
    }
    if tool_path == "/" {
        return base_path;
    }
    if tool_path == base_path
        || matches!(
            tool_path.strip_prefix(&base_path),
            Some(suffix) if suffix.starts_with('/')
        )
    {
        return tool_path;
    }
    format!("{base_path}{tool_path}")
}

fn normalize_base_path(path: &str) -> String {
    let trimmed = path.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed == "/" {
        String::new()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn normalize_tool_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        "/".to_string()
    } else if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn select_discovery_node<'a>(
    nodes: &'a [DiscoveryNode],
    protocol: Option<&str>,
) -> Option<&'a DiscoveryNode> {
    let usable = |node: &&DiscoveryNode| {
        node.connected
            && node.port != 0
            && matches!(
                node.protocol.to_ascii_lowercase().as_str(),
                "http" | "https"
            )
            && protocol.is_none_or(|protocol| node.protocol.eq_ignore_ascii_case(protocol))
    };
    nodes
        .iter()
        .filter(usable)
        .find(|node| node.protocol.eq_ignore_ascii_case("https"))
        .or_else(|| {
            nodes
                .iter()
                .filter(usable)
                .find(|node| node.protocol.eq_ignore_ascii_case("http"))
        })
}

fn discovery_node_base_url(node: &DiscoveryNode) -> String {
    let host = if node.address.contains(':') && !node.address.starts_with('[') {
        format!("[{}]", node.address)
    } else {
        node.address.clone()
    };
    format!(
        "{}://{}:{}",
        node.protocol.to_ascii_lowercase(),
        host,
        node.port
    )
}

fn append_query_arguments(url: &mut Url, arguments: &JsonValue) {
    let Some(arguments) = arguments.as_object() else {
        return;
    };
    if arguments.is_empty() {
        return;
    }
    let mut query = url.query_pairs_mut();
    for (key, value) in arguments {
        if value.is_null() {
            continue;
        }
        query.append_pair(key, json_value_to_query(value).as_str());
    }
}

fn json_value_to_query(value: &JsonValue) -> String {
    match value {
        JsonValue::String(value) => value.clone(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::Bool(value) => value.to_string(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn stable_json_hash(value: &JsonValue) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

fn normalized_header_pairs(headers: &[(String, String)]) -> Vec<(String, String)> {
    let mut pairs = headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect::<Vec<_>>();
    pairs.sort();
    pairs
}

fn outbound_headers(headers: &[(String, String)]) -> Result<HeaderMap, McpExecutionError> {
    let mut outbound = HeaderMap::new();
    for (name, value) in headers {
        if should_regenerate_header(name) {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            McpExecutionError::execution_failed(format!("invalid inbound header `{name}`: {error}"))
        })?;
        let value = HeaderValue::from_str(value).map_err(|error| {
            McpExecutionError::execution_failed(format!(
                "invalid inbound header value for `{name}`: {error}"
            ))
        })?;
        outbound.append(name, value);
    }
    Ok(outbound)
}

fn backend_headers(
    headers: &[(String, String)],
    backend_session_id: Option<&str>,
    protocol_version: Option<&str>,
) -> Result<HeaderMap, McpExecutionError> {
    let mut outbound = outbound_headers(headers)?;
    if !outbound.contains_key(ACCEPT) {
        outbound.insert(ACCEPT, HeaderValue::from_static("application/json"));
    }
    let protocol_version = protocol_version.unwrap_or(DEFAULT_PROTOCOL_VERSION);
    let value = HeaderValue::from_str(protocol_version).map_err(|error| {
        McpExecutionError::execution_failed(format!(
            "invalid MCP protocol version header value: {error}"
        ))
    })?;
    outbound.insert(HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER), value);
    if let Some(session_id) = backend_session_id {
        let value = HeaderValue::from_str(session_id).map_err(|error| {
            McpExecutionError::execution_failed(format!(
                "invalid backend MCP session id header value: {error}"
            ))
        })?;
        outbound.insert(HeaderName::from_static(MCP_SESSION_ID_HEADER), value);
    }
    Ok(outbound)
}

fn should_regenerate_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "accept-encoding"
            | MCP_SESSION_ID_HEADER
            | MCP_PROTOCOL_VERSION_HEADER
    )
}

/// Extracts the MCP session id from an inbound request.
///
/// The `mcp-session-id` header is tried first (MCP Streamable HTTP spec).
/// If the header is absent or empty the function falls back to the `sessionId`
/// or `session_id` query parameter in `path`, which some clients append when
/// reconnecting after a server restart.
fn session_id_from_path_and_headers(path: &str, headers: &[(String, String)]) -> Option<String> {
    if let Some(id) = first_header(headers, MCP_SESSION_ID_HEADER)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return Some(id);
    }
    if let Some(query_str) = path.split('?').nth(1) {
        for (key, val) in form_urlencoded::parse(query_str.as_bytes()) {
            if key == "sessionId" || key == "session_id" {
                let id = val.trim().to_string();
                if !id.is_empty() {
                    return Some(id);
                }
            }
        }
    }
    None
}

fn preferred_response_mode(headers: &[(String, String)]) -> Option<McpResponseMode> {
    let accepts = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(ACCEPT.as_str()))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>();
    if accepts.is_empty() {
        return Some(McpResponseMode::Json);
    }
    for value in accepts {
        for item in value.split(',') {
            let media_type = item.split(';').next().unwrap_or_default().trim();
            match media_type {
                "application/json" | "application/*" | "*/*" => {
                    return Some(McpResponseMode::Json);
                }
                "text/event-stream" | "text/*" => return Some(McpResponseMode::EventStream),
                _ => {}
            }
        }
    }
    None
}

fn first_header(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.to_string())
}

fn protocol_version_supported(version: &str) -> bool {
    matches!(version, "2025-06-18" | "2025-03-26" | "2024-11-05")
}

fn accepted_response() -> McpHttpResponse {
    McpHttpResponse {
        status: 202,
        content_type: JSON_CONTENT_TYPE.to_string(),
        headers: protocol_headers(),
        body: Vec::new(),
        streamed: false,
    }
}

fn accepted_response_with_protocol_version(protocol_version: Option<&str>) -> McpHttpResponse {
    let response = accepted_response();
    match protocol_version {
        Some(protocol_version) => apply_protocol_version_header(response, protocol_version),
        None => response,
    }
}

fn method_not_allowed_response() -> McpHttpResponse {
    McpHttpResponse {
        status: 405,
        content_type: TEXT_CONTENT_TYPE.to_string(),
        headers: vec![("allow".to_string(), "POST, DELETE".to_string())],
        body: b"method not allowed".to_vec(),
        streamed: false,
    }
}

fn rpc_result_response(
    mode: McpResponseMode,
    status: u16,
    id: JsonValue,
    result: JsonValue,
) -> Result<McpHttpResponse, RuntimeError> {
    rpc_body_response(
        mode,
        status,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }),
    )
}

fn response_with_protocol_version(
    response: Result<McpHttpResponse, RuntimeError>,
    protocol_version: Option<&str>,
) -> Result<McpHttpResponse, RuntimeError> {
    let response = response?;
    Ok(match protocol_version {
        Some(protocol_version) => apply_protocol_version_header(response, protocol_version),
        None => response,
    })
}

fn apply_protocol_version_header(
    mut response: McpHttpResponse,
    protocol_version: &str,
) -> McpHttpResponse {
    set_response_header(
        &mut response.headers,
        MCP_PROTOCOL_VERSION_HEADER,
        protocol_version.to_string(),
    );
    response
}

fn initialize_response(
    mode: McpResponseMode,
    status: u16,
    id: JsonValue,
    result: JsonValue,
    session_id: String,
) -> Result<McpHttpResponse, RuntimeError> {
    let protocol_version = result
        .get("protocolVersion")
        .and_then(JsonValue::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION)
        .to_string();
    let mut response = rpc_result_response(mode, status, id, result)?;
    set_response_header(
        &mut response.headers,
        MCP_PROTOCOL_VERSION_HEADER,
        protocol_version,
    );
    response
        .headers
        .push((MCP_SESSION_ID_HEADER.to_string(), session_id));
    Ok(response)
}

fn set_response_header(headers: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some((_, existing)) = headers
        .iter_mut()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
    {
        *existing = value;
    } else {
        headers.push((name.to_string(), value));
    }
}

fn rpc_error_response(
    mode: McpResponseMode,
    status: u16,
    id: JsonValue,
    code: i64,
    message: impl Into<String>,
) -> Result<McpHttpResponse, RuntimeError> {
    rpc_body_response(
        mode,
        status,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message.into()
            }
        }),
    )
}

fn json_error_response(
    status: u16,
    id: JsonValue,
    code: i64,
    message: impl Into<String>,
) -> Result<McpHttpResponse, RuntimeError> {
    rpc_error_response(McpResponseMode::Json, status, id, code, message)
}

fn rpc_body_response(
    mode: McpResponseMode,
    status: u16,
    body: JsonValue,
) -> Result<McpHttpResponse, RuntimeError> {
    match mode {
        McpResponseMode::Json => json_body_response(status, body),
        McpResponseMode::EventStream => event_stream_response(status, body),
    }
}

fn json_body_response(status: u16, body: JsonValue) -> Result<McpHttpResponse, RuntimeError> {
    let body = serde_json::to_vec(&body)?;
    Ok(McpHttpResponse {
        status,
        content_type: JSON_CONTENT_TYPE.to_string(),
        headers: protocol_headers(),
        body,
        streamed: false,
    })
}

fn event_stream_response(status: u16, body: JsonValue) -> Result<McpHttpResponse, RuntimeError> {
    Ok(McpHttpResponse {
        status,
        content_type: EVENT_STREAM_CONTENT_TYPE.to_string(),
        headers: event_stream_headers(),
        body: sse_message_body(&body)?,
        streamed: true,
    })
}

fn protocol_headers() -> Vec<(String, String)> {
    vec![(
        "mcp-protocol-version".to_string(),
        DEFAULT_PROTOCOL_VERSION.to_string(),
    )]
}

fn event_stream_headers() -> Vec<(String, String)> {
    let mut headers = protocol_headers();
    headers.push(("cache-control".to_string(), "no-cache".to_string()));
    headers
}

async fn read_backend_mcp_response(
    response: reqwest::Response,
    operation: &str,
    url: &str,
) -> Result<(reqwest::StatusCode, Option<String>, HeaderMap, Vec<u8>), McpExecutionError> {
    let status = response.status();
    let headers = response.headers().clone();
    let content_type = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    tracing::debug!(
        target: "light_pingora::mcp",
        operation = %operation,
        url = %url,
        status = %status,
        headers = ?headers,
        "received backend MCP response headers"
    );

    match response.bytes().await {
        Ok(body) => {
            tracing::debug!(
                target: "light_pingora::mcp",
                operation = %operation,
                url = %url,
                status = %status,
                headers = ?headers,
                body_len = body.len(),
                body = %String::from_utf8_lossy(&body),
                "received backend MCP response body"
            );
            Ok((status, content_type, headers, body.to_vec()))
        }
        Err(error) => {
            tracing::debug!(
                target: "light_pingora::mcp",
                operation = %operation,
                url = %url,
                status = %status,
                headers = ?headers,
                error = %error,
                "failed to decode backend MCP response body"
            );
            Err(McpExecutionError::execution_failed(format!(
                "backend MCP {operation} response read failed: {error}"
            )))
        }
    }
}

fn parse_mcp_backend_response(
    body: &[u8],
    content_type: Option<&str>,
) -> Result<JsonValue, String> {
    match serde_json::from_slice::<JsonValue>(body) {
        Ok(value) => return Ok(value),
        Err(error) if !looks_like_event_stream(body, content_type) => {
            return Err(error.to_string());
        }
        Err(_) => {}
    }

    parse_sse_json_message(body)
        .ok_or_else(|| "backend returned text/event-stream without a JSON data message".to_string())
}

fn looks_like_event_stream(body: &[u8], content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        value
            .to_ascii_lowercase()
            .contains(EVENT_STREAM_CONTENT_TYPE)
    }) || body.starts_with(b"data:")
        || body.starts_with(b"id:")
        || body.starts_with(b"retry:")
        || body.starts_with(b"event:")
}

fn parse_sse_json_message(body: &[u8]) -> Option<JsonValue> {
    let text = std::str::from_utf8(body).ok()?;
    let mut data_lines = Vec::new();

    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            if let Some(value) = parse_sse_data_lines(&data_lines) {
                return Some(value);
            }
            data_lines.clear();
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("data:") {
            data_lines.push(value.strip_prefix(' ').unwrap_or(value));
        }
    }

    parse_sse_data_lines(&data_lines)
}

fn parse_sse_data_lines(data_lines: &[&str]) -> Option<JsonValue> {
    if data_lines.is_empty() {
        return None;
    }
    let data = data_lines.join("\n");
    serde_json::from_str::<JsonValue>(&data).ok()
}

fn sse_message_body(body: &JsonValue) -> Result<Vec<u8>, RuntimeError> {
    let serialized = serde_json::to_string(body)?;
    let mut event = String::from("event: message\n");
    for line in serialized.lines() {
        event.push_str("data: ");
        event.push_str(line);
        event.push('\n');
    }
    event.push('\n');
    Ok(event.into_bytes())
}

fn deserialize_json_value<'de, D>(deserializer: D) -> Result<JsonValue, D::Error>
where
    D: Deserializer<'de>,
{
    let value = YamlValue::deserialize(deserializer)?;
    yaml_value_to_json(value).map_err(D::Error::custom)
}

fn deserialize_optional_json_value<'de, D>(deserializer: D) -> Result<Option<JsonValue>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = YamlValue::deserialize(deserializer)?;
    yaml_value_to_json(value)
        .map(Some)
        .map_err(D::Error::custom)
}

fn yaml_value_to_json(value: YamlValue) -> Result<JsonValue, String> {
    match value {
        YamlValue::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(default_object());
            }
            if value.starts_with('{') || value.starts_with('[') {
                return serde_json::from_str::<JsonValue>(value)
                    .or_else(|_| serde_yaml::from_str::<JsonValue>(value))
                    .map_err(|error| error.to_string());
            }
            Ok(JsonValue::String(value.to_string()))
        }
        other => serde_json::to_value(other).map_err(|error| error.to_string()),
    }
}

fn default_enabled() -> bool {
    true
}

fn default_mcp_path() -> String {
    DEFAULT_MCP_PATH.to_string()
}

fn default_max_frontend_sessions() -> usize {
    DEFAULT_MCP_MAX_FRONTEND_SESSIONS
}

fn default_max_frontend_sessions_per_client() -> usize {
    DEFAULT_MCP_MAX_FRONTEND_SESSIONS_PER_CLIENT
}

fn default_input_schema() -> JsonValue {
    json!({ "type": "object" })
}

fn default_object() -> JsonValue {
    json!({})
}

impl fmt::Display for McpHttpMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Display for McpToolType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    #[test]
    fn config_accepts_tools_array_and_json_string() {
        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
enabled: true
path: /mcp
maxSessions: 123
maxSessionsPerClient: 7
tools: '[{"name":"weather","description":"Weather","targetHost":"http://127.0.0.1:8080","path":"/weather","method":"GET","inputSchema":{"type":"object"}}]'
"#,
        )
        .expect("parse config");

        assert_eq!(config.max_sessions, 123);
        assert_eq!(config.max_sessions_per_client, 7);
        assert_eq!(config.tools.len(), 1);
        assert_eq!(config.tools[0].name, "weather");
        assert_eq!(config.tools[0].input_schema["type"], "object");
        assert!(config.tools[0].input_schema_configured);

        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
tools:
  - name: pet
    targetHost: http://127.0.0.1:8080
    path: /pet
    method: post
    inputSchema:
      type: object
"#,
        )
        .expect("parse config");

        assert_eq!(config.tools[0].method, McpHttpMethod::Post);
        assert_eq!(config.tools[0].api_type, McpToolType::Http);

        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
tools:
  - name: echo
    targetHost: http://127.0.0.1:8080
    path: /mcp
    method: call
    apiType: mcp
  - name: pet
    targetHost: http://127.0.0.1:8081
    path: /pet
    method: get
    apiType: openapi
"#,
        )
        .expect("parse config");

        assert_eq!(config.tools[0].method, McpHttpMethod::Call);
        assert_eq!(config.tools[0].api_type, McpToolType::Mcp);
        assert!(!config.tools[0].input_schema_configured);
        assert_eq!(config.tools[1].api_type, McpToolType::Http);
    }

    #[test]
    fn runtime_rejects_duplicate_tool_names() {
        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
tools:
  - name: pet
    targetHost: http://127.0.0.1:8080
    path: /pet
  - name: pet
    targetHost: http://127.0.0.1:8081
    path: /pet
"#,
        )
        .expect("parse config");

        let error = McpRouterRuntime::new(config).expect_err("duplicate tool name");
        assert!(error.to_string().contains("duplicate mcp-router tool"));
    }

    #[test]
    fn runtime_uses_client_tls_config_for_backend_client() {
        let client_config = ClientConfig {
            tls: light_client::ClientTlsConfig {
                ca_cert_path: Some(std::path::PathBuf::from("/missing/ca.pem")),
                ..light_client::ClientTlsConfig::default()
            },
            ..ClientConfig::default()
        };

        let error = McpRouterRuntime::new_with_discovery_policy_direct_registry_and_client_config(
            McpRouterConfig::default(),
            None,
            None,
            DirectRegistryConfig::default(),
            client_config,
        )
        .expect_err("invalid CA path should fail client build");

        assert!(error.to_string().contains("invalid MCP HTTP client"));
        assert!(error.to_string().contains("/missing/ca.pem"));
    }

    #[tokio::test]
    async fn initialize_returns_streamable_http_protocol() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(body["result"]["serverInfo"]["name"], "light-gateway-mcp");
        let session_id =
            first_header(&response.headers, MCP_SESSION_ID_HEADER).expect("session id header");
        uuid::Uuid::parse_str(session_id.as_str()).expect("session id uuid");
        assert!(session_id.bytes().all(|byte| (0x21..=0x7e).contains(&byte)));
    }

    #[tokio::test]
    async fn initialize_protocol_header_matches_negotiated_version() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(
            first_header(&response.headers, MCP_PROTOCOL_VERSION_HEADER).as_deref(),
            Some("2025-03-26")
        );
    }

    #[tokio::test]
    async fn initialize_rejects_new_session_when_store_is_full() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        {
            let mut sessions = runtime.sessions.lock().await;
            for index in 0..runtime.config.max_sessions {
                sessions.insert(
                    format!("session-{index}"),
                    McpGatewaySession::new(
                        DEFAULT_PROTOCOL_VERSION.to_string(),
                        format!("test-client-{index}"),
                    ),
                );
            }
        }

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 503);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(body["error"]["message"], "MCP session store is full");
        assert!(
            first_header(&response.headers, MCP_SESSION_ID_HEADER).is_none(),
            "failed initialize must not issue a session id"
        );
    }

    #[tokio::test]
    async fn initialize_rejects_new_session_when_client_limit_is_reached() {
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            max_sessions: 10,
            max_sessions_per_client: 2,
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let client_key = "mcp-client:curl-test:1.0".to_string();
        {
            let mut sessions = runtime.sessions.lock().await;
            for index in 0..runtime.config.max_sessions_per_client {
                sessions.insert(
                    format!("session-{index}"),
                    McpGatewaySession::new(
                        DEFAULT_PROTOCOL_VERSION.to_string(),
                        client_key.clone(),
                    ),
                );
            }
        }

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"curl-test","version":"1.0"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 429);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(
            body["error"]["message"],
            "MCP client session limit exceeded"
        );
        assert!(
            first_header(&response.headers, MCP_SESSION_ID_HEADER).is_none(),
            "failed initialize must not issue a session id"
        );
    }

    #[tokio::test]
    async fn notifications_return_accepted_without_body() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let session = session_header_with_protocol(&runtime, "2025-03-26");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), session],
                body: br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 202);
        assert!(response.body.is_empty());
        assert_eq!(
            first_header(&response.headers, MCP_PROTOCOL_VERSION_HEADER).as_deref(),
            Some("2025-03-26")
        );
    }

    #[tokio::test]
    async fn tools_list_supports_query_filter() {
        let runtime = runtime_with_tool("weather", "Get weather", "http://127.0.0.1:8080");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list","params":{"query":"weath"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["tools"][0]["name"], "weather");
    }

    #[tokio::test]
    async fn tools_list_filters_tools_by_access_control_permission() {
        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                tools_list_access_control: crate::access_control::ToolsListAccessControlConfig {
                    mode: crate::access_control::ToolsListAccessControlMode::Permission,
                    ..crate::access_control::ToolsListAccessControlConfig::default()
                },
                ..crate::access_control::AccessControlConfig::default()
            }),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow role
    ruleType: req-acc
    expression: "'role' in auditInfo.subject_claims.ClaimsMap && 'roles' in permission && permission.roles in auditInfo.subject_claims.ClaimsMap.role"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  allow-group:
    common: Y
    ruleId: allow-group
    ruleName: Allow group
    ruleType: req-acc
    expression: "'scp' in auditInfo.subject_claims.ClaimsMap && 'groups' in permission && permission.groups in auditInfo.subject_claims.ClaimsMap.scp"
endpointRules:
  echo@call:
    req-acc:
      - allow-role
    permission:
      roles: account-manager
  getRandomNumber@call:
    req-acc:
      - allow-role
    permission:
      roles: category-admin
  /offers@get:
    req-acc:
      - allow-group
    permission:
      groups: portal.w
"#,
            )
            .expect("rule config"),
        ));
        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![
                    test_tool(
                        "local_mcp_echo",
                        "Echo",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Call,
                        Some("echo@call"),
                        default_input_schema(),
                    ),
                    test_tool(
                        "local_mcp_get_random_number",
                        "Random number",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Call,
                        Some("getRandomNumber@call"),
                        default_input_schema(),
                    ),
                    test_tool(
                        "demo_offer_decision_api_search_offers",
                        "Search offers",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Get,
                        Some("/offers@get"),
                        default_input_schema(),
                    ),
                ],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("account-manager".to_string()),
                        claims: json!({
                            "role": "account-manager",
                            "scp": ["portal.w"]
                        }),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let names = body["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec!["demo_offer_decision_api_search_offers", "local_mcp_echo"]
        );
    }

    #[tokio::test]
    async fn tools_list_cel_mode_evaluates_request_access_rules() {
        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                tools_list_access_control: crate::access_control::ToolsListAccessControlConfig {
                    mode: crate::access_control::ToolsListAccessControlMode::Cel,
                    ..crate::access_control::ToolsListAccessControlConfig::default()
                },
                ..crate::access_control::AccessControlConfig::default()
            }),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow
    ruleType: req-acc
    expression: "'role' in auditInfo.subject_claims.ClaimsMap && auditInfo.subject_claims.ClaimsMap.role == 'account-manager'"
    conditionLanguage: cel
    conditionSecurityProfile: strict
  deny:
    common: Y
    ruleId: deny
    ruleName: Deny
    ruleType: req-acc
    expression: "false"
    conditionLanguage: cel
    conditionSecurityProfile: strict
endpointRules:
  echo@call:
    req-acc:
      - allow
  getRandomNumber@call:
    req-acc:
      - deny
"#,
            )
            .expect("rule config"),
        ));
        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![
                    test_tool(
                        "local_mcp_echo",
                        "Echo",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Call,
                        Some("echo@call"),
                        default_input_schema(),
                    ),
                    test_tool(
                        "local_mcp_get_random_number",
                        "Random number",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Call,
                        Some("getRandomNumber@call"),
                        default_input_schema(),
                    ),
                ],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("account-manager".to_string()),
                        claims: json!({"role": "account-manager"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let names = body["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["local_mcp_echo"]);
    }

    #[tokio::test]
    async fn tools_list_cel_mode_hides_tools_after_evaluation_limit() {
        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                tools_list_access_control: crate::access_control::ToolsListAccessControlConfig {
                    mode: crate::access_control::ToolsListAccessControlMode::Cel,
                    max_cel_evaluations: 1,
                    ..crate::access_control::ToolsListAccessControlConfig::default()
                },
                default_deny: false,
                ..crate::access_control::AccessControlConfig::default()
            }),
            crate::access_control::RuleFileConfig::default(),
        ));
        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![
                    test_tool(
                        "alpha",
                        "Alpha",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Get,
                        Some("alpha@call"),
                        default_input_schema(),
                    ),
                    test_tool(
                        "beta",
                        "Beta",
                        "http://127.0.0.1:8080",
                        McpHttpMethod::Get,
                        Some("beta@call"),
                        default_input_schema(),
                    ),
                ],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let names = body["result"]["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .map(|tool| tool["name"].as_str().expect("name"))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["alpha"]);
    }

    #[test]
    fn tools_list_visibility_cache_is_bounded_lru() {
        let mut cache = ToolsListVisibilityCache::new(2);
        cache.insert("a".to_string(), vec!["alpha".to_string()]);
        cache.insert("b".to_string(), vec!["beta".to_string()]);
        assert_eq!(cache.get("a"), Some(vec!["alpha".to_string()]));

        cache.insert("c".to_string(), vec!["gamma".to_string()]);

        assert_eq!(cache.get("b"), None);
        assert_eq!(cache.get("a"), Some(vec!["alpha".to_string()]));
        assert_eq!(cache.get("c"), Some(vec!["gamma".to_string()]));
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn session_bound_responses_use_session_protocol_header() {
        let (base, _received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let runtime = runtime_with_tool("weather", "Get weather", base.as_str());
        let session = session_header_with_protocol(&runtime, "2025-03-26");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), session.clone()],
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(
            first_header(&response.headers, MCP_PROTOCOL_VERSION_HEADER).as_deref(),
            Some("2025-03-26")
        );

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), session],
                body: br#"{"jsonrpc":"2.0","id":"2","method":"tools/call","params":{"name":"weather","arguments":{"city":"Toronto"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(
            first_header(&response.headers, MCP_PROTOCOL_VERSION_HEADER).as_deref(),
            Some("2025-03-26")
        );
    }

    #[tokio::test]
    async fn tools_list_requires_frontend_session() {
        let runtime = runtime_with_tool("weather", "Get weather", "http://127.0.0.1:8080");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32600);
        assert_eq!(body["error"]["message"], "missing MCP session id");
    }

    #[tokio::test]
    async fn tools_list_rejects_unknown_frontend_session() {
        let runtime = runtime_with_tool("weather", "Get weather", "http://127.0.0.1:8080");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    (
                        MCP_SESSION_ID_HEADER.to_string(),
                        "missing-session".to_string(),
                    ),
                ],
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(body["error"]["message"], "unknown MCP session id");
    }

    #[tokio::test]
    async fn post_rejects_unacceptable_accept_header() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![("accept".to_string(), "text/html".to_string())],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 406);
        assert!(!response.streamed);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn get_stream_returns_405_until_enabled() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "GET".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_sse()],
                body: Vec::new(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 405);
        assert!(!response.streamed);
        assert_eq!(
            response.headers,
            vec![("allow".to_string(), "POST, DELETE".to_string())]
        );
    }

    #[tokio::test]
    async fn tool_call_get_forwards_arguments_and_agent_headers() {
        let pets = json!([{"id": 1, "name": "catten", "tag": "cat"}]);
        let (base, received) = spawn_http_server(http_json_response(pets.clone())).await;
        let runtime = runtime_with_tool("weather", "Get weather", base.as_str());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    session_header(&runtime),
                    (
                        MCP_PROTOCOL_VERSION_HEADER.to_string(),
                        "2025-03-26".to_string(),
                    ),
                    ("authorization".to_string(), "Bearer abc".to_string()),
                ],
                body: br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"weather","arguments":{"city":"New York","unit":"c"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], pets);
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /weather?city=New+York&unit=c HTTP/1.1"));
        assert!(request.contains("authorization: Bearer abc"));
        assert!(!request.contains("mcp-protocol-version"));
    }

    #[tokio::test]
    async fn tool_call_resolves_non_exact_tool_name_alias() {
        let pets = json!([{"id": 1, "name": "catten", "tag": "cat"}]);
        let (base, received) = spawn_http_server(http_json_response(pets.clone())).await;
        let runtime = runtime_with_tool("listPets", "List pets", base.as_str());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"list_pets","arguments":{"limit":1}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], pets);
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /weather?limit=1 HTTP/1.1"));
    }

    #[tokio::test]
    async fn tool_call_rejects_ambiguous_tool_name_alias() {
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![
                test_tool(
                    "listPets",
                    "List pets",
                    "http://127.0.0.1:1",
                    McpHttpMethod::Get,
                    None,
                    default_input_schema(),
                ),
                test_tool(
                    "list_pets",
                    "List pets",
                    "http://127.0.0.1:1",
                    McpHttpMethod::Get,
                    None,
                    default_input_schema(),
                ),
            ],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":14,"method":"tools/call","params":{"name":"list-pets","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32601);
        assert_eq!(body["error"]["message"], "tool `list-pets` not found");
    }

    #[tokio::test]
    async fn post_can_return_streamed_event_response() {
        let (base, _received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let runtime = runtime_with_tool("weather", "Get weather", base.as_str());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_sse_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"weather","arguments":{"city":"New York"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "text/event-stream");
        assert!(response.streamed);
        let event = sse_json(&response.body);
        assert_eq!(event["id"], 10);
        assert_mcp_json_result(&event["result"], json!({"ok": true}));
    }

    #[tokio::test]
    async fn tool_call_uses_discovered_service_target() {
        let (base, received) =
            spawn_http_server(http_json_response(json!({"forecast": "rain"}))).await;
        let resolver = Arc::new(FakeDiscovery::new(discovery_snapshot(
            base.as_str(),
            "com.networknt.weather-1.0.0",
            Some("dev"),
            Some("http"),
        )));
        let discovery: Arc<dyn McpDiscoveryResolver> = resolver.clone();
        let runtime = McpRouterRuntime::new_with_discovery(
            McpRouterConfig {
                tools: vec![McpToolConfig {
                    name: "weather".to_string(),
                    endpoint_name: None,
                    description: "Get weather".to_string(),
                    protocol: Some("http".to_string()),
                    service_id: Some("com.networknt.weather-1.0.0".to_string()),
                    env_tag: Some("dev".to_string()),
                    target_host: None,
                    path: "/weather".to_string(),
                    method: McpHttpMethod::Get,
                    endpoint: None,
                    api_type: McpToolType::Http,
                    input_schema: default_input_schema(),
                    input_schema_configured: true,
                    tool_metadata: default_object(),
                }],
                ..McpRouterConfig::default()
            },
            Some(discovery),
        )
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Toronto"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], json!({"forecast": "rain"}));
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /weather?city=Toronto HTTP/1.1"));
        let lookups = resolver.lookups.lock().expect("lookup lock");
        assert_eq!(lookups[0].service_id, "com.networknt.weather-1.0.0");
        assert_eq!(lookups[0].env_tag.as_deref(), Some("dev"));
        assert_eq!(lookups[0].protocol.as_deref(), Some("http"));
    }

    #[tokio::test]
    async fn tool_call_uses_direct_registry_before_discovery() {
        let (base, received) =
            spawn_http_server(http_json_response(json!({"forecast": "clear"}))).await;
        let resolver = Arc::new(FakeDiscovery::new(discovery_snapshot(
            "http://127.0.0.1:9",
            "com.networknt.weather-1.0.0",
            Some("dev"),
            Some("http"),
        )));
        let discovery: Arc<dyn McpDiscoveryResolver> = resolver.clone();
        let runtime = McpRouterRuntime::new_with_discovery_policy_and_direct_registry(
            McpRouterConfig {
                tools: vec![McpToolConfig {
                    name: "weather".to_string(),
                    endpoint_name: None,
                    description: "Get weather".to_string(),
                    protocol: Some("http".to_string()),
                    service_id: Some("com.networknt.weather-1.0.0".to_string()),
                    env_tag: Some("dev".to_string()),
                    target_host: None,
                    path: "/weather".to_string(),
                    method: McpHttpMethod::Get,
                    endpoint: None,
                    api_type: McpToolType::Http,
                    input_schema: default_input_schema(),
                    input_schema_configured: true,
                    tool_metadata: default_object(),
                }],
                ..McpRouterConfig::default()
            },
            Some(discovery),
            None,
            DirectRegistryConfig {
                direct_urls: BTreeMap::from([(
                    "com.networknt.weather-1.0.0|dev".to_string(),
                    base.clone(),
                )]),
            },
        )
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Toronto"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], json!({"forecast": "clear"}));
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /weather?city=Toronto HTTP/1.1"));
        assert!(resolver.lookups.lock().expect("lookup lock").is_empty());
    }

    #[tokio::test]
    async fn direct_registry_base_path_is_not_duplicated_when_tool_path_already_contains_it() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let service_id = "api-ba-en-it-mcp-eadpservices-1.0.0";
        let registry_base =
            format!("{base}/dev-apiplatformapplications-genai-ns/api-ba-en-it-mcp-eadpservices");
        let runtime = McpRouterRuntime::new_with_discovery_policy_and_direct_registry(
            McpRouterConfig {
                tools: vec![McpToolConfig {
                    name: "get_sh_vers".to_string(),
                    endpoint_name: None,
                    description: "Search API versions".to_string(),
                    protocol: None,
                    service_id: Some(service_id.to_string()),
                    env_tag: None,
                    target_host: None,
                    path: "/dev-apiplatformapplications-genai-ns/api-ba-en-it-mcp-eadpservices/services/mcp".to_string(),
                    method: McpHttpMethod::Call,
                    endpoint: Some("get_sh_vers@call".to_string()),
                    api_type: McpToolType::Http,
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "apiName": {
                                "type": "string"
                            }
                        },
                        "required": ["apiName"]
                    }),
                    input_schema_configured: true,
                    tool_metadata: default_object(),
                }],
                ..McpRouterConfig::default()
            },
            None,
            None,
            DirectRegistryConfig {
                direct_urls: BTreeMap::from([(service_id.to_string(), registry_base)]),
            },
        )
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"get_sh_vers","arguments":{"apiName":"petstore"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], json!({"ok": true}));
        let request = received.await.expect("server request");
        assert!(request.starts_with(
            "POST /dev-apiplatformapplications-genai-ns/api-ba-en-it-mcp-eadpservices/services/mcp HTTP/1.1"
        ));
        assert!(!request.starts_with(
            "POST /dev-apiplatformapplications-genai-ns/api-ba-en-it-mcp-eadpservices/dev-apiplatformapplications-genai-ns"
        ));
    }

    #[tokio::test]
    async fn mcp_proxy_tool_posts_backend_tools_call_and_forwards_headers() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "cloudy"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let gateway_session = session_header(&runtime);
        let gateway_session_id = gateway_session.1.clone();

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    ("authorization".to_string(), "Bearer abc".to_string()),
                    gateway_session,
                ],
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "cloudy");
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
        for request in &requests {
            assert!(request.contains("accept: application/json, text/event-stream\r\n"));
        }

        assert!(requests[0].starts_with("POST /mcp HTTP/1.1"));
        let backend_initialize = request_json_body(&requests[0]);
        assert_eq!(backend_initialize["method"], "initialize");
        assert!(backend_initialize["id"].is_number());
        assert!(
            !requests[0]
                .contains(format!("{MCP_SESSION_ID_HEADER}: {gateway_session_id}").as_str())
        );

        assert!(requests[1].starts_with("POST /mcp HTTP/1.1"));
        assert!(requests[1].contains("mcp-session-id: backend-session"));
        assert!(requests[1].contains("mcp-protocol-version: 2025-06-18"));
        let backend_initialized = request_json_body(&requests[1]);
        assert_eq!(backend_initialized["method"], "notifications/initialized");

        assert!(requests[2].starts_with("POST /mcp HTTP/1.1"));
        assert!(requests[2].contains("authorization: Bearer abc"));
        assert!(requests[2].contains("mcp-session-id: backend-session"));
        assert!(requests[2].contains("mcp-protocol-version: 2025-06-18"));
        assert!(
            !requests[2]
                .contains(format!("{MCP_SESSION_ID_HEADER}: {gateway_session_id}").as_str())
        );
        let backend_call = request_json_body(&requests[2]);
        assert_eq!(backend_call["method"], "tools/call");
        assert!(backend_call["id"].is_number());
        assert_eq!(backend_call["params"]["name"], "weather");
        assert_eq!(backend_call["params"]["arguments"]["city"], "Ottawa");
    }

    #[tokio::test]
    async fn mcp_proxy_tool_accepts_backend_sse_json_rpc_responses() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "cloudy"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_sse_response("backend-session"),
            http_empty_response(202),
            http_sse_json_response(backend_result),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "cloudy");
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test]
    async fn mcp_proxy_tool_accepts_backend_sse_json_rpc_response_with_id_first() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "cloudy"
                    }
                ]
            }
        });
        let body = format!(
            "retry:1000\nid:my-session-uuid\nevent:message\ndata: {}\n\n",
            serde_json::to_string(&backend_result).expect("serialize backend result")
        );
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_sse_response("backend-session"),
            http_empty_response(202),
            format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body
            ),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "cloudy");
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
    }

    #[tokio::test]
    async fn mcp_proxy_tool_accepts_backend_string_response_ids() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend-string-id",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "cloudy"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["id"], "1");
        assert_eq!(body["result"]["content"][0]["text"], "cloudy");
        let requests = received.await.expect("server requests");
        let backend_initialize = request_json_body(&requests[0]);
        let backend_call = request_json_body(&requests[2]);
        assert!(backend_initialize["id"].is_number());
        assert!(backend_call["id"].is_number());
    }

    /// Regression test: when `endpoint_name` differs from `name`, the gateway-facing `name`
    /// (e.g. `local_mcp_echo`) is used for tool lookup, but the backend `tools/call` must
    /// carry `endpoint_name` (e.g. `echo`) so the real MCP server can find the tool.
    #[tokio::test]
    async fn mcp_proxy_tool_uses_endpoint_name_for_backend_tools_call() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [{"type": "text", "text": "hello"}]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result),
        ])
        .await;
        // Gateway-facing name is the concatenated label; endpointName is the real operationId.
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "local_mcp_echo".to_string(),
                endpoint_name: Some("echo".to_string()),
                description: "Echoes back the input".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: json!({
                    "type": "object",
                    "properties": {"message": {"type": "string"}},
                    "required": ["message"]
                }),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        // The agent calls the tool by its gateway-facing name.
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"local_mcp_echo","arguments":{"message":"hi"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "hello");

        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        let backend_call = request_json_body(&requests[2]);
        assert_eq!(backend_call["method"], "tools/call");
        // Must use endpointName ("echo"), NOT the gateway-facing name ("local_mcp_echo").
        assert_eq!(backend_call["params"]["name"], "echo");
        assert_eq!(backend_call["params"]["arguments"]["message"], "hi");
    }

    #[tokio::test]
    async fn mcp_proxy_call_with_input_schema_posts_backend_tools_call() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "cloudy"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Call,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "city": {
                            "type": "string"
                        }
                    }
                }),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
        let backend_call = request_json_body(&requests[2]);
        assert_eq!(backend_call["method"], "tools/call");
        assert_eq!(backend_call["params"]["name"], "weather");
        assert_eq!(backend_call["params"]["arguments"]["city"], "Ottawa");
    }

    #[tokio::test]
    async fn mcp_proxy_reuses_backend_session_for_frontend_session_and_target() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "ok"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result.clone()),
            http_json_response(backend_result),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Call,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "city": {
                            "type": "string"
                        }
                    }
                }),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let gateway_session = session_header(&runtime);

        for city in ["Ottawa", "Toronto"] {
            let response = runtime
                .handle_request(McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![accept_json(), gateway_session.clone()],
                    body: format!(
                        r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"weather","arguments":{{"city":"{city}"}}}}}}"#
                    )
                    .into_bytes(),
                })
                .await
                .expect("handle")
                .expect("response");
            assert_eq!(response.status, 200);
        }

        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        assert_eq!(request_json_body(&requests[2])["method"], "tools/call");
        assert_eq!(request_json_body(&requests[3])["method"], "tools/call");
        assert!(requests[2].contains("mcp-session-id: backend-session"));
        assert!(requests[3].contains("mcp-session-id: backend-session"));
    }

    #[tokio::test]
    async fn mcp_proxy_terminates_backend_session_when_frontend_session_disappears() {
        let (base, initialize_seen, release_initialize, received) =
            spawn_http_sequence_server_with_first_response_gate(vec![
                backend_initialize_response("backend-session"),
                http_empty_response(202),
                http_empty_response(202),
            ])
            .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let gateway_session = session_header(&runtime);
        let frontend_session_id = gateway_session.1.clone();
        let target_url = Url::parse(format!("{base}/mcp").as_str()).expect("url");
        let runtime_for_task = runtime.clone();
        let frontend_session_id_for_task = frontend_session_id.clone();

        let task = tokio::spawn(async move {
            runtime_for_task
                .ensure_backend_session(
                    frontend_session_id_for_task.as_str(),
                    &target_url,
                    &[accept_json()],
                )
                .await
        });
        initialize_seen.await.expect("initialize observed");
        runtime
            .sessions
            .lock()
            .await
            .remove(frontend_session_id.as_str());
        release_initialize.send(()).expect("release initialize");

        let error = task
            .await
            .expect("task")
            .expect_err("missing frontend session should fail");
        assert_eq!(error.message, "unknown MCP session id");
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        assert!(requests[2].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[2].contains("mcp-session-id: backend-session"));
        assert!(requests[2].contains("mcp-protocol-version: 2025-06-18"));
    }

    #[tokio::test]
    async fn mcp_proxy_terminates_backend_session_when_initialized_notification_fails() {
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_json_response_with_status(401, json!({"error": "unauthorized"})),
            http_empty_response(202),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("backend MCP initialized notification returned HTTP 401")
        );
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 3);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        assert!(requests[2].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[2].contains("mcp-session-id: backend-session"));
        assert!(requests[2].contains("mcp-protocol-version: 2025-06-18"));
    }

    #[tokio::test]
    async fn delete_frontend_session_terminates_backend_session() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "backend",
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": "ok"
                    }
                ]
            }
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response(backend_result),
            http_empty_response(202),
        ])
        .await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
                endpoint_name: None,
                description: "Get weather".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Post,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: true,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let gateway_session = session_header(&runtime);

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    ("authorization".to_string(), "Bearer abc".to_string()),
                    gateway_session.clone(),
                ],
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "DELETE".to_string(),
                path: "/mcp".to_string(),
                headers: vec![gateway_session.clone()],
                body: Vec::new(),
            })
            .await
            .expect("delete")
            .expect("response");
        assert_eq!(response.status, 202);

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), gateway_session],
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);

        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        assert_eq!(request_json_body(&requests[2])["method"], "tools/call");
        assert!(requests[3].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[3].contains("authorization: Bearer abc"));
        assert!(requests[3].contains("mcp-session-id: backend-session"));
        assert!(requests[3].contains("mcp-protocol-version: 2025-06-18"));
    }

    #[tokio::test]
    async fn expired_frontend_session_terminates_backend_sessions() {
        let (base, received) = spawn_http_sequence_server(vec![http_empty_response(202)]).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let frontend_session_id = uuid::Uuid::new_v4().to_string();
        let backend_target_url = format!("{base}/mcp");
        let mut session = McpGatewaySession::new(
            DEFAULT_PROTOCOL_VERSION.to_string(),
            "test-client".to_string(),
        );
        session.last_accessed = Instant::now()
            .checked_sub(MCP_SESSION_IDLE_TIMEOUT + Duration::from_secs(1))
            .expect("expired instant");
        session.backend_sessions.insert(
            backend_target_url.clone(),
            McpBackendSession {
                target_url: backend_target_url,
                session_id: Some("backend-session".to_string()),
                protocol_version: DEFAULT_PROTOCOL_VERSION.to_string(),
                agent_headers: Vec::new(),
            },
        );
        runtime
            .sessions
            .lock()
            .await
            .insert(frontend_session_id.clone(), session);

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        assert!(
            !runtime
                .sessions
                .lock()
                .await
                .contains_key(frontend_session_id.as_str())
        );
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[0].contains("mcp-session-id: backend-session"));
        assert!(requests[0].contains("mcp-protocol-version: 2025-06-18"));
    }

    #[tokio::test]
    async fn initialize_forces_purge_when_session_store_is_full() {
        let (base, received) = spawn_http_sequence_server(vec![http_empty_response(202)]).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            max_sessions: 1,
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let frontend_session_id = uuid::Uuid::new_v4().to_string();
        let backend_target_url = format!("{base}/mcp");
        let mut session = McpGatewaySession::new(
            DEFAULT_PROTOCOL_VERSION.to_string(),
            "old-client".to_string(),
        );
        session.last_accessed = Instant::now()
            .checked_sub(MCP_SESSION_IDLE_TIMEOUT + Duration::from_secs(1))
            .expect("expired instant");
        session.backend_sessions.insert(
            backend_target_url.clone(),
            McpBackendSession {
                target_url: backend_target_url,
                session_id: Some("backend-session".to_string()),
                protocol_version: DEFAULT_PROTOCOL_VERSION.to_string(),
                agent_headers: Vec::new(),
            },
        );
        runtime
            .sessions
            .lock()
            .await
            .insert(frontend_session_id.clone(), session);
        *runtime.last_session_purge.lock().await = Some(Instant::now());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"new-client","version":"1.0"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        assert!(
            !runtime
                .sessions
                .lock()
                .await
                .contains_key(frontend_session_id.as_str())
        );
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[0].contains("mcp-session-id: backend-session"));
    }

    #[tokio::test]
    async fn initialize_forces_purge_when_client_session_limit_is_reached() {
        let (base, received) = spawn_http_sequence_server(vec![http_empty_response(202)]).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            max_sessions: 10,
            max_sessions_per_client: 1,
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let frontend_session_id = uuid::Uuid::new_v4().to_string();
        let backend_target_url = format!("{base}/mcp");
        let mut session = McpGatewaySession::new(
            DEFAULT_PROTOCOL_VERSION.to_string(),
            "mcp-client:curl-test:1.0".to_string(),
        );
        session.last_accessed = Instant::now()
            .checked_sub(MCP_SESSION_IDLE_TIMEOUT + Duration::from_secs(1))
            .expect("expired instant");
        session.backend_sessions.insert(
            backend_target_url.clone(),
            McpBackendSession {
                target_url: backend_target_url,
                session_id: Some("backend-session".to_string()),
                protocol_version: DEFAULT_PROTOCOL_VERSION.to_string(),
                agent_headers: Vec::new(),
            },
        );
        runtime
            .sessions
            .lock()
            .await
            .insert(frontend_session_id.clone(), session);
        *runtime.last_session_purge.lock().await = Some(Instant::now());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"curl-test","version":"1.0"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        assert!(
            !runtime
                .sessions
                .lock()
                .await
                .contains_key(frontend_session_id.as_str())
        );
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("DELETE /mcp HTTP/1.1"));
        assert!(requests[0].contains("mcp-session-id: backend-session"));
    }

    #[tokio::test]
    async fn mcp_proxy_call_without_input_schema_uses_http_get() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "serverInfo".to_string(),
                endpoint_name: None,
                description: "Get server info".to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(base),
                path: "/mcp".to_string(),
                method: McpHttpMethod::Call,
                endpoint: None,
                api_type: McpToolType::Mcp,
                input_schema: default_input_schema(),
                input_schema_configured: false,
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"serverInfo","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], json!({"ok": true}));
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /mcp HTTP/1.1"));
    }

    #[tokio::test]
    async fn access_control_default_deny_blocks_unconfigured_tool() {
        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "weather",
                    "Get weather",
                    "http://127.0.0.1:1",
                    McpHttpMethod::Get,
                    Some("weather@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(Arc::new(crate::access_control::AccessControlRuntime::new(
                Some(crate::access_control::AccessControlConfig::default()),
                crate::access_control::RuleFileConfig::default(),
            ))),
        )
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32001);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("no access control rule")
        );
    }

    #[tokio::test]
    async fn access_control_allows_role_and_masks_schema_marked_arguments() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig::default()),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow MCP reader
    ruleType: req-acc
    expression: "'role' in auditInfo.subject_claims.ClaimsMap"
    conditionLanguage: cel
    conditionSecurityProfile: strict
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
endpointRules:
  weather@call:
    req-acc:
      - allow
    permission:
      roles: mcp-reader
"#,
            )
            .expect("rule config"),
        ));
        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "weather",
                    "Get weather",
                    base.as_str(),
                    McpHttpMethod::Post,
                    Some("weather@call"),
                    json!({
                        "type": "object",
                        "properties": {
                            "ssn": {
                                "type": "string",
                                "x-mask": true,
                                "x-mask-pattern": "^(.*)$"
                            },
                            "city": {
                                "type": "string"
                            }
                        }
                    }),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"weather","arguments":{"ssn":"123-45-6789","city":"Toronto"}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("mcp-reader".to_string()),
                        claims: json!({"role": "mcp-reader"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_mcp_json_result(&body["result"], json!({"ok": true}));
        let request = received.await.expect("server request");
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let arguments = serde_json::from_str::<JsonValue>(body).expect("json request");
        assert_eq!(arguments["ssn"], "******");
        assert_eq!(arguments["city"], "Toronto");
        assert!(!body.contains("123-45-6789"));
    }

    #[tokio::test]
    async fn malformed_json_returns_parse_error() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: b"{".to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32700);
    }

    #[test]
    fn apply_tool_path_appends_tool_path_to_base_path() {
        let mut tool = test_tool(
            "weather",
            "Get weather",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.path = "/weather".to_string();

        let url = apply_tool_path(
            Url::parse("https://example.com/gateway/service").expect("url"),
            &tool,
        );

        assert_eq!(url.as_str(), "https://example.com/gateway/service/weather");
    }

    #[test]
    fn apply_tool_path_preserves_tool_path_that_already_contains_base_path() {
        let mut tool = test_tool(
            "get_sh_vers",
            "Search API versions",
            "https://example.com",
            McpHttpMethod::Post,
            None,
            default_input_schema(),
        );
        tool.path = "/gateway/service/services/mcp".to_string();

        let url = apply_tool_path(
            Url::parse("https://example.com/gateway/service").expect("url"),
            &tool,
        );

        assert_eq!(
            url.as_str(),
            "https://example.com/gateway/service/services/mcp"
        );
    }

    #[test]
    fn apply_tool_path_matches_base_prefix_on_path_segment_boundary() {
        let mut tool = test_tool(
            "weather",
            "Get weather",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.path = "/api2/weather".to_string();

        let url = apply_tool_path(Url::parse("https://example.com/api").expect("url"), &tool);

        assert_eq!(url.as_str(), "https://example.com/api/api2/weather");
    }

    #[test]
    fn apply_tool_path_handles_trailing_slash_in_base_path() {
        let mut tool = test_tool(
            "weather",
            "Get weather",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );

        tool.path = "/weather".to_string();
        let url = apply_tool_path(Url::parse("https://example.com").expect("url"), &tool);
        assert_eq!(url.as_str(), "https://example.com/weather");

        tool.path = "/services/mcp".to_string();
        let url = apply_tool_path(
            Url::parse("https://example.com/gateway/service/").expect("url"),
            &tool,
        );
        assert_eq!(
            url.as_str(),
            "https://example.com/gateway/service/services/mcp"
        );
    }

    fn runtime_with_tool(name: &str, description: &str, target_host: &str) -> McpRouterRuntime {
        McpRouterRuntime::new(McpRouterConfig {
            tools: vec![test_tool(
                name,
                description,
                target_host,
                McpHttpMethod::Get,
                None,
                default_input_schema(),
            )],
            ..McpRouterConfig::default()
        })
        .expect("runtime")
    }

    fn test_tool(
        name: &str,
        description: &str,
        target_host: &str,
        method: McpHttpMethod,
        endpoint: Option<&str>,
        input_schema: JsonValue,
    ) -> McpToolConfig {
        McpToolConfig {
            name: name.to_string(),
            endpoint_name: None,
            description: description.to_string(),
            protocol: None,
            service_id: None,
            env_tag: None,
            target_host: Some(target_host.to_string()),
            path: "/weather".to_string(),
            method,
            endpoint: endpoint.map(str::to_string),
            api_type: McpToolType::Http,
            input_schema,
            input_schema_configured: true,
            tool_metadata: default_object(),
        }
    }

    fn accept_json() -> (String, String) {
        (
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        )
    }

    fn accept_sse() -> (String, String) {
        (
            "accept".to_string(),
            "text/event-stream, application/json".to_string(),
        )
    }

    fn session_header(runtime: &McpRouterRuntime) -> (String, String) {
        session_header_with_protocol(runtime, DEFAULT_PROTOCOL_VERSION)
    }

    fn session_header_with_protocol(
        runtime: &McpRouterRuntime,
        protocol_version: &str,
    ) -> (String, String) {
        let session_id = uuid::Uuid::new_v4().to_string();
        runtime.sessions.try_lock().expect("session lock").insert(
            session_id.clone(),
            McpGatewaySession::new(protocol_version.to_string(), "test-client".to_string()),
        );
        (MCP_SESSION_ID_HEADER.to_string(), session_id)
    }

    fn accept_json_with_session(runtime: &McpRouterRuntime) -> Vec<(String, String)> {
        vec![accept_json(), session_header(runtime)]
    }

    fn accept_sse_with_session(runtime: &McpRouterRuntime) -> Vec<(String, String)> {
        vec![accept_sse(), session_header(runtime)]
    }

    fn sse_json(body: &[u8]) -> JsonValue {
        let event = String::from_utf8(body.to_vec()).expect("sse utf8");
        assert!(event.starts_with("event: message\n"));
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .collect::<Vec<_>>()
            .join("\n");
        serde_json::from_str(&data).expect("sse json")
    }

    fn assert_mcp_json_result(result: &JsonValue, expected: JsonValue) {
        let expected_structured = if expected.is_array() {
            json!({ "items": expected })
        } else {
            expected
        };
        assert_eq!(result["structuredContent"], expected_structured);
        assert_eq!(result["content"][0]["type"], "text");
        let text = result["content"][0]["text"].as_str().expect("text content");
        let parsed = serde_json::from_str::<JsonValue>(text).expect("json text content");
        assert_eq!(parsed, expected_structured);
    }

    fn http_json_response(value: JsonValue) -> String {
        http_json_response_with_headers(value, &[])
    }

    fn http_json_response_with_status(status: u16, value: JsonValue) -> String {
        let body = serde_json::to_string(&value).expect("serialize response");
        let reason = match status {
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            500 => "Internal Server Error",
            _ => "OK",
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn http_json_response_with_headers(value: JsonValue, headers: &[(&str, &str)]) -> String {
        let body = serde_json::to_string(&value).expect("serialize response");
        let extra_headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nconnection: close\r\n{extra_headers}content-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn http_sse_json_response(value: JsonValue) -> String {
        http_sse_json_response_with_headers(value, &[])
    }

    fn http_sse_json_response_with_headers(value: JsonValue, headers: &[(&str, &str)]) -> String {
        let data = sse_message_body(&value).expect("serialize sse response");
        let body = String::from_utf8(data).expect("sse utf8");
        let extra_headers = headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect::<String>();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n{extra_headers}content-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn http_empty_response(status: u16) -> String {
        let reason = match status {
            202 => "Accepted",
            204 => "No Content",
            _ => "OK",
        };
        format!("HTTP/1.1 {status} {reason}\r\nconnection: close\r\ncontent-length: 0\r\n\r\n")
    }

    fn backend_initialize_response(session_id: &str) -> String {
        http_json_response_with_headers(
            json!({
                "jsonrpc": "2.0",
                "id": "backend-init",
                "result": {
                    "protocolVersion": DEFAULT_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {
                            "listChanged": true
                        }
                    },
                    "serverInfo": {
                        "name": "backend-mcp",
                        "version": "1.0"
                    }
                }
            }),
            &[(MCP_SESSION_ID_HEADER, session_id)],
        )
    }

    fn backend_initialize_sse_response(session_id: &str) -> String {
        http_sse_json_response_with_headers(
            json!({
                "jsonrpc": "2.0",
                "id": "backend-init",
                "result": {
                    "protocolVersion": DEFAULT_PROTOCOL_VERSION,
                    "capabilities": {
                        "tools": {
                            "listChanged": true
                        }
                    },
                    "serverInfo": {
                        "name": "backend-mcp",
                        "version": "1.0"
                    }
                }
            }),
            &[(MCP_SESSION_ID_HEADER, session_id)],
        )
    }

    fn request_json_body(request: &str) -> JsonValue {
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        serde_json::from_str::<JsonValue>(body).expect("backend json")
    }

    fn discovery_snapshot(
        base: &str,
        service_id: &str,
        env_tag: Option<&str>,
        protocol: Option<&str>,
    ) -> DiscoverySnapshot {
        let url = Url::parse(base).expect("base url");
        serde_json::from_value(json!({
            "serviceId": service_id,
            "envTag": env_tag,
            "protocol": protocol,
            "nodes": [{
                "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40",
                "serviceId": service_id,
                "envTag": env_tag,
                "environment": "dev",
                "version": "1.0.0",
                "protocol": protocol.unwrap_or(url.scheme()),
                "address": url.host_str().expect("host"),
                "port": url.port_or_known_default().expect("port"),
                "tags": {},
                "connectedAt": "2026-01-01T00:00:00Z",
                "lastSeenAt": "2026-01-01T00:00:01Z",
                "connected": true
            }]
        }))
        .expect("discovery snapshot")
    }

    struct FakeDiscovery {
        snapshot: DiscoverySnapshot,
        lookups: Mutex<Vec<DiscoverySubscription>>,
    }

    impl FakeDiscovery {
        fn new(snapshot: DiscoverySnapshot) -> Self {
            Self {
                snapshot,
                lookups: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl McpDiscoveryResolver for FakeDiscovery {
        async fn lookup_discovery(
            &self,
            subscription: DiscoverySubscription,
        ) -> Result<DiscoverySnapshot, String> {
            self.lookups.lock().expect("lookup lock").push(subscription);
            Ok(self.snapshot.clone())
        }
    }

    async fn spawn_http_server(response: String) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let mut buffer = vec![0_u8; 1024];
            let mut request_bytes = Vec::new();
            loop {
                let read = stream.read(&mut buffer).await.expect("read request");
                if read == 0 {
                    break;
                }
                request_bytes.extend_from_slice(&buffer[..read]);
                if request_complete(&request_bytes) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&request_bytes).to_string();
            let _ = tx.send(request);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), rx)
    }

    async fn spawn_http_sequence_server(
        responses: Vec<String>,
    ) -> (String, oneshot::Receiver<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().await.expect("accept connection");
                let mut buffer = vec![0_u8; 1024];
                let mut request_bytes = Vec::new();
                loop {
                    let read = stream.read(&mut buffer).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    request_bytes.extend_from_slice(&buffer[..read]);
                    if request_complete(&request_bytes) {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request_bytes).to_string());
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            let _ = tx.send(requests);
        });
        (format!("http://{address}"), rx)
    }

    async fn spawn_http_sequence_server_with_first_response_gate(
        responses: Vec<String>,
    ) -> (
        String,
        oneshot::Receiver<()>,
        oneshot::Sender<()>,
        oneshot::Receiver<Vec<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local addr");
        let (requests_tx, requests_rx) = oneshot::channel();
        let (first_seen_tx, first_seen_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut first_seen_tx = Some(first_seen_tx);
            let mut release_rx = Some(release_rx);
            for response in responses {
                let (mut stream, _) = listener.accept().await.expect("accept connection");
                let mut buffer = vec![0_u8; 1024];
                let mut request_bytes = Vec::new();
                loop {
                    let read = stream.read(&mut buffer).await.expect("read request");
                    if read == 0 {
                        break;
                    }
                    request_bytes.extend_from_slice(&buffer[..read]);
                    if request_complete(&request_bytes) {
                        break;
                    }
                }
                requests.push(String::from_utf8_lossy(&request_bytes).to_string());
                if let Some(first_seen_tx) = first_seen_tx.take() {
                    let _ = first_seen_tx.send(());
                    if let Some(release_rx) = release_rx.take() {
                        let _ = release_rx.await;
                    }
                }
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            let _ = requests_tx.send(requests);
        });
        (
            format!("http://{address}"),
            first_seen_rx,
            release_tx,
            requests_rx,
        )
    }

    fn request_complete(request: &[u8]) -> bool {
        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let body_len = request.len().saturating_sub(header_end + 4);
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        });
        content_length.is_none_or(|content_length| body_len >= content_length)
    }

    #[tokio::test]
    async fn tool_call_performs_openapi_parameter_mapping() {
        let (base, received) =
            spawn_http_server(http_json_response(json!({"success": true}))).await;

        let mut tool = test_tool(
            "updateUser",
            "Update user",
            base.as_str(),
            McpHttpMethod::Put,
            None,
            default_input_schema(),
        );
        tool.path = "/users/{userId}".to_string();
        tool.tool_metadata = json!({
            "routing": {
                "parameters": {
                    "userId": "path",
                    "X-Trace-Id": "header",
                    "body": "body",
                    "session_cookie": "cookie"
                }
            }
        });

        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![tool],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "updateUser",
                        "arguments": {
                            "userId": "usr/999",
                            "X-Trace-Id": "trace-uuid-123",
                            "session_cookie": "session_val_xyz",
                            "body": {
                                "name": "Avery"
                            }
                        }
                    }
                }))
                .unwrap(),
            })
            .await
            .expect("handle")
            .expect("response");

        println!("RESPONSE STATUS: {}", response.status);
        println!("RESPONSE BODY: {}", String::from_utf8_lossy(&response.body));
        assert_eq!(response.status, 200);
        let request = received.await.expect("server request");
        println!("RECEIVED REQUEST:\n{}", request);
        assert!(request.starts_with("PUT /users/usr%2F999 HTTP/1.1"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("x-trace-id: trace-uuid-123")
        );
        assert!(request.contains("cookie: session_cookie=session_val_xyz"));
        assert!(request.contains("{\"name\":\"Avery\"}"));
    }

    #[tokio::test]
    async fn tool_call_performs_openapi_parameter_mapping_get() {
        let (base, received) =
            spawn_http_server(http_json_response(json!({"success": true}))).await;

        let mut tool = test_tool(
            "searchOffers",
            "Search offers",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.path = "/offers".to_string();
        tool.tool_metadata = json!({
            "routing": {
                "parameters": {
                    "segment": "query",
                    "state": "query"
                }
            }
        });

        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![tool],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": "searchOffers",
                        "arguments": {
                            "segment": "premium",
                            "state": "ON"
                        }
                    }
                }))
                .unwrap(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let request = received.await.expect("server request");

        assert!(request.starts_with("GET /offers?"));
        assert!(request.contains("segment=premium"));
        assert!(request.contains("state=ON"));
    }

    #[tokio::test]
    async fn preserve_state_from_carries_sessions_across_reload() {
        let original = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        // Seed a session in the original runtime (simulates a connected client).
        let session_id = {
            let mut store = original.sessions.lock().await;
            let id = uuid::Uuid::new_v4().to_string();
            store.insert(
                id.clone(),
                McpGatewaySession::new(
                    DEFAULT_PROTOCOL_VERSION.to_string(),
                    "test-client".to_string(),
                ),
            );
            id
        };

        // Simulate a config reload: build a fresh runtime then preserve state.
        let mut reloaded = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        reloaded.preserve_state_from(&original);

        // The previously established session must still be visible.
        assert!(
            reloaded
                .sessions
                .lock()
                .await
                .contains_key(session_id.as_str()),
            "session must survive config reload"
        );

        // And tools/list with that session must succeed.
        let response = reloaded
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    (MCP_SESSION_ID_HEADER.to_string(), session_id.clone()),
                ],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn tools_list_accepts_session_id_in_query_parameter() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let (_, session_id) = session_header(&runtime);

        // Pass the session id as a query parameter instead of a header.
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: format!("/mcp?sessionId={session_id}"),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert!(body["result"]["tools"].is_array());
    }

    #[tokio::test]
    async fn delete_accepts_session_id_in_query_parameter() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let (_, session_id) = session_header(&runtime);

        // Terminate the session using query parameter only (no header).
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "DELETE".to_string(),
                path: format!("/mcp?sessionId={session_id}"),
                headers: vec![],
                body: vec![],
            })
            .await
            .expect("handle")
            .expect("response");

        // Expect 202 Accepted (session terminated).
        assert_eq!(response.status, 202);
        // Session must now be gone.
        assert!(
            !runtime
                .sessions
                .lock()
                .await
                .contains_key(session_id.as_str()),
            "session must be removed after DELETE"
        );
    }

    #[tokio::test]
    async fn mcp_response_row_filter_default_include_false_returns_empty() {
        let (base, _received) = spawn_http_server(http_json_response(json!([
            {"accountType": "C"},
            {"accountType": "S"}
        ])))
        .await;

        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig::default()),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow teller
    ruleType: req-acc
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  row_filter:
    common: Y
    ruleId: row_filter
    ruleName: Filter rows
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  accounts@call:
    req-acc:
      - allow
    res-fil:
      - row_filter
    permission:
      roles: teller, manager
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: "C"
"#,
            )
            .expect("rule config"),
        ));

        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "accounts",
                    "List accounts",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("accounts@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"accounts","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("manager".to_string()),
                        claims: json!({"role": "manager"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let result = &body["result"];
        assert_eq!(result["structuredContent"], json!({"items": []}));
        assert_eq!(result["content"][0]["text"], r#"{"items":[]}"#);
    }

    #[tokio::test]
    async fn mcp_response_filter_is_skipped_when_access_control_disabled() {
        let (base, _received) = spawn_http_server(http_json_response(json!([
            {"accountType": "C"},
            {"accountType": "S"}
        ])))
        .await;

        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                enabled: false,
                ..crate::access_control::AccessControlConfig::default()
            }),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow teller
    ruleType: req-acc
    expression: "false"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  row_filter:
    common: Y
    ruleId: row_filter
    ruleName: Filter rows
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  accounts@call:
    req-acc:
      - allow
    res-fil:
      - row_filter
    permission:
      roles: teller, manager
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: "C"
"#,
            )
            .expect("rule config"),
        ));

        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "accounts",
                    "List accounts",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("accounts@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"accounts","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("manager".to_string()),
                        claims: json!({"role": "manager"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let result = &body["result"];
        assert_eq!(
            result["structuredContent"],
            json!({"items": [{"accountType": "C"}, {"accountType": "S"}]})
        );
        assert_eq!(
            result["content"][0]["text"],
            r#"{"items":[{"accountType":"C"},{"accountType":"S"}]}"#
        );
    }

    #[tokio::test]
    async fn mcp_access_control_is_skipped_by_tool_name_prefix() {
        let (base, _received) = spawn_http_server(http_json_response(json!([
            {"accountType": "C"},
            {"accountType": "S"}
        ])))
        .await;

        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                skip_path_prefixes: vec!["local_mcp".to_string()],
                ..crate::access_control::AccessControlConfig::default()
            }),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  deny:
    common: Y
    ruleId: deny
    ruleName: Deny teller
    ruleType: req-acc
    expression: "false"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  row_filter:
    common: Y
    ruleId: row_filter
    ruleName: Filter rows
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  accounts@call:
    req-acc:
      - deny
    res-fil:
      - row_filter
    permission:
      roles: teller, manager
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: "C"
"#,
            )
            .expect("rule config"),
        ));

        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "local_mcp_echo",
                    "Echo through local MCP",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("accounts@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"local_mcp_echo","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("manager".to_string()),
                        claims: json!({"role": "manager"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let result = &body["result"];
        assert_eq!(
            result["structuredContent"],
            json!({"items": [{"accountType": "C"}, {"accountType": "S"}]})
        );
        assert_eq!(
            result["content"][0]["text"],
            r#"{"items":[{"accountType":"C"},{"accountType":"S"}]}"#
        );
    }

    #[tokio::test]
    async fn mcp_response_row_filter_default_include_true_includes_all() {
        let (base, _received) = spawn_http_server(http_json_response(json!([
            {"accountType": "C"},
            {"accountType": "S"}
        ])))
        .await;

        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                default_include: true,
                ..crate::access_control::AccessControlConfig::default()
            }),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow teller
    ruleType: req-acc
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  row_filter:
    common: Y
    ruleId: row_filter
    ruleName: Filter rows
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  accounts@call:
    req-acc:
      - allow
    res-fil:
      - row_filter
    permission:
      roles: teller, manager
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: "C"
"#,
            )
            .expect("rule config"),
        ));

        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "accounts",
                    "List accounts",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("accounts@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"accounts","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("manager".to_string()),
                        claims: json!({"role": "manager"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let result = &body["result"];
        assert_eq!(
            result["structuredContent"],
            json!({
                "items": [
                    {"accountType": "C"},
                    {"accountType": "S"}
                ]
            })
        );
    }

    #[tokio::test]
    async fn mcp_response_row_filter_matching_claim_filters_rows() {
        let (base, _received) = spawn_http_server(http_json_response(json!([
            {"accountType": "C"},
            {"accountType": "S"}
        ])))
        .await;

        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig::default()),
            serde_yaml::from_str::<crate::access_control::RuleFileConfig>(
                r#"
ruleBodies:
  allow:
    common: Y
    ruleId: allow
    ruleName: Allow teller
    ruleType: req-acc
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  row_filter:
    common: Y
    ruleId: row_filter
    ruleName: Filter rows
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  accounts@call:
    req-acc:
      - allow
    res-fil:
      - row_filter
    permission:
      roles: teller
      row:
        role:
          teller:
            - colName: accountType
              operator: "="
              colValue: "C"
"#,
            )
            .expect("rule config"),
        ));

        let runtime = McpRouterRuntime::new_with_policy(
            McpRouterConfig {
                tools: vec![test_tool(
                    "accounts",
                    "List accounts",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("accounts@call"),
                    default_input_schema(),
                )],
                ..McpRouterConfig::default()
            },
            Some(policy),
        )
        .expect("runtime");

        let response = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: accept_json_with_session(&runtime),
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"accounts","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("teller".to_string()),
                        claims: json!({"role": "teller"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                },
            )
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        let result = &body["result"];
        assert_eq!(
            result["structuredContent"],
            json!({
                "items": [
                    {"accountType": "C"}
                ]
            })
        );
        assert_eq!(
            result["content"][0]["text"],
            r#"{"items":[{"accountType":"C"}]}"#
        );
    }
}
