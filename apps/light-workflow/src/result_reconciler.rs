use crate::executor::TaskExecutor;
use crate::repositories::WorkflowRepository;
use sqlx::{PgPool, postgres::PgListener};
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info};

pub struct ResultReconciler {
    pool: PgPool,
    repository: WorkflowRepository,
    executor: Arc<TaskExecutor>,
    origin_service_id: String,
}

impl ResultReconciler {
    pub fn new(
        pool: PgPool,
        executor: Arc<TaskExecutor>,
        origin_service_id: String,
        _origin_instance_id: String,
    ) -> Self {
        Self {
            repository: WorkflowRepository::new(pool.clone()),
            pool,
            executor,
            origin_service_id,
        }
    }

    pub async fn run(&self) -> Result<(), sqlx::Error> {
        info!("Starting execution result reconciler");
        loop {
            match self.listen_and_reconcile().await {
                Ok(()) => {
                    return Err(sqlx::Error::Protocol(
                        "execution result listener exited unexpectedly".to_string(),
                    ));
                }
                Err(error) => {
                    error!("execution result listener failed: {error}; reconnecting");
                    sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }

    async fn listen_and_reconcile(&self) -> Result<(), sqlx::Error> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("execution_result_ready_v1").await?;
        // LISTEN is established before catch-up, closing the commit/subscribe gap.
        self.run_once().await?;
        loop {
            match timeout(Duration::from_secs(1), listener.recv()).await {
                Ok(Ok(notification)) => {
                    debug!(
                        payload_bytes = notification.payload().len(),
                        "execution result wakeup received"
                    );
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => {}
            }
            self.run_once().await?;
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        let attempts = self
            .repository
            .pending_terminal_attempts(&self.origin_service_id, 32)
            .await?;
        let mut transitioned = false;
        for attempt in attempts {
            let mut tx = self.pool.begin().await?;
            match self
                .executor
                .reconcile_runner_attempt(&mut tx, &attempt)
                .await
            {
                Ok(true) => {
                    tx.commit().await?;
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
