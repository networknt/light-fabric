//! Bounded operational evidence for `light-gateway`.
//!
//! The public record type deliberately cannot represent headers, credentials,
//! request/response bodies, prompts, messages, tool arguments, or artifacts.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::path::Path;
use std::time::Duration;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "gateway_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_gateway_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/gateway-database-url";
pub const MIGRATION_ID: &str = "0001_gateway_evidence_spool";
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/gateway-postgres/0001_gateway_evidence_spool.sql");
pub const AUTHORITY_TABLES: &[&str] = &["gateway_evidence_quota_t", "gateway_evidence_spool_t"];

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EvidenceClass {
    RequiredAudit,
    Traffic,
}

impl EvidenceClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::RequiredAudit => "REQUIRED_AUDIT",
            Self::Traffic => "TRAFFIC",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceRecord {
    pub event_id: Uuid,
    pub event_class: EvidenceClass,
    pub event_type: String,
    pub method: String,
    pub endpoint: String,
    pub status_code: u16,
    pub duration_micros: u64,
    pub request_bytes: u64,
    pub response_bytes: u64,
    pub correlation_digest: Option<String>,
    pub principal_digest: Option<String>,
    pub policy_digest: Option<String>,
    pub handler_digest: Option<String>,
    pub occurred_at: DateTime<Utc>,
}

impl EvidenceRecord {
    pub fn digest(&self, host_id: Uuid, gateway_instance: &str) -> String {
        let canonical = format!(
            "{host_id}|{gateway_instance}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.event_id,
            self.event_class.as_str(),
            self.event_type,
            self.method,
            self.endpoint,
            self.status_code,
            self.duration_micros,
            self.request_bytes,
            self.response_bytes,
            self.correlation_digest.as_deref().unwrap_or(""),
            self.principal_digest.as_deref().unwrap_or(""),
            self.policy_digest.as_deref().unwrap_or(""),
            self.handler_digest.as_deref().unwrap_or(""),
            self.occurred_at.to_rfc3339(),
        );
        format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionOutcome {
    Persisted,
    DroppedOptional,
}

#[derive(Debug, Clone)]
pub struct SpoolLimits {
    pub maximum_pending_records: i64,
    pub maximum_pending_bytes: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimedEvidence {
    pub host_id: Uuid,
    pub event_id: Uuid,
    pub gateway_instance: String,
    pub event_class: String,
    pub event_type: String,
    pub method: String,
    pub endpoint: String,
    pub status_code: i32,
    pub duration_micros: i64,
    pub request_bytes: i64,
    pub response_bytes: i64,
    pub correlation_digest: Option<String>,
    pub principal_digest: Option<String>,
    pub policy_digest: Option<String>,
    pub handler_digest: Option<String>,
    pub evidence_digest: String,
    #[serde(skip)]
    pub record_bytes: i32,
    #[serde(skip)]
    pub leased_by: String,
    #[serde(skip)]
    pub attempt: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("gateway evidence database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Runtime(#[from] operational_store::runtime::RuntimeValidationError),
    #[error("gateway evidence scope validation failed: {0}")]
    Scope(String),
    #[error("required audit spool is full")]
    SpoolFull,
    #[error("gateway evidence sink failed: {0}")]
    Sink(String),
}

#[derive(Clone)]
pub struct Repository {
    pool: PgPool,
    host_id: Uuid,
    gateway_instance: String,
    limits: SpoolLimits,
}

impl Repository {
    pub fn new(
        pool: PgPool,
        host_id: Uuid,
        gateway_instance: impl Into<String>,
        limits: SpoolLimits,
    ) -> Result<Self, StoreError> {
        let gateway_instance = gateway_instance.into();
        if gateway_instance.trim().is_empty()
            || limits.maximum_pending_records <= 0
            || limits.maximum_pending_bytes <= 0
        {
            return Err(StoreError::Scope(
                "gateway instance and positive spool limits are required".into(),
            ));
        }
        Ok(Self {
            pool,
            host_id,
            gateway_instance,
            limits,
        })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn record(&self, record: &EvidenceRecord) -> Result<AdmissionOutcome, StoreError> {
        validate_record(record)?;
        let record_bytes = i32::try_from(
            serde_json::to_vec(record)
                .map_err(|error| {
                    StoreError::Scope(format!("cannot size gateway evidence: {error}"))
                })?
                .len(),
        )
        .map_err(|_| StoreError::Scope("gateway evidence is too large".into()))?;
        if !(1..=16_384).contains(&record_bytes) {
            return Err(StoreError::Scope(
                "gateway evidence exceeds the bounded record size".into(),
            ));
        }

        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO gateway_evidence_quota_t(host_id,maximum_pending_records,maximum_pending_bytes)
             VALUES($1,$2,$3) ON CONFLICT(host_id) DO NOTHING",
        )
        .bind(self.host_id)
        .bind(self.limits.maximum_pending_records)
        .bind(self.limits.maximum_pending_bytes)
        .execute(&mut *tx)
        .await?;
        let quota = sqlx::query(
            "SELECT maximum_pending_records,maximum_pending_bytes,pending_records,pending_bytes
               FROM gateway_evidence_quota_t WHERE host_id=$1 FOR UPDATE",
        )
        .bind(self.host_id)
        .fetch_one(&mut *tx)
        .await?;
        if quota.try_get::<i64, _>("maximum_pending_records")?
            != self.limits.maximum_pending_records
            || quota.try_get::<i64, _>("maximum_pending_bytes")?
                != self.limits.maximum_pending_bytes
        {
            return Err(StoreError::Scope(
                "configured spool limits do not match the active Host quota".into(),
            ));
        }
        let full = quota.try_get::<i64, _>("pending_records")? + 1
            > self.limits.maximum_pending_records
            || quota.try_get::<i64, _>("pending_bytes")? + i64::from(record_bytes)
                > self.limits.maximum_pending_bytes;
        if full {
            if record.event_class == EvidenceClass::Traffic {
                sqlx::query(
                    "UPDATE gateway_evidence_quota_t
                     SET dropped_optional_records=dropped_optional_records+1,updated_ts=now()
                     WHERE host_id=$1",
                )
                .bind(self.host_id)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(AdmissionOutcome::DroppedOptional);
            }
            return Err(StoreError::SpoolFull);
        }

        sqlx::query(
            "INSERT INTO gateway_evidence_spool_t(
               host_id,event_id,gateway_instance,event_class,event_type,method,endpoint,status_code,
               duration_micros,request_bytes,response_bytes,correlation_digest,principal_digest,
               policy_digest,handler_digest,evidence_digest,record_bytes)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)",
        )
        .bind(self.host_id)
        .bind(record.event_id)
        .bind(&self.gateway_instance)
        .bind(record.event_class.as_str())
        .bind(&record.event_type)
        .bind(&record.method)
        .bind(&record.endpoint)
        .bind(i32::from(record.status_code))
        .bind(i64::try_from(record.duration_micros).unwrap_or(i64::MAX))
        .bind(i64::try_from(record.request_bytes).unwrap_or(i64::MAX))
        .bind(i64::try_from(record.response_bytes).unwrap_or(i64::MAX))
        .bind(&record.correlation_digest)
        .bind(&record.principal_digest)
        .bind(&record.policy_digest)
        .bind(&record.handler_digest)
        .bind(record.digest(self.host_id, &self.gateway_instance))
        .bind(record_bytes)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE gateway_evidence_quota_t
             SET pending_records=pending_records+1,pending_bytes=pending_bytes+$2,updated_ts=now()
             WHERE host_id=$1",
        )
        .bind(self.host_id)
        .bind(i64::from(record_bytes))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AdmissionOutcome::Persisted)
    }

