use crate::invocation::{
    AcceptOutcome, AuthenticatedInvocationContext, InvocationAcceptError, PreparedInvocationStart,
    accept_invocation,
};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use chrono::Utc;
use light_rule::{ActionRegistry, Rule, RuleEngine};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgListener};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use tracing::{error, info};
use uuid::Uuid;
use workflow_core::models::retry::OneOfRetryPolicyDefinitionOrReference;
use workflow_core::models::task::{CallTaskDefinition, TaskDefinition};
use workflow_core::models::workflow::{RuntimeExpressionLanguage, WorkflowDefinition};
use workflow_invocation_contract::{
    CONTRACT_VERSION, CancellationPolicy, EffectState, ErrorCode, InvocationError, InvocationMode,
    InvocationState, InvocationStatus, StartInvocationRequest, canonical_json_bytes,
};

const MAX_WAIT_MS: u64 = 20_000;

#[derive(Clone)]
pub struct RuleApiState {
    engine: Arc<RuleEngine>,
    pool: PgPool,
    invocation_bearer_token: Arc<str>,
    wait_listener_permits: Arc<Semaphore>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTestRequest {
    pub rule_body: Value,
    pub input_context: Value,
    pub expected_result: Option<bool>,
    pub test_mode: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleTestResponse {
    pub executor: String,
    pub passed: bool,
    pub expected_result: Option<bool>,
    pub success: bool,
    pub mutated_context: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WaitRequest {
    wait_ms: u64,
    #[serde(default)]
    observed_version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QuarantineRepairRequest {
    reason: String,
}

#[derive(Debug)]
struct InvocationIdentity {
    host_id: Uuid,
    principal_subject: String,
    end_user_subject: String,
    caller_claims_digest: String,
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    binding_id: Uuid,
    wf_def_id: Uuid,
    workflow_version: String,
    definition_digest: String,
    schema_digest: String,
    policy_digest: String,
    response_policy_digest: String,
    definition: String,
    tool_name: String,
}

pub async fn run_rule_api(pool: PgPool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = std::env::var("LIGHT_WORKFLOW_HTTP_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:8436".to_string())
        .parse()?;
    let invocation_bearer_token = std::env::var("WORKFLOW_INVOCATION_BEARER_TOKEN")
        .map_err(|_| "WORKFLOW_INVOCATION_BEARER_TOKEN is required")?;
    if invocation_bearer_token.len() < 32 {
        return Err("WORKFLOW_INVOCATION_BEARER_TOKEN must contain at least 32 bytes".into());
    }

    let state = RuleApiState {
        engine: Arc::new(RuleEngine::new(Arc::new(ActionRegistry::new()))),
        pool,
        invocation_bearer_token: invocation_bearer_token.into(),
        wait_listener_permits: Arc::new(Semaphore::new(
            std::env::var("WORKFLOW_WAIT_LISTENER_CONNECTIONS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(8)
                .clamp(1, 64),
        )),
    };

    let app = Router::new()
        .route("/rule/test", post(run_rule_test))
        .route("/v1/workflow-invocations", post(start_invocation))
        .route(
            "/v1/workflow-invocations/{workflow_instance_id}",
            get(get_invocation).delete(cancel_invocation),
        )
        .route(
            "/v1/workflow-invocations/{workflow_instance_id}/wait",
            post(wait_for_invocation),
        )
        .route(
            "/v1/workflow-invocations/{workflow_instance_id}/result",
            get(get_invocation_result),
        )
        .route(
            "/v1/workflow-event-quarantine/{quarantine_id}/repair",
            post(repair_quarantined_event),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("Light Workflow API listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

async fn repair_quarantined_event(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(quarantine_id): Path<Uuid>,
    Json(request): Json<QuarantineRepairRequest>,
) -> Result<StatusCode, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let reason = request.reason.trim();
    if reason.is_empty() || reason.len() > 1000 {
        return Err(ApiError::bad_request(
            "quarantine repair reason must contain 1-1000 characters",
        ));
    }
    let affected = sqlx::query(
        "UPDATE workflow_invocation_event_quarantine_t
            SET replay_state='REPAIRED',repaired_by=$1,repair_reason=$2,resolved_ts=NULL
          WHERE host_id=$3 AND quarantine_id=$4 AND replay_state='BLOCKED'",
    )
    .bind(&identity.end_user_subject)
    .bind(reason)
    .bind(identity.host_id)
    .bind(quarantine_id)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?
    .rows_affected();
    if affected != 1 {
        return Err(ApiError::not_found(
            "blocked workflow quarantine entry is unavailable",
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

async fn start_invocation(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<StartInvocationRequest>,
) -> Result<(StatusCode, Json<InvocationStatus>), ApiError> {
    let identity = authenticate(&state, &headers)?;
    request
        .validate(Utc::now())
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    let request_bytes = canonical_json_bytes(&request.input)
        .map_err(|error| ApiError::bad_request(error.to_string()))?;
    if u64::try_from(request_bytes.len()).unwrap_or(u64::MAX) > request.budget.maximum_request_bytes
    {
        return Err(ApiError::bad_request(
            "workflow input exceeds maximumRequestBytes",
        ));
    }
    let binding = sqlx::query_as::<_, BindingRow>(
        "SELECT b.binding_id,b.wf_def_id,b.workflow_version,b.definition_digest,
                b.schema_digest,b.policy_digest,b.response_policy_digest,w.definition,
                t.name AS tool_name
           FROM workflow_tool_binding_t b
           JOIN wf_definition_t w ON w.host_id=b.host_id AND w.wf_def_id=b.wf_def_id
           JOIN tool_t t ON t.host_id=b.host_id AND t.tool_id=b.tool_id
          WHERE b.host_id=$1 AND b.tool_id=$2 AND b.active AND w.active AND t.active",
    )
    .bind(identity.host_id)
    .bind(request.stable_tool_ref)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| ApiError::definition_mismatch("workflow binding is unavailable"))?;
    verify_binding(&request, &binding)?;
    let definition: WorkflowDefinition = serde_yaml::from_str(&binding.definition)
        .map_err(|error| ApiError::definition_mismatch(error.to_string()))?;
    validate_orchestration_definition(&definition, request.mode, &request.budget)?;
    validate_pinned_dependencies(
        &state.pool,
        identity.host_id,
        binding.binding_id,
        &definition,
        request.mode,
        request.budget.maximum_delegation_depth,
    )
    .await?;
    validate_approval_evidence(
        &state.pool,
        identity.host_id,
        binding.binding_id,
        &definition,
    )
    .await?;
    if request.mode == InvocationMode::Sync {
        enforce_deadline_aware_admission(
            &state.pool,
            identity.host_id,
            &request,
            definition.do_.entries.len(),
        )
        .await?;
    }
    let (initial_task_name, initial_task) = definition
        .do_
        .entries
        .first()
        .and_then(|entry| entry.iter().next())
        .ok_or_else(|| ApiError::definition_mismatch("workflow has no initial task"))?;
    let initial_task_type = supported_phase2_task_type(initial_task)?;
    let definition_snapshot = serde_json::to_value(&definition)
        .map_err(|error| ApiError::definition_mismatch(error.to_string()))?;
    let actual_definition_digest = format!(
        "sha256:{}",
        execution_runner_protocol::canonical_sha256(&definition_snapshot)
            .map_err(|error| ApiError::definition_mismatch(error.to_string()))?
    );
    if actual_definition_digest != request.definition_digest {
        return Err(ApiError::definition_mismatch(
            "published workflow definition does not match its pinned digest",
        ));
    }
    validate_cel_expressions(&state.engine, &definition_snapshot)?;
    let public_output_schema = definition
        .output
        .as_ref()
        .and_then(|output| output.schema.as_ref())
        .and_then(|schema| schema.document.as_ref());
    if let Some(schema) = public_output_schema {
        jsonschema::Validator::new(schema).map_err(|error| {
            ApiError::definition_mismatch(format!("invalid workflow output schema: {error}"))
        })?;
    } else if request.mode == InvocationMode::Async {
        return Err(ApiError::definition_mismatch(
            "asynchronous workflow-backed tools require an inline workflow output schema",
        ));
    }
    let process_id = Uuid::now_v7();
    let initial_task_id = Uuid::now_v7();
    let prepared = PreparedInvocationStart {
        binding_id: binding.binding_id,
        process_id,
        initial_task_id,
        application_id: &binding.tool_name,
        initial_task_name,
        initial_task_type,
        definition_snapshot: &definition_snapshot,
        execution_placement: "host",
        task_policy_digest: request.policy_digest.trim_start_matches("sha256:"),
        public_output_schema,
    };
    let auth = AuthenticatedInvocationContext {
        host_id: identity.host_id,
        principal_subject: &identity.principal_subject,
        end_user_subject: &identity.end_user_subject,
        update_user: "light-workflow-invocation",
    };
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    let outcome = accept_invocation(&mut tx, &auth, &request, &prepared)
        .await
        .map_err(ApiError::accept)?;
    tx.commit().await.map_err(ApiError::database)?;
    let workflow_instance_id = match outcome {
        AcceptOutcome::Accepted {
            workflow_instance_id,
        }
        | AcceptOutcome::Replay {
            workflow_instance_id,
            ..
        } => workflow_instance_id,
    };
    let status = load_status(&state.pool, &identity, workflow_instance_id).await?;
    Ok((StatusCode::ACCEPTED, Json(status)))
}

async fn validate_pinned_dependencies(
    pool: &PgPool,
    host_id: Uuid,
    binding_id: Uuid,
    definition: &WorkflowDefinition,
    mode: InvocationMode,
    maximum_delegation_depth: u16,
) -> Result<(), ApiError> {
    let mut tasks: Vec<&TaskDefinition> = definition
        .do_
        .entries
        .iter()
        .filter_map(|entry| entry.iter().next().map(|(_, task)| task))
        .collect();
    let mut cursor = 0;
    while cursor < tasks.len() {
        if let TaskDefinition::Fork(fork) = tasks[cursor] {
            tasks.extend(
                fork.fork
                    .branches
                    .entries
                    .iter()
                    .filter_map(|entry| entry.iter().next().map(|(_, task)| task)),
            );
        }
        cursor += 1;
    }
    for task in tasks {
        let TaskDefinition::Call(CallTaskDefinition::Mcp(call)) = task else {
            continue;
        };
        let tool_name = mcp_tool_name(&call.with).ok_or_else(|| {
            ApiError::definition_mismatch(
                "Phase 1 MCP workflow tasks must call a pinned tool, not a resource or prompt",
            )
        })?;
        let target: Option<(String, Value)> = sqlx::query_as(
            "SELECT contract_digest,dispatch_target FROM workflow_tool_dependency_t
              WHERE host_id=$1 AND outer_binding_id=$2 AND authorization_tool_name=$3
                AND active AND lifecycle_status IN ('active','superseded')",
        )
        .bind(host_id)
        .bind(binding_id)
        .bind(tool_name)
        .fetch_optional(pool)
        .await
        .map_err(ApiError::database)?;
        let (contract_digest, target) = target.ok_or_else(|| {
            ApiError::definition_mismatch(format!(
                "nested MCP tool {tool_name} has no active pinned version target"
            ))
        })?;
        if target
            .get("contractDigest")
            .and_then(Value::as_str)
            .is_some_and(|value| value != contract_digest)
        {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP tool {tool_name} private target drifted from its pinned contract digest"
            )));
        }
        if mode == InvocationMode::Sync
            && (target.get("readOnly").and_then(Value::as_bool) != Some(true)
                || target
                    .get("humanApprovalRequired")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                || target
                    .get("destructive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false))
        {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP tool {tool_name} is not transitively read-only and headless"
            )));
        }
        let nested_depth = target
            .get("maximumDelegationDepth")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if nested_depth.saturating_add(1) > u64::from(maximum_delegation_depth) {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP tool {tool_name} exceeds the published delegation depth"
            )));
        }
    }
    Ok(())
}

fn mcp_tool_name(args: &workflow_core::models::task::McpArguments) -> Option<&str> {
    args.tool.as_deref().or_else(|| {
        (args.method.as_deref() == Some("tools/call"))
            .then(|| {
                args.parameters
                    .as_ref()
                    .and_then(|parameters| parameters.get("name"))
                    .and_then(Value::as_str)
            })
            .flatten()
    })
}

async fn validate_approval_evidence(
    pool: &PgPool,
    host_id: Uuid,
    binding_id: Uuid,
    definition: &WorkflowDefinition,
) -> Result<(), ApiError> {
    fn collect<'a>(
        prefix: Option<&str>,
        entries: &'a [std::collections::HashMap<String, TaskDefinition>],
        output: &mut Vec<(String, &'a TaskDefinition)>,
    ) {
        for entry in entries {
            let Some((name, task)) = entry.iter().next() else {
                continue;
            };
            let qualified =
                prefix.map_or_else(|| name.clone(), |prefix| format!("{prefix}::{name}"));
            output.push((qualified.clone(), task));
            if let TaskDefinition::Fork(fork) = task {
                collect(Some(&qualified), &fork.fork.branches.entries, output);
            }
        }
    }
    let mut tasks = Vec::new();
    collect(None, &definition.do_.entries, &mut tasks);
    for (task_name, task) in tasks {
        let common = match task {
            TaskDefinition::Call(CallTaskDefinition::Http(call))
                if !call.with.method.eq_ignore_ascii_case("GET")
                    && !call.with.method.eq_ignore_ascii_case("HEAD") =>
            {
                Some(&call.common)
            }
            TaskDefinition::Call(CallTaskDefinition::Mcp(call))
                if call
                    .common
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("readOnly"))
                    .and_then(Value::as_bool)
                    == Some(false) =>
            {
                Some(&call.common)
            }
            _ => None,
        };
        let Some(common) = common else { continue };
        let evidence_digest = common
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("approvalEvidenceDigest"))
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::definition_mismatch("write approval evidence is missing"))?;
        let approved: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_tool_approval_evidence_t
              WHERE host_id=$1 AND binding_id=$2 AND task_name=$3
                AND evidence_digest=$4 AND active)",
        )
        .bind(host_id)
        .bind(binding_id)
        .bind(&task_name)
        .bind(evidence_digest)
        .fetch_one(pool)
        .await
        .map_err(ApiError::database)?;
        if !approved {
            return Err(ApiError::definition_mismatch(format!(
                "write task {task_name} has no active publication approval evidence"
            )));
        }
    }
    Ok(())
}

