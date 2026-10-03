use crate::artifact_publish::promote_artifact_evidence;
use crate::artifact_store::DurableArtifactStore;
use crate::configuration::RunnerExecutionConfig;
use crate::executor::{RunnerReconciliation, TaskExecutor};
use crate::repositories::TerminalAttempt;
use execution_client::ExecutionClient;
use execution_runner_protocol::NormalizedExecutionResult;
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use tokio::time::sleep;
use tracing::{error, info};

pub struct ResultReconciler {
    pool: PgPool,
    execution: ExecutionClient,
    executor: Arc<TaskExecutor>,
    artifact_store: Option<DurableArtifactStore>,
    artifact_retention_days: i64,
    sweep: tokio::sync::Mutex<crate::result_sweep::Sweep>,
}

impl ResultReconciler {
    #[cfg(test)]
    pub(crate) fn for_test(
        pool: PgPool,
        executor: Arc<TaskExecutor>,
        execution: ExecutionClient,
    ) -> Self {
        Self {
            pool,
            executor,
            execution,
            artifact_store: None,
            artifact_retention_days: 1,
            sweep: tokio::sync::Mutex::new(Default::default()),
        }
    }
    pub fn new(
        pool: PgPool,
        executor: Arc<TaskExecutor>,
        runner: &RunnerExecutionConfig,
        bearer_token: &str,
        artifact_store: Option<DurableArtifactStore>,
        artifact_retention_days: i64,
    ) -> Result<Self, String> {
        let ca = runner
            .execution_api_ca_cert_file
            .as_ref()
            .map(std::fs::read)
            .transpose()
            .map_err(|error| format!("cannot read execution API CA certificate: {error}"))?;
        let execution = ExecutionClient::new_with_bearer_token(
            &runner.execution_api_url,
            bearer_token,
            Duration::from_secs(10),
            ca.as_deref(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            pool,
            execution,
            executor,
            artifact_store,
            artifact_retention_days: artifact_retention_days.clamp(1, 3650),
            sweep: tokio::sync::Mutex::new(Default::default()),
        })
    }

    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), sqlx::Error> {
        info!("Starting execution result reconciler");
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = self.run_poll(&shutdown).await {
                error!("execution result reconciliation failed: {error}; retrying");
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        self.run_poll(&tokio_util::sync::CancellationToken::new())
            .await
    }

    async fn run_poll(
        &self,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<bool, sqlx::Error> {
        use crate::result_sweep::PollFailure;
        let mut sweep = self.sweep.lock().await;
        sweep.poll(shutdown, |cursor| async move {
            self.execution.pending_results_page(32, cursor.as_deref()).await.map_err(|error| {
                if matches!(&error, execution_client::ClientError::Rejected {status, code}
                    if *status == reqwest::StatusCode::BAD_REQUEST && code == "EXECUTION_RESULT_CURSOR_REJECTED") {
                    PollFailure::CursorRejected
                } else { PollFailure::System(sqlx::Error::Protocol(error.to_string())) }
            })
        }, |result| async move {
            self.reconcile_item(result).await.map_err(|error| {
                let item = matches!(&error, sqlx::Error::Protocol(message)
                    if message == "workflow execution result has no process ID"
                    || message == "workflow execution result has no task ID"
                    || message.starts_with("invalid normalized runner result:")
                    || message == "WORKFLOW_RESULT_ITEM_ACK_REJECTED");
                if item { PollFailure::Item(error) } else { PollFailure::System(error) }
            })
        }).await
    }

    async fn reconcile_item(
        &self,
        result: execution_runner_protocol::ExecutionResultView,
    ) -> Result<bool, sqlx::Error> {
        let mut transitioned = false;
        let attempt = TerminalAttempt {
            host_id: result.host_id,
            execution_id: result.execution_id,
            request_id: result.request_id,
            process_id: result.process_id.ok_or_else(|| {
                sqlx::Error::Protocol("workflow execution result has no process ID".into())
            })?,
            task_id: result.task_id.ok_or_else(|| {
                sqlx::Error::Protocol("workflow execution result has no task ID".into())
            })?,
            attempt_number: result.attempt_number,
            lease_id: result.lease_id,
            fencing_token: result.fencing_token,
            state: result.state,
            normalized_result: result.normalized_result,
            normalized_error: result.normalized_error,
        };
        // Profile/availability is decided before result parsing or artifact effects.
        let mut gate = self.pool.begin().await?;
        if let Some(outcome) = self.executor.w4_runner_gate(&mut gate, &attempt).await? {
            if outcome == RunnerReconciliation::Failed {
                gate.commit().await?;
            } else {
                gate.rollback().await?;
            }
            if matches!(
                outcome,
                RunnerReconciliation::Failed | RunnerReconciliation::Replay
            ) {
                self.execution
                    .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                    .await
                    .map_err(ack_error)?;
                transitioned = true;
            }
            return Ok(transitioned);
        }
        gate.commit().await?;
        let normalized = (attempt.state == "SUCCEEDED")
            .then(|| attempt.normalized_result.clone())
            .flatten()
            .map(serde_json::from_value::<NormalizedExecutionResult>)
            .transpose()
            .map_err(|error| {
                sqlx::Error::Protocol(format!("invalid normalized runner result: {error}"))
            })?;
        if let Some(result) = &normalized {
            if !result.artifacts.is_empty() {
                let store = self.artifact_store.as_ref().ok_or_else(|| {
                    sqlx::Error::Protocol(
                        "runner returned artifacts but no object store is configured".into(),
                    )
                })?;
                let retain_until =
                    chrono::Utc::now() + chrono::Duration::days(self.artifact_retention_days);
                for artifact in &result.artifacts {
                    promote_artifact_evidence(
                        &self.pool,
                        store,
                        attempt.host_id,
                        attempt.execution_id,
                        attempt.process_id,
                        attempt.task_id,
                        &result.policy_digest,
                        retain_until,
                        artifact,
                    )
                    .await
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                }
            }
        }
        let mut tx = self.pool.begin().await?;
        match self
            .executor
            .reconcile_runner_attempt(&mut tx, &attempt)
            .await
        {
            Ok(RunnerReconciliation::Completed) => {
                crate::development_execution::reconcile_runner_result(&mut tx, &attempt)
                    .await
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                tx.commit().await?;
                self.execution
                    .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                    .await
                    .map_err(ack_error)?;
                transitioned = true;
                info!(
                    execution_id = %attempt.execution_id,
                    task_id = %attempt.task_id,
                    "accepted one runner result into workflow state"
                );
            }
            Ok(RunnerReconciliation::Failed) => {
                tx.commit().await?;
                self.execution
                    .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                    .await
                    .map_err(ack_error)?;
                transitioned = true;
            }
            Ok(RunnerReconciliation::Replay) => {
                tx.rollback().await?;
                self.execution
                    .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                    .await
                    .map_err(ack_error)?;
                transitioned = true;
            }
            Ok(RunnerReconciliation::Deferred) => {
                if self
                    .executor
                    .w4_guard(&mut tx, attempt.host_id, attempt.process_id)
                    .await?
                    != crate::executor::expression_completion::ProfileDisposition::Legacy
                {
                    tx.rollback().await?;
                    return Ok(transitioned);
                }
                let replay =
                    crate::development_execution::runner_result_already_recorded(&mut tx, &attempt)
                        .await
                        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                tx.rollback().await?;
                if replay {
                    self.execution
                        .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                        .await
                        .map_err(ack_error)?;
                    transitioned = true;
                }
            }
            Err(error) => {
                tx.rollback().await?;
                return Err(sqlx::Error::Protocol(error.to_string()));
            }
        }
        Ok(transitioned)
    }
}

fn ack_error(error: execution_client::ClientError) -> sqlx::Error {
    match error {
        execution_client::ClientError::Rejected { status, .. }
            if matches!(status.as_u16(), 400 | 404 | 409) =>
        {
            sqlx::Error::Protocol("WORKFLOW_RESULT_ITEM_ACK_REJECTED".into())
        }
        error => sqlx::Error::Protocol(error.to_string()),
    }
}
