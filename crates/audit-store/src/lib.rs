//! Tenant audit authority with fail-closed redaction and immutable evidence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "audit_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_audit_publisher";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/audit-database-url";
pub const MIGRATION_ID: &str = "0001_tenant_audit_store";
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/audit-postgres/0001_tenant_audit_store.sql");
pub const AUTHORITY_TABLES: &[&str] = &["audit_record_t", "audit_delivery_t", "audit_hold_t"];

const FORBIDDEN_KEYS: &[&str] = &[
    "authorization",
    "cookie",
    "set-cookie",
    "password",
    "secret",
    "token",
    "credential",
    "prompt",
    "message",
    "content",
    "arguments",
    "request",
    "requestbody",
    "response",
    "responsebody",
    "body",
    "artifactbytes",
];

#[derive(Debug, Clone)]
pub struct ExpectedBinding<'a> {
    pub binding_id: Uuid,
    pub binding_digest: &'a str,
    pub host_id: Uuid,
    pub environment: &'a str,
    pub minimum_schema_generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AuditClass {
    Security,
    Accounting,
    Approval,
    Artifact,
    Deletion,
    Operator,
}

impl AuditClass {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Security => "SECURITY",
            Self::Accounting => "ACCOUNTING",
            Self::Approval => "APPROVAL",
            Self::Artifact => "ARTIFACT",
            Self::Deletion => "DELETION",
            Self::Operator => "OPERATOR",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditRecord<'a> {
    pub audit_id: Uuid,
    pub source_service: &'a str,
    pub source_instance: &'a str,
    pub event_type: &'a str,
    pub event_class: AuditClass,
    pub actor_digest: Option<&'a str>,
    pub subject_kind: Option<&'a str>,
    pub subject_digest: Option<&'a str>,
    pub correlation_digest: Option<&'a str>,
    pub policy_digest: Option<&'a str>,
    pub redacted_payload: &'a Value,
    pub occurred_at: DateTime<Utc>,
    pub retain_until: DateTime<Utc>,
    pub sink_profile_id: &'a str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuditExportRecord {
    pub audit_id: Uuid,
    pub event_type: String,
    pub event_class: String,
    pub subject_kind: Option<String>,
    pub subject_digest: Option<String>,
    pub evidence_digest: String,
    pub occurred_at: DateTime<Utc>,
    pub retain_until: DateTime<Utc>,
    pub legal_hold: bool,
    pub erasure_state: String,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("audit database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("audit scope validation failed: {0}")]
    Scope(String),
    #[error("audit payload contains prohibited field `{0}`")]
    ProhibitedField(String),
    #[error("audit subject is under legal hold")]
    LegalHold,
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

    pub async fn append(
        &self,
        host_id: Uuid,
        record: &AuditRecord<'_>,
    ) -> Result<String, StoreError> {
        validate_record(record)?;
        let digest = evidence_digest(host_id, record)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO audit_record_t(
               host_id,audit_id,source_service,source_instance,event_type,event_class,
               actor_digest,subject_kind,subject_digest,correlation_digest,policy_digest,
               redacted_payload,evidence_digest,occurred_ts,retain_until_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)",
        )
        .bind(host_id)
        .bind(record.audit_id)
        .bind(record.source_service)
        .bind(record.source_instance)
        .bind(record.event_type)
        .bind(record.event_class.as_str())
        .bind(record.actor_digest)
        .bind(record.subject_kind)
        .bind(record.subject_digest)
        .bind(record.correlation_digest)
        .bind(record.policy_digest)
        .bind(record.redacted_payload)
        .bind(&digest)
        .bind(record.occurred_at)
        .bind(record.retain_until)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO audit_delivery_t(host_id,audit_id,sink_profile_id)
             VALUES($1,$2,$3)",
        )
        .bind(host_id)
        .bind(record.audit_id)
        .bind(record.sink_profile_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(digest)
    }

