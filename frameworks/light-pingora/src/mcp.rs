use crate::access_control::{
    AccessControlRuntime, AccessDecision, ToolVisibility, ToolsListAccessControlConfig,
    ToolsListAccessControlMode, load_access_control_runtime,
};
use crate::config_util::deserialize_typed_list;
use crate::direct_registry::{direct_registry_match, validate_direct_registry_protocol};
use crate::mcp_protocol::{
    Classification, ClassificationRejection, ClassifierConfig, FrontendProfile, RequestHead,
    STATELESS_PROTOCOL_META_KEY, STATELESS_RC_VERSION, classify_post, classify_request_head,
};
use crate::mcp_resources::StatelessResourceBudgets;
use crate::mcp_schema::{
    HeaderValueKind, McpSchemaConfig, PreparedMcpTool, SchemaDiagnostic, SchemaValidationPool,
    ValidationOutcome, prepare_tools,
};
use crate::mcp_stateless::{
    ExpectedParameterHeader, ExpectedParameterValue, SERVER_INFO_META_KEY, StatelessRequestError,
    validate_parameter_headers, validate_stateless_request,
};
use crate::security::AuthPrincipal;
use crate::token::{CLIENT_FILE, load_client_config};
use agent_delegation::{DelegationClaims, DelegationKind};
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
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::error::Error as StdError;
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;
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
const DEFAULT_MCP_MAX_REQUEST_BODY_BYTES: usize = 1_048_576;
const DEFAULT_MCP_MAX_RESPONSE_BODY_BYTES: usize = 4_194_304;
const DEFAULT_MCP_MAX_JSON_DEPTH: usize = 128;
const MAX_LEGACY_CLIENT_METADATA_BYTES: usize = 16_384;
const MAX_LEGACY_CLIENT_METADATA_DEPTH: usize = 16;
const LEGACY_SESSION_BINDING_CONTRACT: u16 = 1;
const JSON_CONTENT_TYPE: &str = "application/json";
const EVENT_STREAM_CONTENT_TYPE: &str = "text/event-stream";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";
const MAX_TOOL_RETRY_ATTEMPTS: usize = 5;
const MAX_TOOL_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const RETRYABLE_STATUS_CODES: &[u16] = &[408, 425, 429, 500, 502, 503, 504];

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
    #[serde(default = "default_max_request_body_bytes")]
    pub max_request_body_bytes: usize,
    #[serde(default = "default_max_response_body_bytes")]
    pub max_response_body_bytes: usize,
    #[serde(default = "default_max_json_depth")]
    pub max_json_depth: usize,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub origin_allowlist: Vec<String>,
    #[serde(default)]
    pub protocols: McpProtocolsConfig,
    #[serde(default)]
    pub schema: McpSchemaConfig,
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
            max_request_body_bytes: default_max_request_body_bytes(),
            max_response_body_bytes: default_max_response_body_bytes(),
            max_json_depth: default_max_json_depth(),
            origin_allowlist: Vec::new(),
            protocols: McpProtocolsConfig::default(),
            schema: McpSchemaConfig::default(),
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpProtocolsConfig {
    #[serde(default)]
    pub legacy: McpLegacyProtocolConfig,
    #[serde(default)]
    pub stateless: McpStatelessProtocolConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpLegacyProtocolConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(
        default = "default_legacy_protocol_versions",
        deserialize_with = "deserialize_typed_list"
    )]
    pub versions: Vec<String>,
}

impl Default for McpLegacyProtocolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            versions: default_legacy_protocol_versions(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatelessProtocolConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(
        default = "default_stateless_protocol_versions",
        deserialize_with = "deserialize_typed_list"
    )]
    pub versions: Vec<String>,
    #[serde(default = "default_stateless_discover_ttl_ms")]
    pub discover_ttl_ms: u64,
    #[serde(default = "default_stateless_tools_list_ttl_ms")]
    pub tools_list_ttl_ms: u64,
    #[serde(default = "default_stateless_discover_cache_entries")]
    pub max_discover_cache_entries: usize,
    #[serde(default = "default_stateless_tools_list_cache_entries")]
    pub max_tools_list_cache_entries: usize,
    #[serde(default = "default_stateless_tools_list_items")]
    pub max_tools_list_items: usize,
    #[serde(default = "default_stateless_concurrent_requests")]
    pub max_concurrent_requests: usize,
    #[serde(default = "default_stateless_concurrent_requests_per_principal")]
    pub max_concurrent_requests_per_principal: usize,
    #[serde(default = "default_stateless_concurrent_backend_calls_per_target")]
    pub max_concurrent_backend_calls_per_target: usize,
}

impl Default for McpStatelessProtocolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            versions: default_stateless_protocol_versions(),
            discover_ttl_ms: default_stateless_discover_ttl_ms(),
            tools_list_ttl_ms: default_stateless_tools_list_ttl_ms(),
            max_discover_cache_entries: default_stateless_discover_cache_entries(),
            max_tools_list_cache_entries: default_stateless_tools_list_cache_entries(),
            max_tools_list_items: default_stateless_tools_list_items(),
            max_concurrent_requests: default_stateless_concurrent_requests(),
            max_concurrent_requests_per_principal:
                default_stateless_concurrent_requests_per_principal(),
            max_concurrent_backend_calls_per_target:
                default_stateless_concurrent_backend_calls_per_target(),
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
    #[serde(default, alias = "outputSchema")]
    pub output_schema: Option<JsonValue>,
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
                default,
                alias = "outputSchema",
                deserialize_with = "deserialize_optional_json_value"
            )]
            output_schema: Option<JsonValue>,
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
            output_schema: raw.output_schema,
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
    pub delegation: Option<DelegationClaims>,
    pub anonymous_binding: Option<String>,
}

#[derive(Debug, Clone)]
struct ForwardedHeaderContext {
    /// Headers visible to the established legacy authorization contract.
    policy_headers: Vec<(String, String)>,
    /// Headers eligible for legacy backend forwarding after profile-owned
    /// routing and credential fields have been removed.
    backend_headers: Vec<(String, String)>,
    /// Stable hashes for the small set of fields that may influence visibility.
    cache_headers: Vec<(String, String)>,
    /// False when legacy policy saw an unmodelled header. In that case cache
    /// lookup is disabled rather than risking an authorization-key collision.
    cache_eligible: bool,
}

impl ForwardedHeaderContext {
    fn legacy(headers: &[(String, String)]) -> Self {
        let backend_headers = headers
            .iter()
            .filter(|(name, _)| !should_regenerate_header(name))
            .cloned()
            .collect();
        let mut cache_headers = headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.to_ascii_lowercase();
                is_cache_identity_header(name.as_str()).then(|| {
                    let digest = Sha256::digest(value.as_bytes());
                    (name, format!("{digest:x}"))
                })
            })
            .collect::<Vec<_>>();
        cache_headers.sort();
        let cache_eligible = headers.iter().all(|(name, _)| {
            let name = name.to_ascii_lowercase();
            is_cache_identity_header(name.as_str()) || is_cache_ignored_header(name.as_str())
        });
        Self {
            policy_headers: headers.to_vec(),
            backend_headers,
            cache_headers,
            cache_eligible,
        }
    }

    #[allow(dead_code)] // Phase 4 activates the already-frozen modern boundary.
    fn stateless(headers: &[(String, String)], context: &McpRequestContext) -> Self {
        let mut admitted = headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "accept-language" | "traceparent" | "tracestate"
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(correlation_id) = context.correlation_id.as_deref() {
            admitted.push(("x-correlation-id".to_string(), correlation_id.to_string()));
        }
        if let Some(auth) = context.auth.as_ref() {
            if let Some(user_id) = auth.user_id.as_deref() {
                admitted.push(("x-user-id".to_string(), user_id.to_string()));
            }
            if let Some(host) = auth.host.as_deref() {
                admitted.push(("x-host-id".to_string(), host.to_string()));
            }
            if let Some(tenant) =
                normalized_claim(&auth.claims, &["tenant", "tenant_id", "tenantId", "tid"])
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            {
                admitted.push(("x-tenant-id".to_string(), tenant.to_string()));
            }
        }
        let legacy_shape = Self::legacy(&admitted);
        Self {
            policy_headers: admitted,
            backend_headers: legacy_shape.backend_headers,
            cache_headers: legacy_shape.cache_headers,
            cache_eligible: true,
        }
    }
}

#[derive(Debug)]
struct EffectiveMcpRequestContext<'a> {
    frontend_profile: FrontendProfile,
    protocol_version: &'a str,
    frontend_session_id: Option<&'a str>,
    forwarded_headers: ForwardedHeaderContext,
    transport_headers: &'a [(String, String)],
    request: &'a McpRequestContext,
}

impl EffectiveMcpRequestContext<'_> {
    fn legacy_backend_profile(&self) -> Result<BackendProfileContext<'_>, McpExecutionError> {
        if self.frontend_profile != FrontendProfile::Legacy {
            return Err(McpExecutionError::execution_failed(
                "legacy backend sessions require a legacy frontend profile",
            ));
        }
        Ok(BackendProfileContext {
            frontend: self.frontend_profile,
            backend: BackendProfile::LegacyStateful,
            protocol_version: self.protocol_version,
            frontend_session_id: self.frontend_session_id,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendProfile {
    LegacyStateful,
}

#[derive(Debug, Clone, Copy)]
struct BackendProfileContext<'a> {
    frontend: FrontendProfile,
    backend: BackendProfile,
    protocol_version: &'a str,
    frontend_session_id: Option<&'a str>,
}

#[derive(Debug, Clone)]
struct McpGatewaySession {
    protocol_version: String,
    principal_binding: String,
    capacity_key: String,
    client_info: JsonValue,
    client_capabilities: JsonValue,
    last_accessed: Instant,
    backend_sessions: BTreeMap<String, McpBackendSession>,
    binding_contract: u16,
}

impl McpGatewaySession {
    fn new_bound(
        protocol_version: String,
        principal_binding: String,
        capacity_key: String,
        client_info: JsonValue,
        client_capabilities: JsonValue,
    ) -> Self {
        Self {
            protocol_version,
            principal_binding,
            capacity_key,
            client_info,
            client_capabilities,
            last_accessed: Instant::now(),
            backend_sessions: BTreeMap::new(),
            binding_contract: LEGACY_SESSION_BINDING_CONTRACT,
        }
    }

    #[cfg(test)]
    fn new(protocol_version: String, capacity_key: String) -> Self {
        Self::new_bound(
            protocol_version,
            "test:any-principal".to_string(),
            capacity_key,
            json!({}),
            json!({}),
        )
    }

    fn is_expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last_accessed) >= MCP_SESSION_IDLE_TIMEOUT
    }

    fn touch(&mut self, now: Instant) {
        // Retain and access the negotiated metadata as part of the live
        // session contract; later profiles may use it for capability routing.
        let _ = (&self.client_info, &self.client_capabilities);
        self.last_accessed = now;
    }
}

#[derive(Debug, Clone, Default)]
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

    fn client_session_count(&self, capacity_key: &str) -> usize {
        self.client_session_counts
            .get(capacity_key)
            .copied()
            .unwrap_or_default()
    }

    fn insert(
        &mut self,
        session_id: String,
        session: McpGatewaySession,
    ) -> Option<McpGatewaySession> {
        let capacity_key = session.capacity_key.clone();
        let replaced = self.sessions.insert(session_id, session);
        if let Some(replaced) = replaced.as_ref() {
            self.decrement_client_session_count(replaced.capacity_key.as_str());
        }
        *self.client_session_counts.entry(capacity_key).or_insert(0) += 1;
        replaced
    }

    fn remove(&mut self, session_id: &str) -> Option<McpGatewaySession> {
        let removed = self.sessions.remove(session_id)?;
        self.decrement_client_session_count(removed.capacity_key.as_str());
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
struct ToolRetryPolicy {
    max_attempts: usize,
    status_codes: BTreeSet<u16>,
    retry_on_timeout: bool,
    retry_on_connect: bool,
    backoff: Duration,
}

impl ToolRetryPolicy {
    fn should_retry_status(&self, attempt: usize, status: u16) -> bool {
        attempt < self.max_attempts && self.status_codes.contains(&status)
    }

    fn should_retry_error(&self, attempt: usize, error: &reqwest::Error) -> bool {
        if attempt >= self.max_attempts {
            return false;
        }
        (self.retry_on_timeout && error.is_timeout())
            || (self.retry_on_connect && error.is_connect())
    }
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

struct LegacyFrontendAdapter<'a> {
    runtime: &'a McpRouterRuntime,
}

impl<'a> LegacyFrontendAdapter<'a> {
    fn new(runtime: &'a McpRouterRuntime) -> Self {
        Self { runtime }
    }

    async fn post(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
        response_mode: McpResponseMode,
        payload: JsonValue,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.runtime
            .handle_legacy_post(request, context, response_mode, payload)
            .await
    }

    async fn delete(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.runtime.handle_legacy_delete(request, context).await
    }
}

/// Sessionless frontend adapter. Production configuration keeps this adapter
/// disabled until Phase 5, while Phase 4 exercises the complete discovery/list
/// slice without granting it a session-store handle.
struct StatelessFrontendAdapter<'a> {
    runtime: &'a McpRouterRuntime,
}

impl<'a> StatelessFrontendAdapter<'a> {
    fn new(runtime: &'a McpRouterRuntime) -> Self {
        Self { runtime }
    }

    fn disabled_response(
        response_mode: McpResponseMode,
        id: JsonValue,
        version: &str,
    ) -> Result<McpHttpResponse, RuntimeError> {
        let _profile = FrontendProfile::Stateless;
        rpc_error_response(
            response_mode,
            400,
            id,
            -32600,
            format!("MCP stateless protocol version `{version}` is disabled"),
        )
    }

    async fn post(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
        payload: JsonValue,
        version: &str,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.runtime
            .handle_stateless_post(request, context, payload, version)
            .await
    }
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

#[derive(Debug, Clone)]
struct StatelessCacheEntry {
    value: JsonValue,
    expires_at: Instant,
}

#[derive(Debug, Clone, Default)]
struct StatelessResponseCache {
    max_entries: usize,
    entries: BTreeMap<String, StatelessCacheEntry>,
    order: VecDeque<String>,
}

impl StatelessResponseCache {
    fn new(max_entries: usize) -> Self {
        Self {
            max_entries,
            entries: BTreeMap::new(),
            order: VecDeque::new(),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&mut self, key: &str, now: Instant) -> Option<JsonValue> {
        self.entries.retain(|_, entry| entry.expires_at > now);
        self.order
            .retain(|candidate| self.entries.contains_key(candidate));
        let value = self.entries.get(key)?.value.clone();
        self.touch(key);
        Some(value)
    }

    fn insert(&mut self, key: String, value: JsonValue, ttl: Duration, now: Instant) {
        if self.max_entries == 0 {
            return;
        }
        self.entries.insert(
            key.clone(),
            StatelessCacheEntry {
                value,
                expires_at: now + ttl,
            },
        );
        self.touch(key.as_str());
        while self.entries.len() > self.max_entries {
            let Some(expired) = self.order.pop_front() else {
                break;
            };
            self.entries.remove(expired.as_str());
        }
    }

    fn touch(&mut self, key: &str) {
        self.order.retain(|candidate| candidate != key);
        self.order.push_back(key.to_string());
    }
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
    tools: BTreeMap<String, PreparedMcpTool>,
    schema_validation: SchemaValidationPool,
    stateless_resources: Arc<StatelessResourceBudgets>,
    client: reqwest::Client,
    direct_registry: DirectRegistryConfig,
    discovery: Option<Arc<dyn McpDiscoveryResolver>>,
    policy: Option<Arc<AccessControlRuntime>>,
    sessions: Arc<AsyncMutex<McpSessionStore>>,
    tools_list_cache: Arc<AsyncMutex<ToolsListVisibilityCache>>,
    stateless_discover_cache: Arc<AsyncMutex<StatelessResponseCache>>,
    stateless_tools_list_cache: Arc<AsyncMutex<StatelessResponseCache>>,
    last_session_purge: Arc<AsyncMutex<Option<Instant>>>,
    next_backend_request_id: Arc<AtomicU64>,
    config_revision: String,
    reload_session_evictions: Arc<AtomicU64>,
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
        mut config: McpRouterConfig,
        discovery: Option<Arc<dyn McpDiscoveryResolver>>,
        policy: Option<Arc<AccessControlRuntime>>,
        direct_registry: DirectRegistryConfig,
        client_config: ClientConfig,
    ) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        config.origin_allowlist = config
            .origin_allowlist
            .iter()
            .map(|origin| normalize_mcp_origin(origin))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| {
                RuntimeError::Unsupported(format!("invalid mcp-router.originAllowlist: {message}"))
            })?;
        let tools = prepare_tools(
            &config.tools,
            &config.schema,
            config.protocols.stateless.enabled,
        )
        .map_err(|message| RuntimeError::Unsupported(format!("invalid MCP schema: {message}")))?;
        let schema_validation = SchemaValidationPool::new(&config.schema).map_err(|message| {
            RuntimeError::Unsupported(format!("invalid MCP schema validation pool: {message}"))
        })?;
        let stateless_resources = StatelessResourceBudgets::new(
            config.protocols.stateless.max_concurrent_requests,
            config
                .protocols
                .stateless
                .max_concurrent_requests_per_principal,
        )
        .map_err(|message| RuntimeError::Unsupported(format!("invalid MCP limits: {message}")))?;
        let client = ClientFactory::from_config(&client_config)
            .reqwest_client(EndpointOptions::default())
            .map_err(|error| {
                RuntimeError::Unsupported(format!("invalid MCP HTTP client: {error}"))
            })?;
        let tools_list_cache_entries = policy
            .as_ref()
            .map(|policy| policy.tools_list_access_control().max_cache_entries)
            .unwrap_or_else(|| ToolsListAccessControlConfig::default().max_cache_entries);
        let stateless_discover_cache_entries =
            config.protocols.stateless.max_discover_cache_entries;
        let stateless_tools_list_cache_entries =
            config.protocols.stateless.max_tools_list_cache_entries;
        let config_revision =
            stable_json_hash(&serde_json::to_value(&config).unwrap_or(JsonValue::Null));
        Ok(Self {
            config,
            tools,
            schema_validation,
            stateless_resources: Arc::new(stateless_resources),
            client,
            direct_registry,
            discovery,
            policy,
            sessions: Arc::new(AsyncMutex::new(McpSessionStore::default())),
            tools_list_cache: Arc::new(AsyncMutex::new(ToolsListVisibilityCache::new(
                tools_list_cache_entries,
            ))),
            stateless_discover_cache: Arc::new(AsyncMutex::new(StatelessResponseCache::new(
                stateless_discover_cache_entries,
            ))),
            stateless_tools_list_cache: Arc::new(AsyncMutex::new(StatelessResponseCache::new(
                stateless_tools_list_cache_entries,
            ))),
            last_session_purge: Arc::new(AsyncMutex::new(None)),
            next_backend_request_id: Arc::new(AtomicU64::new(1)),
            config_revision,
            reload_session_evictions: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn config(&self) -> &McpRouterConfig {
        &self.config
    }

    pub fn max_request_body_bytes(&self) -> usize {
        self.config.max_request_body_bytes
    }

    fn legacy_protocol_version_enabled(&self, version: &str) -> bool {
        self.config.protocols.legacy.enabled
            && self
                .config
                .protocols
                .legacy
                .versions
                .iter()
                .any(|candidate| candidate == version)
    }

    fn classifier_config(&self) -> ClassifierConfig<'_> {
        ClassifierConfig {
            legacy_enabled: self.config.protocols.legacy.enabled,
            legacy_versions: &self.config.protocols.legacy.versions,
            stateless_enabled: self.config.protocols.stateless.enabled,
            stateless_versions: &self.config.protocols.stateless.versions,
        }
    }

    pub fn preflight_request(
        &self,
        path: &str,
        headers: &[(String, String)],
    ) -> Result<Option<McpHttpResponse>, RuntimeError> {
        if !self.matches_path(path) {
            return Ok(None);
        }
        let origins = headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("origin"))
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>();
        if origins.is_empty() {
            return Ok(None);
        }
        if origins.len() != 1 {
            return json_error_response(
                403,
                JsonValue::Null,
                -32001,
                "browser Origin is not allowed",
            )
            .map(Some);
        }
        let origin = match normalize_mcp_origin(origins[0]) {
            Ok(origin) => origin,
            Err(_) => {
                return json_error_response(
                    403,
                    JsonValue::Null,
                    -32001,
                    "browser Origin is not allowed",
                )
                .map(Some);
            }
        };
        if self
            .config
            .origin_allowlist
            .iter()
            .any(|candidate| candidate == &origin)
        {
            Ok(None)
        } else {
            json_error_response(
                403,
                JsonValue::Null,
                -32001,
                "browser Origin is not allowed",
            )
            .map(Some)
        }
    }

