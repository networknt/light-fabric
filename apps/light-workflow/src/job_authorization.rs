//! Internal read-only authorization for an already stored Agent job. The caller
//! selects a job; its identity, grant and inherited limits come from stores.
use axum::{
    Json, Router,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
};
use light_security::{
    SecurityRuntime,
    dual_identity::{self, Origin, RoutePolicy},
};
use sqlx::{PgPool, Row};
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;
#[derive(Clone)]
pub struct JobApi {
    pub supported_profiles: crate::profile_support::SupportedProfiles,
    pub pool: PgPool,
    pub authority: Arc<dyn crate::run_authority::RunAuthority>,
    pub security: Arc<SecurityRuntime>,
    pub policy: RoutePolicy,
    pub agents: BTreeMap<String, Uuid>,
    pub artifacts: Option<crate::artifact_store::DurableArtifactStore>,
    pub long: Option<Arc<crate::long_authority::LongAuthority>>,
    pub tokens: Arc<crate::run_token::RunTokenSelector>,
}
pub fn router(state: JobApi) -> Result<Router, String> {
    state
        .policy
        .validate()
        .map_err(|_| "invalid job caller policy")?;
    if state.agents.iter().any(|(sid, def)| {
        def.is_nil()
            || !state
                .policy
                .apps
                .get(sid)
                .is_some_and(|p| p.origin == Origin::Workflow)
    }) {
        return Err(
            "workflow Agent registrations require Workflow origin and verified peers".into(),
        );
    }
    Ok(Router::new()
        .route("/internal/workflow/jobs/authorize", post(authorize))
        .route("/internal/workflow/jobs/poll", post(poll))
        .route("/internal/workflow/jobs/report", post(report))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state))
}

pub fn long_router(state: JobApi) -> Result<Router, String> {
    state
        .policy
        .validate()
        .map_err(|_| "invalid job caller policy")?;
    if state.long.is_none()
        || state.agents.iter().any(|(sid, def)| {
            def.is_nil()
                || !state
                    .policy
                    .apps
                    .get(sid)
                    .is_some_and(|p| p.origin == Origin::Workflow)
        })
    {
        return Err("invalid LONG Agent registrations".into());
    }
    Ok(Router::new()
        .route(
            "/internal/workflow/{workflowClientId}/jobs/authorize",
            post(authorize_long),
        )
        .route(
            "/internal/workflow/{workflowClientId}/jobs/poll",
            post(poll_long),
        )
        .route(
            "/internal/workflow/{workflowClientId}/jobs/report",
            post(report_long),
        )
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state))
}

async fn peer_agent(
    s: &JobApi,
    peer: &light_axum::mtls::Peer,
    h: &HeaderMap,
    host: Uuid,
) -> Result<(String, Uuid), StatusCode> {
    let (_, sid, origin) =
        dual_identity::authenticate_application(&s.security, &s.policy, h, Some(&peer.fingerprint))
            .await
            .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Workflow || host != s.policy.host_id {
        return Err(StatusCode::FORBIDDEN);
    }
    let def = *s.agents.get(&sid).ok_or(StatusCode::FORBIDDEN)?;
    Ok((sid, def))
}

async fn token_agent(
    s: &JobApi,
    target: &str,
    headers: &HeaderMap,
    host: Uuid,
) -> Result<(String, Uuid, Arc<crate::long_authority::LongAuthority>), StatusCode> {
    let long = s.long.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    if target != long.client_id() || host != s.policy.host_id {
        return Err(StatusCode::FORBIDDEN);
    }
    let (_, sid, origin) =
        dual_identity::authenticate_application_token(&s.security, &s.policy, headers)
            .await
            .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Workflow {
        return Err(StatusCode::FORBIDDEN);
    }
    let def = *s.agents.get(&sid).ok_or(StatusCode::FORBIDDEN)?;
    Ok((sid, def, Arc::clone(long)))
}

async fn poll_long(
    State(s): State<JobApi>,
    Path(target): Path<String>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_job_transport::Poll>,
) -> Result<Json<Vec<light_client::workflow_job_transport::JobDelivery>>, StatusCode> {
    let (_, def, _) = token_agent(&s, &target, &h, request.host_id).await?;
    let jobs = pending_jobs(&s, request.host_id, def).await?;
    let mut deliveries = Vec::with_capacity(jobs.len());
    for job in jobs {
        let owner_token = if job.cancellation_requested {
            None
        } else {
            let (run, owner) = authorized_job(&s, job.host_id, job.job_id, def).await?;
            Some(
                s.tokens
                    .select_run_token(run, job.host_id, owner, chrono::Utc::now())
                    .await
                    .map_err(|_| StatusCode::FORBIDDEN)?,
            )
        };
        deliveries.push(light_client::workflow_job_transport::JobDelivery { job, owner_token });
    }
    Ok(Json(deliveries))
}

