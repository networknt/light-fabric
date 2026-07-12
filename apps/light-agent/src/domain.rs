use agent_core::{AgentSessionId, AgentTurnId, PolicySnapshot, sha256_digest};
use agent_materializer::MaterializationManifest;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgListener};
use uuid::Uuid;

use coding_agent_runtime::{CodingFixtureRequest, CodingTurnSpec, ImmutableRepositoryInput};
use execution_runner_protocol::{
    CommandExecutionSpec, ExecutionRequirements, HostExposure, IsolationBoundary,
};

#[derive(Clone)]
pub struct AgentRepository {
    pool: PgPool,
}

pub struct SessionSpec {
    pub host_id: Uuid,
    pub session_id: AgentSessionId,
    pub principal_id: String,
    pub user_id: Option<Uuid>,
    pub agent_def_id: Uuid,
    pub bank_id: Option<Uuid>,
    pub policy: PolicySnapshot,
    pub idle_expires_at: DateTime<Utc>,
    pub maximum_expires_at: DateTime<Utc>,
    pub resume_handle_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedTurn {
    pub turn_id: AgentTurnId,
    pub turn_sequence: i64,
    pub duplicate: bool,
    pub policy_digest: String,
    pub data_boundary_digest: String,
}

#[derive(Debug, Clone)]
struct PoolAssignment {
    pool_id: Uuid,
    compatibility_digest: String,
}

async fn resolve_pool(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    agent: Uuid,
    version: i64,
    policy: &str,
    boundary: &str,
    profile: &str,
) -> Result<Option<PoolAssignment>> {
    let configured: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_service_pool_t WHERE host_id=$1")
            .bind(host)
            .fetch_one(&mut **tx)
            .await?;
    let row = sqlx::query(
        "SELECT a.pool_id,a.compatibility_digest,p.compatibility_digest pool_digest,
            p.compatibility_dimensions FROM agent_pool_assignment_t a JOIN agent_service_pool_t p
              ON p.host_id=a.host_id AND p.pool_id=a.pool_id AND p.enabled=TRUE
            WHERE a.host_id=$1 AND a.agent_def_id=$2 AND a.agent_definition_version=$3
              AND a.policy_digest=$4 AND a.revoked_ts IS NULL FOR UPDATE OF a,p",
    )
    .bind(host)
    .bind(agent)
    .bind(version)
    .bind(policy)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        if configured > 0 {
            bail!("agent definition has no live compatible service-pool assignment")
        }
        return Ok(None);
    };
    let assignment: String = row.try_get("compatibility_digest")?;
    let pool: String = row.try_get("pool_digest")?;
    let dimensions: Value = row.try_get("compatibility_dimensions")?;
    let object = dimensions
        .as_object()
        .context("service-pool compatibility dimensions must be an object")?;
    for required in [
        "tenant",
        "identity",
        "modelCredential",
        "region",
        "dataBoundary",
        "network",
        "retention",
        "profile",
    ] {
        if object
            .get(required)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            bail!("service-pool compatibility dimension {required} is missing")
        }
    }
    let host_key = host.to_string();
    if object.get("tenant").and_then(Value::as_str) != Some(host_key.as_str())
        || object.get("dataBoundary").and_then(Value::as_str) != Some(boundary)
        || object.get("profile").and_then(Value::as_str) != Some(profile)
    {
        bail!("service-pool tenant, data boundary, or profile mismatch")
    }
    let computed = execution_runner_protocol::canonical_sha256(&dimensions)?;
    if assignment != pool || pool != computed {
        bail!("service-pool compatibility digest mismatch")
    }
    Ok(Some(PoolAssignment {
        pool_id: row.try_get("pool_id")?,
        compatibility_digest: pool,
    }))
}

async fn enforce_quotas(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    principal: &str,
    agent: Uuid,
    profile: &str,
    provider: &str,
    pool: Option<Uuid>,
    turn_id: Option<Uuid>,
    session_admission: bool,
    tokens: i64,
    cost: i64,
) -> Result<()> {
    let keys = [
        ("HOST", host.to_string()),
        ("PRINCIPAL", principal.to_string()),
        ("AGENT", agent.to_string()),
        ("PROFILE", profile.to_string()),
        ("PROVIDER", provider.to_string()),
        ("POOL", pool.map(|v| v.to_string()).unwrap_or_default()),
    ];
    for (kind, key) in keys {
        if key.is_empty() {
            continue;
        }
        let policies=sqlx::query("SELECT quota_id,maximum_active_sessions,maximum_queued_turns,
            maximum_running_turns,token_budget_per_window,cost_budget_micros_per_window,window_seconds
            FROM agent_quota_policy_t WHERE host_id=$1 AND scope_kind=$2 AND scope_key=$3 AND enabled=TRUE FOR UPDATE")
            .bind(host).bind(kind).bind(&key).fetch_all(&mut **tx).await?;
        for q in policies {
            if session_admission {
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_active_sessions")? {
                    let active:i64=match kind {"HOST"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND state='ACTIVE'").bind(host).fetch_one(&mut **tx).await?,
                    "PRINCIPAL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND principal_id=$2 AND state='ACTIVE'").bind(host).bind(principal).fetch_one(&mut **tx).await?,
                    "AGENT"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND agent_def_id=$2 AND state='ACTIVE'").bind(host).bind(agent).fetch_one(&mut **tx).await?,
                    "PROFILE"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE s.host_id=$1 AND p.product_profile_digest=$2 AND s.state='ACTIVE'").bind(host).bind(profile).fetch_one(&mut **tx).await?,
                    "PROVIDER"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t s JOIN agent_definition_t d ON d.host_id=s.host_id AND d.agent_def_id=s.agent_def_id WHERE s.host_id=$1 AND d.model_provider=$2 AND s.state='ACTIVE'").bind(host).bind(provider).fetch_one(&mut **tx).await?,
                    "POOL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND service_pool_id=$2 AND state='ACTIVE'").bind(host).bind(pool).fetch_one(&mut **tx).await?, _=>0};
                    if active >= i64::from(max) {
                        bail!("agent session quota exceeded for {kind}:{key}")
                    }
                }
            } else {
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_queued_turns")? {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state='QUEUED' AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent queued-turn quota exceeded for {kind}:{key}")
                    }
                }
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_running_turns")? {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL') AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent running-turn quota exceeded for {kind}:{key}")
                    }
                }
                let token_max = q.try_get::<Option<i64>, _>("token_budget_per_window")?;
                let cost_max = q.try_get::<Option<i64>, _>("cost_budget_micros_per_window")?;
                if token_max.is_some() || cost_max.is_some() {
                    let quota: Uuid = q.try_get("quota_id")?;
                    let window: i32 = q.try_get("window_seconds")?;
                    let ok:Option<Uuid>=sqlx::query_scalar("INSERT INTO agent_quota_usage_t(host_id,quota_id,window_start_ts,reserved_tokens,reserved_cost_micros) VALUES($1,$2,to_timestamp(floor(extract(epoch FROM now())/$3)*$3),$4,$5) ON CONFLICT(host_id,quota_id,window_start_ts) DO UPDATE SET reserved_tokens=agent_quota_usage_t.reserved_tokens+$4,reserved_cost_micros=agent_quota_usage_t.reserved_cost_micros+$5,updated_ts=now() WHERE ($6::bigint IS NULL OR agent_quota_usage_t.reserved_tokens+agent_quota_usage_t.consumed_tokens+$4<=$6) AND ($7::bigint IS NULL OR agent_quota_usage_t.reserved_cost_micros+agent_quota_usage_t.consumed_cost_micros+$5<=$7) RETURNING quota_id")
                        .bind(host).bind(quota).bind(window).bind(tokens).bind(cost).bind(token_max).bind(cost_max).fetch_optional(&mut **tx).await?;
                    if ok.is_none() {
                        bail!("agent token or cost quota exceeded for {kind}:{key}")
                    }
                    let turn_id = turn_id.context("turn quota reservation requires a turn id")?;
                    sqlx::query("INSERT INTO agent_quota_reservation_t(host_id,quota_id,turn_id,window_start_ts,reserved_tokens,reserved_cost_micros) VALUES($1,$2,$3,to_timestamp(floor(extract(epoch FROM now())/$4)*$4),$5,$6)")
                        .bind(host).bind(quota).bind(turn_id).bind(window).bind(tokens).bind(cost).execute(&mut **tx).await?;
                }
            }
        }
    }
    Ok(())
}

