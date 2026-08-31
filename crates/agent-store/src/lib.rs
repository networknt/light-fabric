//! Agent and embedded-memory operational-store authority.
//!
//! `light-agent` has no Config Server database connection. It validates its
//! dedicated `operations_agent_runtime` pool against the immutable binding
//! projection before becoming ready or accepting work.

use a2a_core::{
    A2aError, ArtifactDescriptor, ArtifactVisibility, AuthorizedInvocation, Direction,
    TaskSnapshot, TaskState,
};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "agent_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_agent_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";
pub const MIGRATION_ID: &str = "0001_agent_and_embedded_memory";
pub const NATIVE_A2A_MIGRATION_ID: &str = "0002_native_a2a_aliases";
pub const NATIVE_A2A_PHASE4_MIGRATION_ID: &str = "0003_native_a2a_phase4";
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        MIGRATION_ID,
        include_str!("../migrations/agent-postgres/0001_agent_and_embedded_memory.sql"),
    ),
    (
        NATIVE_A2A_MIGRATION_ID,
        include_str!("../migrations/agent-postgres/0002_native_a2a_aliases.sql"),
    ),
    (
        NATIVE_A2A_PHASE4_MIGRATION_ID,
        include_str!("../migrations/agent-postgres/0003_native_a2a_phase4.sql"),
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
    pub message_id: String,
    pub skill_mapping: Value,
    pub skill_mapping_digest: String,
    pub invocation: AuthorizedInvocation,
}

#[derive(Debug, Clone)]
pub struct NativeTaskAccess<'a> {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub principal_subject: &'a str,
    pub target_agent_id: Uuid,
    pub publication_id: Uuid,
}

#[derive(Debug, Clone)]
pub struct NativeTaskListAccess<'a> {
    pub host_id: Uuid,
    pub principal_subject: &'a str,
    pub target_agent_id: Uuid,
    pub publication_id: Uuid,
    pub context_id: Option<Uuid>,
    pub status: Option<TaskState>,
    pub status_timestamp_after: Option<DateTime<Utc>>,
    pub cursor: Option<(DateTime<Utc>, Uuid)>,
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct NativeTaskPage {
    pub tasks: Vec<TaskSnapshot>,
    pub total_size: usize,
    pub next_cursor: Option<(DateTime<Utc>, Uuid)>,
}

#[derive(Debug, Clone)]
pub struct NativeArtifactAdmission<'a> {
    pub artifact_id: Uuid,
    pub logical_name: &'a str,
    pub media_type: &'a str,
    pub size_bytes: u64,
    pub content_digest: &'a str,
    pub object_reference: &'a str,
    pub provenance_digest: &'a str,
    pub retain_until: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ExpiredNativeArtifact {
    pub artifact_id: Uuid,
    pub object_reference: String,
}