fn find_task_recursive<'a>(
    entries: &'a [std::collections::HashMap<String, TaskDefinition>],
    requested_name: &str,
) -> Option<&'a TaskDefinition> {
    fn visit<'a>(
        prefix: Option<&str>,
        entries: &'a [std::collections::HashMap<String, TaskDefinition>],
        requested_name: &str,
    ) -> Option<&'a TaskDefinition> {
        for entry in entries {
            let Some((name, task)) = entry.iter().next() else {
                continue;
            };
            let qualified =
                prefix.map_or_else(|| name.clone(), |prefix| format!("{prefix}::{name}"));
            if requested_name == name || requested_name == qualified {
                return Some(task);
            }
            if let TaskDefinition::Fork(fork) = task
                && let Some(found) = visit(
                    Some(&qualified),
                    &fork.fork.branches.entries,
                    requested_name,
                )
            {
                return Some(found);
            }
        }
        None
    }

    visit(None, entries, requested_name)
}

async fn enforce_deadline_aware_admission(
    pool: &PgPool,
    host_id: Uuid,
    request: &StartInvocationRequest,
    task_count: usize,
) -> Result<(), ApiError> {
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM task_info_t
          WHERE host_id=$1 AND active
            AND execution_placement='host' AND execution_class='interactive'
            AND status_code IN ('A','C')
            AND (deadline_ts IS NULL OR deadline_ts>CURRENT_TIMESTAMP)",
    )
    .bind(host_id)
    .fetch_one(pool)
    .await
    .map_err(ApiError::database)?;
    let concurrency = std::env::var("WORKFLOW_HOST_EXECUTOR_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8)
        .clamp(1, 128);
    let estimated_task_ms = std::env::var("WORKFLOW_INTERACTIVE_ESTIMATED_TASK_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(500)
        .clamp(1, 30_000);
    let queued = u64::try_from(queued).unwrap_or(u64::MAX);
    let waves = queued.div_ceil(concurrency);
    let estimated_ms = waves
        .saturating_add(u64::try_from(task_count).unwrap_or(u64::MAX))
        .saturating_mul(estimated_task_ms);
    let remaining_ms = (request.deadline_ts - Utc::now()).num_milliseconds();
    if remaining_ms <= 0 || estimated_ms >= u64::try_from(remaining_ms).unwrap_or_default() {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::WorkflowCapacityExhausted,
            "interactive executor backlog cannot satisfy the declared deadline",
        ));
    }
    Ok(())
}

