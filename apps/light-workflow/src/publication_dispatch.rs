//! Owner-authenticated fixed publication selected from a pinned stage definition.
use crate::{
    artifact_publish::{ArtifactPublication, publish_artifact_in_transaction},
    artifact_store::DurableArtifactStore,
    development_handoff::verify_artifact,
    development_store::*,
    invocation::AuthenticatedInvocationContext,
    publication_journal::*,
};
use chrono::{Duration, Utc};
use development_workflow_contract::{publication::*, *};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationSlot {
    pub repository: String,
    pub destination: Destination,
    pub source_repository: String,
    pub source_path: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PinnedPublication {
    pub policy: PublicationPolicy,
    pub slots: BTreeMap<String, PublicationSlot>,
}

pub(crate) async fn verify_required(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    store: &DurableArtifactStore,
    auth: &AuthenticatedInvocationContext<'_>,
    feature: &FeatureRun,
    definition: &Value,
) -> StoreResult<()> {
    let Some(raw) = definition.pointer("/document/metadata/developmentWorkflowPublication") else {
        return Ok(());
    };
    let pinned: PinnedPublication = serde_json::from_value(raw.clone())?;
    check(
        !pinned.slots.is_empty() && pinned.slots.len() <= 32,
        "invalid required publication slots",
    )?;
    let claim = feature
        .active_claim
        .as_ref()
        .ok_or(StoreError::Conflict("publication claim missing"))?;
    let instance: Uuid = claim
        .workflow_instance_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid publication invocation"))?;
    let process: Uuid = claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid publication process"))?;
    let accepted = feature.accepted_results.last().ok_or(StoreError::Conflict(
        "accepted publication candidate missing",
    ))?;
    let source_process: Uuid = accepted
        .claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid source process"))?;
    let bytes = verify_artifact(
        tx,
        store,
        auth.host_id,
        source_process,
        &accepted.candidate.package,
    )
    .await?;
    let package: task_workspace::SnapshotPackage = serde_json::from_slice(&bytes)?;
    let verified = package
        .verified_receipt()
        .map_err(|_| StoreError::Conflict("publication candidate corrupt"))?;
    check(
        package.feature_id == feature.feature_run_id
            && verified.package_digest == accepted.candidate.candidate_digest,
        "publication candidate changed",
    )?;
    for (slot, selected) in &pinned.slots {
        let file = package
            .repositories
            .get(&selected.source_repository)
            .and_then(|r| r.files.get(&selected.source_path))
            .ok_or(StoreError::Conflict("publication source missing"))?;
        let plan = PublicationPlan {
            feature_id: feature.feature_run_id.clone(),
            slot: slot.clone(),
            revision: selected.revision,
            candidate_digest: accepted.candidate.candidate_digest.clone(),
            repository: selected.repository.clone(),
            destination: selected.destination.clone(),
            content: accepted.candidate.package.clone(),
        };
        let effect = PublicationEffect::prepare(
            &plan,
            &pinned.policy,
            &accepted.candidate.candidate_digest,
        )?;
        let request = PublicationDelivery {
            key: identity(
                "publication-provider/v1",
                &[&auth.host_id.to_string(), &effect.id],
            )
            .trim_start_matches("sha256:")
            .into(),
            plan,
            body: String::from_utf8(file.bytes.clone())
                .map_err(|_| StoreError::Conflict("publication source not UTF-8"))?,
        };
        let result: Option<Value> = sqlx::query_scalar("SELECT result FROM workflow_task_effect_t WHERE host_id=$1 AND workflow_instance_id=$2 AND task_name=$3 AND idempotency_key=$4 AND request_digest=$5 AND effect_state='confirmed'")
            .bind(auth.host_id).bind(instance).bind(format!("publication:{slot}:{}",selected.revision)).bind(&effect.id).bind(&effect.request_digest).fetch_optional(&mut **tx).await?.flatten();
        let result = result.ok_or(StoreError::Conflict("required publication unresolved"))?;
        let receipt: ProviderPublicationReceipt = serde_json::from_value(
            result
                .get("receipt")
                .cloned()
                .ok_or(StoreError::Conflict("publication receipt missing"))?,
        )?;
        let artifact: ArtifactRef = serde_json::from_value(
            result
                .get("verification")
                .cloned()
                .ok_or(StoreError::Conflict("publication verification missing"))?,
        )?;
        let proof = verify_artifact(tx, store, auth.host_id, process, &artifact).await?;
        check(
            serde_json::from_slice::<ProviderPublicationReceipt>(&proof)? == receipt
                && receipt.key == request.key
                && receipt.request_digest == request.request_digest()?,
            "publication proof mismatch",
        )?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct PublicationProvider {
    client: reqwest::Client,
    url: reqwest::Url,
    token: Arc<String>,
}
#[derive(Clone)]
pub struct PublicationProviderAccess(pub Option<PublicationProvider>);

impl PublicationProvider {
    pub fn from_environment()
    -> std::result::Result<Option<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let Ok(url) = std::env::var("WORKFLOW_PUBLICATION_PROVIDER_URL") else {
            return Ok(None);
        };
        let path = std::env::var("WORKFLOW_PUBLICATION_PROVIDER_TOKEN_FILE")?;
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("publication secret must be an owner-only regular file".into());
        }
        let token = std::fs::read_to_string(path)?.trim().to_owned();
        Ok(Some(Self::new(&url, token)?))
    }

    pub fn new(
        url: &str,
        token: String,
    ) -> std::result::Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut url = reqwest::Url::parse(url)?;
        if (url.scheme() != "https"
            && !(url.scheme() == "http"
                && url
                    .host_str()
                    .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "::1"))))
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || token.len() < 32
        {
            return Err("invalid publication provider configuration".into());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            url,
            token: Arc::new(token),
        })
    }

    async fn deliver(
        &self,
        request: &PublicationDelivery,
        first: bool,
    ) -> StoreResult<Option<ProviderPublicationReceipt>> {
        let endpoint = self
            .url
            .join(if first {
                "publications"
            } else {
                "publications/status"
            })
            .map_err(|_| StoreError::Conflict("invalid publication endpoint"))?;
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(self.token.as_str())
            .json(request)
            .send()
            .await
            .map_err(|_| StoreError::Conflict("publication delivery uncertain; reconcile"))?;
        if response.status() == reqwest::StatusCode::ACCEPTED {
            return Ok(None);
        }
        check(
            response.status().is_success(),
            "publication delivery uncertain; reconcile",
        )?;
        use futures_util::StreamExt;
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|_| StoreError::Conflict("publication receipt interrupted"))?;
            check(
                bytes.len() + chunk.len() <= 16 * 1024,
                "publication receipt too large",
            )?;
            bytes.extend_from_slice(&chunk);
        }
        let receipt: ProviderPublicationReceipt = serde_json::from_slice(&bytes)?;
        check(
            receipt.key == request.key
                && receipt.request_digest == request.request_digest()?
                && !receipt.provider_id.is_empty(),
            "provider receipt changed immutable publication",
        )?;
        match &request.plan.destination {
            Destination::Document { path, .. } => {
                let commit = receipt
                    .commit
                    .as_deref()
                    .ok_or(StoreError::Conflict("document receipt missing commit"))?;
                check(
                    commit.len() == 40
                        && commit.bytes().all(|b| b.is_ascii_hexdigit())
                        && receipt.resource_url
                            == format!(
                                "https://github.com/{}/blob/{commit}/{path}",
                                request.plan.repository
                            ),
                    "invalid document receipt",
                )?;
            }
            _ => check(
                receipt.commit.is_none()
                    && receipt.resource_url.starts_with(&format!(
                        "https://github.com/{}/issues/",
                        request.plan.repository
                    )),
                "invalid issue receipt",
            )?,
        }
        Ok(Some(receipt))
    }
}

