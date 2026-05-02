use crate::deployer::DeployerService;
use crate::model::{
    DeploymentAction, DeploymentError, DeploymentRequest, DeploymentResponse, DeploymentStatus,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{StreamExt, stream};
use light_axum::{AxumApp, ServerContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone)]
pub struct ApiState {
    service: DeployerService,
}

impl ApiState {
    pub fn new(service: DeployerService) -> Self {
        Self { service }
    }
}

#[derive(Clone)]
pub struct DeployerApp {
    service: DeployerService,
}

impl DeployerApp {
    pub fn new(service: DeployerService) -> Self {
        Self { service }
    }
}

impl AxumApp for DeployerApp {
    fn router(&self, _context: ServerContext) -> Router {
        router(self.service.clone())
    }
}

pub fn router(service: DeployerService) -> Router {
    let state = ApiState::new(service);
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(health))
        .route("/mcp", post(mcp_rpc))
        .route("/deployments", post(execute))
        .route("/mcp/tools", get(list_tools))
        .route("/mcp/tools/{tool}", get(get_tool).post(execute_tool))
        .route("/events", get(events))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn execute(
    State(state): State<ApiState>,
    Json(request): Json<DeploymentRequest>,
) -> Json<crate::model::DeploymentResponse> {
    Json(state.service.execute(request).await)
}

async fn mcp_rpc(
    State(state): State<ApiState>,
    Json(request): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    Json(handle_mcp_rpc(state, request).await)
}

async fn handle_mcp_rpc(state: ApiState, request: JsonRpcRequest) -> JsonRpcResponse {
    if request.jsonrpc != "2.0" {
        return json_rpc_error(
            request.id,
            -32600,
            "Invalid Request",
            Some(json!({ "message": "jsonrpc must be 2.0" })),
        );
    }

    match request.method.as_str() {
        "tools/list" => json_rpc_result(request.id, json!({ "tools": mcp_tool_definitions() })),
        "tools/call" => call_mcp_tool(state, request).await,
        "initialize" => json_rpc_result(
            request.id,
            json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "light-deployer",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "tools": {}
                }
            }),
        ),
        "notifications/initialized" => json_rpc_result(request.id, JsonValue::Null),
        _ => json_rpc_error(
            request.id,
            -32601,
            "Method not found",
            Some(json!({ "method": request.method })),
        ),
    }
}

async fn call_mcp_tool(state: ApiState, request: JsonRpcRequest) -> JsonRpcResponse {
    let params = request.params.unwrap_or_else(|| json!({}));
    let Some(tool_name) = params.get("name").and_then(JsonValue::as_str) else {
        return json_rpc_error(
            request.id,
            -32602,
            "Invalid params",
            Some(json!({ "message": "tools/call requires params.name" })),
        );
    };
    let Some(action) = action_for_tool(tool_name) else {
        return json_rpc_error(
            request.id,
            -32602,
            "Invalid params",
            Some(json!({ "message": format!("Unknown tool `{tool_name}`") })),
        );
    };

    let mut arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    inject_action(&mut arguments, action);

    let deployment_request = match serde_json::from_value::<DeploymentRequest>(arguments) {
        Ok(request) => request,
        Err(error) => {
            return json_rpc_error(
                request.id,
                -32602,
                "Invalid params",
                Some(json!({ "message": error.to_string() })),
            );
        }
    };

    let deployment_response = state.service.execute(deployment_request).await;
    let text = serde_json::to_string_pretty(&deployment_response)
        .unwrap_or_else(|error| format!("Failed to serialize deployment response: {error}"));
    json_rpc_result(
        request.id,
        json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "structuredContent": deployment_response,
            "isError": deployment_response.error.is_some()
        }),
    )
}

