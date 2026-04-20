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
use hindsight_client::{HindsightMemory, PgHindsightClient};
use portal_registry::{PortalRegistryClient, ServiceRegistrationParams, RegistryHandler};
use sqlx::{PgPool, Row};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;
use tracing::{error, warn};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

const MAX_SESSION_MESSAGES: usize = 40;

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

fn required_uuid_env_var(name: &str) -> anyhow::Result<Uuid> {
    let raw = std::env::var(name)
        .with_context(|| format!("Required environment variable {name} is not set"))?;
    Uuid::parse_str(&raw)
        .with_context(|| format!("Environment variable {name} must be a valid UUID"))
}

struct AgentState {
    ollama_config: OllamaConfig,
    provider: OllamaProvider,
    mcp_client: McpGatewayClient,
    memory: Arc<dyn HindsightMemory>,
    registry: Arc<PortalRegistryClient>,
    db: PgPool,
    host_id: Uuid,
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

    // 1. Load or Initialize Session
    let session_uuid = Uuid::parse_str(&current_session_id).unwrap_or_else(|_| Uuid::new_v4());
    let bank_id = session_uuid; // Using session as bank for simplicity

    let mut history = match sqlx::query(
        "SELECT messages FROM agent_session_history_t WHERE host_id = $1 AND session_id = $2"
    )
    .bind(state.host_id)
    .bind(session_uuid)
    .fetch_optional(&state.db)
    .await {
        Ok(Some(row)) => {
            let messages: serde_json::Value = row.get("messages");
            serde_json::from_value::<Vec<ChatMessage>>(messages).unwrap_or_default()
        },
        _ => Vec::new(),
    };

