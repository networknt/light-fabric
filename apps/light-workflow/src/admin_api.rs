use crate::rule_api::{InvocationIdentity, RuleApiState, authenticate};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::collections::HashSet;
use std::sync::Arc;
use uuid::Uuid;

const DEFAULT_PAGE_SIZE: i64 = 25;
const MAX_PAGE_SIZE: i64 = 100;

#[derive(Debug)]
pub enum RoleAuthorityError {
    Denied,
    Unavailable,
}

#[async_trait::async_trait]
pub trait RoleAuthority: Send + Sync {
    async fn current_roles(
        &self,
        user_authorization: &str,
        host_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<String>, RoleAuthorityError>;
}

#[async_trait::async_trait]
impl RoleAuthority for crate::credential_broker::CredentialBroker {
    async fn current_roles(
        &self,
        user_authorization: &str,
        host_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<String>, RoleAuthorityError> {
        let snapshot = self
            .current_workflow_roles(user_authorization)
            .await
            .map_err(|error| match error {
                light_client::unattended::ProviderFailure::ReauthorizationRequired => {
                    RoleAuthorityError::Denied
                }
                _ => RoleAuthorityError::Unavailable,
            })?;
        validate_role_snapshot(snapshot, host_id, user_id)
    }
}

fn validate_role_snapshot(
    snapshot: light_client::unattended::CurrentWorkflowRoles,
    host_id: Uuid,
    user_id: Uuid,
) -> Result<Vec<String>, RoleAuthorityError> {
    let checked_at = DateTime::parse_from_rfc3339(&snapshot.checked_at)
        .map_err(|_| RoleAuthorityError::Unavailable)?
        .with_timezone(&Utc);
    if snapshot.host_id != host_id
        || snapshot.user_id != user_id
        || snapshot.authority != "portal-current-role-membership"
        || (Utc::now() - checked_at).num_seconds().abs() > 30
        || snapshot
            .current_role_ids
            .iter()
            .any(|role| role.is_empty() || role.len() > 128)
        || snapshot
            .current_role_ids
            .iter()
            .collect::<HashSet<_>>()
            .len()
            != snapshot.current_role_ids.len()
    {
        return Err(RoleAuthorityError::Unavailable);
    }
    Ok(snapshot.current_role_ids)
}

pub(crate) async fn dispatch_tool(
    name: &str,
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Option<Result<Value, Response>> {
    macro_rules! body {
        ($ty:ty) => {
            match serde_json::from_value::<$ty>({
                let mut value = arguments.clone();
                if let Some(object) = value.as_object_mut() {
                    object.remove("processId");
                    object.remove("taskAsstId");
                }
                value
            }) {
                Ok(value) => value,
                Err(_) => {
                    return Some(Err(
                        AdminError::bad("tool arguments are invalid").into_response()
                    ))
                }
            }
        };
    }
    macro_rules! json_result {
        ($future:expr) => {
            Some($future.await.map(|Json(value)| value))
        };
    }
    let id = |field: &str| {
        arguments
            .get(field)
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok())
            .ok_or_else(|| AdminError::bad("tool identifier is invalid").into_response())
    };
    match name {
        "workflow_list_processes" => json_result!(list_processes(
            State(state),
            headers,
            Json(body!(ProcessListRequest))
        )),
        "workflow_get_process" => json_result!(get_process(
            State(state),
            headers,
            Path(match id("processId") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            })
        )),
        "workflow_get_task" => Some(
            get_native_task(state, headers, arguments)
                .await
                .map_err(IntoResponse::into_response),
        ),
        "workflow_delete_process" => Some(
            delete_native_process(state, headers, arguments)
                .await
                .map_err(IntoResponse::into_response),
        ),
        "workflow_add_process_note" => Some(
            add_process_note(state, headers, arguments)
                .await
                .map_err(IntoResponse::into_response),
        ),
        "workflow_list_process_notes" => Some(
            list_process_notes(state, headers, arguments)
                .await
                .map_err(IntoResponse::into_response),
        ),
        "workflow_list_features" => json_result!(list_features(
            State(state),
            headers,
            Json(body!(FeatureListRequest))
        )),
        "workflow_get_human_task_inbox_summary" => {
            json_result!(inbox_summary(State(state), headers, Json(arguments)))
        }
        "workflow_list_human_tasks" => json_result!(list_human_tasks(
            State(state),
            headers,
            Json(body!(HumanTaskListRequest))
        )),
        "workflow_get_human_task" => json_result!(get_human_task(
            State(state),
            headers,
            Path(match id("taskAsstId") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            })
        )),
        "workflow_claim_human_task" => {
            let id = match id("taskAsstId") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            json_result!(claim_human_task(
                State(state),
                headers,
                Path(id),
                Json(body!(ClaimRequest))
            ))
        }
        "workflow_release_human_task" => {
            let id = match id("taskAsstId") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            json_result!(release_human_task(
                State(state),
                headers,
                Path(id),
                Json(body!(VersionRequest))
            ))
        }
        "workflow_complete_human_task" => {
            let id = match id("taskAsstId") {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            };
            json_result!(complete_human_task(
                State(state),
                headers,
                Path(id),
                Json(body!(CompleteRequest))
            ))
        }
        "workflow_decide_tool_access" => Some(
            decide_tool_access(state, headers, arguments)
                .await
                .map_err(IntoResponse::into_response),
        ),
        _ => None,
    }
}

pub(crate) fn routes() -> Router<RuleApiState> {
    Router::new()
        .route("/v1/workflow-admin/processes/search", post(list_processes))
        .route(
            "/v1/workflow-admin/processes/{process_id}",
            get(get_process),
        )
        .route("/v1/workflow-admin/features/search", post(list_features))
        .route(
            "/v1/workflow-admin/human-tasks/inbox-summary",
            post(inbox_summary),
        )
        .route(
            "/v1/workflow-admin/human-tasks/search",
            post(list_human_tasks),
        )
        .route(
            "/v1/workflow-admin/human-tasks/{task_asst_id}",
            get(get_human_task),
        )
        .route(
            "/v1/workflow-admin/human-tasks/{task_asst_id}/claim",
            post(claim_human_task),
        )
        .route(
            "/v1/workflow-admin/human-tasks/{task_asst_id}/release",
            post(release_human_task),
        )
        .route(
            "/v1/workflow-admin/human-tasks/{task_asst_id}/complete",
            post(complete_human_task),
        )
}

#[derive(Debug)]
struct AdminError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    retryable: bool,
}
impl AdminError {
    fn bad(message: &'static str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "INVALID_ARGUMENT",
            message,
            retryable: false,
        }
    }
    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: "workflow resource was not found",
            retryable: false,
        }
    }
    fn conflict(code: &'static str, message: &'static str) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            message,
            retryable: matches!(code, "VERSION_CONFLICT" | "CLAIM_CONFLICT"),
        }
    }
    fn validation(message: &'static str) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "VALIDATION_FAILED",
            message,
            retryable: false,
        }
    }
    fn database(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "workflow admin operational-store failure");
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "STORE_UNAVAILABLE",
            message: "workflow operational store is unavailable",
            retryable: true,
        }
    }
    fn role_not_current() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "ROLE_NOT_CURRENT",
            message: "current role membership is required",
            retryable: false,
        }
    }
    fn unauthenticated() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "UNAUTHORIZED",
            message: "workflow caller identity is required",
            retryable: false,
        }
    }
    fn authority_unavailable() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "AUTHORITY_UNAVAILABLE",
            message: "current role authority is unavailable",
            retryable: true,
        }
    }
}
impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"code":self.code,"message":self.message,"retryable":self.retryable})),
        )
            .into_response()
    }
}

async fn identity(
    state: &RuleApiState,
    headers: &HeaderMap,
) -> Result<InvocationIdentity, Response> {
    authenticate(state, headers)
        .await
        .map(|(identity, _)| identity)
        .map_err(IntoResponse::into_response)
}

async fn current_roles(
    _authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
) -> Result<Vec<String>, AdminError> {
    if caller.user_authorization_exp <= Utc::now().timestamp() {
        return Err(AdminError::role_not_current());
    }
    let Some(role_claim) = caller.caller_claims.get("role").and_then(Value::as_str) else {
        return Ok(Vec::new());
    };
    if role_claim.len() > 4096 {
        return Err(AdminError::role_not_current());
    }
    let mut seen = HashSet::new();
    let mut roles = Vec::new();
    for role in role_claim.split(|ch: char| ch.is_ascii_whitespace() || ch == ',') {
        if !role.is_empty() {
            if role.len() > 128 || !seen.insert(role.to_owned()) {
                return Err(AdminError::role_not_current());
            }
            roles.push(role.to_owned());
        }
    }
    Ok(roles)
}

async fn inbox_roles(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
) -> Result<Vec<String>, AdminError> {
    let has_role_tasks: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM task_asst_t a JOIN task_info_t t
           ON t.host_id=a.host_id AND t.task_id=a.task_id
          WHERE a.host_id=$1 AND a.assignment_type='ROLE' AND a.active AND t.status_code='W')",
    )
    .bind(caller.host_id)
    .fetch_one(pool)
    .await
    .map_err(AdminError::database)?;
    if has_role_tasks {
        current_roles(authority, caller).await
    } else {
        Ok(Vec::new())
    }
}

async fn assignment_roles(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    id: Uuid,
) -> Result<Vec<String>, AdminError> {
    let assignment = sqlx::query_as::<_, (String, String)>(
        "SELECT assignment_type,assignment_id FROM task_asst_t WHERE host_id=$1 AND task_asst_id=$2",
    )
    .bind(caller.host_id)
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(AdminError::database)?
    .ok_or_else(AdminError::not_found)?;
    match assignment.0.as_str() {
        "USER" if assignment.1 == caller.end_user_subject => Ok(Vec::new()),
        "ROLE" => {
            let roles = current_roles(authority, caller).await?;
            if roles.contains(&assignment.1) {
                Ok(roles)
            } else {
                Err(AdminError::role_not_current())
            }
        }
        _ => Err(AdminError::not_found()),
    }
}

