use crate::repositories::{WorkflowRepository, append_runtime_audit};
use serde_json::json;
use sqlx::PgPool;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Debug, sqlx::FromRow)]
struct ExpiredLease {
    host_id: Uuid,
    execution_id: Uuid,
    request_id: Uuid,
    origin_service_id: String,
    origin_instance_id: String,
    subject_id: Uuid,
    lease_id: Uuid,
    fencing_token: i64,
    lease_started_ts: Option<chrono::DateTime<chrono::Utc>>,
}

pub struct LeaseReaper {
    repository: WorkflowRepository,
}

impl LeaseReaper {
    pub fn new(pool: PgPool) -> Self {
        Self {
            repository: WorkflowRepository::new(pool),
        }
    }

    pub async fn run(&self) -> Result<(), sqlx::Error> {
        info!("Starting runner lease reaper");
        loop {
            if let Err(error) = self.run_once().await {
                error!("runner lease reaper pass failed: {error}");
                sleep(Duration::from_secs(2)).await;
            } else {
                sleep(Duration::from_secs(1)).await;
            }
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.repository.pool().begin().await?;
        let expired = sqlx::query_as::<_, ExpiredLease>(
            "SELECT host_id, execution_id, request_id,
                    origin_service_id, origin_instance_id, subject_id,
                    lease_id, fencing_token, lease_started_ts
             FROM execution_attempt_t
             WHERE state IN ('CREATED', 'LEASED', 'STARTED')
               AND lease_deadline_ts <= CURRENT_TIMESTAMP
             ORDER BY lease_deadline_ts, execution_id
             LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(expired) = expired else {
            tx.commit().await?;
            return Ok(false);
        };

        if expired.lease_started_ts.is_none() {
            let updated = sqlx::query(
                "UPDATE execution_attempt_t
                 SET state = 'CANCELLED', terminal_ts = CURRENT_TIMESTAMP,
                     normalized_error = $1, retry_classification = 'safe',
                     accepted_by_origin_ts = CURRENT_TIMESTAMP,
                     updated_ts = CURRENT_TIMESTAMP
                 WHERE host_id = $2 AND execution_id = $3 AND lease_id = $4
                   AND fencing_token = $5 AND state IN ('CREATED', 'LEASED')",
            )
            .bind(json!({"class": "lease_expired_before_start"}))
            .bind(expired.host_id)
            .bind(expired.execution_id)
            .bind(expired.lease_id)
            .bind(expired.fencing_token)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 1 {
                sqlx::query(
                    "UPDATE runner_scheduling_request_t
                     SET state = 'PENDING_CAPACITY', selected_runner_session_id = NULL,
                         selected_backend_id = NULL, reservation_token_hash = NULL,
                         reservation_expires_ts = NULL, retry_count = retry_count + 1,
                         not_before_ts = CURRENT_TIMESTAMP
                             + LEAST(INTERVAL '60 seconds',
                                     (1 << LEAST(retry_count, 6)) * INTERVAL '1 second'),
                         diagnostic_reason = 'lease_expired_before_start',
                         updated_ts = CURRENT_TIMESTAMP
                     WHERE host_id = $1 AND request_id = $2
                       AND state IN ('ATTEMPT_CREATED', 'LEASED')",
                )
                .bind(expired.host_id)
                .bind(expired.request_id)
                .execute(&mut *tx)
                .await?;
            }
            info!(
                execution_id = %expired.execution_id,
                "expired unstarted lease returned to bounded capacity retry"
            );
        } else {
            let updated = sqlx::query(
                "UPDATE execution_attempt_t
                 SET state = 'UNKNOWN', terminal_ts = CURRENT_TIMESTAMP,
                     normalized_error = $1, retry_classification = 'inspect-required',
                     updated_ts = CURRENT_TIMESTAMP
                 WHERE host_id = $2 AND execution_id = $3 AND lease_id = $4
                   AND fencing_token = $5 AND state = 'STARTED'",
            )
            .bind(json!({"class": "lease_expired_after_start", "requiresInspection": true}))
            .bind(expired.host_id)
            .bind(expired.execution_id)
            .bind(expired.lease_id)
            .bind(expired.fencing_token)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 1 {
                warn!(
                    execution_id = %expired.execution_id,
                    "started lease expired with UNKNOWN outcome; automatic retry is blocked"
                );
            }
        }

        append_runtime_audit(
            &mut tx,
            expired.host_id,
            &expired.origin_service_id,
            &expired.origin_instance_id,
            expired.subject_id,
            Some(expired.execution_id),
            if expired.lease_started_ts.is_some() {
                "LEASE_EXPIRED_UNKNOWN"
            } else {
                "LEASE_EXPIRED_BEFORE_START"
            },
            "light-workflow-lease-reaper",
            &json!({
                "leaseId": expired.lease_id,
                "fencingToken": expired.fencing_token
            }),
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}
