use async_trait::async_trait;
use chrono::{DateTime, Utc};
use execution_runner_protocol::ArtifactEvidence;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::artifact_retention::ArtifactStoreError;

#[async_trait]
pub trait ArtifactPublisherStore: Send + Sync {
    /// Stage with a provider-native short TTL. The reference grants no read authority.
    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<String, ArtifactStoreError>;
    /// Atomically bind/copy staged bytes to a content-addressed durable reference.
    async fn promote(
        &self,
        namespace: &str,
        staging: &str,
        digest: &str,
    ) -> Result<String, ArtifactStoreError>;
}

pub struct ArtifactPublication<'a> {
    pub host_id: Uuid,
    pub artifact_id: Uuid,
    pub execution_id: Uuid,
    pub process_id: Option<Uuid>,
    pub task_id: Option<Uuid>,
    pub logical_name: &'a str,
    pub media_type: &'a str,
    pub producer: &'a str,
    pub policy_digest: &'a str,
    pub retain_until: DateTime<Utc>,
    pub bytes: &'a [u8],
}

/// Publishes bytes with a crash-safe stage -> metadata -> bind protocol. A crash
/// before metadata commit leaves only a native-TTL staging object; a crash after
/// commit is recoverable from `promotion_state='METADATA_COMMITTED'`.
pub async fn publish_artifact<S: ArtifactPublisherStore>(
    pool: &PgPool,
    store: &S,
    artifact: ArtifactPublication<'_>,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(artifact.bytes)));
    let staging = store
        .stage(
            &format!(
                "{}/{}/{}",
                artifact.host_id, artifact.execution_id, artifact.artifact_id
            ),
            artifact.bytes,
        )
        .await?;
    sqlx::query("INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,process_id,task_id,logical_name,media_type,size_bytes,content_digest,storage_reference,staging_reference,promotion_state,producer,policy_digest,retain_until_ts,verification_state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,'METADATA_COMMITTED',$11,$12,$13,'PENDING') ON CONFLICT(host_id,artifact_id) DO NOTHING")
        .bind(artifact.host_id).bind(artifact.artifact_id).bind(artifact.execution_id)
        .bind(artifact.process_id).bind(artifact.task_id).bind(artifact.logical_name)
        .bind(artifact.media_type).bind(artifact.bytes.len() as i64).bind(&digest)
        .bind(&staging).bind(artifact.producer).bind(artifact.policy_digest)
        .bind(artifact.retain_until).execute(pool).await?;
    let durable = store
        .promote(&artifact.host_id.to_string(), &staging, &digest)
        .await?;
    let result = sqlx::query("UPDATE workflow_artifact_t SET storage_reference=$3,promotion_state='BOUND',verification_state='VERIFIED',updated_ts=now() WHERE host_id=$1 AND artifact_id=$2 AND content_digest=$4 AND promotion_state IN ('METADATA_COMMITTED','BOUND')")
        .bind(artifact.host_id).bind(artifact.artifact_id).bind(&durable).bind(&digest)
        .execute(pool).await?;
    if result.rows_affected() != 1 {
        return Err("artifact metadata binding was fenced or digest changed".into());
    }
    Ok(digest)
}

/// Verifies and binds a runner-staged object before origin acceptance. The
/// deterministic artifact ID makes notification/catch-up retries idempotent.
pub async fn promote_artifact_evidence<S: ArtifactPublisherStore>(
    pool: &PgPool,
    store: &S,
    host_id: Uuid,
    execution_id: Uuid,
    process_id: Uuid,
    task_id: Uuid,
    policy_digest: &str,
    retain_until: DateTime<Utc>,
    artifact: &ArtifactEvidence,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let artifact_id = deterministic_artifact_id(execution_id, artifact);
    let size =
        i64::try_from(artifact.size).map_err(|_| "artifact size exceeds PostgreSQL bigint")?;
    sqlx::query("INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,process_id,task_id,logical_name,media_type,size_bytes,content_digest,storage_reference,staging_reference,promotion_state,producer,policy_digest,retain_until_ts,verification_state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,'METADATA_COMMITTED','light-workflow-runner',$11,$12,'PENDING') ON CONFLICT(host_id,artifact_id) DO NOTHING")
        .bind(host_id).bind(artifact_id).bind(execution_id).bind(process_id).bind(task_id)
        .bind(&artifact.logical_name).bind(&artifact.media_type).bind(size).bind(&artifact.digest)
        .bind(&artifact.reference).bind(policy_digest).bind(retain_until).execute(pool).await?;
    let existing: (String, i64, String, String) = sqlx::query_as(
        "SELECT content_digest,size_bytes,staging_reference,promotion_state
         FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2",
    )
    .bind(host_id)
    .bind(artifact_id)
    .fetch_one(pool)
    .await?;
    if existing.0 != artifact.digest || existing.1 != size || existing.2 != artifact.reference {
        sqlx::query("UPDATE workflow_artifact_t SET promotion_state='QUARANTINED',verification_state='REJECTED',updated_ts=now() WHERE host_id=$1 AND artifact_id=$2")
            .bind(host_id).bind(artifact_id).execute(pool).await?;
        return Err("artifact evidence changed across reconciliation".into());
    }
    if existing.3 == "BOUND" {
        return Ok(());
    }
    let durable = match store
        .promote(&host_id.to_string(), &artifact.reference, &artifact.digest)
        .await
    {
        Ok(reference) => reference,
        Err(error) => {
            if !error.retryable {
                sqlx::query("UPDATE workflow_artifact_t SET promotion_state='QUARANTINED',verification_state='REJECTED',updated_ts=now() WHERE host_id=$1 AND artifact_id=$2")
                    .bind(host_id).bind(artifact_id).execute(pool).await?;
            }
            return Err(Box::new(error));
        }
    };
    let updated = sqlx::query("UPDATE workflow_artifact_t SET storage_reference=$3,promotion_state='BOUND',verification_state='VERIFIED',updated_ts=now() WHERE host_id=$1 AND artifact_id=$2 AND content_digest=$4 AND promotion_state='METADATA_COMMITTED'")
        .bind(host_id).bind(artifact_id).bind(durable).bind(&artifact.digest).execute(pool).await?;
    if updated.rows_affected() != 1 {
        return Err("artifact promotion binding lost its metadata fence".into());
    }
    Ok(())
}

fn deterministic_artifact_id(execution_id: Uuid, artifact: &ArtifactEvidence) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(execution_id.as_bytes());
    hash.update(artifact.logical_name.as_bytes());
    hash.update(artifact.digest.as_bytes());
    let digest = hash.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // RFC 4122 variant with a private, deterministic version nibble.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
