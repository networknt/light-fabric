//! Agent and embedded-memory operational-store authority.
//!
//! `light-agent` has no Config Server database connection. It validates its
//! dedicated `operations_agent_runtime` pool against the immutable binding
//! projection before becoming ready or accepting work.

use a2a_core::{A2aError, AuthorizedInvocation, Direction, TaskSnapshot, TaskState};
use chrono::Utc;
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "agent_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_agent_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";
pub const MIGRATION_ID: &str = "0001_agent_and_embedded_memory";
pub const NATIVE_A2A_MIGRATION_ID: &str = "0002_native_a2a_aliases";
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        MIGRATION_ID,
        include_str!("../migrations/agent-postgres/0001_agent_and_embedded_memory.sql"),
    ),
    (
        NATIVE_A2A_MIGRATION_ID,
        include_str!("../migrations/agent-postgres/0002_native_a2a_aliases.sql"),
    ),
];

/// The 21 tables frozen in Phase 0 as the Agent/memory cutover wave.
pub const AUTHORITY_TABLES: &[&str] = &[
    "agent_action_attempt_t",
    "agent_approval_t",
    "agent_delegation_replay_t",
    "agent_fixed_action_t",
    "agent_job_t",
    "agent_memory_bank_t",
    "agent_memory_doc_t",
    "agent_memory_entity_t",
    "agent_memory_entity_cooccur_t",
    "agent_memory_link_t",
    "agent_memory_reflection_t",
    "agent_memory_unit_t",
    "agent_memory_unit_entity_t",
    "agent_policy_snapshot_t",
    "agent_quota_reservation_t",
    "agent_quota_usage_t",
    "agent_session_event_t",
    "agent_session_history_t",
    "agent_session_t",
    "agent_turn_materialization_t",
    "agent_turn_t",
];

/// Phase 2/3 Agent-owned support state that must follow the same connection.
pub const SUPPORT_TABLES: &[&str] = &[
    "agent_execution_outbox_t",
    "runtime_operational_scope_t",
    "operational_reference_evidence_t",
    "operational_reference_reconciliation_t",
];

pub const NATIVE_A2A_TABLES: &[&str] = &[
    "agent_a2a_context_alias_t",
    "agent_a2a_task_alias_t",
    "agent_a2a_artifact_t",
];

#[derive(Debug, Clone)]
pub struct ExpectedBinding<'a> {
    pub binding_id: Uuid,
    pub binding_digest: &'a str,
    pub host_id: Uuid,
    pub environment: &'a str,
    pub minimum_schema_generation: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("agent-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("agent-store scope validation failed: {0}")]
    Scope(String),
}

#[derive(Debug, Clone)]
pub struct NativeTaskAdmission {
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub task_id: Uuid,
    pub context_id: Uuid,
    pub agent_def_id: Uuid,
    pub invocation: AuthorizedInvocation,
}

#[derive(Debug, Clone)]
pub struct NativeTaskAccess<'a> {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub principal_subject: &'a str,
    pub target_agent_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: &'a str,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeA2aError {
    #[error(transparent)]
    A2a(#[from] A2aError),
    #[error("native Agent A2A database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("native Agent A2A alias conflicts with durable Agent ownership")]
    Ownership,
}

#[derive(Clone)]
pub struct NativeA2aRepository {
    pool: PgPool,
}