    pub fn request_body_too_large_response(&self) -> Result<McpHttpResponse, RuntimeError> {
        json_error_response(
            413,
            JsonValue::Null,
            -32600,
            "MCP request body exceeds maxRequestBodyBytes",
        )
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

    /// Carries only sessions whose negotiated version and binding contract are
    /// still valid in the freshly loaded runtime.
    pub fn preserve_state_from(&mut self, previous: &Self) {
        let Ok(mut previous_store) = previous.sessions.try_lock() else {
            tracing::warn!(
                target: "light_pingora::mcp",
                reason = "session_store_busy",
                "MCP reload deferred compatibility filtering"
            );
            self.sessions = Arc::clone(&previous.sessions);
            self.last_session_purge = Arc::clone(&previous.last_session_purge);
            self.next_backend_request_id = Arc::clone(&previous.next_backend_request_id);
            let runtime = self.clone();
            tokio::spawn(async move {
                runtime.evict_incompatible_reload_sessions().await;
            });
            return;
        };
        let incompatible_ids = previous_store
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                (!self.legacy_protocol_version_enabled(&session.protocol_version)
                    || session.binding_contract != LEGACY_SESSION_BINDING_CONTRACT)
                    .then(|| session_id.clone())
            })
            .collect::<Vec<_>>();
        let mut backend_sessions = Vec::new();
        for session_id in &incompatible_ids {
            if let Some(session) = previous_store.remove(session_id) {
                backend_sessions.extend(session.backend_sessions.into_values());
            }
        }
        let retained = previous_store.clone();
        drop(previous_store);
        self.sessions = Arc::new(AsyncMutex::new(retained));
        self.last_session_purge = Arc::clone(&previous.last_session_purge);
        self.next_backend_request_id = Arc::clone(&previous.next_backend_request_id);
        let evicted = incompatible_ids.len() as u64;
        if evicted > 0 {
            self.reload_session_evictions
                .fetch_add(evicted, Ordering::Relaxed);
            tracing::warn!(
                target: "light_pingora::mcp",
                reason = "incompatible_profile_or_binding",
                evicted_sessions = evicted,
                "MCP reload evicted incompatible legacy sessions"
            );
        }
        if !backend_sessions.is_empty() {
            self.terminate_backend_sessions_in_background(backend_sessions);
        }
    }

    async fn evict_incompatible_reload_sessions(&self) {
        let mut store = self.sessions.lock().await;
        let incompatible_ids = store
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                (!self.legacy_protocol_version_enabled(&session.protocol_version)
                    || session.binding_contract != LEGACY_SESSION_BINDING_CONTRACT)
                    .then(|| session_id.clone())
            })
            .collect::<Vec<_>>();
        let backend_sessions = incompatible_ids
            .iter()
            .filter_map(|session_id| store.remove(session_id))
            .flat_map(|session| session.backend_sessions.into_values())
            .collect::<Vec<_>>();
        drop(store);
        let evicted = incompatible_ids.len() as u64;
        if evicted > 0 {
            self.reload_session_evictions
                .fetch_add(evicted, Ordering::Relaxed);
            tracing::warn!(
                target: "light_pingora::mcp",
                reason = "incompatible_profile_or_binding",
                evicted_sessions = evicted,
                "MCP reload evicted incompatible legacy sessions"
            );
        }
        if !backend_sessions.is_empty() {
            self.terminate_backend_sessions_in_background(backend_sessions);
        }
    }

    pub async fn handle_request(
        &self,
        request: McpHttpRequest,
    ) -> Result<Option<McpHttpResponse>, RuntimeError> {
        #[cfg(test)]
        let context = McpRequestContext {
            anonymous_binding: Some("in-process-test-client".to_string()),
            ..McpRequestContext::default()
        };
        #[cfg(not(test))]
        let context = McpRequestContext::default();
        self.handle_request_with_context(request, context).await
    }

    pub async fn handle_request_with_context(
        &self,
        request: McpHttpRequest,
        context: McpRequestContext,
    ) -> Result<Option<McpHttpResponse>, RuntimeError> {
        let started = Instant::now();
        if tracing::enabled!(target: "light_pingora::mcp", tracing::Level::DEBUG) {
            tracing::debug!(
                target: "light_pingora::mcp",
                http_method = %request.method,
                path = %request.path,
                header_count = request.headers.len(),
                body_bytes = request.body.len(),
                session_id_present = session_id_from_path_and_headers(
                    request.path.as_str(),
                    &request.headers,
                )
                .is_some(),
                correlation_id = ?context.correlation_id,
                "MCP router received request"
            );
        }
        if !self.matches_path(request.path.as_str()) {
            return Ok(None);
        }

        if let Some(response) = self.preflight_request(request.path.as_str(), &request.headers)? {
            return Ok(Some(response));
        }
        if request.body.len() > self.config.max_request_body_bytes {
            return self.request_body_too_large_response().map(Some);
        }

        let method = request.method.to_ascii_uppercase();
        let protocol_headers = all_headers(&request.headers, MCP_PROTOCOL_VERSION_HEADER);
        let session_id_present =
            session_id_from_path_and_headers(request.path.as_str(), &request.headers).is_some();
        let head_classification = classify_request_head(
            self.classifier_config(),
            RequestHead {
                method: method.as_str(),
                protocol_versions: &protocol_headers,
                session_id_present,
            },
        );
        let outcome = match (method.as_str(), head_classification) {
            (_, Err(rejection)) => {
                classification_rejection_response(McpResponseMode::Json, JsonValue::Null, rejection)
                    .map(Some)
            }
            ("POST", Ok(None)) => self.handle_post(request, &context).await.map(Some),
            ("GET", Ok(Some(_))) => Ok(Some(method_not_allowed_response())),
            ("DELETE", Ok(Some(Classification::Legacy))) => LegacyFrontendAdapter::new(self)
                .delete(request, &context)
                .await
                .map(Some),
            ("DELETE", Ok(Some(Classification::Stateless { .. }))) => {
                Ok(Some(method_not_allowed_response()))
            }
            _ => Ok(Some(method_not_allowed_response())),
        };
        match &outcome {
            Ok(Some(response)) => tracing::debug!(
                target: "light_pingora::mcp",
                http_method = %method,
                status = response.status,
                content_type = %response.content_type,
                body_bytes = response.body.len(),
                streamed = response.streamed,
                elapsed_ms = started.elapsed().as_millis(),
                correlation_id = ?context.correlation_id,
                "MCP router completed request"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(
                target: "light_pingora::mcp",
                http_method = %method,
                elapsed_ms = started.elapsed().as_millis(),
                correlation_id = ?context.correlation_id,
                error = %error,
                "MCP router failed to process request"
            ),
        }
        outcome
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
        if json_value_depth(&payload) > self.config.max_json_depth {
            return rpc_error_response(
                response_mode,
                400,
                JsonValue::Null,
                -32600,
                "JSON-RPC request exceeds maxJsonDepth",
            );
        }
        let protocol_headers = all_headers(&request.headers, MCP_PROTOCOL_VERSION_HEADER);
        let claims_stateless = protocol_headers.iter().any(|version| {
            *version == STATELESS_RC_VERSION
                || self
                    .config
                    .protocols
                    .stateless
                    .versions
                    .iter()
                    .any(|candidate| candidate == *version)
        }) || message
            .get("params")
            .and_then(JsonValue::as_object)
            .and_then(|params| params.get("_meta"))
            .and_then(JsonValue::as_object)
            .is_some_and(|meta| meta.contains_key(STATELESS_PROTOCOL_META_KEY));
        let classification = classify_post(
            self.classifier_config(),
            &protocol_headers,
            session_id_from_path_and_headers(request.path.as_str(), &request.headers).is_some(),
            message,
        );
        let request_id = message.get("id").cloned().unwrap_or(JsonValue::Null);
        match classification {
            Ok(Classification::Legacy) => {
                LegacyFrontendAdapter::new(self)
                    .post(request, context, response_mode, payload)
                    .await
            }
            Ok(Classification::Stateless { version, enabled }) => {
                if enabled {
                    StatelessFrontendAdapter::new(self)
                        .post(request, context, payload, &version)
                        .await
                } else {
                    StatelessFrontendAdapter::disabled_response(response_mode, request_id, &version)
                }
            }
            Err(rejection) => classification_rejection_response_for_post(
                response_mode,
                request_id,
                rejection,
                claims_stateless,
                &self.config.protocols.stateless.versions,
            ),
        }
    }

    async fn handle_stateless_post(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
        payload: JsonValue,
        version: &str,
    ) -> Result<McpHttpResponse, RuntimeError> {
        let message = payload
            .as_object()
            .expect("POST classifier accepts only an object message");
        let id = message.get("id").cloned().unwrap_or(JsonValue::Null);
        let metadata = match validate_stateless_request(&request.headers, message) {
            Ok(metadata) => metadata,
            Err(error) => return stateless_error_response(id, error, version),
        };
        if let Some(delegation) = context.delegation.as_ref() {
            let binding = match metadata.method.as_str() {
                "tools/list" => delegation.validate_binding(
                    "light-gateway",
                    DelegationKind::ToolsList,
                    None,
                    chrono::Utc::now().timestamp(),
                ),
                "tools/call" => delegation.validate_binding(
                    "light-gateway",
                    DelegationKind::ToolCall,
                    message
                        .get("params")
                        .and_then(JsonValue::as_object)
                        .and_then(|params| params.get("name"))
                        .and_then(JsonValue::as_str),
                    chrono::Utc::now().timestamp(),
                ),
                _ => Err(agent_delegation::DelegationError::Binding),
            };
            if binding.is_err() {
                return response_with_protocol_version(
                    json_error_response(
                        403,
                        id,
                        -32001,
                        "delegated authority does not permit this MCP request",
                    ),
                    Some(version),
                );
            }
        }
        let (_, principal_key) = match request_principal_binding(context) {
            Ok(binding) => binding,
            Err(error) => {
                return response_with_protocol_version(
                    json_error_response(error.status, id, error.code, error.message),
                    Some(version),
                );
            }
        };
        let Some(_permit) = self.stateless_resources.try_request(&principal_key) else {
            return response_with_protocol_version(
                json_error_response(
                    429,
                    id,
                    -32000,
                    "stateless MCP request exceeds a gateway resource limit",
                ),
                Some(version),
            );
        };
        let effective = EffectiveMcpRequestContext {
            frontend_profile: FrontendProfile::Stateless,
            protocol_version: version,
            frontend_session_id: None,
            forwarded_headers: ForwardedHeaderContext::stateless(&request.headers, context),
            transport_headers: &request.headers,
            request: context,
        };
        let result = match metadata.method.as_str() {
            "server/discover" => self.stateless_discover_result(&effective).await,
            "tools/list" => self.stateless_tools_list_result(message, &effective).await,
            "tools/call" => self
                .handle_tool_call(message, &effective)
                .await
                .map(stateless_tool_result)
                .map_err(|error| McpSessionError {
                    status: if error.code == -32020 { 400 } else { 200 },
                    code: error.code,
                    message: error.message,
                }),
            method => {
                return response_with_protocol_version(
                    json_error_response(404, id, -32601, format!("method `{method}` not found")),
                    Some(version),
                );
            }
        };
        match result {
            Ok(result) => {
                let Some(response) = bounded_json_result_response(
                    200,
                    &id,
                    &result,
                    self.config.max_response_body_bytes,
                )?
                else {
                    let error = if metadata.method == "tools/list" {
                        stateless_catalog_limit_error()
                    } else {
                        McpSessionError {
                            status: 400,
                            code: -32000,
                            message: "stateless MCP response exceeds a gateway limit".to_string(),
                        }
                    };
                    return response_with_protocol_version(
                        json_error_response(error.status, id, error.code, error.message),
                        Some(version),
                    );
                };
                Ok(apply_protocol_version_header(response, version))
            }
            Err(error) => response_with_protocol_version(
                json_error_response(error.status, id, error.code, error.message),
                Some(version),
            ),
        }
    }

    async fn stateless_discover_result(
        &self,
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> Result<JsonValue, McpSessionError> {
        let key = self.stateless_cache_key("server/discover", effective)?;
        let now = Instant::now();
        if let Some(result) = self.stateless_discover_cache.lock().await.get(&key, now) {
            return Ok(result);
        }
        let ttl_ms = self.config.protocols.stateless.discover_ttl_ms;
        let result = json!({
            "supportedVersions": self.config.protocols.stateless.versions,
            "capabilities": {"tools": {"listChanged": false}},
            "resultType": "complete",
            "_meta": {
                SERVER_INFO_META_KEY: {
                    "name": "light-gateway",
                    "version": env!("CARGO_PKG_VERSION")
                }
            },
            "instructions": "This gateway exposes an authorization-filtered MCP tool facade.",
            "ttlMs": ttl_ms,
            "cacheScope": "private"
        });
        self.stateless_discover_cache.lock().await.insert(
            key,
            result.clone(),
            Duration::from_millis(ttl_ms),
            now,
        );
        Ok(result)
    }

    async fn stateless_tools_list_result(
        &self,
        message: &JsonMap<String, JsonValue>,
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> Result<JsonValue, McpSessionError> {
        let params = message
            .get("params")
            .and_then(JsonValue::as_object)
            .expect("stateless transport validates params");
        if let Some(cursor) = params.get("cursor") {
            match cursor {
                JsonValue::Null => {}
                JsonValue::String(cursor) if cursor.is_empty() => {}
                JsonValue::String(_) => {
                    return Err(McpSessionError {
                        status: 400,
                        code: -32602,
                        message: "pagination cursor is not supported".to_string(),
                    });
                }
                _ => {
                    return Err(McpSessionError {
                        status: 400,
                        code: -32602,
                        message: "pagination cursor must be a string".to_string(),
                    });
                }
            }
        }
        if params
            .get("query")
            .or_else(|| params.get("intent"))
            .is_some()
        {
            return Err(McpSessionError {
                status: 400,
                code: -32602,
                message: "stateless tools/list does not support legacy query or intent".to_string(),
            });
        }
        let key = self.stateless_cache_key("tools/list", effective)?;
        let now = Instant::now();
        if let Some(result) = self.stateless_tools_list_cache.lock().await.get(&key, now) {
            return Ok(result);
        }
        let mut visible_names = Vec::new();
        for (index, tool) in self.tools.values().enumerate() {
            if self
                .tool_visible_for_list(
                    tool,
                    index,
                    &effective.forwarded_headers.policy_headers,
                    effective.request,
                )
                .await
            {
                visible_names.push(tool.name.clone());
            }
        }
        if visible_names.len() > self.config.protocols.stateless.max_tools_list_items {
            return Err(stateless_catalog_limit_error());
        }
        let mut result = self.tools_list_response_from_names(visible_names);
        let ttl_ms = self.config.protocols.stateless.tools_list_ttl_ms;
        let result_object = result.as_object_mut().expect("tools/list result object");
        result_object.insert("resultType".to_string(), json!("complete"));
        result_object.insert("ttlMs".to_string(), json!(ttl_ms));
        result_object.insert("cacheScope".to_string(), json!("private"));
        let encoded = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": message.get("id").cloned().unwrap_or(JsonValue::Null),
            "result": result
        }))
        .expect("JSON value serialization cannot fail");
        if encoded.len() > self.config.max_response_body_bytes {
            return Err(stateless_catalog_limit_error());
        }
        self.stateless_tools_list_cache.lock().await.insert(
            key,
            result.clone(),
            Duration::from_millis(ttl_ms),
            now,
        );
        Ok(result)
    }

    fn stateless_cache_key(
        &self,
        operation: &str,
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> Result<String, McpSessionError> {
        let (principal, _) = request_principal_binding(effective.request)?;
        let policy_revision = self
            .policy
            .as_ref()
            .map(|policy| policy.policy_revision())
            .unwrap_or_else(|| "none".to_string());
        let identity = json!({
            "operation": operation,
            "profile": "stateless",
            "protocolVersion": effective.protocol_version,
            "principalFingerprint": stable_json_hash(&json!(principal)),
            "headers": effective.forwarded_headers.cache_headers,
            "configRevision": self.config_revision,
            "policyRevision": policy_revision,
        });
        Ok(stable_json_hash(&identity))
    }

    async fn handle_legacy_post(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
        response_mode: McpResponseMode,
        payload: JsonValue,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.purge_expired_sessions(false).await;
        let protocol_headers = all_headers(&request.headers, MCP_PROTOCOL_VERSION_HEADER);
        let message = payload
            .as_object()
            .expect("POST classifier accepts only an object message");
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(JsonValue::as_str);
        if method.is_none() {
            return rpc_error_response(
                response_mode,
                400,
                id.unwrap_or(JsonValue::Null),
                -32600,
                "invalid JSON-RPC request",
            );
        }
        let method = method.unwrap_or_default();
        if method == "initialize"
            && let (Some(header_version), Some(requested_version)) = (
                protocol_headers.first(),
                requested_protocol_version(message),
            )
            && *header_version != requested_version
        {
            return rpc_error_response(
                response_mode,
                400,
                id.unwrap_or(JsonValue::Null),
                -32600,
                "MCP protocol version header does not match initialize params",
            );
        }
        if tracing::enabled!(target: "light_pingora::mcp", tracing::Level::DEBUG) {
            tracing::debug!(
                target: "light_pingora::mcp",
                rpc_method = method,
                request_id = ?id,
                session_id_present = session_id_from_path_and_headers(
                    request.path.as_str(),
                    &request.headers,
                )
                .is_some(),
                correlation_id = ?context.correlation_id,
                "MCP JSON-RPC method invoked"
            );
        }
        if let Some(delegation) = context.delegation.as_ref() {
            let binding = match method {
                "tools/list" => delegation.validate_binding(
                    "light-gateway",
                    DelegationKind::ToolsList,
                    None,
                    chrono::Utc::now().timestamp(),
                ),
                "tools/call" => delegation.validate_binding(
                    "light-gateway",
                    DelegationKind::ToolCall,
                    message
                        .get("params")
                        .and_then(JsonValue::as_object)
                        .and_then(|params| params.get("name"))
                        .and_then(JsonValue::as_str),
                    chrono::Utc::now().timestamp(),
                ),
                _ => Err(agent_delegation::DelegationError::Binding),
            };
            if binding.is_err() {
                return rpc_error_response(
                    response_mode,
                    403,
                    id.clone().unwrap_or(JsonValue::Null),
                    -32001,
                    "delegated authority does not permit this MCP request",
                );
            }
        }
        let frontend_session = if method == "initialize" {
            None
        } else {
            match self
                .validate_frontend_session(request.path.as_str(), &request.headers, context)
                .await
            {
                Ok(session) => Some(session),
                Err(error) => {
                    tracing::warn!(
                        target: "light_pingora::mcp",
                        rpc_method = method,
                        path = %request.path,
                        status = error.status,
                        code = error.code,
                        reason = %error.message,
                        correlation_id = ?context.correlation_id,
                        "MCP request rejected during frontend session validation"
                    );
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
                let metadata = match self.validate_initialize(message) {
                    Ok(metadata) => metadata,
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
                let result = self.initialize_result(metadata.protocol_version.as_str());
                let session_id = match self.create_frontend_session(metadata, context).await {
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
            "tools/list" => {
                let session = frontend_session.as_ref().expect("legacy session validated");
                let effective = EffectiveMcpRequestContext {
                    frontend_profile: FrontendProfile::Legacy,
                    protocol_version: session.protocol_version.as_str(),
                    frontend_session_id: Some(session.id.as_str()),
                    forwarded_headers: ForwardedHeaderContext::legacy(&request.headers),
                    transport_headers: &request.headers,
                    request: context,
                };
                response_with_protocol_version(
                    rpc_result_response(
                        response_mode,
                        200,
                        id,
                        self.tools_list_result(message, &effective).await,
                    ),
                    Some(session.protocol_version.as_str()),
                )
            }
            "tools/call" => {
                let session = frontend_session.as_ref().expect("legacy session validated");
                let effective = EffectiveMcpRequestContext {
                    frontend_profile: FrontendProfile::Legacy,
                    protocol_version: session.protocol_version.as_str(),
                    frontend_session_id: Some(session.id.as_str()),
                    forwarded_headers: ForwardedHeaderContext::legacy(&request.headers),
                    transport_headers: &request.headers,
                    request: context,
                };
                match self.handle_tool_call(message, &effective).await {
                    Ok(result) => response_with_protocol_version(
                        rpc_result_response(response_mode, 200, id, result),
                        Some(session.protocol_version.as_str()),
                    ),
                    Err(error) => response_with_protocol_version(
                        rpc_error_response(response_mode, 200, id, error.code, error.message),
                        Some(session.protocol_version.as_str()),
                    ),
                }
            }
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

    async fn handle_legacy_delete(
        &self,
        request: McpHttpRequest,
        context: &McpRequestContext,
    ) -> Result<McpHttpResponse, RuntimeError> {
        self.purge_expired_sessions(false).await;
        let removed_session = match self
            .remove_frontend_session(request.path.as_str(), &request.headers, context)
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

    fn validate_initialize(
        &self,
        message: &serde_json::Map<String, JsonValue>,
    ) -> Result<LegacyInitializeMetadata, McpSessionError> {
        let protocol_version = requested_protocol_version(message)
            .unwrap_or(DEFAULT_PROTOCOL_VERSION)
            .trim();
        if !self.legacy_protocol_version_enabled(protocol_version) {
            return Err(McpSessionError {
                status: 400,
                code: -32600,
                message: format!("unsupported MCP protocol version `{protocol_version}`"),
            });
        }
        let params = message.get("params").and_then(JsonValue::as_object);
        let client_info = params
            .and_then(|params| params.get("clientInfo"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let capabilities = params
            .and_then(|params| params.get("capabilities"))
            .cloned()
            .unwrap_or_else(|| json!({}));

        if protocol_version == "2025-11-25" {
            let valid_client_info = client_info.as_object().is_some_and(|client_info| {
                ["name", "version"].into_iter().all(|field| {
                    client_info
                        .get(field)
                        .and_then(JsonValue::as_str)
                        .is_some_and(|value| !value.trim().is_empty())
                })
            });
            if !valid_client_info || !capabilities.is_object() {
                return Err(McpSessionError {
                    status: 400,
                    code: -32602,
                    message: "2025-11-25 initialize requires clientInfo name/version and object capabilities"
                        .to_string(),
                });
            }
        } else if !client_info.is_object() || !capabilities.is_object() {
            return Err(McpSessionError {
                status: 400,
                code: -32602,
                message: "initialize clientInfo and capabilities must be objects".to_string(),
            });
        }
        for (name, value) in [
            ("clientInfo", &client_info),
            ("capabilities", &capabilities),
        ] {
            if json_value_depth(value) > MAX_LEGACY_CLIENT_METADATA_DEPTH
                || serde_json::to_vec(value)
                    .map(|encoded| encoded.len() > MAX_LEGACY_CLIENT_METADATA_BYTES)
                    .unwrap_or(true)
            {
                return Err(McpSessionError {
                    status: 400,
                    code: -32602,
                    message: format!("initialize {name} exceeds the legacy metadata bound"),
                });
            }
        }
        Ok(LegacyInitializeMetadata {
            protocol_version: protocol_version.to_string(),
            client_info,
            capabilities,
        })
    }

    fn initialize_result(&self, protocol_version: &str) -> JsonValue {
        json!({
            "protocolVersion": protocol_version,
            "capabilities": {
                "tools": {
                    "listChanged": false
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
        metadata: LegacyInitializeMetadata,
        context: &McpRequestContext,
    ) -> Result<String, McpSessionError> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let (principal_binding, capacity_key) = request_principal_binding(context)?;
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
            let client_sessions = store.client_session_count(capacity_key.as_str());
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
                McpGatewaySession::new_bound(
                    metadata.protocol_version,
                    principal_binding,
                    capacity_key,
                    metadata.client_info,
                    metadata.capabilities,
                ),
            );
            return Ok(session_id);
        }
    }

    async fn validate_frontend_session(
        &self,
        path: &str,
        headers: &[(String, String)],
        context: &McpRequestContext,
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
            let incompatible = store.get(session_id.as_str()).is_some_and(|session| {
                !self.legacy_protocol_version_enabled(&session.protocol_version)
                    || session.binding_contract != LEGACY_SESSION_BINDING_CONTRACT
            });
            let expired = store
                .get(session_id.as_str())
                .is_some_and(|session| session.is_expired(now));
            if expired || incompatible {
                if let Some(session) = store.remove(session_id.as_str()) {
                    expired_backend_sessions = session.backend_sessions.into_values().collect();
                }
                if incompatible {
                    self.reload_session_evictions
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        target: "light_pingora::mcp",
                        reason = "incompatible_profile_or_binding",
                        evicted_sessions = 1,
                        "MCP request evicted an incompatible legacy session"
                    );
                }
                Err(McpSessionError {
                    status: 400,
                    code: -32000,
                    message: "unknown MCP session id".to_string(),
                })
            } else if let Some(session) = store.get_mut(session_id.as_str()) {
                if !is_test_principal_wildcard(&session.principal_binding)
                    && !principal_binding_matches(
                        &session.principal_binding,
                        &request_principal_binding(context)?.0,
                    )
                {
                    return Err(McpSessionError {
                        status: 403,
                        code: -32001,
                        message: "MCP session principal does not match".to_string(),
                    });
                }
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
        context: &McpRequestContext,
    ) -> Result<RemovedMcpSession, McpSessionError> {
        let session_id =
            session_id_from_path_and_headers(path, headers).ok_or_else(|| McpSessionError {
                status: 400,
                code: -32600,
                message: "missing MCP session id".to_string(),
            })?;
        let mut store = self.sessions.lock().await;
        let session = store
            .get(session_id.as_str())
            .ok_or_else(|| McpSessionError {
                status: 400,
                code: -32000,
                message: "unknown MCP session id".to_string(),
            })?;
        if !is_test_principal_wildcard(&session.principal_binding)
            && !principal_binding_matches(
                &session.principal_binding,
                &request_principal_binding(context)?.0,
            )
        {
            return Err(McpSessionError {
                status: 403,
                code: -32001,
                message: "MCP session principal does not match".to_string(),
            });
        }
        let session = store.remove(session_id.as_str()).expect("checked above");
        Ok(RemovedMcpSession {
            protocol_version: session.protocol_version,
            backend_sessions: session.backend_sessions.into_values().collect(),
        })
    }

    async fn tools_list_result(
        &self,
        message: &serde_json::Map<String, JsonValue>,
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> JsonValue {
        let query = message
            .get("params")
            .and_then(|params| params.get("query").or_else(|| params.get("intent")))
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_ascii_lowercase());
        let cache_key = self.tools_list_cache_key(query.as_deref(), effective);
        if let Some(cache_key) = cache_key.as_deref() {
            let cached = self.tools_list_cache.lock().await.get(cache_key);
            if let Some(tool_names) = cached {
                tracing::debug!(
                    target: "light_pingora::mcp",
                    cacheKey = %cache_key,
                    query = ?query,
                    toolCount = tool_names.len(),
                    "mcp tools/list visibility cache hit"
                );
                return self.tools_list_response_from_names(tool_names);
            }
            tracing::debug!(
                target: "light_pingora::mcp",
                cacheKey = %cache_key,
                query = ?query,
                "mcp tools/list visibility cache miss"
            );
        }
        let candidates = self
            .tools
            .values()
            .filter(|tool| {
                query
                    .as_deref()
                    .is_none_or(|query| tool_matches_tools_list_query(tool, query))
            })
            .collect::<Vec<_>>();
        let mut visible_tools = Vec::new();
        for (index, tool) in candidates.into_iter().enumerate() {
            if self
                .tool_visible_for_list(
                    tool,
                    index,
                    &effective.forwarded_headers.policy_headers,
                    effective.request,
                )
                .await
            {
                visible_tools.push(tool);
            }
        }
        let visible_tool_names = visible_tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        if let Some(cache_key) = cache_key {
            tracing::debug!(
                target: "light_pingora::mcp",
                cacheKey = %cache_key,
                query = ?query,
                toolCount = visible_tool_names.len(),
                "mcp tools/list visibility cache store"
            );
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
                let mut descriptor = json!({
                    "name": tool.name,
                    "description": tool.description,
                    "inputSchema": tool.input_schema
                });
                if let Some(output_schema) = tool.output_schema.as_ref() {
                    descriptor
                        .as_object_mut()
                        .expect("tool descriptor is an object")
                        .insert("outputSchema".to_string(), output_schema.clone());
                }
                descriptor
            })
            .collect::<Vec<_>>();
        json!({ "tools": tools })
    }

    fn tools_list_cache_key(
        &self,
        query: Option<&str>,
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> Option<String> {
        let policy = self.policy.as_ref()?;
        let tools_list_config = policy.tools_list_access_control();
        if tools_list_config.mode == ToolsListAccessControlMode::None
            || tools_list_config.max_cache_entries == 0
            || !effective.forwarded_headers.cache_eligible
        {
            return None;
        }
        let context = effective.request;
        let principal = json!({
            "clientId": context.auth.as_ref().and_then(|auth| auth.client_id.as_deref()),
            "userId": context.auth.as_ref().and_then(|auth| auth.user_id.as_deref()),
            "issuer": context.auth.as_ref().and_then(|auth| auth.issuer.as_deref()),
            "claims": policy.normalized_claims_for_visibility(context.auth.as_ref()),
        });
        let identity = json!({
            "principalFingerprint": stable_json_hash(&principal),
            "headers": effective.forwarded_headers.cache_headers,
            "legacyQuery": query.unwrap_or_default(),
            "profile": "legacy",
            "protocolVersion": effective.protocol_version,
            "configRevision": self.config_revision,
            "policyRevision": policy.policy_revision(),
        });
        Some(format!(
            "{:?}|{}",
            tools_list_config.mode,
            stable_json_hash(&identity)
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
        effective: &EffectiveMcpRequestContext<'_>,
    ) -> Result<JsonValue, McpExecutionError> {
        let context = effective.request;
        let policy_headers = &effective.forwarded_headers.policy_headers;
        let backend_headers = &effective.forwarded_headers.backend_headers;
        if tracing::enabled!(target: "light_pingora::mcp", tracing::Level::DEBUG) {
            let requested_tool_name = message
                .get("params")
                .and_then(JsonValue::as_object)
                .and_then(|params| params.get("name"))
                .and_then(JsonValue::as_str)
                .unwrap_or("<missing>");
            tracing::debug!(
                target: "light_pingora::mcp",
                tool_name = requested_tool_name,
                session_id_present = effective.frontend_session_id.is_some(),
                correlation_id = ?context.correlation_id,
                "MCP tools/call invoked"
            );
        }
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

        if let Some(policy) = self.policy.as_ref()
            && let AccessDecision::Denied(message) = policy.authorize_tool_by_claims(
                tool.name.as_str(),
                endpoint.as_str(),
                context.auth.as_ref(),
            )
        {
            log_mcp_tool_call(
                tool,
                endpoint.as_str(),
                started,
                "denied",
                "denied",
                context,
            );
            return Err(McpExecutionError {
                code: -32001,
                message,
            });
        }

        match self
            .schema_validation
            .validate(Arc::clone(&tool.input_validator), arguments.clone())
            .await
        {
            ValidationOutcome::Valid => {}
            ValidationOutcome::Invalid(diagnostics) => {
                return Ok(mcp_schema_error_result(
                    "Tool arguments did not conform to inputSchema",
                    &diagnostics,
                ));
            }
            ValidationOutcome::Overloaded => {
                return Ok(mcp_tool_error_result(
                    "Schema validation capacity is temporarily exhausted",
                ));
            }
            ValidationOutcome::WorkerFailed => {
                return Ok(mcp_tool_error_result("Schema validation failed safely"));
            }
        }

        if effective.frontend_profile == FrontendProfile::Stateless {
            let expected_headers = expected_parameter_headers(tool, &arguments)?;
            validate_parameter_headers(effective.transport_headers, &expected_headers).map_err(
                |error| McpExecutionError {
                    code: error.code,
                    message: error.message,
                },
            )?;
        }

        if let Some(policy) = self.policy.as_ref() {
            match policy
                .authorize_tool(
                    tool.name.as_str(),
                    endpoint.as_str(),
                    policy_headers,
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

        let _backend_permit = if effective.frontend_profile == FrontendProfile::Stateless
            && tool.api_type == McpToolType::Http
        {
            Some(
                self.stateless_resources
                    .try_backend_call(
                        stateless_target_key(tool).as_str(),
                        self.config
                            .protocols
                            .stateless
                            .max_concurrent_backend_calls_per_target,
                    )
                    .ok_or_else(|| {
                        McpExecutionError::execution_failed(
                            "stateless MCP backend target exceeds a gateway resource limit",
                        )
                    })?,
            )
        } else {
            None
        };

        let execution = match tool.api_type {
            McpToolType::Http => {
                self.execute_http_tool(tool, &masked_arguments, backend_headers)
                    .await
            }
            McpToolType::Mcp => {
                if effective.frontend_profile == FrontendProfile::Stateless {
                    Ok(mcp_tool_error_result(
                        "Gateway does not support stateless calls to this MCP backend profile",
                    ))
                } else {
                    let backend_profile = effective.legacy_backend_profile()?;
                    self.execute_mcp_proxy_tool(
                        tool,
                        &masked_arguments,
                        backend_headers,
                        backend_profile,
                    )
                    .await
                }
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
        let (mut result, status) = if let Some(policy) = self.policy.as_ref() {
            let filtered = policy
                .filter_mcp_response(
                    tool.name.as_str(),
                    endpoint.as_str(),
                    policy_headers,
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
        let (result, status) = if !is_mcp_error_result(&result) {
            if let Some(validator) = tool.output_validator.as_ref() {
                let Some(mut structured_content) = result.get("structuredContent").cloned() else {
                    let result = mcp_tool_error_result(
                        "Tool output did not provide required structuredContent",
                    );
                    log_mcp_tool_call(
                        tool,
                        endpoint.as_str(),
                        started,
                        "output_schema_error",
                        policy_outcome,
                        context,
                    );
                    return Ok(result);
                };
                if tool
                    .output_schema
                    .as_ref()
                    .is_some_and(schema_declares_array_root)
                    && let Some(items) = structured_content
                        .as_object()
                        .filter(|object| object.len() == 1)
                        .and_then(|object| object.get("items"))
                        .filter(|items| items.is_array())
                        .cloned()
                {
                    structured_content = items;
                    replace_structured_content(&mut result, structured_content.clone());
                }
                match self
                    .schema_validation
                    .validate(Arc::clone(validator), structured_content)
                    .await
                {
                    ValidationOutcome::Valid => (result, status),
                    ValidationOutcome::Invalid(diagnostics) => (
                        mcp_schema_error_result(
                            "Tool output did not conform to outputSchema",
                            &diagnostics,
                        ),
                        "output_schema_error",
                    ),
                    ValidationOutcome::Overloaded => (
                        mcp_tool_error_result(
                            "Schema validation capacity is temporarily exhausted",
                        ),
                        "output_schema_error",
                    ),
                    ValidationOutcome::WorkerFailed => (
                        mcp_tool_error_result("Schema validation failed safely"),
                        "output_schema_error",
                    ),
                }
            } else {
                (result, status)
            }
        } else {
            (result, status)
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

        let mut path_params = BTreeMap::new();
        let mut query_params = BTreeMap::new();
        let mut header_params = BTreeMap::new();
        let mut cookie_params = BTreeMap::new();
        let mut body_val: Option<&JsonValue> = None;
        let path_placeholders =
            openapi_path_placeholders(tool.path.as_str()).map_err(|message| {
                McpExecutionError::execution_failed(format!(
                    "tool `{}` path `{}` is invalid: {message}",
                    tool.name, tool.path
                ))
            })?;

        let mapping = tool_parameter_mapping(tool);
        let has_mapping = mapping.is_some();
        validate_path_placeholder_mapping(tool, mapping, &path_placeholders)?;

        if let Some(mapping) = mapping
            && let Some(args_obj) = arguments.as_object()
        {
            for (key, val) in args_obj {
                match parameter_mapping_location(mapping, key, tool.name.as_str())? {
                    Some(ParameterLocation::Path) => {
                        path_params.insert(key.clone(), val);
                    }
                    Some(ParameterLocation::Query) => {
                        query_params.insert(key.clone(), val);
                    }
                    Some(ParameterLocation::Header) => {
                        header_params.insert(key.clone(), val);
                    }
                    Some(ParameterLocation::Cookie) => {
                        cookie_params.insert(key.clone(), val);
                    }
                    Some(ParameterLocation::Body) => {
                        body_val = Some(val);
                    }
                    None => {
                        if key == "body" {
                            body_val = Some(val);
                        } else if matches!(method, McpHttpMethod::Get | McpHttpMethod::Head) {
                            query_params.insert(key.clone(), val);
                        }
                    }
                }
            }
        }

        if !path_placeholders.is_empty() {
            let placeholder_names = path_placeholders
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            for key in path_params.keys() {
                if !placeholder_names.contains(key.as_str()) {
                    return Err(McpExecutionError::execution_failed(format!(
                        "tool `{}` maps `{key}` to path, but path `{}` has no `{{{key}}}` placeholder",
                        tool.name, tool.path
                    )));
                }
            }
            for placeholder in &path_placeholders {
                if !path_params.contains_key(placeholder) {
                    return Err(McpExecutionError::invalid_params(format!(
                        "tool `{}` requires path argument `{placeholder}`",
                        tool.name
                    )));
                }
            }
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
                let placeholder3 = format!("%7b{}%7d", key);
                path = path.replace(&placeholder3, &encoded_val);
            }
            url.set_path(&path);
        } else if !path_params.is_empty() {
            let mapped = path_params
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(McpExecutionError::execution_failed(format!(
                "tool `{}` maps path parameters [{mapped}], but path `{}` has no supported placeholders",
                tool.name, tool.path
            )));
        }

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

        let mut request_headers = outbound_headers(agent_headers)?;
        if has_mapping {
            for (key, val) in header_params {
                if val.is_null() {
                    continue;
                }
                let val_str = json_value_to_query(val);
                let header_name = mapped_header_name(tool.name.as_str(), key.as_str())?;
                let header_val = HeaderValue::from_str(&val_str).map_err(|error| {
                    McpExecutionError::execution_failed(format!(
                        "tool `{}` mapped header `{key}` has invalid value: {error}",
                        tool.name
                    ))
                })?;
                request_headers.insert(header_name, header_val);
            }
            if !cookie_params.is_empty() {
                let mut cookies = Vec::new();
                for (key, val) in cookie_params {
                    if val.is_null() {
                        continue;
                    }
                    validate_mapped_cookie_name(tool.name.as_str(), key.as_str())?;
                    let val_str = json_value_to_query(val);
                    let encoded_val = Self::percent_encode_path_segment(&val_str);
                    cookies.push(format!("{}={}", key, encoded_val));
                }
                if !cookies.is_empty() {
                    let cookie_header_val = cookies.join("; ");
                    let header_val =
                        HeaderValue::from_str(&cookie_header_val).map_err(|error| {
                            McpExecutionError::execution_failed(format!(
                                "tool `{}` mapped cookie header is invalid: {error}",
                                tool.name
                            ))
                        })?;
                    request_headers.insert(reqwest::header::COOKIE, header_val);
                }
            }
        }

        let final_body = if method.sends_json_body() {
            if has_mapping {
                if let Some(body) = body_val {
                    Some(body.clone())
                } else if let Some(mapping) = mapping {
                    let mut body_obj = serde_json::Map::new();
                    if let Some(args_obj) = arguments.as_object() {
                        for (key, val) in args_obj {
                            let mapped_loc =
                                parameter_mapping_location(mapping, key, tool.name.as_str())?;
                            if !matches!(
                                mapped_loc,
                                Some(
                                    ParameterLocation::Path
                                        | ParameterLocation::Query
                                        | ParameterLocation::Header
                                        | ParameterLocation::Cookie
                                        | ParameterLocation::Body
                                )
                            ) {
                                body_obj.insert(key.clone(), val.clone());
                            }
                        }
                    }
                    if !body_obj.is_empty() {
                        Some(JsonValue::Object(body_obj))
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                Some(arguments.clone())
            }
        } else {
            None
        };

        validate_target_host_resolution(tool, &url).await?;
        let retry_policy = tool_retry_policy(tool, arguments);
        let mut attempt = 1;

        loop {
            let mut request = self
                .client
                .request(request_method.clone(), url.clone())
                .headers(request_headers.clone());

            if let Some(body) = final_body.as_ref() {
                request = request.json(body);
            }

            let mut response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    if retry_policy
                        .as_ref()
                        .is_some_and(|policy| policy.should_retry_error(attempt, &error))
                    {
                        log_mcp_tool_retry(
                            tool,
                            attempt,
                            retry_policy
                                .as_ref()
                                .map_or(1, |policy| policy.max_attempts),
                            "transport",
                            error_chain(&error).as_str(),
                        );
                        if let Some(policy) = retry_policy.as_ref() {
                            sleep_retry_backoff(policy).await;
                        }
                        attempt += 1;
                        continue;
                    }
                    let detail = error_chain(&error);
                    tracing::warn!(
                        target: "light_pingora::mcp",
                        toolName = %tool.name,
                        method = %request_method,
                        url = %request_url,
                        error = %detail,
                        "mcp backend request failed"
                    );
                    return Err(McpExecutionError::execution_failed(detail));
                }
            };
            let status = response.status();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            if response
                .content_length()
                .is_some_and(|length| length > self.config.max_response_body_bytes as u64)
            {
                return Err(McpExecutionError::execution_failed(format!(
                    "tool `{}` response exceeds maxResponseBodyBytes",
                    tool.name
                )));
            }
            let mut body = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?
            {
                if chunk.len()
                    > self
                        .config
                        .max_response_body_bytes
                        .saturating_sub(body.len())
                {
                    return Err(McpExecutionError::execution_failed(format!(
                        "tool `{}` response exceeds maxResponseBodyBytes",
                        tool.name
                    )));
                }
                body.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                if retry_policy
                    .as_ref()
                    .is_some_and(|policy| policy.should_retry_status(attempt, status.as_u16()))
                {
                    let status_detail = status.as_u16().to_string();
                    log_mcp_tool_retry(
                        tool,
                        attempt,
                        retry_policy
                            .as_ref()
                            .map_or(1, |policy| policy.max_attempts),
                        "status",
                        status_detail.as_str(),
                    );
                    if let Some(policy) = retry_policy.as_ref() {
                        sleep_retry_backoff(policy).await;
                    }
                    attempt += 1;
                    continue;
                }
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
            return Ok(mcp_text_result(String::from_utf8_lossy(&body).to_string()));
        }
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
        profile: BackendProfileContext<'_>,
    ) -> Result<JsonValue, McpExecutionError> {
        if profile.frontend != FrontendProfile::Legacy
            || profile.backend != BackendProfile::LegacyStateful
        {
            return Err(McpExecutionError::execution_failed(
                "unsupported frontend/backend MCP profile combination",
            ));
        }
        let frontend_session_id = profile.frontend_session_id.ok_or_else(|| {
            McpExecutionError::execution_failed(
                "legacy backend sessions require a frontend session id",
            )
        })?;
        let method = effective_http_method(tool);
        if matches!(tool.method, McpHttpMethod::Call)
            && matches!(method, McpHttpMethod::Get | McpHttpMethod::Head)
        {
            return self
                .execute_http_tool_with_method(tool, arguments, agent_headers, method)
                .await;
        }

        let url = self.tool_target_url(tool).await?;
        validate_target_host_resolution(tool, &url).await?;
        let backend_session = self
            .ensure_backend_session(frontend_session_id, &url, agent_headers)
            .await?;
        let _requested_protocol_version = profile.protocol_version;
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
        let retry_policy = tool_retry_policy(tool, arguments);
        let mut attempt = 1;
        let (content_type, body) = loop {
            let response = match self
                .client
                .post(url.clone())
                .headers(backend_headers(
                    agent_headers,
                    backend_session.session_id.as_deref(),
                    Some(backend_session.protocol_version.as_str()),
                )?)
                .json(&request)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    if retry_policy
                        .as_ref()
                        .is_some_and(|policy| policy.should_retry_error(attempt, &error))
                    {
                        log_mcp_tool_retry(
                            tool,
                            attempt,
                            retry_policy
                                .as_ref()
                                .map_or(1, |policy| policy.max_attempts),
                            "transport",
                            error_chain(&error).as_str(),
                        );
                        if let Some(policy) = retry_policy.as_ref() {
                            sleep_retry_backoff(policy).await;
                        }
                        attempt += 1;
                        continue;
                    }
                    return Err(McpExecutionError::execution_failed(error.to_string()));
                }
            };
            let (status, content_type, _headers, body) = read_backend_mcp_response(
                response,
                "tools/call",
                &url_for_log,
                self.config.max_response_body_bytes,
            )
            .await?;
            if !status.is_success() {
                if retry_policy
                    .as_ref()
                    .is_some_and(|policy| policy.should_retry_status(attempt, status.as_u16()))
                {
                    let status_detail = status.as_u16().to_string();
                    log_mcp_tool_retry(
                        tool,
                        attempt,
                        retry_policy
                            .as_ref()
                            .map_or(1, |policy| policy.max_attempts),
                        "status",
                        status_detail.as_str(),
                    );
                    if let Some(policy) = retry_policy.as_ref() {
                        sleep_retry_backoff(policy).await;
                    }
                    attempt += 1;
                    continue;
                }
                return Err(McpExecutionError::execution_failed(format!(
                    "MCP tool `{}` returned HTTP {}: {}",
                    tool.name,
                    status.as_u16(),
                    String::from_utf8_lossy(&body)
                )));
            }
            break (content_type, body);
        };
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
        let (status, content_type, _headers, body) = read_backend_mcp_response(
            response,
            "initialize",
            target_url.as_str(),
            self.config.max_response_body_bytes,
        )
        .await?;
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
        let (status, _content_type, _headers, body) = read_backend_mcp_response(
            response,
            "notifications/initialized",
            &target_url_for_log,
            self.config.max_response_body_bytes,
        )
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
            return parse_base_url(
                target_host,
                &tool.name,
                tool_allows_private_target_host(tool),
            );
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
            return parse_base_url(matched.url.trim(), &tool.name, true);
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
        parse_base_url(discovery_node_base_url(node).as_str(), &tool.name, true)
    }

    fn get_tool(&self, requested_name: &str) -> Option<&PreparedMcpTool> {
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

fn mcp_tool_error_result(message: impl Into<String>) -> JsonValue {
    json!({
        "content": [{"type": "text", "text": message.into()}],
        "isError": true,
        "resultType": "complete"
    })
}

fn stateless_tool_result(mut result: JsonValue) -> JsonValue {
    let Some(result) = result.as_object_mut() else {
        return mcp_tool_error_result("Tool returned an invalid result envelope");
    };
    result.insert(
        "resultType".to_string(),
        JsonValue::String("complete".to_string()),
    );
    let meta = result
        .entry("_meta".to_string())
        .or_insert_with(|| JsonValue::Object(JsonMap::new()));
    if !meta.is_object() {
        *meta = JsonValue::Object(JsonMap::new());
    }
    meta.as_object_mut().expect("meta object").insert(
        SERVER_INFO_META_KEY.to_string(),
        json!({
            "name": "light-gateway",
            "version": env!("CARGO_PKG_VERSION")
        }),
    );
    JsonValue::Object(result.clone())
}

fn expected_parameter_headers(
    tool: &PreparedMcpTool,
    arguments: &JsonValue,
) -> Result<Vec<ExpectedParameterHeader>, McpExecutionError> {
    tool.header_extractions
        .iter()
        .map(|extraction| {
            let value = match value_at_property_path(arguments, &extraction.property_path)
                .filter(|value| !value.is_null())
            {
                None => None,
                Some(value) => Some(
                    match extraction.value_kind {
                        HeaderValueKind::String => value
                            .as_str()
                            .map(|value| ExpectedParameterValue::String(value.to_string())),
                        HeaderValueKind::Integer => {
                            value.as_i64().map(ExpectedParameterValue::Integer)
                        }
                        HeaderValueKind::Boolean => {
                            value.as_bool().map(ExpectedParameterValue::Boolean)
                        }
                    }
                    .ok_or_else(|| {
                        McpExecutionError::invalid_params(format!(
                            "tool `{}` header-mirrored argument has an invalid primitive value",
                            tool.name
                        ))
                    })?,
                ),
            };
            Ok(ExpectedParameterHeader {
                name: extraction.header_name.clone(),
                value,
            })
        })
        .collect()
}

fn value_at_property_path<'a>(value: &'a JsonValue, path: &[String]) -> Option<&'a JsonValue> {
    path.iter()
        .try_fold(value, |current, property| current.get(property))
}

fn stateless_target_key(tool: &McpToolConfig) -> String {
    if let Some(target) = tool
        .target_host
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return format!("target:{target}");
    }
    format!(
        "service:{}:{}",
        tool.service_id.as_deref().unwrap_or_default().trim(),
        tool.env_tag.as_deref().unwrap_or_default().trim()
    )
}

fn mcp_schema_error_result(prefix: &str, diagnostics: &[SchemaDiagnostic]) -> JsonValue {
    let details = diagnostics
        .iter()
        .map(|diagnostic| {
            let path = if diagnostic.path.is_empty() {
                "/"
            } else {
                diagnostic.path.as_str()
            };
            format!("{path}: {}", diagnostic.constraint)
        })
        .collect::<Vec<_>>()
        .join("; ");
    let message = if details.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}: {details}")
    };
    mcp_tool_error_result(message)
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

fn replace_structured_content(result: &mut JsonValue, value: JsonValue) {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    if let Some(object) = result.as_object_mut() {
        object.insert("structuredContent".to_string(), value);
    }
    if let Some(item) = result
        .get_mut("content")
        .and_then(JsonValue::as_array_mut)
        .and_then(|content| content.first_mut())
        .and_then(JsonValue::as_object_mut)
        .filter(|item| item.get("type").and_then(JsonValue::as_str) == Some("text"))
    {
        item.insert("text".to_string(), JsonValue::String(text));
    }
}

fn schema_declares_array_root(schema: &JsonValue) -> bool {
    match schema.get("type") {
        Some(JsonValue::String(kind)) => kind == "array",
        Some(JsonValue::Array(kinds)) => kinds.iter().any(|kind| kind.as_str() == Some("array")),
        _ => false,
    }
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

#[derive(Debug)]
struct LegacyInitializeMetadata {
    protocol_version: String,
    client_info: JsonValue,
    capabilities: JsonValue,
}

fn requested_protocol_version(message: &serde_json::Map<String, JsonValue>) -> Option<&str> {
    message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(JsonValue::as_str)
}

fn request_principal_binding(
    context: &McpRequestContext,
) -> Result<(String, String), McpSessionError> {
    if let Some(auth) = &context.auth {
        let has_stable_identity = [
            auth.client_id.as_deref(),
            auth.user_id.as_deref(),
            auth.email.as_deref(),
            auth.host.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|value| !value.trim().is_empty());
        let identity = json!({
            "issuer": normalized_optional(auth.issuer.as_deref()),
            "clientId": normalized_optional(auth.client_id.as_deref()),
            "userId": normalized_optional(auth.user_id.as_deref()),
            "email": normalized_optional(auth.email.as_deref()),
            "host": normalized_optional(auth.host.as_deref()),
            "tenant": normalized_claim(&auth.claims, &["tenant", "tenant_id", "tenantId", "tid"]),
            "product": normalized_claim(&auth.claims, &["product", "product_id", "productId"]),
            "environment": normalized_claim(&auth.claims, &["environment", "environment_id", "environmentId"]),
            "instance": normalized_claim(&auth.claims, &["instance", "instance_id", "instanceId"]),
        });
        if has_stable_identity {
            let binding = format!("auth:{}", stable_json_hash(&identity));
            return Ok((binding.clone(), binding));
        }
        return Err(McpSessionError {
            status: 403,
            code: -32001,
            message: "authenticated MCP request has no stable principal identity".to_string(),
        });
    }

    let binding = context
        .anonymous_binding
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| McpSessionError {
            status: 403,
            code: -32001,
            message: "anonymous MCP request has no trusted connection binding".to_string(),
        })?;
    let binding = format!(
        "anonymous:{}",
        Sha256::digest(binding.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    Ok((binding.clone(), binding))
}

fn normalized_optional(value: Option<&str>) -> JsonValue {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| JsonValue::String(value.to_string()))
        .unwrap_or(JsonValue::Null)
}

fn normalized_claim(claims: &JsonValue, names: &[&str]) -> JsonValue {
    names
        .iter()
        .find_map(|name| claims.get(name))
        .map(|value| match value {
            JsonValue::String(value) => JsonValue::String(value.trim().to_string()),
            value => value.clone(),
        })
        .unwrap_or(JsonValue::Null)
}

fn principal_binding_matches(stored: &str, current: &str) -> bool {
    stored == current
}

fn is_test_principal_wildcard(stored: &str) -> bool {
    #[cfg(test)]
    return stored == "test:any-principal";
    #[cfg(not(test))]
    {
        let _ = stored;
        false
    }
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
    for (name, value) in [
        ("maxRequestBodyBytes", config.max_request_body_bytes),
        ("maxResponseBodyBytes", config.max_response_body_bytes),
        ("maxJsonDepth", config.max_json_depth),
    ] {
        if value == 0 {
            return Err(RuntimeError::Unsupported(format!(
                "mcp-router.{name} must be greater than 0"
            )));
        }
    }
    if !config.protocols.legacy.enabled {
        return Err(RuntimeError::Unsupported(
            "mcp-router.protocols.legacy must remain enabled for the dual-profile milestone"
                .to_string(),
        ));
    }
    if config.protocols.stateless.enabled && config.protocols.stateless.versions.is_empty() {
        return Err(RuntimeError::Unsupported(
            "mcp-router.protocols.stateless.versions must not be empty when enabled".to_string(),
        ));
    }
    if config.protocols.legacy.versions.is_empty() {
        return Err(RuntimeError::Unsupported(
            "mcp-router.protocols.legacy.versions must not be empty".to_string(),
        ));
    }
    let mut versions = BTreeSet::new();
    for version in &config.protocols.legacy.versions {
        if !protocol_version_supported(version) {
            return Err(RuntimeError::Unsupported(format!(
                "unsupported legacy MCP protocol version `{version}`"
            )));
        }
        if !versions.insert(version.as_str()) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate legacy MCP protocol version `{version}`"
            )));
        }
    }
    let mut stateless_versions = BTreeSet::new();
    for version in &config.protocols.stateless.versions {
        if version != STATELESS_RC_VERSION {
            return Err(RuntimeError::Unsupported(format!(
                "unsupported stateless MCP protocol version `{version}`"
            )));
        }
        if !stateless_versions.insert(version.as_str()) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate stateless MCP protocol version `{version}`"
            )));
        }
    }
    for (name, value) in [
        ("discoverTtlMs", config.protocols.stateless.discover_ttl_ms),
        (
            "toolsListTtlMs",
            config.protocols.stateless.tools_list_ttl_ms,
        ),
    ] {
        if value == 0 {
            return Err(RuntimeError::Unsupported(format!(
                "mcp-router.protocols.stateless.{name} must be greater than 0"
            )));
        }
    }
    for (name, value) in [
        (
            "maxDiscoverCacheEntries",
            config.protocols.stateless.max_discover_cache_entries,
        ),
        (
            "maxToolsListCacheEntries",
            config.protocols.stateless.max_tools_list_cache_entries,
        ),
        (
            "maxToolsListItems",
            config.protocols.stateless.max_tools_list_items,
        ),
        (
            "maxConcurrentRequests",
            config.protocols.stateless.max_concurrent_requests,
        ),
        (
            "maxConcurrentRequestsPerPrincipal",
            config
                .protocols
                .stateless
                .max_concurrent_requests_per_principal,
        ),
        (
            "maxConcurrentBackendCallsPerTarget",
            config
                .protocols
                .stateless
                .max_concurrent_backend_calls_per_target,
        ),
    ] {
        if value == 0 {
            return Err(RuntimeError::Unsupported(format!(
                "mcp-router.protocols.stateless.{name} must be greater than 0"
            )));
        }
    }
    for origin in &config.origin_allowlist {
        normalize_mcp_origin(origin).map_err(|message| {
            RuntimeError::Unsupported(format!("invalid mcp-router.originAllowlist: {message}"))
        })?;
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
        let path_placeholders =
            openapi_path_placeholders(tool.path.as_str()).map_err(|message| {
                RuntimeError::Unsupported(format!(
                    "mcp-router tool `{name}` path `{}` is invalid: {message}",
                    tool.path
                ))
            })?;
        validate_path_placeholder_mapping(tool, tool_parameter_mapping(tool), &path_placeholders)
            .map_err(|error| RuntimeError::Unsupported(error.message))?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParameterLocation {
    Path,
    Query,
    Header,
    Cookie,
    Body,
}

impl ParameterLocation {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "path" => Some(Self::Path),
            "query" => Some(Self::Query),
            "header" => Some(Self::Header),
            "cookie" => Some(Self::Cookie),
            "body" => Some(Self::Body),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Query => "query",
            Self::Header => "header",
            Self::Cookie => "cookie",
            Self::Body => "body",
        }
    }
}

fn tool_routing_metadata(tool: &McpToolConfig) -> Option<&JsonMap<String, JsonValue>> {
    tool_metadata_section(tool, &["routing"])
}

fn tool_safety_metadata(tool: &McpToolConfig) -> Option<&JsonMap<String, JsonValue>> {
    tool_metadata_section(tool, &["safety"])
}

fn tool_runtime_metadata(tool: &McpToolConfig) -> Option<&JsonMap<String, JsonValue>> {
    tool_metadata_section(tool, &["runtime"])
}

fn tool_lifecycle_metadata(tool: &McpToolConfig) -> Option<&JsonMap<String, JsonValue>> {
    tool_metadata_section(tool, &["lifecycle"])
}

fn tool_metadata_section<'a>(
    tool: &'a McpToolConfig,
    keys: &[&str],
) -> Option<&'a JsonMap<String, JsonValue>> {
    let metadata = tool.tool_metadata.as_object()?;
    metadata_field(metadata, keys).and_then(JsonValue::as_object)
}

fn metadata_field<'a>(
    metadata: &'a JsonMap<String, JsonValue>,
    keys: &[&str],
) -> Option<&'a JsonValue> {
    keys.iter().find_map(|key| metadata.get(*key))
}

fn metadata_string<'a>(
    metadata: Option<&'a JsonMap<String, JsonValue>>,
    keys: &[&str],
) -> Option<&'a str> {
    metadata
        .and_then(|metadata| metadata_field(metadata, keys))
        .and_then(JsonValue::as_str)
}

fn metadata_bool(metadata: Option<&JsonMap<String, JsonValue>>, keys: &[&str]) -> Option<bool> {
    metadata
        .and_then(|metadata| metadata_field(metadata, keys))
        .and_then(JsonValue::as_bool)
}

fn metadata_u64(metadata: Option<&JsonMap<String, JsonValue>>, keys: &[&str]) -> Option<u64> {
    metadata
        .and_then(|metadata| metadata_field(metadata, keys))
        .and_then(|value| {
            value.as_u64().or_else(|| {
                value
                    .as_str()
                    .and_then(|raw| raw.trim().parse::<u64>().ok())
            })
        })
}

fn tool_retry_policy(tool: &McpToolConfig, arguments: &JsonValue) -> Option<ToolRetryPolicy> {
    let runtime = tool_runtime_metadata(tool)?;
    let retry = metadata_field(runtime, &["retry", "retryPolicy", "retry_policy"])
        .and_then(JsonValue::as_object)?;
    if !metadata_bool(Some(retry), &["enabled"]).unwrap_or(false) {
        return None;
    }
    if !tool_metadata_bool(tool, &["idempotent"]).unwrap_or(false) {
        tracing::warn!(
            target: "light_pingora::mcp",
            toolName = %tool.name,
            "runtime retry ignored because safety.idempotent is not true"
        );
        return None;
    }
    if tool_metadata_bool(tool, &["destructive"]).unwrap_or(false)
        && !arguments_include_idempotency_key(arguments)
    {
        tracing::warn!(
            target: "light_pingora::mcp",
            toolName = %tool.name,
            "runtime retry ignored because destructive tool arguments do not include an idempotency key"
        );
        return None;
    }

    let max_attempts = metadata_u64(Some(retry), &["maxAttempts", "max_attempts"])
        .and_then(|value| usize::try_from(value).ok())
        .map(|value| value.min(MAX_TOOL_RETRY_ATTEMPTS))
        .unwrap_or(1);
    if max_attempts < 2 {
        tracing::warn!(
            target: "light_pingora::mcp",
            toolName = %tool.name,
            "runtime retry ignored because maxAttempts is missing or less than 2"
        );
        return None;
    }

    let status_codes = retry_status_codes(retry);
    let retry_on_timeout = retry_event_enabled(retry, "timeout")
        || metadata_bool(
            Some(retry),
            &["retryOnTimeout", "retry_on_timeout", "onTimeout"],
        )
        .unwrap_or(false);
    let retry_on_connect = retry_event_enabled(retry, "connect")
        || retry_event_enabled(retry, "connection")
        || metadata_bool(
            Some(retry),
            &["retryOnConnect", "retry_on_connect", "onConnect"],
        )
        .unwrap_or(false);
    if status_codes.is_empty() && !retry_on_timeout && !retry_on_connect {
        tracing::warn!(
            target: "light_pingora::mcp",
            toolName = %tool.name,
            "runtime retry ignored because no supported retryStatusCodes or retryOn events are configured"
        );
        return None;
    }

    let backoff = metadata_u64(
        Some(retry),
        &["backoffMs", "backoffMillis", "delayMs", "backoff_ms"],
    )
    .map(Duration::from_millis)
    .unwrap_or(Duration::ZERO)
    .min(MAX_TOOL_RETRY_BACKOFF);

    Some(ToolRetryPolicy {
        max_attempts,
        status_codes,
        retry_on_timeout,
        retry_on_connect,
        backoff,
    })
}

fn tool_metadata_bool(tool: &McpToolConfig, keys: &[&str]) -> Option<bool> {
    metadata_bool(tool_safety_metadata(tool), keys)
        .or_else(|| metadata_bool(tool.tool_metadata.as_object(), keys))
}

fn retry_status_codes(retry: &JsonMap<String, JsonValue>) -> BTreeSet<u16> {
    metadata_field(
        retry,
        &[
            "retryStatusCodes",
            "retry_status_codes",
            "statusCodes",
            "status_codes",
        ],
    )
    .map(status_code_values)
    .unwrap_or_default()
    .into_iter()
    .filter(|status| RETRYABLE_STATUS_CODES.contains(status))
    .collect()
}

fn status_code_values(value: &JsonValue) -> BTreeSet<u16> {
    match value {
        JsonValue::Array(values) => values.iter().filter_map(status_code_value).collect(),
        JsonValue::String(raw) => raw
            .split(',')
            .filter_map(|part| part.trim().parse::<u16>().ok())
            .collect(),
        _ => status_code_value(value).into_iter().collect(),
    }
}

fn status_code_value(value: &JsonValue) -> Option<u16> {
    value
        .as_u64()
        .and_then(|number| u16::try_from(number).ok())
        .or_else(|| {
            value
                .as_str()
                .and_then(|raw| raw.trim().parse::<u16>().ok())
        })
}

fn retry_event_enabled(retry: &JsonMap<String, JsonValue>, event: &str) -> bool {
    let Some(value) = metadata_field(retry, &["retryOn", "retry_on", "events"]) else {
        return false;
    };
    match value {
        JsonValue::Array(values) => values.iter().any(|value| retry_event_matches(value, event)),
        JsonValue::String(_) => retry_event_matches(value, event),
        _ => false,
    }
}

fn retry_event_matches(value: &JsonValue, event: &str) -> bool {
    value.as_str().is_some_and(|raw| {
        raw.split(',')
            .any(|part| part.trim().eq_ignore_ascii_case(event))
    })
}

fn arguments_include_idempotency_key(arguments: &JsonValue) -> bool {
    arguments
        .as_object()
        .is_some_and(|arguments| object_contains_idempotency_key(arguments))
}

fn object_contains_idempotency_key(arguments: &JsonMap<String, JsonValue>) -> bool {
    arguments.iter().any(|(key, value)| {
        is_idempotency_key_name(key)
            || value
                .as_object()
                .is_some_and(object_contains_idempotency_key)
    })
}

fn is_idempotency_key_name(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    normalized == "idempotencykey"
}

async fn sleep_retry_backoff(policy: &ToolRetryPolicy) {
    if !policy.backoff.is_zero() {
        tokio::time::sleep(policy.backoff).await;
    }
}

fn log_mcp_tool_retry(
    tool: &McpToolConfig,
    attempt: usize,
    max_attempts: usize,
    reason: &str,
    detail: &str,
) {
    tracing::warn!(
        target: "light_pingora::mcp",
        toolName = %tool.name,
        attempt,
        maxAttempts = max_attempts,
        retryReason = reason,
        detail = %detail,
        "retrying mcp tool call"
    );
}

fn tool_parameter_mapping(tool: &McpToolConfig) -> Option<&JsonMap<String, JsonValue>> {
    tool_routing_metadata(tool)
        .and_then(|routing| {
            metadata_field(
                routing,
                &["parameters", "parameterMapping", "parameter_mapping"],
            )
        })
        .and_then(JsonValue::as_object)
}

fn tool_endpoint_id(tool: &McpToolConfig) -> Option<&str> {
    tool.tool_metadata
        .as_object()
        .and_then(|metadata| metadata_string(Some(metadata), &["endpointId", "endpoint_id"]))
        .or_else(|| {
            tool_routing_metadata(tool)
                .and_then(|routing| metadata_string(Some(routing), &["endpointId", "endpoint_id"]))
        })
}

fn tool_allows_private_target_host(tool: &McpToolConfig) -> bool {
    metadata_bool(
        tool_runtime_metadata(tool),
        &["allowPrivateTargetHost", "allow_private_target_host"],
    )
    .or_else(|| {
        metadata_bool(
            tool.tool_metadata.as_object(),
            &["allowPrivateTargetHost", "allow_private_target_host"],
        )
    })
    .unwrap_or(false)
}

fn parameter_mapping_location(
    mapping: &JsonMap<String, JsonValue>,
    parameter: &str,
    tool_name: &str,
) -> Result<Option<ParameterLocation>, McpExecutionError> {
    let Some(value) = mapping.get(parameter) else {
        return Ok(None);
    };
    let Some(raw_location) = value.as_str() else {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` parameter mapping `{parameter}` must be a string"
        )));
    };
    let Some(location) = ParameterLocation::parse(raw_location) else {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` parameter mapping `{parameter}` has unsupported location `{raw_location}`"
        )));
    };
    Ok(Some(location))
}

