//! Internal read-only authorization for an already stored Agent job. The caller
//! selects a job; its identity, grant and inherited limits come from stores.
use axum::{
    Json, Router,
    extract::{ConnectInfo, State},
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
    pub pool: PgPool,
    pub broker: Arc<crate::credential_broker::CredentialBroker>,
    pub security: Arc<SecurityRuntime>,
    pub policy: RoutePolicy,
    pub agents: BTreeMap<String, Uuid>,
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
        .layer(axum::extract::DefaultBodyLimit::max(1024))
        .with_state(state))
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
    let def = s.agents.get(&sid).ok_or(StatusCode::FORBIDDEN)?;
    let mut tx = s
        .pool
        .begin()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Do not lock j here: the Agent may hold its admission row lock while it
    // requests this check. The job's producer/consumer store is trusted; no
    // model-facing API can change its root run or identity fields.
    let row=sqlx::query("SELECT i.workflow_instance_id,a.grant_id,a.user_id FROM agent_job_t j JOIN workflow_ops.workflow_invocation_t i ON i.host_id=j.host_id AND i.process_id=j.workflow_process_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE j.host_id=$1 AND j.job_id=$2 AND j.agent_def_id=$3 AND j.state IN('PENDING','TURN_CREATED','RUNNING') AND j.cancellation_requested_ts IS NULL AND j.deadline_ts>clock_timestamp() AND j.deadline_ts<=i.deadline_ts AND j.delegation_depth=i.permit_depth AND j.delegation_depth<=j.maximum_delegation_depth AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND i.deadline_ts>clock_timestamp() AND a.active AND a.deadline>clock_timestamp() AND a.user_id::text=i.end_user_subject FOR SHARE OF i,a")
        .bind(request.host_id).bind(request.job_id).bind(def).fetch_optional(&mut *tx).await.map_err(|_|StatusCode::SERVICE_UNAVAILABLE)?.ok_or(StatusCode::FORBIDDEN)?;
    let _grant = s
        .broker
        .lock_run_authority(
            row.get("workflow_instance_id"),
            row.get("grant_id"),
            request.host_id,
            row.get("user_id"),
        )
        .await
        .map_err(|_| StatusCode::FORBIDDEN)?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(StatusCode::NO_CONTENT)
}
