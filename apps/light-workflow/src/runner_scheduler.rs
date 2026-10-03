use crate::command_template::resolve_run_shell_spec;
use crate::configuration::RunnerExecutionConfig;
use crate::executor::{TaskExecutor, expression_completion};
use crate::repositories::WorkflowRepository;
use execution_client::ExecutionClient;
use execution_runner_protocol::{SchedulingRequestSubmission, canonical_sha256};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};
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
    definition_snapshot: Option<Value>,
    definition_digest: Option<String>,
    wf_task_id: String,
    expression_profile: String,
}

pub struct RunnerScheduler {
    repository: WorkflowRepository,
    config: RunnerExecutionConfig,
    execution: ExecutionClient,
    executor: Option<Arc<TaskExecutor>>,
}

impl RunnerScheduler {
    pub fn new(
        pool: PgPool,
        config: RunnerExecutionConfig,
        bearer_token: &str,
    ) -> Result<Self, String> {
        let ca = config
            .execution_api_ca_cert_file
            .as_ref()
            .map(std::fs::read)
            .transpose()
            .map_err(|error| format!("cannot read execution API CA certificate: {error}"))?;
        let execution = ExecutionClient::new_with_bearer_token(
            &config.execution_api_url,
            bearer_token,
            Duration::from_secs(10),
            ca.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            repository: WorkflowRepository::new(pool),
            config,
            execution,
            executor: None,
        })
    }

    pub fn with_executor(mut self, executor: Arc<TaskExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }
    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), sqlx::Error> {
        info!("Starting runner scheduling loop");
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = self.run_once().await {
                error!("runner scheduling pass failed: {error}");
                tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_secs(2)) => {} }
            } else {
                tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_millis(250)) => {} }
            }
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        if !self.config.enabled {
            return Ok(false);
        }
        self.create_pending_request().await
    }

    async fn create_pending_request(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.repository.pool().begin().await?;
        let support = self
            .executor
            .as_ref()
            .map(|e| e.supported_profiles().clone())
            .unwrap_or_else(|| crate::profile_support::SupportedProfiles::from_evaluator(false));
        let task = claim_unscheduled_runner_task(&mut tx, &support).await?;
        let Some(task) = task else {
            tx.commit().await?;
            return Ok(false);
        };
        let disposition = if let Some(executor) = &self.executor {
            executor
                .w4_guard(&mut tx, task.host_id, task.process_id)
                .await?
        } else {
            support.check(
                &task.expression_profile,
                task.definition_snapshot.as_ref(),
                task.definition_digest.as_deref(),
            )
        };
        match disposition {
            expression_completion::ProfileDisposition::Deferred => {
                tx.rollback().await?;
                return Ok(false);
            }
            expression_completion::ProfileDisposition::Corrupt => {
                expression_completion::reject_step(
                    &mut tx,
                    task.host_id,
                    task.process_id,
                    task.task_id,
                    "/definition_snapshot/expression_profile",
                )
                .await?;
                tx.commit().await?;
                return Ok(true);
            }
            expression_completion::ProfileDisposition::V2 => {
                let parent =
                    crate::durable_timer::try_lock_parent(&mut tx, task.host_id, task.process_id)
                        .await?;
                let now: chrono::DateTime<chrono::Utc> =
                    sqlx::query_scalar("SELECT clock_timestamp()")
                        .fetch_one(&mut *tx)
                        .await?;
                if parent.blocked.is_some() || parent.deadline.is_some_and(|d| d <= now) {
                    tx.rollback().await?;
                    return Ok(false);
                }
                if !expression_completion::capture_step(
                    &mut tx,
                    task.host_id,
                    task.process_id,
                    task.task_id,
                )
                .await?
                {
                    expression_completion::reject_step(
                        &mut tx,
                        task.host_id,
                        task.process_id,
                        task.task_id,
                        "/task_input/stepSnapshot",
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(true);
                }
            }
            expression_completion::ProfileDisposition::Legacy => {}
        }
        let policy =
            serde_json::from_value::<ResolvedExecutionPolicy>(task.resolved_policy.clone())
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
        // Decode nullable persisted identity first so missing v2 snapshots reach
        // the explicit integrity outcome above rather than a row-decoding error.
        let snapshot = task.definition_snapshot.ok_or_else(|| {
            sqlx::Error::Protocol("WORKFLOW_RUNNER_DEFINITION_SNAPSHOT_REQUIRED".into())
        })?;
        let definition_digest = task.definition_digest.ok_or_else(|| {
            sqlx::Error::Protocol("WORKFLOW_RUNNER_DEFINITION_DIGEST_REQUIRED".into())
        })?;
        let execution_spec =
            resolve_run_shell_spec(snapshot, &task.wf_task_id, &self.config.command_templates)
                .map_err(sqlx::Error::Protocol)?;
        let execution_spec = serde_json::to_value(execution_spec)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let fairness_key = format!("{}:{}", task.host_id, task.process_id);
        tx.commit().await?;
        let request_id = task.task_id;
        let workflow_reference_digest = format!(
            "sha256:{}",
            canonical_sha256(&(
                task.host_id,
                task.process_id,
                task.task_id,
                definition_digest.as_str(),
                task.task_policy_digest.as_str(),
            ))
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
        );
        let submitted = self
            .execution
            .submit_request(&SchedulingRequestSubmission {
                request_id,
                idempotency_key: format!("workflow-task:{}", task.task_id),
                origin_kind: "workflow".to_string(),
                origin_instance_id: self.config.origin_instance_id.clone(),
                subject_kind: "workflow-task".to_string(),
                subject_id: task.task_id,
                process_id: Some(task.process_id),
                task_id: Some(task.task_id),
                agent_session_id: None,
                agent_turn_id: None,
                agent_action_id: None,
                policy_snapshot_id: task.policy_snapshot_id,
                policy_digest: task.task_policy_digest.clone(),
                normalized_requirements: serde_json::to_value(requirements)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?,
                execution_spec,
                resolved_policy: task.resolved_policy,
                definition_digest,
                fairness_key,
                priority: task.priority,
                workflow_reference_digest: Some(workflow_reference_digest.clone()),
                origin_reference_digest: workflow_reference_digest,
                approval_id: None,
                approval_evidence_digest: None,
                pinned_runner_id: None,
                pinned_backend_id: None,
                edge_binding_id: None,
                edge_binding_compatibility_digest: None,
                edge_binding_revocation_epoch: None,
                inputs: Vec::new(),
            })
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        if submitted != request_id {
            return Err(sqlx::Error::Protocol(
                "execution authority returned another scheduling request ID".to_string(),
            ));
        }
        let updated = sqlx::query(
            "UPDATE task_info_t SET scheduling_request_id=$1,update_ts=CURRENT_TIMESTAMP
             WHERE host_id=$2 AND task_id=$3 AND execution_placement='runner'
               AND scheduling_request_id IS NULL AND ($4::boolean=FALSE OR (status_code='A' AND active AND (deadline_ts IS NULL OR deadline_ts>clock_timestamp())
                 AND EXISTS (SELECT 1 FROM process_info_t p WHERE p.host_id=task_info_t.host_id AND p.process_id=task_info_t.process_id
                   AND p.active AND p.status_code IN ('A','W') AND p.expression_profile='cel-workflow-v2'
                   AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp()))
                 AND NOT EXISTS (SELECT 1 FROM workflow_invocation_t i WHERE i.host_id=task_info_t.host_id AND i.process_id=task_info_t.process_id
                   AND (i.cancel_requested_ts IS NOT NULL OR i.state NOT IN ('ACCEPTED','RUNNING','WAITING')
                     OR (NOT COALESCE(i.private_lifetime,FALSE) AND i.deadline_ts<=clock_timestamp())))))",
        )
        .bind(request_id)
        .bind(task.host_id)
        .bind(task.task_id)
        .bind(task.expression_profile=="cel-workflow-v2")
        .execute(self.repository.pool())
        .await?;
        if updated.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "workflow task lost its execution scheduling claim".to_string(),
            ));
        }
        info!(
            request_id = %request_id,
            task_id = %task.task_id,
            "created durable runner scheduling request"
        );
        Ok(true)
    }
}