async fn execute_tool(
    State(state): State<ApiState>,
    Path(tool): Path<String>,
    Json(mut request): Json<JsonValue>,
) -> (StatusCode, Json<DeploymentResponse>) {
    let Some(action) = action_for_tool(&tool) else {
        return (
            StatusCode::NOT_FOUND,
            Json(error_response(
                request_id_from_value(&request),
                DeploymentAction::Render,
                "TOOL_NOT_FOUND",
                format!("Unknown deployment tool `{tool}`."),
                json!({ "tool": tool }),
            )),
        );
    };

    inject_action(&mut request, action.clone());

    let request = match serde_json::from_value::<DeploymentRequest>(request) {
        Ok(request) => request,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(error_response(
                    Uuid::now_v7().to_string(),
                    action,
                    "INVALID_TOOL_INPUT",
                    error.to_string(),
                    json!({}),
                )),
            );
        }
    };

    (StatusCode::OK, Json(state.service.execute(request).await))
}

async fn list_tools() -> Json<ToolListResponse> {
    Json(ToolListResponse {
        tools: tool_definitions(),
    })
}

async fn get_tool(Path(tool): Path<String>) -> (StatusCode, Json<JsonValue>) {
    if tool == "list" {
        return (StatusCode::OK, Json(json!({ "tools": tool_definitions() })));
    }

    match tool_definitions()
        .into_iter()
        .find(|definition| definition.name == tool)
    {
        Some(definition) => (StatusCode::OK, Json(json!(definition))),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": {
                    "code": "TOOL_NOT_FOUND",
                    "message": format!("Unknown deployment tool `{tool}`.")
                }
            })),
        ),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventsQuery {
    #[serde(alias = "request_id")]
    request_id: String,
}

