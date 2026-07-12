use execution_fixed_action::{FixedPatchRequest, GitObjectFormat, execute_fixed_patch};
use execution_security::ProtectedPathPolicy;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;
use sqlx::PgPool;
use std::sync::Arc;
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
    repository_provider: Option<Arc<HttpFixedActionProvider>>,
    release_provider: Option<Arc<HttpFixedActionProvider>>,
}

#[derive(sqlx::FromRow)]
struct ClaimedAction {
    host_id: Uuid,
    fixed_action_id: Uuid,
    execution_id: Uuid,
    approval_id: Uuid,
    repository_reference: String,
    base_commit: Option<String>,
    repository_object_format: String,
    target_ref: String,
    patch_artifact_reference: String,
    artifact_digest: String,
    policy_digest: String,
    changed_paths: Value,
    action_kind: String,
    action_spec: Value,
    provenance_digest: Option<String>,
    idempotency_key: Option<String>,
}

#[derive(Clone)]
pub struct HttpFixedActionProvider {
    client: reqwest::Client,
    base_url: reqwest::Url,
    bearer_token: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderReceipt {
    provider_operation_id: String,
    state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    evidence_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resource_reference: Option<String>,
}

#[derive(Debug)]
enum ActionFailure {
    Failed(String),
    Unknown(String),
}

enum ProviderInspection {
    Succeeded(Value),
    Failed(Value),
    Pending,
}

impl HttpFixedActionProvider {
    pub fn new(base_url: &str, bearer_token: String) -> Result<Self, String> {
        if bearer_token.trim().len() < 16 {
            return Err("fixed-action provider token must contain at least 16 bytes".into());
        }
        let mut base_url = reqwest::Url::parse(base_url).map_err(|e| e.to_string())?;
        if base_url.scheme() != "https" && !cfg!(test) {
            return Err("fixed-action provider URL must use HTTPS".into());
        }
        if !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
        {
            return Err(
                "fixed-action provider URL cannot contain credentials, query, or fragment".into(),
            );
        }
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            base_url,
            bearer_token,
        })
    }

    async fn execute(&self, action: &ClaimedAction) -> Result<Value, ActionFailure> {
        validate_provider_action(action).map_err(ActionFailure::Failed)?;
        let endpoint = self
            .base_url
            .join(&format!("fixed-actions/{}", action.action_kind))
            .map_err(|e| ActionFailure::Failed(e.to_string()))?;
        if endpoint.origin() != self.base_url.origin() {
            return Err(ActionFailure::Failed(
                "fixed-action provider endpoint escaped its configured origin".into(),
            ));
        }
        let idempotency_key = action.idempotency_key.as_deref().ok_or_else(|| {
            ActionFailure::Failed("provider action lacks idempotency key".to_string())
        })?;
        let body = json!({
            "fixedActionId": action.fixed_action_id,
            "executionId": action.execution_id,
            "approvalId": action.approval_id,
            "operation": action.action_kind,
            "immutableInputDigest": action.artifact_digest,
            "target": action.action_spec.get("target"),
            "policyDigest": action.policy_digest,
            "provenanceDigest": action.provenance_digest,
            "spec": action.action_spec,
        });
        let mut last_error = String::new();
        let mut accepted = None;
        for attempt in 0..3 {
            match self
                .client
                .post(endpoint.clone())
                .bearer_auth(&self.bearer_token)
                .header("Idempotency-Key", idempotency_key)
                .json(&body)
                .send()
                .await
            {
                Ok(response) if response.status().is_server_error() => {
                    last_error = format!("provider returned {}", response.status());
                }
                Ok(response) => {
                    accepted = Some(response);
                    break;
                }
                Err(error) => last_error = error.to_string(),
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_millis(100 * (attempt + 1))).await;
            }
        }
        let response = accepted.ok_or_else(|| {
            ActionFailure::Unknown(format!(
                "fixed-action provider result is unknown after idempotent retries: {last_error}"
            ))
        })?;
        if !response.status().is_success() {
            return Err(ActionFailure::Failed(format!(
                "fixed-action provider returned {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|size| size > 64 * 1024)
        {
            return Err(ActionFailure::Unknown(
                "fixed-action provider receipt exceeds 64 KiB".into(),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ActionFailure::Unknown(e.to_string()))?;
        if bytes.len() > 64 * 1024 {
            return Err(ActionFailure::Unknown(
                "fixed-action provider receipt exceeds 64 KiB".into(),
            ));
        }
        let receipt: ProviderReceipt = serde_json::from_slice(&bytes)
            .map_err(|e| ActionFailure::Unknown(format!("invalid provider receipt: {e}")))?;
        if receipt.provider_operation_id.is_empty()
            || receipt.provider_operation_id.len() > 255
            || receipt.state != "SUCCEEDED"
            || !receipt
                .evidence_digest
                .as_deref()
                .is_some_and(valid_sha256_digest)
            || receipt
                .resource_reference
                .as_ref()
                .is_some_and(|value| value.len() > 2048)
        {
            return Err(ActionFailure::Unknown(
                "fixed-action provider returned an invalid receipt binding".into(),
            ));
        }
        serde_json::to_value(receipt).map_err(|e| ActionFailure::Unknown(e.to_string()))
    }

    async fn inspect(&self, action: &ClaimedAction) -> Result<ProviderInspection, String> {
        let key = action
            .idempotency_key
            .as_deref()
            .ok_or("provider action lacks idempotency key")?;
        let endpoint = self
            .base_url
            .join("fixed-actions/status")
            .map_err(|e| e.to_string())?;
        let response = self
            .client
            .get(endpoint)
            .bearer_auth(&self.bearer_token)
            .header("Idempotency-Key", key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if response.status() == reqwest::StatusCode::NOT_FOUND
            || response.status().is_server_error()
        {
            return Ok(ProviderInspection::Pending);
        }
        if !response.status().is_success() {
            return Err(format!("provider status returned {}", response.status()));
        }
        if response.content_length().is_some_and(|n| n > 64 * 1024) {
            return Err("provider status receipt exceeds 64 KiB".into());
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > 64 * 1024 {
            return Err("provider status receipt exceeds 64 KiB".into());
        }
        let receipt: ProviderReceipt = serde_json::from_slice(&bytes)
            .map_err(|e| format!("invalid provider status receipt: {e}"))?;
        if receipt.provider_operation_id.is_empty() {
            return Err("provider status receipt is not bound".into());
        }
        if matches!(receipt.state.as_str(), "SUCCEEDED" | "FAILED")
            && !receipt
                .evidence_digest
                .as_deref()
                .is_some_and(valid_sha256_digest)
        {
            return Err("terminal provider status lacks valid evidence".into());
        }
        let value = serde_json::to_value(receipt).map_err(|e| e.to_string())?;
        match value.get("state").and_then(Value::as_str) {
            Some("SUCCEEDED") => Ok(ProviderInspection::Succeeded(value)),
            Some("FAILED") => Ok(ProviderInspection::Failed(value)),
            Some("PENDING" | "NOT_FOUND") => Ok(ProviderInspection::Pending),
            _ => Err("provider status receipt has an invalid state".into()),
        }
    }
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_provider_action(action: &ClaimedAction) -> Result<(), String> {
    let spec = action
        .action_spec
        .as_object()
        .ok_or("fixed-action spec must be an object")?;
    let bound = |name: &str, expected: &str| {
        if spec.get(name).and_then(Value::as_str) == Some(expected) {
            Ok(())
        } else {
            Err(format!(
                "fixed-action {name} differs from its durable binding"
            ))
        }
    };
    bound("operation", &action.action_kind)?;
    bound(
        "target",
        spec.get("target")
            .and_then(Value::as_str)
            .ok_or("fixed-action target is missing")?,
    )?;
    bound("patchDigest", &action.artifact_digest)?;
    if spec.get("policyDigest").and_then(Value::as_str) != Some(action.policy_digest.as_str()) {
        return Err("fixed-action policy digest differs from its durable binding".into());
    }
    if spec.get("provenanceDigest").and_then(Value::as_str) != action.provenance_digest.as_deref() {
        return Err("fixed-action provenance digest differs from its durable binding".into());
    }
    match action.action_kind.as_str() {
        "create-branch" | "open-pr" => {
            bound("repository", &action.repository_reference)?;
            bound("targetBranch", &action.target_ref)?;
            bound(
                "baseCommit",
                action
                    .base_commit
                    .as_deref()
                    .ok_or("repository action lacks base commit")?,
            )?;
        }
        "publish" | "sign" => {
            bound("patchArtifactReference", &action.patch_artifact_reference)?;
            if action.provenance_digest.is_none() {
                return Err("publish/sign requires trusted provenance".into());
            }
        }
        _ => return Err("unsupported provider fixed action".into()),
    }
    Ok(())
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
            repository_provider: None,
            release_provider: None,
        }
    }

    pub fn with_providers(
        mut self,
        repository_provider: Option<HttpFixedActionProvider>,
        release_provider: Option<HttpFixedActionProvider>,
    ) -> Self {
        self.repository_provider = repository_provider.map(Arc::new);
        self.release_provider = release_provider.map(Arc::new);
        self
    }

    pub async fn run_once(&self) -> Result<bool, sqlx::Error> {
        if self.reconcile_unknown_once().await? {
            return Ok(true);
        }
        let mut tx = self.pool.begin().await?;
        let action = sqlx::query_as::<_, ClaimedAction>(
            "SELECT f.host_id,f.fixed_action_id,f.execution_id,f.approval_id,
                    f.repository_reference,f.base_commit,f.repository_object_format,
                    f.target_ref,f.patch_artifact_reference,f.artifact_digest,
                    f.policy_digest,f.changed_paths,f.action_kind,f.action_spec,
                    f.provenance_digest,f.idempotency_key
             FROM execution_fixed_action_t f
             JOIN workflow_approval_t a ON a.host_id=f.host_id AND a.approval_id=f.approval_id
               AND a.state='CONSUMED' AND a.consuming_execution_id=f.execution_id
               AND a.operation=f.action_kind AND a.target=f.action_spec->>'target'
               AND a.policy_digest=f.policy_digest
               AND a.provenance_digest IS NOT DISTINCT FROM f.provenance_digest
               AND a.artifact_digest_set ? f.artifact_digest
             JOIN execution_attempt_t e ON e.host_id=f.host_id AND e.execution_id=f.execution_id
               AND e.state='CREATED'
             WHERE f.state='REQUESTED'
               AND f.action_kind IN ('apply-patch','create-branch','open-pr','publish','sign')
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
                let transitioned=sqlx::query("UPDATE execution_fixed_action_t SET state='SUCCEEDED',result_evidence=$1,
                            provider_receipt=CASE WHEN action_kind<>'apply-patch' THEN $1 ELSE provider_receipt END,
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='RUNNING'")
                    .bind(&evidence).bind(action.host_id).bind(action.fixed_action_id).execute(&mut *tx).await?;
                if transitioned.rows_affected() == 1 {
                    sqlx::query("UPDATE execution_attempt_t SET state='SUCCEEDED',terminal_ts=CURRENT_TIMESTAMP,
                            normalized_result=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(json!({"structuredOutput": evidence, "policyDigest": action.policy_digest}))
                    .bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
                }
            }
            Err((action, ActionFailure::Failed(message))) => {
                let error = json!({"failureClass":"fixed_action_failed","message":message});
                let transitioned=sqlx::query("UPDATE execution_fixed_action_t SET state='FAILED',result_evidence=$1,
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='RUNNING'")
                    .bind(&error).bind(action.host_id).bind(action.fixed_action_id).execute(&mut *tx).await?;
                if transitioned.rows_affected() == 1 {
                    sqlx::query("UPDATE execution_attempt_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
                            normalized_error=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',
                            updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(error).bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
                }
            }
            Err((action, ActionFailure::Unknown(message))) => {
                let evidence = json!({"failureClass":"fixed_action_unknown","message":message});
                sqlx::query("UPDATE execution_fixed_action_t SET state='UNKNOWN',unknown_since_ts=CURRENT_TIMESTAMP,
                            next_reconcile_ts=CURRENT_TIMESTAMP,result_evidence=$1,updated_ts=CURRENT_TIMESTAMP
                            WHERE host_id=$2 AND fixed_action_id=$3 AND state='RUNNING'")
                    .bind(evidence).bind(action.host_id).bind(action.fixed_action_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    async fn reconcile_unknown_once(&self) -> Result<bool, sqlx::Error> {
        sqlx::query("UPDATE execution_fixed_action_t f SET state='UNKNOWN',unknown_since_ts=COALESCE(f.unknown_since_ts,CURRENT_TIMESTAMP),next_reconcile_ts=CURRENT_TIMESTAMP,updated_ts=CURRENT_TIMESTAMP
          FROM execution_attempt_t e WHERE e.host_id=f.host_id AND e.execution_id=f.execution_id AND f.state='RUNNING'
            AND ((e.lease_deadline_ts IS NOT NULL AND e.lease_deadline_ts<=CURRENT_TIMESTAMP)
              OR (e.lease_deadline_ts IS NULL AND f.updated_ts<CURRENT_TIMESTAMP-interval '1 hour'))")
            .execute(&self.pool).await?;
        let token = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let action=sqlx::query_as::<_,ClaimedAction>("WITH candidate AS(
            SELECT host_id,fixed_action_id FROM execution_fixed_action_t
            WHERE state='UNKNOWN' AND (next_reconcile_ts IS NULL OR next_reconcile_ts<=CURRENT_TIMESTAMP)
              AND (reconciliation_claim_token IS NULL OR reconciliation_lease_expires_ts<=CURRENT_TIMESTAMP)
            ORDER BY unknown_since_ts,fixed_action_id LIMIT 1 FOR UPDATE SKIP LOCKED), claimed AS(
            UPDATE execution_fixed_action_t f SET reconciliation_claim_token=$1,reconciliation_lease_expires_ts=CURRENT_TIMESTAMP+interval '1 minute',
              reconciliation_attempt_count=reconciliation_attempt_count+1,updated_ts=CURRENT_TIMESTAMP
            FROM candidate c WHERE f.host_id=c.host_id AND f.fixed_action_id=c.fixed_action_id
            RETURNING f.*)
          SELECT host_id,fixed_action_id,execution_id,approval_id,repository_reference,base_commit,repository_object_format,target_ref,
            patch_artifact_reference,artifact_digest,policy_digest,changed_paths,action_kind,action_spec,provenance_digest,idempotency_key FROM claimed")
            .bind(token).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        let Some(action) = action else {
            return Ok(false);
        };
        let inspection = match action.action_kind.as_str() {
            "create-branch" | "open-pr" => match &self.repository_provider {
                Some(p) => p.inspect(&action).await.ok(),
                None => None,
            },
            "publish" | "sign" => match &self.release_provider {
                Some(p) => p.inspect(&action).await.ok(),
                None => None,
            },
            _ => None,
        };
        let mut tx = self.pool.begin().await?;
        match inspection {
            Some(ProviderInspection::Succeeded(evidence)) => {
                sqlx::query("UPDATE execution_fixed_action_t SET state='SUCCEEDED',provider_receipt=$1,result_evidence=$1,reconciliation_claim_token=NULL,reconciliation_lease_expires_ts=NULL,next_reconcile_ts=NULL,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='UNKNOWN' AND reconciliation_claim_token=$4")
                    .bind(&evidence).bind(action.host_id).bind(action.fixed_action_id).bind(token).execute(&mut *tx).await?;
                sqlx::query("UPDATE execution_attempt_t SET state='SUCCEEDED',terminal_ts=CURRENT_TIMESTAMP,normalized_result=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(json!({"structuredOutput":evidence,"policyDigest":action.policy_digest})).bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
            }
            Some(ProviderInspection::Failed(evidence)) => {
                sqlx::query("UPDATE execution_fixed_action_t SET state='FAILED',provider_receipt=$1,result_evidence=$1,reconciliation_claim_token=NULL,reconciliation_lease_expires_ts=NULL,next_reconcile_ts=NULL,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND fixed_action_id=$3 AND state='UNKNOWN' AND reconciliation_claim_token=$4")
                    .bind(&evidence).bind(action.host_id).bind(action.fixed_action_id).bind(token).execute(&mut *tx).await?;
                sqlx::query("UPDATE execution_attempt_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,normalized_error=$1,cleanup_state='NOT_REQUIRED',retry_classification='unsafe',updated_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND execution_id=$3 AND state='STARTED'")
                    .bind(json!({"failureClass":"fixed_action_provider_failed","providerEvidence":evidence})).bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
            }
            Some(ProviderInspection::Pending) | None => {
                let unknown_since:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT unknown_since_ts FROM execution_fixed_action_t WHERE host_id=$1 AND fixed_action_id=$2 AND state='UNKNOWN' AND reconciliation_claim_token=$3 FOR UPDATE")
                    .bind(action.host_id).bind(action.fixed_action_id).bind(token).fetch_one(&mut *tx).await?;
                let terminal = action.action_kind == "apply-patch"
                    || chrono::Utc::now() - unknown_since > chrono::Duration::hours(24);
                if terminal {
                    sqlx::query("UPDATE execution_fixed_action_t SET reconciliation_claim_token=NULL,reconciliation_lease_expires_ts=NULL,next_reconcile_ts=NULL,result_evidence=jsonb_build_object('failureClass','fixed_action_unknown','operatorActionRequired',true),updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND fixed_action_id=$2 AND state='UNKNOWN' AND reconciliation_claim_token=$3")
                        .bind(action.host_id).bind(action.fixed_action_id).bind(token).execute(&mut *tx).await?;
                    sqlx::query("UPDATE execution_attempt_t SET state='UNKNOWN',terminal_ts=CURRENT_TIMESTAMP,normalized_error=jsonb_build_object('failureClass','fixed_action_unknown','operatorActionRequired',true),cleanup_state='NOT_REQUIRED',retry_classification='unsafe',updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND execution_id=$2 AND state='STARTED'")
                        .bind(action.host_id).bind(action.execution_id).execute(&mut *tx).await?;
                } else {
                    sqlx::query("UPDATE execution_fixed_action_t SET reconciliation_claim_token=NULL,reconciliation_lease_expires_ts=NULL,next_reconcile_ts=CURRENT_TIMESTAMP+LEAST(interval '1 hour',make_interval(secs=>power(2,LEAST(reconciliation_attempt_count,12))::int)),updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND fixed_action_id=$2 AND state='UNKNOWN' AND reconciliation_claim_token=$3")
                        .bind(action.host_id).bind(action.fixed_action_id).bind(token).execute(&mut *tx).await?;
                }
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
    ) -> Result<(ClaimedAction, Value), (ClaimedAction, ActionFailure)> {
        if action.action_kind != "apply-patch" {
            if matches!(action.action_kind.as_str(), "create-branch" | "open-pr")
                && (!action.target_ref.starts_with(&self.branch_prefix)
                    || action.target_ref.contains("..")
                    || action.target_ref.starts_with('-'))
            {
                return Err((
                    action,
                    ActionFailure::Failed(
                        "repository fixed action target ref is outside the configured prefix"
                            .into(),
                    ),
                ));
            }
            let provider = match action.action_kind.as_str() {
                "create-branch" | "open-pr" => self.repository_provider.as_ref(),
                "publish" | "sign" => self.release_provider.as_ref(),
                _ => None,
            };
            let Some(provider) = provider else {
                return Err((
                    action,
                    ActionFailure::Failed("fixed-action provider is not configured".into()),
                ));
            };
            return match provider.execute(&action).await {
                Ok(receipt) => Ok((action, receipt)),
                Err(error) => Err((action, error)),
            };
        }
        let path = match local_artifact_path(&action.patch_artifact_reference, &self.artifact_root)
        {
            Ok(path) => path,
            Err(error) => return Err((action, ActionFailure::Failed(error))),
        };
        let bytes = match fs::read(&path) {
            Ok(bytes) if bytes.len() <= 16 * 1024 * 1024 => bytes,
            Ok(_) => {
                return Err((
                    action,
                    ActionFailure::Failed("approved patch exceeds 16 MiB".into()),
                ));
            }
            Err(error) => return Err((action, ActionFailure::Failed(error.to_string()))),
        };
        let changed_paths =
            match serde_json::from_value::<Vec<String>>(action.changed_paths.clone()) {
                Ok(paths) => paths,
                Err(error) => return Err((action, ActionFailure::Failed(error.to_string()))),
            };
        let base_commit = match action.base_commit.clone() {
            Some(base_commit) => base_commit,
            None => {
                return Err((
                    action,
                    ActionFailure::Failed("apply-patch lacks base commit".into()),
                ));
            }
        };
        let request = FixedPatchRequest {
            request_id: action.fixed_action_id,
            repository: action.repository_reference.clone(),
            base_commit,
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
            Ok(Err(error)) => Err((action, ActionFailure::Failed(error.to_string()))),
            Err(error) => Err((action, ActionFailure::Unknown(error.to_string()))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        http::HeaderMap,
        routing::{get, post},
    };

    fn provider_action(kind: &str) -> ClaimedAction {
        let digest = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        ClaimedAction {
            host_id: Uuid::now_v7(),
            fixed_action_id: Uuid::now_v7(),
            execution_id: Uuid::now_v7(),
            approval_id: Uuid::now_v7(),
            repository_reference: "https://github.com/example/repo.git".into(),
            base_commit: Some("a".repeat(40)),
            repository_object_format: "sha1".into(),
            target_ref: "agent/change".into(),
            patch_artifact_reference: "s3://immutable/change.patch".into(),
            artifact_digest: digest.into(),
            policy_digest: "sha256:policy".into(),
            changed_paths: json!(["src/lib.rs"]),
            action_kind: kind.into(),
            action_spec: json!({
                "operation": kind,
                "target": "https://github.com/example/repo.git",
                "repository": "https://github.com/example/repo.git",
                "baseCommit": "a".repeat(40),
                "targetBranch": "agent/change",
                "patchArtifactReference": "s3://immutable/change.patch",
                "patchDigest": digest,
                "policyDigest": "sha256:policy",
                "provenanceDigest": "sha256:provenance"
            }),
            provenance_digest: Some("sha256:provenance".into()),
            idempotency_key: Some("approval:00000000-0000-0000-0000-000000000001".into()),
        }
    }

    #[test]
    fn provider_action_rejects_binding_substitution() {
        let mut action = provider_action("create-branch");
        assert!(validate_provider_action(&action).is_ok());
        action.target_ref = "agent/substituted".into();
        assert!(validate_provider_action(&action).is_err());
        let mut action = provider_action("publish");
        action.provenance_digest = None;
        action.action_spec["provenanceDigest"] = Value::Null;
        assert!(validate_provider_action(&action).is_err());
    }

    #[tokio::test]
    async fn provider_receives_bounded_typed_idempotent_request() {
        async fn handler(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
            assert_eq!(
                headers.get("idempotency-key").unwrap(),
                "approval:00000000-0000-0000-0000-000000000001"
            );
            assert_eq!(body["operation"], "create-branch");
            assert_eq!(body["immutableInputDigest"], body["spec"]["patchDigest"]);
            Json(json!({
                "providerOperationId":"branch-123",
                "state":"SUCCEEDED",
                "evidenceDigest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            }))
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/api/fixed-actions/create-branch", post(handler)),
            )
            .await
            .unwrap();
        });
        let provider = HttpFixedActionProvider::new(
            &format!("http://{address}/api/"),
            "test-provider-token-32-bytes-long".into(),
        )
        .unwrap();
        let receipt = provider
            .execute(&provider_action("create-branch"))
            .await
            .unwrap();
        assert_eq!(receipt["providerOperationId"], "branch-123");
    }

    #[tokio::test]
    async fn uncertain_dispatch_is_unknown_and_status_is_reconciled_by_key() {
        async fn dispatch() -> axum::http::StatusCode {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }
        async fn status(headers: HeaderMap) -> Json<Value> {
            assert_eq!(
                headers.get("idempotency-key").unwrap(),
                "approval:00000000-0000-0000-0000-000000000001"
            );
            Json(
                json!({"providerOperationId":"branch-123","state":"SUCCEEDED","evidenceDigest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/api/fixed-actions/create-branch", post(dispatch))
                    .route("/api/fixed-actions/status", get(status)),
            )
            .await
            .unwrap();
        });
        let provider = HttpFixedActionProvider::new(
            &format!("http://{address}/api/"),
            "test-provider-token-32-bytes-long".into(),
        )
        .unwrap();
        let action = provider_action("create-branch");
        assert!(matches!(
            provider.execute(&action).await,
            Err(ActionFailure::Unknown(_))
        ));
        assert!(matches!(
            provider.inspect(&action).await.unwrap(),
            ProviderInspection::Succeeded(_)
        ));
    }
}
