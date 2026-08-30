use crate::artifact_publish::promote_artifact_evidence;
use crate::artifact_store::DurableArtifactStore;
use crate::configuration::RunnerExecutionConfig;
use crate::executor::TaskExecutor;
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
}

impl ResultReconciler {
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
            if let Err(error) = self.run_once().await {
                error!("execution result reconciliation failed: {error}; retrying");
            }
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = sleep(Duration::from_secs(1)) => {}
            }
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        let attempts = self
            .execution
            .pending_results(32)
            .await
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let mut transitioned = false;
        for result in attempts {
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
            let normalized = attempt
                .normalized_result
                .clone()
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
                Ok(true) => {
                    tx.commit().await?;
                    self.execution
                        .acknowledge_result(attempt.execution_id, attempt.fencing_token)
                        .await
                        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                    transitioned = true;
                    info!(
                        execution_id = %attempt.execution_id,
                        task_id = %attempt.task_id,
                        "accepted one runner result into workflow state"
                    );
                }
                Ok(false) => tx.rollback().await?,
                Err(error) => {
                    tx.rollback().await?;
                    return Err(sqlx::Error::Protocol(error.to_string()));
                }
            }
        }
        Ok(transitioned)
    }
}
