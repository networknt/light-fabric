use crate::command_template::resolve_run_shell_spec;
use crate::configuration::RunnerExecutionConfig;
use crate::repositories::{NewSchedulingRequest, WorkflowRepository};
use chrono::{Duration as ChronoDuration, Utc};
use execution_runner_protocol::SchedulingRequestId;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;
use workflow_policy::ResolvedExecutionPolicy;

#[derive(Debug, sqlx::FromRow)]
struct PendingRunnerTask {
    host_id: Uuid,
    task_id: Uuid,
    process_id: Uuid,
    priority: i32,
    policy_snapshot_id: Uuid,
    task_policy_digest: String,
    resolved_policy: Value,
    definition_snapshot: Value,
    wf_task_id: String,
}

pub struct RunnerScheduler {
    repository: WorkflowRepository,
    config: RunnerExecutionConfig,
}

impl RunnerScheduler {
    pub fn new(pool: PgPool, config: RunnerExecutionConfig) -> Self {
        Self {
            repository: WorkflowRepository::new(pool),
            config,
        }
    }

    pub async fn run(&self) -> Result<(), sqlx::Error> {
        info!("Starting runner scheduling loop");
        loop {
            if let Err(error) = self.run_once().await {
                error!("runner scheduling pass failed: {error}");
                sleep(Duration::from_secs(2)).await;
            } else {
                sleep(Duration::from_millis(250)).await;
            }
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        if !self.config.enabled {
            return Ok(false);
        }
        let requested = self.create_pending_request().await?;
        let attempted = self.consume_reservation().await?;
        Ok(requested || attempted)
    }

    async fn create_pending_request(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.repository.pool().begin().await?;
        let task = claim_unscheduled_runner_task(&mut tx).await?;
        let Some(task) = task else {
            tx.commit().await?;
            return Ok(false);
        };
        let policy = serde_json::from_value::<ResolvedExecutionPolicy>(task.resolved_policy)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        if policy.policy_digest != task.task_policy_digest {
            return Err(sqlx::Error::Protocol(format!(
                "task {} policy digest does not match immutable snapshot",
                task.task_id
            )));
        }
        let requirements = policy.requirements().ok_or_else(|| {
            sqlx::Error::Protocol(format!(
                "runner task {} resolved without runner requirements",
                task.task_id
            ))
        })?;
        let execution_spec = resolve_run_shell_spec(
            task.definition_snapshot,
            &task.wf_task_id,
            &self.config.command_templates,
        )
        .map_err(sqlx::Error::Protocol)?;
        let execution_spec = serde_json::to_value(execution_spec)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let fairness_key = format!("{}:{}", task.host_id, task.process_id);
        let request_id = WorkflowRepository::create_scheduling_request(
            &mut tx,
            &NewSchedulingRequest {
                host_id: task.host_id,
                request_id: SchedulingRequestId::new(),
                origin_service_id: &self.config.origin_service_id,
                origin_instance_id: &self.config.origin_instance_id,
                process_id: task.process_id,
                task_id: task.task_id,
                policy_snapshot_id: task.policy_snapshot_id,
                policy_digest: &task.task_policy_digest,
                requirements: &requirements,
                execution_spec: &execution_spec,
                fairness_key: &fairness_key,
                priority: task.priority,
            },
        )
        .await?;
        tx.commit().await?;
        info!(
            request_id = %request_id,
            task_id = %task.task_id,
            "created durable runner scheduling request"
        );
        Ok(true)
    }

    async fn consume_reservation(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.repository.pool().begin().await?;
        let request =
            WorkflowRepository::claim_reserved_request(&mut tx, &self.config.origin_service_id)
                .await?;
        let Some(request) = request else {
            tx.commit().await?;
            return Ok(false);
        };
        let token = request.reservation_token_hash.clone();
        let task_deadline: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT deadline_ts FROM task_info_t WHERE host_id = $1 AND task_id = $2",
        )
        .bind(request.host_id)
        .bind(request.task_id)
        .fetch_one(&mut *tx)
        .await?;
        let maximum_deadline = Utc::now() + ChronoDuration::minutes(5);
        let lease_deadline = task_deadline
            .map(|deadline| deadline.min(maximum_deadline))
            .unwrap_or(maximum_deadline);
        let attempt = WorkflowRepository::create_attempt_from_reservation(
            &mut tx,
            &request,
            &token,
            lease_deadline,
        )
        .await?;
        let Some((execution_id, lease_id, fencing_token)) = attempt else {
            warn!(request_id = %request.request_id, "reservation was no longer consumable");
            tx.rollback().await?;
            return Ok(false);
        };
        tx.commit().await?;
        info!(
            execution_id = %execution_id,
            lease_id = %lease_id,
            fencing_token,
            "created fenced execution attempt"
        );
        Ok(true)
    }
}

async fn claim_unscheduled_runner_task(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<Option<PendingRunnerTask>, sqlx::Error> {
    sqlx::query_as::<_, PendingRunnerTask>(
        "SELECT t.host_id, t.task_id, t.process_id, t.priority,
                p.policy_snapshot_id, t.task_policy_digest, p.resolved_policy,
                pi.definition_snapshot, t.wf_task_id
         FROM task_info_t t
         JOIN process_info_t pi
           ON pi.host_id = t.host_id AND pi.process_id = t.process_id
         JOIN workflow_execution_policy_t p
           ON p.host_id = t.host_id AND p.policy_digest = t.task_policy_digest
         WHERE t.active = TRUE AND t.status_code = 'A'
           AND t.execution_placement = 'runner'
           AND t.scheduling_request_id IS NULL
           AND t.accepted_attempt IS NULL
         ORDER BY t.priority DESC, t.started_ts, t.task_id
         LIMIT 1 FOR UPDATE OF t SKIP LOCKED",
    )
    .fetch_optional(&mut **tx)
    .await
}
