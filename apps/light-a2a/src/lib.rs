use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use a2a_core::{AuthorizedInvocation, Direction, verify_authorized_invocation};
use a2a_store::{ExpectedBinding, Repository, TaskAccess, TaskAdmission};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use light_runtime::RuntimeConfig;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aConfig {
    pub runtime_policy: RuntimePolicy,
    pub operational_store: OperationalStore,
    pub authorization_context_key_file: PathBuf,
    pub maximum_database_connections: u32,
    pub maximum_request_bytes: usize,
    pub bindings: Vec<A2aBinding>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicy {
    pub audience: String,
    pub host_id: Uuid,
    pub environment: String,
    pub service_id: String,
    pub instance_id: Uuid,
    pub content_digest: String,
    pub schema_version: u64,
    pub valid_from: String,
    pub refresh_after: String,
    pub expires_at: String,
    pub revocation_epoch: u64,
    pub compatibility_generation: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationalStore {
    pub contract_version: u16,
    pub binding_id: Uuid,
    pub binding_digest: String,
    pub host_id: Uuid,
    pub environment: String,
    pub service_owner: String,
    pub schema: String,
    pub expected_database: String,
    pub minimum_schema_generation: i64,
    pub database_url_file: PathBuf,
    pub credential_generation: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aBinding {
    pub agent_ref: String,
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: String,
    pub directions: Vec<Direction>,
    pub backend_kind: String,
    pub backend_binding_id: Uuid,
}

impl A2aConfig {
    pub fn load(runtime: &RuntimeConfig) -> Result<Self, String> {
        let config = runtime
            .module_registry
            .load_config::<Self>(runtime, "a2a.yml")
            .map_err(|error| format!("load effective A2A configuration: {error}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.runtime_policy.audience != "light-a2a"
            || self.operational_store.contract_version != 1
            || self.operational_store.credential_generation < 1
            || self.operational_store.host_id != self.runtime_policy.host_id
            || self.operational_store.environment != self.runtime_policy.environment
            || self.operational_store.service_owner != "light-a2a"
            || self.operational_store.schema != "a2a_ops"
            || self.operational_store.expected_database != "operations"
            || self.operational_store.binding_digest.len() < 8
            || self.maximum_database_connections == 0
            || self.maximum_request_bytes == 0
            || self.bindings.is_empty()
        {
            return Err("invalid immutable light-a2a runtime/store projection".into());
        }
        let mut aliases = BTreeMap::new();
        for binding in &self.bindings {
            if binding.agent_ref.trim().is_empty()
                || !binding.policy_digest.starts_with("sha256:")
                || binding.directions.is_empty()
                || !matches!(
                    binding.backend_kind.as_str(),
                    "EXTERNAL_SIDECAR" | "REMOTE_A2A"
                )
                || aliases
                    .insert(binding.agent_ref.clone(), binding.binding_id)
                    .is_some()
            {
                return Err("invalid or duplicate A2A binding".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct A2aState {
    repository: Repository,
    bindings: Arc<BTreeMap<String, A2aBinding>>,
    authorization_key: Arc<Vec<u8>>,
    maximum_request_bytes: usize,
}

impl A2aState {
    pub async fn build(config: A2aConfig) -> Result<Self, String> {
        let database_url =
            a2a_store::read_database_url(&config.operational_store.database_url_file)
                .map_err(|error| error.to_string())?;
        let pool = PgPoolOptions::new()
            .max_connections(config.maximum_database_connections)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO a2a_ops, operational_meta")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .map_err(|error| format!("connect A2A operational store: {error}"))?;
        a2a_store::validate(
            &pool,
            &ExpectedBinding {
                binding_id: config.operational_store.binding_id,
                binding_digest: &config.operational_store.binding_digest,
                host_id: config.operational_store.host_id,
                environment: &config.operational_store.environment,
                minimum_schema_generation: config.operational_store.minimum_schema_generation,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let authorization_key = std::fs::read(&config.authorization_context_key_file)
            .map_err(|error| format!("read A2A authorized-context key: {error}"))?;
        if authorization_key.len() < 32 {
            return Err("A2A authorized-context key must contain at least 32 bytes".into());
        }
        Ok(Self {
            repository: Repository::new(pool),
            bindings: Arc::new(
                config
                    .bindings
                    .into_iter()
                    .map(|value| (value.agent_ref.clone(), value))
                    .collect(),
            ),
            authorization_key: Arc::new(authorization_key),
            maximum_request_bytes: config.maximum_request_bytes,
        })
    }

    pub fn pool(&self) -> sqlx::PgPool {
        self.repository.pool().clone()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendParams {
    #[serde(default)]
    task_id: Option<Uuid>,
    #[serde(default)]
    context_id: Option<Uuid>,
    message: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskParams {
    id: Uuid,
}

pub fn router(state: Arc<A2aState>) -> Router {
    let limit = state.maximum_request_bytes;
    Router::new()
        .route("/a2a/{agent_ref}", post(a2a_request))
        .route("/internal/a2a/outbound/{agent_ref}", post(outbound_request))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

async fn a2a_request(
    State(state): State<Arc<A2aState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    handle(state, agent_ref, headers, body, Direction::Inbound).await
}

async fn outbound_request(
    State(state): State<Arc<A2aState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<Value>) {
    handle(state, agent_ref, headers, body, Direction::Outbound).await
}

async fn handle(
    state: Arc<A2aState>,
    agent_ref: String,
    headers: HeaderMap,
    body: Bytes,
    direction: Direction,
) -> (StatusCode, Json<Value>) {
    let request = match serde_json::from_slice::<JsonRpcRequest>(&body) {
        Ok(value) if value.jsonrpc == "2.0" => value,
        _ => {
            return rpc_error(
                Value::Null,
                -32600,
                "Invalid Request",
                StatusCode::BAD_REQUEST,
            );
        }
    };
    let invocation = match verify_context(&headers, &body, &state.authorization_key) {
        Ok(value) => value,
        Err(message) => return rpc_error(request.id, -32001, message, StatusCode::UNAUTHORIZED),
    };
    let Some(binding) = state.bindings.get(&agent_ref) else {
        return rpc_error(
            request.id,
            -32004,
            "Agent binding not found",
            StatusCode::NOT_FOUND,
        );
    };
    if invocation.target_agent_ref != agent_ref
        || invocation.binding_id != binding.binding_id
        || invocation.publication_id != binding.publication_id
        || invocation.policy_digest != binding.policy_digest
        || invocation.direction != direction
        || !binding.directions.contains(&direction)
    {
        return rpc_error(
            request.id,
            -32003,
            "A2A binding denied",
            StatusCode::FORBIDDEN,
        );
    }

    match request.method.as_str() {
        "message/send" | "message/stream" => {
            let params = match serde_json::from_value::<SendParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(
                        request.id,
                        -32602,
                        "Invalid params",
                        StatusCode::BAD_REQUEST,
                    );
                }
            };
            let _bounded_message = params.message;
            let admission = TaskAdmission {
                task_id: params.task_id.unwrap_or_else(Uuid::now_v7),
                context_id: params.context_id.unwrap_or_else(Uuid::now_v7),
                invocation: invocation.clone(),
            };
            match state.repository.admit(&admission).await {
                Ok(snapshot) => {
                    if let Err(error) = state
                        .repository
                        .bind_backend(
                            &access(&invocation, snapshot.task_id),
                            &binding.backend_kind,
                            binding.backend_binding_id,
                            &format!("pending:{}", snapshot.task_id),
                        )
                        .await
                    {
                        return rpc_error(
                            request.id,
                            -32050,
                            &error.to_string(),
                            StatusCode::BAD_GATEWAY,
                        );
                    }
                    rpc_result(
                        request.id,
                        serde_json::to_value(snapshot).unwrap_or(Value::Null),
                    )
                }
                Err(error) => {
                    rpc_error(request.id, -32010, &error.to_string(), StatusCode::CONFLICT)
                }
            }
        }
        "tasks/get" => {
            let params = match serde_json::from_value::<TaskParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(
                        request.id,
                        -32602,
                        "Invalid params",
                        StatusCode::BAD_REQUEST,
                    );
                }
            };
            match state.repository.get(&access(&invocation, params.id)).await {
                Ok(snapshot) => rpc_result(
                    request.id,
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                ),
                Err(error) => rpc_error(
                    request.id,
                    -32004,
                    &error.to_string(),
                    StatusCode::NOT_FOUND,
                ),
            }
        }
        "tasks/cancel" => {
            let params = match serde_json::from_value::<TaskParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(
                        request.id,
                        -32602,
                        "Invalid params",
                        StatusCode::BAD_REQUEST,
                    );
                }
            };
            match state
                .repository
                .cancel(&access(&invocation, params.id))
                .await
            {
                Ok(snapshot) => rpc_result(
                    request.id,
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                ),
                Err(error) => {
                    rpc_error(request.id, -32011, &error.to_string(), StatusCode::CONFLICT)
                }
            }
        }
        _ => rpc_error(
            request.id,
            -32601,
            "Method not found",
            StatusCode::NOT_FOUND,
        ),
    }
}

fn access(invocation: &AuthorizedInvocation, task_id: Uuid) -> TaskAccess<'_> {
    TaskAccess {
        host_id: invocation.host_id,
        task_id,
        principal_subject: &invocation.principal_subject,
        caller_agent_ref: &invocation.caller_agent_ref,
        target_agent_ref: &invocation.target_agent_ref,
        binding_id: invocation.binding_id,
    }
}

fn verify_context(
    headers: &HeaderMap,
    body: &[u8],
    key: &[u8],
) -> Result<AuthorizedInvocation, &'static str> {
    let encoded = headers
        .get("x-light-a2a-context")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context")?;
    let signature = headers
        .get("x-light-a2a-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context signature")?;
    verify_authorized_invocation(
        encoded,
        signature,
        body,
        key,
        "light-a2a",
        chrono::Utc::now(),
    )
    .map_err(|_| "Authorized context rejected")
}

fn rpc_result(id: Value, result: Value) -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({"jsonrpc":"2.0","id":id,"result":result})),
    )
}

fn rpc_error(id: Value, code: i64, message: &str, status: StatusCode) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_destination_is_not_representable_in_send_params() {
        let value = json!({"message":{},"url":"https://forbidden.example"});
        assert!(serde_json::from_value::<SendParams>(value).is_err());
    }
}
