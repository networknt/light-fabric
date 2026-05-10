use crate::config_util::deserialize_typed_list;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue};
use serde::de::Error as DeError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value as JsonValue, json};
use serde_yaml::Value as YamlValue;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::time::Duration;
use url::Url;

pub const MCP_ROUTER_FILE: &str = "mcp-router.yml";
pub const MCP_ROUTER_LEGACY_FILE: &str = "mcp-router.yaml";
pub const MCP_ROUTER_MODULE_ID: &str = "light-pingora/mcp-router";
pub const MCP_ROUTER_CONFIG_NAME: &str = "mcp-router";

const DEFAULT_MCP_PATH: &str = "/mcp";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const TOOL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const JSON_CONTENT_TYPE: &str = "application/json";
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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
    #[serde(
        default = "default_input_schema",
        alias = "inputSchema",
        deserialize_with = "deserialize_json_value"
    )]
    pub input_schema: JsonValue,
    #[serde(
        default = "default_object",
        alias = "toolMetadata",
        deserialize_with = "deserialize_json_value"
    )]
    pub tool_metadata: JsonValue,
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
            "" | "http" | "rest" => Ok(Self::Http),
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
}

#[derive(Debug, Clone)]
pub struct McpRouterRuntime {
    config: McpRouterConfig,
    tools: BTreeMap<String, McpToolConfig>,
    client: reqwest::Client,
}

impl McpRouterRuntime {
    pub fn new(config: McpRouterConfig) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        let tools = config
            .tools
            .iter()
            .map(|tool| (tool.name.clone(), tool.clone()))
            .collect::<BTreeMap<_, _>>();
        let client = reqwest::Client::builder()
            .timeout(TOOL_REQUEST_TIMEOUT)
            .build()
            .map_err(|error| {
                RuntimeError::Unsupported(format!("invalid MCP HTTP client: {error}"))
            })?;
        Ok(Self {
            config,
            tools,
            client,
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
        if !self.matches_path(request.path.as_str()) {
            return Ok(None);
        }

        let method = request.method.to_ascii_uppercase();
        match method.as_str() {
            "POST" => self.handle_post(request).await.map(Some),
            "GET" => Ok(Some(method_not_allowed_response())),
            "DELETE" => Ok(Some(method_not_allowed_response())),
            _ => Ok(Some(method_not_allowed_response())),
        }
    }

    async fn handle_post(&self, request: McpHttpRequest) -> Result<McpHttpResponse, RuntimeError> {
        if !accepts_json_response(&request.headers) {
            return json_error_response(
                406,
                JsonValue::Null,
                -32600,
                "Accept header must allow application/json",
            );
        }
        if let Some(version) = first_header(&request.headers, "mcp-protocol-version")
            && !protocol_version_supported(version.as_str())
        {
            return json_error_response(
                400,
                JsonValue::Null,
                -32600,
                format!("unsupported MCP protocol version `{version}`"),
            );
        }

        let payload = match serde_json::from_slice::<JsonValue>(&request.body) {
            Ok(payload) => payload,
            Err(error) => {
                return json_error_response(
                    400,
                    JsonValue::Null,
                    -32700,
                    format!("parse error: {error}"),
                );
            }
        };
        if payload.is_array() {
            return json_error_response(
                400,
                JsonValue::Null,
                -32600,
                "JSON-RPC batch requests are not supported",
            );
        }
        let Some(message) = payload.as_object() else {
            return json_error_response(400, JsonValue::Null, -32600, "invalid JSON-RPC request");
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
            return json_error_response(
                200,
                id.unwrap_or(JsonValue::Null),
                -32600,
                "invalid JSON-RPC version",
            );
        }

        let id = id.unwrap_or(JsonValue::Null);
        match method {
            "initialize" => json_result_response(200, id, self.initialize_result(message)),
            "tools/list" => json_result_response(200, id, self.tools_list_result(message)),
            "tools/call" => match self.handle_tool_call(message, &request.headers).await {
                Ok(result) => json_result_response(200, id, result),
                Err(error) => json_error_response(200, id, error.code, error.message),
            },
            _ => json_error_response(200, id, -32601, format!("method `{method}` not found")),
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
        let tool = self.tools.get(name).ok_or_else(|| McpExecutionError {
            code: -32601,
            message: format!("tool `{name}` not found"),
        })?;

        match tool.api_type {
            McpToolType::Http => {
                self.execute_http_tool(tool, &arguments, agent_headers)
                    .await
            }
            McpToolType::Mcp => Err(McpExecutionError::execution_failed(
                "MCP proxy tools are planned for phase 2",
            )),
        }
    }

    async fn execute_http_tool(
        &self,
        tool: &McpToolConfig,
        arguments: &JsonValue,
        agent_headers: &[(String, String)],
    ) -> Result<JsonValue, McpExecutionError> {
        let mut url = tool_target_url(tool)?;
        if matches!(tool.method, McpHttpMethod::Get | McpHttpMethod::Head) {
            append_query_arguments(&mut url, arguments);
        }

        let mut request = self
            .client
            .request(tool.method.as_reqwest(), url)
            .headers(outbound_headers(agent_headers)?);
        if tool.method.sends_json_body() {
            request = request.json(arguments);
        }
        let response = request
            .send()
            .await
            .map_err(|error| McpExecutionError::execution_failed(error.to_string()))?;
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
            return Ok(json!({ "result": "success" }));
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
            && let Ok(value) = serde_json::from_slice::<JsonValue>(&body)
        {
            return Ok(value);
        }
        if let Ok(value) = serde_json::from_slice::<JsonValue>(&body) {
            return Ok(value);
        }
        Ok(json!({
            "content": [
                {
                    "type": "text",
                    "text": String::from_utf8_lossy(&body)
                }
            ]
        }))
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
    Ok(Some(McpRouterRuntime::new(config)?))
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
    }
    Ok(())
}

fn tool_target_url(tool: &McpToolConfig) -> Result<Url, McpExecutionError> {
    let base = tool
        .target_host
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            McpExecutionError::execution_failed(format!(
                "tool `{}` requires targetHost until discovery is implemented",
                tool.name
            ))
        })?;
    let mut url = Url::parse(base).map_err(|error| {
        McpExecutionError::execution_failed(format!(
            "tool `{}` targetHost `{base}` is invalid: {error}",
            tool.name
        ))
    })?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(McpExecutionError::execution_failed(format!(
            "tool `{}` targetHost `{base}` must use http or https",
            tool.name
        )));
    }
    let base_path = url.path().trim_end_matches('/');
    let tool_path = tool.path.as_str();
    let combined = if base_path.is_empty() || base_path == "/" {
        tool_path.to_string()
    } else if tool_path == "/" {
        base_path.to_string()
    } else {
        format!("{base_path}{tool_path}")
    };
    url.set_path(combined.as_str());
    Ok(url)
}