fn validate_path_placeholder_mapping(
    tool: &McpToolConfig,
    mapping: Option<&JsonMap<String, JsonValue>>,
    placeholders: &[String],
) -> Result<(), McpExecutionError> {
    if placeholders.is_empty() {
        return Ok(());
    }
    let Some(mapping) = mapping else {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{}` path `{}` has placeholders but no toolMetadata.routing.parameters mapping",
            tool.name, tool.path
        )));
    };
    for placeholder in placeholders {
        match parameter_mapping_location(mapping, placeholder, tool.name.as_str())? {
            Some(ParameterLocation::Path) => {}
            Some(location) => {
                return Err(McpExecutionError::execution_failed(format!(
                    "tool `{}` path placeholder `{placeholder}` must map to `path`, found `{}`",
                    tool.name,
                    location.as_str()
                )));
            }
            None => {
                return Err(McpExecutionError::execution_failed(format!(
                    "tool `{}` path placeholder `{placeholder}` requires toolMetadata.routing.parameters.{placeholder}=path",
                    tool.name
                )));
            }
        }
    }
    Ok(())
}

fn openapi_path_placeholders(path: &str) -> Result<Vec<String>, String> {
    let regex = Regex::new(r"\{([A-Za-z0-9_.-]+)\}").expect("valid path placeholder regex");
    let mut ranges = Vec::new();
    let mut names = BTreeSet::new();
    for capture in regex.captures_iter(path) {
        if let Some(full_match) = capture.get(0) {
            ranges.push(full_match.start()..full_match.end());
        }
        if let Some(name) = capture.get(1) {
            names.insert(name.as_str().to_string());
        }
    }
    for (index, byte) in path.bytes().enumerate() {
        if matches!(byte, b'{' | b'}') && !ranges.iter().any(|range| range.contains(&index)) {
            return Err(
                "only OpenAPI `{name}` placeholders with letters, numbers, `_`, `.`, or `-` are supported"
                    .to_string(),
            );
        }
    }
    Ok(names.into_iter().collect())
}

fn mapped_header_name(tool_name: &str, header: &str) -> Result<HeaderName, McpExecutionError> {
    let normalized = header.to_ascii_lowercase();
    if should_regenerate_header(header)
        || matches!(
            normalized.as_str(),
            "authorization" | "cookie" | "set-cookie" | "www-authenticate"
        )
    {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` cannot map argument to protected header `{header}`"
        )));
    }
    HeaderName::from_bytes(header.as_bytes()).map_err(|error| {
        McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` mapped header `{header}` is invalid: {error}"
        ))
    })
}

fn validate_mapped_cookie_name(tool_name: &str, cookie: &str) -> Result<(), McpExecutionError> {
    if cookie.starts_with('$') || !is_http_token(cookie) {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` mapped cookie name `{cookie}` is invalid"
        )));
    }
    Ok(())
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            matches!(
                byte,
                b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
        })
}