    while let Some(Ok(msg)) = receiver.next().await {
        if let Message::Text(text) = msg {
            let client_msg: ClientMessage = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(e) => {
                    match serde_json::to_string(&ServerMessage::Error { message: format!("Invalid message format: {}", e) }) {
                        Ok(payload) => {
                            let _ = sender.send(Message::Text(payload.into())).await;
                        }
                        Err(serialize_err) => {
                            error!("Failed to serialize server error message: {}", serialize_err);
                        }
                    }
                    continue;
                }
            };

            let user_text = client_msg.text.clone();
            history.push(ChatMessage::user(user_text.clone()));
            trim_history(&mut history);

            match run_agent_loop(
                &state,
                history.clone(),
                authorization.as_deref(),
                &current_session_id,
                bank_id,
            )
            .await
            {
                Ok(response) => {
                    if let Some(text) = response.text {
                        history.push(ChatMessage::assistant(text.clone()));
                        trim_history(&mut history);

                        match serde_json::to_value(&history) {
                            Ok(history_payload) => {
                                let _ = sqlx::query(
                                    "INSERT INTO agent_session_history_t (host_id, session_id, bank_id, messages)
                                    VALUES ($1, $2, $3, $4)
                                    ON CONFLICT (host_id, session_id) 
                                    DO UPDATE SET messages = $4, update_ts = CURRENT_TIMESTAMP"
                                )
                                .bind(state.host_id)
                                .bind(session_uuid)
                                .bind(bank_id)
                                .bind(history_payload)
                                .execute(&state.db)
                                .await;
                            }
                            Err(e) => {
                                error!("Failed to serialize session history: {}", e);
                            }
                        }

                        match serde_json::to_string(&ServerMessage::Text { text }) {
                            Ok(payload) => {
                                let _ = sender.send(Message::Text(payload.into())).await;
                            }
                            Err(e) => {
                                error!("Failed to serialize server text message: {}", e);
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Agent loop error: {}", e);
                    rollback_last_user_message(&mut history, &user_text);
                    match serde_json::to_string(&ServerMessage::Error { message: format!("Error: {}", e) }) {
                        Ok(payload) => {
                            let _ = sender.send(Message::Text(payload.into())).await;
                        }
                        Err(serialize_err) => {
                            error!("Failed to serialize server error message: {}", serialize_err);
                        }
                    }
                }
            }
        }
    }
}

async fn run_agent_loop(
    state: &AgentState,
    mut messages: Vec<ChatMessage>,
    authorization: Option<&str>,
    session_id: &str,
    bank_id: Uuid,
) -> Result<ChatResponse> {
    let user_prompt = messages.last().map(|m| m.content.clone()).unwrap_or_default();

    // 1. Recall Memory (Context Injection)
    // For now, we use a zero-vector since we don't have an embedding service yet.
    // In production, user_prompt would be embedded first.
    let relevant_memories = state.memory.recall(state.host_id, bank_id, vec![0.0; 384], 5).await?;
    if !relevant_memories.is_empty() {
        let mut context_msg = String::from("Relevant context from your memory:\n");
        for mem in relevant_memories {
            context_msg.push_str(&format!("- {}\n", mem.content));
        }
        // Inject as a system hint or prefix to the user message
        if let Some(msg) = messages.last_mut() {
            msg.content = format!("{}\n\n{}", context_msg, msg.content);
        }
    }

    // 2. Discover Skills (Dynamic Tooling - Pattern B)
    let search_results = state.registry.search_skills(user_prompt.to_string(), Some(10)).await.unwrap_or_else(|e| {
        warn!("Skill search failed: {}", e);
        portal_registry::SkillSearchResponse { skills: vec![] }
    });

    let mut tool_specs: Vec<ToolSpec> = search_results.skills.into_iter().map(|s| ToolSpec {
        name: s.tool_name,
        description: s.description,
        parameters: s.input_schema,
    }).collect();

    // Still include MCP tools for now
    let mcp_tools = state.mcp_client.list_tools(authorization).await.unwrap_or_default();
    for t in mcp_tools {
        tool_specs.push(ToolSpec {
            name: t.name,
            description: t.description,
            parameters: t.input_schema,
        });
    }

    // 3. Main LLM Loop
    let mut final_response = None;
    for _ in 0..10 {
        let response = {
            let request = ChatRequest {
                messages: &messages,
                tools: if tool_specs.is_empty() { None } else { Some(&tool_specs) },
            };
            state.provider.chat(request, &state.ollama_config.model, 0.7).await?
        };

        if response.tool_calls.is_empty() {
            final_response = Some(response);
            break;
        }

        // Add assistant message with tool calls
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: serde_json::to_string(&serde_json::json!({ "tool_calls": response.tool_calls })).unwrap(),
        });

        for tool_call in &response.tool_calls {
            let args: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or_default();
            match state.mcp_client.call_tool(authorization, &tool_call.name, args).await {
                Ok(result) => {
                    let mut text_result = String::new();
                    for content in result.content {
                        if let McpContent::Text { text } = content { text_result.push_str(&text); }
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

    let response = final_response.ok_or_else(|| anyhow::anyhow!("Max iterations reached"))?;

    // 4. Retain Experience (Learning)
    if let Some(ref text) = response.text {
        let trajectory = format!("User: {}\nAssistant: {}", user_prompt, text);
        let _ = state.memory.retain(
            state.host_id,
            bank_id,
            &trajectory,
            "experience",
            None,
            serde_json::json!({ "session_id": session_id })
        ).await.map_err(|e| warn!("Failed to retain memory: {}", e));
    }

    Ok(response)
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

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/portal".to_string());
    let pool = PgPool::connect(&db_url).await.context("Failed to connect to database")?;

    let memory = Arc::new(PgHindsightClient::new(pool.clone()));
    let host_id = required_uuid_env_var("LIGHT_AGENT_HOST_ID")?;

    // Registry Client Configuration
    let registry_handler = Arc::new(NoopRegistryHandler);
    let registration_params = ServiceRegistrationParams {
        service_id: "light-agent".to_string(),
        version: "0.1.0".to_string(),
        protocol: "ws".to_string(),
        address: "localhost".to_string(),
        port: 4000,
        tags: HashMap::new(),
        env_tag: Some("dev".to_string()),
        jwt: "agent-token".to_string(),
    };
    
    let registry_url = "ws://localhost:8080/ws"; // Controller URL
    let registry = Arc::new(PortalRegistryClient::new(registry_url, registration_params, registry_handler)?);
    let registry_clone = Arc::clone(&registry);
    tokio::spawn(async move {
        registry_clone.run().await;
    });

    let state = Arc::new(AgentState {
        provider: OllamaProvider::new(Some(&ollama_config.ollama_url), None)
            .context("Failed to build Ollama provider")?,
        mcp_client,
        ollama_config,
        memory,
        registry,
        db: pool,
        host_id,
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

struct NoopRegistryHandler;
#[async_trait::async_trait]
impl RegistryHandler for NoopRegistryHandler {}

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
