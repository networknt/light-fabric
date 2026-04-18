use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::IntoResponse,
    routing::get,
};
use config_loader::ConfigLoader;
use futures_util::{SinkExt, StreamExt};
use light_axum::{AxumApp, AxumTransport, ServerContext};
use light_runtime::{
    LightRuntimeBuilder,
    config::{BootstrapConfig, ClientConfig},
};
use mcp_client::{McpContent, McpGatewayClient};
use model_provider::{ChatMessage, ChatRequest, ChatResponse, OllamaProvider, Provider, ToolSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

const MAX_SESSION_MESSAGES: usize = 40;
const MAX_ACTIVE_SESSIONS: usize = 256;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OllamaConfig {
    pub ollama_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpClientConfig {
    pub gateway_url: String,
    pub path: String,
    pub timeout_ms: u64,
}

struct AgentState {
    ollama_config: OllamaConfig,
    provider: OllamaProvider,
    mcp_client: McpGatewayClient,
    sessions: Arc<Mutex<HashMap<String, Vec<ChatMessage>>>>,
}

#[derive(Clone)]
struct AgentApp {
    state: Arc<AgentState>,
}

impl AxumApp for AgentApp {
    fn router(&self, _context: ServerContext) -> Router {
        Router::new()
            .route("/health", get(health))
            .route("/chat", get(ws_handler))
            .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
            .with_state(self.state.clone())
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<Arc<AgentState>>,
) -> impl IntoResponse {
    let session_id = params.get("sessionId").cloned();
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id, authorization))
}

#[derive(Debug, Deserialize)]
struct ClientMessage {
    pub text: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ServerMessage {
    #[serde(rename = "session")]
    Session { session_id: String },
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "error")]
    Error { message: String },
}

fn trim_history(history: &mut Vec<ChatMessage>) {
    let excess = history.len().saturating_sub(MAX_SESSION_MESSAGES);
    if excess > 0 {
        history.drain(0..excess);
    }
}

fn rollback_last_user_message(history: &mut Vec<ChatMessage>, expected_text: &str) {
    if history
        .last()
        .is_some_and(|message| message.role == "user" && message.content == expected_text)
    {
        history.pop();
    }
}

async fn handle_socket(
    socket: WebSocket,
    state: Arc<AgentState>,
    initial_session_id: Option<String>,
    authorization: Option<String>,
) {
    let (mut sender, mut receiver) = socket.split();

    // Immediate Session Initialization
    let session_id = initial_session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let _ = sender
        .send(Message::Text(
            serde_json::to_string(&ServerMessage::Session {
                session_id: session_id.clone(),
            })
            .unwrap()
            .into(),
        ))
        .await;

    let current_session_id: String = session_id;

    {
        let mut sessions = state.sessions.lock().await;
        if !sessions.contains_key(&current_session_id) {
            if sessions.len() >= MAX_ACTIVE_SESSIONS {
                let _ = sender
                    .send(Message::Text(
                        serde_json::to_string(&ServerMessage::Error {
                            message: format!(
                                "Too many active sessions. Limit is {}.",
                                MAX_ACTIVE_SESSIONS
                            ),
                        })
                        .unwrap()
                        .into(),
                    ))
                    .await;
                return;
            }
            sessions.insert(current_session_id.clone(), Vec::new());
        }
    }

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let client_msg: ClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    let _ = sender
                        .send(Message::Text(
                            serde_json::to_string(&ServerMessage::Error {
                                message: format!("Invalid message format: {}", e),
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await;
                    continue;
                }
            };

            let session_id = current_session_id.clone();
            let user_text = client_msg.text.clone();

            let mut history_guard = state.sessions.lock().await;
            let history = history_guard
                .entry(session_id.clone())
                .or_insert_with(Vec::new);
            history.push(ChatMessage::user(user_text.clone()));
            trim_history(history);
            let messages = history.clone();
            drop(history_guard);

            match run_agent_loop(&state, messages, authorization.as_deref()).await {
                Ok(response) => {
                    if let Some(text) = response.text {
                        let mut history_guard = state.sessions.lock().await;
                        if let Some(h) = history_guard.get_mut(&session_id) {
                            h.push(ChatMessage::assistant(text.clone()));
                            trim_history(h);
                        }
                        let _ = sender
                            .send(Message::Text(
                                serde_json::to_string(&ServerMessage::Text { text })
                                    .unwrap()
                                    .into(),
                            ))
                            .await;
                    }
                }
                Err(e) => {
                    error!("Agent loop error: {}", e);
                    let mut history_guard = state.sessions.lock().await;
                    if let Some(history) = history_guard.get_mut(&session_id) {
                        rollback_last_user_message(history, &user_text);
                    }
                    drop(history_guard);
                    let _ = sender
                        .send(Message::Text(
                            serde_json::to_string(&ServerMessage::Error {
                                message: format!("Error: {}", e),
                            })
                            .unwrap()
                            .into(),
                        ))
                        .await;
                }
            }
        }
    }
    // Connection closed — remove session history to prevent unbounded memory growth.
    state.sessions.lock().await.remove(&current_session_id);
}

