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
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

const DEFAULT_PAGE_SIZE: i64 = 25;
const MAX_PAGE_SIZE: i64 = 100;

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

#[derive(Deserialize, Default)]
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

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProcessListRequest {
    page: Option<PageInput>,
    definition_id: Option<Uuid>,
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
        "workflowName":snapshot.get("name").and_then(Value::as_str).unwrap_or("workflow"),
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
        "{PROCESS_SELECT} WHERE p.host_id=$1 AND i.principal_subject=$2 AND i.end_user_subject=$3 AND ($4::uuid IS NULL OR p.wf_def_id=$4) AND ($5::text[] IS NULL OR i.state=ANY($5)) AND ($6::timestamptz IS NULL OR p.started_ts >= $6) AND ($7::timestamptz IS NULL OR p.started_ts <= $7) ORDER BY CASE WHEN $8 THEN p.started_ts END DESC,CASE WHEN NOT $8 THEN p.started_ts END ASC,p.process_id OFFSET $9 LIMIT $10"
    );
    let rows = sqlx::query(&query)
        .bind(caller.host_id)
        .bind(&caller.principal_subject)
        .bind(&caller.end_user_subject)
        .bind(request.definition_id)
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
        "{PROCESS_SELECT} WHERE p.host_id=$1 AND p.process_id=$2 AND i.principal_subject=$3 AND i.end_user_subject=$4"
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
    Ok(
        json!({"taskAsstId":row.try_get::<Uuid,_>("task_asst_id").map_err(AdminError::database)?,"taskId":row.try_get::<Uuid,_>("task_id").map_err(AdminError::database)?,"processId":row.try_get::<Uuid,_>("process_id").map_err(AdminError::database)?,"workflowInstanceId":Uuid::parse_str(&row.try_get::<String,_>("wf_instance_id").map_err(AdminError::database)?).ok(),"assignmentVersion":row.try_get::<i64,_>("aggregate_version").map_err(AdminError::database)?,"assignmentType":row.try_get::<String,_>("assignment_type").map_err(AdminError::database)?,"assignmentId":row.try_get::<String,_>("assignment_id").map_err(AdminError::database)?,"assignmentLabel":row.try_get::<String,_>("assignee_id").map_err(AdminError::database)?,"assignmentStatus":status,"taskStatus":process_state(&task_status),"assignedAt":row.try_get::<DateTime<Utc>,_>("assigned_ts").map_err(AdminError::database)?,"claimedBy":claimed_by,"claimExpiresAt":expires,"deadline":row.try_get::<Option<DateTime<Utc>>,_>("deadline_ts").map_err(AdminError::database)?,"category":row.try_get::<Option<String>,_>("category_code").map_err(AdminError::database)?,"reason":row.try_get::<Option<String>,_>("reason_code").map_err(AdminError::database)?,"prompt":output.pointer("/ask/prompt").and_then(Value::as_str),"canClaim":action_hint(can_claim,(!can_claim).then_some(if waiting{"TASK_NOT_AVAILABLE"}else{"TASK_NOT_WAITING"})),"canRelease":action_hint(can_mutate_claim,(!can_mutate_claim).then_some(if waiting{"CLAIM_NOT_OWNED"}else{"TASK_NOT_WAITING"})),"canComplete":action_hint(can_mutate_claim,(!can_mutate_claim).then_some(if waiting{"CLAIM_REQUIRED"}else{"TASK_NOT_WAITING"})),"readOnly":!can_claim&&!can_mutate_claim}),
    )
}

async fn inbox_summary(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    let count:i64=sqlx::query_scalar("SELECT count(*) FROM task_asst_t a JOIN task_info_t t ON t.host_id=a.host_id AND t.task_id=a.task_id WHERE a.host_id=$1 AND a.assignment_type='USER' AND a.assignment_id=$2 AND a.active AND t.status_code='W'").bind(caller.host_id).bind(&caller.end_user_subject).fetch_one(&state.pool).await.map_err(|e|AdminError::database(e).into_response())?;
    Ok(Json(
        json!({"tabs":[{"id":"all","label":"All","assignmentType":Value::Null,"assignmentId":Value::Null,"count":count},{"id":"user:self","label":"Assigned to me","assignmentType":"USER","assignmentId":caller.end_user_subject,"count":count}]}),
    ))
}

