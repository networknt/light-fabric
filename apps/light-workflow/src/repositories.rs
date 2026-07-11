use chrono::{DateTime, Utc};
use execution_runner_protocol::{
    AttemptState, ExecutionId, ExecutionRequirements, LeaseId, SchedulingRequestId,
};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
use workflow_policy::{ExecutionPlacement, ResolvedExecutionPolicy};

#[derive(Clone)]
pub struct WorkflowRepository {
    pool: PgPool,
}

#[derive(Debug, Clone)]
pub struct NewProcess<'a> {
    pub host_id: Uuid,
    pub process_id: Uuid,
    pub wf_def_id: Uuid,
    pub wf_instance_id: String,
    pub app_id: &'a str,
    pub input_data: &'a Value,
    pub definition_snapshot: &'a Value,
    pub definition_digest: &'a str,
    pub policy_snapshot_id: Uuid,
    pub policy_digest: &'a str,
    pub source_event_id: &'a str,
    pub execution_profile_id: &'a str,
}

#[derive(Debug, Clone)]
pub struct NewTask<'a> {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub task_type: &'a str,
    pub process_id: Uuid,
    pub wf_instance_id: String,
    pub wf_task_id: &'a str,
    pub task_input: &'a Value,
    pub placement: ExecutionPlacement,
    pub policy_digest: &'a str,
}

