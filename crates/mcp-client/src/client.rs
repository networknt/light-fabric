use crate::protocol::{JsonRpcRequest, JsonRpcResponse, McpTool, McpToolsListResult, McpToolCallResult};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::Client;
use serde_json::json;

pub struct McpGatewayClient {
    url: String,
    client: Client,
}

impl McpGatewayClient {
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            client: Client::new(),
        }
    }

    pub async fn list_tools(&self, auth_header: Option<&str>) -> Result<Vec<McpTool>> {
        let req = JsonRpcRequest::new("tools/list", json!({}));
        let resp = self.post(auth_header, req).await?;
        
        let result = resp.result.ok_or_else(|| anyhow!("No result in tools/list response"))?;
        let tools_list: McpToolsListResult = serde_json::from_value(result)
            .context("Failed to parse tools/list result")?;
        
        Ok(tools_list.tools)
    }

    pub async fn call_tool(
        &self,
        auth_header: Option<&str>,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<McpToolCallResult> {
        let req = JsonRpcRequest::new("tools/call", json!({
            "name": name,
            "arguments": arguments
        }));
        let resp = self.post(auth_header, req).await?;
        
        let result = resp.result.ok_or_else(|| anyhow!("No result in tools/call response"))?;
        let call_result: McpToolCallResult = serde_json::from_value(result)
            .context("Failed to parse tools/call result")?;
        
        Ok(call_result)
    }

    async fn post(&self, auth_header: Option<&str>, request: JsonRpcRequest) -> Result<JsonRpcResponse> {
        let mut builder = self.client.post(&self.url).json(&request);
        
        if let Some(auth) = auth_header {
            builder = builder.header("Authorization", auth);
        }

        let resp = builder.send().await.context("HTTP request to MCP gateway failed")?;
        
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP gateway returned HTTP {}: {}", status, body);
        }

        let rpc_resp: JsonRpcResponse = resp.json().await.context("Failed to parse JSON-RPC response")?;
        
        if let Some(err) = rpc_resp.error {
            bail!("MCP error ({}): {}", err.code, err.message);
        }

        Ok(rpc_resp)
    }
}