impl NativeA2aRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn bind(
        &self,
        admission: &NativeTaskAdmission,
    ) -> Result<TaskSnapshot, NativeA2aError> {
        admission.invocation.validate("light-agent", Utc::now())?;
        if admission.invocation.direction != Direction::Inbound {
            return Err(A2aError::InvalidInvocation.into());
        }
        let mut tx = self.pool.begin().await?;
        let owner: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT s.agent_def_id,t.session_id FROM agent_turn_t t
              JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
             WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.turn_id)
        .bind(admission.session_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((agent_def_id, session_id)) = owner else {
            return Err(NativeA2aError::Ownership);
        };
        if agent_def_id != admission.agent_def_id {
            return Err(NativeA2aError::Ownership);
        }
        sqlx::query(
            "INSERT INTO agent_a2a_context_alias_t(host_id,public_context_id,session_id,
               principal_subject,agent_def_id,publication_id,policy_digest,expires_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT(host_id,public_context_id) DO NOTHING",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.context_id.to_string())
        .bind(session_id)
        .bind(&admission.invocation.principal_subject)
        .bind(agent_def_id)
        .bind(admission.invocation.publication_id)
        .bind(&admission.invocation.policy_digest)
        .bind(admission.invocation.expires_at)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO agent_a2a_task_alias_t(host_id,public_task_id,public_context_id,turn_id,
               principal_subject,agent_def_id,publication_id,policy_digest,state)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,'SUBMITTED')
             ON CONFLICT(host_id,public_task_id) DO NOTHING",
        )
        .bind(admission.invocation.host_id)
        .bind(admission.task_id.to_string())
        .bind(admission.context_id.to_string())
        .bind(admission.turn_id)
        .bind(&admission.invocation.principal_subject)
        .bind(agent_def_id)
        .bind(admission.invocation.publication_id)
        .bind(&admission.invocation.policy_digest)
        .execute(&mut *tx)
        .await?;
        let snapshot = load_native_task(
            &mut tx,
            &NativeTaskAccess {
                host_id: admission.invocation.host_id,
                task_id: admission.task_id,
                principal_subject: &admission.invocation.principal_subject,
                target_agent_id: agent_def_id,
                publication_id: admission.invocation.publication_id,
                policy_digest: &admission.invocation.policy_digest,
            },
            false,
        )
        .await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub async fn get(&self, access: &NativeTaskAccess<'_>) -> Result<TaskSnapshot, NativeA2aError> {
        let mut tx = self.pool.begin().await?;
        let snapshot = load_native_task(&mut tx, access, false).await?;
        tx.commit().await?;
        Ok(snapshot)
    }

    pub async fn resolve_turn(
        &self,
        access: &NativeTaskAccess<'_>,
    ) -> Result<(Uuid, Uuid), NativeA2aError> {
        let row: Option<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT t.session_id,a.turn_id FROM agent_a2a_task_alias_t a
               JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id
              WHERE a.host_id=$1 AND a.public_task_id=$2 AND a.principal_subject=$3
                AND a.agent_def_id=$4 AND a.publication_id=$5 AND a.policy_digest=$6",
        )
        .bind(access.host_id)
        .bind(access.task_id.to_string())
        .bind(access.principal_subject)
        .bind(access.target_agent_id)
        .bind(access.publication_id)
        .bind(access.policy_digest)
        .fetch_optional(&self.pool)
        .await?;
        row.ok_or_else(|| A2aError::WrongTaskOwner.into())
    }

    pub async fn mark_canceled(
        &self,
        access: &NativeTaskAccess<'_>,
    ) -> Result<TaskSnapshot, NativeA2aError> {
        let mut tx = self.pool.begin().await?;
        let current = load_native_task(&mut tx, access, true).await?;
        if current.state.terminal() {
            return Err(A2aError::NotCancellable.into());
        }
        sqlx::query(
            "UPDATE agent_a2a_task_alias_t SET state='CANCELED',updated_ts=now()
              WHERE host_id=$1 AND public_task_id=$2",
        )
        .bind(access.host_id)
        .bind(access.task_id.to_string())
        .execute(&mut *tx)
        .await?;
        let snapshot = load_native_task(&mut tx, access, false).await?;
        tx.commit().await?;
        Ok(snapshot)
    }
}

async fn load_native_task(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    access: &NativeTaskAccess<'_>,
    lock: bool,
) -> Result<TaskSnapshot, NativeA2aError> {
    let lock_clause = if lock { " FOR UPDATE OF a" } else { "" };
    let row = sqlx::query(&format!(
        "SELECT a.public_context_id,a.state,t.terminal_result,t.terminal_error,t.state AS turn_state
           FROM agent_a2a_task_alias_t a
           JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id
          WHERE a.host_id=$1 AND a.public_task_id=$2 AND a.principal_subject=$3
            AND a.agent_def_id=$4 AND a.publication_id=$5 AND a.policy_digest=$6{lock_clause}"
    ))
    .bind(access.host_id)
    .bind(access.task_id.to_string())
    .bind(access.principal_subject)
    .bind(access.target_agent_id)
    .bind(access.publication_id)
    .bind(access.policy_digest)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(A2aError::WrongTaskOwner)?;
    let context_id = Uuid::parse_str(&row.try_get::<String, _>("public_context_id")?)
        .map_err(|_| A2aError::InvalidInvocation)?;
    let alias_state: String = row.try_get("state")?;
    let turn_state: String = row.try_get("turn_state")?;
    let state = if alias_state == "CANCELED" || turn_state == "CANCELLED" {
        TaskState::Canceled
    } else {
        match turn_state.as_str() {
            "COMPLETED" => TaskState::Completed,
            "FAILED" | "UNKNOWN" => TaskState::Failed,
            "QUEUED" => TaskState::Submitted,
            _ => TaskState::Working,
        }
    };
    Ok(TaskSnapshot {
        task_id: access.task_id,
        context_id,
        state,
        direction: Direction::Inbound,
        target_agent_ref: access.target_agent_id.to_string(),
        result: row.try_get("terminal_result")?,
        error: row.try_get("terminal_error")?,
    })
}

