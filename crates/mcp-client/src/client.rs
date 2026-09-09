use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, McpTool, McpToolCallResult, McpToolsListResult,
};
use crate::wire;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::Client;
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tracing::debug;

#[derive(Debug)]
struct LegacySessionUnavailable;
impl std::fmt::Display for LegacySessionUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "MCP session is unavailable; the next explicit operation will initialize a new session",
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpProfile {
    Stateless20260728,
    Legacy20251125,
    Legacy20250618,
    Legacy20250326,
}
impl McpProfile {
    pub fn version(self) -> &'static str {
        match self {
            Self::Stateless20260728 => wire::MODERN_VERSION,
            Self::Legacy20251125 => "2025-11-25",
            Self::Legacy20250618 => "2025-06-18",
            Self::Legacy20250326 => "2025-03-26",
        }
    }
    pub fn from_version(version: &str) -> Result<Self> {
        match version {
            "2026-07-28" => Ok(Self::Stateless20260728),
            "2025-11-25" => Ok(Self::Legacy20251125),
            "2025-06-18" => Ok(Self::Legacy20250618),
            "2025-03-26" => Ok(Self::Legacy20250326),
            _ => bail!("unsupported MCP profile {version}"),
        }
    }
}
#[derive(Default)]
struct ClientState {
    initialized: bool,
    session: Option<String>,
    version: Option<String>,
    discovery: Option<(Instant, Value)>,
    tools: Option<(Instant, Vec<McpTool>)>,
}

pub struct McpGatewayClient {
    url: String,
    client: Client,
    max_response_bytes: usize,
    profile: McpProfile,
    states: Mutex<BTreeMap<[u8; 32], (Instant, Arc<Mutex<ClientState>>)>>,
}

const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

impl McpGatewayClient {
    pub fn new(url: &str) -> Result<Self> {
        Self::with_options(url, None, true, 30_000)
    }

    /// Create a client with explicit TLS options.
    ///
    /// - `ca_cert_pem`: PEM-encoded CA certificate or CA bundle to trust.
    /// - `verify_hostname`: When `false`, hostname verification is skipped but the certificate
    ///   chain is still validated against `ca_cert_pem` (mirrors the config-server client behaviour).
    pub fn with_options(
        url: &str,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        Self::with_tls_options(url, ca_cert_pem, verify_hostname, timeout_ms)
    }

    /// Create a client with explicit TLS options.
    pub fn with_tls_options(
        url: &str,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        Self::with_tls_options_and_response_limit(
            url,
            ca_cert_pem,
            verify_hostname,
            timeout_ms,
            DEFAULT_MAX_RESPONSE_BYTES,
        )
    }

    /// Create a client with explicit TLS options and a hard response-body limit.
    pub fn with_tls_options_and_response_limit(
        url: &str,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        timeout_ms: u64,
        max_response_bytes: usize,
    ) -> Result<Self> {
        if max_response_bytes == 0 {
            bail!("MCP gateway max_response_bytes must be greater than zero");
        }
        let endpoint = reqwest::Url::parse(url).context("invalid MCP URL")?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.fragment().is_some()
            || endpoint.query().is_some()
        {
            bail!("MCP endpoint must be an HTTP URL without credentials, query, or fragment");
        }
        let mut builder = Client::builder().redirect(reqwest::redirect::Policy::none());
        builder = builder
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
            tracing::info!(
                ca_cert_count = certificate_count,
                "loaded MCP gateway CA certificate bundle"
            );
        }

        if !verify_hostname {
            builder = builder.danger_accept_invalid_hostnames(true);
        }

