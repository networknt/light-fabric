//! Trusted review persistence and atomic stage handoffs. No worker-facing API
//! accepts a replacement ledger, acceptance policy, or feature budget.
use crate::{
    artifact_store::DurableArtifactStore, development_store::*,
    invocation::AuthenticatedInvocationContext,
};
use development_workflow_contract::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;
use workflow_invocation_contract::canonical_sha256;

async fn ledger(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    feature: &str,
) -> StoreResult<FindingLedger> {
    let value: Option<Value> = sqlx::query_scalar(
        "SELECT finding_ledger FROM development_feature_t WHERE host_id=$1 AND feature_id=$2",
    )
    .bind(host)
    .bind(feature)
    .fetch_one(&mut **tx)
    .await?;
    Ok(match value {
        Some(value) => serde_json::from_value(value)?,
        None => FindingLedger::new(feature.into()),
    })
}
async fn save_ledger(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    value: &FindingLedger,
) -> StoreResult<()> {
    sqlx::query(
        "UPDATE development_feature_t SET finding_ledger=$3 WHERE host_id=$1 AND feature_id=$2",
    )
    .bind(host)
    .bind(&value.feature_run_id)
    .bind(serde_json::to_value(value)?)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Workflow allocates a review before dispatch, under the same feature lock.
pub async fn allocate_review(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    binding: ReviewBinding,
) -> StoreResult<()> {
    let feature = load_feature(tx, auth, &binding.feature_run_id).await?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    check(
        feature.state == FeatureState::Active
            && feature
                .active_claim
                .as_ref()
                .is_some_and(|c| c.stage_execution_id == binding.stage_execution_id),
        "review allocation has stale stage owner",
    )?;
    let mut ledger = ledger(tx, auth.host_id, &feature.feature_run_id).await?;
    ledger.allocate_review(binding)?;
    save_ledger(tx, auth.host_id, &ledger).await
}

/// The reconciler's completed review turn is the source of the result. A caller
/// cannot substitute arbitrary JSON or change a review binding after dispatch.
pub async fn record_review(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
    logical_turn_id: &str,
) -> StoreResult<ReviewReceipt> {
    let feature = load_feature(tx, auth, feature_id).await?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    let claim = feature
        .active_claim
        .as_ref()
        .ok_or(StoreError::Conflict("stage is not active"))?;
    check(feature.state == FeatureState::Active, "stage is fenced")?;
    let (charge, result): (Value, Option<Value>) = sqlx::query_as("SELECT charge,result FROM development_turn_t WHERE host_id=$1 AND feature_id=$2 AND logical_turn_id=$3 AND claim_id=$4")
        .bind(auth.host_id).bind(feature_id).bind(logical_turn_id).bind(&claim.claim_id).fetch_one(&mut **tx).await?;
    let charge: TurnCharge = serde_json::from_value(charge)?;
    check(charge.kind == TurnKind::Review, "turn is not a review")?;
    let output = result.ok_or(StoreError::Conflict("review turn is unresolved"))?;
    let result: ReviewResult = serde_json::from_value(
        output
            .get("reviewResult")
            .cloned()
            .ok_or(StoreError::Conflict("review result missing"))?,
    )?;
    check(
        result.binding.stage_execution_id == claim.stage_execution_id
            && result.binding.review_id == logical_turn_id,
        "review result is bound to another turn",
    )?;
    let mut ledger = ledger(tx, auth.host_id, feature_id).await?;
    let receipt = ledger.apply_review(result)?;
    save_ledger(tx, auth.host_id, &ledger).await?;
    Ok(receipt)
}

/// Reads metadata and actual content. A digest string or a prior VERIFIED bit
/// alone is insufficient; deletion and corruption must fail the handoff.
pub(crate) async fn verify_artifact(
    tx: &mut Transaction<'_, Postgres>,
    store: &DurableArtifactStore,
    host: Uuid,
    process: Uuid,
    artifact: &ArtifactRef,
) -> StoreResult<Vec<u8>> {
    artifact.validate()?;
    let id: Uuid = artifact
        .id
        .parse()
        .map_err(|_| StoreError::Conflict("durable artifact ID must be a UUID"))?;
    let size: Option<i64> = sqlx::query_scalar("SELECT size_bytes FROM workflow_artifact_t WHERE host_id=$1 AND artifact_id=$2 AND process_id=$3 AND content_digest=$4 AND promotion_state='BOUND' AND verification_state='VERIFIED' AND deletion_state='RETAINED' AND (legal_hold OR retain_until_ts>clock_timestamp()) FOR SHARE")
        .bind(host).bind(id).bind(process).bind(&artifact.digest).fetch_optional(&mut **tx).await?;
    let size = size.ok_or(StoreError::Conflict(
        "artifact is missing, expired, unbound or belongs to another process",
    ))?;
    check(
        (0..=32 * 1024 * 1024).contains(&size),
        "artifact exceeds handoff bound",
    )?;
    let bytes = store
        .read_verified(&host.to_string(), &artifact.digest, size as usize)
        .await
        .map_err(|_| StoreError::Conflict("artifact content is missing or corrupt"))?;
    check(bytes.len() == size as usize, "artifact size mismatch")?;
    Ok(bytes)
}

async fn verify_candidate(
    tx: &mut Transaction<'_, Postgres>,
    store: &DurableArtifactStore,
    host: Uuid,
    process: Uuid,
    candidate: &CandidateSnapshot,
) -> StoreResult<()> {
    let bytes = verify_artifact(tx, store, host, process, &candidate.package).await?;
    let package: task_workspace::SnapshotPackage = serde_json::from_slice(&bytes)?;
    let copy = package.clone();
    let receipt = tokio::task::spawn_blocking(move || copy.verified_receipt())
        .await
        .map_err(|_| StoreError::Conflict("snapshot verification task failed"))?
        .map_err(|_| StoreError::Conflict("snapshot package is invalid"))?;
    check(
        receipt.package_digest == candidate.package.digest
            && receipt.package_digest == candidate.candidate_digest
            && receipt.checkpoint_digest == candidate.checkpoint_digest
            && package.feature_id == candidate.feature_run_id
            && package.stage_id == candidate.stage_execution_id
            && package.task_id == candidate.task_id
            && package.repositories.len() == candidate.repositories.len(),
        "candidate package binding mismatch",
    )?;
    for (name, reference) in &candidate.repositories {
        let retained = package
            .repositories
            .get(name)
            .ok_or(StoreError::Conflict("candidate repository missing"))?;
        check(
            retained.base_commit == reference.base_commit && retained.tree == reference.tree,
            "candidate tree/base mismatch",
        )?;
        let manifest =
            verify_artifact(tx, store, host, process, &reference.content_manifest).await?;
        let manifest: task_workspace::SnapshotRepository = serde_json::from_slice(&manifest)?;
        check(manifest == *retained, "candidate content manifest mismatch")?;
    }
    Ok(())
}

/// Host-only quiescence is local; runner tasks additionally require the exact
/// fence recorded during Controller result acceptance. Unbridged Agent jobs stay
/// blocked: a terminal invocation alone does not prove a remote worker stopped.
async fn quiescent(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    claim: &StageClaimReceipt,
) -> StoreResult<()> {
    let process: Uuid = claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid process ID"))?;
    let run: Uuid = claim
        .workflow_instance_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid run ID"))?;
    let safe: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_invocation_t i JOIN process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id WHERE i.host_id=$1 AND i.workflow_instance_id=$2 AND i.process_id=$3 AND i.state='COMPLETED' AND p.status_code='C') AND NOT EXISTS(SELECT 1 FROM task_info_t t WHERE host_id=$1 AND process_id=$3 AND (status_code IN ('A','W') OR locked='Y' OR ((scheduling_request_id IS NOT NULL OR task_type NOT IN ('set','assert','switch','ask')) AND (accepted_attempt IS NULL OR NOT EXISTS(SELECT 1 FROM development_execution_fence_t f WHERE f.host_id=t.host_id AND f.task_id=t.task_id AND f.claim_id=$4))))) AND NOT EXISTS(SELECT 1 FROM development_turn_t WHERE host_id=$1 AND claim_id=$4 AND result IS NULL)")
        .bind(host).bind(run).bind(process).bind(&claim.claim_id).fetch_one(&mut **tx).await?;
    check(safe, "stage execution is not confirmed quiescent")?;
    let unresolved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM workflow_task_effect_t WHERE host_id=$1
         AND workflow_instance_id=$2 AND (effect_state<>'confirmed' OR result IS NULL))",
    )
    .bind(host)
    .bind(run)
    .fetch_one(&mut **tx)
    .await?;
    check(!unresolved, "stage fixed effects are not reconciled")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptStage {
    pub operation_id: Uuid,
    pub expected_version: u64,
    pub result: StageResult,
    pub next_stage: Option<StageSelector>,
}

