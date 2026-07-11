use async_trait::async_trait;
use chrono::{Duration, Utc};
use serde_json::json;
use sqlx::{PgPool, Row};
use thiserror::Error;
use uuid::Uuid;

#[async_trait]
pub trait ArtifactObjectStore: Send + Sync {
    async fn delete(&self, reference: &str) -> Result<(), ArtifactStoreError>;
    async fn exists(&self, reference: &str) -> Result<bool, ArtifactStoreError>;
}

#[derive(Debug, Error)]
#[error("artifact object store operation failed: {message}")]
pub struct ArtifactStoreError {
    pub message: String,
}

pub struct ArtifactRetentionReconciler<S> {
    pool: PgPool,
    store: S,
    batch_size: i64,
}

impl<S: ArtifactObjectStore> ArtifactRetentionReconciler<S> {
    pub fn new(pool: PgPool, store: S, batch_size: i64) -> Self {
        Self {
            pool,
            store,
            batch_size: batch_size.clamp(1, 500),
        }
    }

    pub async fn reconcile_once(&self) -> Result<u64, sqlx::Error> {
        let mut claimed = Vec::new();
        let mut tx = self.pool.begin().await?;
        let rows = sqlx::query(
            "SELECT host_id,artifact_id,storage_reference,deletion_attempt
             FROM workflow_artifact_t
             WHERE legal_hold=FALSE
               AND ((deletion_state='RETAINED' AND retain_until_ts<=now())
                 OR (deletion_state IN ('DELETE_PENDING','DELETE_FAILED') AND COALESCE(deletion_next_retry_ts,now())<=now()))
             ORDER BY retain_until_ts,artifact_id FOR UPDATE SKIP LOCKED LIMIT $1"
        ).bind(self.batch_size).fetch_all(&mut *tx).await?;
        for row in rows {
            let host_id: Uuid = row.try_get("host_id")?;
            let artifact_id: Uuid = row.try_get("artifact_id")?;
            sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETING',deletion_attempt=deletion_attempt+1,updated_ts=now() WHERE host_id=$1 AND artifact_id=$2")
                .bind(host_id).bind(artifact_id).execute(&mut *tx).await?;
            claimed.push((
                host_id,
                artifact_id,
                row.try_get::<String, _>("storage_reference")?,
                row.try_get::<i32, _>("deletion_attempt")? + 1,
            ));
        }
        tx.commit().await?;
        for (host_id, artifact_id, reference, attempt) in &claimed {
            let outcome = match self.store.delete(reference).await {
                Ok(()) => match self.store.exists(reference).await {
                    Ok(false) => Ok(()),
                    Ok(true) => Err("object still exists after delete".into()),
                    Err(error) => Err(error.to_string()),
                },
                Err(error) => Err(error.to_string()),
            };
            match outcome {
                Ok(()) => {
                    sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETED',deleted_ts=now(),deletion_next_retry_ts=NULL,deletion_evidence=$3,updated_ts=now() WHERE host_id=$1 AND artifact_id=$2 AND deletion_state='DELETING'").bind(host_id).bind(artifact_id).bind(json!({"verifiedAbsent":true,"reference":reference})).execute(&self.pool).await?;
                }
                Err(error) => {
                    let backoff = Duration::seconds((1_i64 << (*attempt).min(12)).min(3600));
                    sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETE_FAILED',deletion_next_retry_ts=$3,deletion_evidence=$4,updated_ts=now() WHERE host_id=$1 AND artifact_id=$2 AND deletion_state='DELETING'").bind(host_id).bind(artifact_id).bind(Utc::now()+backoff).bind(json!({"error":error,"attempt":attempt})).execute(&self.pool).await?;
                }
            }
        }
        Ok(claimed.len() as u64)
    }

    pub async fn mark_process_deleted(
        pool: &PgPool,
        host_id: Uuid,
        process_id: Uuid,
        event_id: &str,
    ) -> Result<u64, sqlx::Error> {
        let result=sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETE_PENDING',deletion_next_retry_ts=now(),deletion_evidence=COALESCE(deletion_evidence,'{}'::jsonb)||jsonb_build_object('processDeletedEvent',$3),updated_ts=now() WHERE host_id=$1 AND process_id=$2 AND legal_hold=FALSE AND deletion_state='RETAINED'")
            .bind(host_id).bind(process_id).bind(event_id).execute(pool).await?;
        Ok(result.rows_affected())
    }
}