fn tool_matches_tools_list_query(tool: &McpToolConfig, query: &str) -> bool {
    contains_query(tool.name.as_str(), query)
        || contains_query(tool.description.as_str(), query)
        || tool_endpoint_id(tool).is_some_and(|endpoint_id| contains_query(endpoint_id, query))
        || tool_routing_metadata(tool).is_some_and(|routing| routing_matches_query(routing, query))
        || tool_safety_metadata(tool).is_some_and(|safety| metadata_matches_query(safety, query))
        || tool_lifecycle_metadata(tool)
            .is_some_and(|lifecycle| metadata_matches_query(lifecycle, query))
}

fn routing_matches_query(routing: &JsonMap<String, JsonValue>, query: &str) -> bool {
    metadata_any_string_matches(
        routing,
        &[
            "domain",
            "semanticNamespace",
            "semantic_namespace",
            "semanticDescription",
            "semantic_description",
            "sourceProtocol",
            "source_protocol",
            "sensitivityTier",
            "sensitivity_tier",
        ],
        query,
    ) || metadata_field(routing, &["semanticKeywords", "semantic_keywords"])
        .and_then(JsonValue::as_array)
        .is_some_and(|keywords| {
            keywords
                .iter()
                .filter_map(JsonValue::as_str)
                .any(|keyword| contains_query(keyword, query))
        })
}

