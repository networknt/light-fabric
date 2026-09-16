//! Bounded manager chunks arrive through Controller-accepted Agent results.
//! Workflow alone stages/promotes bytes; durable job reports are the retry log.
use crate::{
    artifact_publish::{ArtifactPublication, publish_artifact_in_transaction},
    artifact_store::DurableArtifactStore,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
use task_workspace::{SNAPSHOT_CHUNK_BYTES, SnapshotChunk, SnapshotPackage, SnapshotReceipt};
use uuid::Uuid;
use workspace_execution_protocol::ManagerSnapshotRead;

type Error = Box<dyn std::error::Error + Send + Sync>;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChunkResult {
    pub manager_snapshot: ManagerSnapshotRead,
    pub workspace_id: String,
    pub task_id: String,
    pub receipt: SnapshotReceipt,
    pub chunk: SnapshotChunk,
}

impl ChunkResult {
    pub fn validate(&self) -> Result<(), Error> {
        self.manager_snapshot.validate()?;
        let c = &self.chunk;
        let r = &self.receipt;
        if r.snapshot_id != self.manager_snapshot.snapshot_id
            || r.checkpoint_digest != self.manager_snapshot.checkpoint_digest
            || c.package_digest != r.package_digest
            || c.total_bytes != r.package_bytes
            || !workspace_execution_protocol::digest(&r.package_digest)
            || c.total_bytes == 0
            || c.total_bytes > 32 * 1024 * 1024
            || c.offset != self.manager_snapshot.offset
            || c.offset >= c.total_bytes
            || c.bytes.len() != ((c.total_bytes - c.offset) as usize).min(SNAPSHOT_CHUNK_BYTES)
            || self
                .manager_snapshot
                .package_digest
                .as_ref()
                .is_some_and(|d| d != &r.package_digest)
        {
            return Err("invalid snapshot chunk bounds or binding".into());
        }
        Ok(())
    }
}

/// Pure assembly gate: accepts duplicates only when byte-identical and refuses
/// inconsistent receipts, missing offsets, tampered package bytes or Git trees.
pub fn assemble(parts: &[ChunkResult]) -> Result<Option<Vec<u8>>, Error> {
    let first = parts.first().ok_or("empty snapshot transfer")?;
    if parts.len() > 257 {
        return Err("snapshot transfer exceeds chunk bound".into());
    }
    let mut chunks: BTreeMap<u64, &Vec<u8>> = BTreeMap::new();
    for p in parts {
        p.validate()?;
        if p.receipt != first.receipt
            || p.workspace_id != first.workspace_id
            || p.task_id != first.task_id
            || p.manager_snapshot.feature_id != first.manager_snapshot.feature_id
            || p.manager_snapshot.stage_id != first.manager_snapshot.stage_id
        {
            return Err("snapshot transfer identity changed".into());
        }
        if chunks
            .insert(p.chunk.offset, &p.chunk.bytes)
            .is_some_and(|old| old != &p.chunk.bytes)
        {
            return Err("snapshot chunk replay changed".into());
        }
    }
    let mut bytes = Vec::new();
    for (offset, chunk) in chunks {
        if offset != bytes.len() as u64 {
            return Ok(None);
        }
        bytes.extend_from_slice(chunk);
    }
    if bytes.len() as u64 != first.receipt.package_bytes {
        return Ok(None);
    }
    if format!("sha256:{:x}", Sha256::digest(&bytes)) != first.receipt.package_digest {
        return Err("snapshot package digest mismatch".into());
    }
    let package: SnapshotPackage = serde_json::from_slice(&bytes)?;
    if package.workspace_id != first.workspace_id
        || package.task_id != first.task_id
        || package.feature_id != first.manager_snapshot.feature_id
        || package.stage_id != first.manager_snapshot.stage_id
        || package.verified_receipt()? != first.receipt
    {
        return Err("snapshot package identity or tree mismatch".into());
    }
    Ok(Some(bytes))
}

pub async fn accept(
    tx: &mut Transaction<'_, Postgres>,
    store: Option<&DurableArtifactStore>,
    host: Uuid,
    job: Uuid,
    output: &Value,
) -> Result<Value, Error> {
    let row=sqlx::query("SELECT j.input,j.workflow_process_id,j.workflow_task_id,t.task_policy_digest FROM workflow_agent_job_t j JOIN task_info_t t ON t.host_id=j.host_id AND t.task_id=j.workflow_task_id WHERE j.host_id=$1 AND j.job_id=$2")
        .bind(host).bind(job).fetch_one(&mut **tx).await?;
    let input: Value = row.get("input");
    let Some(request) = input.get("managerSnapshot") else {
        return Ok(output.clone());
    };
    let current: ChunkResult = serde_json::from_value(output.clone())?;
    current.validate()?;
    if current.manager_snapshot != decode_request(request)?
        || input
            .pointer("/workspace/workspaceId")
            .and_then(Value::as_str)
            != Some(current.workspace_id.as_str())
        || input
            .pointer("/workspace/task/taskId")
            .and_then(Value::as_str)
            != Some(current.task_id.as_str())
    {
        return Err("snapshot result differs from dispatched fixed request".into());
    }
    let process: Uuid = row.get("workflow_process_id");
    let active:bool=sqlx::query_scalar("SELECT f.record->>'state'='active' FROM development_stage_t s JOIN development_feature_t f ON f.host_id=s.host_id AND f.feature_id=s.feature_id WHERE s.host_id=$1 AND s.process_id=$2 AND s.feature_id=$3 AND s.receipt->>'stageExecutionId'=$4")
        .bind(host).bind(process).bind(&current.manager_snapshot.feature_id).bind(&current.manager_snapshot.stage_id)
        .fetch_one(&mut **tx).await?;
    if !active {
        return Ok(json!({"state":"CANCELLED","snapshotNotPublished":true}));
    }
    let reports:Vec<Value>=sqlx::query_scalar("SELECT report FROM workflow_agent_job_t WHERE host_id=$1 AND workflow_process_id=$2 AND job_id<>$3 AND state='SUCCEEDED' AND input->'managerSnapshot'->>'snapshotId'=$4 AND report IS NOT NULL ORDER BY job_id LIMIT 257")
        .bind(host).bind(process).bind(job).bind(&current.manager_snapshot.snapshot_id).fetch_all(&mut **tx).await?;
    let mut parts = Vec::with_capacity(reports.len() + 1);
    for report in reports {
        let normalized: execution_runner_protocol::NormalizedExecutionResult =
            serde_json::from_value(
                report
                    .pointer("/output/result")
                    .cloned()
                    .ok_or("snapshot report result missing")?,
            )?;
        parts.push(serde_json::from_value(
            normalized
                .structured_output
                .ok_or("snapshot report output missing")?,
        )?);
    }
    parts.push(current.clone());
    let bytes = assemble(&parts)?;
    let mut result = json!({"receipt":current.receipt,"nextOffset":current.chunk.offset+current.chunk.bytes.len() as u64,
        "transferComplete":false});
    if let Some(bytes) = bytes {
        let store = store.ok_or("Workflow artifact store unavailable")?;
        let hash = Sha256::digest(serde_json::to_vec(&(
            host,
            process,
            &current.manager_snapshot.snapshot_id,
        ))?);
        let artifact = Uuid::from_slice(&hash[..16])?;
        let execution:Uuid=sqlx::query_scalar("SELECT execution_id FROM development_execution_fence_t WHERE host_id=$1 AND task_id=$2")
            .bind(host).bind(job).fetch_one(&mut **tx).await?;
        let policy: String = row.get("task_policy_digest");
        let digest = publish_artifact_in_transaction(
            tx,
            store,
            ArtifactPublication {
                host_id: host,
                artifact_id: artifact,
                execution_id: execution,
                process_id: Some(process),
                task_id: Some(row.get("workflow_task_id")),
                logical_name: &format!("snapshot:{}", current.receipt.snapshot_id),
                media_type: "application/json",
                producer: "workflow-manager-snapshot",
                policy_digest: &policy,
                retain_until: chrono::Utc::now() + chrono::Duration::days(90),
                bytes: &bytes,
            },
        )
        .await?;
        // Stage acceptance consumes independently addressable repository manifests.
        // Publish them under the same transaction as the package and job report:
        // failure leaves no accepted partial candidate, and report replay uses
        // the same identities without another manager or model turn.
        let package: SnapshotPackage = serde_json::from_slice(&bytes)?;
        let definition: Value = sqlx::query_scalar(
            "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&mut **tx)
        .await?;
        if let Some(checks) = crate::snapshot_validation::evaluate(&definition, &package)? {
            let mut validation = development_workflow_contract::ValidationReceipt {
                candidate: digest.clone(),
                required_checks: checks.keys().cloned().collect(),
                passed_checks: BTreeMap::new(),
            };
            let mut failures = Vec::new();
            for (name, passed) in checks {
                let evidence =
                    serde_json::to_vec(&json!({"candidate":digest,"check":name,"passed":passed}))?;
                let hash = Sha256::digest(serde_json::to_vec(&(
                    "snapshot-document-check/v1",
                    host,
                    process,
                    &current.manager_snapshot.snapshot_id,
                    &name,
                ))?);
                let id = Uuid::from_slice(&hash[..16])?;
                let check_digest = publish_artifact_in_transaction(
                    tx,
                    store,
                    ArtifactPublication {
                        host_id: host,
                        artifact_id: id,
                        execution_id: execution,
                        process_id: Some(process),
                        task_id: Some(row.get("workflow_task_id")),
                        logical_name: &format!(
                            "snapshot:{}:check:{}",
                            current.receipt.snapshot_id, name
                        ),
                        media_type: "application/json",
                        producer: "workflow-fixed-document-check",
                        policy_digest: &policy,
                        retain_until: chrono::Utc::now() + chrono::Duration::days(90),
                        bytes: &evidence,
                    },
                )
                .await?;
                if passed {
                    validation.passed_checks.insert(
                        name,
                        development_workflow_contract::ArtifactRef {
                            id: id.to_string(),
                            digest: check_digest,
                        },
                    );
                } else {
                    failures.push(name);
                }
            }
            result["validation"] = serde_json::to_value(validation)?;
            result["validationFailures"] = json!(failures);
        }
        let mut repositories = BTreeMap::new();
        for (name, repository) in &package.repositories {
            let manifest_bytes = serde_json::to_vec(repository)?;
            let manifest_hash = Sha256::digest(serde_json::to_vec(&(
                "snapshot-repository/v1",
                host,
                process,
                &current.manager_snapshot.snapshot_id,
                name,
            ))?);
            let manifest_id = Uuid::from_slice(&manifest_hash[..16])?;
            let manifest_digest = publish_artifact_in_transaction(
                tx,
                store,
                ArtifactPublication {
                    host_id: host,
                    artifact_id: manifest_id,
                    execution_id: execution,
                    process_id: Some(process),
                    task_id: Some(row.get("workflow_task_id")),
                    logical_name: &format!(
                        "snapshot:{}:repository:{}",
                        current.receipt.snapshot_id, name
                    ),
                    media_type: "application/json",
                    producer: "workflow-manager-snapshot",
                    policy_digest: &policy,
                    retain_until: chrono::Utc::now() + chrono::Duration::days(90),
                    bytes: &manifest_bytes,
                },
            )
            .await?;
            repositories.insert(
                name.clone(),
                development_workflow_contract::RepositorySnapshot {
                    base_commit: repository.base_commit.clone(),
                    tree: repository.tree.clone(),
                    content_manifest: development_workflow_contract::ArtifactRef {
                        id: manifest_id.to_string(),
                        digest: manifest_digest,
                    },
                },
            );
        }
        let candidate = development_workflow_contract::CandidateSnapshot {
            feature_run_id: package.feature_id,
            stage_execution_id: package.stage_id,
            task_id: package.task_id,
            candidate_digest: digest.clone(),
            checkpoint_digest: current.receipt.checkpoint_digest.clone(),
            repositories,
            package: development_workflow_contract::ArtifactRef {
                id: artifact.to_string(),
                digest: digest.clone(),
            },
        };
        candidate.validate()?;
        result["transferComplete"] = json!(true);
        result["artifact"] = json!({"id":artifact,"digest":digest});
        result["candidate"] = serde_json::to_value(candidate)?;
    }
    Ok(result)
}

// Optional first-chunk packageDigest may be absent or null on the wire. Compare
// the strict typed contract rather than JSON spelling; all identities stay exact.
fn decode_request(value: &Value) -> Result<ManagerSnapshotRead, Error> {
    let request: ManagerSnapshotRead = serde_json::from_value(value.clone())?;
    request.validate()?;
    Ok(request)
}

#[cfg(test)]
mod request_tests {
    use super::*;
    #[test]
    fn first_chunk_optional_digest_round_trips_and_identity_stays_exact() {
        let mut wire = json!({"featureId":"feature","stageId":"stage","snapshotId":"snapshot",
            "checkpointDigest":format!("sha256:{}","a".repeat(64)),"offset":0});
        let original = decode_request(&wire).unwrap();
        wire["packageDigest"] = Value::Null;
        assert_eq!(original, decode_request(&wire).unwrap());
        wire["stageId"] = json!("other");
        assert_ne!(original, decode_request(&wire).unwrap());
        wire["offset"] = json!(128 * 1024);
        assert!(decode_request(&wire).is_err());
        wire["offset"] = json!(0);
        wire["unknown"] = json!(true);
        assert!(decode_request(&wire).is_err());
    }
}
