//! Fixed terminal handoff for a reviewed design. Full implementation/final-review
//! delivery uses its separate acceptance policy; this action is explicitly pinned
//! to design qualification and cannot terminate an implementation feature.
use crate::{
    artifact_publish::{ArtifactPublication, publish_artifact_in_transaction},
    artifact_store::DurableArtifactStore,
    development_handoff::{AcceptStage, accept_stage, verify_artifact},
    development_store::*,
    invocation::AuthenticatedInvocationContext,
};
use chrono::{Duration, Utc};
use development_workflow_contract::*;
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub async fn complete_design(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    store: &DurableArtifactStore,
    feature_id: &str,
    expected_version: u64,
    operation_id: Uuid,
) -> StoreResult<FeatureRun> {
    check(!operation_id.is_nil(), "finalization operation id missing")?;
    let feature = load_feature(tx, auth, feature_id).await?;
    if feature.state == FeatureState::Completed {
        let saved: Option<Value>=sqlx::query_scalar("SELECT result FROM development_transition_t WHERE host_id=$1 AND feature_id=$2 AND operation_id=$3").bind(auth.host_id).bind(feature_id).bind(operation_id).fetch_optional(&mut **tx).await?;
        let saved: FeatureRun = serde_json::from_value(saved.ok_or(StoreError::Conflict(
            "finalization operation does not match",
        ))?)?;
        check(saved == feature, "finalization replay changed state")?;
        check(
            expected_version.checked_add(1) == Some(saved.version),
            "finalization replay input conflict",
        )?;
        return Ok(saved);
    }
    check(
        feature.version == expected_version
            && feature.state == FeatureState::Active
            && feature.allowed_stage.kind == StageKind::Finalize,
        "finalization version or stage stale",
    )?;
    let claim = feature
        .active_claim
        .clone()
        .ok_or(StoreError::Conflict("finalization claim missing"))?;
    let process: Uuid = claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid finalization process"))?;
    let definition: Value = sqlx::query_scalar(
        "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(auth.host_id)
    .bind(process)
    .fetch_one(&mut **tx)
    .await?;
    check(
        definition.pointer("/document/metadata/developmentWorkflowFinalizeAcceptedDesign")
            == Some(&Value::Bool(true)),
        "fixed design finalization is not pinned",
    )?;
    let source = feature
        .accepted_results
        .last()
        .ok_or(StoreError::Conflict("accepted design missing"))?;
    check(
        source.stage.kind == StageKind::Design
            && source.accepted
            && feature
                .accepted_results
                .iter()
                .all(|r| matches!(r.stage.kind, StageKind::Intake | StageKind::Design)),
        "fixed finalization requires a reviewed design-only feature",
    )?;
    let source_process: Uuid = source
        .claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid source process"))?;
    let bytes = verify_artifact(
        tx,
        store,
        auth.host_id,
        source_process,
        &source.candidate.package,
    )
    .await?;
    let mut package: task_workspace::SnapshotPackage = serde_json::from_slice(&bytes)?;
    let original = package
        .verified_receipt()
        .map_err(|_| StoreError::Conflict("accepted design corrupt"))?;
    check(
        original.package_digest == source.candidate.candidate_digest
            && package.feature_id == feature_id,
        "accepted design binding changed",
    )?;
    crate::publication_dispatch::verify_required(tx, store, auth, &feature, &definition).await?;
    package.stage_id = claim.stage_execution_id.clone();
    package.snapshot_id = identity("finalize-snapshot/v1", &[&claim.claim_id])
        .trim_start_matches("sha256:")
        .to_owned();
    let receipt = package
        .verified_receipt()
        .map_err(|_| StoreError::Conflict("finalization snapshot invalid"))?;
    let package_ref = bind(
        tx,
        store,
        auth,
        &claim,
        "finalize-package",
        &serde_json::to_vec(&package)?,
    )
    .await?;
    let mut repositories = BTreeMap::new();
    for (name, repository) in &package.repositories {
        let manifest = bind(
            tx,
            store,
            auth,
            &claim,
            &format!("finalize-manifest:{name}"),
            &serde_json::to_vec(repository)?,
        )
        .await?;
        repositories.insert(
            name.clone(),
            RepositorySnapshot {
                base_commit: repository.base_commit.clone(),
                tree: repository.tree.clone(),
                content_manifest: manifest,
            },
        );
    }
    let checks = crate::snapshot_validation::evaluate(&definition, &package)
        .map_err(StoreError::Conflict)?
        .ok_or(StoreError::Conflict("finalization fixed checks missing"))?;
    let mut passed = BTreeMap::new();
    for (name, ok) in checks {
        check(ok, "finalization fixed validation failed")?;
        let proof = bind(
            tx,
            store,
            auth,
            &claim,
            &format!("finalize-check:{name}"),
            &serde_json::to_vec(
                &serde_json::json!({"candidate":receipt.package_digest,"check":name,"passed":true}),
            )?,
        )
        .await?;
        passed.insert(name, proof);
    }
    let result = StageResult {
        feature_run_id: feature_id.into(),
        claim: claim.clone(),
        stage: feature.allowed_stage.clone(),
        inputs: feature.accepted_inputs.clone(),
        outputs: BTreeMap::from([("design".into(), package_ref.clone())]),
        candidate: CandidateSnapshot {
            feature_run_id: feature_id.into(),
            stage_execution_id: claim.stage_execution_id.clone(),
            task_id: package.task_id.clone(),
            candidate_digest: receipt.package_digest.clone(),
            checkpoint_digest: receipt.checkpoint_digest,
            repositories,
            package: package_ref,
        },
        validation: ValidationReceipt {
            candidate: receipt.package_digest,
            required_checks: passed.keys().cloned().collect(),
            passed_checks: passed,
        },
        review_ids: BTreeSet::new(),
        finding_ids: BTreeSet::new(),
        native_checkpoint: None,
        publication_receipts: vec![],
        accepted: true,
    };
    accept_stage(
        tx,
        auth,
        store,
        &AcceptStage {
            operation_id,
            expected_version,
            result,
            next_stage: None,
        },
    )
    .await
}

async fn bind(
    tx: &mut Transaction<'_, Postgres>,
    store: &DurableArtifactStore,
    auth: &AuthenticatedInvocationContext<'_>,
    claim: &StageClaimReceipt,
    name: &str,
    bytes: &[u8],
) -> StoreResult<ArtifactRef> {
    let identity = identity(
        "finalize-artifact/v1",
        &[&auth.host_id.to_string(), &claim.claim_id, name],
    );
    let raw: [u8; 16] = hex::decode(&identity[7..39])
        .map_err(|_| StoreError::Conflict("invalid artifact identity"))?
        .try_into()
        .map_err(|_| StoreError::Conflict("invalid artifact identity"))?;
    let id = Uuid::from_bytes(raw);
    let digest = publish_artifact_in_transaction(
        tx,
        store,
        ArtifactPublication {
            host_id: auth.host_id,
            artifact_id: id,
            execution_id: id,
            process_id: Some(
                claim
                    .process_id
                    .parse()
                    .map_err(|_| StoreError::Conflict("invalid process"))?,
            ),
            task_id: None,
            logical_name: name,
            media_type: "application/json",
            producer: "workflow-design-finalize",
            policy_digest: claim.request_digest.trim_start_matches("sha256:"),
            retain_until: Utc::now() + Duration::days(30),
            bytes,
        },
    )
    .await
    .map_err(|_| StoreError::Conflict("finalization artifact persistence failed"))?;
    Ok(ArtifactRef {
        id: id.to_string(),
        digest,
    })
}