async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    feature: &str,
    operation: Uuid,
    digest: &str,
) -> StoreResult<Option<FeatureRun>> {
    let previous: Option<(String,Value)> = sqlx::query_as("SELECT request_digest,result FROM development_transition_t WHERE host_id=$1 AND feature_id=$2 AND operation_id=$3")
        .bind(host).bind(feature).bind(operation).fetch_optional(&mut **tx).await?;
    if let Some((old, result)) = previous {
        check(old == digest, "transition replay input conflict")?;
        return Ok(Some(serde_json::from_value(result)?));
    }
    Ok(None)
}
async fn save_transition(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    operation: Uuid,
    digest: &str,
    feature: &FeatureRun,
) -> StoreResult<()> {
    save_feature(tx, host, feature).await?;
    sqlx::query("INSERT INTO development_transition_t(host_id,feature_id,operation_id,request_digest,result) VALUES($1,$2,$3,$4,$5)")
        .bind(host).bind(&feature.feature_run_id).bind(operation).bind(digest).bind(serde_json::to_value(feature)?).execute(&mut **tx).await?;
    Ok(())
}

pub async fn accept_stage(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    store: &DurableArtifactStore,
    request: &AcceptStage,
) -> StoreResult<FeatureRun> {
    let mut feature = load_feature(tx, auth, &request.result.feature_run_id).await?;
    check(
        !request.operation_id.is_nil(),
        "transition operation ID is required",
    )?;
    let digest = canonical_sha256(&json!({"kind":"accept","request":request}))
        .map_err(|_| StoreError::Conflict("invalid acceptance fingerprint"))?;
    if let Some(old) = replay(
        tx,
        auth.host_id,
        &feature.feature_run_id,
        request.operation_id,
        &digest,
    )
    .await?
    {
        return Ok(old);
    }
    check(
        feature.version == request.expected_version && feature.state == FeatureState::Active,
        "acceptance version or state is stale",
    )?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    let process: Uuid = request
        .result
        .claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid process ID"))?;
    let definition: Value = sqlx::query_scalar(
        "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(auth.host_id)
    .bind(process)
    .fetch_one(&mut **tx)
    .await?;
    let policy: StageAcceptancePolicy = serde_json::from_value(
        definition
            .pointer("/document/metadata/developmentWorkflowAcceptance")
            .cloned()
            .ok_or(StoreError::Conflict("pinned acceptance policy missing"))?,
    )?;
    let successors: Vec<StageSelector> = serde_json::from_value(
        definition
            .pointer("/document/metadata/developmentWorkflowSuccessors")
            .cloned()
            .ok_or(StoreError::Conflict("pinned successors missing"))?,
    )?;
    match &request.next_stage {
        Some(next) => {
            next.validate()?;
            check(
                successors.contains(next),
                "successor is not allowed by pinned definition",
            )?;
        }
        None => check(
            request.result.stage.kind == StageKind::Finalize
                && successors.is_empty()
                && definition.pointer("/document/metadata/developmentWorkflowTerminal")
                    == Some(&Value::Bool(true)),
            "only a pinned terminal finalize stage may complete the feature",
        )?,
    }
    let ledger = ledger(tx, auth.host_id, &feature.feature_run_id).await?;
    let signoff: Option<Value> = sqlx::query_scalar(
        "SELECT signoff FROM development_stage_t WHERE host_id=$1 AND claim_id=$2",
    )
    .bind(auth.host_id)
    .bind(&request.result.claim.claim_id)
    .fetch_one(&mut **tx)
    .await?;
    let signoff: Option<DesignSignoff> = signoff.map(serde_json::from_value).transpose()?;
    request
        .result
        .validate_acceptance(&feature, &policy, &ledger, signoff.as_ref())?;
    quiescent(tx, auth.host_id, &request.result.claim).await?;
    crate::publication_dispatch::verify_required(tx, store, auth, &feature, &definition).await?;
    verify_candidate(tx, store, auth.host_id, process, &request.result.candidate).await?;
    if policy.required_reviewers.is_empty() {
        check(
            request.result.stage.kind == StageKind::Finalize
                && definition
                    .pointer("/document/metadata/developmentWorkflowFinalizeAcceptedDesign")
                    == Some(&Value::Bool(true)),
            "review-free finalization must be explicitly pinned",
        )?;
        let source = feature
            .accepted_results
            .last()
            .ok_or(StoreError::Conflict("reviewed design missing"))?;
        let source_process: Uuid = source
            .claim
            .process_id
            .parse()
            .map_err(|_| StoreError::Conflict("invalid reviewed design process"))?;
        let before: task_workspace::SnapshotPackage = serde_json::from_slice(
            &verify_artifact(
                tx,
                store,
                auth.host_id,
                source_process,
                &source.candidate.package,
            )
            .await?,
        )?;
        let after: task_workspace::SnapshotPackage = serde_json::from_slice(
            &verify_artifact(
                tx,
                store,
                auth.host_id,
                process,
                &request.result.candidate.package,
            )
            .await?,
        )?;
        check(
            before.workspace_id == after.workspace_id
                && before.task_id == after.task_id
                && before.feature_id == after.feature_id
                && before.checkpoint == after.checkpoint
                && before.repositories == after.repositories
                && request.result.outputs
                    == std::collections::BTreeMap::from([(
                        "design".into(),
                        request.result.candidate.package.clone(),
                    )]),
            "review-free finalization changed accepted design",
        )?;
    }
    for (name, artifact) in &request.result.validation.passed_checks {
        let bytes = verify_artifact(tx, store, auth.host_id, process, artifact).await?;
        let evidence: Value = serde_json::from_slice(&bytes)?;
        check(
            evidence
                == json!({"candidate":request.result.candidate.candidate_digest,"check":name,"passed":true}),
            "fixed validation evidence does not match check and candidate",
        )?;
    }
    for publication in &request.result.publication_receipts {
        let bytes =
            verify_artifact(tx, store, auth.host_id, process, &publication.verification).await?;
        let evidence: Value = serde_json::from_slice(&bytes)?;
        let mut expected = serde_json::to_value(publication)?;
        expected
            .as_object_mut()
            .expect("typed publication object")
            .remove("verification");
        check(
            evidence == expected,
            "publication evidence does not match receipt",
        )?;
    }
    let mut artifacts = Vec::new();
    artifacts.extend(request.result.outputs.values());
    artifacts.extend(
        request
            .result
            .publication_receipts
            .iter()
            .map(|p| &p.verification),
    );
    artifacts.extend(signoff.iter().map(|s| &s.authority));
    artifacts.extend(request.result.native_checkpoint.iter());
    for id in &request.result.review_ids {
        let review = &ledger.reviews[id].result;
        artifacts.push(&review.evidence);
        artifacts.extend(review.existing_findings.iter().map(|f| &f.evidence));
        artifacts.extend(review.new_findings.iter().map(|f| &f.evidence));
    }
    let mut seen = BTreeSet::new();
    for artifact in artifacts {
        if seen.insert((&artifact.id, &artifact.digest)) {
            verify_artifact(tx, store, auth.host_id, process, artifact).await?;
        }
    }
    feature.version = feature
        .version
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(StoreError::Conflict("feature version exhausted"))?;
    feature
        .accepted_inputs
        .extend(request.result.outputs.clone());
    feature.accepted_results.push(request.result.clone());
    feature.active_claim = None;
    feature.transition_id = Uuid::now_v7().to_string();
    if let Some(next) = &request.next_stage {
        feature.allowed_stage = next.clone();
        feature.state = FeatureState::ReadyForNextStage;
    } else {
        check(
            feature.claims.values().all(|claim| {
                feature
                    .accepted_results
                    .iter()
                    .any(|result| result.accepted && &result.claim == claim)
            }),
            "unsettled historical stage retains VM reservation",
        )?;
        let changed = sqlx::query("UPDATE development_vm_t SET feature_id=NULL,generation=generation+1 WHERE host_id=$1 AND vm_id=$2 AND feature_id=$3 AND generation=$4")
            .bind(auth.host_id).bind(&feature.vm.vm_id).bind(&feature.feature_run_id)
            .bind(feature.vm.generation as i64).execute(&mut **tx).await?.rows_affected();
        check(changed == 1, "VM release ownership changed")?;
        feature.state = FeatureState::Completed;
        feature.vm.released = true;
        feature.vm.release_pending = false;
    }
    sqlx::query("UPDATE development_stage_t SET result=$3 WHERE host_id=$1 AND claim_id=$2 AND result IS NULL")
        .bind(auth.host_id).bind(&request.result.claim.claim_id).bind(serde_json::to_value(&request.result)?).execute(&mut **tx).await?;
    save_transition(tx, auth.host_id, request.operation_id, &digest, &feature).await?;
    Ok(feature)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplanStage {
    pub operation_id: Uuid,
    pub feature_id: String,
    pub expected_version: u64,
    pub stage: StageSelector,
    pub inputs: BTreeMap<String, ArtifactRef>,
    pub reason: String,
}

/// Reopen an accepted stage using a subset of current accepted inputs. In
/// particular, superseded downstream outputs disappear from the current map.
pub async fn replan_stage(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    request: &ReplanStage,
) -> StoreResult<FeatureRun> {
    let mut feature = load_feature(tx, auth, &request.feature_id).await?;
    let digest = canonical_sha256(&json!({"kind":"replan","request":request}))
        .map_err(|_| StoreError::Conflict("invalid replan fingerprint"))?;
    check(
        !request.operation_id.is_nil()
            && !request.reason.trim().is_empty()
            && request.reason.len() <= 2000,
        "invalid replan operation or reason",
    )?;
    if let Some(old) = replay(
        tx,
        auth.host_id,
        &feature.feature_run_id,
        request.operation_id,
        &digest,
    )
    .await?
    {
        return Ok(old);
    }
    check_vm_owner(tx, auth.host_id, &feature).await?;
    request.stage.validate()?;
    check(
        feature.version == request.expected_version
            && feature.active_claim.is_none()
            && feature.state == FeatureState::ReadyForNextStage,
        "replan requires an idle current feature",
    )?;
    let previous = feature
        .accepted_results
        .iter()
        .rev()
        .find(|r| r.stage == request.stage)
        .ok_or(StoreError::Conflict(
            "stage has no accepted revision to reopen",
        ))?;
    check(
        request.inputs == previous.inputs
            && request
                .inputs
                .iter()
                .all(|(key, value)| feature.accepted_inputs.get(key) == Some(value)),
        "replan inputs are stale or not the accepted stage inputs",
    )?;
    feature.version = feature
        .version
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(StoreError::Conflict("feature version exhausted"))?;
    feature.accepted_inputs = request.inputs.clone();
    feature.allowed_stage = request.stage.clone();
    feature.transition_id = Uuid::now_v7().to_string();
    save_transition(tx, auth.host_id, request.operation_id, &digest, &feature).await?;
    Ok(feature)
}