pub fn read_database_url(path: &Path) -> Result<String, ValidationError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ValidationError::Scope(format!("cannot inspect Agent database URL file: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ValidationError::Scope(
            "Agent database URL path must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(ValidationError::Scope(
                "Agent database URL file permissions are too broad".into(),
            ));
        }
    }
    let value = std::fs::read_to_string(path).map_err(|error| {
        ValidationError::Scope(format!("cannot read Agent database URL file: {error}"))
    })?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty()
        || value.len() > 2048
        || value.contains(['\r', '\n'])
        || !value.starts_with("postgres://operations_agent_runtime:")
        || !value.ends_with("/operations")
    {
        return Err(ValidationError::Scope(
            "Agent database URL file does not match the redacted role/database contract".into(),
        ));
    }
    Ok(value.to_string())
}

/// Fail closed unless `pool` is the least-privileged Agent connection for the
/// exact active Host/environment binding in the accepted Config projection.
pub async fn validate(
    pool: &PgPool,
    expected: &ExpectedBinding<'_>,
) -> Result<(), ValidationError> {
    let identity = sqlx::query(
        "SELECT current_database() AS database_name, current_user AS role_name,
                has_database_privilege(current_user,current_database(),'CREATE') AS database_create,
                has_schema_privilege(current_user,'agent_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    let database_name: String = identity.try_get("database_name")?;
    let role_name: String = identity.try_get("role_name")?;
    let database_create: bool = identity.try_get("database_create")?;
    let schema_create: bool = identity.try_get("schema_create")?;
    if database_name != EXPECTED_DATABASE
        || role_name != EXPECTED_RUNTIME_ROLE
        || database_create
        || schema_create
    {
        return Err(ValidationError::Scope(format!(
            "expected database {EXPECTED_DATABASE}, role {EXPECTED_RUNTIME_ROLE}, and no CREATE authority; got database {database_name}, role {role_name}"
        )));
    }

    let binding = sqlx::query(
        "SELECT binding_id,binding_digest,host_id,environment,schema_contract_generation
           FROM operational_meta.operational_store_binding_t WHERE active",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ValidationError::Scope("no active operational-store binding".into()))?;
    let actual_binding_id: Uuid = binding.try_get("binding_id")?;
    let actual_binding_digest: String = binding.try_get("binding_digest")?;
    let actual_host_id: Uuid = binding.try_get("host_id")?;
    let actual_environment: String = binding.try_get("environment")?;
    let generation: i64 = binding.try_get("schema_contract_generation")?;
    if actual_binding_id != expected.binding_id
        || actual_binding_digest != expected.binding_digest
        || actual_host_id != expected.host_id
        || actual_environment != expected.environment
        || generation < expected.minimum_schema_generation
    {
        return Err(ValidationError::Scope(
            "active operational-store binding does not match the Agent projection".into(),
        ));
    }

    let required = AUTHORITY_TABLES
        .iter()
        .chain(SUPPORT_TABLES)
        .chain(NATIVE_A2A_TABLES)
        .copied()
        .collect::<Vec<_>>();
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT required.table_name FROM unnest($1::text[]) AS required(table_name)
          WHERE to_regclass('agent_ops.' || required.table_name) IS NULL",
    )
    .bind(&required)
    .fetch_all(pool)
    .await?;
    if !missing.is_empty() {
        return Err(ValidationError::Scope(format!(
            "Agent schema is incomplete; missing {}",
            missing.join(",")
        )));
    }

    let migration_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='agent-store' AND schema_name='agent_ops'
            AND migration_id=$1)",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !migration_ready {
        return Err(ValidationError::Scope(
            "agent-store migration ledger entry is missing".into(),
        ));
    }
    let native_a2a_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='agent-store' AND schema_name='agent_ops'
            AND migration_id=$1)",
    )
    .bind(NATIVE_A2A_MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !native_a2a_ready {
        return Err(ValidationError::Scope(
            "native Agent A2A migration ledger entry is missing".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_agent_inventory_is_exact() {
        assert_eq!(AUTHORITY_TABLES.len(), 21);
        assert_eq!(SUPPORT_TABLES.len(), 4);
        assert_eq!(MIGRATIONS.len(), 2);
        let sql = MIGRATIONS[0].1;
        for table in AUTHORITY_TABLES.iter().chain(SUPPORT_TABLES) {
            assert!(sql.contains(&format!("agent_ops.{table}")));
        }
        assert!(!sql.contains("configserver."));
        assert!(!sql.contains("knowledge."));
        assert!(!sql.contains("REFERENCES public."));
        assert!(sql.contains("agent_session_t_host_id_bank_id_fkey"));
        let native_a2a_sql = MIGRATIONS[1].1;
        for table in NATIVE_A2A_TABLES {
            assert!(native_a2a_sql.contains(&format!("agent_ops.{table}")));
        }
        assert!(native_a2a_sql.contains("REFERENCES agent_ops.agent_session_t"));
        assert!(native_a2a_sql.contains("REFERENCES agent_ops.agent_turn_t"));
    }
}