fn append_query_arguments(url: &mut Url, arguments: &JsonValue) {
    let Some(arguments) = arguments.as_object() else {
        return;
    };
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

fn accepts_json_response(headers: &[(String, String)]) -> bool {
    let accepts = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(ACCEPT.as_str()))
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>();
    if accepts.is_empty() {
        return true;
    }
    accepts.iter().any(|value| {
        value.split(',').any(|item| {
            let media_type = item.split(';').next().unwrap_or_default().trim();
            matches!(media_type, "*/*" | "application/*" | "application/json")
        })
    })
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
    }
}

fn method_not_allowed_response() -> McpHttpResponse {
    McpHttpResponse {
        status: 405,
        content_type: TEXT_CONTENT_TYPE.to_string(),
        headers: vec![("allow".to_string(), "POST, GET".to_string())],
        body: b"method not allowed".to_vec(),
    }
}

fn json_result_response(
    status: u16,
    id: JsonValue,
    result: JsonValue,
) -> Result<McpHttpResponse, RuntimeError> {
    json_body_response(
        status,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }),
    )
}

fn json_error_response(
    status: u16,
    id: JsonValue,
    code: i64,
    message: impl Into<String>,
) -> Result<McpHttpResponse, RuntimeError> {
    json_body_response(
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

fn json_body_response(status: u16, body: JsonValue) -> Result<McpHttpResponse, RuntimeError> {
    let body = serde_json::to_vec(&body)?;
    Ok(McpHttpResponse {
        status,
        content_type: JSON_CONTENT_TYPE.to_string(),
        headers: protocol_headers(),
        body,
    })
}

fn protocol_headers() -> Vec<(String, String)> {
    vec![(
        "mcp-protocol-version".to_string(),
        DEFAULT_PROTOCOL_VERSION.to_string(),
    )]
}

fn deserialize_json_value<'de, D>(deserializer: D) -> Result<JsonValue, D::Error>
where
    D: Deserializer<'de>,
{
    let value = YamlValue::deserialize(deserializer)?;
    yaml_value_to_json(value).map_err(D::Error::custom)
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
    async fn tool_call_get_forwards_arguments_and_agent_headers() {
        let (base, received) = spawn_http_server(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}",
        )
        .await;
        let runtime = runtime_with_tool("weather", "Get weather", base.as_str());

        let response = runtime
            .handle_request(McpHttpRequest {
                method: "POST".to_string(),
                path: "/mcp".to_string(),
                headers: vec![accept_json(), ("authorization".to_string(), "Bearer abc".to_string())],
                body: br#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"weather","arguments":{"city":"New York","unit":"c"}}}"#.to_vec(),
            })
            .await
            .expect("handle")
            .expect("response");

        assert_eq!(response.status, 200);
        let body = serde_json::from_slice::<JsonValue>(&response.body).expect("json body");
        assert_eq!(body["result"]["ok"], true);
        let request = received.await.expect("server request");
        assert!(request.starts_with("GET /weather?city=New+York&unit=c HTTP/1.1"));
        assert!(request.contains("authorization: Bearer abc"));
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

    fn runtime_with_tool(name: &str, description: &str, target_host: &str) -> McpRouterRuntime {
        McpRouterRuntime::new(McpRouterConfig {
            tools: vec![McpToolConfig {
                name: name.to_string(),
                description: description.to_string(),
                protocol: None,
                service_id: None,
                env_tag: None,
                target_host: Some(target_host.to_string()),
                path: "/weather".to_string(),
                method: McpHttpMethod::Get,
                endpoint: None,
                api_type: McpToolType::Http,
                input_schema: default_input_schema(),
                tool_metadata: default_object(),
            }],
            ..McpRouterConfig::default()
        })
        .expect("runtime")
    }

    fn accept_json() -> (String, String) {
        (
            "accept".to_string(),
            "application/json, text/event-stream".to_string(),
        )
    }

    async fn spawn_http_server(response: &'static str) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept connection");
            let mut buffer = vec![0_u8; 4096];
            let read = stream.read(&mut buffer).await.expect("read request");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let _ = tx.send(request);
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });
        (format!("http://{address}"), rx)
    }
}
