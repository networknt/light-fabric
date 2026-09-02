//! Durable task facade for external business agents and remote A2A servers.

use a2a_core::{
    A2aError, ArtifactDescriptor, ArtifactVisibility, AuthorizedInvocation, Direction,
    TaskSnapshot, TaskState, canonical_projection_digest,
};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "a2a_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_a2a_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";
pub const MIGRATION_ID: &str = "0001_external_a2a_durability";
pub const PHASE3_MIGRATION_ID: &str = "0002_backend_skill_correlation";
pub const PHASE6_MIGRATION_ID: &str = "0003_governed_push_delivery";
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/a2a-postgres/0001_external_a2a_durability.sql");
pub const PHASE3_MIGRATION_SQL: &str =
    include_str!("../migrations/a2a-postgres/0002_backend_skill_correlation.sql");
pub const PHASE6_MIGRATION_SQL: &str =
    include_str!("../migrations/a2a-postgres/0003_governed_push_delivery.sql");
pub const AUTHORITY_TABLES: &[&str] = &[
    "a2a_context_t",
    "a2a_task_t",
    "a2a_message_idempotency_t",
    "a2a_backend_correlation_t",
    "a2a_callback_t",
    "a2a_artifact_t",
    "a2a_task_event_t",
    "a2a_audit_outbox_t",
    "a2a_delegation_replay_t",
    "a2a_push_config_t",
    "a2a_push_delivery_t",
];

#[derive(Debug, Clone)]
pub struct ExpectedBinding<'a> {
    pub binding_id: Uuid,
    pub binding_digest: &'a str,
    pub host_id: Uuid,
    pub environment: &'a str,
    pub server_host: &'a str,
    pub port: u16,
    pub tls_mode: &'a str,
    pub expected_database: &'a str,
    pub minimum_schema_generation: i64,
}

#[derive(Debug, Clone)]
pub struct TaskAdmission {
    pub task_id: Uuid,
    pub context_id: Uuid,
    pub invocation: AuthorizedInvocation,
}

#[derive(Debug, Clone)]
pub struct TaskAccess<'a> {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub principal_subject: &'a str,
    pub caller_agent_ref: &'a str,
    pub target_agent_ref: &'a str,
    pub binding_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct TaskScope<'a> {
    pub host_id: Uuid,
    pub principal_subject: &'a str,
    pub caller_agent_ref: &'a str,
    pub target_agent_ref: &'a str,
    pub binding_id: Uuid,
    pub context_id: Option<Uuid>,
    pub maximum_results: i64,
}

#[derive(Debug, Clone)]
pub struct ArtifactMetadata<'a> {
    pub artifact_id: Uuid,
    pub logical_name: &'a str,
    pub media_type: &'a str,
    pub size_bytes: i64,
    pub content_digest: &'a str,
    pub object_reference: &'a str,
    pub visibility: &'a str,
    pub retain_until: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct OwnedArtifact {
    pub artifact_id: Uuid,
    pub task_id: Uuid,
    pub content_digest: String,
    pub object_reference: String,
    pub retain_until: DateTime<Utc>,
    pub legal_hold: bool,
    pub deletion_state: String,
}

