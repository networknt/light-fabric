//! Durable task facade for external business agents and remote A2A servers.

use a2a_core::{A2aError, AuthorizedInvocation, Direction, TaskSnapshot, TaskState};
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
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/a2a-postgres/0001_external_a2a_durability.sql");
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
];

#[derive(Debug, Clone)]
pub struct ExpectedBinding<'a> {
    pub binding_id: Uuid,
    pub binding_digest: &'a str,
    pub host_id: Uuid,
    pub environment: &'a str,
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

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    A2a(#[from] A2aError),
    #[error("a2a-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
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
        snapshot_from_row(&row)
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
    ) -> Result<(), StoreError> {
        self.owned_task(access, false).await?;
        sqlx::query(
            "INSERT INTO a2a_backend_correlation_t(host_id,task_id,backend_kind,backend_binding_id,
               opaque_correlation_id) VALUES($1,$2,$3,$4,$5)
             ON CONFLICT(host_id,task_id) DO UPDATE SET
               opaque_correlation_id=EXCLUDED.opaque_correlation_id,updated_ts=now()",
        )
        .bind(access.host_id)
        .bind(access.task_id)
        .bind(backend_kind)
        .bind(backend_binding_id)
        .bind(correlation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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
    snapshot_from_row(&row)
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

pub fn read_database_url(path: &Path) -> Result<String, StoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        StoreError::Scope(format!("cannot inspect A2A database URL file: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Scope(
            "A2A database URL path must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(StoreError::Scope(
                "A2A database URL file permissions are too broad".into(),
            ));
        }
    }
    let value = std::fs::read_to_string(path).map_err(|error| {
        StoreError::Scope(format!("cannot read A2A database URL file: {error}"))
    })?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty()
        || value.len() > 2048
        || value.contains(['\r', '\n'])
        || !value.starts_with("postgres://operations_a2a_runtime:")
        || !value.ends_with("/operations")
    {
        return Err(StoreError::Scope(
            "A2A database URL file does not match the redacted role/database contract".into(),
        ));
    }
    Ok(value.to_string())
}

pub async fn validate(pool: &PgPool, expected: &ExpectedBinding<'_>) -> Result<(), StoreError> {
    let identity = sqlx::query(
        "SELECT current_database() AS database_name,current_user AS role_name,
                has_database_privilege(current_user,current_database(),'CREATE') AS database_create,
                has_schema_privilege(current_user,'a2a_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<String, _>("database_name")? != EXPECTED_DATABASE
        || identity.try_get::<String, _>("role_name")? != EXPECTED_RUNTIME_ROLE
        || identity.try_get::<bool, _>("database_create")?
        || identity.try_get::<bool, _>("schema_create")?
    {
        return Err(StoreError::Scope(
            "A2A database identity or privilege mismatch".into(),
        ));
    }
    let binding = sqlx::query(
        "SELECT binding_id,binding_digest,host_id,environment,schema_contract_generation
           FROM operational_meta.operational_store_binding_t WHERE active",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::Scope("no active operational-store binding".into()))?;
    if binding.try_get::<Uuid, _>("binding_id")? != expected.binding_id
        || binding.try_get::<String, _>("binding_digest")? != expected.binding_digest
        || binding.try_get::<Uuid, _>("host_id")? != expected.host_id
        || binding.try_get::<String, _>("environment")? != expected.environment
        || binding.try_get::<i64, _>("schema_contract_generation")?
            < expected.minimum_schema_generation
    {
        return Err(StoreError::Scope(
            "active operational-store binding does not match the A2A projection".into(),
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
    let migrated: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='a2a-store' AND schema_name='a2a_ops' AND migration_id=$1)",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !migrated {
        return Err(StoreError::Scope(
            "a2a-store migration ledger entry is missing".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_authority_is_bounded_and_has_no_payload_bytes() {
        assert_eq!(AUTHORITY_TABLES.len(), 9);
        for table in AUTHORITY_TABLES {
            assert!(MIGRATION_SQL.contains(&format!("a2a_ops.{table}")));
        }
        assert!(!MIGRATION_SQL.contains(" BYTEA"));
        assert!(!MIGRATION_SQL.contains("REFERENCES public."));
        assert!(MIGRATION_SQL.contains("object_reference !~ '^(https?|file)://'"));
    }
}