#[derive(Debug, Clone)]
pub struct NewSchedulingRequest<'a> {
    pub host_id: Uuid,
    pub request_id: SchedulingRequestId,
    pub origin_service_id: &'a str,
    pub origin_instance_id: &'a str,
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub policy_snapshot_id: Uuid,
    pub policy_digest: &'a str,
    pub requirements: &'a ExecutionRequirements,
    pub execution_spec: &'a Value,
    pub fairness_key: &'a str,
    pub priority: i32,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReservedRequest {
    pub host_id: Uuid,
    pub request_id: Uuid,
    pub origin_service_id: String,
    pub origin_instance_id: String,
    pub subject_id: Uuid,
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub selected_runner_session_id: Uuid,
    pub selected_backend_id: String,
    pub reservation_token_hash: String,
    pub reservation_expires_ts: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TerminalAttempt {
    pub host_id: Uuid,
    pub execution_id: Uuid,
    pub request_id: Uuid,
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub attempt_number: i32,
    pub lease_id: Uuid,
    pub fencing_token: i64,
    pub state: String,
    pub normalized_result: Option<Value>,
    pub normalized_error: Option<Value>,
}

impl WorkflowRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn find_process_by_source_event(
        tx: &mut Transaction<'_, Postgres>,
        host_id: Uuid,
        wf_def_id: Uuid,
        source_event_id: &str,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT process_id FROM process_info_t
             WHERE host_id = $1 AND wf_def_id = $2 AND source_event_id = $3",
        )
        .bind(host_id)
        .bind(wf_def_id)
        .bind(source_event_id)
        .fetch_optional(&mut **tx)
        .await
    }

    pub async fn store_policy_snapshot(
        tx: &mut Transaction<'_, Postgres>,
        host_id: Uuid,
        definition_digest: &str,
        policy: &ResolvedExecutionPolicy,
        created_by: &str,
    ) -> Result<Uuid, sqlx::Error> {
        let policy_snapshot_id = Uuid::now_v7();
        let profile_id = policy
            .profile
            .as_ref()
            .map(|profile| profile.id.as_str())
            .unwrap_or("host");
        let profile_version = policy
            .profile
            .as_ref()
            .map(|profile| profile.version as i32)
            .unwrap_or(1);
        let resolved_policy = serde_json::to_value(policy)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;

        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO workflow_execution_policy_t (
                policy_snapshot_id, host_id, definition_digest, profile_id,
                profile_version, resolved_policy, policy_digest, source, created_by
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, 'workflow-start', $8)
             ON CONFLICT (host_id, policy_digest) DO NOTHING
             RETURNING policy_snapshot_id",
        )
        .bind(policy_snapshot_id)
        .bind(host_id)
        .bind(definition_digest)
        .bind(profile_id)
        .bind(profile_version)
        .bind(resolved_policy)
        .bind(&policy.policy_digest)
        .bind(created_by)
        .fetch_optional(&mut **tx)
        .await?;

        if let Some(policy_snapshot_id) = inserted {
            return Ok(policy_snapshot_id);
        }
        sqlx::query_scalar(
            "SELECT policy_snapshot_id FROM workflow_execution_policy_t
             WHERE host_id = $1 AND policy_digest = $2",
        )
        .bind(host_id)
        .bind(&policy.policy_digest)
        .fetch_one(&mut **tx)
        .await
    }

    pub async fn insert_process_if_absent(
        tx: &mut Transaction<'_, Postgres>,
        process: &NewProcess<'_>,
    ) -> Result<bool, sqlx::Error> {
        let inserted: Option<Uuid> = sqlx::query_scalar(
            "INSERT INTO process_info_t (
                host_id, process_id, wf_def_id, wf_instance_id, app_id,
                process_type, status_code, started_ts, ex_trigger_ts,
                input_data, context_data, definition_snapshot, definition_digest,
                policy_snapshot_id, policy_digest, source_event_id, execution_profile_id
             ) VALUES (
                $1, $2, $3, $4, $5, 'Workflow', 'A', CURRENT_TIMESTAMP,
                CURRENT_TIMESTAMP, $6, $6, $7, $8, $9, $10, $11, $12
             )
             ON CONFLICT (host_id, wf_def_id, source_event_id)
                 WHERE source_event_id IS NOT NULL
             DO NOTHING
             RETURNING process_id",
        )
        .bind(process.host_id)
        .bind(process.process_id)
        .bind(process.wf_def_id)
        .bind(process.wf_instance_id.to_string())
        .bind(process.app_id)
        .bind(process.input_data)
        .bind(process.definition_snapshot)
        .bind(process.definition_digest)
        .bind(process.policy_snapshot_id)
        .bind(process.policy_digest)
        .bind(process.source_event_id)
        .bind(process.execution_profile_id)
        .fetch_optional(&mut **tx)
        .await?;
        Ok(inserted.is_some())
    }

    pub async fn insert_task(
        tx: &mut Transaction<'_, Postgres>,
        task: &NewTask<'_>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO task_info_t (
                host_id, task_id, task_type, process_id, wf_instance_id,
                wf_task_id, status_code, started_ts, locked, priority,
                task_input, execution_placement, task_policy_digest
             ) VALUES ($1, $2, $3, $4, $5, $6, 'A', CURRENT_TIMESTAMP,
                       'N', 1, $7, $8, $9)",
        )
        .bind(task.host_id)
        .bind(task.task_id)
        .bind(task.task_type)
        .bind(task.process_id)
        .bind(&task.wf_instance_id)
        .bind(task.wf_task_id)
        .bind(task.task_input)
        .bind(match task.placement {
            ExecutionPlacement::Host => "host",
            ExecutionPlacement::Runner => "runner",
        })
        .bind(task.policy_digest)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn create_scheduling_request(
        tx: &mut Transaction<'_, Postgres>,
        request: &NewSchedulingRequest<'_>,
    ) -> Result<SchedulingRequestId, sqlx::Error> {
        let idempotency_key = format!("workflow-task:{}", request.task_id);
        let requirements = serde_json::to_value(request.requirements)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let request_id: Uuid = request.request_id.0;
        let existing_or_inserted: Uuid = sqlx::query_scalar(
            "INSERT INTO runner_scheduling_request_t (
                host_id, request_id, idempotency_key, origin_kind,
                origin_service_id, origin_instance_id, subject_kind, subject_id,
                process_id, task_id, policy_snapshot_id, policy_digest,
                normalized_requirements, execution_spec, fairness_key, priority, state
             ) VALUES ($1, $2, $3, 'workflow', $4, $5, 'workflow-task', $6,
                       $7, $6, $8, $9, $10, $11, $12, $13, 'PENDING_CAPACITY')
             ON CONFLICT (host_id, origin_service_id, origin_instance_id, idempotency_key)
             DO UPDATE SET updated_ts = runner_scheduling_request_t.updated_ts
             RETURNING request_id",
        )
        .bind(request.host_id)
        .bind(request_id)
        .bind(idempotency_key)
        .bind(request.origin_service_id)
        .bind(request.origin_instance_id)
        .bind(request.task_id)
        .bind(request.process_id)
        .bind(request.policy_snapshot_id)
        .bind(request.policy_digest)
        .bind(requirements)
        .bind(request.execution_spec)
        .bind(request.fairness_key)
        .bind(request.priority)
        .fetch_one(&mut **tx)
        .await?;

        sqlx::query(
            "UPDATE task_info_t SET scheduling_request_id = $1, update_ts = CURRENT_TIMESTAMP
             WHERE host_id = $2 AND task_id = $3 AND execution_placement = 'runner'",
        )
        .bind(existing_or_inserted)
        .bind(request.host_id)
        .bind(request.task_id)
        .execute(&mut **tx)
        .await?;
        Ok(SchedulingRequestId(existing_or_inserted))
    }

    pub async fn claim_reserved_request(
        tx: &mut Transaction<'_, Postgres>,
        origin_service_id: &str,
    ) -> Result<Option<ReservedRequest>, sqlx::Error> {
        sqlx::query_as::<_, ReservedRequest>(
            "SELECT host_id, request_id, origin_service_id, origin_instance_id,
                    subject_id, process_id, task_id,
                    selected_runner_session_id, selected_backend_id,
                    reservation_token_hash, reservation_expires_ts
             FROM runner_scheduling_request_t
             WHERE state = 'RESERVED'
               AND origin_service_id = $1
               AND reservation_expires_ts > CURRENT_TIMESTAMP
             ORDER BY priority DESC, queue_sequence
             LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .bind(origin_service_id)
        .fetch_optional(&mut **tx)
        .await
    }

    pub async fn create_attempt_from_reservation(
        tx: &mut Transaction<'_, Postgres>,
        request: &ReservedRequest,
        expected_reservation_token_hash: &str,
        lease_deadline: DateTime<Utc>,
    ) -> Result<Option<(ExecutionId, LeaseId, u64)>, sqlx::Error> {
        if request.reservation_token_hash != expected_reservation_token_hash
            || request.reservation_expires_ts <= Utc::now()
        {
            return Ok(None);
        }
        let attempt_number: i32 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(attempt_number), 0) + 1 FROM execution_attempt_t
             WHERE host_id = $1 AND origin_service_id = $2 AND origin_instance_id = $3
               AND subject_kind = 'workflow-task' AND subject_id = $4",
        )
        .bind(request.host_id)
        .bind(&request.origin_service_id)
        .bind(&request.origin_instance_id)
        .bind(request.subject_id)
        .fetch_one(&mut **tx)
        .await?;
        let fencing_token: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(fencing_token), 0) + 1 FROM execution_attempt_t
             WHERE host_id = $1 AND origin_service_id = $2 AND origin_instance_id = $3
               AND subject_kind = 'workflow-task' AND subject_id = $4",
        )
        .bind(request.host_id)
        .bind(&request.origin_service_id)
        .bind(&request.origin_instance_id)
        .bind(request.subject_id)
        .fetch_one(&mut **tx)
        .await?;
        let execution_id = ExecutionId::new();
        let lease_id = LeaseId::new();
        let connection_generation: i64 = sqlx::query_scalar(
            "SELECT connection_generation FROM runner_session_t
             WHERE host_id = $1 AND session_id = $2 AND status = 'CONNECTED'
             FOR SHARE",
        )
        .bind(request.host_id)
        .bind(request.selected_runner_session_id)
        .fetch_one(&mut **tx)
        .await?;

        let inserted = sqlx::query(
            "INSERT INTO execution_attempt_t (
                host_id, execution_id, request_id, origin_kind, origin_service_id,
                origin_instance_id, subject_kind, subject_id, attempt_number,
                process_id, task_id, lease_id, fencing_token, runner_session_id,
                connection_generation, backend_id, state, lease_issued_ts,
                lease_deadline_ts, cleanup_state
             ) SELECT $1, $2, $3, 'workflow', $4, $5, 'workflow-task', $6, $7,
                      $8, $6, $9, $10, $11, $12, $13, 'CREATED',
                      CURRENT_TIMESTAMP, $14, 'REQUIRED'
               WHERE EXISTS (
                   SELECT 1 FROM runner_scheduling_request_t
                   WHERE host_id = $1 AND request_id = $3 AND state = 'RESERVED'
                     AND reservation_token_hash = $15
                     AND reservation_expires_ts > CURRENT_TIMESTAMP
               )",
        )
        .bind(request.host_id)
        .bind(execution_id.0)
        .bind(request.request_id)
        .bind(&request.origin_service_id)
        .bind(&request.origin_instance_id)
        .bind(request.task_id)
        .bind(attempt_number)
        .bind(request.process_id)
        .bind(lease_id.0)
        .bind(fencing_token)
        .bind(request.selected_runner_session_id)
        .bind(connection_generation)
        .bind(&request.selected_backend_id)
        .bind(lease_deadline)
        .bind(expected_reservation_token_hash)
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() != 1 {
            return Ok(None);
        }

        sqlx::query(
            "UPDATE runner_scheduling_request_t
             SET state = 'ATTEMPT_CREATED', updated_ts = CURRENT_TIMESTAMP
             WHERE host_id = $1 AND request_id = $2 AND state = 'RESERVED'
               AND reservation_token_hash = $3 AND reservation_expires_ts > CURRENT_TIMESTAMP",
        )
        .bind(request.host_id)
        .bind(request.request_id)
        .bind(expected_reservation_token_hash)
        .execute(&mut **tx)
        .await?;
        Ok(Some((execution_id, lease_id, fencing_token as u64)))
    }

    pub async fn pending_terminal_attempts(
        &self,
        origin_service_id: &str,
        limit: i64,
    ) -> Result<Vec<TerminalAttempt>, sqlx::Error> {
        sqlx::query_as::<_, TerminalAttempt>(
            "SELECT host_id, execution_id, request_id, process_id, task_id,
                    attempt_number, lease_id, fencing_token, state,
                    normalized_result, normalized_error
             FROM execution_attempt_t
             WHERE origin_service_id = $1
               AND state IN ('SUCCEEDED', 'FAILED', 'CANCELLED', 'TIMED_OUT')
               AND retry_classification IS DISTINCT FROM 'safe'
               AND accepted_by_origin_ts IS NULL
             ORDER BY terminal_ts, execution_id LIMIT $2",
        )
        .bind(origin_service_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    pub async fn conditionally_accept_terminal_attempt(
        tx: &mut Transaction<'_, Postgres>,
        attempt: &TerminalAttempt,
    ) -> Result<bool, sqlx::Error> {
        let state = parse_attempt_state(&attempt.state)?;
        if state == AttemptState::Unknown {
            return Ok(false);
        }
        let accepted = sqlx::query(
            "UPDATE task_info_t SET accepted_attempt = $1, update_ts = CURRENT_TIMESTAMP
             WHERE host_id = $2 AND task_id = $3 AND accepted_attempt IS NULL
               AND execution_placement = 'runner' AND status_code = 'A'",
        )
        .bind(attempt.attempt_number)
        .bind(attempt.host_id)
        .bind(attempt.task_id)
        .execute(&mut **tx)
        .await?;
        if accepted.rows_affected() != 1 {
            return Ok(false);
        }
        let accepted_attempt = sqlx::query(
            "UPDATE execution_attempt_t SET accepted_by_origin_ts = CURRENT_TIMESTAMP,
                    updated_ts = CURRENT_TIMESTAMP
             WHERE host_id = $1 AND execution_id = $2 AND lease_id = $3
               AND fencing_token = $4 AND accepted_by_origin_ts IS NULL",
        )
        .bind(attempt.host_id)
        .bind(attempt.execution_id)
        .bind(attempt.lease_id)
        .bind(attempt.fencing_token)
        .execute(&mut **tx)
        .await?;
        if accepted_attempt.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "terminal attempt lost its lease/fencing acceptance race".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE runner_scheduling_request_t SET state = 'SATISFIED',
                    updated_ts = CURRENT_TIMESTAMP
             WHERE host_id = $1 AND request_id = $2
               AND state IN ('ATTEMPT_CREATED', 'LEASED')",
        )
        .bind(attempt.host_id)
        .bind(attempt.request_id)
        .execute(&mut **tx)
        .await?;
        Ok(true)
    }
}

