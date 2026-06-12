use crate::access_control::{AccessControlRuntime, AccessDecision, load_access_control_runtime};
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
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;
use url::Url;

pub const MCP_ROUTER_FILE: &str = "mcp-router.yml";
pub const MCP_ROUTER_LEGACY_FILE: &str = "mcp-router.yaml";
pub const MCP_ROUTER_MODULE_ID: &str = "light-pingora/mcp-router";
pub const MCP_ROUTER_CONFIG_NAME: &str = "mcp-router";

const DEFAULT_MCP_PATH: &str = "/mcp";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
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
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub tools: Vec<McpToolConfig>,
}

impl Default for McpRouterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: default_mcp_path(),
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolConfig {
    pub name: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpResponseMode {
    Json,
    EventStream,
}

#[derive(Clone)]
pub struct McpRouterRuntime {
    config: McpRouterConfig,
    tools: BTreeMap<String, McpToolConfig>,
    client: reqwest::Client,
    direct_registry: DirectRegistryConfig,
    discovery: Option<Arc<dyn McpDiscoveryResolver>>,
    policy: Option<Arc<AccessControlRuntime>>,
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
        Ok(Self {
            config,
            tools,
            client,
            direct_registry,
            discovery,
            policy,
        })
    }

    pub fn config(&self) -> &McpRouterConfig {
        &self.config
    }

    pub fn matches_path(&self, path: &str) -> bool {
        self.config.enabled && path == self.config.path
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
            "DELETE" => Ok(Some(method_not_allowed_response())),
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
        if id.is_none() {
            return Ok(accepted_response());
        }
        if message.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
            return rpc_error_response(
                response_mode,
                200,
                id.unwrap_or(JsonValue::Null),
                -32600,
                "invalid JSON-RPC version",
            );
        }

        let id = id.unwrap_or(JsonValue::Null);
        match method {
            "initialize" => {
                rpc_result_response(response_mode, 200, id, self.initialize_result(message))
            }
            "tools/list" => {
                rpc_result_response(response_mode, 200, id, self.tools_list_result(message))
            }
            "tools/call" => match self
                .handle_tool_call(message, &request.headers, context)
                .await
            {
                Ok(result) => rpc_result_response(response_mode, 200, id, result),
                Err(error) => rpc_error_response(response_mode, 200, id, error.code, error.message),
            },
            _ => rpc_error_response(
                response_mode,
                200,
                id,
                -32601,
                format!("method `{method}` not found"),
            ),
        }
    }

    fn initialize_result(&self, message: &serde_json::Map<String, JsonValue>) -> JsonValue {
        let requested = message
            .get("params")
            .and_then(|params| params.get("protocolVersion"))
            .and_then(JsonValue::as_str)
            .filter(|version| protocol_version_supported(version));
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

    fn tools_list_result(&self, message: &serde_json::Map<String, JsonValue>) -> JsonValue {
        let query = message
            .get("params")
            .and_then(|params| params.get("query").or_else(|| params.get("intent")))
            .and_then(JsonValue::as_str)
            .map(|value| value.to_ascii_lowercase());
        let tools = self
            .tools
            .values()
            .filter(|tool| {
                query.as_deref().is_none_or(|query| {
                    tool.name.to_ascii_lowercase().contains(query)
                        || tool.description.to_ascii_lowercase().contains(query)
                })
            })
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

    async fn handle_tool_call(
        &self,
        message: &serde_json::Map<String, JsonValue>,
        agent_headers: &[(String, String)],
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
                self.execute_mcp_proxy_tool(tool, &masked_arguments, agent_headers)
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

        let result = if let Some(policy) = self.policy.as_ref() {
            policy
                .filter_mcp_response(
                    tool.name.as_str(),
                    endpoint.as_str(),
                    agent_headers,
                    context.auth.as_ref(),
                    &masked_arguments,
                    context.correlation_id.as_deref(),
                    result,
                )
                .await
        } else {
            result
        };
        log_mcp_tool_call(
            tool,
            endpoint.as_str(),
            started,
            "success",
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
        if matches!(method, McpHttpMethod::Get | McpHttpMethod::Head) {
            append_query_arguments(&mut url, arguments);
        }

        let request_url = url.to_string();
        let request_method = method.as_reqwest();
        let mut request = self
            .client
            .request(request_method.clone(), url)
            .headers(outbound_headers(agent_headers)?);
        if method.sends_json_body() {
            request = request.json(arguments);
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

    async fn execute_mcp_proxy_tool(
        &self,
        tool: &McpToolConfig,
        arguments: &JsonValue,
        agent_headers: &[(String, String)],
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
        let request = json!({
            "jsonrpc": "2.0",
            "id": uuid::Uuid::new_v4().to_string(),
            "method": "tools/call",
            "params": {
                "name": tool.name,
                "arguments": arguments
            }
        });
        let response = self
            .client
            .post(url)
            .headers(outbound_headers(agent_headers)?)
            .json(&request)
            .send()
            .await
            .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
        if !status.is_success() {
            return Err(McpExecutionError::execution_failed(format!(
                "MCP tool `{}` returned HTTP {}: {}",
                tool.name,
                status.as_u16(),
                String::from_utf8_lossy(&body)
            )));
        }
        let message = serde_json::from_slice::<JsonValue>(&body).map_err(|error| {
            McpExecutionError::execution_failed(format!("invalid MCP backend response: {error}"))
        })?;
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
    let text = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "structuredContent": value
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
    )
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

fn method_not_allowed_response() -> McpHttpResponse {
    McpHttpResponse {
        status: 405,
        content_type: TEXT_CONTENT_TYPE.to_string(),
        headers: vec![("allow".to_string(), "POST".to_string())],
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
tools: '[{"name":"weather","description":"Weather","targetHost":"http://127.0.0.1:8080","path":"/weather","method":"GET","inputSchema":{"type":"object"}}]'
"#,
        )
        .expect("parse config");

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
    }

    #[tokio::test]
    async fn notifications_return_accepted_without_body() {
        let runtime = McpRouterRuntime::new(McpRouterConfig::default()).expect("runtime");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 202);
        assert!(response.body.is_empty());
    }

    #[tokio::test]
    async fn tools_list_supports_query_filter() {
        let runtime = runtime_with_tool("weather", "Get weather", "http://127.0.0.1:8080");

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":"1","method":"tools/list","params":{"query":"weath"}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["tools"][0]["name"], "weather");
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
            vec![("allow".to_string(), "POST".to_string())]
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
                headers: vec![accept_json()],
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
                headers: vec![accept_json()],
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
                headers: vec![accept_sse()],
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
                headers: vec![accept_json()],
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
                headers: vec![accept_json()],
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
                headers: vec![accept_json()],
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
        let (base, received) = spawn_http_server(http_json_response(backend_result)).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
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
                headers: vec![
                    accept_json(),
                    ("authorization".to_string(), "Bearer abc".to_string()),
                ],
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["content"][0]["text"], "cloudy");
        let request = received.await.expect("server request");
        assert!(request.starts_with("POST /mcp HTTP/1.1"));
        assert!(request.contains("authorization: Bearer abc"));
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let backend_call = serde_json::from_str::<JsonValue>(body).expect("backend json");
        assert_eq!(backend_call["method"], "tools/call");
        assert_eq!(backend_call["params"]["name"], "weather");
        assert_eq!(backend_call["params"]["arguments"]["city"], "Ottawa");
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
        let (base, received) = spawn_http_server(http_json_response(backend_result)).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "weather".to_string(),
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
                headers: vec![accept_json()],
                body: br#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"weather","arguments":{"city":"Ottawa"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let request = received.await.expect("server request");
        assert!(request.starts_with("POST /mcp HTTP/1.1"));
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let backend_call = serde_json::from_str::<JsonValue>(body).expect("backend json");
        assert_eq!(backend_call["method"], "tools/call");
        assert_eq!(backend_call["params"]["name"], "weather");
        assert_eq!(backend_call["params"]["arguments"]["city"], "Ottawa");
    }

    #[tokio::test]
    async fn mcp_proxy_call_without_input_schema_uses_http_get() {
        let (base, received) = spawn_http_server(http_json_response(json!({"ok": true}))).await;
        let runtime = McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: "serverInfo".to_string(),
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
                headers: vec![accept_json()],
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
                headers: vec![accept_json()],
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
    conditions:
      - operatorCode: isNotNull
        propertyPath: auditInfo.subject_claims.ClaimsMap.role
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
                    headers: vec![accept_json()],
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
        assert_eq!(result["structuredContent"], expected);
        assert_eq!(result["content"][0]["type"], "text");
        let text = result["content"][0]["text"].as_str().expect("text content");
        let parsed = serde_json::from_str::<JsonValue>(text).expect("json text content");
        assert_eq!(parsed, expected);
    }

    fn http_json_response(value: JsonValue) -> String {
        let body = serde_json::to_string(&value).expect("serialize response");
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
            body.len(),
            body
        )
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
}