#[derive(Debug, thiserror::Error)]
pub enum NativeA2aError {
    #[error(transparent)]
    A2a(#[from] A2aError),
    #[error("native Agent A2A database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("native Agent A2A alias conflicts with durable Agent ownership")]
    Ownership,
    #[error("native Agent A2A artifact request is invalid")]
    InvalidArtifact,
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
        if admission.invocation.direction != Direction::Inbound
            || admission.message_id.trim().is_empty()
            || admission.message_id.len() > 256
            || !canonical_digest(&admission.skill_mapping_digest)
            || !admission.skill_mapping.is_array()
        {
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
        let context_inserted = sqlx::query(
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
        .await?
        .rows_affected();
        if context_inserted == 0 {
            let matches: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM agent_a2a_context_alias_t
                  WHERE host_id=$1 AND public_context_id=$2 AND session_id=$3
                    AND principal_subject=$4 AND agent_def_id=$5 AND publication_id=$6)",
            )
            .bind(admission.invocation.host_id)
            .bind(admission.context_id.to_string())
            .bind(session_id)
            .bind(&admission.invocation.principal_subject)
            .bind(agent_def_id)
            .bind(admission.invocation.publication_id)
            .fetch_one(&mut *tx)
            .await?;
            if !matches {
                return Err(NativeA2aError::Ownership);
            }
        }
        let task_inserted = sqlx::query(
            "INSERT INTO agent_a2a_task_alias_t(host_id,public_task_id,public_context_id,turn_id,
               principal_subject,agent_def_id,publication_id,policy_digest,state,message_id,
               skill_mapping,skill_mapping_digest)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,'SUBMITTED',$9,$10,$11)
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
        .bind(&admission.message_id)
        .bind(&admission.skill_mapping)
        .bind(&admission.skill_mapping_digest)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if task_inserted == 0 {
            let matches: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM agent_a2a_task_alias_t
                  WHERE host_id=$1 AND public_task_id=$2 AND public_context_id=$3 AND turn_id=$4
                    AND principal_subject=$5 AND agent_def_id=$6 AND publication_id=$7
                    AND message_id=$8)",
            )
            .bind(admission.invocation.host_id)
            .bind(admission.task_id.to_string())
            .bind(admission.context_id.to_string())
            .bind(admission.turn_id)
            .bind(&admission.invocation.principal_subject)
            .bind(agent_def_id)
            .bind(admission.invocation.publication_id)
            .bind(&admission.message_id)
            .fetch_one(&mut *tx)
            .await?;
            if !matches {
                return Err(NativeA2aError::Ownership);
            }
        }
        let snapshot = load_native_task(
            &mut tx,
            &NativeTaskAccess {
                host_id: admission.invocation.host_id,
                task_id: admission.task_id,
                principal_subject: &admission.invocation.principal_subject,
                target_agent_id: agent_def_id,
                publication_id: admission.invocation.publication_id,
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
                AND a.agent_def_id=$4 AND a.publication_id=$5",
        )
        .bind(access.host_id)
        .bind(access.task_id.to_string())
        .bind(access.principal_subject)
        .bind(access.target_agent_id)
        .bind(access.publication_id)
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
        if current.state == TaskState::Canceled {
            tx.commit().await?;
            return Ok(current);
        }
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

    pub async fn list(
        &self,
        access: &NativeTaskListAccess<'_>,
    ) -> Result<NativeTaskPage, NativeA2aError> {
        if !(1..=100).contains(&access.limit) {
            return Err(A2aError::InvalidInvocation.into());
        }
        let status = access.status.map(task_state_filter);
        let total_size: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_a2a_task_alias_t a
               JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id
              WHERE a.host_id=$1 AND a.principal_subject=$2 AND a.agent_def_id=$3
                AND a.publication_id=$4
                AND ($5::text IS NULL OR a.public_context_id=$5)
                AND ($6::text IS NULL OR CASE
                  WHEN a.state='CANCELED' OR t.state='CANCELLED' THEN 'CANCELED'
                  WHEN t.state='COMPLETED' THEN 'COMPLETED'
                  WHEN t.state IN ('FAILED','UNKNOWN') THEN 'FAILED'
                  WHEN t.state='QUEUED' THEN 'SUBMITTED'
                  ELSE 'WORKING' END=$6)
                AND ($7::timestamptz IS NULL OR t.updated_ts >= $7)",
        )
        .bind(access.host_id)
        .bind(access.principal_subject)
        .bind(access.target_agent_id)
        .bind(access.publication_id)
        .bind(access.context_id.map(|value| value.to_string()))
        .bind(status)
        .bind(access.status_timestamp_after)
        .fetch_one(&self.pool)
        .await?;
        let cursor_at = access.cursor.map(|cursor| cursor.0);
        let cursor_id = access.cursor.map(|cursor| cursor.1.to_string());
        let rows = sqlx::query(
            "SELECT a.public_task_id,a.created_ts FROM agent_a2a_task_alias_t a
               JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id
              WHERE a.host_id=$1 AND a.principal_subject=$2 AND a.agent_def_id=$3
                AND a.publication_id=$4
                AND ($5::text IS NULL OR a.public_context_id=$5)
                AND ($6::text IS NULL OR CASE
                  WHEN a.state='CANCELED' OR t.state='CANCELLED' THEN 'CANCELED'
                  WHEN t.state='COMPLETED' THEN 'COMPLETED'
                  WHEN t.state IN ('FAILED','UNKNOWN') THEN 'FAILED'
                  WHEN t.state='QUEUED' THEN 'SUBMITTED'
                  ELSE 'WORKING' END=$6)
                AND ($7::timestamptz IS NULL OR t.updated_ts >= $7)
                AND ($8::timestamptz IS NULL OR (a.created_ts,a.public_task_id) < ($8,$9))
              ORDER BY a.created_ts DESC,a.public_task_id DESC LIMIT $10",
        )
        .bind(access.host_id)
        .bind(access.principal_subject)
        .bind(access.target_agent_id)
        .bind(access.publication_id)
        .bind(access.context_id.map(|value| value.to_string()))
        .bind(status)
        .bind(access.status_timestamp_after)
        .bind(cursor_at)
        .bind(cursor_id)
        .bind(i64::try_from(access.limit + 1).map_err(|_| A2aError::InvalidInvocation)?)
        .fetch_all(&self.pool)
        .await?;
        let has_more = rows.len() > access.limit;
        let visible = rows.into_iter().take(access.limit).collect::<Vec<_>>();
        let next_cursor = if has_more {
            visible
                .last()
                .map(|row| -> Result<_, NativeA2aError> {
                    Ok((
                        row.try_get::<DateTime<Utc>, _>("created_ts")?,
                        Uuid::parse_str(&row.try_get::<String, _>("public_task_id")?)
                            .map_err(|_| A2aError::InvalidInvocation)?,
                    ))
                })
                .transpose()?
        } else {
            None
        };
        let mut tasks = Vec::with_capacity(visible.len());
        for row in visible {
            let id: String = row.try_get("public_task_id")?;
            let task_id = Uuid::parse_str(&id).map_err(|_| A2aError::InvalidInvocation)?;
            tasks.push(
                self.get(&NativeTaskAccess {
                    host_id: access.host_id,
                    task_id,
                    principal_subject: access.principal_subject,
                    target_agent_id: access.target_agent_id,
                    publication_id: access.publication_id,
                })
                .await?,
            );
        }
        Ok(NativeTaskPage {
            tasks,
            total_size: usize::try_from(total_size).map_err(|_| A2aError::InvalidInvocation)?,
            next_cursor,
        })
    }

    pub async fn register_artifact(
        &self,
        access: &NativeTaskAccess<'_>,
        artifact: &NativeArtifactAdmission<'_>,
    ) -> Result<(), NativeA2aError> {
        if artifact.logical_name.trim().is_empty()
            || artifact.media_type.trim().is_empty()
            || artifact.size_bytes == 0
            || artifact.size_bytes > i64::MAX as u64
            || !canonical_digest(artifact.content_digest)
            || !canonical_digest(artifact.provenance_digest)
            || artifact.object_reference.trim().is_empty()
            || artifact.object_reference.contains("://")
            || artifact.retain_until <= Utc::now()
        {
            return Err(A2aError::InvalidArtifact.into());
        }
        self.get(access).await?;
        let inserted = sqlx::query(
            "INSERT INTO agent_a2a_artifact_t(host_id,artifact_id,public_task_id,logical_name,
               media_type,size_bytes,content_digest,object_reference,visibility,retain_until_ts,
               provenance_digest)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,'OWNER',$9,$10)
             ON CONFLICT(host_id,artifact_id) DO NOTHING",
        )
        .bind(access.host_id)
        .bind(artifact.artifact_id)
        .bind(access.task_id.to_string())
        .bind(artifact.logical_name)
        .bind(artifact.media_type)
        .bind(artifact.size_bytes as i64)
        .bind(artifact.content_digest)
        .bind(artifact.object_reference)
        .bind(artifact.retain_until)
        .bind(artifact.provenance_digest)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if inserted == 0 {
            let matches: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM agent_a2a_artifact_t
                  WHERE host_id=$1 AND artifact_id=$2 AND public_task_id=$3
                    AND logical_name=$4 AND media_type=$5 AND size_bytes=$6
                    AND content_digest=$7 AND object_reference=$8
                    AND provenance_digest=$9)",
            )
            .bind(access.host_id)
            .bind(artifact.artifact_id)
            .bind(access.task_id.to_string())
            .bind(artifact.logical_name)
            .bind(artifact.media_type)
            .bind(artifact.size_bytes as i64)
            .bind(artifact.content_digest)
            .bind(artifact.object_reference)
            .bind(artifact.provenance_digest)
            .fetch_one(&self.pool)
            .await?;
            if !matches {
                return Err(NativeA2aError::Ownership);
            }
        }
        Ok(())
    }

    pub async fn expire_artifacts(
        &self,
        host_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<u64, NativeA2aError> {
        Ok(sqlx::query(
            "UPDATE agent_a2a_artifact_t SET deletion_state='TOMBSTONED',
               deletion_evidence=jsonb_build_object('expiredAt',$2::timestamptz,
                 'contentRetrievable',FALSE),updated_ts=now()
             WHERE host_id=$1 AND deletion_state='RETAINED' AND NOT legal_hold
               AND retain_until_ts <= $2",
        )
        .bind(host_id)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn expired_artifacts(
        &self,
        host_id: Uuid,
        now: DateTime<Utc>,
        maximum_results: i64,
    ) -> Result<Vec<ExpiredNativeArtifact>, NativeA2aError> {
        if !(1..=100).contains(&maximum_results) {
            return Err(NativeA2aError::InvalidArtifact);
        }
        let rows = sqlx::query(
            "SELECT artifact_id,object_reference FROM agent_a2a_artifact_t
              WHERE host_id=$1 AND deletion_state='RETAINED' AND NOT legal_hold
                AND retain_until_ts<=$2 ORDER BY retain_until_ts,artifact_id LIMIT $3",
        )
        .bind(host_id)
        .bind(now)
        .bind(maximum_results)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ExpiredNativeArtifact {
                    artifact_id: row.try_get("artifact_id")?,
                    object_reference: row.try_get("object_reference")?,
                })
            })
            .collect()
    }

    pub async fn complete_artifact_deletion(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        now: DateTime<Utc>,
        evidence_digest: &str,
    ) -> Result<(), NativeA2aError> {
        if !canonical_digest(evidence_digest) {
            return Err(NativeA2aError::InvalidArtifact);
        }
        let changed = sqlx::query(
            "UPDATE agent_a2a_artifact_t SET deletion_state='TOMBSTONED',
               deletion_evidence=jsonb_build_object('expiredAt',$3::timestamptz,
                 'contentRetrievable',FALSE,'evidenceDigest',$4),updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2 AND deletion_state='RETAINED'
               AND NOT legal_hold AND retain_until_ts<=$3",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(now)
        .bind(evidence_digest)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(NativeA2aError::Ownership);
        }
        Ok(())
    }
}