async fn get_invocation(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(workflow_instance_id): Path<Uuid>,
) -> Result<Json<InvocationStatus>, ApiError> {
    let identity = authenticate(&state, &headers)?;
    Ok(Json(
        load_status(&state.pool, &identity, workflow_instance_id).await?,
    ))
}

async fn wait_for_invocation(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(workflow_instance_id): Path<Uuid>,
    Json(wait): Json<WaitRequest>,
) -> Result<Json<InvocationStatus>, ApiError> {
    let identity = authenticate(&state, &headers)?;
    if wait.wait_ms == 0 || wait.wait_ms > MAX_WAIT_MS || wait.observed_version < 0 {
        return Err(ApiError::bad_request("invalid bounded wait request"));
    }
    // LISTEN connections stay outside the query pool, but are globally bounded.
    // Excess waiters use durable polling instead of opening another connection.
    let listener_permit = state.wait_listener_permits.clone().try_acquire_owned().ok();
    let mut listener = if listener_permit.is_some() {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|error| ApiError::database(sqlx::Error::Configuration(Box::new(error))))?;
        let mut listener = PgListener::connect(&database_url)
            .await
            .map_err(ApiError::database)?;
        listener
            .listen("workflow_invocation_state_v1")
            .await
            .map_err(ApiError::database)?;
        Some(listener)
    } else {
        None
    };
    let deadline = tokio::time::Instant::now() + Duration::from_millis(wait.wait_ms);
    loop {
        let status = load_status(&state.pool, &identity, workflow_instance_id).await?;
        if status.state.is_terminal() || status.state_version > wait.observed_version {
            return Ok(Json(status));
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Ok(Json(status));
        }
        let remaining = deadline - now;
        if let Some(listener) = listener.as_mut() {
            let _ =
                tokio::time::timeout(remaining.min(Duration::from_millis(500)), listener.recv())
                    .await;
        } else {
            tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        }
    }
}

