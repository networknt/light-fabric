//! Connect to `light-gateway`'s `/mcp` endpoint as a public client: HTTPS, and the signed-in
//! user's access token as `authorization: Bearer ...`. Nothing else identifies the caller: no
//! client certificate and no application token on this call. The CLI is open and downloadable, so
//! it cannot keep a secret; what the caller may do is decided by the user's own roles and the route's ACL.
//!
//! Each operation uses the sessionless MCP 2026-07-28 profile. There is no
//! initialize exchange, session identifier, affinity requirement, or legacy fallback.

use mcp_client::McpGatewayClient;
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

use crate::config::{CliConfig, Secret, ensure_transport_is_safe};
use crate::error::CliError;
use crate::remote;

const PROTOCOL_VERSION: &str = "2026-07-28";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayReport {
    pub endpoint: String,
    pub protocol_version: String,
    pub session: bool,
    pub server: Option<String>,
    pub tool_count: usize,
    pub tools: Vec<String>,
}

fn structured_tool_result(result: Value) -> Result<Value, CliError> {
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        let detail = result
            .get("content")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find_map(|item| item.get("text").and_then(Value::as_str))
            })
            .unwrap_or("Workflow operation failed");
        return Err(CliError::Failed(detail.to_string()));
    }
    if let Some(value) = result.get("structuredContent") {
        return Ok(value.clone());
    }
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
        .ok_or_else(|| CliError::Failed("Gateway returned no structured tool result".into()))?;
    serde_json::from_str(text)
        .map_err(|_| CliError::Failed("Gateway returned an invalid JSON tool result".into()))
}

fn client(config: &CliConfig, endpoint: &str) -> Result<McpGatewayClient, CliError> {
    let ca = config
        .ca_bundle
        .as_ref()
        .map(std::fs::read)
        .transpose()
        .map_err(|e| CliError::Config(format!("reading Gateway CA bundle: {e}")))?;
    McpGatewayClient::with_tls_options_and_response_limit(
        endpoint,
        ca.as_deref(),
        true,
        30_000,
        2 * 1024 * 1024,
    )
    .map_err(|e| CliError::Config(format!("creating stateless Gateway client: {e}")))
}

fn gateway_error(context: &str, error: anyhow::Error) -> CliError {
    let detail = format!("{error:#}");
    if detail.contains("HTTP 401")
        || detail.contains("HTTP 403")
        || detail.contains("MCP error (-32001)")
    {
        CliError::Denied(format!("{context}: {detail}"))
    } else if detail.contains("HTTP 500")
        || detail.contains("HTTP 502")
        || detail.contains("HTTP 503")
        || detail.contains("HTTP 504")
        || error
            .chain()
            .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
    {
        let hint = if detail.to_ascii_lowercase().contains("certificate") {
            " (a TLS failure: check bootstrapCaCertPath)"
        } else {
            ""
        };
        CliError::Unreachable(format!("{context}: {detail}{hint}"))
    } else {
        CliError::Failed(format!("{context}: {detail}"))
    }
}

/// Connect to the Gateway with the user's access token and list its tools.
///
/// There is nothing to send without a user, so no token is a "sign in first" error and the
/// Gateway is not called.
pub async fn check(
    config: &CliConfig,
    user_token: Option<Secret>,
) -> Result<GatewayReport, CliError> {
    let user_token = user_token.ok_or_else(|| {
        CliError::LoginRequired("no user login to send to the Gateway; run `/login`".into())
    })?;
    let (settings, from) = remote::load_settings(config).await?;
    let base = settings.gateway_uri.ok_or_else(|| {
        CliError::Config(
            "no Gateway URL: set cli.gatewayUri in cli.yml, the config server, or CLI_GATEWAYURI"
                .into(),
        )
    })?;
    ensure_transport_is_safe(&base)?;
    let endpoint = format!("{base}/mcp");
    eprintln!(
        "gateway: {base} ({})",
        remote::origin_of(&from, "cli.gatewayUri")
    );

    let client = client(config, &endpoint)?;
    let authorization = format!("Bearer {}", user_token.expose());
    let discovered = client
        .discover(Some(&authorization))
        .await
        .map_err(|e| gateway_error("Gateway stateless discovery failed", e))?;
    let tools: Vec<String> = client
        .list_tools(Some(&authorization))
        .await
        .map_err(|e| gateway_error("Gateway stateless tool discovery failed", e))?
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    Ok(GatewayReport {
        endpoint,
        protocol_version: PROTOCOL_VERSION.into(),
        session: false,
        server: discovered
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_count: tools.len(),
        tools,
    })
}

/// Invoke one authorized Gateway tool through the CLI's explicitly retained restoration profile.
pub async fn invoke(
    config: &CliConfig,
    user_token: Option<Secret>,
    tool: &str,
    arguments: Value,
) -> Result<Value, CliError> {
    let user_token = user_token.ok_or_else(|| {
        CliError::LoginRequired("no user login to send to the Gateway; run `/login`".into())
    })?;
    let (settings, _) = remote::load_settings(config).await?;
    let base = settings.gateway_uri.ok_or_else(|| {
        CliError::Config(
            "no Gateway URL: set cli.gatewayUri in cli.yml, the config server, or CLI_GATEWAYURI"
                .into(),
        )
    })?;
    ensure_transport_is_safe(&base)?;
    let endpoint = format!("{base}/mcp");
    let client = client(config, &endpoint)?;
    let authorization = format!("Bearer {}", user_token.expose());
    let result = client
        .call_tool(Some(&authorization), tool, arguments)
        .await
        .map_err(|e| gateway_error("Gateway stateless tool call failed", e))?;
    structured_tool_result(
        serde_json::to_value(result).map_err(|e| CliError::Failed(e.to_string()))?,
    )
}

#[cfg(test)]
mod tool_result_tests {
    use super::*;

    #[test]
    fn structured_and_text_tool_results_are_supported() {
        assert_eq!(
            structured_tool_result(json!({"structuredContent":{"ok":true}})).unwrap(),
            json!({"ok":true})
        );
        assert_eq!(
            structured_tool_result(json!({"content":[{"type":"text","text":"{\"ok\":true}"}]}))
                .unwrap(),
            json!({"ok":true})
        );
        assert!(
            structured_tool_result(
                json!({"isError":true,"content":[{"type":"text","text":"denied"}]})
            )
            .is_err()
        );
    }

    #[test]
    fn client_errors_are_not_reported_as_unreachable() {
        let bad_request = gateway_error(
            "call failed",
            anyhow::anyhow!("MCP gateway returned HTTP 400 Bad Request"),
        );
        assert!(matches!(bad_request, CliError::Failed(_)));
        let unavailable = gateway_error(
            "call failed",
            anyhow::anyhow!("MCP gateway returned HTTP 503 Service Unavailable"),
        );
        assert!(matches!(unavailable, CliError::Unreachable(_)));
    }
}