        let client = builder.build().context("Failed to build reqwest Client")?;
        Ok(Self {
            url: url.to_string(),
            client,
            max_response_bytes,
            profile: McpProfile::Stateless20260728,
            states: Mutex::new(BTreeMap::new()),
        })
    }

    /// Explicit profile selection; never retries a mutation under another revision.
    pub fn with_profile(mut self, profile: McpProfile) -> Self {
        self.profile = profile;
        self.states = Mutex::new(BTreeMap::new());
        self
    }

    fn key(auth: Option<&str>) -> [u8; 32] {
        Sha256::digest(auth.unwrap_or("").as_bytes()).into()
    }
    async fn state(&self, auth: Option<&str>) -> Result<Arc<Mutex<ClientState>>> {
        let key = Self::key(auth);
        let mut states = self.states.lock().await;
        if !states.contains_key(&key) && states.len() >= 32 {
            // Never evict a partition held by an active or queued operation.
            let idle = states
                .iter()
                .filter(|(_, (_, state))| Arc::strong_count(state) == 1)
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| *key);
            if let Some(idle) = idle {
                states.remove(&idle);
            } else {
                bail!(
                    "MCP client credential partitions are busy; retry after an operation completes"
                );
            }
        }
        let (used, state) = states
            .entry(key)
            .or_insert_with(|| (Instant::now(), Arc::new(Mutex::new(ClientState::default()))));
        *used = Instant::now();
        Ok(Arc::clone(state))
    }

    pub async fn discover(&self, auth: Option<&str>) -> Result<Value> {
        if self.profile != McpProfile::Stateless20260728 {
            bail!("server/discover requires stateless profile");
        }
        let partition = self.state(auth).await?;
        let mut state = partition.lock().await;
        self.ensure_discovery(auth, &mut state).await
    }

    async fn ensure_discovery(&self, auth: Option<&str>, state: &mut ClientState) -> Result<Value> {
        if let Some((expires, value)) = &state.discovery {
            if *expires > Instant::now() {
                return Ok(value.clone());
            }
        }
        let (response, _) = self
            .exchange(
                auth,
                JsonRpcRequest::new("server/discover", json!({})),
                None,
                wire::MODERN_VERSION,
                &[],
            )
            .await?;
        let result = response
            .result
            .ok_or_else(|| anyhow!("missing discovery result"))?;
        if !result["capabilities"].is_object()
            || !result["supportedVersions"]
                .as_array()
                .is_some_and(|versions| versions.iter().any(|v| v == wire::MODERN_VERSION))
        {
            bail!("invalid discovery result");
        }
        let expiry = cache_expiry(&result)?;
        state.discovery = Some((expiry, result.clone()));
        Ok(result)
    }

    async fn ensure_legacy(&self, auth: Option<&str>, state: &mut ClientState) -> Result<()> {
        if state.initialized {
            return Ok(());
        }
        let (response,session)=self.exchange(auth,JsonRpcRequest::new("initialize",json!({"protocolVersion":self.profile.version(),
            "capabilities":{},"clientInfo":{"name":"light-mcp-client","version":env!("CARGO_PKG_VERSION")}})),None,self.profile.version(),&[]).await?;
        let result = response
            .result
            .ok_or_else(|| anyhow!("missing initialize result"))?;
        let version = result["protocolVersion"]
            .as_str()
            .ok_or_else(|| anyhow!("missing negotiated version"))?;
        // A different revision requires a deliberate new profile, never silent fallback.
        if version != self.profile.version() {
            bail!("backend selected a version outside the configured profile");
        }
        if !result["capabilities"].is_object() || !result["serverInfo"].is_object() {
            bail!("invalid initialize result");
        }
        let mut request = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", version)
            .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        if let Some(auth) = auth {
            request = request.header("Authorization", auth);
        }
        if let Some(session) = &session {
            request = request.header("Mcp-Session-Id", session);
        }
        let response = request.send().await?;
        if response.status() != reqwest::StatusCode::ACCEPTED {
            bail!("initialized notification was not accepted");
        }
        let body = read_limited_body(response, self.max_response_bytes).await?;
        if !body.is_empty() {
            bail!("unexpected initialized response body");
        }
        state.initialized = true;
        state.session = session;
        state.version = Some(version.into());
        Ok(())
    }

    pub async fn list_tools(&self, auth: Option<&str>) -> Result<Vec<McpTool>> {
        let partition = self.state(auth).await?;
        let mut state = partition.lock().await;
        self.fetch_tools(auth, &mut state).await
    }

    async fn fetch_tools(
        &self,
        auth: Option<&str>,
        state: &mut ClientState,
    ) -> Result<Vec<McpTool>> {
        if let Some((expires, tools)) = &state.tools {
            if *expires > Instant::now() {
                return Ok(tools.clone());
            }
        }
        let modern = self.profile == McpProfile::Stateless20260728;
        if modern {
            self.ensure_discovery(auth, state).await?;
        } else {
            self.ensure_legacy(auth, state).await?;
        }
        let mut all = Vec::new();
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        let mut names = BTreeSet::new();
        let mut expires = Instant::now() + Duration::from_secs(300);
        for _ in 0..64 {
            let params = cursor
                .as_ref()
                .map_or_else(|| json!({}), |c| json!({"cursor":c}));
            let (response, _) = self
                .exchange_session(auth, JsonRpcRequest::new("tools/list", params), state, &[])
                .await?;
            let result = response
                .result
                .ok_or_else(|| anyhow!("missing tools/list result"))?;
            if modern {
                expires = expires.min(cache_expiry(&result)?);
            }
            let page: McpToolsListResult = serde_json::from_value(result)?;
            for tool in page.tools {
                if all.len() >= 4096 {
                    bail!("tool catalog exceeds limit");
                }
                if !names.insert(tool.name.clone()) {
                    bail!("duplicate tool name");
                }
                if modern && wire::parameter_headers(&tool.input_schema).is_err() {
                    continue;
                }
                all.push(tool);
            }
            match page.next_cursor {
                None => {
                    if modern {
                        state.tools = Some((expires, all.clone()));
                    }
                    return Ok(all);
                }
                Some(next) => {
                    if next.len() > 4096 || !cursors.insert(next.clone()) {
                        bail!("invalid or repeated list cursor");
                    }
                    cursor = Some(next);
                }
            }
        }
        bail!("tool pagination limit exceeded")
    }

    pub async fn call_tool(
        &self,
        auth: Option<&str>,
        name: &str,
        arguments: Value,
    ) -> Result<McpToolCallResult> {
        let partition = self.state(auth).await?;
        let mut state = partition.lock().await;
        let headers = if self.profile == McpProfile::Stateless20260728 {
            let tools = self.fetch_tools(auth, &mut state).await?;
            let tool = tools
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| anyhow!("tool unavailable or has invalid header annotations"))?;
            wire::argument_headers(&wire::parameter_headers(&tool.input_schema)?, &arguments)?
        } else {
            self.ensure_legacy(auth, &mut state).await?;
            Vec::new()
        };
        let (response, _) = self
            .exchange_session(
                auth,
                JsonRpcRequest::new("tools/call", json!({"name":name,"arguments":arguments})),
                &mut state,
                &headers,
            )
            .await?;
        let value = response
            .result
            .ok_or_else(|| anyhow!("missing tools/call result"))?;
        if self.profile == McpProfile::Stateless20260728 && value["resultType"] != "complete" {
            bail!("unsupported or missing MCP resultType");
        }
        if let Some(kind) = value.get("resultType") {
            if kind != "complete" {
                bail!("unsupported MCP resultType");
            }
        }
        serde_json::from_value(value).context("invalid tools/call result")
    }

    /// Explicitly release a credential's legacy session and all private caches.
    pub async fn close(&self, auth: Option<&str>) -> Result<()> {
        let partition = self
            .states
            .lock()
            .await
            .get(&Self::key(auth))
            .map(|(_, state)| Arc::clone(state));
        if let Some(partition) = partition {
            let mut guard = partition.lock().await;
            let state = std::mem::take(&mut *guard);
            if let Some(session) = state.session {
                let mut request = self
                    .client
                    .delete(&self.url)
                    .header("Mcp-Session-Id", session)
                    .header(
                        "MCP-Protocol-Version",
                        state
                            .version
                            .unwrap_or_else(|| self.profile.version().into()),
                    );
                if let Some(auth) = auth {
                    request = request.header("Authorization", auth);
                }
                let response = request.send().await?;
                if !(response.status().is_success()
                    || response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
                    || response.status() == reqwest::StatusCode::NOT_FOUND)
                {
                    bail!("MCP session cleanup failed: {}", response.status());
                }
            }
        }
        Ok(())
    }

    async fn exchange_session(
        &self,
        auth: Option<&str>,
        request: JsonRpcRequest,
        state: &mut ClientState,
        headers: &[(String, String)],
    ) -> Result<(JsonRpcResponse, Option<String>)> {
        let result = self
            .exchange(
                auth,
                request,
                state.session.as_deref(),
                self.profile.version(),
                headers,
            )
            .await;
        if result
            .as_ref()
            .err()
            .is_some_and(|e| e.is::<LegacySessionUnavailable>())
        {
            *state = ClientState::default();
        }
        result
    }

    async fn exchange(
        &self,
        auth: Option<&str>,
        mut request: JsonRpcRequest,
        session: Option<&str>,
        version: &str,
        headers: &[(String, String)],
    ) -> Result<(JsonRpcResponse, Option<String>)> {
        debug!(method=%request.method,"Sending MCP request");
        let mut builder = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", version);
        if self.profile == McpProfile::Stateless20260728 {
            request.params["_meta"] = json!({wire::VERSION_META:wire::MODERN_VERSION,wire::CAPABILITIES_META:{},
                wire::CLIENT_META:{"name":"light-mcp-client","version":env!("CARGO_PKG_VERSION")}});
            builder = builder.header("Mcp-Method", &request.method);
            if request.method == "tools/call" {
                builder = builder.header(
                    "Mcp-Name",
                    wire::encode_header(request.params["name"].as_str().unwrap())?,
                );
            }
        }
        if let Some(auth) = auth {
            builder = builder.header("Authorization", auth);
        }
        if let Some(session) = session {
            builder = builder.header("Mcp-Session-Id", session);
        }
        for (name, value) in headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .json(&request)
            .send()
            .await
            .context("HTTP request to MCP gateway failed")?;
        let status = response.status();
        let lost_session = self.profile != McpProfile::Stateless20260728
            && session.is_some()
            && status == reqwest::StatusCode::NOT_FOUND;
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let session = response
            .headers()
            .get("mcp-session-id")
            .map(|v| v.to_str().map(str::to_string))
            .transpose()?;
        if session.as_ref().is_some_and(|s| {
            s.is_empty() || s.len() > 4096 || !s.bytes().all(|b| (0x21..=0x7e).contains(&b))
        }) {
            bail!("invalid MCP session identifier");
        }
        let parsed = read_rpc_response(
            response,
            self.max_response_bytes,
            &content_type,
            &request.id,
        )
        .await
        .map_err(|e| {
            if lost_session {
                e.context(LegacySessionUnavailable)
            } else {
                e
            }
        });
        if !status.is_success() {
            let error = parsed
                .as_ref()
                .ok()
                .and_then(|v| v.get("error"))
                .and_then(|e| {
                    serde_json::from_value::<crate::protocol::JsonRpcError>(e.clone()).ok()
                })
                .map(anyhow::Error::new)
                .unwrap_or_else(|| anyhow!("MCP gateway returned HTTP {status}"));
            return Err(if lost_session {
                error.context(LegacySessionUnavailable)
            } else {
                error
            });
        }
        let response: JsonRpcResponse = serde_json::from_value(parsed?)?;
        if let Some(error) = &response.error {
            return Err(error.clone().into());
        }
        Ok((response, session))
    }
}