async fn events(
    State(state): State<ApiState>,
    Query(query): Query<EventsQuery>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.service.events().subscribe();
    let request_id = query.request_id;
    let history = state.service.events().history_for(&request_id).await;

    let history_stream = stream::iter(history.into_iter().map(|event| {
        let payload = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Ok(Event::default().event("deployment").data(payload))
    }));

    let live_stream = stream::unfold(
        (receiver, request_id),
        |(mut receiver, request_id)| async move {
            loop {
                match receiver.recv().await {
                    Ok(event) if event.request_id == request_id => {
                        let payload =
                            serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
                        return Some((
                            Ok(Event::default().event("deployment").data(payload)),
                            (receiver, request_id),
                        ));
                    }
                    Ok(_) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    Sse::new(history_stream.chain(live_stream)).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn action_for_tool(tool: &str) -> Option<DeploymentAction> {
    match tool {
        "deployment.render" => Some(DeploymentAction::Render),
        "deployment.dryRun" => Some(DeploymentAction::DryRun),
        "deployment.diff" => Some(DeploymentAction::Diff),
        "deployment.apply" => Some(DeploymentAction::Deploy),
        "deployment.delete" => Some(DeploymentAction::Undeploy),
        "deployment.status" => Some(DeploymentAction::Status),
        "deployment.rollback" => Some(DeploymentAction::Rollback),
        _ => None,
    }
}

fn inject_action(request: &mut JsonValue, action: DeploymentAction) {
    if let JsonValue::Object(map) = request {
        map.insert("action".to_string(), json!(action));
    }
}

fn request_id_from_value(value: &JsonValue) -> String {
    value
        .get("requestId")
        .and_then(JsonValue::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string())
}

fn error_response(
    request_id: String,
    action: DeploymentAction,
    code: &str,
    message: String,
    details: JsonValue,
) -> DeploymentResponse {
    let mut error_details = BTreeMap::new();
    if !details.is_null() {
        error_details.insert("details".to_string(), details);
    }

    DeploymentResponse {
        request_id,
        action,
        status: DeploymentStatus::Failed,
        deployer_id: "light-deployer".to_string(),
        cluster_id: String::new(),
        namespace: String::new(),
        manifest_hash: None,
        values_hash: None,
        values_snapshot_id: None,
        runtime_values_hash: None,
        runtime_values_snapshot_id: None,
        template_commit_sha: None,
        resources: Vec::new(),
        diff: None,
        artifact_ref: None,
        events: Vec::new(),
        error: Some(DeploymentError {
            code: code.to_string(),
            message,
            details: error_details,
        }),
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolListResponse {
    tools: Vec<ToolDefinition>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: JsonValue,
    endpoint: &'static str,
    method: &'static str,
}

fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        tool_definition(
            "deployment.render",
            "Render Kubernetes templates and return a safe manifest summary without calling the Kubernetes API.",
        ),
        tool_definition(
            "deployment.dryRun",
            "Render Kubernetes templates and validate them with Kubernetes server-side dry-run.",
        ),
        tool_definition(
            "deployment.diff",
            "Render Kubernetes templates, compare them with currently managed resources, and return a redacted diff summary.",
        ),
        tool_definition(
            "deployment.apply",
            "Apply rendered Kubernetes resources and start deployment progress reporting.",
        ),
        tool_definition(
            "deployment.delete",
            "Delete resources managed for the requested deployment instance.",
        ),
        tool_definition(
            "deployment.status",
            "Return current Kubernetes status for resources managed by the requested deployment instance.",
        ),
        tool_definition(
            "deployment.rollback",
            "Redeploy a previous deployment snapshot when snapshot integration is available.",
        ),
    ]
}

fn tool_definition(name: &'static str, description: &'static str) -> ToolDefinition {
    ToolDefinition {
        name,
        description,
        input_schema: deployment_request_schema(),
        endpoint: "/mcp/tools/{tool}",
        method: "POST",
    }
}

fn deployment_request_schema() -> JsonValue {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": [
            "hostId",
            "instanceId",
            "environment",
            "clusterId",
            "namespace",
            "template"
        ],
        "properties": {
            "requestId": {
                "type": "string",
                "description": "Optional caller-supplied request identifier. If omitted, the deployer creates one."
            },
            "hostId": {
                "type": "string",
                "description": "Tenant or host identifier for audit and management labels."
            },
            "instanceId": {
                "type": "string",
                "description": "Portal instance identifier. Also used for management labels and pruning."
            },
            "environment": {
                "type": "string",
                "description": "Deployment environment such as dev, test, or prod."
            },
            "clusterId": {
                "type": "string",
                "description": "Expected cluster identifier."
            },
            "namespace": {
                "type": "string",
                "description": "Target Kubernetes namespace."
            },
            "values": {
                "type": "object",
                "description": "Inline deployment values used to render template placeholders.",
                "additionalProperties": true
            },
            "valuesRef": {
                "type": "object",
                "description": "Future values reference for config-server backed deployments.",
                "additionalProperties": true
            },
            "template": {
                "type": "object",
                "required": ["repoUrl", "path"],
                "additionalProperties": false,
                "properties": {
                    "repoUrl": {
                        "type": "string",
                        "description": "Git repository URL or local marker when LIGHT_DEPLOYER_TEMPLATE_BASE_DIR is used."
                    },
                    "ref": {
                        "type": "string",
                        "description": "Git branch, tag, or commit ref. Defaults to main."
                    },
                    "path": {
                        "type": "string",
                        "description": "Path inside the repository containing Kubernetes YAML templates."
                    }
                }
            },
            "options": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "dryRun": { "type": "boolean" },
                    "waitForRollout": { "type": "boolean" },
                    "timeoutSeconds": { "type": "integer", "minimum": 1 },
                    "pruneOverride": { "type": "boolean" }
                }
            }
        }
    })
}

#[derive(Debug, Clone, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<JsonValue>,
    #[serde(default)]
    id: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize)]
struct JsonRpcError {
    code: i32,
    message: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<JsonValue>,
}

fn json_rpc_result(id: Option<JsonValue>, result: JsonValue) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        result: Some(result),
        error: None,
        id,
    }
}

fn json_rpc_error(
    id: Option<JsonValue>,
    code: i32,
    message: &'static str,
    data: Option<JsonValue>,
) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: "2.0",
        result: None,
        error: Some(JsonRpcError {
            code,
            message,
            data,
        }),
        id,
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct McpToolDefinition {
    name: &'static str,
    description: &'static str,
    input_schema: JsonValue,
}

fn mcp_tool_definitions() -> Vec<McpToolDefinition> {
    tool_definitions()
        .into_iter()
        .map(|definition| McpToolDefinition {
            name: definition.name,
            description: definition.description,
            input_schema: definition.input_schema,
        })
        .collect()
}
