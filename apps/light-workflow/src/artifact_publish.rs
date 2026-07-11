use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::artifact_retention::ArtifactStoreError;

#[async_trait]
pub trait ArtifactPublisherStore: Send + Sync {
    /// Stage with a provider-native short TTL. The reference grants no read authority.
    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<String, ArtifactStoreError>;
    /// Atomically bind/copy staged bytes to a content-addressed durable reference.
    async fn promote(&self, staging: &str, digest: &str) -> Result<String, ArtifactStoreError>;
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
            &format!("{}/{}", artifact.execution_id, artifact.artifact_id),
            artifact.bytes,
        )
        .await?;
    sqlx::query("INSERT INTO workflow_artifact_t(host_id,artifact_id,execution_id,process_id,task_id,logical_name,media_type,size_bytes,content_digest,storage_reference,staging_reference,promotion_state,producer,policy_digest,retain_until_ts,verification_state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$10,'METADATA_COMMITTED',$11,$12,$13,'PENDING') ON CONFLICT(host_id,artifact_id) DO NOTHING")
        .bind(artifact.host_id).bind(artifact.artifact_id).bind(artifact.execution_id)
        .bind(artifact.process_id).bind(artifact.task_id).bind(artifact.logical_name)
        .bind(artifact.media_type).bind(artifact.bytes.len() as i64).bind(&digest)
        .bind(&staging).bind(artifact.producer).bind(artifact.policy_digest)
        .bind(artifact.retain_until).execute(pool).await?;
    let durable = store.promote(&staging, &digest).await?;
    let result = sqlx::query("UPDATE workflow_artifact_t SET storage_reference=$3,promotion_state='BOUND',verification_state='VERIFIED',updated_ts=now() WHERE host_id=$1 AND artifact_id=$2 AND content_digest=$4 AND promotion_state IN ('METADATA_COMMITTED','BOUND')")
        .bind(artifact.host_id).bind(artifact.artifact_id).bind(&durable).bind(&digest)
        .execute(pool).await?;
    if result.rows_affected() != 1 {
        return Err("artifact metadata binding was fenced or digest changed".into());
    }
    Ok(digest)
}