#[derive(Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageInput {
    cursor: Option<String>,
    page_size: Option<i64>,
}
fn page(input: Option<PageInput>) -> Result<(i64, i64), AdminError> {
    let input = input.unwrap_or_default();
    let size = input.page_size.unwrap_or(DEFAULT_PAGE_SIZE);
    if !(1..=MAX_PAGE_SIZE).contains(&size) {
        return Err(AdminError::bad("pageSize must be between 1 and 100"));
    }
    let offset = input
        .cursor
        .as_deref()
        .unwrap_or("0")
        .parse::<i64>()
        .map_err(|_| AdminError::bad("cursor is invalid"))?;
    if offset < 0 {
        return Err(AdminError::bad("cursor is invalid"));
    }
    Ok((offset, size))
}
fn process_state(code: &str) -> &'static str {
    match code {
        "C" => "COMPLETED",
        "F" => "FAILED",
        "W" => "WAITING",
        _ => "RUNNING",
    }
}
fn action_hint(allowed: bool, reason: Option<&str>) -> Value {
    json!({"allowed":allowed,"reason":reason})
}
fn workflow_name(snapshot: &Value) -> &str {
    snapshot
        .pointer("/document/name")
        .or_else(|| snapshot.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("workflow")
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessListRequest {
    page: Option<PageInput>,
    definition_id: Option<Uuid>,
    workflow_instance_id: Option<Uuid>,
    states: Option<Vec<String>>,
    created_from: Option<DateTime<Utc>>,
    created_to: Option<DateTime<Utc>>,
    sort: Option<String>,
}

fn process_json(row: &sqlx::postgres::PgRow) -> Result<Value, AdminError> {
    let process_id: Uuid = row.try_get("process_id").map_err(AdminError::database)?;
    let invocation_state: String = row
        .try_get("invocation_state")
        .map_err(AdminError::database)?;
    let terminal = matches!(
        invocation_state.as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED"
    );
    let snapshot: Value = row
        .try_get("definition_snapshot")
        .map_err(AdminError::database)?;
    Ok(json!({
        "processId":process_id,
        "workflowInstanceId":row.try_get::<Uuid,_>("workflow_instance_id").map_err(AdminError::database)?,
        "featureRunId":row.try_get::<Option<String>,_>("feature_id").map_err(AdminError::database)?,
        "definitionId":row.try_get::<Uuid,_>("wf_def_id").map_err(AdminError::database)?,
        "workflowName":workflow_name(&snapshot),
        "workflowVersion":row.try_get::<String,_>("workflow_version").map_err(AdminError::database)?,
        "processState":process_state(&row.try_get::<String,_>("process_status").map_err(AdminError::database)?),
        "invocationState":invocation_state,
        "invocationStateVersion":row.try_get::<i64,_>("state_version").map_err(AdminError::database)?,
        "createdAt":row.try_get::<DateTime<Utc>,_>("started_ts").map_err(AdminError::database)?,
        "updatedAt":row.try_get::<DateTime<Utc>,_>("updated_ts").map_err(AdminError::database)?,
        "deadline":row.try_get::<Option<DateTime<Utc>>,_>("deadline_ts").map_err(AdminError::database)?,
        "ownerDisplay":row.try_get::<String,_>("end_user_subject").map_err(AdminError::database)?,
        "safeFailureSummary":row.try_get::<Option<String>,_>("error_info").map_err(AdminError::database)?.map(|v|v.chars().take(512).collect::<String>()),
        "featureState":Value::Null,"vmState":Value::Null,"vmGeneration":Value::Null,
        "canCancelInvocation":action_hint(!terminal,terminal.then_some("INVOCATION_TERMINAL")),
        "canCancelFeature":action_hint(false,Some("USE_FEATURE_VIEW"))
    }))
}

// The INNER JOIN is deliberate: invocation identity supplies the frozen owner/disclosure predicate.
// Process-only rows stay excluded until an equivalent visibility policy is defined.
const PROCESS_SELECT: &str = "SELECT p.process_id,p.wf_def_id,p.status_code::text AS process_status,p.started_ts,COALESCE(p.update_ts,p.started_ts) AS updated_ts,p.deadline_ts,p.error_info,p.definition_snapshot,i.workflow_instance_id,i.workflow_version,i.state AS invocation_state,i.state_version,i.end_user_subject,s.feature_id FROM process_info_t p JOIN workflow_invocation_t i ON i.host_id=p.host_id AND i.process_id=p.process_id LEFT JOIN development_stage_t s ON s.host_id=p.host_id AND s.workflow_instance_id=i.workflow_instance_id";

async fn list_processes(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<ProcessListRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    let (offset, size) = page(request.page).map_err(IntoResponse::into_response)?;
    let allowed = [
        "ACCEPTED",
        "RUNNING",
        "WAITING",
        "COMPENSATING",
        "COMPLETED",
        "FAILED",
        "CANCELLED",
    ];
    if request
        .states
        .as_ref()
        .is_some_and(|v| v.is_empty() || v.iter().any(|s| !allowed.contains(&s.as_str())))
    {
        return Err(
            AdminError::bad("states contains an unsupported invocation state").into_response(),
        );
    }
    let descending = match request.sort.as_deref().unwrap_or("createdAt:desc") {
        "createdAt:desc" | "updatedAt:desc" => true,
        "createdAt:asc" | "updatedAt:asc" => false,
        _ => return Err(AdminError::bad("sort is invalid").into_response()),
    };
    let query = format!(
        "{PROCESS_SELECT} WHERE p.host_id=$1 AND p.active AND i.principal_subject=$2 AND i.end_user_subject=$3 AND ($4::uuid IS NULL OR p.wf_def_id=$4) AND ($5::uuid IS NULL OR i.workflow_instance_id=$5) AND ($6::text[] IS NULL OR i.state=ANY($6)) AND ($7::timestamptz IS NULL OR p.started_ts >= $7) AND ($8::timestamptz IS NULL OR p.started_ts <= $8) ORDER BY CASE WHEN $9 THEN p.started_ts END DESC,CASE WHEN NOT $9 THEN p.started_ts END ASC,p.process_id OFFSET $10 LIMIT $11"
    );
    let rows = sqlx::query(&query)
        .bind(caller.host_id)
        .bind(&caller.principal_subject)
        .bind(&caller.end_user_subject)
        .bind(request.definition_id)
        .bind(request.workflow_instance_id)
        .bind(request.states)
        .bind(request.created_from)
        .bind(request.created_to)
        .bind(descending)
        .bind(offset)
        .bind(size + 1)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| AdminError::database(e).into_response())?;
    let has_more = rows.len() as i64 > size;
    let processes = rows
        .iter()
        .take(size as usize)
        .map(process_json)
        .collect::<Result<Vec<_>, _>>()
        .map_err(IntoResponse::into_response)?;
    Ok(Json(
        json!({"processes":processes,"page":{"pageSize":processes.len(),"nextCursor":has_more.then(||(offset+size).to_string()),"hasMore":has_more}}),
    ))
}

async fn get_process(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(process_id): Path<Uuid>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    let query = format!(
        "{PROCESS_SELECT} WHERE p.host_id=$1 AND p.process_id=$2 AND p.active AND i.principal_subject=$3 AND i.end_user_subject=$4"
    );
    let row = sqlx::query(&query)
        .bind(caller.host_id)
        .bind(process_id)
        .bind(&caller.principal_subject)
        .bind(&caller.end_user_subject)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| AdminError::database(e).into_response())?
        .ok_or_else(|| AdminError::not_found().into_response())?;
    let process = process_json(&row).map_err(IntoResponse::into_response)?;
    let tasks=sqlx::query("SELECT t.task_id,t.task_type,t.status_code::text,a.task_asst_id FROM task_info_t t LEFT JOIN task_asst_t a ON a.host_id=t.host_id AND a.task_id=t.task_id AND a.active WHERE t.host_id=$1 AND t.process_id=$2 ORDER BY t.started_ts,t.task_id").bind(caller.host_id).bind(process_id).fetch_all(&state.pool).await.map_err(|e|AdminError::database(e).into_response())?.into_iter().map(|r|json!({"taskId":r.get::<Uuid,_>("task_id"),"taskAsstId":r.get::<Option<Uuid>,_>("task_asst_id"),"state":process_state(&r.get::<String,_>("status_code")),"type":r.get::<String,_>("task_type")})).collect::<Vec<_>>();
    let instance = process["workflowInstanceId"].as_str().map(str::to_owned);
    Ok(Json(
        json!({"process":process,"tasks":tasks,"links":{"status":instance.as_ref().map(|id|format!("/app/workflow/status?workflowInstanceId={id}")),"result":instance.as_ref().map(|id|format!("/app/workflow/result?workflowInstanceId={id}")),"feature":Value::Null,"audit":format!("/app/workflow/audit?processId={process_id}")}}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeTaskGet {
    task_id: Uuid,
}

async fn get_native_task(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, AdminError> {
    let input: NativeTaskGet =
        serde_json::from_value(arguments).map_err(|_| AdminError::bad("taskId is required"))?;
    let (caller, _) = authenticate(&state, &headers)
        .await
        .map_err(|_| AdminError::unauthenticated())?;
    get_native_task_for_caller(&state.pool, &caller, input).await
}

async fn get_native_task_for_caller(
    pool: &PgPool,
    caller: &InvocationIdentity,
    input: NativeTaskGet,
) -> Result<Value, AdminError> {
    let row = sqlx::query(
        "SELECT t.task_id,t.process_id,t.wf_instance_id,t.wf_task_id,t.task_type,
                t.status_code::text AS task_status,t.aggregate_version,t.started_ts,
                t.completed_ts,t.deadline_ts,t.result_code,t.effect_state,
                i.workflow_instance_id
           FROM task_info_t t JOIN workflow_invocation_t i
             ON i.host_id=t.host_id AND i.process_id=t.process_id
          WHERE t.host_id=$1 AND t.task_id=$2 AND i.principal_subject=$3
            AND i.end_user_subject=$4",
    )
    .bind(caller.host_id)
    .bind(input.task_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .fetch_optional(pool)
    .await
    .map_err(AdminError::database)?
    .ok_or_else(AdminError::not_found)?;
    let assignments = sqlx::query(
        "SELECT task_asst_id,assignment_status_code,assignment_type,assignment_id
           FROM task_asst_t WHERE host_id=$1 AND task_id=$2 ORDER BY assigned_ts,task_asst_id",
    )
    .bind(caller.host_id)
    .bind(input.task_id)
    .fetch_all(pool)
    .await
    .map_err(AdminError::database)?
    .into_iter()
    .map(|r| {
        json!({"taskAsstId":r.get::<Uuid,_>("task_asst_id"),
        "state":r.get::<String,_>("assignment_status_code"),
        "type":r.get::<String,_>("assignment_type"),
        "assigneeId":r.get::<String,_>("assignment_id")})
    })
    .collect::<Vec<_>>();
    let status: String = row.get("task_status");
    Ok(
        json!({"taskId":input.task_id,"processId":row.get::<Uuid,_>("process_id"),
        "workflowInstanceId":row.get::<Uuid,_>("workflow_instance_id"),
        "workflowTaskId":row.get::<String,_>("wf_task_id"),
        "type":row.get::<String,_>("task_type"),"state":process_state(&status),
        "taskVersion":row.get::<i64,_>("aggregate_version"),
        "startedAt":row.get::<DateTime<Utc>,_>("started_ts"),
        "completedAt":row.get::<Option<DateTime<Utc>>,_>("completed_ts"),
        "deadline":row.get::<Option<DateTime<Utc>>,_>("deadline_ts"),
        "resultCode":row.get::<Option<String>,_>("result_code"),
        "effectState":row.get::<String,_>("effect_state"),
        "assignments":assignments}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessNoteInput {
    process_id: Uuid,
    task_id: Option<Uuid>,
    text: String,
    idempotency_key: String,
}

async fn add_process_note(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, AdminError> {
    let input: ProcessNoteInput = serde_json::from_value(arguments)
        .map_err(|_| AdminError::bad("process note arguments are invalid"))?;
    if input.text.trim().is_empty()
        || input.text.len() > 4000
        || !(8..=128).contains(&input.idempotency_key.len())
    {
        return Err(AdminError::bad("note text or idempotency key is invalid"));
    }
    let (caller, _) = authenticate(&state, &headers)
        .await
        .map_err(|_| AdminError::unauthenticated())?;
    add_process_note_for_caller(&state.pool, &caller, input).await
}

async fn add_process_note_for_caller(
    pool: &PgPool,
    caller: &InvocationIdentity,
    input: ProcessNoteInput,
) -> Result<Value, AdminError> {
    let mut tx = pool.begin().await.map_err(AdminError::database)?;
    let instance: Uuid = sqlx::query_scalar(
        "SELECT i.workflow_instance_id FROM workflow_invocation_t i
          WHERE i.host_id=$1 AND i.process_id=$2 AND i.principal_subject=$3
            AND i.end_user_subject=$4 FOR SHARE",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AdminError::database)?
    .ok_or_else(AdminError::not_found)?;
    if let Some(task_id) = input.task_id {
        let linked: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM task_info_t WHERE host_id=$1 AND process_id=$2 AND task_id=$3)",
        ).bind(caller.host_id).bind(input.process_id).bind(task_id)
            .fetch_one(&mut *tx).await.map_err(AdminError::database)?;
        if !linked {
            return Err(AdminError::not_found());
        }
    }
    let note_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO workflow_process_note_t(host_id,note_id,process_id,workflow_instance_id,
                    task_id,idempotency_key,note_text,actor_subject)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT(host_id,process_id,idempotency_key) DO NOTHING",
    )
    .bind(caller.host_id)
    .bind(note_id)
    .bind(input.process_id)
    .bind(instance)
    .bind(input.task_id)
    .bind(&input.idempotency_key)
    .bind(input.text.trim())
    .bind(&caller.end_user_subject)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    let row = sqlx::query(
        "SELECT note_id,task_id,note_text,actor_subject,created_ts
                FROM workflow_process_note_t WHERE host_id=$1 AND process_id=$2
                  AND idempotency_key=$3",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&input.idempotency_key)
    .fetch_one(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    if row.get::<Option<Uuid>, _>("task_id") != input.task_id
        || row.get::<String, _>("note_text") != input.text.trim()
        || row.get::<String, _>("actor_subject") != caller.end_user_subject
    {
        return Err(AdminError::conflict(
            "IDEMPOTENCY_CONFLICT",
            "note retry differs",
        ));
    }
    tx.commit().await.map_err(AdminError::database)?;
    Ok(
        json!({"noteId":row.get::<Uuid,_>("note_id"),"processId":input.process_id,
        "workflowInstanceId":instance,"taskId":input.task_id,
        "text":input.text.trim(),"actor":caller.end_user_subject,
        "createdAt":row.get::<DateTime<Utc>,_>("created_ts")}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessNotesQuery {
    process_id: Uuid,
    page: Option<PageInput>,
}

async fn list_process_notes(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, AdminError> {
    let input: ProcessNotesQuery =
        serde_json::from_value(arguments).map_err(|_| AdminError::bad("processId is required"))?;
    let (offset, size) = page(input.page.clone())?;
    let (caller, _) = authenticate(&state, &headers)
        .await
        .map_err(|_| AdminError::unauthenticated())?;
    list_process_notes_for_caller(&state.pool, &caller, input, offset, size).await
}

async fn list_process_notes_for_caller(
    pool: &PgPool,
    caller: &InvocationIdentity,
    input: ProcessNotesQuery,
    offset: i64,
    size: i64,
) -> Result<Value, AdminError> {
    let owned: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workflow_invocation_t
            WHERE host_id=$1 AND process_id=$2 AND principal_subject=$3 AND end_user_subject=$4)",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .fetch_one(pool)
    .await
    .map_err(AdminError::database)?;
    if !owned {
        return Err(AdminError::not_found());
    }
    let rows = sqlx::query(
        "SELECT n.note_id,n.task_id,n.note_text,n.actor_subject,n.created_ts,
                n.workflow_instance_id FROM workflow_process_note_t n
                JOIN workflow_invocation_t i ON i.host_id=n.host_id AND i.process_id=n.process_id
                WHERE n.host_id=$1 AND n.process_id=$2 AND i.principal_subject=$3
                  AND i.end_user_subject=$4 ORDER BY n.created_ts,n.note_id OFFSET $5 LIMIT $6",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .bind(offset)
    .bind(size + 1)
    .fetch_all(pool)
    .await
    .map_err(AdminError::database)?;
    let has_more = rows.len() as i64 > size;
    let notes=rows.iter().take(size as usize).map(|r|json!({
        "noteId":r.get::<Uuid,_>("note_id"),"processId":input.process_id,
        "workflowInstanceId":r.get::<Uuid,_>("workflow_instance_id"),
        "taskId":r.get::<Option<Uuid>,_>("task_id"),"text":r.get::<String,_>("note_text"),
        "actor":r.get::<String,_>("actor_subject"),"createdAt":r.get::<DateTime<Utc>,_>("created_ts")
    })).collect::<Vec<_>>();
    Ok(json!({"notes":notes,"page":{"pageSize":notes.len(),
        "nextCursor":has_more.then(||(offset+size).to_string()),"hasMore":has_more}}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeDeleteInput {
    process_id: Uuid,
    expected_lifecycle_version: i64,
    reason: String,
    idempotency_key: String,
}

async fn deletion_receipt(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    operation: Uuid,
    version: i64,
) -> Result<Value, AdminError> {
    let (pending, failed, deleted, held, retained): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER(WHERE deletion_state IN ('DELETE_PENDING','DELETING')),
                count(*) FILTER(WHERE deletion_state='DELETE_FAILED'),
                count(*) FILTER(WHERE deletion_state='DELETED'),
                count(*) FILTER(WHERE legal_hold),
                count(*) FILTER(WHERE deletion_state='RETAINED' AND NOT legal_hold)
           FROM workflow_artifact_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(host)
    .bind(process)
    .fetch_one(&mut **tx)
    .await
    .map_err(AdminError::database)?;
    let cleanup = if pending > 0 || failed > 0 {
        "PENDING"
    } else if held > 0 || retained > 0 {
        "RETAINED"
    } else {
        "COMPLETE"
    };
    Ok(
        json!({"operationId":operation,"processId":process,"lifecycleVersion":version,
        "logicalDeletion":"RECORDED","artifactCleanup":cleanup,
        "artifacts":{"pending":pending,"failed":failed,"deleted":deleted,
            "legalHold":held,"retained":retained}}),
    )
}

async fn delete_native_process(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, AdminError> {
    let input: NativeDeleteInput = serde_json::from_value(arguments)
        .map_err(|_| AdminError::bad("process deletion arguments are invalid"))?;
    if input.expected_lifecycle_version < 1
        || input.reason.trim().is_empty()
        || input.reason.len() > 1000
        || !(8..=128).contains(&input.idempotency_key.len())
    {
        return Err(AdminError::bad(
            "deletion version, reason or idempotency key is invalid",
        ));
    }
    let (caller, _) = authenticate(&state, &headers)
        .await
        .map_err(|_| AdminError::unauthenticated())?;
    delete_native_process_for_caller(&state.pool, &caller, input).await
}

async fn delete_native_process_for_caller(
    pool: &PgPool,
    caller: &InvocationIdentity,
    input: NativeDeleteInput,
) -> Result<Value, AdminError> {
    let mut tx = pool.begin().await.map_err(AdminError::database)?;
    let row = sqlx::query(
        "SELECT i.workflow_instance_id,i.state,i.state_version,
               p.status_code::text AS process_status,p.active
          FROM workflow_invocation_t i JOIN process_info_t p
            ON p.host_id=i.host_id AND p.process_id=i.process_id
          WHERE i.host_id=$1 AND i.process_id=$2 AND i.principal_subject=$3
            AND i.end_user_subject=$4 FOR UPDATE OF i,p",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AdminError::database)?
    .ok_or_else(AdminError::not_found)?;
    let instance: Uuid = row.get("workflow_instance_id");
    let existing = sqlx::query(
        "SELECT operation_id,idempotency_key,reason,actor_subject,
                requested_lifecycle_version,lifecycle_version
             FROM workflow_process_deletion_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    if let Some(existing) = existing {
        if existing.get::<String, _>("idempotency_key") != input.idempotency_key
            || existing.get::<String, _>("reason") != input.reason.trim()
            || existing.get::<String, _>("actor_subject") != caller.end_user_subject
            || existing.get::<i64, _>("requested_lifecycle_version")
                != input.expected_lifecycle_version
        {
            return Err(AdminError::conflict(
                "IDEMPOTENCY_CONFLICT",
                "deletion request differs",
            ));
        }
        return deletion_receipt(
            &mut tx,
            caller.host_id,
            input.process_id,
            existing.get("operation_id"),
            existing.get("lifecycle_version"),
        )
        .await;
    }
    if row.get::<i64, _>("state_version") != input.expected_lifecycle_version {
        return Err(AdminError::conflict(
            "VERSION_CONFLICT",
            "process lifecycle version changed",
        ));
    }
    if !matches!(
        row.get::<String, _>("state").as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED"
    ) || !matches!(row.get::<String, _>("process_status").as_str(), "C" | "F")
    {
        return Err(AdminError::conflict(
            "PROCESS_NOT_TERMINAL",
            "native process is not terminal",
        ));
    }
    let held_resources: bool=sqlx::query_scalar("SELECT
        EXISTS(SELECT 1 FROM task_info_t WHERE host_id=$1 AND process_id=$2
               AND (status_code NOT IN ('C','F') OR locked='Y'))
        OR EXISTS(SELECT 1 FROM workflow_agent_job_t WHERE host_id=$1 AND workflow_process_id=$2
               AND state IN ('PENDING','TURN_CREATED','RUNNING','UNKNOWN'))
        OR EXISTS(SELECT 1 FROM workflow_approval_t WHERE host_id=$1 AND process_id=$2
               AND state IN ('REQUESTED','APPROVED'))
        OR EXISTS(SELECT 1 FROM workflow_action_authority_t
               WHERE host_id=$1 AND run_id=$3 AND reserved>0)
        OR EXISTS(SELECT 1 FROM workflow_action_permit_t p
               JOIN workflow_action_dispatch_t d ON d.host_id=p.host_id AND d.action_id=p.action_id
               WHERE p.host_id=$1 AND p.run_id=$3 AND d.reservation_held)
        OR EXISTS(SELECT 1 FROM development_stage_t s
               JOIN development_feature_t f ON f.host_id=s.host_id AND f.feature_id=s.feature_id
               WHERE s.host_id=$1 AND s.process_id=$2
                 AND COALESCE((f.record#>>'{vm,released}')::boolean,FALSE)=FALSE)
        OR EXISTS(SELECT 1 FROM workflow_tool_access_approval_run_t
               WHERE host_id=$1 AND workflow_instance_id=$3 AND delivery_state IN ('PENDING','BLOCKED'))")
        .bind(caller.host_id).bind(input.process_id).bind(instance)
        .fetch_one(&mut *tx).await.map_err(AdminError::database)?;
    if held_resources {
        return Err(AdminError::conflict(
            "RESOURCE_HELD",
            "native process still owns resources",
        ));
    }
    let operation = Uuid::now_v7();
    let next_version = input.expected_lifecycle_version + 1;
    sqlx::query(
        "INSERT INTO workflow_process_deletion_t(host_id,process_id,
        workflow_instance_id,operation_id,idempotency_key,requested_lifecycle_version,
        lifecycle_version,reason,actor_subject) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(instance)
    .bind(operation)
    .bind(&input.idempotency_key)
    .bind(input.expected_lifecycle_version)
    .bind(next_version)
    .bind(input.reason.trim())
    .bind(&caller.end_user_subject)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    sqlx::query(
        "UPDATE workflow_invocation_t SET state_version=$3,updated_ts=clock_timestamp()
                WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(caller.host_id)
    .bind(instance)
    .bind(next_version)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    sqlx::query(
        "UPDATE process_info_t SET active=FALSE,aggregate_version=aggregate_version+1,
                update_ts=clock_timestamp(),update_user=$3 WHERE host_id=$1 AND process_id=$2",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(&caller.end_user_subject)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    sqlx::query(
        "UPDATE workflow_artifact_t SET deletion_state='DELETE_PENDING',
                deletion_next_retry_ts=clock_timestamp(),updated_ts=clock_timestamp(),
                deletion_evidence=COALESCE(deletion_evidence,'{}'::jsonb)
                    || jsonb_build_object('processDeletionOperation',$3::text)
                WHERE host_id=$1 AND process_id=$2 AND legal_hold=FALSE
                  AND deletion_state='RETAINED' AND retain_until_ts<=clock_timestamp()",
    )
    .bind(caller.host_id)
    .bind(input.process_id)
    .bind(operation)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    let receipt = deletion_receipt(
        &mut tx,
        caller.host_id,
        input.process_id,
        operation,
        next_version,
    )
    .await?;
    tx.commit().await.map_err(AdminError::database)?;
    Ok(receipt)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FeatureListRequest {
    page: Option<PageInput>,
    states: Option<Vec<String>>,
    holds_vm: Option<bool>,
}

async fn list_features(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<FeatureListRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    let (offset, size) = page(request.page).map_err(IntoResponse::into_response)?;
    let rows = sqlx::query(
        "SELECT record FROM development_feature_t
          WHERE host_id=$1 AND principal_subject=$2 AND end_user_subject=$3
            AND ($4::text[] IS NULL OR record->>'state'=ANY($4))
            AND ($5::boolean IS NULL OR ((record#>>'{vm,released}')::boolean=FALSE)=$5)
          ORDER BY updated_ts DESC,feature_id OFFSET $6 LIMIT $7",
    )
    .bind(caller.host_id)
    .bind(&caller.principal_subject)
    .bind(&caller.end_user_subject)
    .bind(request.states)
    .bind(request.holds_vm)
    .bind(offset)
    .bind(size + 1)
    .fetch_all(&state.pool)
    .await
    .map_err(|e| AdminError::database(e).into_response())?;
    let has_more = rows.len() as i64 > size;
    let features = rows.iter().take(size as usize).map(|row| {
        let record: Value = row.get("record");
        let state_value = record.get("state").cloned().unwrap_or(Value::Null);
        let released = record.pointer("/vm/released").and_then(Value::as_bool).unwrap_or(false);
        let release_pending = record.pointer("/vm/releasePending").and_then(Value::as_bool).unwrap_or(false);
        let terminal = matches!(state_value.as_str(), Some("completed" | "cancelled" | "failed"));
        json!({"featureRunId":record.get("featureRunId"),"featureVersion":record.get("version"),"state":state_value,
            "holdsVm":!released,"vmState":if released{"RELEASED"}else if release_pending{"RELEASE_PENDING"}else{"RESERVED"},
            "vmGeneration":record.pointer("/vm/generation"),"activeWorkflowInstanceId":record.pointer("/activeClaim/workflowInstanceId"),
            "canCancelFeature":action_hint(!terminal,terminal.then_some("FEATURE_TERMINAL"))})
    }).collect::<Vec<_>>();
    Ok(Json(
        json!({"features":features,"page":{"pageSize":features.len(),"nextCursor":has_more.then(||(offset+size).to_string()),"hasMore":has_more}}),
    ))
}

const TASK_SELECT: &str = "SELECT a.*,t.process_id,t.wf_instance_id,t.status_code::text AS task_status,t.deadline_ts,t.task_output,t.task_type FROM task_asst_t a JOIN task_info_t t ON t.host_id=a.host_id AND t.task_id=a.task_id";
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HumanTaskListRequest {
    page: Option<PageInput>,
    tab_id: Option<String>,
    include_claimed: Option<bool>,
    include_claimed_by_others: Option<bool>,
}

fn task_json(row: &sqlx::postgres::PgRow, subject: &str) -> Result<Value, AdminError> {
    let status: String = row
        .try_get("assignment_status_code")
        .map_err(AdminError::database)?;
    let claimed_by: Option<String> = row.try_get("claimed_by").map_err(AdminError::database)?;
    let expires: Option<DateTime<Utc>> = row
        .try_get("claim_expires_ts")
        .map_err(AdminError::database)?;
    let mine = claimed_by.as_deref() == Some(subject) && expires.is_some_and(|v| v > Utc::now());
    let available =
        status == "ASSIGNED" || (status == "CLAIMED" && expires.is_some_and(|v| v <= Utc::now()));
    let output: Value = row.try_get("task_output").map_err(AdminError::database)?;
    let task_status: String = row.try_get("task_status").map_err(AdminError::database)?;
    let waiting = task_status == "W";
    let can_claim = waiting && available;
    let can_mutate_claim = waiting && mine;
    let typed_approval = row
        .try_get::<Option<String>, _>("reason_code")
        .map_err(AdminError::database)?
        .as_deref()
        == Some("grant-tools-to-workflow");
    Ok(
        json!({"taskAsstId":row.try_get::<Uuid,_>("task_asst_id").map_err(AdminError::database)?,"taskId":row.try_get::<Uuid,_>("task_id").map_err(AdminError::database)?,"processId":row.try_get::<Uuid,_>("process_id").map_err(AdminError::database)?,"workflowInstanceId":Uuid::parse_str(&row.try_get::<String,_>("wf_instance_id").map_err(AdminError::database)?).ok(),"assignmentVersion":row.try_get::<i64,_>("aggregate_version").map_err(AdminError::database)?,"assignmentType":row.try_get::<String,_>("assignment_type").map_err(AdminError::database)?,"assignmentId":row.try_get::<String,_>("assignment_id").map_err(AdminError::database)?,"assignmentLabel":row.try_get::<String,_>("assignee_id").map_err(AdminError::database)?,"assignmentStatus":status,"taskStatus":process_state(&task_status),"assignedAt":row.try_get::<DateTime<Utc>,_>("assigned_ts").map_err(AdminError::database)?,"claimedBy":claimed_by,"claimExpiresAt":expires,"deadline":row.try_get::<Option<DateTime<Utc>>,_>("deadline_ts").map_err(AdminError::database)?,"category":row.try_get::<Option<String>,_>("category_code").map_err(AdminError::database)?,"reason":row.try_get::<Option<String>,_>("reason_code").map_err(AdminError::database)?,"prompt":output.pointer("/ask/prompt").and_then(Value::as_str),"canClaim":action_hint(can_claim,(!can_claim).then_some(if waiting{"TASK_NOT_AVAILABLE"}else{"TASK_NOT_WAITING"})),"canRelease":action_hint(can_mutate_claim,(!can_mutate_claim).then_some(if waiting{"CLAIM_NOT_OWNED"}else{"TASK_NOT_WAITING"})),"canComplete":action_hint(can_mutate_claim && !typed_approval, (typed_approval || !can_mutate_claim).then_some(if typed_approval{"TYPED_APPROVAL_REQUIRED"}else if waiting{"CLAIM_REQUIRED"}else{"TASK_NOT_WAITING"})),"readOnly":!can_claim&&!can_mutate_claim}),
    )
}

async fn inbox_summary(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    inbox_summary_for_caller(&state.pool, state.role_authority.as_ref(), &caller)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn inbox_summary_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
) -> Result<Value, AdminError> {
    let roles = inbox_roles(pool, authority, caller).await?;
    let rows = sqlx::query(
        "SELECT a.assignment_type,a.assignment_id,count(*) AS count
         FROM task_asst_t a JOIN task_info_t t ON t.host_id=a.host_id AND t.task_id=a.task_id
         WHERE a.host_id=$1 AND a.active AND t.status_code='W'
           AND ((a.assignment_type='USER' AND a.assignment_id=$2)
             OR (a.assignment_type='ROLE' AND a.assignment_id=ANY($3::text[])))
         GROUP BY a.assignment_type,a.assignment_id",
    )
    .bind(caller.host_id)
    .bind(&caller.end_user_subject)
    .bind(&roles)
    .fetch_all(pool)
    .await
    .map_err(AdminError::database)?;
    let mut user_count = 0_i64;
    let mut role_counts = std::collections::HashMap::new();
    for row in rows {
        let kind: String = row.get("assignment_type");
        let id: String = row.get("assignment_id");
        let count: i64 = row.get("count");
        if kind == "USER" {
            user_count += count;
        } else {
            role_counts.insert(id, count);
        }
    }
    let total = user_count + role_counts.values().sum::<i64>();
    let mut tabs = vec![
        json!({"id":"all","label":"All","assignmentType":Value::Null,"assignmentId":Value::Null,"count":total}),
        json!({"id":"user:self","label":"Assigned to me","assignmentType":"USER","assignmentId":caller.end_user_subject,"count":user_count}),
    ];
    for role in roles {
        tabs.push(json!({"id":format!("role:{role}"),"label":role,"assignmentType":"ROLE","assignmentId":role,"count":role_counts.get(&role).copied().unwrap_or(0)}));
    }
    Ok(json!({"tabs":tabs}))
}

async fn list_human_tasks(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<HumanTaskListRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    list_for_caller(
        &state.pool,
        state.role_authority.as_ref(),
        &caller,
        &request,
    )
    .await
    .map(Json)
    .map_err(IntoResponse::into_response)
}

async fn list_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    request: &HumanTaskListRequest,
) -> Result<Value, AdminError> {
    let roles = inbox_roles(pool, authority, caller).await?;
    let tab = request.tab_id.as_deref().unwrap_or("all");
    let role_tab = tab.strip_prefix("role:");
    if !matches!(tab, "all" | "user:self")
        && !role_tab.is_some_and(|role| roles.iter().any(|r| r == role))
    {
        return Err(AdminError::bad("tabId is not available"));
    }
    let (offset, size) = page(request.page.clone())?;
    let query = format!(
        "{TASK_SELECT} WHERE a.host_id=$1 AND a.active AND t.status_code='W'
         AND ((a.assignment_type='USER' AND a.assignment_id=$2)
           OR (a.assignment_type='ROLE' AND a.assignment_id=ANY($3::text[])))
         AND ($4 OR a.assignment_status_code<>'CLAIMED')
         AND ($5 OR a.claimed_by IS NULL OR a.claimed_by=$2 OR a.claim_expires_ts<=CURRENT_TIMESTAMP)
         AND ($8::text IS NULL OR (a.assignment_type='ROLE' AND a.assignment_id=$8))
         AND (NOT $9::boolean OR a.assignment_type='USER')
         ORDER BY a.assigned_ts DESC,a.task_asst_id OFFSET $6 LIMIT $7"
    );
    let rows = sqlx::query(&query)
        .bind(caller.host_id)
        .bind(&caller.end_user_subject)
        .bind(&roles)
        .bind(request.include_claimed.unwrap_or(true))
        .bind(request.include_claimed_by_others.unwrap_or(false))
        .bind(offset)
        .bind(size + 1)
        .bind(role_tab)
        .bind(tab == "user:self")
        .fetch_all(pool)
        .await
        .map_err(AdminError::database)?;
    let has_more = rows.len() as i64 > size;
    let tasks = rows
        .iter()
        .take(size as usize)
        .map(|r| task_json(r, &caller.end_user_subject))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(
        json!({"humanTasks":tasks,"page":{"pageSize":tasks.len(),"nextCursor":has_more.then(||(offset+size).to_string()),"hasMore":has_more}}),
    )
}

async fn load_task(
    pool: &PgPool,
    host_id: Uuid,
    task_asst_id: Uuid,
    subject: &str,
    roles: &[String],
) -> Result<sqlx::postgres::PgRow, AdminError> {
    let query = format!(
        "{TASK_SELECT} WHERE a.host_id=$1 AND a.task_asst_id=$2
         AND ((a.assignment_type='USER' AND a.assignment_id=$3)
           OR (a.assignment_type='ROLE' AND a.assignment_id=ANY($4::text[])))"
    );
    sqlx::query(&query)
        .bind(host_id)
        .bind(task_asst_id)
        .bind(subject)
        .bind(roles)
        .fetch_optional(pool)
        .await
        .map_err(AdminError::database)?
        .ok_or_else(AdminError::not_found)
}
async fn get_human_task(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    get_for_caller(&state.pool, state.role_authority.as_ref(), &caller, id)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn get_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    id: Uuid,
) -> Result<Value, AdminError> {
    let roles = assignment_roles(pool, authority, caller, id).await?;
    let row = load_task(pool, caller.host_id, id, &caller.end_user_subject, &roles).await?;
    let task = task_json(&row, &caller.end_user_subject)?;
    let output: Value = row.try_get("task_output").map_err(AdminError::database)?;
    Ok(
        json!({"task":task,"ask":output.get("ask").cloned().unwrap_or_else(||json!({"mode":"text","prompt":"Input required","required":true})),"contextSummary":{}}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimRequest {
    assignment_version: i64,
    claim_minutes: Option<i64>,
}
async fn claim_human_task(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(r): Json<ClaimRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    claim_for_caller(&state.pool, state.role_authority.as_ref(), &caller, id, &r)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn claim_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    id: Uuid,
    r: &ClaimRequest,
) -> Result<Value, AdminError> {
    let minutes = r.claim_minutes.unwrap_or(30);
    if !(1..=120).contains(&minutes) {
        return Err(AdminError::bad("claimMinutes must be between 1 and 120"));
    }
    let roles = assignment_roles(pool, authority, caller, id).await?;
    let row=sqlx::query("UPDATE task_asst_t a SET assignment_status_code='CLAIMED',claimed_by=$1,claimed_ts=CURRENT_TIMESTAMP,claim_expires_ts=CURRENT_TIMESTAMP+make_interval(mins=>$2::int),aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$3 AND task_asst_id=$4 AND ((assignment_type='USER' AND assignment_id=$1) OR (assignment_type='ROLE' AND assignment_id=ANY($6::text[]))) AND active AND aggregate_version=$5 AND (assignment_status_code='ASSIGNED' OR (assignment_status_code='CLAIMED' AND claim_expires_ts<=CURRENT_TIMESTAMP)) AND EXISTS(SELECT 1 FROM task_info_t t WHERE t.host_id=a.host_id AND t.task_id=a.task_id AND t.status_code='W') RETURNING aggregate_version,claim_expires_ts").bind(&caller.end_user_subject).bind(minutes as i32).bind(caller.host_id).bind(id).bind(r.assignment_version).bind(&roles).fetch_optional(pool).await.map_err(AdminError::database)?.ok_or_else(||AdminError::conflict("CLAIM_CONFLICT","task is no longer available to claim"))?;
    Ok(
        json!({"taskAsstId":id,"assignmentVersion":row.get::<i64,_>("aggregate_version"),"assignmentStatus":"CLAIMED","claimExpiresAt":row.get::<DateTime<Utc>,_>("claim_expires_ts")}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VersionRequest {
    assignment_version: i64,
}
async fn release_human_task(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(r): Json<VersionRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    release_for_caller(&state.pool, state.role_authority.as_ref(), &caller, id, &r)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn release_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    id: Uuid,
    r: &VersionRequest,
) -> Result<Value, AdminError> {
    let roles = assignment_roles(pool, authority, caller, id).await?;
    let version=sqlx::query_scalar::<_,i64>("UPDATE task_asst_t SET assignment_status_code='ASSIGNED',claimed_by=NULL,claimed_ts=NULL,claim_expires_ts=NULL,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$2 AND task_asst_id=$3 AND ((assignment_type='USER' AND assignment_id=$1) OR (assignment_type='ROLE' AND assignment_id=ANY($5::text[]))) AND active AND aggregate_version=$4 AND assignment_status_code='CLAIMED' AND claimed_by=$1 AND claim_expires_ts>CURRENT_TIMESTAMP RETURNING aggregate_version").bind(&caller.end_user_subject).bind(caller.host_id).bind(id).bind(r.assignment_version).bind(&roles).fetch_optional(pool).await.map_err(AdminError::database)?.ok_or_else(||AdminError::conflict("CLAIM_NOT_OWNED","active claim is not owned by this caller"))?;
    Ok(json!({"taskAsstId":id,"assignmentVersion":version,"assignmentStatus":"ASSIGNED"}))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompleteRequest {
    assignment_version: i64,
    decision: Value,
    comment: Option<String>,
    idempotency_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolAccessDecisionInput {
    task_asst_id: Uuid,
    assignment_version: i64,
    request_id: Uuid,
    request_digest: String,
    idempotency_key: String,
    decision: String,
    comment: Option<String>,
}

async fn decide_tool_access(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, AdminError> {
    let input: ToolAccessDecisionInput = serde_json::from_value(arguments)
        .map_err(|_| AdminError::bad("typed approval arguments are invalid"))?;
    if !matches!(input.decision.as_str(), "APPROVE" | "REJECT")
        || input.request_digest.len() != 71
        || !input.request_digest.starts_with("sha256:")
        || input
            .comment
            .as_ref()
            .is_some_and(|value| value.len() > 2000)
        || input.idempotency_key.len() < 8
        || input.idempotency_key.len() > 128
    {
        return Err(AdminError::bad("typed approval decision is invalid"));
    }
    let (caller, _) = authenticate(&state, &headers)
        .await
        .map_err(|_| AdminError::unauthenticated())?;
    let roles = assignment_roles(
        &state.pool,
        state.role_authority.as_ref(),
        &caller,
        input.task_asst_id,
    )
    .await?;
    if !roles.iter().any(|role| role == "genai-admin") {
        return Err(AdminError::role_not_current());
    }
    let approver = caller
        .end_user_subject
        .parse::<Uuid>()
        .map_err(|_| AdminError::bad("approver identity is invalid"))?;
    let digest = workflow_invocation_contract::canonical_sha256(&json!({
        "hostId":caller.host_id,"requestId":input.request_id,
        "requestDigest":input.request_digest,"taskAsstId":input.task_asst_id,
        "idempotencyKey":input.idempotency_key,
        "decision":input.decision,"comment":input.comment,
        "approverUserId":approver,
    }))
    .map_err(|_| AdminError::bad("typed approval cannot be canonicalized"))?;
    let mut tx = state.pool.begin().await.map_err(AdminError::database)?;
    let row = lock_assignment(
        &mut tx,
        caller.host_id,
        input.task_asst_id,
        &caller.end_user_subject,
        &roles,
    )
    .await?;
    if row.get::<Option<String>, _>("reason_code").as_deref() != Some("grant-tools-to-workflow") {
        return Err(AdminError::conflict(
            "APPROVAL_TASK_MISMATCH",
            "assignment is not this approval task",
        ));
    }
    let existing: Option<(
        Uuid,
        String,
        String,
        Uuid,
        Option<Uuid>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<DateTime<Utc>>,
    )> = sqlx::query_as(
        "SELECT workflow_instance_id,request_digest,approval_definition_digest,
                requester_user_id,decision_id,decision_payload_digest,delivery_state,
                decision_idempotency_key,portal_outcome,portal_committed_ts
                FROM workflow_tool_access_approval_run_t
                WHERE host_id=$1 AND request_id=$2 FOR UPDATE",
    )
    .bind(caller.host_id)
    .bind(input.request_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    let Some((
        run,
        request_digest,
        _,
        _,
        previous_id,
        previous_digest,
        previous_state,
        previous_key,
        portal_outcome,
        acknowledged_at,
    )) = existing
    else {
        return Err(AdminError::not_found());
    };
    if row.get::<String, _>("wf_instance_id") != run.to_string()
        || request_digest != input.request_digest
    {
        return Err(AdminError::conflict(
            "APPROVAL_REQUEST_MISMATCH",
            "approval request linkage differs",
        ));
    }
    if let Some(previous_id) = previous_id {
        if previous_key.as_deref() != Some(input.idempotency_key.as_str())
            || previous_digest.as_deref() != Some(digest.as_str())
        {
            return Err(AdminError::conflict(
                "ALREADY_DECIDED",
                "approval decision conflicts",
            ));
        }
        return Ok(
            json!({"decisionId":previous_id,"requestId":input.request_id,
            "taskAsstId":input.task_asst_id,
            "state":if previous_state.as_deref()==Some("ACKED") {portal_outcome.as_deref().unwrap_or("STALE")} else {"PENDING_DELIVERY"},
            "assignmentVersion":row.get::<i64,_>("aggregate_version"),"portalCommittedAt":acknowledged_at}),
        );
    }
    if row.get::<String, _>("assignment_status_code") != "CLAIMED"
        || row.get::<Option<String>, _>("claimed_by").as_deref() != Some(&caller.end_user_subject)
        || row
            .get::<Option<DateTime<Utc>>, _>("claim_expires_ts")
            .is_none_or(|time| time <= Utc::now())
        || row.get::<i64, _>("aggregate_version") != input.assignment_version
        || deadline_expired(
            row.get::<Option<DateTime<Utc>>, _>("deadline_ts"),
            Utc::now(),
        )
    {
        return Err(AdminError::conflict(
            "CLAIM_NOT_OWNED",
            "approval claim is stale",
        ));
    }
    validate_decision(
        row.get::<Value, _>("task_output")
            .get("ask")
            .unwrap_or(&Value::Null),
        &Value::String(input.decision.clone()),
        input.comment.as_deref(),
    )?;
    let task_id: Uuid = row.get("task_id");
    let decision_id = Uuid::now_v7();
    sqlx::query(
        "UPDATE workflow_tool_access_approval_run_t SET decision_id=$1,decision_kind=$2,
        decision_idempotency_key=$3,decision_payload_digest=$4,approver_user_id=$5,
        approver_claims_digest=$6,task_id=$7,task_asst_id=$8,decision_comment=$9,
        delivery_state='PENDING',decision_ts=CURRENT_TIMESTAMP
        WHERE host_id=$10 AND request_id=$11 AND decision_id IS NULL",
    )
    .bind(decision_id)
    .bind(&input.decision)
    .bind(&input.idempotency_key)
    .bind(&digest)
    .bind(approver)
    .bind(&caller.caller_claims_digest)
    .bind(task_id)
    .bind(input.task_asst_id)
    .bind(&input.comment)
    .bind(caller.host_id)
    .bind(input.request_id)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    sqlx::query(
        "UPDATE task_asst_t SET assignment_status_code='DECISION_PENDING',
        aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP
        WHERE host_id=$1 AND task_asst_id=$2 AND assignment_status_code='CLAIMED'",
    )
    .bind(caller.host_id)
    .bind(input.task_asst_id)
    .execute(&mut *tx)
    .await
    .map_err(AdminError::database)?;
    tx.commit().await.map_err(AdminError::database)?;
    Ok(
        json!({"decisionId":decision_id,"requestId":input.request_id,
        "taskAsstId":input.task_asst_id,"state":"PENDING_DELIVERY",
        "assignmentVersion":input.assignment_version+1,"portalCommittedAt":null}),
    )
}
fn validate_decision(
    ask: &Value,
    decision: &Value,
    comment: Option<&str>,
) -> Result<(), AdminError> {
    if ask
        .get("required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && (decision.is_null() || decision.as_str() == Some(""))
    {
        return Err(AdminError::validation("a decision is required"));
    }
    if ask
        .get("commentRequired")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && comment.is_none_or(|v| v.trim().is_empty())
    {
        return Err(AdminError::validation("a comment is required"));
    }
    if let Some(options) = ask.get("options").and_then(Value::as_array) {
        let allowed = |v: &Value| options.iter().any(|o| o.get("value") == Some(v));
        if !decision
            .as_array()
            .map(|v| v.iter().all(allowed))
            .unwrap_or_else(|| allowed(decision))
        {
            return Err(AdminError::validation(
                "decision is not one of the supported options",
            ));
        }
    }
    if let Some(schema) = ask.get("schema") {
        let validator = jsonschema::validator_for(schema)
            .map_err(|_| AdminError::validation("ask schema is invalid"))?;
        if !validator.is_valid(decision) {
            return Err(AdminError::validation(
                "decision does not satisfy the ask schema",
            ));
        }
    }
    Ok(())
}

fn deadline_expired(deadline: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    deadline.is_some_and(|deadline| deadline <= now)
}

async fn complete_human_task(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(r): Json<CompleteRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    complete_for_caller(&state.pool, state.role_authority.as_ref(), &caller, id, &r)
        .await
        .map(Json)
        .map_err(IntoResponse::into_response)
}

async fn complete_for_caller(
    pool: &PgPool,
    authority: Option<&Arc<dyn RoleAuthority>>,
    caller: &InvocationIdentity,
    id: Uuid,
    r: &CompleteRequest,
) -> Result<Value, AdminError> {
    if r.comment.as_ref().is_some_and(|v| v.len() > 4000) {
        return Err(AdminError::validation("comment exceeds 4000 characters"));
    }
    if r.idempotency_key
        .as_ref()
        .is_some_and(|v| v.len() < 8 || v.len() > 128)
    {
        return Err(AdminError::bad(
            "idempotencyKey must contain 8 to 128 characters",
        ));
    }
    let roles = assignment_roles(pool, authority, caller, id).await?;
    let mut tx = pool.begin().await.map_err(AdminError::database)?;
    let row = lock_assignment(
        &mut tx,
        caller.host_id,
        id,
        &caller.end_user_subject,
        &roles,
    )
    .await?;
    if row.get::<Option<String>, _>("reason_code").as_deref() == Some("grant-tools-to-workflow") {
        return Err(AdminError::conflict(
            "TYPED_APPROVAL_REQUIRED",
            "workflow Tool access approval requires its typed decision tool",
        ));
    }
    let status: String = row.get("assignment_status_code");
    if status == "COMPLETED" {
        if r.idempotency_key.as_deref()
            == row
                .get::<Option<String>, _>("completion_idempotency_key")
                .as_deref()
        {
            return Ok(
                json!({"taskAsstId":id,"assignmentVersion":row.get::<i64,_>("aggregate_version"),"assignmentStatus":"COMPLETED","completionId":row.get::<Uuid,_>("completion_id"),"completionRecorded":true}),
            );
        }
        return Err(AdminError::conflict(
            "ALREADY_COMPLETED",
            "task assignment is already completed",
        ));
    }
    if deadline_expired(
        row.get::<Option<DateTime<Utc>>, _>("deadline_ts"),
        Utc::now(),
    ) {
        return Err(AdminError::conflict(
            "TASK_EXPIRED",
            "human task deadline has expired",
        ));
    }
    if status != "CLAIMED"
        || row.get::<Option<String>, _>("claimed_by").as_deref() != Some(&caller.end_user_subject)
        || row
            .get::<Option<DateTime<Utc>>, _>("claim_expires_ts")
            .is_none_or(|v| v <= Utc::now())
    {
        return Err(AdminError::conflict(
            "CLAIM_NOT_OWNED",
            "active claim is not owned by this caller",
        ));
    }
    if row.get::<i64, _>("aggregate_version") != r.assignment_version {
        return Err(AdminError::conflict(
            "VERSION_CONFLICT",
            "assignment version is stale",
        ));
    }
    let output: Value = row.get("task_output");
    validate_decision(
        output.get("ask").unwrap_or(&Value::Null),
        &r.decision,
        r.comment.as_deref(),
    )?;
    let completion_id = Uuid::now_v7();
    let task_id: Uuid = row.get("task_id");
    let updated=sqlx::query("UPDATE task_info_t SET status_code='C',locked='N',completed_ts=CURRENT_TIMESTAMP,completed_user=$1,result_code=$2,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$3 AND task_id=$4 AND status_code='W'").bind(&caller.end_user_subject).bind(r.decision.to_string()).bind(caller.host_id).bind(task_id).execute(&mut *tx).await.map_err(AdminError::database)?;
    if updated.rows_affected() != 1 {
        return Err(AdminError::conflict(
            "INVALID_STATE",
            "human task is no longer waiting",
        ));
    }
    let version=sqlx::query_scalar::<_,i64>("UPDATE task_asst_t SET assignment_status_code='COMPLETED',decision=$1,decision_comment=$2,completion_id=$3,completion_idempotency_key=$4,completed_ts=CURRENT_TIMESTAMP,active=FALSE,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$5 WHERE host_id=$6 AND task_asst_id=$7 RETURNING aggregate_version").bind(&r.decision).bind(&r.comment).bind(completion_id).bind(&r.idempotency_key).bind(&caller.end_user_subject).bind(caller.host_id).bind(id).fetch_one(&mut *tx).await.map_err(AdminError::database)?;
    sqlx::query("UPDATE task_asst_t SET assignment_status_code='CANCELLED',claimed_by=NULL,claimed_ts=NULL,claim_expires_ts=NULL,active=FALSE,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$2 AND task_id=$3 AND task_asst_id<>$4 AND active")
        .bind(&caller.end_user_subject)
        .bind(caller.host_id)
        .bind(task_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(AdminError::database)?;
    tx.commit().await.map_err(AdminError::database)?;
    Ok(
        json!({"taskAsstId":id,"assignmentVersion":version,"assignmentStatus":"COMPLETED","completionId":completion_id,"completionRecorded":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_name_uses_saved_open_workflow_document() {
        assert_eq!(
            workflow_name(&json!({"document":{"name":"simple-set-assert"}})),
            "simple-set-assert"
        );
        assert_eq!(workflow_name(&json!({"name":"legacy-name"})), "legacy-name");
    }

    #[test]
    fn pagination_is_bounded_and_rejects_bad_cursors() {
        assert_eq!(page(None).expect("default page"), (0, DEFAULT_PAGE_SIZE));
        assert_eq!(
            page(Some(PageInput {
                cursor: Some("17".into()),
                page_size: Some(MAX_PAGE_SIZE),
            }))
            .expect("bounded page"),
            (17, MAX_PAGE_SIZE)
        );
        assert!(
            page(Some(PageInput {
                cursor: Some("not-an-offset".into()),
                page_size: Some(25),
            }))
            .is_err()
        );
        assert!(
            page(Some(PageInput {
                cursor: None,
                page_size: Some(MAX_PAGE_SIZE + 1),
            }))
            .is_err()
        );
    }

    #[test]
    fn ask_decision_validation_enforces_required_comment_options_and_schema() {
        assert!(validate_decision(&json!({"required": true}), &Value::Null, None).is_err());
        assert!(
            validate_decision(
                &json!({"commentRequired": true}),
                &json!("approve"),
                Some("  ")
            )
            .is_err()
        );

        let option_ask = json!({"options": [{"value": "approve"}, {"value": "deny"}]});
        assert!(validate_decision(&option_ask, &json!("approve"), None).is_ok());
        assert!(validate_decision(&option_ask, &json!(["approve", "deny"]), None).is_ok());
        assert!(validate_decision(&option_ask, &json!("other"), None).is_err());

        let schema_ask = json!({
            "schema": {
                "type": "object",
                "required": ["digest"],
                "properties": {"digest": {"type": "string", "minLength": 1}},
                "additionalProperties": false
            }
        });
        assert!(validate_decision(&schema_ask, &json!({"digest": "sha256:abc"}), None).is_ok());
        assert!(validate_decision(&schema_ask, &json!({"digest": ""}), None).is_err());
    }

    #[test]
    fn completion_deadline_is_inclusive_and_optional() {
        let now = Utc::now();
        assert!(!deadline_expired(None, now));
        assert!(deadline_expired(Some(now), now));
        assert!(deadline_expired(
            Some(now - chrono::Duration::seconds(1)),
            now
        ));
        assert!(!deadline_expired(
            Some(now + chrono::Duration::seconds(1)),
            now
        ));
    }
}

#[cfg(test)]
mod native_process_postgres_tests {
    use super::*;
    use crate::artifact_retention::{
        ArtifactObjectStore, ArtifactRetentionReconciler, ArtifactStoreError,
    };
    use sqlx::postgres::PgPoolOptions;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FlakyObjectStore {
        deletes: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ArtifactObjectStore for FlakyObjectStore {
        async fn delete(&self, _: &str) -> Result<(), ArtifactStoreError> {
            if self.deletes.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(ArtifactStoreError {
                    message: "fixture transient failure".into(),
                    retryable: true,
                })
            } else {
                Ok(())
            }
        }
        async fn exists(&self, _: &str) -> Result<bool, ArtifactStoreError> {
            Ok(false)
        }
    }

    fn caller(host: Uuid, user: &str) -> InvocationIdentity {
        InvocationIdentity {
            host_id: host,
            principal_subject: user.into(),
            end_user_subject: user.into(),
            caller_claims_digest: "fixture".into(),
            caller_claims: json!({}),
            user_authorization: "Bearer isolated-fixture".into(),
            user_authorization_exp: i64::MAX,
        }
    }

    #[tokio::test]
    #[ignore = "requires a disposable migrated Workflow PostgreSQL database"]
    async fn native_note_delete_owner_retry_hold_and_definition_preservation() {
        let url = std::env::var("WORKFLOW_NATIVE_OPS_TEST_DATABASE_URL")
            .expect("disposable migrated Workflow database URL required");
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let host = Uuid::now_v7();
        let process = Uuid::now_v7();
        let instance = Uuid::now_v7();
        let definition = Uuid::now_v7();
        let binding = Uuid::now_v7();
        let tool = Uuid::now_v7();
        let digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query(
            "INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,
            definition,lifecycle_status) VALUES($1,$2,'test','preserved','1.0.0',
            'document: {dsl: 1.0.3}','PUBLISHED')",
        )
        .bind(host)
        .bind(definition)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,
            workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,
            total_deadline_ms,execution_class,result_text_mode,idempotency_policy,
            delegation_policy,response_policy_digest,runtime_bounds,policy_digest)
            VALUES($1,$2,$3,$4,'1.0.0',$5,$5,'async',1000,10000,'standard',
            'compact-json','{}','{}',$5,'{}',$5)",
        )
        .bind(host)
        .bind(binding)
        .bind(tool)
        .bind(definition)
        .bind(&digest)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO process_info_t(host_id,process_id,wf_def_id,wf_instance_id,
            app_id,process_type,status_code,ex_trigger_ts,definition_snapshot)
            VALUES($1,$2,$3,$4,'fixture','WORKFLOW','A',now(),'{}')",
        )
        .bind(host)
        .bind(process)
        .bind(definition)
        .bind(instance.to_string())
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO workflow_invocation_t(host_id,workflow_instance_id,binding_id,
            process_id,stable_tool_ref,wf_def_id,workflow_version,definition_digest,
            schema_digest,policy_digest,response_policy_digest,principal_subject,
            end_user_subject,input,input_digest,canonical_input_profile,invocation_mode,
            execution_class,state,correlation_id,deadline_ts)
            VALUES($1,$2,$3,$4,$5,$6,'1.0.0',$7,$7,$7,$7,'owner','owner','{}',$7,
            'rfc8785-safe-json-v1','async','standard','ACCEPTED','fixture',now()+interval '1 hour')")
            .bind(host).bind(instance).bind(binding).bind(process).bind(tool).bind(definition)
            .bind(&digest).execute(&pool).await.unwrap();
        let owner = caller(host, "owner");
        let task_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO task_info_t(host_id,task_id,task_type,process_id,
            wf_instance_id,wf_task_id,status_code,locked,priority,task_input,task_output)
            VALUES($1,$2,'http',$3,$4,'call-api','C','N',1,
            '{\"secret\":\"do-not-disclose\"}','{\"secret\":\"do-not-disclose\"}')",
        )
        .bind(host)
        .bind(task_id)
        .bind(process)
        .bind(instance.to_string())
        .execute(&pool)
        .await
        .unwrap();
        let detail = get_native_task_for_caller(&pool, &owner, NativeTaskGet { task_id })
            .await
            .unwrap();
        assert_eq!(detail["type"], "http");
        assert!(!detail.to_string().contains("do-not-disclose"));
        assert!(
            get_native_task_for_caller(&pool, &caller(host, "other"), NativeTaskGet { task_id })
                .await
                .is_err()
        );
        assert!(
            add_process_note_for_caller(
                &pool,
                &owner,
                ProcessNoteInput {
                    process_id: process,
                    task_id: Some(Uuid::now_v7()),
                    text: "wrong task".into(),
                    idempotency_key: "note-wrong-task".into()
                }
            )
            .await
            .is_err()
        );
        let note = ProcessNoteInput {
            process_id: process,
            task_id: None,
            text: "reviewed".into(),
            idempotency_key: "note-fixture-1".into(),
        };
        let first = add_process_note_for_caller(&pool, &owner, note)
            .await
            .unwrap();
        let notes = list_process_notes_for_caller(
            &pool,
            &owner,
            ProcessNotesQuery {
                process_id: process,
                page: None,
            },
            0,
            25,
        )
        .await
        .unwrap();
        assert_eq!(notes["notes"].as_array().unwrap().len(), 1);
        assert_eq!(notes["notes"][0]["noteId"], first["noteId"]);
        assert!(
            list_process_notes_for_caller(
                &pool,
                &caller(host, "other"),
                ProcessNotesQuery {
                    process_id: process,
                    page: None
                },
                0,
                25
            )
            .await
            .is_err()
        );
        let retry = add_process_note_for_caller(
            &pool,
            &owner,
            ProcessNoteInput {
                process_id: process,
                task_id: None,
                text: "reviewed".into(),
                idempotency_key: "note-fixture-1".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(first["noteId"], retry["noteId"]);
        let linked = add_process_note_for_caller(
            &pool,
            &owner,
            ProcessNoteInput {
                process_id: process,
                task_id: Some(task_id),
                text: "task reviewed".into(),
                idempotency_key: "note-fixture-task".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(linked["taskId"], task_id.to_string());
        let page = list_process_notes_for_caller(
            &pool,
            &owner,
            ProcessNotesQuery {
                process_id: process,
                page: None,
            },
            0,
            1,
        )
        .await
        .unwrap();
        assert_eq!(page["notes"].as_array().unwrap().len(), 1);
        assert_eq!(page["page"]["hasMore"], true);
        assert!(
            add_process_note_for_caller(
                &pool,
                &caller(Uuid::now_v7(), "owner"),
                ProcessNoteInput {
                    process_id: process,
                    task_id: None,
                    text: "wrong host".into(),
                    idempotency_key: "note-wrong-host".into()
                }
            )
            .await
            .is_err()
        );
        assert!(
            add_process_note_for_caller(
                &pool,
                &owner,
                ProcessNoteInput {
                    process_id: process,
                    task_id: None,
                    text: "changed".into(),
                    idempotency_key: "note-fixture-1".into()
                }
            )
            .await
            .is_err()
        );
        assert!(
            add_process_note_for_caller(
                &pool,
                &caller(host, "other"),
                ProcessNoteInput {
                    process_id: process,
                    task_id: None,
                    text: "denied".into(),
                    idempotency_key: "note-fixture-2".into()
                }
            )
            .await
            .is_err()
        );
        let deletion = || NativeDeleteInput {
            process_id: process,
            expected_lifecycle_version: 1,
            reason: "fixture cleanup".into(),
            idempotency_key: "delete-fixture-1".into(),
        };
        assert!(
            delete_native_process_for_caller(&pool, &owner, deletion())
                .await
                .is_err()
        );
        sqlx::query(
            "UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=now()
            WHERE host_id=$1 AND workflow_instance_id=$2",
        )
        .bind(host)
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE process_info_t SET status_code='C' WHERE host_id=$1 AND process_id=$2")
            .bind(host)
            .bind(process)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE task_info_t SET status_code='W' WHERE host_id=$1 AND task_id=$2")
            .bind(host)
            .bind(task_id)
            .execute(&pool)
            .await
            .unwrap();
        let held_error = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap_err();
        assert_eq!(held_error.code, "RESOURCE_HELD");
        sqlx::query("UPDATE task_info_t SET status_code='C' WHERE host_id=$1 AND task_id=$2")
            .bind(host)
            .bind(task_id)
            .execute(&pool)
            .await
            .unwrap();
        let stale = delete_native_process_for_caller(
            &pool,
            &owner,
            NativeDeleteInput {
                process_id: process,
                expected_lifecycle_version: 2,
                reason: "fixture cleanup".into(),
                idempotency_key: "delete-fixture-1".into(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(stale.code, "VERSION_CONFLICT");
        sqlx::query(
            "INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,
            user_id,grant_generation,run_generation,budget_generation,active,deadline,
            action_limit,reserved) VALUES($1,$2,$3,$4,1,1,1,true,now()+interval '1 hour',2,1)",
        )
        .bind(host)
        .bind(instance)
        .bind(Uuid::now_v7())
        .bind(Uuid::now_v7())
        .execute(&pool)
        .await
        .unwrap();
        let action_held = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap_err();
        assert_eq!(action_held.code, "RESOURCE_HELD");
        sqlx::query(
            "UPDATE workflow_action_authority_t SET reserved=0,active=false
            WHERE host_id=$1 AND run_id=$2",
        )
        .bind(host)
        .bind(instance)
        .execute(&pool)
        .await
        .unwrap();
        for held in [false, true] {
            sqlx::query(
                "INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,
                process_id,logical_name,media_type,size_bytes,content_digest,storage_reference,
                producer,policy_digest,retain_until_ts,legal_hold,verification_state)
                VALUES($1,$2,$3,$4,'fixture','text/plain',1,'digest','fixture://object',
                'fixture','digest',now()-interval '1 minute',$5,'VERIFIED')",
            )
            .bind(host)
            .bind(Uuid::now_v7())
            .bind(instance)
            .bind(process)
            .bind(held)
            .execute(&pool)
            .await
            .unwrap();
        }
        assert!(
            delete_native_process_for_caller(&pool, &caller(host, "other"), deletion())
                .await
                .is_err()
        );
        let receipt = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap();
        assert_eq!(receipt["artifactCleanup"], "PENDING");
        assert_eq!(receipt["artifacts"]["legalHold"], 1);
        assert_eq!(receipt["artifacts"]["pending"], 1);
        let replay = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap();
        assert_eq!(receipt["operationId"], replay["operationId"]);
        let retention = ArtifactRetentionReconciler::new(
            pool.clone(),
            FlakyObjectStore {
                deletes: AtomicUsize::new(0),
            },
            10,
        );
        assert_eq!(retention.reconcile_once().await.unwrap(), 1);
        let failed = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap();
        assert_eq!(failed["artifacts"]["failed"], 1);
        sqlx::query(
            "UPDATE workflow_artifact_t SET deletion_next_retry_ts=now()
            WHERE host_id=$1 AND process_id=$2 AND deletion_state='DELETE_FAILED'",
        )
        .bind(host)
        .bind(process)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(retention.reconcile_once().await.unwrap(), 1);
        let finished = delete_native_process_for_caller(&pool, &owner, deletion())
            .await
            .unwrap();
        assert_eq!(finished["artifactCleanup"], "RETAINED");
        assert_eq!(finished["artifacts"]["deleted"], 1);
        assert_eq!(finished["artifacts"]["legalHold"], 1);
        let evidence: serde_json::Value = sqlx::query_scalar(
            "SELECT deletion_evidence FROM workflow_artifact_t
            WHERE host_id=$1 AND process_id=$2 AND deletion_state='DELETED'",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(evidence["verifiedAbsent"], true);
        let active: bool = sqlx::query_scalar(
            "SELECT active FROM process_info_t
            WHERE host_id=$1 AND process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!active);
        let published: String = sqlx::query_scalar(
            "SELECT lifecycle_status FROM wf_definition_t
            WHERE host_id=$1 AND wf_def_id=$2",
        )
        .bind(host)
        .bind(definition)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(published, "PUBLISHED");
        pool.close().await;
    }
}
async fn lock_assignment(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    id: Uuid,
    subject: &str,
    roles: &[String],
) -> Result<sqlx::postgres::PgRow, AdminError> {
    sqlx::query("SELECT a.*,t.task_output,t.deadline_ts,t.reason_code FROM task_asst_t a JOIN task_info_t t ON t.host_id=a.host_id AND t.task_id=a.task_id WHERE a.host_id=$1 AND a.task_asst_id=$2 AND ((a.assignment_type='USER' AND a.assignment_id=$3) OR (a.assignment_type='ROLE' AND a.assignment_id=ANY($4::text[]))) FOR UPDATE OF a,t").bind(host_id).bind(id).bind(subject).bind(roles).fetch_optional(&mut **tx).await.map_err(AdminError::database)?.ok_or_else(AdminError::not_found)
}

#[cfg(test)]
mod role_postgres_tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;
    use std::{
        collections::HashMap,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    #[derive(Default)]
    struct TestAuthority {
        roles: Mutex<HashMap<Uuid, Vec<String>>>,
        unavailable: AtomicBool,
    }

    #[async_trait::async_trait]
    impl RoleAuthority for TestAuthority {
        async fn current_roles(
            &self,
            _: &str,
            _: Uuid,
            user: Uuid,
        ) -> Result<Vec<String>, RoleAuthorityError> {
            if self.unavailable.load(Ordering::SeqCst) {
                return Err(RoleAuthorityError::Unavailable);
            }
            Ok(self
                .roles
                .lock()
                .unwrap()
                .get(&user)
                .cloned()
                .unwrap_or_default())
        }
    }

    fn caller(host: Uuid, user: Uuid, reviewer: bool) -> InvocationIdentity {
        InvocationIdentity {
            host_id: host,
            principal_subject: user.to_string(),
            end_user_subject: user.to_string(),
            caller_claims_digest: "fixture".into(),
            caller_claims: if reviewer {
                json!({"role":"reviewer"})
            } else {
                json!({})
            },
            user_authorization: "Bearer same-unexpired-fixture-token".into(),
            user_authorization_exp: i64::MAX,
        }
    }

    #[tokio::test]
    async fn role_assignment_uses_verified_acting_user_claims() {
        let host = Uuid::new_v4();
        let user = Uuid::new_v4();
        let mut identity = caller(host, user, true);
        assert_eq!(
            current_roles(None, &identity).await.unwrap(),
            vec!["reviewer"]
        );
        identity.caller_claims = json!({"role":"reviewer,approver"});
        assert_eq!(
            current_roles(None, &identity).await.unwrap(),
            vec!["reviewer", "approver"]
        );
        identity.caller_claims = json!({"role":"reviewer reviewer"});
        assert_eq!(
            current_roles(None, &identity).await.unwrap_err().code,
            "ROLE_NOT_CURRENT"
        );
        identity.caller_claims = json!({"role":"reviewer"});
        identity.user_authorization_exp = 0;
        assert_eq!(
            current_roles(None, &identity).await.unwrap_err().code,
            "ROLE_NOT_CURRENT"
        );
    }

    async fn insert_ask(
        pool: &PgPool,
        host: Uuid,
        task: Uuid,
        asst: Uuid,
        kind: &str,
        assignment: &str,
    ) {
        sqlx::query("INSERT INTO task_info_t(host_id,task_id,process_id,wf_instance_id,status_code,task_output,task_type,locked,aggregate_version)
            VALUES($1,$2,$3,$4,'W',$5,'ask','Y',1)")
            .bind(host).bind(task).bind(Uuid::new_v4()).bind(Uuid::new_v4().to_string())
            .bind(json!({"ask":{"required":true,"options":[{"value":"approve"},{"value":"deny"}]}}))
            .execute(pool).await.unwrap();
        sqlx::query("INSERT INTO task_asst_t(host_id,task_asst_id,task_id,assignment_type,assignment_id,active,assignment_status_code,aggregate_version,assigned_ts,assignee_id)
            VALUES($1,$2,$3,$4,$5,true,'ASSIGNED',1,CURRENT_TIMESTAMP,$5)")
            .bind(host).bind(asst).bind(task).bind(kind).bind(assignment).execute(pool).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires WORKFLOW_ROLE_TEST_DATABASE_URL pointing to a disposable PostgreSQL database"]
    async fn role_claim_revocation_completion_and_user_regression() {
        let url = std::env::var("WORKFLOW_ROLE_TEST_DATABASE_URL")
            .expect("qualification database required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("workflow_role_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let path = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(move |conn, _| {
                let path = path.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {path},pg_catalog"))
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE task_info_t(
            host_id uuid,task_id uuid,process_id uuid,wf_instance_id text,status_code text,
            deadline_ts timestamptz,task_output jsonb,task_type text,locked text,
            completed_ts timestamptz,completed_user text,result_code text,
            aggregate_version bigint,update_ts timestamptz,update_user text);
            CREATE TABLE task_asst_t(
            host_id uuid,task_asst_id uuid,task_id uuid,assignment_type text,assignment_id text,
            active boolean,assignment_status_code text,claimed_by text,claimed_ts timestamptz,
            claim_expires_ts timestamptz,aggregate_version bigint,update_ts timestamptz,
            update_user text,assigned_ts timestamptz,assignee_id text,category_code text,
            reason_code text,decision jsonb,decision_comment text,completion_id uuid,
            completion_idempotency_key text,completed_ts timestamptz)",
        )
        .execute(&pool)
        .await
        .unwrap();

        let host = Uuid::new_v4();
        let other_host = Uuid::new_v4();
        let alice = Uuid::new_v4();
        let bob = Uuid::new_v4();
        let outsider = Uuid::new_v4();
        let user_task = Uuid::new_v4();
        let user_asst = Uuid::new_v4();
        let role_task = Uuid::new_v4();
        let role_asst = Uuid::new_v4();
        let sibling_asst = Uuid::new_v4();
        let other_asst = Uuid::new_v4();
        insert_ask(&pool, host, role_task, role_asst, "ROLE", "reviewer").await;
        sqlx::query("INSERT INTO task_asst_t(host_id,task_asst_id,task_id,assignment_type,assignment_id,active,assignment_status_code,aggregate_version,assigned_ts,assignee_id)
            VALUES($1,$2,$3,'ROLE','reviewer',true,'ASSIGNED',1,CURRENT_TIMESTAMP,'reviewer')")
            .bind(host).bind(sibling_asst).bind(role_task).execute(&pool).await.unwrap();
        insert_ask(
            &pool,
            host,
            user_task,
            user_asst,
            "USER",
            &alice.to_string(),
        )
        .await;
        insert_ask(
            &pool,
            other_host,
            Uuid::new_v4(),
            other_asst,
            "ROLE",
            "reviewer",
        )
        .await;

        let test = Arc::new(TestAuthority::default());
        test.roles
            .lock()
            .unwrap()
            .insert(alice, vec!["reviewer".into()]);
        test.roles
            .lock()
            .unwrap()
            .insert(bob, vec!["reviewer".into()]);
        let authority: Arc<dyn RoleAuthority> = test.clone();
        let a = caller(host, alice, true);
        let b = caller(host, bob, true);
        let nonmember = caller(host, outsider, false);
        let roles = inbox_roles(&pool, Some(&authority), &a).await.unwrap();
        assert_eq!(roles, vec!["reviewer"]);
        let summary = inbox_summary_for_caller(&pool, Some(&authority), &a)
            .await
            .unwrap();
        assert_eq!(summary["tabs"][0]["count"], 3);
        assert_eq!(summary["tabs"][1]["count"], 1);
        assert_eq!(summary["tabs"][2]["count"], 2);
        let all = list_for_caller(
            &pool,
            Some(&authority),
            &a,
            &HumanTaskListRequest::default(),
        )
        .await
        .unwrap();
        assert_eq!(all["humanTasks"].as_array().unwrap().len(), 3);
        let role_list = list_for_caller(
            &pool,
            Some(&authority),
            &a,
            &HumanTaskListRequest {
                tab_id: Some("role:reviewer".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(role_list["humanTasks"].as_array().unwrap().len(), 2);
        let detail = get_for_caller(&pool, Some(&authority), &a, role_asst)
            .await
            .unwrap();
        assert_eq!(detail["task"]["assignmentType"], "ROLE");
        assert!(
            list_for_caller(
                &pool,
                Some(&authority),
                &nonmember,
                &HumanTaskListRequest::default()
            )
            .await
            .unwrap()["humanTasks"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            get_for_caller(&pool, Some(&authority), &nonmember, role_asst)
                .await
                .unwrap_err()
                .code,
            "ROLE_NOT_CURRENT"
        );
        assert_eq!(
            claim_for_caller(
                &pool,
                Some(&authority),
                &nonmember,
                role_asst,
                &ClaimRequest {
                    assignment_version: 1,
                    claim_minutes: Some(30)
                }
            )
            .await
            .unwrap_err()
            .code,
            "ROLE_NOT_CURRENT"
        );
        assert!(
            load_task(&pool, host, role_asst, &a.end_user_subject, &roles)
                .await
                .is_ok()
        );
        assert!(
            load_task(&pool, host, other_asst, &a.end_user_subject, &roles)
                .await
                .is_err()
        );
        assert_eq!(
            assignment_roles(&pool, Some(&authority), &a, other_asst)
                .await
                .unwrap_err()
                .code,
            "NOT_FOUND"
        );

        let claim = ClaimRequest {
            assignment_version: 1,
            claim_minutes: Some(30),
        };
        let (a_claim, b_claim) = tokio::join!(
            claim_for_caller(&pool, Some(&authority), &a, role_asst, &claim),
            claim_for_caller(&pool, Some(&authority), &b, role_asst, &claim)
        );
        assert_eq!(a_claim.is_ok() as u8 + b_claim.is_ok() as u8, 1);
        let (winner, loser) = if a_claim.is_ok() { (&a, &b) } else { (&b, &a) };
        let failed = a_claim.err().or_else(|| b_claim.err()).unwrap();
        assert_eq!(failed.code, "CLAIM_CONFLICT");
        assert_eq!(
            release_for_caller(
                &pool,
                Some(&authority),
                loser,
                role_asst,
                &VersionRequest {
                    assignment_version: 2
                }
            )
            .await
            .unwrap_err()
            .code,
            "CLAIM_NOT_OWNED"
        );
        release_for_caller(
            &pool,
            Some(&authority),
            winner,
            role_asst,
            &VersionRequest {
                assignment_version: 2,
            },
        )
        .await
        .unwrap();
        let reclaimed = claim_for_caller(
            &pool,
            Some(&authority),
            loser,
            role_asst,
            &ClaimRequest {
                assignment_version: 3,
                claim_minutes: Some(30),
            },
        )
        .await
        .unwrap();
        assert_eq!(reclaimed["assignmentVersion"], 4);

        // A new token without the role must not complete the ROLE assignment.
        let revoked = caller(
            host,
            Uuid::parse_str(&loser.end_user_subject).unwrap(),
            false,
        );
        let completion = CompleteRequest {
            assignment_version: 4,
            decision: json!("approve"),
            comment: None,
            idempotency_key: Some("role-complete-001".into()),
        };
        assert_eq!(
            complete_for_caller(&pool, Some(&authority), &revoked, role_asst, &completion)
                .await
                .unwrap_err()
                .code,
            "ROLE_NOT_CURRENT"
        );
        let status: String = sqlx::query_scalar(
            "SELECT status_code FROM task_info_t WHERE host_id=$1 AND task_id=$2",
        )
        .bind(host)
        .bind(role_task)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "W");
        let mut expired = caller(host, alice, true);
        expired.user_authorization_exp = 0;
        assert_eq!(
            inbox_roles(&pool, Some(&authority), &expired)
                .await
                .unwrap_err()
                .code,
            "ROLE_NOT_CURRENT"
        );
        let renewed = caller(
            host,
            Uuid::parse_str(&loser.end_user_subject).unwrap(),
            true,
        );
        let receipt =
            complete_for_caller(&pool, Some(&authority), &renewed, role_asst, &completion)
                .await
                .unwrap();
        let replay = complete_for_caller(&pool, Some(&authority), &renewed, role_asst, &completion)
            .await
            .unwrap();
        assert_eq!(receipt["completionId"], replay["completionId"]);
        let sibling: String = sqlx::query_scalar(
            "SELECT assignment_status_code FROM task_asst_t WHERE task_asst_id=$1",
        )
        .bind(sibling_asst)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(sibling, "CANCELLED");

        let user_claim = claim_for_caller(&pool, None, &a, user_asst, &claim)
            .await
            .unwrap();
        assert_eq!(user_claim["assignmentStatus"], "CLAIMED");
        let user_receipt = complete_for_caller(
            &pool,
            None,
            &a,
            user_asst,
            &CompleteRequest {
                assignment_version: 2,
                decision: json!("approve"),
                comment: None,
                idempotency_key: Some("user-complete-001".into()),
            },
        )
        .await
        .unwrap();
        assert_eq!(user_receipt["completionRecorded"], true);

        pool.close().await;
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
