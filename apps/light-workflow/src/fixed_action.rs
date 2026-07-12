use execution_fixed_action::{FixedPatchRequest, GitObjectFormat, execute_fixed_patch};
use execution_security::ProtectedPathPolicy;
use serde_json::{Value, json};
use sha2::Digest;
use sqlx::PgPool;
use std::time::Duration;
use std::{
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct FixedActionExecutor {
    pool: PgPool,
    work_root: PathBuf,
    artifact_root: PathBuf,
    branch_prefix: String,
    protected_paths: ProtectedPathPolicy,
}

#[derive(sqlx::FromRow)]
struct ClaimedAction {
    host_id: Uuid,
    fixed_action_id: Uuid,
    execution_id: Uuid,
    approval_id: Uuid,
    repository_reference: String,
    base_commit: String,
    repository_object_format: String,
    target_ref: String,
    patch_artifact_reference: String,
    artifact_digest: String,
    policy_digest: String,
    changed_paths: Value,
}

impl FixedActionExecutor {
    pub fn new(
        pool: PgPool,
        work_root: PathBuf,
        artifact_root: PathBuf,
        branch_prefix: impl Into<String>,
        protected_paths: ProtectedPathPolicy,
    ) -> Self {
        Self {
            pool,
            work_root,
            artifact_root,
            branch_prefix: branch_prefix.into(),
            protected_paths,
        }
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let action = sqlx::query_as::<_, ClaimedAction>(
            "SELECT f.host_id,f.fixed_action_id,f.execution_id,f.approval_id,
                    f.repository_reference,f.base_commit,f.repository_object_format,
                    f.target_ref,f.patch_artifact_reference,f.artifact_digest,
                    f.policy_digest,f.changed_paths
             FROM execution_fixed_action_t f
             JOIN workflow_approval_t a ON a.host_id=f.host_id AND a.approval_id=f.approval_id
               AND a.state='CONSUMED' AND a.consuming_execution_id=f.execution_id
             JOIN execution_attempt_t e ON e.host_id=f.host_id AND e.execution_id=f.execution_id
               AND e.state='CREATED'
             WHERE f.state='REQUESTED' AND f.action_kind='apply-patch'
             ORDER BY f.created_ts,f.fixed_action_id LIMIT 1
             FOR UPDATE OF f,e SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(action) = action else {
            tx.commit().await?;
            return Ok(false);
        };
        sqlx::query(
            "UPDATE execution_fixed_action_t SET state='RUNNING',updated_ts=CURRENT_TIMESTAMP
                    WHERE host_id=$1 AND fixed_action_id=$2 AND state='REQUESTED'",
        )
        .bind(action.host_id)
        .bind(action.fixed_action_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE execution_attempt_t SET state='STARTED',lease_started_ts=CURRENT_TIMESTAMP,
                    updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND execution_id=$2 AND state='CREATED'")
            .bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
        tx.commit().await?;

        let result = self.execute(action).await;
        let mut tx = self.pool.begin().await?;
        match result {
            Ok((action, evidence)) => {
                sqlx::query("UPDATE execution_fixed_action_t SET state='SUCCEEDED',result_evidence=$1,
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='RUNNING'")
                    .bind(&evidence).bind(action.host_id).bind(action.fixed_action_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE execution_attempt_t SET state='SUCCEEDED',terminal_ts=CURRENT_TIMESTAMP,
                            normalized_result=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(json!({"structuredOutput": evidence, "policyDigest": action.policy_digest}))
                    .bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
            }
            Err((action, message)) => {
                let error = json!({"failureClass":"fixed_action_failed","message":message});
                sqlx::query("UPDATE execution_fixed_action_t SET state='FAILED',result_evidence=$1,
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='RUNNING'")
                    .bind(&error).bind(action.host_id).bind(action.fixed_action_id).execute(&mut *tx).await?;
                sqlx::query("UPDATE execution_attempt_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
                            normalized_error=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(error).bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn run(self) -> Result<(), sqlx::Error> {
        loop {
            if !self.run_once().await? {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }

    async fn execute(
        &self,
        action: ClaimedAction,
    ) -> Result<(ClaimedAction, Value), (ClaimedAction, String)> {
        let path = match local_artifact_path(&action.patch_artifact_reference, &self.artifact_root)
        {
            Ok(path) => path,
            Err(error) => return Err((action, error)),
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) if bytes.len() <= 16 * 1024 * 1024 => bytes,
            Ok(_) => return Err((action, "approved patch exceeds 16 MiB".into())),
            Err(error) => return Err((action, error.to_string())),
        };
        let changed_paths =
            match serde_json::from_value::<Vec<String>>(action.changed_paths.clone()) {
                Ok(paths) => paths,
                Err(error) => return Err((action, error.to_string())),
            };
        let request = FixedPatchRequest {
            request_id: action.fixed_action_id,
            repository: action.repository_reference.clone(),
            base_commit: action.base_commit.clone(),
            repository_object_format: if action.repository_object_format == "sha256" {
                GitObjectFormat::Sha256
            } else {
                GitObjectFormat::Sha1
            },
            target_branch: action.target_ref.clone(),
            patch_artifact_ref: path.display().to_string(),
            patch_digest: action.artifact_digest.clone(),
            policy_digest: action.policy_digest.clone(),
            approval_id: action.approval_id,
            changed_paths,
        };
        let workspace = self.work_root.join(action.fixed_action_id.to_string());
        let repository = action.repository_reference.clone();
        let branch_prefix = self.branch_prefix.clone();
        let protected = self.protected_paths.clone();
        match tokio::task::spawn_blocking(move || {
            execute_fixed_patch(
                &request,
                &bytes,
                &repository,
                &branch_prefix,
                &workspace,
                &protected,
            )
        })
        .await
        {
            Ok(Ok(evidence)) => Ok((
                action,
                json!({
                    "canonicalPatchDigest": format!("sha256:{}", hex::encode(sha2::Sha256::digest(&evidence.canonical_patch))),
                    "changedPaths": evidence.changed_paths,
                    "checkedOutCommit": evidence.checked_out_commit,
                    "repositoryObjectFormat": evidence.repository_object_format
                }),
            )),
            Ok(Err(error)) => Err((action, error.to_string())),
            Err(error) => Err((action, error.to_string())),
        }
    }
}

fn local_artifact_path(reference: &str, artifact_root: &Path) -> Result<PathBuf, String> {
    let raw = reference.strip_prefix("file://").unwrap_or(reference);
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("fixed actions accept only absolute file:// artifact references".into());
    }
    let root = artifact_root.canonicalize().map_err(|e| e.to_string())?;
    let path = path.canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(&root) {
        return Err("fixed-action artifact reference escapes the trusted artifact root".into());
    }
    Ok(path)
}