    pub async fn place_hold(
        &self,
        host_id: Uuid,
        hold_id: Uuid,
        subject_kind: &str,
        subject_digest: &str,
        reason_code: &str,
    ) -> Result<(), StoreError> {
        if subject_kind.trim().is_empty()
            || reason_code.trim().is_empty()
            || !valid_digest(subject_digest)
        {
            return Err(StoreError::Scope("invalid audit hold".into()));
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO audit_hold_t(host_id,hold_id,subject_kind,subject_digest,reason_code)
             VALUES($1,$2,$3,$4,$5)",
        )
        .bind(host_id)
        .bind(hold_id)
        .bind(subject_kind)
        .bind(subject_digest)
        .bind(reason_code)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE audit_record_t SET legal_hold=TRUE
             WHERE host_id=$1 AND subject_kind=$2 AND subject_digest=$3",
        )
        .bind(host_id)
        .bind(subject_kind)
        .bind(subject_digest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn release_hold(&self, host_id: Uuid, hold_id: Uuid) -> Result<(), StoreError> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "UPDATE audit_hold_t SET active=FALSE,released_ts=now()
             WHERE host_id=$1 AND hold_id=$2 AND active
             RETURNING subject_kind,subject_digest",
        )
        .bind(host_id)
        .bind(hold_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| StoreError::Scope("active audit hold not found".into()))?;
        let subject_kind: String = row.try_get("subject_kind")?;
        let subject_digest: String = row.try_get("subject_digest")?;
        sqlx::query(
            "UPDATE audit_record_t SET legal_hold=EXISTS(
                SELECT 1 FROM audit_hold_t h WHERE h.host_id=$1 AND h.subject_kind=$2
                  AND h.subject_digest=$3 AND h.active)
             WHERE host_id=$1 AND subject_kind=$2 AND subject_digest=$3",
        )
        .bind(host_id)
        .bind(subject_kind)
        .bind(subject_digest)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn tombstone_subject(
        &self,
        host_id: Uuid,
        subject_kind: &str,
        subject_digest: &str,
        erasure_evidence_digest: &str,
    ) -> Result<u64, StoreError> {
        if !valid_digest(subject_digest) || !valid_digest(erasure_evidence_digest) {
            return Err(StoreError::Scope("invalid audit erasure digest".into()));
        }
        let held: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM audit_hold_t
             WHERE host_id=$1 AND subject_kind=$2 AND subject_digest=$3 AND active)",
        )
        .bind(host_id)
        .bind(subject_kind)
        .bind(subject_digest)
        .fetch_one(&self.pool)
        .await?;
        if held {
            return Err(StoreError::LegalHold);
        }
        Ok(sqlx::query(
            "UPDATE audit_record_t SET erasure_state='TOMBSTONED',
               erasure_evidence_digest=$4
             WHERE host_id=$1 AND subject_kind=$2 AND subject_digest=$3 AND NOT legal_hold",
        )
        .bind(host_id)
        .bind(subject_kind)
        .bind(subject_digest)
        .bind(erasure_evidence_digest)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn export_host(&self, host_id: Uuid) -> Result<Vec<AuditExportRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT audit_id,event_type,event_class,subject_kind,subject_digest,evidence_digest,
                    occurred_ts,retain_until_ts,legal_hold,erasure_state
               FROM audit_record_t WHERE host_id=$1 ORDER BY occurred_ts,audit_id",
        )
        .bind(host_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(AuditExportRecord {
                    audit_id: row.try_get("audit_id")?,
                    event_type: row.try_get("event_type")?,
                    event_class: row.try_get("event_class")?,
                    subject_kind: row.try_get("subject_kind")?,
                    subject_digest: row.try_get("subject_digest")?,
                    evidence_digest: row.try_get("evidence_digest")?,
                    occurred_at: row.try_get("occurred_ts")?,
                    retain_until: row.try_get("retain_until_ts")?,
                    legal_hold: row.try_get("legal_hold")?,
                    erasure_state: row.try_get("erasure_state")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }
}

fn validate_record(record: &AuditRecord<'_>) -> Result<(), StoreError> {
    if record.source_service.trim().is_empty()
        || record.source_instance.trim().is_empty()
        || record.event_type.trim().is_empty()
        || record.sink_profile_id.trim().is_empty()
        || record.retain_until <= record.occurred_at
        || !record.redacted_payload.is_object()
    {
        return Err(StoreError::Scope("invalid audit record".into()));
    }
    reject_prohibited_fields(record.redacted_payload)?;
    for digest in [
        record.actor_digest,
        record.subject_digest,
        record.correlation_digest,
        record.policy_digest,
    ]
    .into_iter()
    .flatten()
    {
        if !valid_digest(digest) {
            return Err(StoreError::Scope("invalid audit digest".into()));
        }
    }
    Ok(())
}

pub fn reject_prohibited_fields(value: &Value) -> Result<(), StoreError> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let normalized = key
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
                    .collect::<String>()
                    .to_ascii_lowercase();
                if FORBIDDEN_KEYS
                    .iter()
                    .any(|forbidden| normalized == *forbidden)
                {
                    return Err(StoreError::ProhibitedField(key.clone()));
                }
                reject_prohibited_fields(child)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                reject_prohibited_fields(child)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn evidence_digest(host_id: Uuid, record: &AuditRecord<'_>) -> Result<String, StoreError> {
    let payload = serde_json::to_string(record.redacted_payload).map_err(|error| {
        StoreError::Scope(format!("cannot canonicalize audit payload: {error}"))
    })?;
    let canonical = format!(
        "{host_id}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
        record.audit_id,
        record.source_service,
        record.source_instance,
        record.event_type,
        record.event_class.as_str(),
        record.actor_digest.unwrap_or(""),
        record.subject_kind.unwrap_or(""),
        record.subject_digest.unwrap_or(""),
        record.correlation_digest.unwrap_or(""),
        record.policy_digest.unwrap_or(""),
        record.occurred_at.to_rfc3339(),
        payload,
    );
    Ok(format!("sha256:{:x}", Sha256::digest(canonical.as_bytes())))
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
        StoreError::Scope(format!("cannot inspect audit database URL: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Scope(
            "audit database URL must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(StoreError::Scope(
                "audit database URL file permissions are too broad".into(),
            ));
        }
    }
    let value = std::fs::read_to_string(path)
        .map_err(|error| StoreError::Scope(format!("cannot read audit database URL: {error}")))?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty()
        || value.len() > 2048
        || value.contains(['\r', '\n'])
        || !value.starts_with("postgres://operations_audit_publisher:")
        || !value.ends_with("/operations")
    {
        return Err(StoreError::Scope(
            "audit database URL does not match the publisher role/database contract".into(),
        ));
    }
    Ok(value.to_string())
}