async fn run_agent_loop(
    state: &AgentState,
    mut messages: Vec<ChatMessage>,
    authorization: Option<&str>,
) -> Result<ChatResponse> {
    // 1. Fetch tools from MCP Gateway (forward Authorization if present)
    let mcp_tools = state
        .mcp_client
        .list_tools(authorization)
        .await
        .unwrap_or_else(|e| {
            warn!("Failed to fetch MCP tools: {}", e);
            vec![]
        });
    info!("Fetched {} MCP tool(s) from gateway", mcp_tools.len());
    let tool_specs: Vec<ToolSpec> = mcp_tools
        .into_iter()
        .map(|t| ToolSpec {
            name: t.name,
            description: t.description,
            parameters: t.input_schema,
        })
        .collect();

    // 2. Loop until we have a final text response (max 10 iterations)
    for _ in 0..10 {
        let request = ChatRequest {
            messages: &messages,
            tools: if tool_specs.is_empty() {
                None
            } else {
                Some(&tool_specs)
            },
        };

        let response = state
            .provider
            .chat(request, &state.ollama_config.model, 0.7)
            .await?;

        if response.tool_calls.is_empty() {
            return Ok(response);
        }

        // Handle tool calls
        info!("Executing {} tool call(s)", response.tool_calls.len());

        // Add assistant message with tool calls to history
        let assistant_msg = ChatMessage {
            role: "assistant".into(),
            content: serde_json::to_string(&serde_json::json!({
                "tool_calls": response.tool_calls
            }))
            .unwrap(),
        };
        messages.push(assistant_msg);

        for tool_call in &response.tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&tool_call.arguments).unwrap_or_default();
            match state
                .mcp_client
                .call_tool(authorization, &tool_call.name, args)
                .await
            {
                Ok(result) => {
                    let mut text_result = String::new();
                    for content in result.content {
                        if let McpContent::Text { text } = content {
                            text_result.push_str(&text);
                        }
                    }
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: serde_json::to_string(&serde_json::json!({
                            "tool_call_id": tool_call.id,
                            "tool_name": tool_call.name,
                            "content": text_result
                        }))
                        .unwrap(),
                    });
                }
                Err(e) => {
                    warn!("Tool call failed: {}", e);
                    messages.push(ChatMessage {
                        role: "tool".into(),
                        content: serde_json::to_string(&serde_json::json!({
                            "tool_call_id": tool_call.id,
                            "tool_name": tool_call.name,
                            "content": format!("Error: {}", e)
                        }))
                        .unwrap(),
                    });
                }
            }
        }
    }

    Err(anyhow::anyhow!("Max iterations reached in agent loop"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_dir = PathBuf::from("config");
    let values_path = config_dir.join("values.yml");
    let values_yaml = std::fs::read_to_string(&values_path).unwrap_or_default();
    let loader = ConfigLoader::new(&values_yaml, None, None)?;

    let ollama_config: OllamaConfig = loader.load_typed([config_dir.join("ollama.yml")])?;
    let mcp_config: McpClientConfig = loader.load_typed([config_dir.join("mcp-client.yml")])?;

    // Load startup.yml (for bootstrap_ca_cert_path) and client.yml (for verify_hostname).
    // This mirrors how the config-server and controller-rs clients are configured in light-runtime.
    let startup_config: BootstrapConfig = loader
        .load_typed([config_dir.join("startup.yml")])
        .unwrap_or_default();
    let client_config: Option<ClientConfig> =
        loader.load_typed([config_dir.join("client.yml")]).ok();

    let mcp_gateway_url = format!(
        "{}/{}",
        mcp_config.gateway_url.trim_end_matches('/'),
        mcp_config.path.trim_start_matches('/')
    );

    // Load TLS settings from the shared config files, consistent with how the
    // config-server and controller-rs clients are built by light-runtime.
    let ca_cert: Option<Vec<u8>> = startup_config
        .bootstrap_ca_cert_path
        .as_deref()
        .and_then(|path| std::fs::read(path).ok());
    let verify_hostname: bool = client_config
        .as_ref()
        .map(|c| c.verify_hostname)
        .unwrap_or(true);
    if !verify_hostname {
        warn!(
            "TLS hostname verification is disabled for the MCP gateway client; this weakens server identity validation"
        );
    }

    let mcp_client = McpGatewayClient::with_options(
        &mcp_gateway_url,
        ca_cert.as_deref(),
        verify_hostname,
        mcp_config.timeout_ms,
    )
    .context("Failed to build MCP gateway client")?;

    let state = Arc::new(AgentState {
        provider: OllamaProvider::new(Some(&ollama_config.ollama_url), None)
            .context("Failed to build Ollama provider")?,
        mcp_client,
        ollama_config,
        sessions: Arc::new(Mutex::new(HashMap::new())),
    });

    let app = AgentApp { state };

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_config_dir("config")
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start agent runtime")?;

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    running
        .shutdown()
        .await
        .context("failed to shut down agent")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ChatMessage, MAX_SESSION_MESSAGES, rollback_last_user_message, trim_history};

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

    #[test]
    fn rollback_last_user_message_removes_failed_turn() {
        let mut history = vec![
            ChatMessage::assistant("existing reply"),
            ChatMessage::user("failed prompt"),
        ];

        rollback_last_user_message(&mut history, "failed prompt");

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, "assistant");
    }

    #[test]
    fn rollback_last_user_message_leaves_other_entries_untouched() {
        let mut history = vec![
            ChatMessage::user("previous prompt"),
            ChatMessage::assistant("previous reply"),
        ];

        rollback_last_user_message(&mut history, "failed prompt");

        assert_eq!(history.len(), 2);
        assert_eq!(history[1].content, "previous reply");
    }
}