/// The caller chooses a pinned slot only; it cannot replace its body or target.
pub async fn dispatch(
    pool: &PgPool,
    store: &DurableArtifactStore,
    provider: &PublicationProvider,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
    slot: &str,
) -> StoreResult<Option<Value>> {
    let mut tx = pool.begin().await?;
    let feature = load_feature(&mut tx, auth, feature_id).await?;
    let claim = feature
        .active_claim
        .as_ref()
        .ok_or(StoreError::Conflict("publication stage is not active"))?;
    let process: Uuid = claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid publication process"))?;
    let instance: Uuid = claim
        .workflow_instance_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid publication invocation"))?;
    let definition: Value = sqlx::query_scalar(
        "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(auth.host_id)
    .bind(process)
    .fetch_one(&mut *tx)
    .await?;
    let pinned: PinnedPublication = serde_json::from_value(
        definition
            .pointer("/document/metadata/developmentWorkflowPublication")
            .cloned()
            .ok_or(StoreError::Conflict("publication policy is not pinned"))?,
    )?;
    let selected = pinned
        .slots
        .get(slot)
        .ok_or(StoreError::Conflict("publication slot is not pinned"))?;
    let accepted = feature.accepted_results.last().ok_or(StoreError::Conflict(
        "publication requires an accepted candidate",
    ))?;
    check(
        accepted.accepted && accepted.feature_run_id == feature_id,
        "invalid accepted publication source",
    )?;
    let accepted_process: Uuid = accepted
        .claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid accepted process"))?;
    let bytes = verify_artifact(
        &mut tx,
        store,
        auth.host_id,
        accepted_process,
        &accepted.candidate.package,
    )
    .await?;
    let package: task_workspace::SnapshotPackage = serde_json::from_slice(&bytes)?;
    let verified = package
        .verified_receipt()
        .map_err(|_| StoreError::Conflict("publication source package corrupt"))?;
    check(
        package.feature_id == feature_id
            && verified.package_digest == accepted.candidate.candidate_digest
            && verified.checkpoint_digest == accepted.candidate.checkpoint_digest,
        "publication source candidate mismatch",
    )?;
    let file = package
        .repositories
        .get(&selected.source_repository)
        .and_then(|r| r.files.get(&selected.source_path))
        .ok_or(StoreError::Conflict("pinned publication source missing"))?;
    check(
        !file.executable && file.bytes.len() <= 64 * 1024,
        "publication source exceeds text bound",
    )?;
    let body = String::from_utf8(file.bytes.clone())
        .map_err(|_| StoreError::Conflict("publication source is not UTF-8"))?;
    let plan = PublicationPlan {
        feature_id: feature_id.into(),
        slot: slot.into(),
        revision: selected.revision,
        candidate_digest: accepted.candidate.candidate_digest.clone(),
        repository: selected.repository.clone(),
        destination: selected.destination.clone(),
        content: accepted.candidate.package.clone(),
    };
    let effect =
        PublicationEffect::prepare(&plan, &pinned.policy, &accepted.candidate.candidate_digest)?;
    let request = PublicationDelivery {
        key: identity(
            "publication-provider/v1",
            &[&auth.host_id.to_string(), &effect.id],
        )
        .trim_start_matches("sha256:")
        .into(),
        plan,
        body,
    };
    let task_name = format!("publication:{slot}:{}", selected.revision);
    let journal = PublicationJournalKey {
        host_id: auth.host_id,
        workflow_instance_id: instance,
        task_name: &task_name,
        effect: &effect,
    };
    // Read a saved intent before deadline checks: old intents may reconcile after
    // expiry/cancellation, but expiry cannot authorize a new write.
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_task_effect_t WHERE host_id=$1 AND workflow_instance_id=$2 AND task_name=$3 AND idempotency_key=$4)").bind(auth.host_id).bind(instance).bind(&task_name).bind(&effect.id).fetch_one(&mut *tx).await?;
    if !exists {
        check(
            feature.state == FeatureState::Active
                && feature.allowed_stage.kind == StageKind::Finalize
                && feature.budgets.deadline_epoch_seconds > Utc::now().timestamp() as u64,
            "publication stage expired or fenced",
        )?;
        check_vm_owner(&mut tx, auth.host_id, &feature).await?;
    }
    let decision = journal.claim(&mut tx).await?;
    tx.commit().await?;
    if let PublicationClaim::Confirmed(result) = decision {
        return Ok(Some(result));
    }
    let Some(receipt) = provider
        .deliver(&request, decision == PublicationClaim::Dispatch)
        .await?
    else {
        return Ok(None);
    };
    let proof = serde_json::to_vec(&receipt)?;
    let artifact_id = Uuid::from_bytes(
        hex::decode(&request.key[..32])
            .map_err(|_| StoreError::Conflict("invalid publication proof identity"))?
            .try_into()
            .map_err(|_| StoreError::Conflict("invalid publication proof identity"))?,
    );
    let mut tx = pool.begin().await?;
    // Owner remains required on late reconciliation; no new external write occurs.
    load_feature(&mut tx, auth, feature_id).await?;
    let digest = publish_artifact_in_transaction(
        &mut tx,
        store,
        ArtifactPublication {
            host_id: auth.host_id,
            artifact_id,
            execution_id: artifact_id,
            process_id: Some(process),
            task_id: None,
            logical_name: &task_name,
            media_type: "application/json",
            producer: "workflow-publication",
            policy_digest: claim.request_digest.trim_start_matches("sha256:"),
            retain_until: Utc::now() + Duration::days(30),
            bytes: &proof,
        },
    )
    .await
    .map_err(|_| StoreError::Conflict("publication proof persistence failed; reconcile"))?;
    let result = serde_json::json!({"receipt":receipt,"verification":ArtifactRef{id:artifact_id.to_string(),digest}});
    journal.confirm(&mut tx, &result).await?;
    tx.commit().await?;
    Ok(Some(result))
}
