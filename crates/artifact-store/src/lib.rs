//! Artifact lifecycle metadata. Artifact bytes are never accepted by this API.

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "artifact_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_artifact_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/artifact-database-url";
pub const MIGRATION_ID: &str = "0001_artifact_metadata";
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/artifact-postgres/0001_artifact_metadata.sql");
pub const AUTHORITY_TABLES: &[&str] = &[
    "artifact_metadata_t",
    "artifact_relationship_t",
    "artifact_hold_t",
    "artifact_event_t",
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
pub struct ArtifactRegistration<'a> {
    pub artifact_id: Uuid,
    pub owner_service: &'a str,
    pub owner_kind: &'a str,
    pub owner_id: &'a str,
    pub logical_name: &'a str,
    pub media_type: &'a str,
    pub size_bytes: i64,
    pub content_digest: &'a str,
    pub object_reference: &'a str,
    pub visibility: &'a str,
    pub retain_until: DateTime<Utc>,
    pub relationship_kind: &'a str,
    pub related_service: &'a str,
    pub related_id: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactExportRecord {
    pub artifact_id: Uuid,
    pub owner_service: String,
    pub owner_kind: String,
    pub owner_id: String,
    pub logical_name: String,
    pub size_bytes: i64,
    pub content_digest: String,
    pub object_reference: String,
    pub scan_state: String,
    pub retain_until: DateTime<Utc>,
    pub legal_hold: bool,
    pub lifecycle_state: String,
    pub tombstone_digest: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("artifact database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("artifact scope validation failed: {0}")]
    Scope(String),
    #[error("artifact is under legal hold")]
    LegalHold,
    #[error("artifact retention period has not expired")]
    Retained,
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

    pub async fn register(
        &self,
        host_id: Uuid,
        artifact: &ArtifactRegistration<'_>,
    ) -> Result<(), StoreError> {
        validate_registration(artifact)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO artifact_metadata_t(
               host_id,artifact_id,owner_service,owner_kind,owner_id,logical_name,media_type,
               size_bytes,content_digest,object_reference,visibility,retain_until_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind(host_id)
        .bind(artifact.artifact_id)
        .bind(artifact.owner_service)
        .bind(artifact.owner_kind)
        .bind(artifact.owner_id)
        .bind(artifact.logical_name)
        .bind(artifact.media_type)
        .bind(artifact.size_bytes)
        .bind(artifact.content_digest)
        .bind(artifact.object_reference)
        .bind(artifact.visibility)
        .bind(artifact.retain_until)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO artifact_relationship_t(
               host_id,artifact_id,relationship_kind,related_service,related_id)
             VALUES($1,$2,$3,$4,$5)",
        )
        .bind(host_id)
        .bind(artifact.artifact_id)
        .bind(artifact.relationship_kind)
        .bind(artifact.related_service)
        .bind(artifact.related_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            host_id,
            artifact.artifact_id,
            "ARTIFACT_REGISTERED",
            &sha256_digest(&format!(
                "{}|{}|{}",
                artifact.artifact_id, artifact.content_digest, artifact.object_reference
            )),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn record_scan(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        scan_state: &str,
        scan_profile_id: &str,
        scan_evidence_digest: &str,
    ) -> Result<(), StoreError> {
        if !matches!(scan_state, "CLEAN" | "REJECTED" | "ERROR")
            || scan_profile_id.trim().is_empty()
            || !valid_digest(scan_evidence_digest)
        {
            return Err(StoreError::Scope("invalid artifact scan evidence".into()));
        }
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE artifact_metadata_t SET scan_state=$3,scan_profile_id=$4,
               scan_evidence_digest=$5,updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2 AND lifecycle_state='RETAINED'",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(scan_state)
        .bind(scan_profile_id)
        .bind(scan_evidence_digest)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::Scope(
                "owned retained artifact not found".into(),
            ));
        }
        append_event(
            &mut tx,
            host_id,
            artifact_id,
            "ARTIFACT_SCANNED",
            scan_evidence_digest,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn place_hold(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        hold_id: Uuid,
        reason_code: &str,
    ) -> Result<(), StoreError> {
        if reason_code.trim().is_empty() {
            return Err(StoreError::Scope("artifact hold reason is required".into()));
        }
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE artifact_metadata_t SET legal_hold=TRUE,updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2 AND lifecycle_state='RETAINED'",
        )
        .bind(host_id)
        .bind(artifact_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(StoreError::Scope(
                "owned retained artifact not found".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO artifact_hold_t(host_id,artifact_id,hold_id,reason_code)
             VALUES($1,$2,$3,$4)
             ON CONFLICT(host_id,artifact_id,hold_id) DO UPDATE SET
               reason_code=EXCLUDED.reason_code,active=TRUE,released_ts=NULL",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(hold_id)
        .bind(reason_code)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            host_id,
            artifact_id,
            "ARTIFACT_HOLD_PLACED",
            &sha256_digest(reason_code),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Establish the linearization point for managed-byte deletion.
    ///
    /// The row lock makes retention, legal-hold placement, and deletion
    /// authorization mutually exclusive. Once this succeeds, a new hold may
    /// not be placed and the caller may remove the managed bytes.
    pub async fn begin_deletion(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT retain_until_ts,legal_hold,lifecycle_state FROM artifact_metadata_t
             WHERE host_id=$1 AND artifact_id=$2 FOR UPDATE",
        )
        .bind(host_id)
        .bind(artifact_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::Scope("owned artifact not found".into()))?;
        let lifecycle_state: String = row.try_get("lifecycle_state")?;
        if lifecycle_state == "TOMBSTONED" || lifecycle_state == "DELETING" {
            tx.commit().await?;
            return Ok(());
        }
        if lifecycle_state != "RETAINED" {
            return Err(StoreError::Scope(
                "artifact is not eligible for deletion".into(),
            ));
        }
        if row.try_get::<bool, _>("legal_hold")? {
            return Err(StoreError::LegalHold);
        }
        if row.try_get::<DateTime<Utc>, _>("retain_until_ts")? > now {
            return Err(StoreError::Retained);
        }
        sqlx::query(
            "UPDATE artifact_metadata_t SET lifecycle_state='DELETING',updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(host_id)
        .bind(artifact_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            host_id,
            artifact_id,
            "ARTIFACT_DELETION_STARTED",
            &sha256_digest(&artifact_id.to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn release_hold(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        hold_id: Uuid,
    ) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let changed = sqlx::query(
            "UPDATE artifact_hold_t SET active=FALSE,released_ts=now()
             WHERE host_id=$1 AND artifact_id=$2 AND hold_id=$3 AND active",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(hold_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            let inactive_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM artifact_hold_t
                   WHERE host_id=$1 AND artifact_id=$2 AND hold_id=$3 AND NOT active)",
            )
            .bind(host_id)
            .bind(artifact_id)
            .bind(hold_id)
            .fetch_one(&mut *tx)
            .await?;
            if !inactive_exists {
                return Err(StoreError::Scope("artifact hold not found".into()));
            }
        }
        sqlx::query(
            "UPDATE artifact_metadata_t SET legal_hold=EXISTS(
                SELECT 1 FROM artifact_hold_t h WHERE h.host_id=$1 AND h.artifact_id=$2 AND h.active),
               updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(host_id)
        .bind(artifact_id)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            host_id,
            artifact_id,
            "ARTIFACT_HOLD_RELEASED",
            &sha256_digest(&hold_id.to_string()),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn tombstone(
        &self,
        host_id: Uuid,
        artifact_id: Uuid,
        tombstone_digest: &str,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if !valid_digest(tombstone_digest) {
            return Err(StoreError::Scope(
                "invalid artifact tombstone digest".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT retain_until_ts,legal_hold,lifecycle_state FROM artifact_metadata_t
             WHERE host_id=$1 AND artifact_id=$2 FOR UPDATE",
        )
        .bind(host_id)
        .bind(artifact_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::Scope("owned artifact not found".into()))?;
        let lifecycle_state: String = row.try_get("lifecycle_state")?;
        if lifecycle_state == "TOMBSTONED" {
            tx.commit().await?;
            return Ok(());
        }
        if lifecycle_state != "DELETING" {
            return Err(StoreError::Scope(
                "artifact deletion was not authorized".into(),
            ));
        }
        if row.try_get::<bool, _>("legal_hold")? {
            return Err(StoreError::LegalHold);
        }
        if row.try_get::<DateTime<Utc>, _>("retain_until_ts")? > now {
            return Err(StoreError::Retained);
        }
        sqlx::query(
            "UPDATE artifact_metadata_t SET lifecycle_state='TOMBSTONED',
               tombstone_digest=$3,updated_ts=now()
             WHERE host_id=$1 AND artifact_id=$2",
        )
        .bind(host_id)
        .bind(artifact_id)
        .bind(tombstone_digest)
        .execute(&mut *tx)
        .await?;
        append_event(
            &mut tx,
            host_id,
            artifact_id,
            "ARTIFACT_TOMBSTONED",
            tombstone_digest,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn export_host(
        &self,
        host_id: Uuid,
    ) -> Result<Vec<ArtifactExportRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT artifact_id,owner_service,owner_kind,owner_id,logical_name,size_bytes,
                    content_digest,object_reference,scan_state,retain_until_ts,legal_hold,
                    lifecycle_state,tombstone_digest
               FROM artifact_metadata_t WHERE host_id=$1 ORDER BY artifact_id",
        )
        .bind(host_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ArtifactExportRecord {
                    artifact_id: row.try_get("artifact_id")?,
                    owner_service: row.try_get("owner_service")?,
                    owner_kind: row.try_get("owner_kind")?,
                    owner_id: row.try_get("owner_id")?,
                    logical_name: row.try_get("logical_name")?,
                    size_bytes: row.try_get("size_bytes")?,
                    content_digest: row.try_get("content_digest")?,
                    object_reference: row.try_get("object_reference")?,
                    scan_state: row.try_get("scan_state")?,
                    retain_until: row.try_get("retain_until_ts")?,
                    legal_hold: row.try_get("legal_hold")?,
                    lifecycle_state: row.try_get("lifecycle_state")?,
                    tombstone_digest: row.try_get("tombstone_digest")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }
}

async fn append_event(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    host_id: Uuid,
    artifact_id: Uuid,
    event_type: &str,
    evidence_digest: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO artifact_event_t(host_id,artifact_id,sequence_no,event_type,evidence_digest)
         SELECT $1,$2,COALESCE(MAX(sequence_no),0)+1,$3,$4 FROM artifact_event_t
          WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(host_id)
    .bind(artifact_id)
    .bind(event_type)
    .bind(evidence_digest)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn validate_registration(artifact: &ArtifactRegistration<'_>) -> Result<(), StoreError> {
    if artifact.owner_service.trim().is_empty()
        || artifact.owner_kind.trim().is_empty()
        || artifact.owner_id.trim().is_empty()
        || artifact.logical_name.trim().is_empty()
        || artifact.media_type.trim().is_empty()
        || artifact.size_bytes < 0
        || !valid_digest(artifact.content_digest)
        || artifact.object_reference.trim().is_empty()
        || artifact.object_reference.contains("://")
        || artifact
            .object_reference
            .split('/')
            .any(|part| part == "..")
        || !matches!(
            artifact.owner_kind,
            "TASK" | "SESSION" | "TURN" | "PROCESS" | "EXECUTION" | "CONTEXT"
        )
        || !matches!(
            artifact.visibility,
            "OWNER" | "AUTHORIZED_CALLER" | "TENANT_POLICY"
        )
        || !matches!(
            artifact.relationship_kind,
            "TASK" | "SESSION" | "TURN" | "PROCESS" | "EXECUTION" | "CONTEXT"
        )
        || artifact.related_service.trim().is_empty()
        || artifact.related_id.trim().is_empty()
    {
        return Err(StoreError::Scope("invalid artifact registration".into()));
    }
    Ok(())
}

pub fn sha256_digest(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

pub fn read_database_url(path: &Path) -> Result<String, StoreError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        StoreError::Scope(format!("cannot inspect artifact database URL: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Scope(
            "artifact database URL must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(StoreError::Scope(
                "artifact database URL file permissions are too broad".into(),
            ));
        }
    }
    let value = std::fs::read_to_string(path).map_err(|error| {
        StoreError::Scope(format!("cannot read artifact database URL: {error}"))
    })?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty()
        || value.len() > 2048
        || value.contains(['\r', '\n'])
        || !value.starts_with("postgres://operations_artifact_runtime:")
        || !value.ends_with("/operations")
    {
        return Err(StoreError::Scope(
            "artifact database URL does not match the runtime role/database contract".into(),
        ));
    }
    Ok(value.to_string())
}

pub async fn validate(pool: &PgPool, expected: &ExpectedBinding<'_>) -> Result<(), StoreError> {
    let identity = sqlx::query(
        "SELECT current_database() AS database_name,current_user AS role_name,
                has_database_privilege(current_user,current_database(),'CREATE') AS database_create,
                has_schema_privilege(current_user,'artifact_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<String, _>("database_name")? != EXPECTED_DATABASE
        || identity.try_get::<String, _>("role_name")? != EXPECTED_RUNTIME_ROLE
        || identity.try_get::<bool, _>("database_create")?
        || identity.try_get::<bool, _>("schema_create")?
    {
        return Err(StoreError::Scope(
            "artifact database identity mismatch".into(),
        ));
    }
    let binding = sqlx::query(
        "SELECT binding_id,binding_digest,host_id,environment,schema_contract_generation
           FROM operational_meta.operational_store_binding_t WHERE active",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| StoreError::Scope("active operational binding is missing".into()))?;
    if binding.try_get::<Uuid, _>("binding_id")? != expected.binding_id
        || binding.try_get::<String, _>("binding_digest")? != expected.binding_digest
        || binding.try_get::<Uuid, _>("host_id")? != expected.host_id
        || binding.try_get::<String, _>("environment")? != expected.environment
        || binding.try_get::<i64, _>("schema_contract_generation")?
            < expected.minimum_schema_generation
    {
        return Err(StoreError::Scope(
            "artifact operational binding mismatch".into(),
        ));
    }
    let applied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='artifact-store' AND schema_name='artifact_ops' AND migration_id=$1",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if applied != 1 {
        return Err(StoreError::Scope(
            "artifact migration is not applied".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifact_registration_has_no_bytes_field() {
        let registration = ArtifactRegistration {
            artifact_id: Uuid::nil(),
            owner_service: "light-agent",
            owner_kind: "SESSION",
            owner_id: "session",
            logical_name: "result.json",
            media_type: "application/json",
            size_bytes: 2,
            content_digest: &sha256_digest("{}"),
            object_reference: "host/artifacts/result",
            visibility: "OWNER",
            retain_until: Utc::now(),
            relationship_kind: "SESSION",
            related_service: "light-agent",
            related_id: "session",
        };
        assert!(validate_registration(&registration).is_ok());
    }
}