async fn list_human_tasks(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<HumanTaskListRequest>,
) -> Result<Json<Value>, Response> {
    let caller = identity(&state, &headers).await?;
    if !matches!(
        request.tab_id.as_deref().unwrap_or("all"),
        "all" | "user:self"
    ) {
        return Err(AdminError::bad("tabId is not available").into_response());
    }
    let (offset, size) = page(request.page).map_err(IntoResponse::into_response)?;
    let query = format!(
        "{TASK_SELECT} WHERE a.host_id=$1 AND a.assignment_type='USER' AND a.assignment_id=$2 AND a.active AND t.status_code='W' AND ($3 OR a.assignment_status_code<>'CLAIMED') AND ($4 OR a.claimed_by IS NULL OR a.claimed_by=$2 OR a.claim_expires_ts<=CURRENT_TIMESTAMP) ORDER BY a.assigned_ts DESC,a.task_asst_id OFFSET $5 LIMIT $6"
    );
    let rows = sqlx::query(&query)
        .bind(caller.host_id)
        .bind(&caller.end_user_subject)
        .bind(request.include_claimed.unwrap_or(true))
        .bind(request.include_claimed_by_others.unwrap_or(false))
        .bind(offset)
        .bind(size + 1)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| AdminError::database(e).into_response())?;
    let has_more = rows.len() as i64 > size;
    let tasks = rows
        .iter()
        .take(size as usize)
        .map(|r| task_json(r, &caller.end_user_subject))
        .collect::<Result<Vec<_>, _>>()
        .map_err(IntoResponse::into_response)?;
    Ok(Json(
        json!({"humanTasks":tasks,"page":{"pageSize":tasks.len(),"nextCursor":has_more.then(||(offset+size).to_string()),"hasMore":has_more}}),
    ))
}