pub async fn validate(pool: &PgPool, expected: &ExpectedBinding<'_>) -> Result<(), StoreError> {
    validate_identity_and_binding(
        pool,
        expected,
        EXPECTED_RUNTIME_ROLE,
        EXPECTED_SCHEMA,
        "audit-store",
        MIGRATION_ID,
    )
    .await
}

async fn validate_identity_and_binding(
    pool: &PgPool,
    expected: &ExpectedBinding<'_>,
    role: &str,
    schema: &str,
    owner: &str,
    migration_id: &str,
) -> Result<(), StoreError> {
    let row = sqlx::query(
        "SELECT current_database() AS database_name,current_user AS role_name,
                has_database_privilege(current_user,current_database(),'CREATE') AS database_create",
    )
    .fetch_one(pool)
    .await?;
    if row.try_get::<String, _>("database_name")? != EXPECTED_DATABASE
        || row.try_get::<String, _>("role_name")? != role
        || row.try_get::<bool, _>("database_create")?
    {
        return Err(StoreError::Scope("audit database identity mismatch".into()));
    }
    let create: bool = sqlx::query_scalar("SELECT has_schema_privilege(current_user,$1,'CREATE')")
        .bind(schema)
        .fetch_one(pool)
        .await?;
    if create {
        return Err(StoreError::Scope(
            "audit runtime can create schema objects".into(),
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
        || binding
            .try_get::<Option<String>, _>("environment")?
            .is_some()
        || binding.try_get::<i64, _>("schema_contract_generation")?
            < expected.minimum_schema_generation
    {
        return Err(StoreError::Scope(
            "audit operational binding mismatch".into(),
        ));
    }
    let applied: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner=$1 AND schema_name=$2 AND migration_id=$3",
    )
    .bind(owner)
    .bind(schema)
    .bind(migration_id)
    .fetch_one(pool)
    .await?;
    if applied != 1 {
        return Err(StoreError::Scope("audit migration is not applied".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redaction_rejects_prohibited_fields_at_any_depth() {
        assert!(reject_prohibited_fields(&json!({"status": "denied"})).is_ok());
        assert!(matches!(
            reject_prohibited_fields(&json!({"safe": [{"authorization": "secret"}]})),
            Err(StoreError::ProhibitedField(_))
        ));
        assert!(matches!(
            reject_prohibited_fields(&json!({"requestBody": "payload"})),
            Err(StoreError::ProhibitedField(_))
        ));
    }
}