fn metadata_any_string_matches(
    metadata: &JsonMap<String, JsonValue>,
    keys: &[&str],
    query: &str,
) -> bool {
    keys.iter()
        .filter_map(|key| metadata.get(*key).and_then(JsonValue::as_str))
        .any(|value| contains_query(value, query))
}

fn metadata_matches_query(metadata: &JsonMap<String, JsonValue>, query: &str) -> bool {
    metadata.values().any(|value| match value {
        JsonValue::String(value) => contains_query(value, query),
        JsonValue::Bool(value) => value.to_string().contains(query),
        JsonValue::Number(value) => value.to_string().contains(query),
        JsonValue::Array(values) => values.iter().any(|value| match value {
            JsonValue::String(value) => contains_query(value, query),
            _ => false,
        }),
        JsonValue::Object(_) | JsonValue::Null => false,
    })
}

fn contains_query(value: &str, query: &str) -> bool {
    value.to_ascii_lowercase().contains(query)
}

fn parse_base_url(
    base: &str,
    tool_name: &str,
    allow_private_host: bool,
) -> Result<Url, McpExecutionError> {
    let base = base.trim();
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
    if !url.username().is_empty() || url.password().is_some() {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` target `{base}` must not include userinfo"
        )));
    }
    if url.host_str().is_none() {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{tool_name}` target `{base}` must include a host"
        )));
    }
    if !allow_private_host {
        validate_public_target_host(&url, tool_name, base)?;
    }
    Ok(url)
}