async fn reconcile_turn_quota_usage(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    turn_id: Uuid,
    actual_tokens: i64,
    actual_cost_micros: i64,
) -> Result<()> {
    let reservations = sqlx::query(
        "SELECT quota_id,window_start_ts,reserved_tokens,reserved_cost_micros
         FROM agent_quota_reservation_t
         WHERE host_id=$1 AND turn_id=$2 AND reconciled_ts IS NULL
         FOR UPDATE",
    )
    .bind(host_id)
    .bind(turn_id)
    .fetch_all(&mut **tx)
    .await?;
    for reservation in reservations {
        let quota_id: Uuid = reservation.try_get("quota_id")?;
        let window_start: DateTime<Utc> = reservation.try_get("window_start_ts")?;
        let reserved_tokens: i64 = reservation.try_get("reserved_tokens")?;
        let reserved_cost: i64 = reservation.try_get("reserved_cost_micros")?;
        sqlx::query(
            "UPDATE agent_quota_usage_t SET
               reserved_tokens=GREATEST(0,reserved_tokens-$4),
               reserved_cost_micros=GREATEST(0,reserved_cost_micros-$5),
               consumed_tokens=consumed_tokens+$6,
               consumed_cost_micros=consumed_cost_micros+$7,updated_ts=now()
             WHERE host_id=$1 AND quota_id=$2 AND window_start_ts=$3",
        )
        .bind(host_id)
        .bind(quota_id)
        .bind(window_start)
        .bind(reserved_tokens)
        .bind(reserved_cost)
        .bind(actual_tokens.max(0))
        .bind(actual_cost_micros.max(0))
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE agent_quota_reservation_t SET actual_tokens=$4,
               actual_cost_micros=$5,reconciled_ts=now(),updated_ts=now()
             WHERE host_id=$1 AND quota_id=$2 AND turn_id=$3 AND reconciled_ts IS NULL",
        )
        .bind(host_id)
        .bind(quota_id)
        .bind(turn_id)
        .bind(actual_tokens.max(0))
        .bind(actual_cost_micros.max(0))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

impl AgentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn spawn_result_reconciler(&self) -> tokio::task::JoinHandle<()> {
        let repository = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = repository.listen_and_reconcile().await {
                    tracing::warn!("agent execution-result reconciler disconnected: {error}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        })
    }

    async fn listen_and_reconcile(&self) -> Result<()> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("execution_result_ready_v1").await?;
        self.reconcile_execution_results().await?;
        self.reconcile_agent_jobs().await?;
        loop {
            tokio::select! {
                notification = listener.recv() => { notification?; self.reconcile_execution_results().await?; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    self.reconcile_agent_jobs().await?;
                    self.reconcile_execution_results().await?;
                    self.reconcile_expiry_and_cleanup().await?;
                    self.reconcile_projections().await?;
                    let retention_days = std::env::var("LIGHT_AGENT_QUOTA_USAGE_RETENTION_DAYS").ok()
                        .and_then(|value| value.parse::<i32>().ok()).unwrap_or(30).clamp(1, 3650);
                    self.sweep_quota_usage(retention_days, 1_000).await?;
                },
            }
        }
    }

    pub async fn sweep_quota_usage(&self, retention_days: i32, batch_size: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM agent_quota_usage_t u WHERE (u.host_id,u.quota_id,u.window_start_ts) IN
             (SELECT q.host_id,q.quota_id,q.window_start_ts FROM agent_quota_usage_t q
              WHERE q.window_start_ts < now()-make_interval(days=>$1)
                AND NOT EXISTS(SELECT 1 FROM agent_quota_reservation_t r
                  WHERE r.host_id=q.host_id AND r.quota_id=q.quota_id
                    AND r.window_start_ts=q.window_start_ts AND r.reconciled_ts IS NULL)
              ORDER BY q.window_start_ts LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(retention_days.clamp(1, 3650))
        .bind(batch_size.clamp(1, 10_000))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn reconcile_agent_jobs(&self) -> Result<u64> {
        let mut changed = 0;
        changed += sqlx::query("WITH expired AS (UPDATE agent_job_t SET state='FAILED',
                    error=jsonb_build_object('class','deadline_exceeded'),terminal_ts=now(),updated_ts=now()
                    WHERE state IN('PENDING','TURN_CREATED','RUNNING') AND deadline_ts<=now()
                    RETURNING host_id,turn_id) UPDATE agent_turn_t t SET state='CANCELLED',
                    terminal_error=jsonb_build_object('class','deadline_exceeded'),terminal_ts=now(),updated_ts=now()
                    FROM expired WHERE t.host_id=expired.host_id AND t.turn_id=expired.turn_id
                      AND t.state NOT IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?.rows_affected();
        for _ in 0..100 {
            let mut tx = self.pool.begin().await?;
            let row=sqlx::query("SELECT j.host_id,j.job_id,j.agent_def_id,j.idempotency_key,j.policy_digest,
                    j.data_boundary_digest,j.deadline_ts,j.token_budget,j.cost_budget_micros,j.delegation_depth,
                    d.aggregate_version,d.policy_snapshot_id,d.model_provider,d.model_name
                 FROM agent_job_t j JOIN agent_definition_t d ON d.host_id=j.host_id AND d.agent_def_id=j.agent_def_id
                 JOIN agent_policy_snapshot_t p ON p.host_id=d.host_id AND p.policy_snapshot_id=d.policy_snapshot_id
                   AND p.policy_digest=j.policy_digest AND p.data_boundary_digest=j.data_boundary_digest AND p.revoked_ts IS NULL
                 WHERE j.state='PENDING' AND j.deadline_ts>now() ORDER BY j.created_ts,j.job_id
                 LIMIT 1 FOR UPDATE OF j SKIP LOCKED")
                .fetch_optional(&mut *tx).await?;
            let Some(row) = row else {
                tx.commit().await?;
                break;
            };
            let host: Uuid = row.try_get("host_id")?;
            let job: Uuid = row.try_get("job_id")?;
            let turn = Uuid::now_v7();
            let deadline: DateTime<Utc> = row.try_get("deadline_ts")?;
            sqlx::query("INSERT INTO agent_session_t(host_id,session_id,principal_id,agent_def_id,
                    agent_definition_version,policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$7,$8) ON CONFLICT(host_id,session_id) DO NOTHING")
                .bind(host).bind(job).bind(format!("workflow-job:{job}"))
                .bind(row.try_get::<Uuid,_>("agent_def_id")?).bind(row.try_get::<i64,_>("aggregate_version")?)
                .bind(row.try_get::<Uuid,_>("policy_snapshot_id")?).bind(deadline)
                .bind(sha256_digest(format!("workflow-job:{job}").as_bytes())).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO agent_turn_t(host_id,turn_id,session_id,turn_sequence,queue_sequence,
                    origin_kind,origin_ref,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,
                    data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,
                    cost_budget_micros,deadline_ts,delegation_depth)
                    VALUES($1,$2,$3,1,1,'workflow',$4,$5,$5,$6,$7,$8,$9,$10,20,$11,$12,$13,$14)")
                .bind(host).bind(turn).bind(job).bind(job.to_string())
                .bind(row.try_get::<String,_>("idempotency_key")?).bind(row.try_get::<Uuid,_>("policy_snapshot_id")?)
                .bind(row.try_get::<String,_>("policy_digest")?).bind(row.try_get::<String,_>("data_boundary_digest")?)
                .bind(row.try_get::<String,_>("model_provider")?).bind(row.try_get::<String,_>("model_name")?)
                .bind(row.try_get::<i64,_>("token_budget")?).bind(row.try_get::<i64,_>("cost_budget_micros")?)
                .bind(deadline).bind(row.try_get::<i32,_>("delegation_depth")?).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE agent_job_t SET turn_id=$1,state='TURN_CREATED',updated_ts=now()
                        WHERE host_id=$2 AND job_id=$3 AND state='PENDING'",
            )
            .bind(turn)
            .bind(host)
            .bind(job)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            changed += 1;
        }
        let terminal=sqlx::query("UPDATE agent_job_t j SET state=CASE t.state WHEN 'COMPLETED' THEN 'SUCCEEDED'
                    WHEN 'FAILED' THEN 'FAILED' WHEN 'CANCELLED' THEN 'CANCELLED' ELSE 'UNKNOWN' END,
                    public_output=CASE WHEN t.state='COMPLETED' THEN t.terminal_result END,
                    error=CASE WHEN t.state<>'COMPLETED' THEN t.terminal_error END,
                    terminal_ts=COALESCE(t.terminal_ts,now()),updated_ts=now()
                    FROM agent_turn_t t WHERE t.host_id=j.host_id AND t.turn_id=j.turn_id
                      AND j.state IN('TURN_CREATED','RUNNING') AND t.state IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?;
        let cancelled=sqlx::query("WITH jobs AS (UPDATE agent_job_t j SET state='CANCELLED',
                    error=jsonb_build_object('class','workflow_cancelled'),
                    cancellation_requested_ts=COALESCE(j.cancellation_requested_ts,now()),
                    terminal_ts=now(),updated_ts=now()
                    FROM task_info_t t,process_info_t p WHERE t.host_id=j.host_id
                      AND t.task_id=j.workflow_task_id AND p.host_id=j.host_id
                      AND p.process_id=j.workflow_process_id
                      AND j.state IN('PENDING','TURN_CREATED','RUNNING')
                      AND (p.status_code<>'A' OR t.status_code IN('F','X'))
                    RETURNING j.host_id,j.turn_id) UPDATE agent_turn_t t SET state='CANCELLED',
                    terminal_error=jsonb_build_object('class','workflow_cancelled'),terminal_ts=now(),updated_ts=now()
                    FROM jobs WHERE t.host_id=jobs.host_id AND t.turn_id=jobs.turn_id
                      AND t.state NOT IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?;
        sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,
                    execution_session_id,origin_kind,origin_service_id,origin_instance_id,
                    origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,
                    cleanup_deadline_ts,state)
                    SELECT j.host_id,gen_random_uuid(),s.execution_session_id,'agent','light-agent',
                      'workflow-job-reconciler',s.session_id,'agent-turn',j.turn_id,
                      'workflow-job-cancel:'||j.job_id,'workflow-cancelled','light-agent',
                      now()+interval '5 minutes','PENDING'
                    FROM agent_job_t j JOIN agent_session_t s ON s.host_id=j.host_id AND s.session_id=j.job_id
                    WHERE j.cancellation_requested_ts IS NOT NULL AND s.execution_session_id IS NOT NULL
                      AND s.cleanup_state IN('NOT_REQUIRED','CLEANUP_REQUESTED')
                    ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
            .execute(&self.pool).await?;
        sqlx::query("UPDATE agent_session_t s SET state='CLOSING',cleanup_state='CLEANUP_PENDING',updated_ts=now()
                    FROM agent_job_t j WHERE j.host_id=s.host_id AND j.job_id=s.session_id
                      AND j.cancellation_requested_ts IS NOT NULL AND s.execution_session_id IS NOT NULL
                      AND s.state='ACTIVE'").execute(&self.pool).await?;
        Ok(changed + terminal.rows_affected() + cancelled.rows_affected())
    }

    pub async fn reconcile_execution_results(&self) -> Result<u64> {
        let rows = sqlx::query("SELECT a.host_id,a.action_attempt_id,a.turn_id,a.execution_attempt_id FROM agent_action_attempt_t a JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id WHERE a.origin_accepted_ts IS NULL AND e.terminal_ts IS NOT NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        let mut accepted = 0;
        for row in rows {
            accepted += self
                .accept_execution_result(
                    row.try_get("host_id")?,
                    row.try_get("action_attempt_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_attempt_id")?,
                )
                .await? as u64;
        }
        let turns = sqlx::query("SELECT t.host_id,t.turn_id,e.execution_id FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.execution_attempt_id IS NULL AND e.terminal_ts IS NOT NULL AND e.accepted_by_origin_ts IS NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in turns {
            accepted += self
                .accept_coding_turn_result(
                    row.try_get("host_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_id")?,
                )
                .await? as u64;
        }
        Ok(accepted)
    }

    async fn accept_coding_turn_result(
        &self,
        host_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.host_id=$1 AND t.turn_id=$2 AND e.execution_id=$3 AND e.terminal_ts IS NOT NULL FOR UPDATE OF t,e")
            .bind(host_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let session: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?});
        let actual_tokens = result
            .pointer("/result/usage/totalTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let actual_cost = result
            .pointer("/result/usage/costMicros")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        append_event(
            &mut tx,
            host_id,
            session,
            Some(turn_id),
            None,
            "runner",
            "CODING_TURN_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET execution_attempt_id=$3,state=CASE WHEN $4='SUCCEEDED' THEN 'COMPLETED' WHEN $4='CANCELLED' THEN 'CANCELLED' WHEN $4='UNKNOWN' THEN 'UNKNOWN' ELSE 'FAILED' END,terminal_result=CASE WHEN $4='SUCCEEDED' THEN $5 ELSE terminal_result END,terminal_error=CASE WHEN $4<>'SUCCEEDED' THEN $5 ELSE terminal_error END,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND execution_attempt_id IS NULL AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id).bind(execution_id).bind(&state).bind(&result).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2").bind(host_id).bind(execution_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(host_id).bind(session).bind(turn_id).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id, actual_tokens, actual_cost).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn schedule_coding_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        manifest: &MaterializationManifest,
        spec: &CodingTurnSpec,
        repository: &ImmutableRepositoryInput,
        fixture: &CodingFixtureRequest,
        compatibility_digest: &str,
    ) -> Result<Uuid> {
        spec.validate()?;
        repository.validate(spec)?;
        fixture.validate()?;
        if &fixture.spec != spec {
            bail!("coding fixture spec differs from the admitted turn spec")
        }
        let manifest_digest = manifest.digest()?;
        if manifest.product_profile != agent_materializer::ProductProfile::Coding
            || spec.materialization_manifest_digest != manifest_digest
        {
            bail!("coding materialization profile or digest mismatch")
        }
        if !manifest.packages.is_empty() {
            bail!("the first Cube coding fixture admits only the immutable repository input")
        }
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,t.data_boundary_digest,s.principal_id FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id AND p.revoked_ts IS NULL WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state IN ('RECEIVED','RUNNING_MODEL') FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).fetch_one(&mut *tx).await?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let principal: String = row.try_get("principal_id")?;
        let request_id = Uuid::now_v7();
        let requirements = ExecutionRequirements {
            action_kind: "coding.fixture".into(),
            minimum_boundary: IsolationBoundary::MicroVm,
            maximum_host_exposure: HostExposure::None,
            network_enabled: false,
            credential_classes: vec![],
            persistent_workspace: false,
            required_features: vec![
                "deny-all-egress".into(),
                "immutable-repository-upload".into(),
                "canonical-patch-output".into(),
            ],
            policy_digest: policy.clone(),
            compatibility_digest: compatibility_digest.into(),
        };
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-coding-fixture-v1".into(),
            template_version: 1,
            template_digest:
                "sha256:503c1f8879addd7dec140d9f2e703e6b7230979188bbd6f7c9e4f941e276a717".into(),
            executable: "/usr/local/bin/light-coding-agent-fixture".into(),
            arguments: vec![
                "--repository".into(),
                "/inputs/repository.bundle".into(),
                "--request-base64".into(),
                fixture.encode_argument()?,
            ],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            wall_clock_timeout_ms: 120_000,
            stdout_limit_bytes: 1024 * 1024,
            stderr_limit_bytes: 1024 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        let execution_spec = serde_json::to_value(&command)?;
        sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,agent_session_id,agent_turn_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,state) VALUES($1,$2,$3,'agent','light-agent',$4,'agent-turn',$5,$6,$5,$7,$8,$9,$10,$11,'PENDING_CAPACITY')")
            .bind(host_id).bind(request_id).bind(format!("coding-turn:{}",turn_id.0)).bind(instance_id).bind(turn_id.0).bind(session_id.0).bind(snapshot).bind(&policy).bind(serde_json::to_value(requirements)?).bind(execution_spec).bind(format!("agent:{principal}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,mount_options) VALUES($1,$2,$3,'repository-bundle',$4,$5,$6,$7,'{}'::jsonb,jsonb_build_object('baseRevision',$8),'{}'::jsonb,jsonb_build_object('state','IMMUTABLE'),$9,'/inputs/repository.bundle',TRUE,FALSE,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb)")
            .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(&repository.artifact_uri).bind(&repository.digest).bind(repository.size as i64).bind(&repository.media_type).bind(&spec.base_revision).bind(format!("{}/inputs",spec.workspace_root)).execute(&mut *tx).await?;
        for package in &manifest.packages {
            let inserted=sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,trust_bundle_id,package_manifest_digest,mount_options) SELECT $1,$2,$3,'skill-package',p.object_reference,p.content_digest,p.size_bytes,p.media_type,jsonb_build_object('signer',p.signer_reference,'signature',p.signature_reference),jsonb_build_object('reference',p.provenance_reference),jsonb_build_object('scanner',p.scanner_reference,'digest',p.scan_digest),jsonb_build_object('state',p.state,'revokedTs',p.revoked_ts),$4,$5,TRUE,FALSE,p.signer_reference,$6,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb FROM skill_package_t p WHERE p.host_id=$1 AND p.package_id=$7 AND p.state='PUBLISHED' AND p.revoked_ts IS NULL AND p.content_digest=$6")
                .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(format!("{}/inputs",spec.workspace_root)).bind(&package.mount_target).bind(&package.content_digest).bind(package.package_id).execute(&mut *tx).await?;
            if inserted.rows_affected() != 1 {
                bail!(
                    "skill package {} became unavailable during admission",
                    package.package_id
                );
            }
        }
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","CODING_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision}),&policy).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn reconcile_expiry_and_cleanup(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_approval_t SET state='EXPIRED',decision_ts=now(),decision_reason='approval deadline expired' WHERE state='REQUESTED' AND expires_ts<=now()")
            .execute(&mut *tx).await?;
        let stale = sqlx::query("UPDATE agent_turn_t SET state='UNKNOWN',terminal_error=jsonb_build_object('message','turn deadline expired during reconciliation'),terminal_ts=now(),updated_ts=now() WHERE state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION') AND deadline_ts<=now() RETURNING host_id,session_id,turn_id")
            .fetch_all(&mut *tx).await?;
        for row in stale {
            sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(row.try_get::<Uuid,_>("host_id")?).bind(row.try_get::<Uuid,_>("session_id")?).bind(row.try_get::<Uuid,_>("turn_id")?).execute(&mut *tx).await?;
        }
        let expired = sqlx::query("UPDATE agent_session_t SET state='EXPIRED',cleanup_state=CASE WHEN execution_session_id IS NULL THEN 'NOT_REQUIRED' ELSE 'CLEANUP_REQUESTED' END,updated_ts=now() WHERE state='ACTIVE' AND LEAST(idle_expires_ts,maximum_expires_ts)<=now() RETURNING host_id,session_id,execution_session_id")
            .fetch_all(&mut *tx).await?;
        for row in expired {
            let host_id: Uuid = row.try_get("host_id")?;
            let session_id: Uuid = row.try_get("session_id")?;
            if let Some(execution_session_id) =
                row.try_get::<Option<Uuid>, _>("execution_session_id")?
            {
                let cleanup_id = Uuid::now_v7();
                sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,execution_session_id,origin_kind,origin_service_id,origin_instance_id,origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,cleanup_deadline_ts,state) VALUES($1,$2,$3,'agent','light-agent','session-reconciler',$4,'agent-turn',$4,$5,'session-expired','light-agent',now()+interval '5 minutes','PENDING') ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
                    .bind(host_id).bind(cleanup_id).bind(execution_session_id).bind(session_id).bind(format!("session-expired:{session_id}")).execute(&mut *tx).await?;
                sqlx::query("UPDATE agent_session_t SET cleanup_request_id=$3,cleanup_state='CLEANUP_PENDING' WHERE host_id=$1 AND session_id=$2").bind(host_id).bind(session_id).bind(cleanup_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reconcile_projections(&self) -> Result<()> {
        let rows = sqlx::query("SELECT s.host_id,s.session_id,h.bank_id FROM agent_session_t s JOIN agent_session_history_t h ON h.host_id=s.host_id AND h.durable_session_id=s.session_id WHERE h.projection_sequence < (SELECT COALESCE(MAX(e.event_sequence),0) FROM agent_session_event_t e WHERE e.host_id=s.host_id AND e.session_id=s.session_id) LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in rows {
            self.rebuild_history_projection(
                row.try_get("host_id")?,
                AgentSessionId(row.try_get("session_id")?),
                row.try_get("bank_id")?,
            )
            .await?;
        }
        Ok(())
    }

    async fn accept_execution_result(
        &self,
        host_id: Uuid,
        action_attempt_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error,e.fencing_token FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 AND e.execution_id=$4 AND e.terminal_ts IS NOT NULL FOR UPDATE OF a,t,e")
            .bind(host_id).bind(action_attempt_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(false);
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?,"fencingToken":row.try_get::<i64,_>("fencing_token")?});
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id),
            Some(action_attempt_id),
            "runner",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(&result).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2 AND terminal_ts IS NOT NULL")
            .bind(host_id).bind(execution_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state=CASE WHEN $3 IN ('SUCCEEDED','FAILED','CANCELLED') THEN 'RUNNING_MODEL' ELSE 'UNKNOWN' END,updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state IN ('RUNNING_ACTION','WAITING_RECONCILIATION')")
            .bind(host_id).bind(turn_id).bind(state).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn create_or_resume_session(&self, spec: &SessionSpec) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        persist_policy(&mut tx, spec.host_id, spec.agent_def_id, &spec.policy).await?;
        let policy_digest =
            sha256_digest(&serde_json::to_vec(&serde_json::to_value(&spec.policy)?)?);
        let (definition_version, provider): (i64, String) = sqlx::query_as(
            "SELECT aggregate_version,model_provider
            FROM agent_definition_t WHERE host_id=$1 AND agent_def_id=$2",
        )
        .bind(spec.host_id)
        .bind(spec.agent_def_id)
        .fetch_one(&mut *tx)
        .await?;
        let pool = resolve_pool(
            &mut tx,
            spec.host_id,
            spec.agent_def_id,
            definition_version,
            &policy_digest,
            &spec.policy.data_boundary_digest,
            &spec.policy.product_profile_digest,
        )
        .await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "agent-session:{}:{}",
                spec.host_id, spec.session_id.0
            ))
            .execute(&mut *tx)
            .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_session_t WHERE host_id=$1 AND session_id=$2)",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            enforce_quotas(
                &mut tx,
                spec.host_id,
                &spec.principal_id,
                spec.agent_def_id,
                &spec.policy.product_profile_digest,
                &provider,
                pool.as_ref().map(|p| p.pool_id),
                None,
                true,
                0,
                0,
            )
            .await?;
        }
        let result = sqlx::query(
            "INSERT INTO agent_session_t
             (host_id,session_id,principal_id,user_id,agent_def_id,agent_definition_version,bank_id,
              policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest,
              service_pool_id,service_pool_compatibility_digest)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
             ON CONFLICT (host_id,session_id) DO NOTHING",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .bind(&spec.principal_id)
        .bind(spec.user_id)
        .bind(spec.agent_def_id)
        .bind(definition_version)
        .bind(spec.bank_id)
        .bind(spec.policy.snapshot_id)
        .bind(spec.idle_expires_at)
        .bind(spec.maximum_expires_at)
        .bind(&spec.resume_handle_digest)
        .bind(pool.as_ref().map(|p| p.pool_id))
        .bind(pool.as_ref().map(|p| p.compatibility_digest.as_str()))
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            let row = sqlx::query("SELECT principal_id,agent_def_id,state,service_pool_id,service_pool_compatibility_digest FROM agent_session_t WHERE host_id=$1 AND session_id=$2 FOR UPDATE")
                .bind(spec.host_id).bind(spec.session_id.0).fetch_one(&mut *tx).await?;
            let principal: String = row.try_get("principal_id")?;
            let definition: Uuid = row.try_get("agent_def_id")?;
            let state: String = row.try_get("state")?;
            if principal != spec.principal_id
                || definition != spec.agent_def_id
                || state != "ACTIVE"
                || row.try_get::<Option<Uuid>, _>("service_pool_id")?
                    != pool.as_ref().map(|p| p.pool_id)
                || row.try_get::<Option<String>, _>("service_pool_compatibility_digest")?
                    != pool.as_ref().map(|p| p.compatibility_digest.clone())
            {
                bail!("durable agent session ownership or state mismatch");
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn admit_user_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        client_message_id: &str,
        text: &str,
        model_provider: &str,
        model_name: &str,
    ) -> Result<AdmittedTurn> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT next_turn_sequence,next_queue_sequence,policy_snapshot_id,
              p.policy_digest,p.data_boundary_digest,p.product_profile_digest,maximum_expires_ts,
              s.principal_id,s.agent_def_id,s.service_pool_id
              FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id
                AND p.policy_snapshot_id=s.policy_snapshot_id AND p.revoked_ts IS NULL
              WHERE s.host_id=$1 AND s.session_id=$2 AND s.state='ACTIVE'
                AND (s.service_pool_id IS NULL OR EXISTS(SELECT 1 FROM agent_pool_assignment_t a
                  JOIN agent_service_pool_t sp ON sp.host_id=a.host_id AND sp.pool_id=a.pool_id AND sp.enabled=TRUE
                  WHERE a.host_id=s.host_id AND a.agent_def_id=s.agent_def_id
                    AND a.agent_definition_version=s.agent_definition_version AND a.policy_digest=p.policy_digest
                    AND a.pool_id=s.service_pool_id AND a.compatibility_digest=s.service_pool_compatibility_digest
                    AND a.revoked_ts IS NULL)) FOR UPDATE OF s,p",
        )
        .bind(host_id)
        .bind(session_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .context("active agent session not found")?;
        if let Some(existing) = sqlx::query("SELECT turn_id,turn_sequence,policy_digest,data_boundary_digest FROM agent_turn_t WHERE host_id=$1 AND session_id=$2 AND client_message_id=$3")
            .bind(host_id).bind(session_id.0).bind(client_message_id).fetch_optional(&mut *tx).await? {
            tx.commit().await?;
            return Ok(AdmittedTurn { turn_id: AgentTurnId(existing.try_get("turn_id")?), turn_sequence: existing.try_get("turn_sequence")?, duplicate: true, policy_digest: existing.try_get("policy_digest")?, data_boundary_digest: existing.try_get("data_boundary_digest")? });
        }
        let turn_sequence: i64 = row.try_get("next_turn_sequence")?;
        let queue_sequence: i64 = row.try_get("next_queue_sequence")?;
        let policy_snapshot_id: Uuid = row.try_get("policy_snapshot_id")?;
        let policy_digest: String = row.try_get("policy_digest")?;
        let boundary: String = row.try_get("data_boundary_digest")?;
        let maximum: DateTime<Utc> = row.try_get("maximum_expires_ts")?;
        let principal: String = row.try_get("principal_id")?;
        let agent: Uuid = row.try_get("agent_def_id")?;
        let pool: Option<Uuid> = row.try_get("service_pool_id")?;
        let profile: String = row.try_get("product_profile_digest")?;
        let turn_id = AgentTurnId::new();
        enforce_quotas(
            &mut tx,
            host_id,
            &principal,
            agent,
            &profile,
            model_provider,
            pool,
            Some(turn_id.0),
            false,
            65_536,
            0,
        )
        .await?;
        let deadline = std::cmp::min(Utc::now() + Duration::minutes(2), maximum);
        sqlx::query("INSERT INTO agent_turn_t (host_id,turn_id,session_id,turn_sequence,queue_sequence,origin_kind,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,cost_budget_micros,deadline_ts,service_pool_id) VALUES ($1,$2,$3,$4,$5,'user',$6,$6,$7,$8,$9,$10,$11,20,65536,0,$12,$13)")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).bind(turn_sequence).bind(queue_sequence).bind(client_message_id)
            .bind(policy_snapshot_id).bind(&policy_digest).bind(&boundary).bind(model_provider).bind(model_name).bind(deadline).bind(pool).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET next_turn_sequence=next_turn_sequence+1,next_queue_sequence=next_queue_sequence+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2")
            .bind(host_id).bind(session_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "user",
            "USER_MESSAGE",
            json!({"text": text}),
            &policy_digest,
        )
        .await?;
        tx.commit().await?;
        Ok(AdmittedTurn {
            turn_id,
            turn_sequence,
            duplicate: false,
            policy_digest,
            data_boundary_digest: boundary,
        })
    }

    pub async fn activate_next_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
    ) -> Result<Option<AgentTurnId>> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query("SELECT active_turn_id,service_pool_id FROM agent_session_t WHERE host_id=$1 AND session_id=$2 AND state='ACTIVE' FOR UPDATE")
            .bind(host_id).bind(session_id.0).fetch_optional(&mut *tx).await?;
        let Some(row) = locked else {
            tx.commit().await?;
            return Ok(None);
        };
        if row.try_get::<Option<Uuid>, _>("active_turn_id")?.is_some() {
            tx.commit().await?;
            return Ok(None);
        }
        if let Some(pool_id) = row.try_get::<Option<Uuid>, _>("service_pool_id")? {
            let available:bool=sqlx::query_scalar("SELECT (SELECT COUNT(*) FROM agent_turn_t
                  WHERE host_id=$1 AND service_pool_id=$2 AND state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL')) < maximum_concurrency
                FROM agent_service_pool_t WHERE host_id=$1 AND pool_id=$2 AND enabled=TRUE FOR UPDATE")
                .bind(host_id).bind(pool_id).fetch_optional(&mut *tx).await?.unwrap_or(false);
            if !available {
                tx.commit().await?;
                return Ok(None);
            }
        }
        let turn = sqlx::query("SELECT turn_id FROM agent_turn_t WHERE host_id=$1 AND session_id=$2 AND state='QUEUED' ORDER BY queue_sequence FOR UPDATE SKIP LOCKED LIMIT 1")
            .bind(host_id).bind(session_id.0).fetch_optional(&mut *tx).await?;
        let Some(turn) = turn else {
            tx.commit().await?;
            return Ok(None);
        };
        let turn_id: Uuid = turn.try_get("turn_id")?;
        sqlx::query("UPDATE agent_turn_t SET state='RECEIVED',activated_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='QUEUED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=$3,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id IS NULL")
            .bind(host_id).bind(session_id.0).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE agent_job_t SET state='RUNNING',updated_ts=now()
                    WHERE host_id=$1 AND turn_id=$2 AND state='TURN_CREATED'",
        )
        .bind(host_id)
        .bind(turn_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(AgentTurnId(turn_id)))
    }

    pub async fn propose_gateway_action(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        stable_tool_ref: Uuid,
        model_alias: &str,
        arguments: &str,
    ) -> Result<(Uuid, Uuid)> {
        let mut tx = self.pool.begin().await?;
        let policy: String = sqlx::query_scalar("SELECT policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 AND state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION') FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let logical_action_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let tool_ref = stable_tool_ref;
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,gateway_request_id) VALUES($1,$2,$3,$4,1,$5,$6,'gateway',$7,$8,$9,'unknown','DISPATCHED',$10)")
            .bind(host_id).bind(attempt_id).bind(turn_id.0).bind(logical_action_id).bind(tool_ref).bind(model_alias)
            .bind(sha256_digest(model_alias.as_bytes())).bind(&policy).bind(sha256_digest(arguments.as_bytes())).bind(Uuid::now_v7()).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        let session_id = session_id_for_turn(&mut tx, host_id, turn_id.0).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(attempt_id),
            "agent",
            "ACTION_DISPATCHED",
            json!({"modelAlias":model_alias,"placement":"gateway"}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok((attempt_id, tool_ref))
    }

    pub async fn accept_gateway_result(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        action_attempt_id: Uuid,
        succeeded: bool,
        result: Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 FOR UPDATE OF a,t")
            .bind(host_id).bind(action_attempt_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(());
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(action_attempt_id),
            "gateway",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(result.clone()).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_MODEL',updated_ts=now(),terminal_error=CASE WHEN $3 THEN terminal_error ELSE $4 END WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(succeeded).bind((!succeeded).then_some(result)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_approval(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        logical_action_id: Uuid,
        input_digest: &str,
        subject_digest: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT session_id,policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let approval_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_approval_t(host_id,approval_id,turn_id,logical_action_id,subject_digest,input_digest,policy_digest,approver_scope,nonce_digest,expires_ts) VALUES($1,$2,$3,$4,$5,$6,$7,'{}',$8,$9)")
            .bind(host_id).bind(approval_id).bind(turn_id.0).bind(logical_action_id).bind(subject_digest).bind(input_digest).bind(&policy).bind(sha256_digest(Uuid::now_v7().as_bytes())).bind(expires_at).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_APPROVAL',updated_ts=now() WHERE host_id=$1 AND turn_id=$2").bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            None,
            "agent",
            "APPROVAL_REQUESTED",
            json!({"approvalId":approval_id,"logicalActionId":logical_action_id}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok(approval_id)
    }

    pub async fn approve_and_create_fresh_attempt(
        &self,
        host_id: Uuid,
        approval_id: Uuid,
        actor: &str,
        stable_tool_ref: Uuid,
        model_alias: &str,
        placement: &str,
        schema_digest: &str,
        argument_digest: &str,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.turn_id,a.logical_action_id,a.policy_digest,t.session_id FROM agent_approval_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.approval_id=$2 AND a.state='REQUESTED' AND a.expires_ts>now() FOR UPDATE OF a,t")
            .bind(host_id).bind(approval_id).fetch_optional(&mut *tx).await?.context("approval is unavailable or expired")?;
        let turn_id: Uuid = row.try_get("turn_id")?;
        let logical: Uuid = row.try_get("logical_action_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let session: Uuid = row.try_get("session_id")?;
        let attempt_number: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(attempt_number),0)+1 FROM agent_action_attempt_t WHERE host_id=$1 AND turn_id=$2 AND logical_action_id=$3").bind(host_id).bind(turn_id).bind(logical).fetch_one(&mut *tx).await?;
        let attempt_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,approval_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'unknown','READY',$12)")
            .bind(host_id).bind(attempt_id).bind(turn_id).bind(logical).bind(attempt_number).bind(stable_tool_ref).bind(model_alias).bind(placement).bind(schema_digest).bind(&policy).bind(argument_digest).bind(approval_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_approval_t SET state='APPROVED',decision_actor=$3,decision_ts=now(),consumed_action_attempt_id=$4 WHERE host_id=$1 AND approval_id=$2 AND state='REQUESTED'").bind(host_id).bind(approval_id).bind(actor).bind(attempt_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='WAITING_APPROVAL'").bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session,Some(turn_id),Some(attempt_id),"approver","APPROVAL_GRANTED",json!({"approvalId":approval_id,"freshAttempt":attempt_id,"attemptNumber":attempt_number}),&policy).await?;
        tx.commit().await?;
        Ok(attempt_id)
    }

    pub async fn complete_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        response: &str,
        actual_tokens: i64,
        actual_cost_micros: i64,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE",
        )
        .bind(host_id)
        .bind(turn_id.0)
        .fetch_one(&mut *tx)
        .await?;
        let policy: String = row.try_get("policy_digest")?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "model",
            "MODEL_RESULT",
            json!({"text":response}),
            &policy,
        )
        .await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "system",
            "TURN_COMPLETED",
            json!({}),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET state='COMPLETED',terminal_result=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"text":response})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(
            &mut tx,
            host_id,
            turn_id.0,
            actual_tokens,
            actual_cost_micros,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn fail_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_turn_t SET state='FAILED',terminal_error=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"message":reason})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id.0, 0, 0).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn rebuild_history_projection(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        bank_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let events = sqlx::query("SELECT event_sequence,event_type,content FROM agent_session_event_t WHERE host_id=$1 AND session_id=$2 AND event_type IN ('USER_MESSAGE','MODEL_RESULT') ORDER BY event_sequence")
            .bind(host_id).bind(session_id.0).fetch_all(&mut *tx).await?;
        let mut messages = Vec::with_capacity(events.len());
        let mut sequence = 0_i64;
        for event in events {
            sequence = event.try_get("event_sequence")?;
            let kind: String = event.try_get("event_type")?;
            let content: Value = event.try_get("content")?;
            messages.push(json!({"role": if kind == "USER_MESSAGE" {"user"} else {"assistant"}, "content": content.get("text").cloned().unwrap_or(Value::Null)}));
        }
        sqlx::query("INSERT INTO agent_session_history_t(host_id,bank_id,session_id,durable_session_id,messages,projection_sequence) VALUES($1,$2,$3,$3,$4,$5) ON CONFLICT(host_id,bank_id,session_id) DO UPDATE SET messages=EXCLUDED.messages,durable_session_id=EXCLUDED.durable_session_id,projection_sequence=EXCLUDED.projection_sequence,aggregate_version=agent_session_history_t.aggregate_version+1,update_ts=now() WHERE agent_session_history_t.projection_sequence < EXCLUDED.projection_sequence")
            .bind(host_id).bind(bank_id).bind(session_id.0).bind(Value::Array(messages)).bind(sequence).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn persist_policy(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    agent_def_id: Uuid,
    policy: &PolicySnapshot,
) -> Result<()> {
    let value = serde_json::to_value(policy)?;
    let digest = sha256_digest(&serde_json::to_vec(&value)?);
    sqlx::query("INSERT INTO agent_policy_snapshot_t(host_id,policy_snapshot_id,agent_def_id,definition_digest,product_profile_digest,model_digest,catalog_digest,memory_digest,execution_digest,channel_digest,data_boundary_digest,resolved_snapshot,policy_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(host_id,policy_snapshot_id) DO NOTHING")
        .bind(host_id).bind(policy.snapshot_id).bind(agent_def_id).bind(&policy.definition_digest).bind(&policy.product_profile_digest).bind(&policy.model_digest).bind(&policy.catalog_digest).bind(&policy.memory_digest).bind(&policy.execution_digest).bind(&policy.channel_digest).bind(&policy.data_boundary_digest).bind(value).bind(digest).execute(&mut **tx).await?;
    Ok(())
}

async fn session_id_for_turn(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    turn_id: Uuid,
) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT session_id FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id)
            .bind(turn_id)
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    session_id: Uuid,
    turn_id: Option<Uuid>,
    action_attempt_id: Option<Uuid>,
    actor: &str,
    kind: &str,
    content: Value,
    policy_digest: &str,
) -> Result<()> {
    let digest = sha256_digest(&serde_json::to_vec(&content)?);
    sqlx::query("INSERT INTO agent_session_event_t(host_id,event_id,session_id,event_sequence,turn_id,action_attempt_id,actor_class,event_type,content,content_digest,policy_digest) SELECT $1,$2,$3,COALESCE(MAX(event_sequence),0)+1,$4,$5,$6,$7,$8,$9,$10 FROM agent_session_event_t WHERE host_id=$1 AND session_id=$3")
        .bind(host_id).bind(Uuid::now_v7()).bind(session_id).bind(turn_id).bind(action_attempt_id).bind(actor).bind(kind).bind(content).bind(digest).bind(policy_digest).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn durable_admission_is_idempotent_fifo_and_projection_rebuildable() {
        let Ok(url) = std::env::var("LIGHT_AGENT_TEST_DATABASE_URL") else {
            return;
        };
        let pool = PgPool::connect(&url).await.unwrap();
        let host_id = Uuid::now_v7();
        let agent_def_id = Uuid::now_v7();
        let owner = Uuid::now_v7();
        let domain = format!("agent-{}.test", host_id.simple());
        let mut setup = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO org_t(domain,org_name,org_desc,org_owner) VALUES($1,'agent-test','agent-test',$2)").bind(&domain).bind(owner).execute(&mut *setup).await.unwrap();
        sqlx::query(
            "INSERT INTO host_t(host_id,domain,sub_domain,host_owner) VALUES($1,$2,'test',$3)",
        )
        .bind(host_id)
        .bind(&domain)
        .bind(owner)
        .execute(&mut *setup)
        .await
        .unwrap();
        sqlx::query("INSERT INTO api_t(host_id,api_id,api_name,api_status) VALUES($1,'agent','agent','Published')").bind(host_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO api_version_t(host_id,api_version_id,api_id,api_version,api_type,service_id) VALUES($1,$2,'agent','1.0.0','mcp','agent-test')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO agent_definition_t(host_id,agent_def_id,model_provider,model_name) VALUES($1,$2,'mock','mock')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        setup.commit().await.unwrap();
        let repository = AgentRepository::new(pool.clone());
        let session = AgentSessionId::new();
        let digest = |name: &str| sha256_digest(name.as_bytes());
        repository
            .create_or_resume_session(&SessionSpec {
                host_id,
                session_id: session,
                principal_id: Uuid::now_v7().to_string(),
                user_id: None,
                agent_def_id,
                bank_id: None,
                policy: PolicySnapshot {
                    snapshot_id: session.0,
                    definition_digest: digest("definition"),
                    product_profile_digest: digest("profile"),
                    model_digest: digest("model"),
                    catalog_digest: digest("catalog"),
                    memory_digest: digest("memory"),
                    execution_digest: digest("execution"),
                    channel_digest: digest("channel"),
                    data_boundary_digest: digest("boundary"),
                    tools: BTreeMap::new(),
                },
                idle_expires_at: Utc::now() + Duration::hours(1),
                maximum_expires_at: Utc::now() + Duration::hours(2),
                resume_handle_digest: digest(&session.to_string()),
            })
            .await
            .unwrap();
        let first = repository
            .admit_user_turn(host_id, session, "message-1", "hello", "mock", "mock")
            .await
            .unwrap();
        let duplicate = repository
            .admit_user_turn(host_id, session, "message-1", "hello", "mock", "mock")
            .await
            .unwrap();
        let second = repository
            .admit_user_turn(host_id, session, "message-2", "again", "mock", "mock")
            .await
            .unwrap();
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert!(duplicate.duplicate);
        assert!(second.turn_sequence > first.turn_sequence);
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(first.turn_id)
        );
        repository
            .complete_turn(host_id, session, first.turn_id, "world", 1, 0)
            .await
            .unwrap();
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(second.turn_id)
        );
        sqlx::query("DELETE FROM agent_session_t WHERE host_id=$1 AND session_id=$2")
            .bind(host_id)
            .bind(session.0)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM agent_policy_snapshot_t WHERE host_id=$1 AND policy_snapshot_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM org_t WHERE domain=$1")
            .bind(domain)
            .execute(&pool)
            .await
            .unwrap();
    }
}
