use crate::protocol::{
    JsonRpcRequest, JsonRpcResponse, McpTool, McpToolCallResult, McpToolsListResult,
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde_json::json;
use tracing::debug;

pub struct McpGatewayClient {
    url: String,
    client: Client,
}

impl McpGatewayClient {
    pub fn new(url: &str) -> Result<Self> {
        Self::with_options(url, None, true, 30_000)
    }

    /// Create a client with explicit TLS options.
    ///
    /// - `ca_cert_pem`: PEM-encoded CA certificate to trust (e.g. loaded from `config/ca.pem`).
    /// - `verify_hostname`: When `false`, hostname verification is skipped but the certificate
    ///   chain is still validated against `ca_cert_pem` (mirrors the config-server client behaviour).
    pub fn with_options(
        url: &str,
        ca_cert_pem: Option<&[u8]>,
        verify_hostname: bool,
        timeout_ms: u64,
    ) -> Result<Self> {
        let mut builder = Client::builder();
        builder = builder
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

        let client = builder.build().context("Failed to build reqwest Client")?;
        Ok(Self {
            url: url.to_string(),
            client,
        })
    }

    pub async fn list_tools(&self, auth_header: Option<&str>) -> Result<Vec<McpTool>> {
        let req = JsonRpcRequest::new("tools/list", json!({}));
        let resp = self.post(auth_header, req).await?;

        let result = resp
            .result
            .ok_or_else(|| anyhow!("No result in tools/list response"))?;
        let tools_list: McpToolsListResult =
            serde_json::from_value(result).context("Failed to parse tools/list result")?;

        Ok(tools_list.tools)
    }

    pub async fn call_tool(
        &self,
        auth_header: Option<&str>,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolCallResult> {
        let req = JsonRpcRequest::new(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
        );
        let resp = self.post(auth_header, req).await?;

        let result = resp
            .result
            .ok_or_else(|| anyhow!("No result in tools/call response"))?;
        let call_result: McpToolCallResult =
            serde_json::from_value(result).context("Failed to parse tools/call result")?;

        Ok(call_result)
    }

    async fn post(
        &self,
        auth_header: Option<&str>,
        request: JsonRpcRequest,
    ) -> Result<JsonRpcResponse> {
        debug!(method = %request.method, url = %self.url, "Sending MCP JSON-RPC request");
        let mut builder = self.client.post(&self.url).json(&request);

        if let Some(auth) = auth_header {
            builder = builder.header("Authorization", auth);
        }

        let resp = builder
            .send()
            .await
            .context("HTTP request to MCP gateway failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP gateway returned HTTP {}: {}", status, body);
        }

        let rpc_resp: JsonRpcResponse = resp
            .json()
            .await
            .context("Failed to parse JSON-RPC response")?;

        if let Some(err) = rpc_resp.error {
            bail!("MCP error ({}): {}", err.code, err.message);
        }

        Ok(rpc_resp)
    }
}

#[cfg(test)]
mod tests {
    use super::McpGatewayClient;
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    async fn spawn_test_server(response: String) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(Mutex::new(String::new()));
        let captured_clone = Arc::clone(&captured);

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = vec![0u8; 8192];
            let bytes_read = stream.read(&mut buffer).await.unwrap();
            *captured_clone.lock().await = String::from_utf8_lossy(&buffer[..bytes_read]).into();
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{}", addr), captured)
    }

    fn http_response(status_line: &str, content_type: &str, body: &str) -> String {
        format!(
            "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
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
        assert_eq!(json["params"], serde_json::json!({}));
        assert!(json["id"].is_string());
    }

    #[tokio::test]
    async fn returns_http_errors() {
        let response = http_response("HTTP/1.1 502 Bad Gateway", "text/plain", "bad gateway");
        let (url, _) = spawn_test_server(response).await;
        let client = McpGatewayClient::new(&url).unwrap();

        let error = client.list_tools(None).await.unwrap_err().to_string();

        assert!(error.contains("HTTP 502"));
        assert!(error.contains("bad gateway"));
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