fn parse_attempt_state(state: &str) -> Result<AttemptState, sqlx::Error> {
    match state {
        "SUCCEEDED" => Ok(AttemptState::Succeeded),
        "FAILED" => Ok(AttemptState::Failed),
        "CANCELLED" => Ok(AttemptState::Cancelled),
        "TIMED_OUT" => Ok(AttemptState::TimedOut),
        "UNKNOWN" => Ok(AttemptState::Unknown),
        other => Err(sqlx::Error::Protocol(format!(
            "unexpected terminal attempt state {other}"
        ))),
    }
}

pub async fn append_runtime_audit(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    origin_service_id: &str,
    origin_instance_id: &str,
    subject_id: Uuid,
    execution_id: Option<Uuid>,
    event_type: &str,
    actor: &str,
    redacted_payload: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO execution_runtime_audit_t (
            host_id, origin_kind, origin_service_id, origin_instance_id,
            subject_kind, subject_id, execution_id, actor, event_type,
            redacted_payload
         ) VALUES ($1, 'workflow', $2, $3, 'workflow-task', $4, $5, $6, $7, $8)",
    )
    .bind(host_id)
    .bind(origin_service_id)
    .bind(origin_instance_id)
    .bind(subject_id)
    .bind(execution_id)
    .bind(actor)
    .bind(event_type)
    .bind(redacted_payload)
    .execute(&mut **tx)
    .await?;
    Ok(())
}