#[derive(Debug, Clone)]
pub struct ExpiredArtifactCandidate {
    pub artifact_id: Uuid,
    pub task_id: Uuid,
    pub principal_subject: String,
    pub caller_agent_ref: String,
    pub target_agent_ref: String,
    pub binding_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct BackendTaskBinding {
    pub context_id: Uuid,
    pub idempotency_key: String,
    pub backend_kind: String,
    pub backend_binding_id: Uuid,
    pub backend_operation_id: String,
    pub selected_skill_id: Option<String>,
    pub remote_task_id: Option<String>,
    pub remote_context_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PushConfig {
    pub config_id: Uuid,
    pub task_id: Uuid,
    pub callback_registration_id: Uuid,
    pub callback_url_digest: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct PushDelivery {
    pub delivery_id: Uuid,
    pub config_id: Uuid,
    pub task_id: Uuid,
    pub callback_registration_id: Uuid,
    pub binding_id: Uuid,
    pub delivery_nonce: Uuid,
    pub payload: Value,
    pub payload_digest: String,
    pub attempt: i64,
    pub maximum_attempts: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    A2a(#[from] A2aError),
    #[error("a2a-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Runtime(#[from] operational_store::runtime::RuntimeValidationError),
    #[error("a2a-store scope validation failed: {0}")]
    Scope(String),
}

#[derive(Clone)]
pub struct Repository {
    pool: PgPool,
}

impl Repository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn expired_artifacts(
        &self,
        host_id: Uuid,
        now: DateTime<Utc>,
        maximum_results: i64,
    ) -> Result<Vec<ExpiredArtifactCandidate>, StoreError> {
        if !(1..=100).contains(&maximum_results) {
            return Err(StoreError::Scope(
                "invalid artifact retention batch size".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT a.artifact_id,a.task_id,t.principal_subject,t.caller_agent_ref,
                    t.target_agent_ref,t.binding_id
               FROM a2a_artifact_t a
               JOIN a2a_task_t t ON t.host_id=a.host_id AND t.task_id=a.task_id
              WHERE a.host_id=$1 AND a.retain_until_ts<=$2 AND NOT a.legal_hold
                AND a.deletion_state='RETAINED'
              ORDER BY a.retain_until_ts,a.artifact_id LIMIT $3",
        )
        .bind(host_id)
        .bind(now)
        .bind(maximum_results)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ExpiredArtifactCandidate {
                    artifact_id: row.try_get("artifact_id")?,
                    task_id: row.try_get("task_id")?,
                    principal_subject: row.try_get("principal_subject")?,
                    caller_agent_ref: row.try_get("caller_agent_ref")?,
                    target_agent_ref: row.try_get("target_agent_ref")?,
                    binding_id: row.try_get("binding_id")?,
                })
            })
            .collect()
    }

    pub async fn admit(&self, admission: &TaskAdmission) -> Result<TaskSnapshot, StoreError> {
        admission.invocation.validate("light-a2a", Utc::now())?;
        let mut tx = self.pool.begin().await?;
        if let Some(row) = sqlx::query(
            "SELECT request_digest,task_id FROM a2a_message_idempotency_t
              WHERE host_id=$1 AND binding_id=$2 AND direction=$3 AND idempotency_key=$4 FOR UPDATE",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.invocation.binding_id)
        .bind(admission.invocation.direction.as_str())
        .bind(&admission.invocation.idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        {
            let digest: String = row.try_get("request_digest")?;
            if digest != admission.invocation.request_digest {
                return Err(A2aError::Replay.into());
            }
            let task_id: Uuid = row.try_get("task_id")?;
            sqlx::query(
                "UPDATE a2a_message_idempotency_t SET replay_count=replay_count+1,last_replay_ts=now()
                  WHERE host_id=$1 AND binding_id=$2 AND direction=$3 AND idempotency_key=$4",
            )
            .bind(admission.invocation.host_id)
            .bind(admission.invocation.binding_id)
            .bind(admission.invocation.direction.as_str())
            .bind(&admission.invocation.idempotency_key)
            .execute(&mut *tx)
            .await?;
            let snapshot = load_task(&mut tx, admission.invocation.host_id, task_id).await?;
            tx.commit().await?;
            return Ok(snapshot);
        }

        sqlx::query(
            "INSERT INTO a2a_context_t(host_id,context_id,public_context_id,principal_subject,
               caller_agent_ref,target_agent_ref,binding_id,publication_id,policy_digest,audience,expires_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.context_id)
        .bind(admission.context_id.to_string())
        .bind(&admission.invocation.principal_subject)
        .bind(&admission.invocation.caller_agent_ref)
        .bind(&admission.invocation.target_agent_ref)
        .bind(admission.invocation.binding_id)
        .bind(admission.invocation.publication_id)
        .bind(&admission.invocation.policy_digest)
        .bind(&admission.invocation.audience)
        .bind(admission.invocation.expires_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO a2a_task_t(host_id,task_id,public_task_id,context_id,direction,
               caller_agent_ref,target_agent_ref,binding_id,publication_id,principal_subject,
               policy_digest,idempotency_key,request_digest)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.task_id)
        .bind(admission.task_id.to_string())
        .bind(admission.context_id)
        .bind(admission.invocation.direction.as_str())
        .bind(&admission.invocation.caller_agent_ref)
        .bind(&admission.invocation.target_agent_ref)
        .bind(admission.invocation.binding_id)
        .bind(admission.invocation.publication_id)
        .bind(&admission.invocation.principal_subject)
        .bind(&admission.invocation.policy_digest)
        .bind(&admission.invocation.idempotency_key)
        .bind(&admission.invocation.request_digest)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO a2a_message_idempotency_t(host_id,binding_id,direction,idempotency_key,
               request_digest,task_id) VALUES($1,$2,$3,$4,$5,$6)",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.invocation.binding_id)
        .bind(admission.invocation.direction.as_str())
        .bind(&admission.invocation.idempotency_key)
        .bind(&admission.invocation.request_digest)
        .bind(admission.task_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            admission.invocation.host_id,
            admission.task_id,
            "TASK_SUBMITTED",
            json!({"direction": admission.invocation.direction.as_str()}),
        )
        .await?;
        append_audit(
            &mut tx,
            admission.invocation.host_id,
            admission.task_id,
            "a2a.task.submitted",
        )
        .await?;
        let snapshot = load_task(&mut tx, admission.invocation.host_id, admission.task_id).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub async fn get(&self, access: &TaskAccess<'_>) -> Result<TaskSnapshot, StoreError> {
        let row = self.owned_task(access, false).await?;
        let mut snapshot = snapshot_from_row(&row)?;
        snapshot.artifacts = load_artifacts(&self.pool, access.host_id, access.task_id).await?;
        Ok(snapshot)
    }

    pub async fn list(&self, scope: &TaskScope<'_>) -> Result<Vec<TaskSnapshot>, StoreError> {
        if !(1..=100).contains(&scope.maximum_results) {
            return Err(StoreError::Scope(
                "task list maximumResults must be between 1 and 100".into(),
            ));
        }
        let rows = sqlx::query(
            "SELECT task_id,context_id,state,direction,target_agent_ref,result,error
               FROM a2a_task_t
              WHERE host_id=$1 AND principal_subject=$2 AND caller_agent_ref=$3
                AND target_agent_ref=$4 AND binding_id=$5
                AND ($6::uuid IS NULL OR context_id=$6)
              ORDER BY created_ts DESC,task_id DESC LIMIT $7",
        )
        .bind(scope.host_id)
        .bind(scope.principal_subject)
        .bind(scope.caller_agent_ref)
        .bind(scope.target_agent_ref)
        .bind(scope.binding_id)
        .bind(scope.context_id)
        .bind(scope.maximum_results)
        .fetch_all(&self.pool)
        .await?;
        let mut snapshots = Vec::with_capacity(rows.len());
        for row in rows {
            let task_id: Uuid = row.try_get("task_id")?;
            let mut snapshot = snapshot_from_row(&row)?;
            snapshot.artifacts = load_artifacts(&self.pool, scope.host_id, task_id).await?;
            snapshots.push(snapshot);
        }
        Ok(snapshots)
    }

    pub async fn cancel(&self, access: &TaskAccess<'_>) -> Result<TaskSnapshot, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = owned_task_in(&mut tx, access, true).await?;
        let state = parse_state(row.try_get("state")?)?;
        if state.terminal() {
            return Err(A2aError::NotCancellable.into());
        }
        sqlx::query(
            "UPDATE a2a_task_t SET state='CANCELED',cancel_requested_ts=now(),terminal_ts=now(),
               aggregate_version=aggregate_version+1,updated_ts=now()
             WHERE host_id=$1 AND task_id=$2",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            access.host_id,
            access.task_id,
            "TASK_CANCELED",
            json!({}),
        )
        .await?;
        append_audit(&mut tx, access.host_id, access.task_id, "a2a.task.canceled").await?;
        let snapshot = load_task(&mut tx, access.host_id, access.task_id).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub async fn bind_backend(
        &self,
        access: &TaskAccess<'_>,
        backend_kind: &str,
        backend_binding_id: Uuid,
        correlation_id: &str,
        selected_skill_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        sqlx::query(
            "INSERT INTO a2a_backend_correlation_t(host_id,task_id,backend_kind,backend_binding_id,
               opaque_correlation_id,selected_skill_id) VALUES($1,$2,$3,$4,$5,$6)
             ON CONFLICT(host_id,task_id) DO UPDATE SET
               opaque_correlation_id=EXCLUDED.opaque_correlation_id,
               selected_skill_id=COALESCE(EXCLUDED.selected_skill_id,a2a_backend_correlation_t.selected_skill_id),
               updated_ts=now()",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(backend_kind)
        .bind(backend_binding_id)
        .bind(correlation_id)
        .bind(selected_skill_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn bind_remote_task(
        &self,
        access: &TaskAccess<'_>,
        backend_binding_id: Uuid,
        remote_task_id: &str,
        remote_context_id: Option<&str>,
        selected_skill_id: Option<&str>,
    ) -> Result<(), StoreError> {
        if remote_task_id.is_empty()
            || remote_task_id.len() > 512
            || remote_context_id.is_some_and(|value| value.is_empty() || value.len() > 512)
        {
            return Err(StoreError::Scope(
                "remote A2A task or context identity is invalid".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        owned_task_in(&mut tx, access, true).await?;
        sqlx::query(
            "UPDATE a2a_task_t SET remote_task_id=$3,remote_context_id=$4,updated_ts=now()
              WHERE host_id=$1 AND task_id=$2",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(remote_task_id)
        .bind(remote_context_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO a2a_backend_correlation_t(host_id,task_id,backend_kind,backend_binding_id,
               opaque_correlation_id,selected_skill_id) VALUES($1,$2,'REMOTE_A2A',$3,$4,$5)
             ON CONFLICT(host_id,task_id) DO UPDATE SET
               backend_kind='REMOTE_A2A',backend_binding_id=EXCLUDED.backend_binding_id,
               opaque_correlation_id=EXCLUDED.opaque_correlation_id,
               selected_skill_id=COALESCE(EXCLUDED.selected_skill_id,a2a_backend_correlation_t.selected_skill_id),
               updated_ts=now()",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(backend_binding_id)
        .bind(remote_task_id)
        .bind(selected_skill_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            access.host_id,
            access.task_id,
            "REMOTE_TASK_BOUND",
            json!({"backendBindingId": backend_binding_id}),
        )
        .await?;
        append_audit(
            &mut tx,
            access.host_id,
            access.task_id,
            "a2a.remote-task.bound",
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn backend_task_binding(
        &self,
        access: &TaskAccess<'_>,
    ) -> Result<BackendTaskBinding, StoreError> {
        self.owned_task(access, false).await?;
        let row = sqlx::query(
            "SELECT t.context_id,t.idempotency_key,t.remote_task_id,t.remote_context_id,
                    c.backend_kind,c.backend_binding_id,c.opaque_correlation_id,c.selected_skill_id
               FROM a2a_task_t t
               JOIN a2a_backend_correlation_t c
                 ON c.host_id=t.host_id AND c.task_id=t.task_id
              WHERE t.host_id=$1 AND t.task_id=$2",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(A2aError::NotFound)?;
        Ok(BackendTaskBinding {
            context_id: row.try_get("context_id")?,
            idempotency_key: row.try_get("idempotency_key")?,
            backend_kind: row.try_get("backend_kind")?,
            backend_binding_id: row.try_get("backend_binding_id")?,
            backend_operation_id: row.try_get("opaque_correlation_id")?,
            selected_skill_id: row.try_get("selected_skill_id")?,
            remote_task_id: row.try_get("remote_task_id")?,
            remote_context_id: row.try_get("remote_context_id")?,
        })
    }

    pub async fn reconcile(
        &self,
        access: &TaskAccess<'_>,
        state: TaskState,
        result: Option<Value>,
        error: Option<Value>,
    ) -> Result<TaskSnapshot, StoreError> {
        let mut tx = self.pool.begin().await?;
        let current = owned_task_in(&mut tx, access, true).await?;
        let current_state = parse_state(current.try_get("state")?)?;
        if current_state.terminal() && current_state != state {
            return Err(A2aError::NotCancellable.into());
        }
        sqlx::query(
            "UPDATE a2a_task_t SET state=$3,result=$4,error=$5,
               terminal_ts=CASE WHEN $6 THEN COALESCE(terminal_ts,now()) ELSE NULL END,
               aggregate_version=aggregate_version+1,updated_ts=now()
             WHERE host_id=$1 AND task_id=$2",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(state.as_str())
        .bind(result)
        .bind(error)
        .bind(state.terminal())
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            access.host_id,
            access.task_id,
            "TASK_STATUS_CHANGED",
            json!({"state": state.as_str()}),
        )
        .await?;
        let snapshot = load_task(&mut tx, access.host_id, access.task_id).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub async fn schedule_callback(
        &self,
        access: &TaskAccess<'_>,
        callback_id: Uuid,
        callback_kind: &str,
        callback_reference: &str,
        callback_secret_ref: Option<&str>,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        if callback_kind.trim().is_empty()
            || callback_reference.trim().is_empty()
            || callback_reference.contains("://")
            || callback_secret_ref.is_some_and(|value| value.trim().is_empty())
        {
            return Err(A2aError::InvalidInvocation.into());
        }
        sqlx::query(
            "INSERT INTO a2a_callback_t(host_id,callback_id,task_id,callback_kind,
               callback_reference,callback_secret_ref)
             VALUES($1,$2,$3,$4,$5,$6)",
        )
        .bind(access.host_id)
        .bind(callback_id)
        .bind(access.task_id)
        .bind(callback_kind)
        .bind(callback_reference)
        .bind(callback_secret_ref)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn create_push_config(
        &self,
        access: &TaskAccess<'_>,
        config_id: Uuid,
        callback_registration_id: Uuid,
        callback_url_digest: &str,
    ) -> Result<PushConfig, StoreError> {
        self.owned_task(access, false).await?;
        if !callback_url_digest.starts_with("sha256:") || callback_url_digest.len() != 71 {
            return Err(A2aError::InvalidInvocation.into());
        }
        let row = sqlx::query(
            "INSERT INTO a2a_push_config_t(host_id,config_id,task_id,binding_id,
               principal_subject,callback_registration_id,callback_url_digest)
             VALUES($1,$2,$3,$4,$5,$6,$7)
             RETURNING config_id,task_id,callback_registration_id,callback_url_digest,created_ts",
        )
        .bind(access.host_id)
        .bind(config_id)
        .bind(access.task_id)
        .bind(access.binding_id)
        .bind(access.principal_subject)
        .bind(callback_registration_id)
        .bind(callback_url_digest)
        .fetch_one(&self.pool)
        .await?;
        push_config_from_row(&row)
    }

    pub async fn get_push_config(
        &self,
        access: &TaskAccess<'_>,
        config_id: Uuid,
    ) -> Result<PushConfig, StoreError> {
        self.owned_task(access, false).await?;
        let row = sqlx::query(
            "SELECT config_id,task_id,callback_registration_id,callback_url_digest,created_ts
               FROM a2a_push_config_t
              WHERE host_id=$1 AND task_id=$2 AND binding_id=$3
                AND principal_subject=$4 AND config_id=$5",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(access.binding_id)
        .bind(access.principal_subject)
        .bind(config_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(A2aError::NotFound)?;
        push_config_from_row(&row)
    }

    pub async fn list_push_configs(
        &self,
        access: &TaskAccess<'_>,
    ) -> Result<Vec<PushConfig>, StoreError> {
        self.owned_task(access, false).await?;
        let rows = sqlx::query(
            "SELECT config_id,task_id,callback_registration_id,callback_url_digest,created_ts
               FROM a2a_push_config_t
              WHERE host_id=$1 AND task_id=$2 AND binding_id=$3 AND principal_subject=$4
              ORDER BY created_ts,config_id LIMIT 100",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(access.binding_id)
        .bind(access.principal_subject)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(push_config_from_row).collect()
    }

    pub async fn delete_push_config(
        &self,
        access: &TaskAccess<'_>,
        config_id: Uuid,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        let deleted = sqlx::query(
            "DELETE FROM a2a_push_config_t
              WHERE host_id=$1 AND task_id=$2 AND binding_id=$3
                AND principal_subject=$4 AND config_id=$5",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(access.binding_id)
        .bind(access.principal_subject)
        .bind(config_id)
        .execute(&self.pool)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(A2aError::NotFound.into());
        }
        Ok(())
    }

    pub async fn enqueue_push_deliveries(
        &self,
        access: &TaskAccess<'_>,
        payload: &Value,
        maximum_attempts: i64,
    ) -> Result<u64, StoreError> {
        self.owned_task(access, false).await?;
        if maximum_attempts < 1 || maximum_attempts > 100 || !payload.is_object() {
            return Err(A2aError::InvalidInvocation.into());
        }
        let digest = canonical_projection_digest(payload)
            .map_err(|error| StoreError::Scope(error.to_string()))?;
        let result = sqlx::query(
            "INSERT INTO a2a_push_delivery_t(host_id,delivery_id,config_id,task_id,
               delivery_nonce,payload,payload_digest,maximum_attempts)
             SELECT host_id,gen_random_uuid(),config_id,task_id,gen_random_uuid(),$5,$6,$7
               FROM a2a_push_config_t
              WHERE host_id=$1 AND task_id=$2 AND binding_id=$3 AND principal_subject=$4",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(access.binding_id)
        .bind(access.principal_subject)
        .bind(payload)
        .bind(digest)
        .bind(maximum_attempts)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn claim_push_deliveries(
        &self,
        host_id: Uuid,
        worker_id: &str,
        maximum_results: i64,
        lease_seconds: i64,
    ) -> Result<Vec<PushDelivery>, StoreError> {
        if worker_id.trim().is_empty()
            || worker_id.len() > 256
            || !(1..=100).contains(&maximum_results)
            || !(1..=300).contains(&lease_seconds)
        {
            return Err(A2aError::InvalidInvocation.into());
        }
        let rows = sqlx::query(
            "WITH due AS (
               SELECT host_id,delivery_id FROM a2a_push_delivery_t
                WHERE host_id=$1 AND attempt<maximum_attempts
                  AND ((state='PENDING' AND next_attempt_ts<=now())
                    OR (state='DELIVERING' AND lease_until_ts<=now()))
                ORDER BY next_attempt_ts,delivery_id
                FOR UPDATE SKIP LOCKED LIMIT $2
             )
             UPDATE a2a_push_delivery_t d SET state='DELIVERING',attempt=d.attempt+1,
                    lease_owner=$3,lease_until_ts=now()+make_interval(secs=>$4),updated_ts=now()
               FROM due JOIN a2a_push_config_t c
                 ON c.host_id=due.host_id AND c.config_id=(
                   SELECT config_id FROM a2a_push_delivery_t
                    WHERE host_id=due.host_id AND delivery_id=due.delivery_id)
              WHERE d.host_id=due.host_id AND d.delivery_id=due.delivery_id
              RETURNING d.delivery_id,d.config_id,d.task_id,c.callback_registration_id,c.binding_id,
                        d.delivery_nonce,d.payload,d.payload_digest,d.attempt,d.maximum_attempts",
        )
        .bind(host_id)
        .bind(maximum_results)
        .bind(worker_id)
        .bind(lease_seconds)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(push_delivery_from_row).collect()
    }

    pub async fn complete_push_delivery(
        &self,
        host_id: Uuid,
        delivery_id: Uuid,
        worker_id: &str,
        http_status: u16,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            "UPDATE a2a_push_delivery_t SET state='DELIVERED',last_http_status=$4,
                    delivered_ts=now(),lease_owner=NULL,lease_until_ts=NULL,updated_ts=now()
              WHERE host_id=$1 AND delivery_id=$2 AND state='DELIVERING' AND lease_owner=$3",
        )
        .bind(host_id)
        .bind(delivery_id)
        .bind(worker_id)
        .bind(i32::from(http_status))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::Scope(
                "push delivery lease is not owned by this worker".into(),
            ));
        }
        Ok(())
    }

    pub async fn retry_push_delivery(
        &self,
        host_id: Uuid,
        delivery_id: Uuid,
        worker_id: &str,
        error_code: &str,
        delay_seconds: i64,
        http_status: Option<u16>,
    ) -> Result<(), StoreError> {
        if error_code.trim().is_empty()
            || error_code.len() > 128
            || !(1..=86400).contains(&delay_seconds)
        {
            return Err(A2aError::InvalidInvocation.into());
        }
        let result = sqlx::query(
            "UPDATE a2a_push_delivery_t SET
               state=CASE WHEN attempt>=maximum_attempts THEN 'DEAD_LETTER' ELSE 'PENDING' END,
               next_attempt_ts=now()+make_interval(secs=>$5),last_error_code=$4,
               last_http_status=$6,dead_letter_ts=CASE WHEN attempt>=maximum_attempts THEN now() ELSE NULL END,
               lease_owner=NULL,lease_until_ts=NULL,updated_ts=now()
              WHERE host_id=$1 AND delivery_id=$2 AND state='DELIVERING' AND lease_owner=$3",
        )
        .bind(host_id)
        .bind(delivery_id)
        .bind(worker_id)
        .bind(error_code)
        .bind(delay_seconds)
        .bind(http_status.map(i32::from))
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(StoreError::Scope(
                "push delivery lease is not owned by this worker".into(),
            ));
        }
        Ok(())
    }

    pub async fn add_artifact(
        &self,
        access: &TaskAccess<'_>,
        artifact: &ArtifactMetadata<'_>,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        if artifact.object_reference.contains("://")
            || !artifact.content_digest.starts_with("sha256:")
            || artifact.size_bytes < 0
        {
            return Err(A2aError::InvalidInvocation.into());
        }
        sqlx::query(
            "INSERT INTO a2a_artifact_t(host_id,artifact_id,task_id,logical_name,media_type,size_bytes,
               content_digest,object_reference,visibility,retain_until_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
        )
        .bind(access.host_id)
        .bind(artifact.artifact_id)
        .bind(access.task_id)
        .bind(artifact.logical_name)
        .bind(artifact.media_type)
        .bind(artifact.size_bytes)
        .bind(artifact.content_digest)
        .bind(artifact.object_reference)
        .bind(artifact.visibility)
        .bind(artifact.retain_until)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn owned_artifact(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
    ) -> Result<OwnedArtifact, StoreError> {
        self.owned_task(access, false).await?;
        let row = sqlx::query(
            "SELECT artifact_id,task_id,content_digest,object_reference,retain_until_ts,
                    legal_hold,deletion_state
               FROM a2a_artifact_t
              WHERE host_id=$1 AND task_id=$2 AND artifact_id=$3",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(artifact_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(A2aError::NotFound)?;
        Ok(OwnedArtifact {
            artifact_id: row.try_get("artifact_id")?,
            task_id: row.try_get("task_id")?,
            content_digest: row.try_get("content_digest")?,
            object_reference: row.try_get("object_reference")?,
            retain_until: row.try_get("retain_until_ts")?,
            legal_hold: row.try_get("legal_hold")?,
            deletion_state: row.try_get("deletion_state")?,
        })
    }

    pub async fn set_artifact_hold(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        held: bool,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        let changed = sqlx::query(
            "UPDATE a2a_artifact_t SET legal_hold=$4,updated_ts=now()
              WHERE host_id=$1 AND task_id=$2 AND artifact_id=$3
                AND deletion_state<>'DELETED'",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(artifact_id)
        .bind(held)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(A2aError::NotFound.into());
        }
        Ok(())
    }

    pub async fn begin_artifact_deletion(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<OwnedArtifact, StoreError> {
        self.owned_task(access, false).await?;
        let artifact = self.owned_artifact(access, artifact_id).await?;
        if artifact.legal_hold {
            return Err(StoreError::Scope("artifact is under legal hold".into()));
        }
        if artifact.retain_until > now {
            return Err(StoreError::Scope(
                "artifact retention period has not expired".into(),
            ));
        }
        if artifact.deletion_state != "DELETED" {
            sqlx::query(
                "UPDATE a2a_artifact_t SET deletion_state='DELETE_PENDING',updated_ts=now()
                  WHERE host_id=$1 AND task_id=$2 AND artifact_id=$3",
            )
            .bind(access.host_id)
            .bind(access.task_id)
            .bind(artifact_id)
            .execute(&self.pool)
            .await?;
        }
        Ok(artifact)
    }

    pub async fn complete_artifact_deletion(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        tombstone_digest: &str,
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        if tombstone_digest.len() != 71
            || !tombstone_digest.starts_with("sha256:")
            || !tombstone_digest[7..]
                .bytes()
                .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase())
        {
            return Err(A2aError::InvalidInvocation.into());
        }
        let changed = sqlx::query(
            "UPDATE a2a_artifact_t SET deletion_state='DELETED',
                    deletion_evidence=jsonb_build_object('verifiedAbsent',TRUE,'tombstoneDigest',$4),
                    updated_ts=now()
              WHERE host_id=$1 AND task_id=$2 AND artifact_id=$3
                AND deletion_state IN ('DELETE_PENDING','DELETING','DELETED')",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(artifact_id)
        .bind(tombstone_digest)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(A2aError::NotFound.into());
        }
        Ok(())
    }

    pub async fn consume_delegation(
        &self,
        host_id: Uuid,
        delegation_id: Uuid,
        request_digest: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let inserted = sqlx::query(
            "INSERT INTO a2a_delegation_replay_t(host_id,delegation_id,audience,request_digest,expires_ts)
             VALUES($1,$2,'light-a2a',$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(host_id)
        .bind(delegation_id)
        .bind(request_digest)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() != 1 {
            return Err(A2aError::Replay.into());
        }
        Ok(())
    }

    async fn owned_task(
        &self,
        access: &TaskAccess<'_>,
        lock: bool,
    ) -> Result<sqlx::postgres::PgRow, StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = owned_task_in(&mut tx, access, lock).await?;
        tx.commit().await?;
        Ok(row)
    }
}

async fn owned_task_in(
    tx: &mut Transaction<'_, Postgres>,
    access: &TaskAccess<'_>,
    lock: bool,
) -> Result<sqlx::postgres::PgRow, StoreError> {
    let suffix = if lock { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT task_id,context_id,state,direction,target_agent_ref,result,error FROM a2a_task_t
          WHERE host_id=$1 AND task_id=$2 AND principal_subject=$3 AND caller_agent_ref=$4
            AND target_agent_ref=$5 AND binding_id=$6{suffix}"
    );
    sqlx::query(&sql)
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(access.principal_subject)
        .bind(access.caller_agent_ref)
        .bind(access.target_agent_ref)
        .bind(access.binding_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| A2aError::WrongTaskOwner.into())
}

async fn load_task(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    task_id: Uuid,
) -> Result<TaskSnapshot, StoreError> {
    let row = sqlx::query(
        "SELECT task_id,context_id,state,direction,target_agent_ref,result,error
           FROM a2a_task_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(host_id)
    .bind(task_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(A2aError::NotFound)?;
    let mut snapshot = snapshot_from_row(&row)?;
    snapshot.artifacts = load_artifacts_in(tx, host_id, task_id).await?;
    Ok(snapshot)
}

async fn load_artifacts(
    pool: &PgPool,
    host_id: Uuid,
    task_id: Uuid,
) -> Result<Vec<ArtifactDescriptor>, StoreError> {
    let rows = sqlx::query(
        "SELECT artifact_id,logical_name,media_type,size_bytes,content_digest,retain_until_ts
           FROM a2a_artifact_t WHERE host_id=$1 AND task_id=$2 AND deletion_state='RETAINED'
           ORDER BY logical_name",
    )
    .bind(host_id)
    .bind(task_id)
    .fetch_all(pool)
    .await?;
    artifact_descriptors(rows)
}

async fn load_artifacts_in(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    task_id: Uuid,
) -> Result<Vec<ArtifactDescriptor>, StoreError> {
    let rows = sqlx::query(
        "SELECT artifact_id,logical_name,media_type,size_bytes,content_digest,retain_until_ts
           FROM a2a_artifact_t WHERE host_id=$1 AND task_id=$2 AND deletion_state='RETAINED'
           ORDER BY logical_name",
    )
    .bind(host_id)
    .bind(task_id)
    .fetch_all(&mut **tx)
    .await?;
    artifact_descriptors(rows)
}

fn artifact_descriptors(
    rows: Vec<sqlx::postgres::PgRow>,
) -> Result<Vec<ArtifactDescriptor>, StoreError> {
    rows.into_iter()
        .map(|row| {
            let content_digest: String = row.try_get("content_digest")?;
            Ok(ArtifactDescriptor {
                artifact_id: row.try_get("artifact_id")?,
                logical_name: row.try_get("logical_name")?,
                media_type: row.try_get("media_type")?,
                size_bytes: row.try_get::<i64, _>("size_bytes")? as u64,
                content_digest: content_digest.clone(),
                visibility: ArtifactVisibility::TaskOwner,
                retention_deadline: row.try_get("retain_until_ts")?,
                provenance_digest: content_digest,
            })
        })
        .collect::<Result<Vec<_>, sqlx::Error>>()
        .map_err(StoreError::from)
}

fn snapshot_from_row(row: &sqlx::postgres::PgRow) -> Result<TaskSnapshot, StoreError> {
    let direction: String = row.try_get("direction")?;
    Ok(TaskSnapshot {
        task_id: row.try_get("task_id")?,
        context_id: row.try_get("context_id")?,
        state: parse_state(row.try_get("state")?)?,
        direction: if direction == "INBOUND" {
            Direction::Inbound
        } else {
            Direction::Outbound
        },
        target_agent_ref: row.try_get("target_agent_ref")?,
        result: row.try_get("result")?,
        error: row.try_get("error")?,
        artifacts: Vec::new(),
    })
}

fn parse_state(value: String) -> Result<TaskState, StoreError> {
    match value.as_str() {
        "SUBMITTED" => Ok(TaskState::Submitted),
        "WORKING" => Ok(TaskState::Working),
        "INPUT_REQUIRED" => Ok(TaskState::InputRequired),
        "AUTH_REQUIRED" => Ok(TaskState::AuthRequired),
        "COMPLETED" => Ok(TaskState::Completed),
        "FAILED" => Ok(TaskState::Failed),
        "CANCELED" => Ok(TaskState::Canceled),
        "REJECTED" => Ok(TaskState::Rejected),
        _ => Err(StoreError::Scope(format!("unknown A2A task state {value}"))),
    }
}

fn push_config_from_row(row: &sqlx::postgres::PgRow) -> Result<PushConfig, StoreError> {
    Ok(PushConfig {
        config_id: row.try_get("config_id")?,
        task_id: row.try_get("task_id")?,
        callback_registration_id: row.try_get("callback_registration_id")?,
        callback_url_digest: row.try_get("callback_url_digest")?,
        created_at: row.try_get("created_ts")?,
    })
}

fn push_delivery_from_row(row: &sqlx::postgres::PgRow) -> Result<PushDelivery, StoreError> {
    Ok(PushDelivery {
        delivery_id: row.try_get("delivery_id")?,
        config_id: row.try_get("config_id")?,
        task_id: row.try_get("task_id")?,
        callback_registration_id: row.try_get("callback_registration_id")?,
        binding_id: row.try_get("binding_id")?,
        delivery_nonce: row.try_get("delivery_nonce")?,
        payload: row.try_get("payload")?,
        payload_digest: row.try_get("payload_digest")?,
        attempt: row.try_get("attempt")?,
        maximum_attempts: row.try_get("maximum_attempts")?,
    })
}

async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    task_id: Uuid,
    event_type: &str,
    payload: Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO a2a_task_event_t(host_id,task_id,sequence_no,event_type,event_payload)
         SELECT $1,$2,COALESCE(MAX(sequence_no),0)+1,$3,$4 FROM a2a_task_event_t
          WHERE host_id=$1 AND task_id=$2",
    )
    .bind(host_id)
    .bind(task_id)
    .bind(event_type)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn append_audit(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    task_id: Uuid,
    event_type: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO a2a_audit_outbox_t(host_id,event_id,task_id,event_type,correlation_id,redacted_payload)
         VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(host_id)
    .bind(Uuid::now_v7())
    .bind(task_id)
    .bind(event_type)
    .bind(task_id.to_string())
    .bind(json!({"taskId": task_id, "contentRedacted": true}))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub fn read_database_url(path: &Path, server_host: &str, port: u16, tls_mode: &str,
                         expected_database: &str) -> Result<String, StoreError> {
    Ok(operational_store::runtime::read_database_url(
        path,
        server_host,
        port,
        tls_mode,
        expected_database,
        "a2a_runtime",
    )?)
}

pub async fn validate(pool: &PgPool, expected: &ExpectedBinding<'_>) -> Result<(), StoreError> {
    operational_store::runtime::validate_binding(
        pool,
        &operational_store::runtime::ExpectedBinding {
            binding_id: expected.binding_id,
            binding_digest: expected.binding_digest,
            host_id: expected.host_id,
            environment: expected.environment,
            server_host: expected.server_host,
            port: expected.port,
            tls_mode: expected.tls_mode,
            expected_database: expected.expected_database,
            role_suffix: "a2a_runtime",
            minimum_schema_generation: expected.minimum_schema_generation,
        },
    )
    .await?;
    let identity = sqlx::query(
        "SELECT has_schema_privilege(current_user,'a2a_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<bool, _>("schema_create")? {
        return Err(StoreError::Scope(
            "A2A runtime role must not have CREATE authority".into(),
        ));
    }
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT required.table_name FROM unnest($1::text[]) AS required(table_name)
          WHERE to_regclass('a2a_ops.' || required.table_name) IS NULL",
    )
    .bind(AUTHORITY_TABLES)
    .fetch_all(pool)
    .await?;
    if !missing.is_empty() {
        return Err(StoreError::Scope(format!(
            "A2A schema is incomplete; missing {}",
            missing.join(",")
        )));
    }
    for migration_id in [MIGRATION_ID, PHASE3_MIGRATION_ID, PHASE6_MIGRATION_ID] {
        let migrated: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
              WHERE migration_owner='a2a-store' AND schema_name='a2a_ops' AND migration_id=$1)",
        )
        .bind(migration_id)
        .fetch_one(pool)
        .await?;
        if !migrated {
            return Err(StoreError::Scope(format!(
                "required a2a-store migration {migration_id} is missing"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_authority_is_bounded_and_has_no_payload_bytes() {
        assert_eq!(AUTHORITY_TABLES.len(), 11);
        for table in &AUTHORITY_TABLES[..9] {
            assert!(MIGRATION_SQL.contains(&format!("a2a_ops.{table}")));
        }
        assert!(!MIGRATION_SQL.contains(" BYTEA"));
        assert!(!MIGRATION_SQL.contains("REFERENCES public."));
        assert!(MIGRATION_SQL.contains("object_reference !~ '^(https?|file)://'"));
        assert!(PHASE3_MIGRATION_SQL.contains("selected_skill_id"));
        assert!(PHASE6_MIGRATION_SQL.contains("a2a_push_config_t"));
        assert!(PHASE6_MIGRATION_SQL.contains("a2a_push_delivery_t"));
    }
}