fn cache_expiry(result: &Value) -> Result<Instant> {
    let ttl = result["ttlMs"]
        .as_u64()
        .ok_or_else(|| anyhow!("missing or invalid cache ttlMs"))?;
    if !matches!(result["cacheScope"].as_str(), Some("public" | "private")) {
        bail!("invalid cacheScope");
    }
    // Even public catalogs are partitioned by credential; never broaden scope.
    Ok(Instant::now() + Duration::from_millis(ttl.min(300_000)))
}

async fn read_rpc_response(
    response: reqwest::Response,
    limit: usize,
    content_type: &str,
    id: &Value,
) -> Result<Value> {
    if content_type.split(';').next().unwrap_or("").trim() != "text/event-stream" {
        return wire::response(&read_limited_body(response, limit).await?, content_type, id);
    }
    let mut stream = response.bytes_stream();
    let mut event = Vec::new();
    let mut total = 0usize;
    let mut line_start = 0usize;
    let mut previous_cr = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read MCP gateway response")?;
        for byte in chunk {
            total = total.saturating_add(1);
            if total > limit {
                bail!("MCP gateway response exceeds {limit} bytes");
            }
            if byte == b'\n' && previous_cr {
                previous_cr = false;
                continue;
            }
            previous_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                if event.len() == line_start {
                    let text = std::str::from_utf8(&event)?;
                    let data = text
                        .lines()
                        .filter_map(|l| {
                            l.strip_prefix("data:")
                                .map(|v| v.strip_prefix(' ').unwrap_or(v))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !data.is_empty() {
                        let value: Value = serde_json::from_str(&data)?;
                        if value["jsonrpc"] != "2.0" {
                            bail!("invalid SSE JSON-RPC envelope");
                        }
                        if value.get("method").is_some() {
                            if value.get("id").is_some() {
                                bail!("independent server requests are unsupported");
                            }
                        } else {
                            return wire::response(data.as_bytes(), "application/json", id);
                        }
                    }
                    event.clear();
                    line_start = 0;
                } else {
                    event.push(b'\n');
                    line_start = event.len();
                }
            } else {
                event.push(byte);
            }
        }
    }
    bail!("SSE ended before final response")
}

