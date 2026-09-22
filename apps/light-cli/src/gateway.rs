//! Connect to `light-gateway`'s `/mcp` endpoint as a public client: HTTPS, and the signed-in
//! user's access token as `authorization: Bearer ...`. Nothing else identifies the caller: no
//! client certificate and no application token on this call. The CLI is open and downloadable, so
//! it cannot keep a secret; what the caller may do is decided by the user's own roles and the route's ACL.
//!
//! The request sequence is the one the Portal's own MCP client uses: `initialize`
//! (protocol `2025-03-26`), keep the `mcp-session-id` it returns, send the
//! `notifications/initialized` notification the lifecycle requires, then `tools/list`.

use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::config::{CliConfig, Secret, ensure_transport_is_safe};
use crate::error::CliError;
use crate::http;
use crate::remote;

const PROTOCOL_VERSION: &str = "2025-03-26";

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

/// Every error in the chain, so a TLS failure says what actually failed.
fn describe(error: &reqwest::Error) -> String {
    let mut text = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        text.push_str(": ");
        text.push_str(&inner.to_string());
        source = inner.source();
    }
    text
}

fn transport_failure(endpoint: &str, error: reqwest::Error) -> CliError {
    let detail = describe(&error.without_url());
    let lower = detail.to_ascii_lowercase();
    let hint = if lower.contains("certificate")
        || lower.contains("handshake")
        || lower.contains("alert")
    {
        " (a TLS failure: check that this CLI trusts the Gateway's certificate via bootstrapCaCertPath)"
    } else {
        ""
    };
    CliError::Unreachable(format!("{endpoint}: {detail}{hint}"))
}

struct Call<'a> {
    client: &'a reqwest::Client,
    endpoint: &'a str,
    user_token: &'a Secret,
}

/// A non-success HTTP status from the Gateway, as the error the caller should see.
fn refusal(status: reqwest::StatusCode, text: &str) -> CliError {
    let detail: String = text.trim().chars().take(200).collect();
    match status.as_u16() {
        401 | 403 => CliError::Denied(format!(
            "the Gateway refused the call (HTTP {status}): {detail}"
        )),
        500..=599 => CliError::Unreachable(format!("the Gateway failed (HTTP {status}): {detail}")),
        _ => CliError::Failed(format!(
            "the Gateway rejected the request (HTTP {status}): {detail}"
        )),
    }
}

impl Call<'_> {
    /// Send a JSON-RPC notification (no `id`, so no reply body). The server answers `202 Accepted`;
    /// any success status is taken as accepted and the body is ignored.
    async fn notify(&self, session: &str, method: &str) -> Result<(), CliError> {
        let response = self
            .client
            .post(self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .header(
                "authorization",
                format!("Bearer {}", self.user_token.expose()),
            )
            .header("mcp-session-id", session)
            .json(&json!({"jsonrpc": "2.0", "method": method}))
            .send()
            .await
            .map_err(|e| transport_failure(self.endpoint, e))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(refusal(status, &text))
    }

    async fn rpc(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(Value, Option<String>), CliError> {
        let id = Uuid::new_v4().to_string();
        let mut request = self
            .client
            .post(self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", PROTOCOL_VERSION)
            .header(
                "authorization",
                format!("Bearer {}", self.user_token.expose()),
            );
        if let Some(session) = session {
            request = request.header("mcp-session-id", session);
        }
        let response = request
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .send()
            .await
            .map_err(|e| transport_failure(self.endpoint, e))?;

        let status = response.status();
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.map_err(|e| {
            CliError::Unreachable(format!("reading the Gateway's reply: {}", e.without_url()))
        })?;

        if !status.is_success() {
            return Err(refusal(status, &text));
        }
        let body: Value = serde_json::from_str(&text).map_err(|_| {
            CliError::Failed(
                "expected a JSON reply from the Gateway; check that this is the /mcp route".into(),
            )
        })?;
        if body.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
            || body.get("id").and_then(Value::as_str) != Some(&id)
        {
            return Err(CliError::Failed(
                "the Gateway's reply did not match the request".into(),
            ));
        }
        if let Some(error) = body.get("error") {
            let code = error
                .get("code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("(no message)");
            return Err(if code == -32001 {
                CliError::Denied(format!("Gateway RPC {code}: {message}"))
            } else {
                CliError::Failed(format!("Gateway RPC {code}: {message}"))
            });
        }
        Ok((
            body.get("result").cloned().unwrap_or(Value::Null),
            session_id,
        ))
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

    let client = http::client(
        config.ca_bundle.as_deref(),
        Duration::from_secs(5),
        Duration::from_secs(30),
    )?;

    let call = Call {
        client: &client,
        endpoint: &endpoint,
        user_token: &user_token,
    };

    let (initialized, session) = call
        .rpc(
            None,
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "light-cli", "version": env!("CARGO_PKG_VERSION")},
            }),
        )
        .await?;
    let protocol_version = initialized
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if protocol_version != PROTOCOL_VERSION {
        return Err(CliError::Failed(format!(
            "the Gateway speaks MCP {protocol_version:?}, not {PROTOCOL_VERSION}"
        )));
    }
    let session =
        session.ok_or_else(|| CliError::Failed("the Gateway returned no MCP session".into()))?;

    call.notify(&session, "notifications/initialized").await?;

    let (listed, _) = call.rpc(Some(&session), "tools/list", json!({})).await?;
    let tools: Vec<String> = listed
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Ok(GatewayReport {
        endpoint,
        protocol_version,
        session: true,
        server: initialized
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
        tool_count: tools.len(),
        tools,
    })
}