async fn load_task(
    state: &RuleApiState,
    host_id: Uuid,
    task_asst_id: Uuid,
    subject: &str,
) -> Result<sqlx::postgres::PgRow, AdminError> {
    let query = format!(
        "{TASK_SELECT} WHERE a.host_id=$1 AND a.task_asst_id=$2 AND a.assignment_type='USER' AND a.assignment_id=$3"
    );
    sqlx::query(&query)
        .bind(host_id)
        .bind(task_asst_id)
        .bind(subject)
        .fetch_optional(&state.pool)
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
    let row = load_task(&state, caller.host_id, id, &caller.end_user_subject)
        .await
        .map_err(IntoResponse::into_response)?;
    let task = task_json(&row, &caller.end_user_subject).map_err(IntoResponse::into_response)?;
    let output: Value = row
        .try_get("task_output")
        .map_err(|e| AdminError::database(e).into_response())?;
    Ok(Json(
        json!({"task":task,"ask":output.get("ask").cloned().unwrap_or_else(||json!({"mode":"text","prompt":"Input required","required":true})),"contextSummary":{}}),
    ))
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
    let minutes = r.claim_minutes.unwrap_or(30);
    if !(1..=120).contains(&minutes) {
        return Err(AdminError::bad("claimMinutes must be between 1 and 120").into_response());
    }
    let row=sqlx::query("UPDATE task_asst_t a SET assignment_status_code='CLAIMED',claimed_by=$1,claimed_ts=CURRENT_TIMESTAMP,claim_expires_ts=CURRENT_TIMESTAMP+make_interval(mins=>$2::int),aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$3 AND task_asst_id=$4 AND assignment_type='USER' AND assignment_id=$1 AND active AND aggregate_version=$5 AND (assignment_status_code='ASSIGNED' OR (assignment_status_code='CLAIMED' AND claim_expires_ts<=CURRENT_TIMESTAMP)) AND EXISTS(SELECT 1 FROM task_info_t t WHERE t.host_id=a.host_id AND t.task_id=a.task_id AND t.status_code='W') RETURNING aggregate_version,claim_expires_ts").bind(&caller.end_user_subject).bind(minutes as i32).bind(caller.host_id).bind(id).bind(r.assignment_version).fetch_optional(&state.pool).await.map_err(|e|AdminError::database(e).into_response())?.ok_or_else(||AdminError::conflict("CLAIM_CONFLICT","task is no longer available to claim").into_response())?;
    Ok(Json(
        json!({"taskAsstId":id,"assignmentVersion":row.get::<i64,_>("aggregate_version"),"assignmentStatus":"CLAIMED","claimExpiresAt":row.get::<DateTime<Utc>,_>("claim_expires_ts")}),
    ))
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
    let version=sqlx::query_scalar::<_,i64>("UPDATE task_asst_t SET assignment_status_code='ASSIGNED',claimed_by=NULL,claimed_ts=NULL,claim_expires_ts=NULL,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$2 AND task_asst_id=$3 AND assignment_type='USER' AND assignment_id=$1 AND active AND aggregate_version=$4 AND assignment_status_code='CLAIMED' AND claimed_by=$1 AND claim_expires_ts>CURRENT_TIMESTAMP RETURNING aggregate_version").bind(&caller.end_user_subject).bind(caller.host_id).bind(id).bind(r.assignment_version).fetch_optional(&state.pool).await.map_err(|e|AdminError::database(e).into_response())?.ok_or_else(||AdminError::conflict("CLAIM_NOT_OWNED","active claim is not owned by this caller").into_response())?;
    Ok(Json(
        json!({"taskAsstId":id,"assignmentVersion":version,"assignmentStatus":"ASSIGNED"}),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompleteRequest {
    assignment_version: i64,
    decision: Value,
    comment: Option<String>,
    idempotency_key: Option<String>,
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
    if r.comment.as_ref().is_some_and(|v| v.len() > 4000) {
        return Err(AdminError::validation("comment exceeds 4000 characters").into_response());
    }
    if r.idempotency_key
        .as_ref()
        .is_some_and(|v| v.len() < 8 || v.len() > 128)
    {
        return Err(
            AdminError::bad("idempotencyKey must contain 8 to 128 characters").into_response(),
        );
    }
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|e| AdminError::database(e).into_response())?;
    let row = lock_assignment(&mut tx, caller.host_id, id, &caller.end_user_subject)
        .await
        .map_err(IntoResponse::into_response)?;
    let status: String = row.get("assignment_status_code");
    if status == "COMPLETED" {
        if r.idempotency_key.as_deref()
            == row
                .get::<Option<String>, _>("completion_idempotency_key")
                .as_deref()
        {
            return Ok(Json(
                json!({"taskAsstId":id,"assignmentVersion":row.get::<i64,_>("aggregate_version"),"assignmentStatus":"COMPLETED","completionId":row.get::<Uuid,_>("completion_id"),"completionRecorded":true}),
            ));
        }
        return Err(AdminError::conflict(
            "ALREADY_COMPLETED",
            "task assignment is already completed",
        )
        .into_response());
    }
    if deadline_expired(
        row.get::<Option<DateTime<Utc>>, _>("deadline_ts"),
        Utc::now(),
    ) {
        return Err(
            AdminError::conflict("TASK_EXPIRED", "human task deadline has expired").into_response(),
        );
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
        )
        .into_response());
    }
    if row.get::<i64, _>("aggregate_version") != r.assignment_version {
        return Err(
            AdminError::conflict("VERSION_CONFLICT", "assignment version is stale").into_response(),
        );
    }
    let output: Value = row.get("task_output");
    validate_decision(
        output.get("ask").unwrap_or(&Value::Null),
        &r.decision,
        r.comment.as_deref(),
    )
    .map_err(IntoResponse::into_response)?;
    let completion_id = Uuid::now_v7();
    let task_id: Uuid = row.get("task_id");
    let updated=sqlx::query("UPDATE task_info_t SET status_code='C',locked='N',completed_ts=CURRENT_TIMESTAMP,completed_user=$1,result_code=$2,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$3 AND task_id=$4 AND status_code='W'").bind(&caller.end_user_subject).bind(r.decision.to_string()).bind(caller.host_id).bind(task_id).execute(&mut *tx).await.map_err(|e|AdminError::database(e).into_response())?;
    if updated.rows_affected() != 1 {
        return Err(
            AdminError::conflict("INVALID_STATE", "human task is no longer waiting")
                .into_response(),
        );
    }
    let version=sqlx::query_scalar::<_,i64>("UPDATE task_asst_t SET assignment_status_code='COMPLETED',decision=$1,decision_comment=$2,completion_id=$3,completion_idempotency_key=$4,completed_ts=CURRENT_TIMESTAMP,active=FALSE,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$5 WHERE host_id=$6 AND task_asst_id=$7 RETURNING aggregate_version").bind(&r.decision).bind(&r.comment).bind(completion_id).bind(&r.idempotency_key).bind(&caller.end_user_subject).bind(caller.host_id).bind(id).fetch_one(&mut *tx).await.map_err(|e|AdminError::database(e).into_response())?;
    sqlx::query("UPDATE task_asst_t SET assignment_status_code='CANCELLED',claimed_by=NULL,claimed_ts=NULL,claim_expires_ts=NULL,active=FALSE,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,update_user=$1 WHERE host_id=$2 AND task_id=$3 AND task_asst_id<>$4 AND active")
        .bind(&caller.end_user_subject)
        .bind(caller.host_id)
        .bind(task_id)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| AdminError::database(e).into_response())?;
    tx.commit()
        .await
        .map_err(|e| AdminError::database(e).into_response())?;
    Ok(Json(
        json!({"taskAsstId":id,"assignmentVersion":version,"assignmentStatus":"COMPLETED","completionId":completion_id,"completionRecorded":true}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

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
async fn lock_assignment(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    id: Uuid,
    subject: &str,
) -> Result<sqlx::postgres::PgRow, AdminError> {
    sqlx::query("SELECT a.*,t.task_output,t.deadline_ts FROM task_asst_t a JOIN task_info_t t ON t.host_id=a.host_id AND t.task_id=a.task_id WHERE a.host_id=$1 AND a.task_asst_id=$2 AND a.assignment_type='USER' AND a.assignment_id=$3 FOR UPDATE OF a,t").bind(host_id).bind(id).bind(subject).fetch_optional(&mut **tx).await.map_err(AdminError::database)?.ok_or_else(AdminError::not_found)
}