    pub async fn claim(
        &self,
        publisher: &str,
        maximum_records: i64,
        lease: Duration,
    ) -> Result<Vec<ClaimedEvidence>, StoreError> {
        if publisher.trim().is_empty() || maximum_records <= 0 {
            return Err(StoreError::Scope(
                "publisher and positive batch size are required".into(),
            ));
        }
        let lease_expires = Utc::now()
            + ChronoDuration::from_std(lease)
                .map_err(|_| StoreError::Scope("publisher lease is invalid".into()))?;
        let rows = sqlx::query(
            "WITH candidates AS (
               SELECT host_id,event_id FROM gateway_evidence_spool_t
                WHERE host_id=$1 AND (
                  (state='PENDING' AND next_attempt_ts<=now()) OR
                  (state='PUBLISHING' AND lease_expires_ts<now()))
                ORDER BY created_ts FOR UPDATE SKIP LOCKED LIMIT $2
             )
             UPDATE gateway_evidence_spool_t s
                SET state='PUBLISHING',leased_by=$3,lease_expires_ts=$4,attempt=attempt+1
               FROM candidates c
              WHERE s.host_id=c.host_id AND s.event_id=c.event_id
             RETURNING s.host_id,s.event_id,s.gateway_instance,s.event_class,s.event_type,
               s.method,s.endpoint,s.status_code,s.duration_micros,s.request_bytes,s.response_bytes,
               s.correlation_digest,s.principal_digest,s.policy_digest,s.handler_digest,
               s.evidence_digest,s.record_bytes,s.leased_by,s.attempt",
        )
        .bind(self.host_id)
        .bind(maximum_records)
        .bind(publisher)
        .bind(lease_expires)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ClaimedEvidence {
                    host_id: row.try_get("host_id")?,
                    event_id: row.try_get("event_id")?,
                    gateway_instance: row.try_get("gateway_instance")?,
                    event_class: row.try_get("event_class")?,
                    event_type: row.try_get("event_type")?,
                    method: row.try_get("method")?,
                    endpoint: row.try_get("endpoint")?,
                    status_code: row.try_get("status_code")?,
                    duration_micros: row.try_get("duration_micros")?,
                    request_bytes: row.try_get("request_bytes")?,
                    response_bytes: row.try_get("response_bytes")?,
                    correlation_digest: row.try_get("correlation_digest")?,
                    principal_digest: row.try_get("principal_digest")?,
                    policy_digest: row.try_get("policy_digest")?,
                    handler_digest: row.try_get("handler_digest")?,
                    evidence_digest: row.try_get("evidence_digest")?,
                    record_bytes: row.try_get("record_bytes")?,
                    leased_by: row.try_get("leased_by")?,
                    attempt: row.try_get("attempt")?,
                })
            })
            .collect::<Result<Vec<_>, sqlx::Error>>()
            .map_err(StoreError::from)
    }

    pub async fn delivered(&self, records: &[ClaimedEvidence]) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        for record in records {
            if record.host_id != self.host_id {
                return Err(StoreError::Scope(
                    "cross-Host delivery acknowledgement".into(),
                ));
            }
        }
        let mut tx = self.pool.begin().await?;
        let mut bytes = 0_i64;
        for record in records {
            let delivered_bytes: Option<i32> = sqlx::query_scalar(
                "UPDATE gateway_evidence_spool_t SET state='DELIVERED',delivered_ts=now(),
                   leased_by=NULL,lease_expires_ts=NULL,last_error_code=NULL
                 WHERE host_id=$1 AND event_id=$2 AND state='PUBLISHING'
                   AND leased_by=$3 AND attempt=$4
                 RETURNING record_bytes",
            )
            .bind(self.host_id)
            .bind(record.event_id)
            .bind(&record.leased_by)
            .bind(record.attempt)
            .fetch_optional(&mut *tx)
            .await?;
            let delivered_bytes = delivered_bytes.ok_or_else(|| {
                StoreError::Scope("stale Gateway evidence delivery acknowledgement".into())
            })?;
            bytes += i64::from(delivered_bytes);
        }
        sqlx::query(
            "UPDATE gateway_evidence_quota_t SET
               pending_records=pending_records-$2,pending_bytes=pending_bytes-$3,updated_ts=now()
             WHERE host_id=$1",
        )
        .bind(self.host_id)
        .bind(i64::try_from(records.len()).unwrap_or(i64::MAX))
        .bind(bytes)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn retry(
        &self,
        records: &[ClaimedEvidence],
        error_code: &str,
        retry_after: Duration,
    ) -> Result<(), StoreError> {
        if error_code.trim().is_empty() || error_code.len() > 128 {
            return Err(StoreError::Scope("bounded error code is required".into()));
        }
        let next_attempt = Utc::now()
            + ChronoDuration::from_std(retry_after)
                .map_err(|_| StoreError::Scope("retry delay is invalid".into()))?;
        for record in records {
            if record.host_id != self.host_id {
                return Err(StoreError::Scope("cross-Host retry acknowledgement".into()));
            }
        }
        let mut tx = self.pool.begin().await?;
        for record in records {
            let changed = sqlx::query(
                "UPDATE gateway_evidence_spool_t SET state='PENDING',next_attempt_ts=$3,
                   leased_by=NULL,lease_expires_ts=NULL,last_error_code=$4
                 WHERE host_id=$1 AND event_id=$2 AND state='PUBLISHING'
                   AND leased_by=$5 AND attempt=$6",
            )
            .bind(self.host_id)
            .bind(record.event_id)
            .bind(next_attempt)
            .bind(error_code)
            .bind(&record.leased_by)
            .bind(record.attempt)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if changed != 1 {
                return Err(StoreError::Scope(
                    "stale Gateway evidence retry acknowledgement".into(),
                ));
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn purge_delivered_before(&self, cutoff: DateTime<Utc>) -> Result<u64, StoreError> {
        Ok(sqlx::query(
            "DELETE FROM gateway_evidence_spool_t
             WHERE host_id=$1 AND state='DELIVERED' AND delivered_ts<$2",
        )
        .bind(self.host_id)
        .bind(cutoff)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}

#[derive(Clone)]
pub struct HttpPublisher {
    client: reqwest::Client,
    endpoint: String,
    bearer_token: Option<String>,
}

impl HttpPublisher {
    pub fn new(
        endpoint: impl Into<String>,
        bearer_token: Option<String>,
    ) -> Result<Self, StoreError> {
        let endpoint = endpoint.into();
        if endpoint != "stdout://collector"
            && !(endpoint.starts_with("http://") || endpoint.starts_with("https://"))
        {
            return Err(StoreError::Scope(
                "sink endpoint must be stdout://collector or an HTTP(S) URL".into(),
            ));
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .redirect(Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|error| StoreError::Sink(error.to_string()))?,
            endpoint,
            bearer_token,
        })
    }

    pub async fn publish(&self, records: &[ClaimedEvidence]) -> Result<(), StoreError> {
        if self.endpoint == "stdout://collector" {
            for record in records {
                tracing::info!(
                    target: "light_gateway::traffic_evidence",
                    evidence = %serde_json::to_string(record).map_err(|error| StoreError::Sink(error.to_string()))?,
                    "gateway evidence delivered to the structured-log collector"
                );
            }
            return Ok(());
        }
        let mut request = self.client.post(&self.endpoint).json(records);
        if let Some(token) = self.bearer_token.as_ref() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|error| StoreError::Sink(error.to_string()))?;
        if !response.status().is_success() {
            return Err(StoreError::Sink(format!(
                "collector returned HTTP {}",
                response.status().as_u16()
            )));
        }
        Ok(())
    }
}

fn validate_record(record: &EvidenceRecord) -> Result<(), StoreError> {
    if record.event_type.trim().is_empty()
        || record.event_type.len() > 128
        || record.method.is_empty()
        || record.method.len() > 16
        || !record.method.bytes().all(|byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
        || record.endpoint.is_empty()
        || record.endpoint.len() > 1024
        || record.endpoint.contains('?')
        || !(100..=599).contains(&record.status_code)
    {
        return Err(StoreError::Scope("invalid bounded gateway evidence".into()));
    }
    for digest in [
        record.correlation_digest.as_deref(),
        record.principal_digest.as_deref(),
        record.policy_digest.as_deref(),
        record.handler_digest.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if !valid_digest(digest) {
            return Err(StoreError::Scope("invalid gateway evidence digest".into()));
        }
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

pub fn read_secret(path: &Path, label: &str, maximum_bytes: usize) -> Result<String, StoreError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| StoreError::Scope(format!("cannot inspect {label}: {error}")))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StoreError::Scope(format!(
            "{label} must be a regular non-symlink file"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(StoreError::Scope(format!(
                "{label} permissions are too broad"
            )));
        }
    }
    let value = std::fs::read_to_string(path)
        .map_err(|error| StoreError::Scope(format!("cannot read {label}: {error}")))?;
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.len() > maximum_bytes || value.contains(['\r', '\n']) {
        return Err(StoreError::Scope(format!("{label} is empty or malformed")));
    }
    Ok(value.to_string())
}

pub fn read_database_url(path: &Path, server_host: &str, port: u16, tls_mode: &str,
                         expected_database: &str) -> Result<String, StoreError> {
    Ok(operational_store::runtime::read_database_url(
        path,
        server_host,
        port,
        tls_mode,
        expected_database,
        "gateway_runtime",
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
            role_suffix: "gateway_runtime",
            minimum_schema_generation: expected.minimum_schema_generation,
        },
    )
    .await?;
    let identity = sqlx::query(
        "SELECT has_schema_privilege(current_user,'gateway_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<bool, _>("schema_create")? {
        return Err(StoreError::Scope(
            "Gateway runtime role must not have CREATE authority".into(),
        ));
    }
    let migration = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM operational_meta.operational_schema_migration_t
         WHERE migration_owner='gateway-operational-store' AND schema_name='gateway_ops'
           AND migration_id=$1",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if migration != 1 {
        return Err(StoreError::Scope("gateway migration is not applied".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_type_has_no_content_or_credential_fields() {
        let json = serde_json::to_value(EvidenceRecord {
            event_id: Uuid::nil(),
            event_class: EvidenceClass::Traffic,
            event_type: "gateway.request.completed".into(),
            method: "GET".into(),
            endpoint: "/health".into(),
            status_code: 200,
            duration_micros: 1,
            request_bytes: 0,
            response_bytes: 2,
            correlation_digest: None,
            principal_digest: None,
            policy_digest: None,
            handler_digest: None,
            occurred_at: Utc::now(),
        })
        .unwrap();
        let rendered = json.to_string().to_ascii_lowercase();
        for forbidden in [
            "authorization",
            "cookie",
            "password",
            "secret",
            "prompt",
            "message",
            "arguments",
            "requestbody",
            "responsebody",
        ] {
            assert!(!rendered.contains(forbidden));
        }
    }
}