async fn authorize_long(
    State(s): State<JobApi>,
    Path(target): Path<String>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_jobs::Check>,
) -> Result<StatusCode, StatusCode> {
    let (_, def, _) = token_agent(&s, &target, &h, request.host_id).await?;
    authorized_job(&s, request.host_id, request.job_id, def).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn report_long(
    State(s): State<JobApi>,
    Path(target): Path<String>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_job_transport::Report>,
) -> Result<StatusCode, StatusCode> {
    let (sid, def, _) = token_agent(&s, &target, &h, request.host_id).await?;
    persist_verified_report_with_profiles(
        &s.pool,
        s.artifacts.as_ref(),
        &sid,
        def,
        request,
        &s.supported_profiles,
    )
    .await
}

async fn poll(
    State(s): State<JobApi>,
    ConnectInfo(peer): ConnectInfo<light_axum::mtls::Peer>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_job_transport::Poll>,
) -> Result<Json<Vec<light_client::workflow_job_transport::Job>>, StatusCode> {
    let (_, def) = peer_agent(&s, &peer, &h, request.host_id).await?;
    Ok(Json(pending_jobs(&s, request.host_id, def).await?))
}

async fn pending_jobs(
    s: &JobApi,
    host: Uuid,
    def: Uuid,
) -> Result<Vec<light_client::workflow_job_transport::Job>, StatusCode> {
    pending_jobs_guarded(&s.pool, &s.supported_profiles, host, def, |id| async move {
        authorized_job(s, host, id, def).await.map(|_| ())
    })
    .await
}

pub(crate) async fn pending_jobs_guarded<F, T>(
    pool: &PgPool,
    support: &crate::profile_support::SupportedProfiles,
    host: Uuid,
    def: Uuid,
    mut authorize: F,
) -> Result<Vec<light_client::workflow_job_transport::Job>, StatusCode>
where
    F: FnMut(Uuid) -> T,
    T: std::future::Future<Output = Result<(), StatusCode>>,
{
    // An offline Agent must still receive cancelled/expired jobs so it can
    // durably fence admission and acknowledge cleanup. PENDING in Workflow
    // alone is not proof that the Agent never received a previous poll.
    let rows = sqlx::query(&format!("SELECT j.*,p.expression_profile,p.definition_snapshot,p.definition_digest,i.end_user_subject,COALESCE((j.cancellation_requested_ts IS NOT NULL OR j.deadline_ts<=now() OR i.cancel_requested_ts IS NOT NULL OR i.state NOT IN('ACCEPTED','RUNNING','WAITING') OR p.deadline_ts<=now() OR (i.deadline_ts<=now() AND i.response_policy_snapshot->'privateExecutionProfile'->>'version' IS DISTINCT FROM '1')),false) AS cleanup_only FROM workflow_agent_job_t j JOIN workflow_invocation_t i ON i.host_id=j.host_id AND i.process_id=j.workflow_process_id JOIN process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id WHERE j.host_id=$1 AND j.agent_def_id=$2 AND j.state='PENDING' AND ({} OR j.cancellation_requested_ts IS NOT NULL OR j.deadline_ts<=now() OR i.cancel_requested_ts IS NOT NULL OR i.state NOT IN('ACCEPTED','RUNNING','WAITING') OR p.deadline_ts<=now() OR (i.deadline_ts<=now() AND i.response_policy_snapshot->'privateExecutionProfile'->>'version' IS DISTINCT FROM '1')) ORDER BY j.created_ts,j.job_id LIMIT 4", crate::profile_support::eligible("p","$3")))
        .bind(host).bind(def).bind(support.profiles()).fetch_all(pool).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
    let mut jobs = Vec::new();
    for row in rows {
        let id: Uuid = row.get("job_id");
        let cancellation_requested: bool = row.get("cleanup_only");
        if !cancellation_requested {
            let profile: String = row.get("expression_profile");
            let snapshot: Option<serde_json::Value> = row.get("definition_snapshot");
            let digest: Option<String> = row.get("definition_digest");
            match support.check(&profile, snapshot.as_ref(), digest.as_deref()) {
                crate::executor::expression_completion::ProfileDisposition::Deferred => continue,
                crate::executor::expression_completion::ProfileDisposition::Corrupt => {
                    let mut tx = pool
                        .begin()
                        .await
                        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                    crate::executor::expression_completion::reject_step(
                        &mut tx,
                        host,
                        row.get("workflow_process_id"),
                        row.get("workflow_task_id"),
                        "/definition_snapshot/expression_profile",
                    )
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                    tx.commit()
                        .await
                        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                    continue;
                }
                _ => {}
            }
            match authorize(id).await {
                Ok(_) => {}
                Err(StatusCode::FORBIDDEN) => continue,
                Err(error) => return Err(error),
            }
        }
        jobs.push(light_client::workflow_job_transport::Job {
            host_id: host,
            job_id: id,
            process_id: row.get("workflow_process_id"),
            task_id: row.get("workflow_task_id"),
            agent_def_id: def,
            end_user_subject: row.get("end_user_subject"),
            input: row.get("input"),
            input_digest: row.get("input_schema_digest"),
            output_schema: row.get("output_schema"),
            deadline: row
                .get::<chrono::DateTime<chrono::Utc>, _>("deadline_ts")
                .to_rfc3339(),
            token_budget: row.get("token_budget"),
            cost_budget_micros: row.get("cost_budget_micros"),
            depth: row.get("delegation_depth"),
            maximum_depth: row.get("maximum_delegation_depth"),
            cancellation_requested,
        });
    }
    Ok(jobs)
}

async fn report(
    State(s): State<JobApi>,
    ConnectInfo(peer): ConnectInfo<light_axum::mtls::Peer>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_job_transport::Report>,
) -> Result<StatusCode, StatusCode> {
    let (sid, def) = peer_agent(&s, &peer, &h, request.host_id).await?;
    persist_verified_report_with_profiles(
        &s.pool,
        s.artifacts.as_ref(),
        &sid,
        def,
        request,
        &s.supported_profiles,
    )
    .await
}

/// Internal persistence seam. The HTTP handler must authenticate the mTLS Agent
/// and resolve its definition before calling; this is not a separate route.
#[doc(hidden)]
pub async fn persist_verified_report(
    pool: &PgPool,
    artifacts: Option<&crate::artifact_store::DurableArtifactStore>,
    sid: &str,
    def: Uuid,
    request: light_client::workflow_job_transport::Report,
) -> Result<StatusCode, StatusCode> {
    persist_verified_report_with_profiles(
        pool,
        artifacts,
        sid,
        def,
        request,
        &crate::profile_support::SupportedProfiles::from_evaluator(false),
    )
    .await
}

fn report_recovery(
    committed: Option<&Option<serde_json::Value>>,
    value: &serde_json::Value,
) -> Option<Result<StatusCode, StatusCode>> {
    match committed {
        None => Some(Err(StatusCode::FORBIDDEN)),
        Some(Some(old)) => Some(if old == value {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(StatusCode::CONFLICT)
        }),
        Some(None) => None,
    }
}

#[derive(sqlx::FromRow)]
struct LockedSuccessJob {
    report: Option<serde_json::Value>,
    state: String,
    deadline_ts: chrono::DateTime<chrono::Utc>,
    cancellation_requested_ts: Option<chrono::DateTime<chrono::Utc>>,
    workflow_process_id: Uuid,
    workflow_task_id: Uuid,
}

fn fresh_success_gate(
    job: &LockedSuccessJob,
    value: &serde_json::Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool, StatusCode> {
    // An exact already-committed report is recovery, even after expiry/cancellation.
    if let Some(recovered) = report_recovery(Some(&job.report), value) {
        return recovered.map(|_| false);
    }
    if !matches!(job.state.as_str(), "PENDING" | "TURN_CREATED" | "RUNNING")
        || job.cancellation_requested_ts.is_some()
        || job.deadline_ts <= now
    {
        return Err(StatusCode::CONFLICT);
    }
    Ok(true)
}

pub async fn persist_verified_report_with_profiles(
    pool: &PgPool,
    artifacts: Option<&crate::artifact_store::DurableArtifactStore>,
    sid: &str,
    def: Uuid,
    request: light_client::workflow_job_transport::Report,
    support: &crate::profile_support::SupportedProfiles,
) -> Result<StatusCode, StatusCode> {
    let value = serde_json::to_value(&request).map_err(|_| StatusCode::BAD_REQUEST)?;
    let committed:Option<Option<serde_json::Value>>=sqlx::query_scalar("SELECT report FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2 AND agent_def_id=$3")
        .bind(request.host_id).bind(request.job_id).bind(def).fetch_optional(pool).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
    if let Some(status) = report_recovery(committed.as_ref(), &value) {
        return status;
    }
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let mut success_job = None;
    if request.state == "SUCCEEDED" {
        // Lock the execution-state parents before the first acceptance mutation.
        let (process,task):(Uuid,Uuid)=sqlx::query_as("SELECT workflow_process_id,workflow_task_id FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2 AND agent_def_id=$3")
            .bind(request.host_id).bind(request.job_id).bind(def).fetch_one(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
        let parent = crate::durable_timer::try_lock_parent(&mut tx, request.host_id, process)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        sqlx::query(
            "SELECT task_id FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT",
        )
        .bind(request.host_id)
        .bind(task)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        // Exact concurrent committed recovery must also precede fresh validation.
        let previous:Option<LockedSuccessJob>=sqlx::query_as("SELECT report,state,deadline_ts,cancellation_requested_ts,workflow_process_id,workflow_task_id FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2 AND agent_def_id=$3 FOR UPDATE")
        .bind(request.host_id).bind(request.job_id).bind(def).fetch_optional(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
        let previous = previous.ok_or(StatusCode::FORBIDDEN)?;
        let now = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        if !fresh_success_gate(&previous, &value, now)? {
            return Ok(StatusCode::NO_CONTENT);
        }
        if previous.workflow_process_id != process || previous.workflow_task_id != task {
            return Err(StatusCode::CONFLICT);
        }
        success_job = Some(previous);

        match crate::profile_support::read(&mut tx, request.host_id, process, support)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
        {
            crate::executor::expression_completion::ProfileDisposition::Deferred => {
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            crate::executor::expression_completion::ProfileDisposition::Corrupt => {
                crate::executor::expression_completion::reject_step(
                    &mut tx,
                    request.host_id,
                    process,
                    task,
                    "/definition_snapshot/expression_profile",
                )
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                tx.commit()
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                return Err(StatusCode::UNPROCESSABLE_ENTITY);
            }
            _ => {}
        }
        let live:bool=sqlx::query_scalar("SELECT active AND status_code IN ('A','W') AND (deadline_ts IS NULL OR deadline_ts>clock_timestamp()) FROM task_info_t WHERE host_id=$1 AND task_id=$2")
            .bind(request.host_id).bind(task).fetch_one(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
        if !live
            || parent.blocked.is_some()
            || parent.deadline.is_some_and(|d| d <= chrono::Utc::now())
        {
            return Err(StatusCode::CONFLICT);
        }
    } else {
        let previous:Option<Option<serde_json::Value>>=sqlx::query_scalar("SELECT report FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2 AND agent_def_id=$3 FOR UPDATE")
        .bind(request.host_id).bind(request.job_id).bind(def).fetch_optional(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
        match previous {
            None => return Err(StatusCode::FORBIDDEN),
            Some(Some(old)) if old != value => return Err(StatusCode::CONFLICT),
            Some(Some(_)) => return Ok(StatusCode::NO_CONTENT),
            Some(None) => {}
        }
    }
    if !matches!(
        request.state.as_str(),
        "SUCCEEDED" | "FAILED" | "CANCELLED" | "UNKNOWN"
    ) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut output = request.output.clone();
    if request.state != "SUCCEEDED" {
        crate::development_cancel::validate_cleanup(&request, sid)
            .map_err(|_| StatusCode::CONFLICT)?;
    }
    if request.state == "SUCCEEDED" {
        use execution_runner_protocol::{
            CleanupState, ExecutionSubject, NormalizedExecutionResult, OriginKind,
        };
        let normalized: NormalizedExecutionResult = serde_json::from_value(
            request
                .output
                .as_ref()
                .and_then(|v| v.get("result"))
                .cloned()
                .ok_or(StatusCode::BAD_REQUEST)?,
        )
        .map_err(|_| StatusCode::BAD_REQUEST)?;
        if normalized.cleanup_state != CleanupState::Confirmed
            || normalized.origin.kind != OriginKind::Agent
            || normalized.origin.host_id != request.host_id
            || normalized.origin.service_id != sid
            || normalized.state != execution_runner_protocol::AttemptState::Succeeded
            || normalized.attempt == 0
            || !matches!(normalized.subject,ExecutionSubject::AgentTurn{session_id,subject_id,turn_id} if session_id==request.job_id && subject_id==turn_id && !turn_id.is_nil())
            || request
                .output
                .as_ref()
                .and_then(|v| v.get("executionId"))
                .and_then(serde_json::Value::as_str)
                != Some(normalized.execution_id.0.to_string().as_str())
            || request
                .output
                .as_ref()
                .and_then(|v| v.get("fencingToken"))
                .and_then(serde_json::Value::as_i64)
                .is_none_or(|v| v < 1)
        {
            return Err(StatusCode::CONFLICT);
        }
        output = normalized.structured_output;
        if output.is_none() {
            return Err(StatusCode::CONFLICT);
        }
    }
    let mut state = request.state.clone();
    let mut error = request.error.clone();
    if request.state == "SUCCEEDED" {
        // Deadline can expire while structural validation runs. The job is still
        // locked, and no acceptance/turn/finding mutation has happened yet.
        let now = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        let job = success_job.as_ref().ok_or(StatusCode::CONFLICT)?;
        fresh_success_gate(job, &value, now)?;
        let outcome = crate::native_result::record(
            &mut tx,
            request.host_id,
            request.job_id,
            def,
            request.output.as_ref().ok_or(StatusCode::BAD_REQUEST)?,
        )
        .await
        .map_err(|error| match error {
            crate::development_store::StoreError::Database(_) => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        })?;
        if outcome == crate::native_result::RecordOutcome::InvalidReview {
            state = "FAILED".into();
            output = None;
            error = Some(serde_json::json!({
                "code":"NATIVE_REVIEW_OUTPUT_INVALID",
                "message":"Native review output failed its schema, binding or finding-ledger contract",
                "retryable":false
            }));
        }
    }
    if state == "SUCCEEDED" {
        output = Some(
            crate::snapshot_transfer::accept(
                &mut tx,
                artifacts,
                request.host_id,
                request.job_id,
                output.as_ref().ok_or(StatusCode::BAD_REQUEST)?,
            )
            .await
            .map_err(|_| StatusCode::CONFLICT)?,
        );
    }
    sqlx::query("UPDATE workflow_agent_job_t SET state=$3,public_output=$4,error=$5,report=$6,updated_ts=now() WHERE host_id=$1 AND job_id=$2")
        .bind(request.host_id).bind(request.job_id).bind(state).bind(output).bind(error).bind(value)
        .execute(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(StatusCode::NO_CONTENT)
}
async fn authorize(
    State(s): State<JobApi>,
    ConnectInfo(peer): ConnectInfo<light_axum::mtls::Peer>,
    h: HeaderMap,
    Json(request): Json<light_client::workflow_jobs::Check>,
) -> Result<StatusCode, StatusCode> {
    let (_, sid, origin) = dual_identity::authenticate_application(
        &s.security,
        &s.policy,
        &h,
        Some(&peer.fingerprint),
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if origin != Origin::Workflow || request.host_id != s.policy.host_id || request.job_id.is_nil()
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let def = *s.agents.get(&sid).ok_or(StatusCode::FORBIDDEN)?;
    authorized_job(&s, request.host_id, request.job_id, def).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn authorized_job(
    s: &JobApi,
    host: Uuid,
    job: Uuid,
    def: Uuid,
) -> Result<(Uuid, Uuid), StatusCode> {
    if host != s.policy.host_id || job.is_nil() {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut tx = s
        .pool
        .begin()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Do not lock j here: the Agent may hold its admission row lock while it
    // requests this check. The job's producer/consumer store is trusted; no
    // model-facing API can change its root run or identity fields.
    let row=sqlx::query("SELECT i.workflow_instance_id,a.grant_id,a.user_id FROM workflow_agent_job_t j JOIN workflow_ops.workflow_invocation_t i ON i.host_id=j.host_id AND i.process_id=j.workflow_process_id JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE j.host_id=$1 AND j.job_id=$2 AND j.agent_def_id=$3 AND j.state IN('PENDING','TURN_CREATED','RUNNING') AND j.cancellation_requested_ts IS NULL AND j.deadline_ts>clock_timestamp() AND j.deadline_ts<=a.deadline AND (j.deadline_ts<=i.deadline_ts OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1') AND (p.deadline_ts IS NULL OR j.deadline_ts<=p.deadline_ts) AND j.delegation_depth=i.permit_depth AND j.delegation_depth<=j.maximum_delegation_depth AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND (i.deadline_ts>clock_timestamp() OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1') AND a.active AND a.deadline>clock_timestamp() AND a.user_id::text=i.end_user_subject FOR SHARE OF i,a")
        .bind(host).bind(job).bind(def).fetch_optional(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?.ok_or(StatusCode::FORBIDDEN)?;
    let run: Uuid = row.get("workflow_instance_id");
    let owner: Uuid = row.get("user_id");
    let _grant = s
        .authority
        .lock_run_authority(run, row.get("grant_id"), host, owner)
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok((run, owner))
}

#[cfg(test)]
mod w6_recovery_tests {
    use super::*;
    #[test]
    fn w6_review_job_fences_precede_fresh_success_and_preserve_exact_recovery() {
        let now = chrono::Utc::now();
        let value = serde_json::json!({"state":"SUCCEEDED","output":{"fixture":true}});
        let mut job = LockedSuccessJob {
            report: None,
            state: "RUNNING".into(),
            deadline_ts: now + chrono::Duration::minutes(1),
            cancellation_requested_ts: None,
            workflow_process_id: Uuid::new_v4(),
            workflow_task_id: Uuid::new_v4(),
        };
        // Positive control: with a live parent/task, the job's own fence is eligible.
        assert_eq!(fresh_success_gate(&job, &value, now), Ok(true));
        for fence in [
            "expired",
            "cancel-requested",
            "cancelled",
            "failed",
            "succeeded",
            "unknown",
        ] {
            job.report = None;
            job.state = "RUNNING".into();
            job.deadline_ts = now + chrono::Duration::minutes(1);
            job.cancellation_requested_ts = None;
            match fence {
                "expired" => job.deadline_ts = now,
                "cancel-requested" => job.cancellation_requested_ts = Some(now),
                "cancelled" => job.state = "CANCELLED".into(),
                "failed" => job.state = "FAILED".into(),
                "succeeded" => job.state = "SUCCEEDED".into(),
                _ => job.state = "UNKNOWN".into(),
            }
            let mut accepted_attempt = None;
            let mut turn_consumed = false;
            let mut findings_consumed = false;
            let admission = fresh_success_gate(&job, &value, now);
            if matches!(admission, Ok(true)) {
                // Mock native_result acceptance mutation boundary.
                accepted_attempt = Some(1);
                turn_consumed = true;
                findings_consumed = true;
                job.report = Some(value.clone());
            }
            assert_eq!(admission, Err(StatusCode::CONFLICT), "{fence}");
            assert!(job.report.is_none());
            assert_eq!(accepted_attempt, None);
            assert!(!turn_consumed && !findings_consumed);
            job.report = Some(value.clone());
            assert_eq!(
                fresh_success_gate(&job, &value, now),
                Ok(false),
                "exact recovery: {fence}"
            );
            assert_eq!(
                fresh_success_gate(&job, &serde_json::json!({"different":true}), now),
                Err(StatusCode::CONFLICT)
            );
        }
        job.report = None;
        job.state = "PENDING".into();
        job.cancellation_requested_ts = None;
        job.deadline_ts = now + chrono::Duration::seconds(1);
        assert_eq!(fresh_success_gate(&job, &value, now), Ok(true));
        assert_eq!(
            fresh_success_gate(&job, &value, job.deadline_ts),
            Err(StatusCode::CONFLICT)
        );
    }
    #[test]
    fn exact_committed_report_recovers_without_fresh_success_validation() {
        // Recovery must accept an exact historical payload even when current
        // fresh-success validation would reject its structure or profile.
        let historical = serde_json::json!({"state":"SUCCEEDED","output":{"historical":true}});
        assert_eq!(
            report_recovery(Some(&Some(historical.clone())), &historical),
            Some(Ok(StatusCode::NO_CONTENT))
        );
        assert_eq!(
            report_recovery(
                Some(&Some(historical.clone())),
                &serde_json::json!({"state":"SUCCEEDED"})
            ),
            Some(Err(StatusCode::CONFLICT))
        );
        assert_eq!(report_recovery(Some(&None), &historical), None);
        assert_eq!(
            report_recovery(None, &historical),
            Some(Err(StatusCode::FORBIDDEN))
        );
    }
}
