use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalDecision {
    pub host_id: Uuid,
    pub approval_id: Uuid,
    pub actor: String,
    pub reason: Option<String>,
    pub artifact_digest_set: Value,
    pub provenance_digest: Option<String>,
    pub target: String,
    pub operation: String,
    pub policy_digest: String,
}

#[derive(Clone)]
pub struct WorkflowApprovalService {
    pool: PgPool,
    origin_service_id: String,
    origin_instance_id: String,
}

impl WorkflowApprovalService {
    pub fn new(
        pool: PgPool,
        origin_service_id: impl Into<String>,
        origin_instance_id: impl Into<String>,
    ) -> Self {
        Self {
            pool,
            origin_service_id: origin_service_id.into(),
            origin_instance_id: origin_instance_id.into(),
        }
    }

    pub async fn approve(&self, decision: &ApprovalDecision) -> Result<Uuid, sqlx::Error> {
        if decision.actor.trim().is_empty() {
            return Err(sqlx::Error::Protocol(
                "approval actor must be authenticated".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let row: Option<(Uuid, Uuid, Uuid, Value, Option<String>, String, String, String, Value, Value, Uuid, Value)> = sqlx::query_as(
            "SELECT a.process_id, a.task_id, a.preceding_execution_id,
                    a.artifact_digest_set, a.provenance_digest, a.target, a.operation,
                    a.policy_digest, r.normalized_requirements, r.execution_spec,
                    r.policy_snapshot_id, e.normalized_result
             FROM workflow_approval_t a
             JOIN execution_attempt_t e ON e.host_id = a.host_id AND e.execution_id = a.preceding_execution_id
             JOIN runner_scheduling_request_t r ON r.host_id = e.host_id AND r.request_id = e.request_id
             WHERE a.host_id = $1 AND a.approval_id = $2 AND a.state = 'REQUESTED'
               AND a.expires_ts > CURRENT_TIMESTAMP
             FOR UPDATE OF a",
        )
        .bind(decision.host_id)
        .bind(decision.approval_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((
            process_id,
            task_id,
            preceding_execution_id,
            artifacts,
            provenance,
            target,
            operation,
            policy_digest,
            mut requirements,
            prior_spec,
            policy_snapshot_id,
            normalized_result,
        )) = row
        else {
            return Err(sqlx::Error::Protocol(
                "approval is absent, expired, or already decided".into(),
            ));
        };
        if artifacts != decision.artifact_digest_set
            || provenance != decision.provenance_digest
            || target != decision.target
            || operation != decision.operation
            || policy_digest != decision.policy_digest
        {
            return Err(sqlx::Error::Protocol(
                "approval decision does not match the immutable approved subject".into(),
            ));
        }
        if operation != "apply-patch" {
            return Err(sqlx::Error::Protocol(
                "this deployment implements only the apply-patch fixed action".into(),
            ));
        }
        self.revalidate_artifacts(
            &mut tx,
            decision.host_id,
            preceding_execution_id,
            &artifacts,
            provenance.as_deref(),
        )
        .await?;

        let request_id = Uuid::now_v7();
        if let Some(object) = requirements.as_object_mut() {
            object.insert(
                "actionKind".into(),
                Value::String(format!("fixed.{operation}")),
            );
            object.insert("requiredFeatures".into(), json!(["trusted-fixed-action"]));
        } else {
            return Err(sqlx::Error::Protocol(
                "approved execution requirements are not an object".into(),
            ));
        }
        let structured = normalized_result
            .get("structuredOutput")
            .unwrap_or(&normalized_result);
        let base_commit = structured
            .get("baseRevision")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                sqlx::Error::Protocol("approved fixed action lacks baseRevision".into())
            })?;
        let changed_paths = structured
            .get("changedPaths")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                sqlx::Error::Protocol("approved fixed action lacks changedPaths".into())
            })?;
        let (patch_artifact_reference, patch_digest): (String, String) = sqlx::query_as(
            "SELECT storage_reference, content_digest FROM workflow_artifact_t
             WHERE host_id=$1 AND execution_id=$2 AND verification_state='VERIFIED'
               AND promotion_state='BOUND' ORDER BY created_ts, artifact_id LIMIT 1",
        )
        .bind(decision.host_id)
        .bind(preceding_execution_id)
        .fetch_one(&mut *tx)
        .await?;
        let target_branch = structured
            .get("targetBranch")
            .and_then(Value::as_str)
            .unwrap_or("agent/approved");
        let repository_digest = execution_runner_protocol::canonical_sha256(&target)
            .map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
        let execution_spec = json!({
            "kind": "fixed-action",
            "operation": operation,
            "target": target,
            "artifactDigestSet": artifacts,
            "provenanceDigest": provenance,
            "policyDigest": policy_digest,
            "precedingExecutionId": preceding_execution_id,
            "repository": target,
            "repositoryDigest": repository_digest,
            "baseCommit": base_commit,
            "repositoryObjectFormat": if base_commit.len() == 64 { "sha256" } else { "sha1" },
            "targetBranch": target_branch,
            "patchArtifactReference": patch_artifact_reference,
            "patchDigest": patch_digest,
            "changedPaths": changed_paths,
            "priorExecutionSpecDigest": execution_runner_protocol::canonical_sha256(&prior_spec)
                .map_err(|e| sqlx::Error::Protocol(e.to_string()))?
        });
        sqlx::query(
            "INSERT INTO runner_scheduling_request_t (
                host_id, request_id, idempotency_key, origin_kind, origin_service_id,
                origin_instance_id, subject_kind, subject_id, process_id, task_id,
                policy_snapshot_id, policy_digest, normalized_requirements, execution_spec,
                fairness_key, priority, state, approval_id
             ) VALUES ($1,$2,$3,'workflow',$4,$5,'workflow-task',$6,$7,$6,$8,$9,$10,$11,$12,100,'PENDING_CAPACITY',$13)",
        )
        .bind(decision.host_id).bind(request_id)
        .bind(format!("approval:{}", decision.approval_id))
        .bind(&self.origin_service_id).bind(&self.origin_instance_id)
        .bind(task_id).bind(process_id).bind(policy_snapshot_id).bind(&policy_digest)
        .bind(requirements).bind(execution_spec)
        .bind(format!("workflow:{process_id}")).bind(decision.approval_id)
        .execute(&mut *tx).await?;
        sqlx::query(
            "UPDATE workflow_approval_t SET state='APPROVED', actor=$1, reason=$2,
                    decided_ts=CURRENT_TIMESTAMP WHERE host_id=$3 AND approval_id=$4 AND state='REQUESTED'",
        ).bind(decision.actor.trim()).bind(&decision.reason).bind(decision.host_id)
          .bind(decision.approval_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn reject(&self, decision: &ApprovalDecision) -> Result<(), sqlx::Error> {
        self.decide_terminal(decision, "REJECTED").await
    }

    pub async fn expire_due(&self, limit: i64) -> Result<u64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let result = sqlx::query(
            "WITH due AS (SELECT host_id, approval_id FROM workflow_approval_t
              WHERE state='REQUESTED' AND expires_ts <= CURRENT_TIMESTAMP
              ORDER BY expires_ts LIMIT $1 FOR UPDATE SKIP LOCKED)
             UPDATE workflow_approval_t a SET state='EXPIRED', decided_ts=CURRENT_TIMESTAMP,
                    reason='approval_ttl_expired'
             FROM due WHERE a.host_id=due.host_id AND a.approval_id=due.approval_id",
        )
        .bind(limit)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE process_info_t p SET status_code='F', custom_status_code='APPROVAL_EXPIRED',
                    completed_ts=CURRENT_TIMESTAMP
             WHERE p.status_code='W' AND EXISTS (SELECT 1 FROM workflow_approval_t a
               WHERE a.host_id=p.host_id AND a.process_id=p.process_id
                 AND a.state='EXPIRED' AND a.reason='approval_ttl_expired')",
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }

    async fn decide_terminal(
        &self,
        decision: &ApprovalDecision,
        state: &str,
    ) -> Result<(), sqlx::Error> {
        if decision.actor.trim().is_empty() {
            return Err(sqlx::Error::Protocol(
                "approval actor must be authenticated".into(),
            ));
        }
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE workflow_approval_t SET state=$1, actor=$2, reason=$3, decided_ts=CURRENT_TIMESTAMP
             WHERE host_id=$4 AND approval_id=$5 AND state='REQUESTED'
               AND artifact_digest_set=$6 AND provenance_digest IS NOT DISTINCT FROM $7
               AND target=$8 AND operation=$9 AND policy_digest=$10",
        ).bind(state).bind(decision.actor.trim()).bind(&decision.reason)
          .bind(decision.host_id).bind(decision.approval_id).bind(&decision.artifact_digest_set)
          .bind(&decision.provenance_digest).bind(&decision.target).bind(&decision.operation)
          .bind(&decision.policy_digest).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "approval subject changed or was already decided".into(),
            ));
        }
        sqlx::query("UPDATE process_info_t SET status_code='F', custom_status_code=$1,
                    completed_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND process_id=(
                    SELECT process_id FROM workflow_approval_t WHERE host_id=$2 AND approval_id=$3)")
            .bind(format!("APPROVAL_{state}")).bind(decision.host_id).bind(decision.approval_id)
            .execute(&mut *tx).await?;
        tx.commit().await
    }

    async fn revalidate_artifacts(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: Uuid,
        execution_id: Uuid,
        expected: &Value,
        provenance: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let actual: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(content_digest ORDER BY content_digest), '[]'::jsonb)
             FROM workflow_artifact_t WHERE host_id=$1 AND execution_id=$2
               AND verification_state='VERIFIED' AND promotion_state='BOUND'",
        )
        .bind(host_id)
        .bind(execution_id)
        .fetch_one(&mut **tx)
        .await?;
        if &actual != expected {
            return Err(sqlx::Error::Protocol(
                "approved artifacts are no longer verified and bound".into(),
            ));
        }
        if let Some(digest) = provenance {
            let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_provenance_t
                WHERE host_id=$1 AND execution_id=$2 AND statement_digest=$3 AND trusted_generator <> '')")
                .bind(host_id).bind(execution_id).bind(digest).fetch_one(&mut **tx).await?;
            if !valid {
                return Err(sqlx::Error::Protocol(
                    "approved provenance is no longer trusted".into(),
                ));
            }
        }
        Ok(())
    }
}