async fn read_limited_body(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > limit as u64)
    {
        bail!("MCP gateway response exceeds {limit} bytes");
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(limit as u64) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("Failed to read MCP gateway response")?;
        if body.len().saturating_add(chunk.len()) > limit {
            bail!("MCP gateway response exceeds {limit} bytes");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::McpGatewayClient;
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    const TEST_CA_PEM: &[u8] = include_bytes!("../../../apps/light-gateway/config/ca.pem");

    async fn spawn_test_server(response: String) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_clone = Arc::clone(&captured);

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = Vec::new();
                loop {
                    let mut chunk = [0u8; 4096];
                    let count = stream.read(&mut chunk).await.unwrap();
                    if count == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..count]);
                    if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buffer[..pos]);
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if buffer.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                let request = String::from_utf8(buffer).unwrap();
                *captured_clone.lock().await = request.clone();
                let json: Value =
                    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                let (_, body) = response.split_once("\r\n\r\n").unwrap();
                let mut fixture: Value = match serde_json::from_str(body) {
                    Ok(value) => value,
                    Err(_) => {
                        stream.write_all(response.as_bytes()).await.unwrap();
                        return;
                    }
                };
                if fixture.get("error").is_some() {
                    fixture["id"] = json["id"].clone();
                } else if json["method"] == "server/discover" {
                    fixture = serde_json::json!({"jsonrpc":"2.0","id":json["id"],"result":{"supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},"ttlMs":30000,"cacheScope":"private"}});
                } else if json["method"] == "tools/list" && fixture["result"].get("tools").is_none()
                {
                    fixture = serde_json::json!({"jsonrpc":"2.0","id":json["id"],"result":{"tools":[{"name":"listPets","inputSchema":{"type":"object"}}],"ttlMs":30000,"cacheScope":"private"}});
                } else {
                    fixture["id"] = json["id"].clone();
                    if fixture["result"].get("tools").is_some() {
                        fixture["result"]["ttlMs"] = serde_json::json!(30000);
                        fixture["result"]["cacheScope"] = serde_json::json!("private");
                    } else {
                        fixture["result"]["resultType"] = serde_json::json!("complete");
                    }
                }
                let finished = fixture.get("error").is_some()
                    || json["method"] == "tools/call"
                    || (json["method"] == "tools/list" && body.contains("\"tools\""));
                let reply =
                    http_response("HTTP/1.1 200 OK", "application/json", &fixture.to_string());
                stream.write_all(reply.as_bytes()).await.unwrap();
                if finished {
                    return;
                }
            }
        });

        (format!("http://{}", addr), captured)
    }

    fn http_response(status_line: &str, content_type: &str, body: &str) -> String {
        format!(
            "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn mcp_tls_accepts_ca_bundle_bytes() {
        let mut bundle = Vec::from(TEST_CA_PEM);
        bundle.extend_from_slice(TEST_CA_PEM);

        let client =
            McpGatewayClient::with_tls_options("http://127.0.0.1", Some(&bundle), true, 1000);

        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn list_tools_sends_json_rpc_request_and_parses_tools() {
        let response = http_response(
            "HTTP/1.1 200 OK",
            "application/json",
            "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"tools\":[{\"name\":\"search\",\"description\":\"Search docs\",\"inputSchema\":{\"type\":\"object\"}}]}}",
        );
        let (url, captured) = spawn_test_server(response).await;
        let client = McpGatewayClient::new(&url).unwrap();

        let tools = client.list_tools(Some("Bearer test-token")).await.unwrap();

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");
        assert_eq!(tools[0].description, "Search docs");
        assert_eq!(tools[0].input_schema["type"], "object");

        let request = captured.lock().await.clone();
        assert!(request.contains("authorization: Bearer test-token"));
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let json: Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["method"], "tools/list");
        assert_eq!(
            json["params"]["_meta"][crate::wire::VERSION_META],
            crate::wire::MODERN_VERSION
        );
        assert!(json["id"].is_string());
    }

    #[tokio::test]
    async fn call_tool_parses_content_result() {
        let response = http_response(
            "HTTP/1.1 200 OK",
            "application/json",
            "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"[{\\\"id\\\":1}]\"}],\"structuredContent\":[{\"id\":1}]}}",
        );
        let (url, captured) = spawn_test_server(response).await;
        let client = McpGatewayClient::new(&url).unwrap();

        let result = client
            .call_tool(None, "listPets", serde_json::json!({"limit": 1}))
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        match &result.content[0] {
            crate::protocol::McpContent::Text { text, .. } => {
                assert_eq!(text, "[{\"id\":1}]");
            }
            _ => panic!("expected text content"),
        }

        let request = captured.lock().await.clone();
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let json: Value = serde_json::from_str(body).unwrap();
        assert_eq!(json["method"], "tools/call");
        assert_eq!(json["params"]["name"], "listPets");
        assert_eq!(json["params"]["arguments"]["limit"], 1);
    }

    #[tokio::test]
    async fn returns_http_errors() {
        let response = http_response("HTTP/1.1 502 Bad Gateway", "text/plain", "bad gateway");
        let (url, _) = spawn_test_server(response).await;
        let client = McpGatewayClient::new(&url).unwrap();

        let error = client.list_tools(None).await.unwrap_err().to_string();

        assert!(error.contains("HTTP 502"));
        assert!(!error.contains("bad gateway"));
    }

    #[tokio::test]
    async fn rejects_response_larger_than_configured_limit() {
        let body = "x".repeat(256);
        let response = http_response("HTTP/1.1 200 OK", "application/json", &body);
        let (url, _) = spawn_test_server(response).await;
        let client =
            McpGatewayClient::with_tls_options_and_response_limit(&url, None, true, 1_000, 128)
                .unwrap();

        let error = client.list_tools(None).await.unwrap_err();

        assert!(error.to_string().contains("response exceeds 128 bytes"));
    }

    #[tokio::test]
    async fn returns_json_rpc_errors() {
        let response = http_response(
            "HTTP/1.1 200 OK",
            "application/json",
            "{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}",
        );
        let (url, _) = spawn_test_server(response).await;
        let client = McpGatewayClient::new(&url).unwrap();

        let error = client
            .call_tool(None, "missing", serde_json::json!({}))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("MCP error (-32601): Method not found"));
    }
}

#[cfg(test)]
mod review_stream_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn incremental_sse_rejects_invalid_envelopes_and_bounded_incomplete_streams() {
        for (body, limit) in [
            (
                "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n",
                4096,
            ),
            (
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{},\"error\":{}}\n\n",
                4096,
            ),
            (
                "data: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"sampling/createMessage\"}\n\n",
                4096,
            ),
            (
                "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
                4096,
            ),
            ("data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}", 4096),
            ("data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n", 10),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",body.len(),body).as_bytes()).await.unwrap();
            });
            let response = reqwest::get(format!("http://{address}")).await.unwrap();
            assert!(
                read_rpc_response(response, limit, "text/event-stream", &json!(1))
                    .await
                    .is_err()
            );
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn completes_sse_without_waiting_for_eof() {
        for ending in ["\n", "\r\n", "\r"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let ending = ending.to_string();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                socket.read(&mut request).await.unwrap();
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
                let event = format!(
                    "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}}{ending}{ending}data: {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}{ending}{ending}"
                );
                for b in event.bytes() {
                    socket
                        .write_all(format!("1\r\n{}\r\n", b as char).as_bytes())
                        .await
                        .unwrap();
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
            let response = reqwest::get(format!("http://{address}")).await.unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                read_rpc_response(response, 4096, "text/event-stream", &json!(1)),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(result["result"], json!({}));
            task.abort();
        }
    }
}

#[cfg(test)]
mod partition_review_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn idle_partitions_are_reclaimed_without_evicting_active_operations() {
        let client = McpGatewayClient::new("http://127.0.0.1:1/mcp").unwrap();
        let active = client.state(Some("active")).await.unwrap();
        for i in 0..100 {
            drop(client.state(Some(&format!("token-{i}"))).await.unwrap());
        }
        assert_eq!(client.states.lock().await.len(), 32);
        assert!(Arc::ptr_eq(
            &active,
            &client.state(Some("active")).await.unwrap()
        ));
        let mut busy = vec![active];
        for i in 0..31 {
            busy.push(client.state(Some(&format!("busy-{i}"))).await.unwrap());
        }
        assert!(client.state(Some("new-token")).await.is_err());
        drop(busy.pop());
        assert!(client.state(Some("new-token")).await.is_ok());
        assert_eq!(client.states.lock().await.len(), 32);
    }

    #[tokio::test]
    async fn different_credentials_have_overlapping_network_requests() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut sockets = Vec::new();
            // Neither response is sent until BOTH HTTP requests have arrived.
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let request = loop {
                    let mut buffer = [0; 4096];
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&buffer[..n]);
                    if let Some(pos) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        if let Ok(value) = serde_json::from_slice::<Value>(&bytes[pos + 4..]) {
                            break value;
                        }
                    }
                };
                sockets.push((socket, request));
            }
            for (mut socket, request) in sockets {
                let body = json!({"jsonrpc":"2.0","id":request["id"],"result":{"capabilities":{},"supportedVersions":[wire::MODERN_VERSION],"cacheScope":"private","ttlMs":1000}}).to_string();
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let client = McpGatewayClient::new(&format!("http://{address}/mcp")).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let (a, b) = tokio::join!(
                client.discover(Some("Bearer alice")),
                client.discover(Some("Bearer bob"))
            );
            a.unwrap();
            b.unwrap();
        })
        .await
        .expect("credentials must not serialize their HTTP exchanges");
        server.await.unwrap();
    }
}
