use chrono::Utc;
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub struct ExecutionSessionReconciler {
    pool: PgPool,
    origin_service_id: String,
    origin_instance_id: String,
}

impl ExecutionSessionReconciler {
    pub fn new(pool: PgPool, origin_service_id: String, origin_instance_id: String) -> Self {
        Self {
            pool,
            origin_service_id,
            origin_instance_id,
        }
    }

    pub async fn reconcile_once(&self) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let rows=sqlx::query("SELECT host_id,execution_session_id,origin_session_id,state FROM execution_session_t WHERE state IN ('READY','IDLE','IDLE_APPROVAL_HOLD') AND (effective_expires_ts<=now() OR (state='IDLE_APPROVAL_HOLD' AND hold_until_ts<=now())) ORDER BY effective_expires_ts FOR UPDATE SKIP LOCKED LIMIT 100").fetch_all(&mut *tx).await?;
        for row in &rows {
            let host: Uuid = row.try_get("host_id")?;
            let session: Uuid = row.try_get("execution_session_id")?;
            let origin_session: Option<Uuid> = row.try_get("origin_session_id")?;
            sqlx::query("UPDATE execution_session_t SET state='CLEANUP_REQUESTED',cleanup_status='PENDING',session_version=session_version+1,session_fence=session_fence+1,updated_ts=now() WHERE host_id=$1 AND execution_session_id=$2").bind(host).bind(session).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,execution_session_id,origin_kind,origin_service_id,origin_instance_id,origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,cleanup_deadline_ts,state) VALUES($1,$2,$3,'workflow',$4,$5,$6,'workflow-task',$3,$7,'session-expired',$4,$8,'PENDING') ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
                .bind(host).bind(Uuid::now_v7()).bind(session).bind(&self.origin_service_id).bind(&self.origin_instance_id).bind(origin_session).bind(format!("session-expired:{session}")).bind(Utc::now()+chrono::Duration::minutes(5)).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(rows.len() as u64)
    }
}