fn validate_public_target_host(
    url: &Url,
    tool_name: &str,
    base: &str,
) -> Result<(), McpExecutionError> {
    if let Some(host) = url.host() {
        match host {
            url::Host::Ipv4(address) => {
                if is_blocked_target_ip(IpAddr::V4(address)) {
                    return Err(blocked_target_host_error(tool_name, base));
                }
            }
            url::Host::Ipv6(address) => {
                if is_blocked_target_ip(IpAddr::V6(address)) {
                    return Err(blocked_target_host_error(tool_name, base));
                }
            }
            url::Host::Domain(domain) => {
                let normalized = domain.trim_end_matches('.').to_ascii_lowercase();
                if matches!(
                    normalized.as_str(),
                    "localhost" | "metadata.google.internal"
                ) || normalized.ends_with(".localhost")
                {
                    return Err(blocked_target_host_error(tool_name, base));
                }
            }
        }
    }
    Ok(())
}

async fn validate_target_host_resolution(
    tool: &McpToolConfig,
    url: &Url,
) -> Result<(), McpExecutionError> {
    let Some(configured_target) = tool
        .target_host
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(());
    };
    if tool_allows_private_target_host(tool) {
        return Ok(());
    }
    if !matches!(url.host(), Some(url::Host::Domain(_))) {
        return Ok(());
    }
    let host = url.host_str().ok_or_else(|| {
        McpExecutionError::execution_failed(format!(
            "tool `{}` targetHost `{configured_target}` must include a host",
            tool.name
        ))
    })?;
    let port = url.port_or_known_default().ok_or_else(|| {
        McpExecutionError::execution_failed(format!(
            "tool `{}` targetHost `{configured_target}` must include a port or use a known scheme",
            tool.name
        ))
    })?;
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| {
            McpExecutionError::execution_failed(format!(
                "tool `{}` targetHost `{configured_target}` DNS lookup failed: {error}",
                tool.name
            ))
        })?;
    let mut checked = false;
    for addr in addrs {
        checked = true;
        if is_blocked_target_ip(addr.ip()) {
            return Err(blocked_target_host_error(&tool.name, configured_target));
        }
    }
    if !checked {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{}` targetHost `{configured_target}` DNS lookup returned no addresses",
            tool.name
        )));
    }
    Ok(())
}

fn blocked_target_host_error(tool_name: &str, base: &str) -> McpExecutionError {
    McpExecutionError::execution_failed(format!(
        "tool `{tool_name}` targetHost `{base}` resolves to a loopback, private, link-local, or metadata host; set toolMetadata.runtime.allowPrivateTargetHost=true only for approved internal targets"
    ))
}

fn is_blocked_target_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || octets == [169, 254, 169, 254]
                || octets[0] == 0
                || octets[0] >= 224
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        }
        IpAddr::V6(address) => {
            let segment = address.segments()[0];
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segment & 0xfe00) == 0xfc00
                || (segment & 0xffc0) == 0xfe80
        }
    }
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
            | "authorization"
            | "cookie"
            | "accept-encoding"
            | MCP_SESSION_ID_HEADER
            | MCP_PROTOCOL_VERSION_HEADER
            | "mcp-method"
            | "mcp-name"
    ) || name.starts_with("mcp-param-")
}

fn is_cache_identity_header(name: &str) -> bool {
    matches!(
        name,
        "accept-language"
            | "traceparent"
            | "tracestate"
            | "x-correlation-id"
            | "x-tenant-id"
            | "x-user-id"
            | "x-host-id"
    )
}

fn is_cache_ignored_header(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "content-type"
            | "content-length"
            | "host"
            | "origin"
            | "user-agent"
            | "authorization"
            | "cookie"
            | "x-csrf-token"
            | MCP_SESSION_ID_HEADER
            | MCP_PROTOCOL_VERSION_HEADER
    ) || should_regenerate_header(name)
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

fn all_headers<'a>(headers: &'a [(String, String)], name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .collect()
}

fn protocol_version_supported(version: &str) -> bool {
    matches!(
        version,
        "2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05"
    )
}

fn normalize_mcp_origin(raw: &str) -> Result<String, String> {
    let parsed = Url::parse(raw.trim()).map_err(|error| error.to_string())?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path() != "/"
    {
        return Err("origin must be an absolute http(s) origin without credentials, path, query, or fragment".to_string());
    }
    let host = parsed.host_str().unwrap_or_default().to_ascii_lowercase();
    let port = parsed
        .port()
        .map(|port| format!(":{port}"))
        .unwrap_or_default();
    Ok(format!("{}://{host}{port}", parsed.scheme()))
}

fn json_value_depth(value: &JsonValue) -> usize {
    match value {
        JsonValue::Array(values) => {
            1 + values
                .iter()
                .map(json_value_depth)
                .max()
                .unwrap_or_default()
        }
        JsonValue::Object(values) => {
            1 + values
                .values()
                .map(json_value_depth)
                .max()
                .unwrap_or_default()
        }
        _ => 1,
    }
}

fn classification_rejection_response(
    response_mode: McpResponseMode,
    id: JsonValue,
    rejection: ClassificationRejection,
) -> Result<McpHttpResponse, RuntimeError> {
    match rejection {
        ClassificationRejection::MultipleProtocolVersionHeaders => rpc_error_response(
            response_mode,
            400,
            id,
            -32600,
            "multiple MCP protocol version headers are not allowed",
        ),
        ClassificationRejection::ProtocolVersionMismatch => rpc_error_response(
            response_mode,
            400,
            id,
            -32600,
            "MCP protocol version header does not match request metadata",
        ),
        ClassificationRejection::UnsupportedProtocolVersion(version) => rpc_error_response(
            response_mode,
            400,
            id,
            -32600,
            format!("unsupported MCP protocol version `{version}`"),
        ),
        ClassificationRejection::InvalidJsonRpcRequest => {
            rpc_error_response(response_mode, 400, id, -32600, "invalid JSON-RPC request")
        }
        ClassificationRejection::MissingLegacySession => {
            rpc_error_response(response_mode, 400, id, -32600, "missing MCP session id")
        }
        ClassificationRejection::MethodNotAllowed => Ok(method_not_allowed_response()),
    }
}

fn classification_rejection_response_for_post(
    response_mode: McpResponseMode,
    id: JsonValue,
    rejection: ClassificationRejection,
    claims_stateless: bool,
    supported_stateless_versions: &[String],
) -> Result<McpHttpResponse, RuntimeError> {
    if claims_stateless {
        return match rejection {
            ClassificationRejection::MultipleProtocolVersionHeaders
            | ClassificationRejection::ProtocolVersionMismatch => rpc_error_response(
                response_mode,
                400,
                id,
                -32020,
                "MCP protocol version header does not match request metadata",
            ),
            ClassificationRejection::UnsupportedProtocolVersion(version) => rpc_error_response(
                response_mode,
                400,
                id,
                -32022,
                format!(
                    "unsupported MCP protocol version `{version}`; supported stateless versions: {}",
                    supported_stateless_versions.join(", ")
                ),
            ),
            other => classification_rejection_response(response_mode, id, other),
        };
    }
    classification_rejection_response(response_mode, id, rejection)
}

fn stateless_error_response(
    id: JsonValue,
    error: StatelessRequestError,
    version: &str,
) -> Result<McpHttpResponse, RuntimeError> {
    response_with_protocol_version(
        json_error_response(error.status, id, error.code, error.message),
        Some(version),
    )
}

fn stateless_catalog_limit_error() -> McpSessionError {
    McpSessionError {
        status: 400,
        code: -32000,
        message: "visible catalog exceeds a gateway limit and pagination is not supported"
            .to_string(),
    }
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

#[derive(Serialize)]
struct BorrowedJsonRpcResult<'a> {
    jsonrpc: &'static str,
    id: &'a JsonValue,
    result: &'a JsonValue,
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    overflowed: bool,
}

impl BoundedJsonWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(16 * 1024)),
            max_bytes,
            overflowed: false,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.overflowed = true;
            return Err(io::Error::other("bounded JSON response exceeded limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn bounded_json_result_response(
    status: u16,
    id: &JsonValue,
    result: &JsonValue,
    max_bytes: usize,
) -> Result<Option<McpHttpResponse>, RuntimeError> {
    let mut writer = BoundedJsonWriter::new(max_bytes);
    let serialization = serde_json::to_writer(
        &mut writer,
        &BorrowedJsonRpcResult {
            jsonrpc: "2.0",
            id,
            result,
        },
    );
    if writer.overflowed {
        return Ok(None);
    }
    serialization?;
    Ok(Some(McpHttpResponse {
        status,
        content_type: JSON_CONTENT_TYPE.to_string(),
        headers: protocol_headers(),
        body: writer.bytes,
        streamed: false,
    }))
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
    mut response: reqwest::Response,
    operation: &str,
    url: &str,
    max_response_body_bytes: usize,
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

    if response
        .content_length()
        .is_some_and(|length| length > max_response_body_bytes as u64)
    {
        return Err(McpExecutionError::execution_failed(format!(
            "backend MCP {operation} response exceeds maxResponseBodyBytes"
        )));
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() > max_response_body_bytes.saturating_sub(body.len()) => {
                return Err(McpExecutionError::execution_failed(format!(
                    "backend MCP {operation} response exceeds maxResponseBodyBytes"
                )));
            }
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            Ok(None) => {
                tracing::debug!(
                    target: "light_pingora::mcp",
                    operation = %operation,
                    url = %url,
                    status = %status,
                    headers = ?headers,
                    body_len = body.len(),
                    body = %String::from_utf8_lossy(body.as_slice()),
                    "received backend MCP response body"
                );
                return Ok((status, content_type, headers, body));
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
                return Err(McpExecutionError::execution_failed(format!(
                    "backend MCP {operation} response read failed: {error}"
                )));
            }
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

fn default_max_request_body_bytes() -> usize {
    DEFAULT_MCP_MAX_REQUEST_BODY_BYTES
}

fn default_max_response_body_bytes() -> usize {
    DEFAULT_MCP_MAX_RESPONSE_BODY_BYTES
}

fn default_max_json_depth() -> usize {
    DEFAULT_MCP_MAX_JSON_DEPTH
}

fn default_legacy_protocol_versions() -> Vec<String> {
    ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn default_stateless_protocol_versions() -> Vec<String> {
    Vec::new()
}

fn default_stateless_discover_ttl_ms() -> u64 {
    30_000
}

fn default_stateless_tools_list_ttl_ms() -> u64 {
    30_000
}

fn default_stateless_discover_cache_entries() -> usize {
    1_024
}

fn default_stateless_tools_list_cache_entries() -> usize {
    4_096
}

fn default_stateless_tools_list_items() -> usize {
    1_024
}

fn default_stateless_concurrent_requests() -> usize {
    1_024
}

fn default_stateless_concurrent_requests_per_principal() -> usize {
    32
}

fn default_stateless_concurrent_backend_calls_per_target() -> usize {
    32
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

    #[tokio::test]
    async fn invalid_json_rpc_returns_invalid_request() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), session_header(&runtime)],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"unknown/method"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn delete_rejects_unknown_frontend_session() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "DELETE".to_string(),
                path: "/mcp".to_string(),
                headers: vec![(MCP_SESSION_ID_HEADER.to_string(), "unknown".to_string())],
                body: Vec::new(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn initialize_2025_11_25_stores_bounded_metadata_and_list_is_stable() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","clientInfo":{"name":"phase1","version":"1"},"capabilities":{"roots":{}}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(
            body["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        let session_id = first_header(&response.headers, MCP_SESSION_ID_HEADER).expect("session");
        let sessions = runtime.sessions.lock().await;
        let session = sessions.get(&session_id).expect("stored session");
        assert_eq!(session.client_info["name"], "phase1");
        assert!(session.client_capabilities["roots"].is_object());
    }

    #[tokio::test]
    async fn initialize_2025_11_25_rejects_missing_metadata() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn browser_origin_is_exact_and_checked_before_payload_parsing() {
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            origin_allowlist: vec!["https://portal.example.com".to_string()],
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    (
                        "origin".to_string(),
                        "https://portal.example.com.evil".to_string(),
                    ),
                ],
                body: b"not-json".to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 403);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32001);
    }

    #[tokio::test]
    async fn empty_origin_allowlist_rejects_browser_but_allows_non_browser() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        for origin in ["https://portal.example.com", "not an origin"] {
            let response = runtime
                .handle_request(McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![accept_json(), ("origin".to_string(), origin.to_string())],
                    body: b"not-json".to_vec(),
                })
                .await
                .expect("handle")
                .expect("response");
            assert_eq!(response.status, 403, "{origin}");
        }
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: b"not-json".to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
    }

    #[tokio::test]
    async fn oversized_request_is_rejected_atomically() {
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            max_request_body_bytes: 8,
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: vec![b'x'; 9],
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 413);
    }

    #[tokio::test]
    async fn misleading_content_length_is_not_used_as_body_limit_enforcement() {
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            max_request_body_bytes: 512,
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    ("content-length".to_string(), "999999".to_string()),
                ],
                body: br#"{"jsonrpc":"2.0","id":1}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
    }

    #[test]
    fn outbound_header_denylist_strips_mcp_routing_headers() {
        for header in [
            "Mcp-Session-Id",
            "MCP-Protocol-Version",
            "Mcp-Method",
            "Mcp-Name",
            "Mcp-Param-Host",
            "Authorization",
            "Cookie",
            "Proxy-Authorization",
        ] {
            assert!(should_regenerate_header(header), "header {header}");
        }
        assert!(!should_regenerate_header("x-correlation-id"));
    }

    #[tokio::test]
    async fn session_rejects_post_and_delete_from_different_principal() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let principal = |user: &str| McpRequestContext {
            auth: Some(AuthPrincipal {
                issuer: Some("https://issuer.example".to_string()),
                user_id: Some(user.to_string()),
                ..AuthPrincipal::default()
            }),
            ..McpRequestContext::default()
        };
        let initialized = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![accept_json()],
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_vec(),
                },
                principal("alice"),
            )
            .await
            .expect("handle")
            .expect("response");
        let session_id =
            first_header(&initialized.headers, MCP_SESSION_ID_HEADER).expect("session");
        for method in ["POST", "DELETE"] {
            let response = runtime
                .handle_request_with_context(
                    McpHttpRequest {
                        method: method.to_string(),
                        path: "/mcp".to_string(),
                        headers: vec![
                            accept_json(),
                            (MCP_SESSION_ID_HEADER.to_string(), session_id.clone()),
                        ],
                        body: if method == "POST" {
                            br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec()
                        } else {
                            Vec::new()
                        },
                    },
                    principal("mallory"),
                )
                .await
                .expect("handle")
                .expect("response");
            assert_eq!(response.status, 403, "{method}");
        }
        assert!(runtime.sessions.lock().await.contains_key(&session_id));
    }

    #[tokio::test]
    async fn anonymous_session_binding_isolated_and_missing_binding_fails_closed() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let anonymous = |binding: Option<&str>| McpRequestContext {
            anonymous_binding: binding.map(str::to_string),
            ..McpRequestContext::default()
        };
        let initialized = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![accept_json()],
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#.to_vec(),
                },
                anonymous(Some("peer:192.0.2.1")),
            )
            .await
            .expect("handle")
            .expect("response");
        let session_id =
            first_header(&initialized.headers, MCP_SESSION_ID_HEADER).expect("session");
        for context in [anonymous(Some("peer:192.0.2.2")), anonymous(None)] {
            let response = runtime
                .handle_request_with_context(
                    McpHttpRequest {
                        method: "POST".to_string(),
                        path: "/mcp".to_string(),
                        headers: vec![
                            accept_json(),
                            (MCP_SESSION_ID_HEADER.to_string(), session_id.clone()),
                        ],
                        body: br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec(),
                    },
                    context,
                )
                .await
                .expect("handle")
                .expect("response");
            assert_eq!(response.status, 403);
        }
    }

    #[tokio::test]
    async fn unsupported_initialize_version_does_not_fall_back() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        assert!(first_header(&response.headers, MCP_SESSION_ID_HEADER).is_none());
    }

    #[test]
    fn legacy_flat_config_defaults_to_legacy_only() {
        let config = serde_yaml::from_str::<McpRouterConfig>(
            "enabled: true\npath: /mcp\nmaxSessions: 5\nmaxSessionsPerClient: 2\ntools: []\n",
        )
        .expect("legacy config");
        assert!(config.protocols.legacy.enabled);
        assert!(
            config
                .protocols
                .legacy
                .versions
                .iter()
                .any(|v| v == "2025-11-25")
        );
        assert!(!config.protocols.stateless.enabled);
        assert!(config.protocols.stateless.versions.is_empty());
    }

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
    fn runtime_rejects_unmapped_openapi_path_placeholders() {
        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
tools:
  - name: get_pet
    targetHost: http://127.0.0.1:8080
    path: /pets/{petId}
    method: get
    inputSchema:
      type: object
      properties:
        petId:
          type: string
"#,
        )
        .expect("parse config");

        let error = McpRouterRuntime::new(config).expect_err("missing path mapping");

        assert!(
            error
                .to_string()
                .contains("path `/pets/{petId}` has placeholders")
        );
        assert!(
            error
                .to_string()
                .contains("no toolMetadata.routing.parameters mapping")
        );
    }

    #[test]
    fn runtime_rejects_non_path_mapping_for_openapi_placeholder() {
        let config = serde_yaml::from_str::<McpRouterConfig>(
            r#"
tools:
  - name: get_pet
    targetHost: http://127.0.0.1:8080
    path: /pets/{petId}
    method: get
    inputSchema:
      type: object
      properties:
        petId:
          type: string
    toolMetadata:
      routing:
        parameters:
          petId: query
"#,
        )
        .expect("parse config");

        let error = McpRouterRuntime::new(config).expect_err("incorrect path mapping");

        assert!(
            error
                .to_string()
                .contains("path placeholder `petId` must map to `path`, found `query`")
        );
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
        let client_key = request_principal_binding(&McpRequestContext {
            anonymous_binding: Some("in-process-test-client".to_string()),
            ..McpRequestContext::default()
        })
        .expect("test principal")
        .1;
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
    async fn tools_list_supports_metadata_query_filter() {
        let mut tool = test_tool(
            "demo_offer_decision_api_search_offers",
            "Search active offers",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.tool_metadata = json!({
            "routing": {
                "domain": "Offers",
                "semanticNamespace": "API0005",
                "semanticKeywords": ["offers", "Search active offers"],
                "sourceProtocol": "openapi"
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
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list","params":{"query":"api0005"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(
            body["result"]["tools"][0]["name"],
            "demo_offer_decision_api_search_offers"
        );
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
                    delegation: None,
                    anonymous_binding: None,
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
                    delegation: None,
                    anonymous_binding: None,
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
        assert!(!request.contains("authorization: Bearer abc"));
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
                    output_schema: None,
                    input_schema_configured: true,
                    tool_metadata: allow_private_target_metadata(),
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
                    output_schema: None,
                    input_schema_configured: true,
                    tool_metadata: allow_private_target_metadata(),
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
                    output_schema: None,
                    input_schema_configured: true,
                    tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
        assert!(!requests[2].contains("authorization: Bearer abc"));
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: allow_private_target_metadata(),
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
        assert!(!requests[3].contains("authorization: Bearer abc"));
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
            request_principal_binding(&McpRequestContext {
                anonymous_binding: Some("in-process-test-client".to_string()),
                ..McpRequestContext::default()
            })
            .expect("test principal")
            .1,
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
                output_schema: None,
                input_schema_configured: false,
                tool_metadata: allow_private_target_metadata(),
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
                    delegation: None,
                    anonymous_binding: None,
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
    async fn legacy_and_stateless_http_calls_share_policy_and_masking_core() {
        let (base, received) = spawn_http_sequence_server(vec![
            http_json_response(json!({"ok": true})),
            http_json_response(json!({"ok": true})),
        ])
        .await;
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
        let mut config = stateless_test_config(vec![test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Post,
            Some("weather@call"),
            json!({
                "type":"object",
                "properties":{
                    "ssn":{"type":"string","x-mask":true,"x-mask-pattern":"^(.*)$"},
                    "city":{"type":"string"}
                }
            }),
        )]);
        config.protocols.stateless.enabled = true;
        let runtime = McpRouterRuntime::new_with_policy(config, Some(policy)).expect("runtime");
        let context = McpRequestContext {
            auth: Some(AuthPrincipal {
                user_id: Some("alice".into()),
                role: Some("mcp-reader".into()),
                claims: json!({"role":"mcp-reader"}),
                ..AuthPrincipal::default()
            }),
            correlation_id: Some("corr-equivalence".into()),
            ..McpRequestContext::default()
        };
        let arguments = json!({"ssn":"123-45-6789","city":"Toronto"});
        let legacy = runtime
            .handle_request_with_context(
                McpHttpRequest {
                    method: "POST".into(),
                    path: "/mcp".into(),
                    headers: accept_json_with_session(&runtime),
                    body: serde_json::to_vec(&json!({
                        "jsonrpc":"2.0","id":1,"method":"tools/call",
                        "params":{"name":"weather","arguments":arguments}
                    }))
                    .expect("body"),
                },
                context.clone(),
            )
            .await
            .expect("handle")
            .expect("response");
        let modern = runtime
            .handle_request_with_context(
                stateless_request(
                    "tools/call",
                    json!({"name":"weather","arguments":arguments}),
                    None,
                ),
                context,
            )
            .await
            .expect("handle")
            .expect("response");
        let legacy = serde_json::from_slice::<JsonValue>(&legacy.body).expect("legacy json");
        let modern = serde_json::from_slice::<JsonValue>(&modern.body).expect("modern json");
        assert_eq!(
            legacy["result"]["structuredContent"],
            modern["result"]["structuredContent"]
        );
        assert_eq!(modern["result"]["resultType"], "complete");

        let requests = received.await.expect("backend requests");
        assert_eq!(requests.len(), 2);
        for request in requests {
            let body = request.split("\r\n\r\n").nth(1).expect("request body");
            assert!(body.contains("******"));
            assert!(!body.contains("123-45-6789"));
        }
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

    #[tokio::test]
    async fn input_schema_failure_is_a_tool_error_without_backend_traffic() {
        let tool = test_tool(
            "weather",
            "Get weather",
            "http://127.0.0.1:1",
            McpHttpMethod::Get,
            None,
            json!({
                "type": "object",
                "required": ["city"],
                "properties": {"city": {"type": "string"}}
            }),
        );
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
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(body["result"]["resultType"], "complete");
        assert!(body["error"].is_null());
        assert!(!body["result"].to_string().contains("properties"));
    }

    #[tokio::test]
    async fn output_schema_failure_discards_contradictory_structured_content() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let mut tool = test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.output_schema = Some(json!({"type": "array"}));
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
                body: br#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(body["result"]["resultType"], "complete");
        assert!(body["result"].get("structuredContent").is_none());
        received.await.expect("backend request");
    }

    #[tokio::test]
    async fn declared_array_output_preserves_arbitrary_structured_root() {
        let (base, received) = spawn_http_server(http_json_response(json!([1, 2]))).await;
        let mut tool = test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.output_schema = Some(json!({"type": "array", "items": {"type": "integer"}}));
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
                body: br#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["structuredContent"], json!([1, 2]));
        assert_eq!(
            serde_json::from_str::<JsonValue>(
                body["result"]["content"][0]["text"].as_str().unwrap()
            )
            .unwrap(),
            json!([1, 2])
        );
        received.await.expect("backend request");
    }

    #[test]
    fn candidate_runtime_rejects_invalid_output_schema_atomically() {
        let mut tool = test_tool(
            "weather",
            "Get weather",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.output_schema = Some(json!({"$ref": "https://example.com/schema"}));
        assert!(
            McpRouterRuntime::new(McpRouterConfig {
                tools: vec![tool],
                ..McpRouterConfig::default()
            })
            .is_err()
        );
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
            output_schema: None,
            input_schema_configured: true,
            tool_metadata: allow_private_target_metadata(),
        }
    }

    fn allow_private_target_metadata() -> JsonValue {
        json!({
            "runtime": {
                "allowPrivateTargetHost": true
            }
        })
    }

    fn retry_target_metadata(status_codes: &[u16]) -> JsonValue {
        json!({
            "safety": {
                "idempotent": true
            },
            "runtime": {
                "allowPrivateTargetHost": true,
                "retry": {
                    "enabled": true,
                    "maxAttempts": 2,
                    "retryStatusCodes": status_codes
                }
            }
        })
    }

    fn accept_json() -> (String, String) {
        (
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        )
    }

    fn stateless_test_config(tools: Vec<McpToolConfig>) -> McpRouterConfig {
        McpRouterConfig {
            protocols: McpProtocolsConfig {
                legacy: McpLegacyProtocolConfig::default(),
                stateless: McpStatelessProtocolConfig {
                    enabled: false,
                    versions: vec![STATELESS_RC_VERSION.to_string()],
                    ..McpStatelessProtocolConfig::default()
                },
            },
            tools,
            ..McpRouterConfig::default()
        }
    }

    fn stateless_test_runtime(tools: Vec<McpToolConfig>) -> McpRouterRuntime {
        McpRouterRuntime::new(stateless_test_config(tools)).expect("runtime")
    }

    fn stateless_request(
        method: &str,
        params: JsonValue,
        stale_session_id: Option<&str>,
    ) -> McpHttpRequest {
        let mut params = params.as_object().cloned().expect("params object");
        params.insert(
            "_meta".to_string(),
            json!({
                "io.modelcontextprotocol/protocolVersion": STATELESS_RC_VERSION,
                "io.modelcontextprotocol/clientInfo": {"name":"phase4-test","version":"1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }),
        );
        let mut headers = vec![
            accept_json(),
            ("content-type".to_string(), JSON_CONTENT_TYPE.to_string()),
            (
                MCP_PROTOCOL_VERSION_HEADER.to_string(),
                STATELESS_RC_VERSION.to_string(),
            ),
            ("mcp-method".to_string(), method.to_string()),
        ];
        if method == "tools/call"
            && let Some(name) = params.get("name").and_then(JsonValue::as_str)
        {
            headers.push(("mcp-name".to_string(), name.to_string()));
        }
        if let Some(session_id) = stale_session_id {
            headers.push((MCP_SESSION_ID_HEADER.to_string(), session_id.to_string()));
        }
        McpHttpRequest {
            method: "POST".to_string(),
            path: stale_session_id
                .map(|session_id| format!("/mcp?sessionId={session_id}"))
                .unwrap_or_else(|| "/mcp".to_string()),
            headers,
            body: serde_json::to_vec(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": method,
                "params": params
            }))
            .expect("body"),
        }
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
    async fn http_tool_retries_configured_retryable_status() {
        let (base, received) = spawn_http_sequence_server(vec![
            http_json_response_with_status(500, json!({"error": "temporary"})),
            http_json_response(json!({"ok": true})),
        ])
        .await;
        let mut tool = test_tool(
            "weather",
            "Get weather",
            &base,
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.tool_metadata = retry_target_metadata(&[500]);
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
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["structuredContent"]["ok"], true);
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 2);
    }

    #[tokio::test]
    async fn http_tool_does_not_retry_destructive_call_without_idempotency_key() {
        let (base, received) = spawn_http_server(http_json_response_with_status(
            500,
            json!({"error": "temporary"}),
        ))
        .await;
        let mut tool = test_tool(
            "recordDecision",
            "Record decision",
            &base,
            McpHttpMethod::Post,
            None,
            default_input_schema(),
        );
        tool.tool_metadata = json!({
            "safety": {
                "idempotent": true,
                "destructive": true
            },
            "runtime": {
                "allowPrivateTargetHost": true,
                "retry": {
                    "enabled": true,
                    "maxAttempts": 2,
                    "retryStatusCodes": [500]
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
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"recordDecision","arguments":{"customerId":"CUST-1001"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        let request = received.await.expect("server request");
        assert!(request.contains("POST /weather HTTP/1.1"));
    }

    #[tokio::test]
    async fn mcp_proxy_tool_retries_configured_retryable_status() {
        let backend_result = json!({
            "jsonrpc": "2.0",
            "id": "retry-success",
            "result": mcp_text_result("ok")
        });
        let (base, received) = spawn_http_sequence_server(vec![
            backend_initialize_response("backend-session"),
            http_empty_response(202),
            http_json_response_with_status(503, json!({"error": "temporary"})),
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
                input_schema: default_input_schema(),
                output_schema: None,
                input_schema_configured: true,
                tool_metadata: retry_target_metadata(&[503]),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: accept_json_with_session(&runtime),
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "ok");
        let requests = received.await.expect("server requests");
        assert_eq!(requests.len(), 4);
        assert_eq!(request_json_body(&requests[0])["method"], "initialize");
        assert_eq!(
            request_json_body(&requests[1])["method"],
            "notifications/initialized"
        );
        assert_eq!(request_json_body(&requests[2])["method"], "tools/call");
        assert_eq!(request_json_body(&requests[3])["method"], "tools/call");
    }

    #[tokio::test]
    async fn tool_call_rejects_private_target_host_without_explicit_allow() {
        let mut tool = test_tool(
            "weather",
            "Get weather",
            "http://127.0.0.1:8080",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.tool_metadata = default_object();
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
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"weather","arguments":{}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("allowPrivateTargetHost")
        );
    }

    #[test]
    fn runtime_rejects_missing_path_parameter_mapping() {
        let mut tool = test_tool(
            "getCustomerProfile",
            "Get customer profile",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.path = "/customers/{customerId}".to_string();
        tool.tool_metadata = json!({
            "routing": {
                "parameters": {
                    "channel": "query"
                }
            }
        });
        let error = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![tool],
            ..McpRouterConfig::default()
        })
        .expect_err("missing path parameter mapping");

        assert!(
            error
                .to_string()
                .contains("requires toolMetadata.routing.parameters.customerId=path")
        );
    }

    #[tokio::test]
    async fn tool_call_rejects_protected_mapped_header() {
        let mut tool = test_tool(
            "searchOffers",
            "Search offers",
            "https://example.com",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        );
        tool.tool_metadata = json!({
            "routing": {
                "parameters": {
                    "Authorization": "header"
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
                body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"searchOffers","arguments":{"Authorization":"Bearer token"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["error"]["code"], -32000);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("protected header `Authorization`")
        );
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
            "runtime": {
                "allowPrivateTargetHost": true
            },
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
            "runtime": {
                "allowPrivateTargetHost": true
            },
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
    async fn stateless_post_is_classified_but_disabled_without_session_mutation() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let stale_session_id = {
            let mut store = runtime.sessions.lock().await;
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
        let before = runtime.sessions.lock().await.len();
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    accept_json(),
                    (MCP_SESSION_ID_HEADER.to_string(), stale_session_id),
                    (
                        MCP_PROTOCOL_VERSION_HEADER.to_string(),
                        STATELESS_RC_VERSION.to_string(),
                    ),
                ],
                body: serde_json::to_vec(&json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/list",
                    "params": {
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": STATELESS_RC_VERSION
                        }
                    }
                }))
                .expect("body"),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        assert!(String::from_utf8_lossy(&response.body).contains("is disabled"));
        assert_eq!(runtime.sessions.lock().await.len(), before);
    }

    #[tokio::test]
    async fn stateless_discover_is_complete_private_and_sessionless() {
        let mut runtime = stateless_test_runtime(Vec::new());
        let stale_session_id = {
            let mut store = runtime.sessions.lock().await;
            let id = uuid::Uuid::new_v4().to_string();
            store.insert(
                id.clone(),
                McpGatewaySession::new(DEFAULT_PROTOCOL_VERSION.to_string(), "stale".to_string()),
            );
            id
        };
        runtime.config.protocols.stateless.enabled = true;
        let response = runtime
            .handle_request(stateless_request(
                "server/discover",
                json!({}),
                Some(stale_session_id.as_str()),
            ))
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        assert_eq!(
            first_header(&response.headers, MCP_PROTOCOL_VERSION_HEADER).as_deref(),
            Some(STATELESS_RC_VERSION)
        );
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["result"]["cacheScope"], "private");
        assert_eq!(body["result"]["ttlMs"], 30_000);
        assert_eq!(
            body["result"]["capabilities"],
            json!({"tools":{"listChanged":false}})
        );
        assert_eq!(
            body["result"]["supportedVersions"],
            json!([STATELESS_RC_VERSION])
        );
        assert_eq!(
            body["result"]["capabilities"]["tools"]["listChanged"],
            false
        );
        assert_eq!(
            body["result"]["_meta"][SERVER_INFO_META_KEY]["name"],
            "light-gateway"
        );
        assert!(
            runtime
                .sessions
                .lock()
                .await
                .contains_key(&stale_session_id)
        );
        assert_eq!(runtime.stateless_discover_cache.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn stateless_tools_call_executes_http_without_session_and_uses_typed_headers() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let tool = test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            json!({
                "type":"object",
                "required":["region"],
                "properties":{
                    "region":{"type":"string","x-mcp-header":"Mcp-Param-Region"}
                }
            }),
        );
        let mut config = stateless_test_config(vec![tool]);
        config.protocols.stateless.enabled = true;
        let runtime = McpRouterRuntime::new(config).expect("runtime");
        let mut request = stateless_request(
            "tools/call",
            json!({"name":"weather","arguments":{"region":"ca-central-1"}}),
            None,
        );
        request.headers.extend([
            ("Mcp-Param-Region".into(), "ca-central-1".into()),
            ("Accept-Language".into(), "fr-CA".into()),
            (
                "Traceparent".into(),
                "00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01".into(),
            ),
            ("Authorization".into(), "Bearer frontend-secret".into()),
            ("Cookie".into(), "session=frontend-secret".into()),
            ("X-Forwarded-For".into(), "203.0.113.5".into()),
            ("X-Correlation-Id".into(), "spoofed".into()),
            ("X-Tenant-Id".into(), "spoofed".into()),
            ("Mcp-Unknown".into(), "spoofed".into()),
            ("X-Backend-Token".into(), "spoofed".into()),
            ("Idempotency-Key".into(), "attacker-chosen-key".into()),
        ]);
        let response = runtime
            .handle_request_with_context(
                request,
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        issuer: Some("https://issuer.example".into()),
                        user_id: Some("alice".into()),
                        host: Some("host-1".into()),
                        claims: json!({"tenant":"tenant-1"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("trusted-correlation".into()),
                    ..McpRequestContext::default()
                },
            )
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["resultType"], "complete");
        assert_eq!(body["result"]["structuredContent"], json!({"ok":true}));
        assert_eq!(
            body["result"]["_meta"][SERVER_INFO_META_KEY]["name"],
            "light-gateway"
        );
        assert_eq!(runtime.sessions.lock().await.len(), 0);

        let request = received
            .await
            .expect("backend request")
            .to_ascii_lowercase();
        for expected in [
            "accept-language: fr-ca",
            "traceparent: 00-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-bbbbbbbbbbbbbbbb-01",
            "x-correlation-id: trusted-correlation",
            "x-user-id: alice",
            "x-host-id: host-1",
            "x-tenant-id: tenant-1",
        ] {
            assert!(request.contains(expected), "missing {expected}: {request}");
        }
        for forbidden in [
            "frontend-secret",
            "x-forwarded-for:",
            "x-correlation-id: spoofed",
            "x-tenant-id: spoofed",
            "mcp-unknown:",
            "mcp-param-region:",
            "x-backend-token:",
            "idempotency-key:",
            "attacker-chosen-key",
        ] {
            assert!(
                !request.contains(forbidden),
                "leaked {forbidden}: {request}"
            );
        }
    }

    #[tokio::test]
    async fn stateless_tools_call_rejects_header_mismatch_before_backend_traffic() {
        let tool = test_tool(
            "weather",
            "Get weather",
            "http://127.0.0.1:1",
            McpHttpMethod::Get,
            None,
            json!({
                "type":"object",
                "required":["region"],
                "properties":{
                    "region":{"type":"string","x-mcp-header":"Mcp-Param-Region"}
                }
            }),
        );
        let mut config = stateless_test_config(vec![tool]);
        config.protocols.stateless.enabled = true;
        let runtime = McpRouterRuntime::new(config).expect("runtime");
        let mut request = stateless_request(
            "tools/call",
            json!({"name":"weather","arguments":{"region":"ca-central-1"}}),
            None,
        );
        request
            .headers
            .push(("Mcp-Param-Region".into(), "us-east-1".into()));
        let response = runtime
            .handle_request(request)
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 400);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32020);
        assert_eq!(runtime.sessions.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn stateless_tools_call_returns_explicit_error_for_legacy_mcp_backend() {
        let mut tool = test_tool(
            "legacy_backend",
            "Legacy backend",
            "http://127.0.0.1:1",
            McpHttpMethod::Post,
            None,
            default_input_schema(),
        );
        tool.api_type = McpToolType::Mcp;
        let mut config = stateless_test_config(vec![tool]);
        config.protocols.stateless.enabled = true;
        let runtime = McpRouterRuntime::new(config).expect("runtime");
        let response = runtime
            .handle_request(stateless_request(
                "tools/call",
                json!({"name":"legacy_backend","arguments":{}}),
                None,
            ))
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(body["result"]["resultType"], "complete");
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("does not support stateless calls")
        );
        assert_eq!(runtime.sessions.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn stateless_http_tool_streams_and_rejects_oversized_backend_response() {
        let (base, received) = spawn_http_server(http_json_response(json!({
            "payload":"x".repeat(512)
        })))
        .await;
        let mut config = stateless_test_config(vec![test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        )]);
        config.protocols.stateless.enabled = true;
        config.max_response_body_bytes = 128;
        let runtime = McpRouterRuntime::new(config).expect("runtime");
        let response = runtime
            .handle_request(stateless_request(
                "tools/call",
                json!({"name":"weather","arguments":{}}),
                None,
            ))
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32000);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("maxResponseBodyBytes")
        );
        received.await.expect("backend request");
    }

    #[tokio::test]
    async fn stateless_http_tool_enforces_live_per_target_concurrency() {
        let (base, first_seen, release, received) =
            spawn_http_sequence_server_with_first_response_gate(vec![http_json_response(json!({
                "ok": true
            }))])
            .await;
        let mut config = stateless_test_config(vec![test_tool(
            "weather",
            "Get weather",
            base.as_str(),
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        )]);
        config.protocols.stateless.enabled = true;
        config
            .protocols
            .stateless
            .max_concurrent_backend_calls_per_target = 1;
        let runtime = McpRouterRuntime::new(config).expect("runtime");
        let first_runtime = runtime.clone();
        let first = tokio::spawn(async move {
            first_runtime
                .handle_request(stateless_request(
                    "tools/call",
                    json!({"name":"weather","arguments":{}}),
                    None,
                ))
                .await
                .expect("handle")
                .expect("response")
        });
        first_seen.await.expect("first backend call");

        let second = runtime
            .handle_request(stateless_request(
                "tools/call",
                json!({"name":"weather","arguments":{}}),
                None,
            ))
            .await
            .expect("handle")
            .expect("response");
        let second = serde_json::from_slice::<JsonValue>(&second.body).expect("json");
        assert_eq!(second["error"]["code"], -32000);
        assert!(
            second["error"]["message"]
                .as_str()
                .expect("message")
                .contains("backend target exceeds")
        );

        release.send(()).expect("release first response");
        let first = first.await.expect("first task");
        let first = serde_json::from_slice::<JsonValue>(&first.body).expect("json");
        assert_eq!(first["result"]["structuredContent"]["ok"], true);
        assert_eq!(received.await.expect("backend requests").len(), 1);
    }

    #[tokio::test]
    async fn stateless_discover_is_replica_and_backend_snapshot_independent() {
        let discovery = Arc::new(FakeDiscovery::new(discovery_snapshot(
            "http://127.0.0.1:8080",
            "service-a",
            Some("dev"),
            None,
        )));
        let mut first = McpRouterRuntime::new_with_discovery(
            stateless_test_config(Vec::new()),
            Some(discovery.clone()),
        )
        .expect("runtime");
        first.config.protocols.stateless.enabled = true;
        let mut second = stateless_test_runtime(Vec::new());
        second.config.protocols.stateless.enabled = true;
        let first_body = first
            .handle_request(stateless_request("server/discover", json!({}), None))
            .await
            .expect("handle")
            .expect("response")
            .body;
        let second_body = second
            .handle_request(stateless_request("server/discover", json!({}), None))
            .await
            .expect("handle")
            .expect("response")
            .body;
        assert_eq!(first_body, second_body);
        assert!(discovery.lookups.lock().expect("lookups").is_empty());
    }

    #[tokio::test]
    async fn stateless_tools_list_is_deterministic_bounded_and_principal_private() {
        let mut runtime = stateless_test_runtime(vec![
            test_tool(
                "zeta",
                "Zeta",
                "http://127.0.0.1:1",
                McpHttpMethod::Get,
                None,
                default_input_schema(),
            ),
            test_tool(
                "alpha",
                "Alpha",
                "http://127.0.0.1:1",
                McpHttpMethod::Get,
                None,
                default_input_schema(),
            ),
        ]);
        runtime.config.protocols.stateless.enabled = true;
        for user in ["alice", "bob"] {
            let response = runtime
                .handle_request_with_context(
                    stateless_request("tools/list", json!({}), None),
                    McpRequestContext {
                        auth: Some(AuthPrincipal {
                            issuer: Some("https://issuer.example".to_string()),
                            user_id: Some(user.to_string()),
                            ..AuthPrincipal::default()
                        }),
                        ..McpRequestContext::default()
                    },
                )
                .await
                .expect("handle")
                .expect("response");
            let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
            assert_eq!(body["result"]["resultType"], "complete");
            assert_eq!(body["result"]["cacheScope"], "private");
            assert!(body["result"].get("nextCursor").is_none());
            let names = body["result"]["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .map(|tool| tool["name"].as_str().expect("name"))
                .collect::<Vec<_>>();
            assert_eq!(names, vec!["alpha", "zeta"]);
        }
        assert_eq!(runtime.sessions.lock().await.len(), 0);
        assert_eq!(runtime.stateless_tools_list_cache.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn stateless_tools_list_fails_atomically_before_cache_on_catalog_limit() {
        let mut config = stateless_test_config(vec![
            test_tool(
                "one",
                "One",
                "http://127.0.0.1:1",
                McpHttpMethod::Get,
                None,
                default_input_schema(),
            ),
            test_tool(
                "two",
                "Two",
                "http://127.0.0.1:1",
                McpHttpMethod::Get,
                None,
                default_input_schema(),
            ),
        ]);
        config.protocols.stateless.max_tools_list_items = 1;
        let mut runtime = McpRouterRuntime::new(config).expect("runtime");
        runtime.config.protocols.stateless.enabled = true;
        let response = runtime
            .handle_request(stateless_request("tools/list", json!({}), None))
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32000);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains("pagination")
        );
        assert_eq!(runtime.stateless_tools_list_cache.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn stateless_tools_list_fails_atomically_on_encoded_response_limit() {
        let mut config = stateless_test_config(vec![test_tool(
            "weather",
            &"large descriptor ".repeat(32),
            "http://127.0.0.1:1",
            McpHttpMethod::Get,
            None,
            default_input_schema(),
        )]);
        config.max_response_body_bytes = 128;
        let mut runtime = McpRouterRuntime::new(config).expect("runtime");
        runtime.config.protocols.stateless.enabled = true;
        let response = runtime
            .handle_request(stateless_request("tools/list", json!({}), None))
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(runtime.stateless_tools_list_cache.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn stateless_cached_list_rechecks_final_envelope_size() {
        let mut config = stateless_test_config(Vec::new());
        config.max_response_body_bytes = 256;
        let mut runtime = McpRouterRuntime::new(config).expect("runtime");
        runtime.config.protocols.stateless.enabled = true;
        let first = runtime
            .handle_request(stateless_request("tools/list", json!({}), None))
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(first.status, 200);
        assert_eq!(runtime.stateless_tools_list_cache.lock().await.len(), 1);

        let mut oversized = stateless_request("tools/list", json!({}), None);
        let mut payload = serde_json::from_slice::<JsonValue>(&oversized.body).expect("body");
        payload["id"] = json!("x".repeat(512));
        oversized.body = serde_json::to_vec(&payload).expect("body");
        let response = runtime
            .handle_request(oversized)
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32000);
        assert_eq!(runtime.stateless_tools_list_cache.lock().await.len(), 1);
    }

    #[test]
    fn stateless_response_cache_is_bounded_and_expires_at_advertised_ttl() {
        let now = Instant::now();
        let mut cache = StatelessResponseCache::new(1);
        cache.insert(
            "first".to_string(),
            json!({"value":1}),
            Duration::from_millis(10),
            now,
        );
        assert_eq!(cache.get("first", now), Some(json!({"value":1})));
        cache.insert(
            "second".to_string(),
            json!({"value":2}),
            Duration::from_millis(10),
            now,
        );
        assert!(cache.get("first", now).is_none());
        assert!(
            cache
                .get("second", now + Duration::from_millis(10))
                .is_none()
        );
        assert!(
            bounded_json_result_response(200, &json!(1), &json!("x".repeat(256)), 32)
                .expect("bounded serialization")
                .is_none()
        );
    }

    #[tokio::test]
    async fn stateless_list_rejects_cursor_legacy_query_and_unknown_method() {
        let mut runtime = stateless_test_runtime(Vec::new());
        runtime.config.protocols.stateless.enabled = true;
        for params in [
            json!({"cursor":"opaque"}),
            json!({"cursor":7}),
            json!({"query":"weather"}),
        ] {
            let response = runtime
                .handle_request(stateless_request("tools/list", params, None))
                .await
                .expect("handle")
                .expect("response");
            let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
            assert_eq!(body["error"]["code"], -32602);
        }
        let response = runtime
            .handle_request(stateless_request("unknown/method", json!({}), None))
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 404);
        assert_eq!(response.content_type, JSON_CONTENT_TYPE);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn stateless_transport_returns_frozen_header_and_version_errors() {
        let mut runtime = stateless_test_runtime(Vec::new());
        runtime.config.protocols.stateless.enabled = true;
        let mut mismatch = stateless_request("server/discover", json!({}), None);
        mismatch
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case(MCP_PROTOCOL_VERSION_HEADER));
        let response = runtime
            .handle_request(mismatch)
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32020);

        let mut unsupported = stateless_request("server/discover", json!({}), None);
        for (_, value) in unsupported
            .headers
            .iter_mut()
            .filter(|(name, _)| name.eq_ignore_ascii_case(MCP_PROTOCOL_VERSION_HEADER))
        {
            *value = "2099-01-01".to_string();
        }
        let mut payload = serde_json::from_slice::<JsonValue>(&unsupported.body).expect("body");
        payload["params"]["_meta"][STATELESS_PROTOCOL_META_KEY] = json!("2099-01-01");
        unsupported.body = serde_json::to_vec(&payload).expect("body");
        let response = runtime
            .handle_request(unsupported)
            .await
            .expect("handle")
            .expect("response");
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json");
        assert_eq!(body["error"]["code"], -32022);
        assert!(
            body["error"]["message"]
                .as_str()
                .expect("message")
                .contains(STATELESS_RC_VERSION)
        );
        assert_eq!(runtime.sessions.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn stateless_delete_does_not_read_or_remove_stale_legacy_session() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let session_id = {
            let mut store = runtime.sessions.lock().await;
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
        let response = runtime
            .handle_request(McpHttpRequest {
                method: "DELETE".to_string(),
                path: "/mcp".to_string(),
                headers: vec![
                    (MCP_SESSION_ID_HEADER.to_string(), session_id.clone()),
                    (
                        MCP_PROTOCOL_VERSION_HEADER.to_string(),
                        STATELESS_RC_VERSION.to_string(),
                    ),
                ],
                body: Vec::new(),
            })
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(response.status, 405);
        assert!(runtime.sessions.lock().await.contains_key(&session_id));
    }

    #[tokio::test]
    async fn reload_evicts_sessions_with_disabled_version_or_binding_contract() {
        let original = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");
        let (retained_id, version_evicted_id, binding_evicted_id) = {
            let mut store = original.sessions.lock().await;
            let retained_id = uuid::Uuid::new_v4().to_string();
            let version_evicted_id = uuid::Uuid::new_v4().to_string();
            let binding_evicted_id = uuid::Uuid::new_v4().to_string();
            store.insert(
                retained_id.clone(),
                McpGatewaySession::new("2025-11-25".to_string(), "a".to_string()),
            );
            store.insert(
                version_evicted_id.clone(),
                McpGatewaySession::new("2024-11-05".to_string(), "b".to_string()),
            );
            let mut incompatible =
                McpGatewaySession::new("2025-11-25".to_string(), "c".to_string());
            incompatible.binding_contract = 0;
            store.insert(binding_evicted_id.clone(), incompatible);
            (retained_id, version_evicted_id, binding_evicted_id)
        };
        let mut reloaded = McpRouterRuntime::new(McpRouterConfig {
            protocols: McpProtocolsConfig {
                legacy: McpLegacyProtocolConfig {
                    enabled: true,
                    versions: vec!["2025-11-25".to_string()],
                },
                stateless: McpStatelessProtocolConfig::default(),
            },
            ..McpRouterConfig::default()
        })
        .expect("runtime");
        reloaded.preserve_state_from(&original);
        let store = reloaded.sessions.lock().await;
        assert!(store.contains_key(&retained_id));
        assert!(!store.contains_key(&version_evicted_id));
        assert!(!store.contains_key(&binding_evicted_id));
        assert_eq!(reloaded.reload_session_evictions.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn typed_legacy_headers_keep_secrets_out_of_backend_and_cache_identity() {
        let headers = ForwardedHeaderContext::legacy(&[
            ("Authorization".to_string(), "Bearer secret".to_string()),
            ("Cookie".to_string(), "session=secret".to_string()),
            ("X-Tenant-Id".to_string(), "tenant-a".to_string()),
            ("X-Unknown".to_string(), "legacy-compatible".to_string()),
        ]);
        assert!(headers.backend_headers.iter().all(|(name, _)| {
            !name.eq_ignore_ascii_case("authorization") && !name.eq_ignore_ascii_case("cookie")
        }));
        assert!(
            headers
                .backend_headers
                .iter()
                .any(|(name, _)| name == "X-Unknown")
        );
        let cache = format!("{:?}", headers.cache_headers);
        assert!(!cache.contains("secret"));
        assert!(!cache.contains("authorization"));
        assert!(!cache.contains("cookie"));
        assert!(cache.contains("x-tenant-id"));
        assert!(headers.cache_headers.iter().all(|(_, digest)| {
            digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        }));
        assert!(
            !headers.cache_eligible,
            "unknown policy headers disable caching"
        );
    }

    #[test]
    fn stateless_header_policy_is_allowlist_only_and_prefers_trusted_context() {
        let context = McpRequestContext {
            auth: Some(AuthPrincipal {
                user_id: Some("trusted-user".to_string()),
                host: Some("trusted-host".to_string()),
                ..AuthPrincipal::default()
            }),
            correlation_id: Some("trusted-correlation".to_string()),
            ..McpRequestContext::default()
        };
        let headers = ForwardedHeaderContext::stateless(
            &[
                ("Authorization".to_string(), "Bearer secret".to_string()),
                ("Cookie".to_string(), "session=secret".to_string()),
                ("X-Forwarded-User".to_string(), "spoofed".to_string()),
                ("X-User-Id".to_string(), "spoofed".to_string()),
                ("Traceparent".to_string(), "00-trace-parent".to_string()),
                ("Accept-Language".to_string(), "en-CA".to_string()),
            ],
            &context,
        );
        assert!(
            headers
                .backend_headers
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("x-forwarded-user"))
        );
        assert!(headers.backend_headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-user-id") && value == "trusted-user"
        }));
        assert!(headers.backend_headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("x-correlation-id") && value == "trusted-correlation"
        }));
        assert!(
            headers
                .backend_headers
                .iter()
                .any(|(name, _)| { name.eq_ignore_ascii_case("traceparent") })
        );
        assert!(
            headers
                .backend_headers
                .iter()
                .any(|(name, _)| { name.eq_ignore_ascii_case("accept-language") })
        );
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
                    delegation: None,
                    anonymous_binding: None,
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
    async fn mcp_denied_top_level_object_returns_tool_error() {
        let (base, _received) = spawn_http_server(http_json_response(json!({
            "accountType": "S",
            "ssn": "secret"
        })))
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
  account@call:
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
                    "account",
                    "Get account",
                    base.as_str(),
                    McpHttpMethod::Get,
                    Some("account@call"),
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
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"account","arguments":{}}}"#.to_vec(),
                },
                McpRequestContext {
                    auth: Some(AuthPrincipal {
                        role: Some("teller".to_string()),
                        claims: json!({"role": "teller"}),
                        ..AuthPrincipal::default()
                    }),
                    correlation_id: Some("corr-1".to_string()),
                    delegation: None,
                    anonymous_binding: None,
                },
            )
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["isError"], true);
        assert_eq!(
            body["result"]["content"][0]["text"],
            "Access denied by response filter"
        );
        assert!(body["result"].get("structuredContent").is_none());
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
                    delegation: None,
                    anonymous_binding: None,
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
                    delegation: None,
                    anonymous_binding: None,
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
                    delegation: None,
                    anonymous_binding: None,
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
                    delegation: None,
                    anonymous_binding: None,
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