fn canonical_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

const fn task_state_filter(state: TaskState) -> &'static str {
    match state {
        TaskState::Submitted => "SUBMITTED",
        TaskState::Working => "WORKING",
        TaskState::InputRequired => "INPUT_REQUIRED",
        TaskState::AuthRequired => "AUTH_REQUIRED",
        TaskState::Completed => "COMPLETED",
        TaskState::Failed => "FAILED",
        TaskState::Rejected => "REJECTED",
        TaskState::Canceled => "CANCELED",
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
            AND a.agent_def_id=$4 AND a.publication_id=$5{lock_clause}"
    ))
    .bind(access.host_id)
    .bind(access.task_id.to_string())
    .bind(access.principal_subject)
    .bind(access.target_agent_id)
    .bind(access.publication_id)
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
    let artifact_rows = sqlx::query(
        "SELECT artifact_id,logical_name,media_type,size_bytes,content_digest,
                retain_until_ts,provenance_digest
           FROM agent_a2a_artifact_t
          WHERE host_id=$1 AND public_task_id=$2 AND deletion_state='RETAINED'
            AND (retain_until_ts > now() OR legal_hold)
          ORDER BY created_ts,artifact_id",
    )
    .bind(access.host_id)
    .bind(access.task_id.to_string())
    .fetch_all(&mut **tx)
    .await?;
    let artifacts = artifact_rows
        .into_iter()
        .map(|artifact| {
            Ok(ArtifactDescriptor {
                artifact_id: artifact.try_get("artifact_id")?,
                logical_name: artifact.try_get("logical_name")?,
                media_type: artifact.try_get("media_type")?,
                size_bytes: u64::try_from(artifact.try_get::<i64, _>("size_bytes")?)
                    .map_err(|_| A2aError::InvalidArtifact)?,
                content_digest: artifact.try_get("content_digest")?,
                visibility: ArtifactVisibility::TaskOwner,
                retention_deadline: artifact.try_get("retain_until_ts")?,
                provenance_digest: artifact.try_get("provenance_digest")?,
            })
        })
        .collect::<Result<Vec<_>, NativeA2aError>>()?;
    Ok(TaskSnapshot {
        task_id: access.task_id,
        context_id,
        state,
        direction: Direction::Inbound,
        target_agent_ref: access.target_agent_id.to_string(),
        result: row.try_get("terminal_result")?,
        error: row.try_get("terminal_error")?,
        artifacts,
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
    for migration_id in [NATIVE_A2A_MIGRATION_ID, NATIVE_A2A_PHASE4_MIGRATION_ID] {
        let native_a2a_ready: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
              WHERE migration_owner='agent-store' AND schema_name='agent_ops'
                AND migration_id=$1)",
        )
        .bind(migration_id)
        .fetch_one(pool)
        .await?;
        if !native_a2a_ready {
            return Err(ValidationError::Scope(format!(
                "native Agent A2A migration ledger entry is missing: {migration_id}"
            )));
        }
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
        assert_eq!(MIGRATIONS.len(), 3);
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
        assert!(MIGRATIONS[2].1.contains("skill_mapping_digest"));
    }
}