async fn get_invocation_result(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(workflow_instance_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let identity = authenticate(&state, &headers)?;
    let status = load_status(&state.pool, &identity, workflow_instance_id).await?;
    if status.state != InvocationState::Completed {
        return Err(ApiError::conflict("workflow invocation is not completed"));
    }
    Ok(Json(status.public_result.unwrap_or_else(|| json!({}))))
}

async fn cancel_invocation(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(workflow_instance_id): Path<Uuid>,
) -> Result<Json<InvocationStatus>, ApiError> {
    let identity = authenticate(&state, &headers)?;
    // Enforce the current-claims/publication-ceiling check before cancellation
    // mutates any durable state.
    let _ = load_status(&state.pool, &identity, workflow_instance_id).await?;
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    let row: Option<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT process_id,cancellation_policy,effect_state,state
           FROM workflow_invocation_t
          WHERE host_id=$1 AND workflow_instance_id=$2
            AND principal_subject=$3 AND end_user_subject=$4 FOR UPDATE",
    )
    .bind(identity.host_id)
    .bind(workflow_instance_id)
    .bind(&identity.principal_subject)
    .bind(&identity.end_user_subject)
    .fetch_optional(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    let Some((process_id, cancellation_policy, effect_state, invocation_state)) = row else {
        return Err(ApiError::not_found("workflow invocation is unavailable"));
    };
    if matches!(
        invocation_state.as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED"
    ) {
        tx.commit().await.map_err(ApiError::database)?;
        return Ok(Json(
            load_status(&state.pool, &identity, workflow_instance_id).await?,
        ));
    }
    let policy = match cancellation_policy.as_str() {
        "COOPERATIVE" => CancellationPolicy::Cooperative,
        "DISABLED" => CancellationPolicy::Disabled,
        _ => CancellationPolicy::BeforeEffectsOnly,
    };
    if policy == CancellationPolicy::Disabled
        || (policy == CancellationPolicy::BeforeEffectsOnly && effect_state != "none")
    {
        let reason = if policy == CancellationPolicy::Disabled {
            "CANCELLATION_DISABLED"
        } else {
            "EFFECT_ALREADY_POSSIBLE_OR_CONFIRMED"
        };
        sqlx::query(
            "UPDATE workflow_invocation_t SET non_cancellable_reason=$1,
                    updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1
              WHERE host_id=$2 AND workflow_instance_id=$3",
        )
        .bind(reason)
        .bind(identity.host_id)
        .bind(workflow_instance_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        tx.commit().await.map_err(ApiError::database)?;
        return Ok(Json(
            load_status(&state.pool, &identity, workflow_instance_id).await?,
        ));
    }
    if policy == CancellationPolicy::Cooperative && effect_state != "none" {
        let snapshot: Value = sqlx::query_scalar(
            "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(identity.host_id)
        .bind(process_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        let definition: WorkflowDefinition = serde_json::from_value(snapshot)
            .map_err(|error| ApiError::definition_mismatch(error.to_string()))?;
        let compensations: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT ON (compensation_task) compensation_task,task_policy_digest
               FROM task_info_t WHERE host_id=$1 AND process_id=$2
                 AND effect_state='confirmed' AND compensation_task IS NOT NULL
               ORDER BY compensation_task,started_ts DESC",
        )
        .bind(identity.host_id)
        .bind(process_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        if compensations.is_empty() {
            sqlx::query(
                "UPDATE workflow_invocation_t SET non_cancellable_reason='COMPENSATION_UNAVAILABLE',
                        updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1
                  WHERE host_id=$1 AND workflow_instance_id=$2",
            )
            .bind(identity.host_id)
            .bind(workflow_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(ApiError::database)?;
            tx.commit().await.map_err(ApiError::database)?;
            return Ok(Json(
                load_status(&state.pool, &identity, workflow_instance_id).await?,
            ));
        }
        let context: Value = sqlx::query_scalar(
            "SELECT context_data FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(identity.host_id)
        .bind(process_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        sqlx::query(
            "UPDATE task_info_t SET status_code='F',result_code='CANCELLED_FOR_COMPENSATION',
                    locked='N',completed_ts=CURRENT_TIMESTAMP,lease_owner=NULL,lease_expires_ts=NULL,
                    lease_fencing_token=lease_fencing_token+1
              WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')
                AND NOT is_compensation",
        )
        .bind(identity.host_id)
        .bind(process_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        for (sequence, (task_name, policy_digest)) in compensations.iter().enumerate() {
            let task = find_task_recursive(&definition.do_.entries, task_name)
                .ok_or_else(|| ApiError::definition_mismatch("compensation task is unavailable"))?;
            let task_type = supported_phase2_task_type(task)?;
            sqlx::query(
                "INSERT INTO task_info_t(host_id,task_id,task_type,process_id,wf_instance_id,
                    wf_task_id,status_code,started_ts,locked,priority,task_input,
                    execution_placement,task_policy_digest,execution_class,deadline_ts,is_compensation)
                 VALUES($1,$2,$3,$4,$5,$6,'A',CURRENT_TIMESTAMP,'N',$7,$8,
                    'host',$9,'standard',(
                      SELECT deadline_ts FROM workflow_invocation_t
                       WHERE host_id=$1 AND workflow_instance_id=$10),TRUE)",
            )
            .bind(identity.host_id)
            .bind(Uuid::now_v7())
            .bind(task_type)
            .bind(process_id)
            .bind(workflow_instance_id.to_string())
            .bind(task_name)
            .bind(10_000_i32.saturating_sub(i32::try_from(sequence).unwrap_or(i32::MAX)))
            .bind(&context)
            .bind(policy_digest)
            .bind(workflow_instance_id)
            .execute(&mut *tx)
            .await
            .map_err(ApiError::database)?;
        }
        sqlx::query(
            "UPDATE workflow_invocation_t SET state='COMPENSATING',cancel_requested_ts=CURRENT_TIMESTAMP,
                    non_cancellable_reason=NULL,updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1
              WHERE host_id=$1 AND workflow_instance_id=$2",
        )
        .bind(identity.host_id)
        .bind(workflow_instance_id)
        .execute(&mut *tx)
        .await
        .map_err(ApiError::database)?;
        tx.commit().await.map_err(ApiError::database)?;
        return Ok(Json(
            load_status(&state.pool, &identity, workflow_instance_id).await?,
        ));
    }
    sqlx::query(
        "UPDATE workflow_invocation_t SET state='CANCELLED',terminal_ts=CURRENT_TIMESTAMP,
                cancel_requested_ts=CURRENT_TIMESTAMP,updated_ts=CURRENT_TIMESTAMP,
                state_version=state_version+1,non_cancellable_reason=NULL
          WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(identity.host_id)
    .bind(workflow_instance_id)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    sqlx::query(
        "UPDATE process_info_t SET status_code='F',custom_status_code='CANCELLED',
                completed_ts=CURRENT_TIMESTAMP
          WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')",
    )
    .bind(identity.host_id)
    .bind(process_id)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    sqlx::query(
        "UPDATE task_info_t SET status_code='F',result_code='CANCELLED',locked='N',
                completed_ts=CURRENT_TIMESTAMP,lease_owner=NULL,lease_expires_ts=NULL
          WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')",
    )
    .bind(identity.host_id)
    .bind(process_id)
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    tx.commit().await.map_err(ApiError::database)?;
    Ok(Json(
        load_status(&state.pool, &identity, workflow_instance_id).await?,
    ))
}

async fn load_status(
    pool: &PgPool,
    identity: &InvocationIdentity,
    workflow_instance_id: Uuid,
) -> Result<InvocationStatus, ApiError> {
    let row = sqlx::query(
        "SELECT stable_tool_ref,definition_digest,state,state_version,accepted_ts,updated_ts,deadline_ts,
                public_result,normalized_error,correlation_id,effect_state,non_cancellable_reason,
                response_policy_snapshot->>'acceptedSubjectClaimsDigest' AS accepted_claims_digest
           FROM workflow_invocation_t
          WHERE host_id=$1 AND workflow_instance_id=$2
            AND principal_subject=$3 AND end_user_subject=$4",
    )
    .bind(identity.host_id)
    .bind(workflow_instance_id)
    .bind(&identity.principal_subject)
    .bind(&identity.end_user_subject)
    .fetch_optional(pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| ApiError::not_found("workflow invocation is unavailable"))?;
    let accepted_claims_digest: String = row
        .try_get("accepted_claims_digest")
        .map_err(ApiError::database)?;
    if accepted_claims_digest != identity.caller_claims_digest {
        return Err(ApiError::unauthorized(
            "current subject authorization no longer matches the accepted disclosure ceiling",
        ));
    }
    let state_value: String = row.try_get("state").map_err(ApiError::database)?;
    let state = parse_state(&state_value)?;
    let normalized_error: Option<Value> = row
        .try_get("normalized_error")
        .map_err(ApiError::database)?;
    let correlation_id: String = row.try_get("correlation_id").map_err(ApiError::database)?;
    Ok(InvocationStatus {
        contract_version: CONTRACT_VERSION,
        workflow_instance_id,
        stable_tool_ref: row.try_get("stable_tool_ref").map_err(ApiError::database)?,
        definition_digest: row
            .try_get("definition_digest")
            .map_err(ApiError::database)?,
        state,
        state_version: row.try_get("state_version").map_err(ApiError::database)?,
        accepted_ts: row.try_get("accepted_ts").map_err(ApiError::database)?,
        updated_ts: row.try_get("updated_ts").map_err(ApiError::database)?,
        deadline_ts: row.try_get("deadline_ts").map_err(ApiError::database)?,
        retryable: matches!(
            state,
            InvocationState::Accepted | InvocationState::Running | InvocationState::Waiting
        ),
        effect_state: match row
            .try_get::<String, _>("effect_state")
            .map_err(ApiError::database)?
            .as_str()
        {
            "possible" => EffectState::Possible,
            "confirmed" => EffectState::Confirmed,
            _ => EffectState::None,
        },
        non_cancellable_reason: row
            .try_get("non_cancellable_reason")
            .map_err(ApiError::database)?,
        public_result: row.try_get("public_result").map_err(ApiError::database)?,
        error: normalized_error.map(|value| InvocationError {
            code: serde_json::from_value(value["code"].clone())
                .unwrap_or(ErrorCode::WorkflowTaskFailed),
            message: value["message"]
                .as_str()
                .unwrap_or("workflow invocation failed")
                .to_string(),
            retryable: value["retryable"].as_bool().unwrap_or(false),
            workflow_instance_id: Some(workflow_instance_id),
            correlation_id,
        }),
    })
}

fn authenticate(state: &RuleApiState, headers: &HeaderMap) -> Result<InvocationIdentity, ApiError> {
    let authorization = header(headers, "authorization")?;
    let supplied = authorization
        .strip_prefix("Bearer ")
        .ok_or_else(|| ApiError::unauthorized("Bearer authentication is required"))?;
    if supplied
        .as_bytes()
        .ct_eq(state.invocation_bearer_token.as_bytes())
        .unwrap_u8()
        != 1
    {
        return Err(ApiError::unauthorized(
            "invocation service authentication failed",
        ));
    }
    Ok(InvocationIdentity {
        host_id: header(headers, "x-host-id")?
            .parse()
            .map_err(|_| ApiError::unauthorized("x-host-id must be a UUID"))?,
        principal_subject: header(headers, "x-principal-subject")?.to_string(),
        end_user_subject: header(headers, "x-end-user-subject")?.to_string(),
        caller_claims_digest: header(headers, "x-caller-claims-digest")?.to_string(),
    })
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, ApiError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::unauthorized(format!("{name} is required")))
}

fn verify_binding(request: &StartInvocationRequest, binding: &BindingRow) -> Result<(), ApiError> {
    if request.workflow_definition_id != binding.wf_def_id
        || request.workflow_version != binding.workflow_version
        || request.definition_digest != binding.definition_digest
        || request.schema_digest != binding.schema_digest
        || request.policy_digest != binding.policy_digest
        || request.response_policy_digest != binding.response_policy_digest
    {
        return Err(ApiError::definition_mismatch(
            "workflow binding pins do not match the published version",
        ));
    }
    Ok(())
}

fn validate_orchestration_definition(
    definition: &WorkflowDefinition,
    mode: InvocationMode,
    budget: &workflow_invocation_contract::InvocationBudget,
) -> Result<(), ApiError> {
    if definition
        .evaluate
        .as_ref()
        .map(|value| value.language.as_str())
        != Some(RuntimeExpressionLanguage::CEL)
    {
        return Err(ApiError::definition_mismatch(
            "Phase 1 workflow-backed tools require evaluate.language: cel",
        ));
    }
    if definition.do_.entries.is_empty() || definition.do_.entries.len() > 64 {
        return Err(ApiError::definition_mismatch(
            "workflow must contain between one and sixty-four top-level tasks",
        ));
    }
    for entry in &definition.do_.entries {
        let Some((_, task)) = entry.iter().next() else {
            return Err(ApiError::definition_mismatch(
                "workflow task entry is empty",
            ));
        };
        validate_phase2_task(task, mode, budget)?;
    }
    let (task_attempts, nested_calls, cost_units) = phase2_budget_envelope(definition)?;
    if task_attempts > u64::from(budget.maximum_task_attempts)
        || nested_calls > u64::from(budget.maximum_nested_calls)
        || cost_units > budget.maximum_cost_units
    {
        return Err(ApiError::definition_mismatch(
            "workflow retry, nested-call, or cost envelope exceeds the published invocation budget",
        ));
    }
    Ok(())
}

fn phase2_budget_envelope(definition: &WorkflowDefinition) -> Result<(u64, u64, u64), ApiError> {
    fn task_envelope(
        definition: &WorkflowDefinition,
        task: &TaskDefinition,
    ) -> Result<(u64, u64, u64), ApiError> {
        let common = match task {
            TaskDefinition::Ask(task) => &task.common,
            TaskDefinition::Assert(task) => &task.common,
            TaskDefinition::Call(call) => call.common(),
            TaskDefinition::Fork(task) => &task.common,
            TaskDefinition::Set(task) => &task.common,
            TaskDefinition::Switch(task) => &task.common,
            _ => {
                return Err(ApiError::definition_mismatch(
                    "task type is outside the Phase 2 budget profile",
                ));
            }
        };
        let retry = match common.retry.as_ref() {
            Some(OneOfRetryPolicyDefinitionOrReference::Retry(policy)) => Some(policy),
            Some(OneOfRetryPolicyDefinitionOrReference::Reference(reference)) => definition
                .use_
                .as_ref()
                .and_then(|components| components.retries.as_ref())
                .and_then(|policies| policies.get(reference)),
            None => None,
        };
        if common.retry.is_some() && retry.is_none() {
            return Err(ApiError::definition_mismatch(
                "workflow task references an unavailable retry policy",
            ));
        }
        let attempts = u64::from(
            retry
                .and_then(|policy| policy.limit.as_ref())
                .and_then(|limit| limit.attempt.as_ref())
                .and_then(|attempt| attempt.count)
                .unwrap_or(1)
                .max(1),
        );
        let own_nested = u64::from(matches!(
            task,
            TaskDefinition::Call(CallTaskDefinition::Mcp(_))
        ))
        .saturating_mul(attempts);
        let own_cost = common
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("costUnits"))
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .saturating_mul(attempts);
        if let TaskDefinition::Fork(fork) = task {
            let mut envelope = (attempts, own_nested, own_cost);
            for branch in &fork.fork.branches.entries {
                let Some((_, branch_task)) = branch.iter().next() else {
                    continue;
                };
                let branch = task_envelope(definition, branch_task)?;
                envelope.0 = envelope.0.saturating_add(branch.0);
                envelope.1 = envelope.1.saturating_add(branch.1);
                envelope.2 = envelope.2.saturating_add(branch.2);
            }
            Ok(envelope)
        } else {
            Ok((attempts, own_nested, own_cost))
        }
    }

    let mut envelope = (0_u64, 0_u64, 0_u64);
    for entry in &definition.do_.entries {
        let Some((_, task)) = entry.iter().next() else {
            continue;
        };
        let task = task_envelope(definition, task)?;
        envelope.0 = envelope.0.saturating_add(task.0);
        envelope.1 = envelope.1.saturating_add(task.1);
        envelope.2 = envelope.2.saturating_add(task.2);
    }
    Ok(envelope)
}

fn validate_phase2_task(
    task: &TaskDefinition,
    mode: InvocationMode,
    budget: &workflow_invocation_contract::InvocationBudget,
) -> Result<(), ApiError> {
    match task {
        TaskDefinition::Ask(_) if mode == InvocationMode::Async => Ok(()),
        TaskDefinition::Fork(fork) => {
            if fork.fork.branches.entries.is_empty()
                || fork.fork.branches.entries.len() > usize::from(budget.maximum_parallelism)
            {
                return Err(ApiError::definition_mismatch(
                    "fork branch count exceeds maximumParallelism",
                ));
            }
            for branch in &fork.fork.branches.entries {
                let Some((_, branch_task)) = branch.iter().next() else {
                    return Err(ApiError::definition_mismatch("fork branch is empty"));
                };
                if matches!(branch_task, TaskDefinition::Fork(_)) {
                    return Err(ApiError::definition_mismatch(
                        "nested forks are not supported in the Phase 2 profile",
                    ));
                }
                validate_phase2_task(branch_task, mode, budget)?;
            }
            Ok(())
        }
        TaskDefinition::Call(CallTaskDefinition::Http(call)) => {
            let endpoint_registered = call
                .common
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get("endpointRef"))
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty());
            if !endpoint_registered {
                return Err(ApiError::definition_mismatch(
                    "HTTP workflow tasks require a registered metadata.endpointRef",
                ));
            }
            let read_only = call.with.method.eq_ignore_ascii_case("GET")
                || call.with.method.eq_ignore_ascii_case("HEAD");
            validate_effect_policy(&call.common, read_only)
        }
        TaskDefinition::Call(CallTaskDefinition::Mcp(call)) => {
            if mcp_tool_name(&call.with).is_none_or(str::is_empty)
                || call.with.resource.is_some()
                || call.with.prompt.is_some()
            {
                return Err(ApiError::definition_mismatch(
                    "workflow-backed MCP tasks must call one registered tool",
                ));
            }
            validate_effect_policy(
                &call.common,
                call.common
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("readOnly"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            )
        }
        TaskDefinition::Assert(_) | TaskDefinition::Set(_) | TaskDefinition::Switch(_) => Ok(()),
        _ => Err(ApiError::definition_mismatch(
            "task type is outside the Phase 2 production orchestration profile",
        )),
    }
}

fn validate_effect_policy(
    common: &workflow_core::models::task::TaskDefinitionFields,
    read_only: bool,
) -> Result<(), ApiError> {
    if common.retry.is_some() && !read_only && common.idempotency_key.is_none() {
        return Err(ApiError::definition_mismatch(
            "write-capable retried tasks require idempotencyKey",
        ));
    }
    if !read_only {
        let metadata = common.metadata.as_ref();
        let compensation = metadata
            .and_then(|value| value.get("compensationTask"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        let approval = metadata
            .and_then(|value| value.get("approvalEvidenceDigest"))
            .and_then(Value::as_str)
            .is_some_and(|value| value.starts_with("sha256:") && value.len() == 71);
        if !compensation || !approval {
            return Err(ApiError::definition_mismatch(
                "write-capable tasks require compensationTask and approvalEvidenceDigest",
            ));
        }
    }
    Ok(())
}

fn validate_cel_expressions(engine: &RuleEngine, definition: &Value) -> Result<(), ApiError> {
    fn walk(
        engine: &RuleEngine,
        value: &Value,
        key: Option<&str>,
        in_export_as: bool,
        sequence: &mut usize,
    ) -> Result<(), ApiError> {
        match value {
            Value::Object(object) => {
                for (child_key, child_value) in object {
                    if key == Some("export") && child_key == "as" {
                        let Value::Object(exports) = child_value else {
                            return Err(ApiError::definition_mismatch(
                                "workflow export.as must be a flat object with string expressions",
                            ));
                        };
                        for (export_key, export_value) in exports {
                            if !export_value.is_string() {
                                return Err(ApiError::definition_mismatch(
                                    "workflow export.as must be a flat object with string expressions",
                                ));
                            }
                            walk(engine, export_value, Some(export_key), true, sequence)?;
                        }
                    } else {
                        walk(engine, child_value, Some(child_key), false, sequence)?;
                    }
                }
            }
            Value::Array(values) => {
                for value in values {
                    walk(engine, value, key, false, sequence)?;
                }
            }
            Value::String(expression) => {
                let expression = expression.trim();
                let executor_output_selector = in_export_as
                    && (expression == ".output"
                        || expression
                            .strip_prefix(".output.")
                            .is_some_and(|path| !path.is_empty()));
                if expression.contains("${ .")
                    || (expression.starts_with('.') && !executor_output_selector)
                {
                    return Err(ApiError::definition_mismatch(
                        "jq-style workflow expressions are not allowed for workflow-backed tools",
                    ));
                }
                if executor_output_selector {
                    return Ok(());
                }
                let direct_expression = matches!(key, Some("when") | Some("as"));
                if direct_expression || expression.starts_with("${{") {
                    let expression = expression
                        .strip_prefix("${{")
                        .and_then(|value| value.strip_suffix("}}"))
                        .unwrap_or(expression)
                        .trim();
                    *sequence += 1;
                    engine
                        .validate_cel_value_expression(
                            &format!("workflow-publication-{sequence}"),
                            expression,
                        )
                        .map_err(|error| {
                            ApiError::definition_mismatch(format!(
                                "invalid CEL expression at {key:?}: {error}"
                            ))
                        })?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    let mut sequence = 0;
    walk(engine, definition, None, false, &mut sequence)
}

fn supported_phase2_task_type(task: &TaskDefinition) -> Result<&'static str, ApiError> {
    match task {
        TaskDefinition::Ask(_) => Ok("ask"),
        TaskDefinition::Assert(_) => Ok("assert"),
        TaskDefinition::Fork(_) => Ok("fork"),
        TaskDefinition::Set(_) => Ok("set"),
        TaskDefinition::Switch(_) => Ok("switch"),
        TaskDefinition::Call(_) => Ok("call"),
        _ => Err(ApiError::definition_mismatch(
            "task type is outside the Phase 2 production orchestration profile",
        )),
    }
}

fn parse_state(value: &str) -> Result<InvocationState, ApiError> {
    match value {
        "ACCEPTED" => Ok(InvocationState::Accepted),
        "RUNNING" => Ok(InvocationState::Running),
        "WAITING" => Ok(InvocationState::Waiting),
        "COMPENSATING" => Ok(InvocationState::Compensating),
        "COMPLETED" => Ok(InvocationState::Completed),
        "FAILED" => Ok(InvocationState::Failed),
        "CANCELLED" => Ok(InvocationState::Cancelled),
        _ => Err(ApiError::database(sqlx::Error::Protocol(format!(
            "unknown invocation state {value}"
        )))),
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    error: InvocationError,
}

impl ApiError {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            error: InvocationError {
                code,
                message: message.into(),
                retryable: status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS,
                workflow_instance_id: None,
                correlation_id: "unavailable".to_string(),
            },
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ErrorCode::WorkflowStartRejected,
            message,
        )
    }
    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ErrorCode::WorkflowPolicyDenied,
            message,
        )
    }
    fn definition_mismatch(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            ErrorCode::WorkflowDefinitionMismatch,
            message,
        )
    }
    fn not_found(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ErrorCode::WorkflowInvocationUnavailable,
            message,
        )
    }
    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, ErrorCode::WorkflowTaskFailed, message)
    }
    fn database(error: sqlx::Error) -> Self {
        error!("workflow invocation database failure: {error}");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::WorkflowInvocationUnavailable,
            "workflow invocation service is unavailable",
        )
    }
    fn accept(error: InvocationAcceptError) -> Self {
        match error {
            InvocationAcceptError::IdempotencyConflict => Self::new(
                StatusCode::CONFLICT,
                ErrorCode::WorkflowIdempotencyConflict,
                "idempotency key is already bound to different input",
            ),
            InvocationAcceptError::Contract(error) => Self::bad_request(error.to_string()),
            InvocationAcceptError::Database(error) => Self::database(error),
        }
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(self.error)).into_response()
    }
}

async fn run_rule_test(
    State(state): State<RuleApiState>,
    Json(request): Json<RuleTestRequest>,
) -> Result<Json<RuleTestResponse>, (StatusCode, Json<Value>)> {
    let mut rule: Rule = serde_json::from_value(request.rule_body).map_err(bad_request)?;
    if request
        .test_mode
        .as_deref()
        .unwrap_or("conditions")
        .eq_ignore_ascii_case("conditions")
    {
        rule.actions = None;
    }

    let mut context = request.input_context;
    let passed = state
        .engine
        .execute_rule(&rule, &mut context)
        .await
        .map_err(|err| {
            error!("Rust rule test failed for {}: {}", rule.rule_id, err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("Rule engine failed: {}", err) })),
            )
        })?;

    let success = request
        .expected_result
        .is_none_or(|expected| expected == passed);
    Ok(Json(RuleTestResponse {
        executor: "rust".to_string(),
        passed,
        expected_result: request.expected_result,
        success,
        mutated_context: context,
    }))
}