async fn claim_unscheduled_runner_task(
    tx: &mut Transaction<'_, Postgres>,
    support: &crate::profile_support::SupportedProfiles,
) -> Result<Option<PendingRunnerTask>, sqlx::Error> {
    sqlx::query_as::<_, PendingRunnerTask>(&format!(
        "SELECT t.host_id, t.task_id, t.process_id, t.priority,
                p.policy_snapshot_id, t.task_policy_digest, p.resolved_policy,
                pi.definition_snapshot, pi.definition_digest, t.wf_task_id,pi.expression_profile
         FROM task_info_t t
         JOIN process_info_t pi
           ON pi.host_id = t.host_id AND pi.process_id = t.process_id
         JOIN workflow_execution_policy_t p
           ON p.host_id = t.host_id AND p.policy_digest = t.task_policy_digest
         WHERE t.active = TRUE AND t.status_code = 'A'
           AND t.execution_placement = 'runner'
           AND t.scheduling_request_id IS NULL
           AND t.accepted_attempt IS NULL AND {} AND {} AND (t.deadline_ts IS NULL OR t.deadline_ts>clock_timestamp())
         ORDER BY t.priority DESC, t.started_ts, t.task_id
         LIMIT 1 FOR UPDATE OF t SKIP LOCKED",
        crate::profile_support::eligible("pi", "$1"),crate::profile_support::live_selection("pi","t.is_compensation")
    ))
    .bind(support.profiles())
    .fetch_optional(&mut **tx)
    .await
}
