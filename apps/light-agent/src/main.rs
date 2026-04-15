use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{State, ws::{Message, WebSocket, WebSocketUpgrade}},
    routing::get,
    response::IntoResponse,
};
use tower_http::services::ServeDir;
use futures_util::{SinkExt, StreamExt};
use light_runtime::LightRuntimeBuilder;
use light_axum::{AxumApp, AxumTransport, ServerContext};
use config_loader::ConfigLoader;
use model_provider::{OllamaProvider, Provider, ChatMessage, ChatRequest, ToolSpec, ChatResponse};
use mcp_client::{McpGatewayClient, McpContent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, error, warn};
use tracing_subscriber::EnvFilter;

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
    pub timeout: u64,
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
            .route("/ws", get(ws_handler))
            .fallback_service(ServeDir::new("public").append_index_html_on_directories(true))
            .with_state(self.state.clone())
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AgentState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

#[derive(Debug, Deserialize)]
struct ClientMessage {
    pub session_id: Option<String>,
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

async fn handle_socket(socket: WebSocket, state: Arc<AgentState>) {
    let (mut sender, mut receiver) = socket.split();
    let mut current_session_id: Option<String> = None;

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let client_msg: ClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    let _ = sender.send(Message::Text(serde_json::to_string(&ServerMessage::Error {
                        message: format!("Invalid message format: {}", e),
                    }).unwrap().into())).await;
                    continue;
                }
            };

            let session_id = client_msg.session_id.unwrap_or_else(|| {
                uuid::Uuid::new_v4().to_string()
            });

            if current_session_id.is_none() {
                current_session_id = Some(session_id.clone());
                let _ = sender.send(Message::Text(serde_json::to_string(&ServerMessage::Session {
                    session_id: session_id.clone(),
                }).unwrap().into())).await;
            }

            let mut history_guard = state.sessions.lock().await;
            let history = history_guard.entry(session_id.clone()).or_insert_with(Vec::new);
            history.push(ChatMessage::user(client_msg.text));
            let messages = history.clone();
            drop(history_guard);

            match run_agent_loop(&state, messages).await {
                Ok(response) => {
                    if let Some(text) = response.text {
                        let mut history_guard = state.sessions.lock().await;
                        if let Some(h) = history_guard.get_mut(&session_id) {
                            h.push(ChatMessage::assistant(text.clone()));
                        }
                        let _ = sender.send(Message::Text(serde_json::to_string(&ServerMessage::Text {
                            text,
                        }).unwrap().into())).await;
                    }
                }
                Err(e) => {
                    error!("Agent loop error: {}", e);
                    let _ = sender.send(Message::Text(serde_json::to_string(&ServerMessage::Error {
                        message: format!("Error: {}", e),
                    }).unwrap().into())).await;
                }
            }
        }
    }
}

async fn run_agent_loop(state: &AgentState, mut messages: Vec<ChatMessage>) -> Result<ChatResponse> {
    // 1. Fetch tools from MCP Gateway
    let mcp_tools = state.mcp_client.list_tools(None).await.unwrap_or_default();
    let tool_specs: Vec<ToolSpec> = mcp_tools.into_iter().map(|t| ToolSpec {
        name: t.name,
        description: t.description,
        parameters: t.input_schema,
    }).collect();

    // 2. Loop until we have a final text response (max 10 iterations)
    for _ in 0..10 {
        let request = ChatRequest {
            messages: &messages,
            tools: if tool_specs.is_empty() { None } else { Some(&tool_specs) },
        };

        let response = state.provider.chat(request, &state.ollama_config.model, 0.7).await?;

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
            })).unwrap(),
        };
        messages.push(assistant_msg);

        for tool_call in &response.tool_calls {
            let args: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or_default();
            match state.mcp_client.call_tool(None, &tool_call.name, args).await {
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
                        })).unwrap(),
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
                        })).unwrap(),
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

    let mcp_gateway_url = format!("{}{}", mcp_config.gateway_url, mcp_config.path);

    let state = Arc::new(AgentState {
        provider: OllamaProvider::new(Some(&ollama_config.ollama_url), None),
        mcp_client: McpGatewayClient::new(&mcp_gateway_url),
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