fn bad_request<E: std::fmt::Display>(err: E) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": err.to_string() })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase2_accepts_read_tasks_and_requires_write_effect_policy() {
        let get: TaskDefinition = serde_yaml::from_str(
            "call: http\nwith:\n  method: GET\n  endpoint:\n    uri: https://example.com/customer",
        )
        .unwrap();
        assert_eq!(supported_phase2_task_type(&get).unwrap(), "call");
        let post: TaskDefinition = serde_yaml::from_str(
            "call: http\nwith:\n  method: POST\n  endpoint:\n    uri: https://example.com/customer",
        )
        .unwrap();
        let TaskDefinition::Call(CallTaskDefinition::Http(post)) = post else {
            panic!("expected HTTP call");
        };
        assert!(validate_effect_policy(&post.common, false).is_err());
        let protected: TaskDefinition = serde_yaml::from_str(
            "call: http\nwith:\n  method: POST\n  endpoint:\n    uri: https://example.com/customer\nidempotencyKey: '${{ requestId }}'\nretry:\n  limit:\n    attempt:\n      count: 3\nmetadata:\n  compensationTask: undoCustomer\n  approvalEvidenceDigest: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();
        let TaskDefinition::Call(CallTaskDefinition::Http(protected)) = protected else {
            panic!("expected protected HTTP call");
        };
        validate_effect_policy(&protected.common, false).expect("protected write policy");
    }

    #[test]
    fn phase2_resolves_legacy_and_canonical_mcp_tool_names() {
        let legacy: workflow_core::models::task::McpArguments =
            serde_json::from_value(json!({ "tool": "customer_lookup" })).unwrap();
        assert_eq!(mcp_tool_name(&legacy), Some("customer_lookup"));

        let canonical: workflow_core::models::task::McpArguments = serde_json::from_value(json!({
            "method": "tools/call",
            "parameters": {
                "name": "customer_lookup",
                "arguments": { "customerId": "C-1" }
            },
            "transport": {
                "http": { "endpoint": "https://gateway.example/mcp" }
            }
        }))
        .unwrap();
        assert_eq!(mcp_tool_name(&canonical), Some("customer_lookup"));
    }

    #[test]
    fn phase2_rejects_jq_and_accepts_cel_value_expressions() {
        let engine = RuleEngine::new(Arc::new(ActionRegistry::default()));
        let jq = json!({"output": {"as": ".customer.name"}});
        assert!(validate_cel_expressions(&engine, &jq).is_err());

        let cel = json!({"output": {"as": "customer.name"}});
        validate_cel_expressions(&engine, &cel).expect("valid CEL expression");
    }

    #[test]
    fn phase2_accepts_only_executor_output_selectors_in_task_exports() {
        let engine = RuleEngine::new(Arc::new(ActionRegistry::default()));
        let task_exports = json!({
            "do": [{
                "loadCustomerContext": {
                    "fork": {"branches": []},
                    "export": {
                        "as": {
                            "responses": ".output",
                            "profile": ".output.profile"
                        }
                    }
                }
            }]
        });
        validate_cel_expressions(&engine, &task_exports)
            .expect("executor-compatible task output selectors");

        let jq_export = json!({"export": {"as": {"customer": ".customer"}}});
        assert!(validate_cel_expressions(&engine, &jq_export).is_err());

        let output_selector_outside_export = json!({"output": {"as": ".output"}});
        assert!(validate_cel_expressions(&engine, &output_selector_outside_export).is_err());

        for invalid_export in [
            json!({"export": {"as": {"value": ".output."}}}),
            json!({"export": {"as": {"value": ".outputExtra"}}}),
            json!({"export": {"as": {"value": {"nested": ".output"}}}}),
            json!({"export": {"as": ".output"}}),
            json!({"export": {"as": {"value": 42}}}),
        ] {
            assert!(validate_cel_expressions(&engine, &invalid_export).is_err());
        }
    }
}
