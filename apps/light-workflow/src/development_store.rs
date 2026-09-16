//! Transactional development-stage ownership. Callers supply authenticated
//! identity and a validated, pinned invocation; no worker output grants authority.
use crate::invocation::{
    AcceptOutcome, AuthenticatedInvocationContext, InvocationAcceptError, PreparedInvocationStart,
    accept_invocation_in,
};
use chrono::Utc;
use development_workflow_contract::*;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
use workflow_invocation_contract::{StartInvocationRequest, canonical_sha256};

pub const MIGRATION_SQL: &str = include_str!(
    "../../../crates/workflow-store/migrations/workflow-postgres/0008_development_workflow.sql"
);

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("development workflow conflict: {0}")]
    Conflict(&'static str),
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Invocation(#[from] InvocationAcceptError),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
pub(crate) type StoreResult<T> = std::result::Result<T, StoreError>;

pub(crate) fn check(condition: bool, message: &'static str) -> StoreResult<()> {
    if condition {
        Ok(())
    } else {
        Err(StoreError::Conflict(message))
    }
}

/// Reserved document names also fail closed if a publication loses its marker.
pub fn is_development_definition(definition: &Value) -> bool {
    definition
        .pointer("/document/metadata/developmentWorkflowStage")
        .is_some()
        || matches!(
            definition.pointer("/document/name").and_then(Value::as_str),
            Some(
                "feature-intake"
                    | "feature-design"
                    | "feature-plan"
                    | "feature-implement"
                    | "feature-finalize"
            )
        )
}

/// Creates only a pristine feature. The VM generation is database-owned, not
/// selected by the request. All operations use the caller's transaction.
pub async fn create_feature(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    seed: &FeatureRun,
) -> StoreResult<FeatureRun> {
    let creation_digest = canonical_sha256(&serde_json::to_value(seed)?)
        .map_err(|_| StoreError::Conflict("invalid feature creation fingerprint"))?;
    check(
        seed.schema_version == 1
            && seed.version == 1
            && seed.state == FeatureState::ReadyForNextStage
            && seed.active_claim.is_none()
            && seed.claims.is_empty()
            && seed.accepted_results.is_empty()
            && seed.budgets.charges.is_empty()
            && seed.budgets.maximum_turns > 0
            && seed.budgets.maximum_remediation_rounds > 0
            && seed.budgets.deadline_epoch_seconds > Utc::now().timestamp() as u64
            && !seed.feature_run_id.is_empty()
            && !seed.transition_id.is_empty()
            && seed.vm.feature_run_id == seed.feature_run_id
            && !seed.vm.released
            && !seed.vm.release_pending,
        "feature must start pristine with a live budget",
    )?;
    seed.allowed_stage.validate()?;
    seed.vm.runner_binding.validate()?;
    for artifact in seed.accepted_inputs.values() {
        artifact.validate()?;
    }
    // Stable VM row serializes competing feature acquisitions and survives release.
    sqlx::query("INSERT INTO development_vm_t(host_id,vm_id,generation) VALUES($1,$2,1) ON CONFLICT DO NOTHING")
        .bind(auth.host_id).bind(&seed.vm.vm_id).execute(&mut **tx).await?;
    let (generation, holder): (i64, Option<String>) = sqlx::query_as(
        "SELECT generation,feature_id FROM development_vm_t WHERE host_id=$1 AND vm_id=$2 FOR UPDATE")
        .bind(auth.host_id).bind(&seed.vm.vm_id).fetch_one(&mut **tx).await?;
    if holder.as_deref() == Some(&seed.feature_run_id) {
        let saved: Option<(String, Value)> = sqlx::query_as("SELECT creation_digest,record FROM development_feature_t WHERE host_id=$1 AND feature_id=$2 AND principal_subject=$3 AND end_user_subject=$4")
            .bind(auth.host_id).bind(&seed.feature_run_id).bind(auth.principal_subject).bind(auth.end_user_subject)
            .fetch_optional(&mut **tx).await?;
        let (digest, record) =
            saved.ok_or(StoreError::Conflict("feature unavailable for owner"))?;
        check(
            digest == creation_digest,
            "feature creation replay changed inputs",
        )?;
        return Ok(serde_json::from_value(record)?);
    }
    check(holder.is_none(), "VM is held by another feature")?;
    let mut feature = seed.clone();
    feature.vm.generation =
        u64::try_from(generation).map_err(|_| StoreError::Conflict("invalid VM generation"))?;
    feature.vm.acquired_epoch_seconds = Utc::now().timestamp() as u64;
    sqlx::query("INSERT INTO development_feature_t(host_id,feature_id,principal_subject,end_user_subject,vm_id,generation,version,record,creation_digest) VALUES($1,$2,$3,$4,$5,$6,1,$7,$8)")
        .bind(auth.host_id).bind(&feature.feature_run_id).bind(auth.principal_subject).bind(auth.end_user_subject)
        .bind(&feature.vm.vm_id).bind(generation).bind(serde_json::to_value(&feature)?).bind(creation_digest).execute(&mut **tx).await?;
    sqlx::query("UPDATE development_vm_t SET feature_id=$3 WHERE host_id=$1 AND vm_id=$2")
        .bind(auth.host_id)
        .bind(&feature.vm.vm_id)
        .bind(&feature.feature_run_id)
        .execute(&mut **tx)
        .await?;
    Ok(feature)
}

/// Owner-scoped lock; a feature ID is never an authorization token.
pub async fn load_feature(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
) -> StoreResult<FeatureRun> {
    let record: Option<Value> = sqlx::query_scalar(
        "SELECT record FROM development_feature_t WHERE host_id=$1 AND feature_id=$2 AND principal_subject=$3 AND end_user_subject=$4 FOR UPDATE")
        .bind(auth.host_id).bind(feature_id).bind(auth.principal_subject).bind(auth.end_user_subject)
        .fetch_optional(&mut **tx).await?;
    Ok(serde_json::from_value(record.ok_or(
        StoreError::Conflict("feature unavailable for owner"),
    )?)?)
}

pub(crate) async fn save_feature(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    feature: &FeatureRun,
) -> StoreResult<()> {
    let version = i64::try_from(feature.version)
        .map_err(|_| StoreError::Conflict("feature version exhausted"))?;
    sqlx::query("UPDATE development_feature_t SET version=$3,record=$4,updated_ts=clock_timestamp() WHERE host_id=$1 AND feature_id=$2")
        .bind(host).bind(&feature.feature_run_id).bind(version).bind(serde_json::to_value(feature)?)
        .execute(&mut **tx).await?;
    Ok(())
}

/// Claim replay precedes current version/state checks. Different transport run
/// IDs may retry the same logical start, but input/budget/authority changes may not.
pub async fn claim_and_start(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    claim: &StageClaim,
    request: &StartInvocationRequest,
    prepared: &PreparedInvocationStart<'_>,
) -> StoreResult<StageClaimReceipt> {
    let mut feature = load_feature(tx, auth, &claim.feature_run_id).await?;
    let invocation_digest = canonical_sha256(&json!({
        "tool":request.stable_tool_ref,"definition":request.workflow_definition_id,
        "version":request.workflow_version,"definitionDigest":request.definition_digest,
        "schemaDigest":request.schema_digest,"policyDigest":request.policy_digest,
        "responsePolicyDigest":request.response_policy_digest,"input":request.input,
        "mode":request.mode,"class":request.execution_class,"budget":request.budget,
        "deadline":request.deadline_ts,"parent":request.parent_action_id,
        "grant":request.renewable_grant_id,"depth":request.permit_depth,
        "cancellation":request.cancellation_policy,"claims":request.caller_claims,
        "bindingId":prepared.binding_id
    }))
    .map_err(|_| StoreError::Conflict("invalid invocation fingerprint"))?;
    let (claim_id, request_digest) = match feature
        .check_claim(claim, Utc::now().timestamp() as u64)?
    {
        ClaimDecision::Replay(receipt) => {
            let saved: String = sqlx::query_scalar("SELECT invocation_digest FROM development_stage_t WHERE host_id=$1 AND claim_id=$2")
                .bind(auth.host_id).bind(&receipt.claim_id).fetch_one(&mut **tx).await?;
            check(
                saved == invocation_digest,
                "stage replay changed invocation",
            )?;
            return Ok(receipt);
        }
        ClaimDecision::Admit {
            claim_id,
            request_digest,
        } => (claim_id, request_digest),
    };
    let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM development_vm_t WHERE host_id=$1 AND vm_id=$2 AND feature_id=$3 AND generation=$4)")
        .bind(auth.host_id).bind(&feature.vm.vm_id).bind(&feature.feature_run_id).bind(feature.vm.generation as i64)
        .fetch_one(&mut **tx).await?;
    check(current, "stale VM reservation")?;
    let stage: StageSelector = serde_json::from_value(
        prepared
            .definition_snapshot
            .pointer("/document/metadata/developmentWorkflowStage")
            .cloned()
            .ok_or(StoreError::Conflict(
                "definition lacks development stage marker",
            ))?,
    )?;
    check(
        stage == claim.stage
            && claim.definition.id == request.workflow_definition_id.to_string()
            && claim.definition.digest == request.definition_digest
            && claim.workspace_binding == feature.vm.runner_binding
            && request
                .input
                .get("stageClaim")
                .cloned()
                .and_then(|value| serde_json::from_value::<StageClaim>(value).ok())
                .as_ref()
                == Some(claim)
            && request.deadline_ts.timestamp() >= 0
            && request.deadline_ts.timestamp() as u64 <= claim.deadline_epoch_seconds,
        "invocation does not match pinned stage claim",
    )?;
    check(
        format!(
            "sha256:{}",
            execution_runner_protocol::canonical_sha256(prepared.definition_snapshot)
                .map_err(|_| StoreError::Conflict("invalid definition"))?
        ) == claim.definition.digest,
        "definition content digest mismatch",
    )?;
    // The process/initial task, invocation and stage ownership have one commit.
    feature.version = feature
        .version
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(StoreError::Conflict("feature version exhausted"))?;
    let outcome = accept_invocation_in(tx, auth, request, prepared).await?;
    check(
        matches!(outcome, AcceptOutcome::Accepted { .. }),
        "invocation already belongs to a different claim",
    )?;
    let receipt = StageClaimReceipt {
        claim_id: claim_id.clone(),
        request_digest,
        stage_execution_id: Uuid::now_v7().to_string(),
        workflow_instance_id: request.workflow_instance_id.to_string(),
        process_id: prepared.process_id.to_string(),
        feature_version: feature.version,
    };
    sqlx::query("INSERT INTO development_stage_t(host_id,claim_id,feature_id,process_id,workflow_instance_id,invocation_digest,request,receipt) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(auth.host_id).bind(&claim_id).bind(&feature.feature_run_id).bind(prepared.process_id)
        .bind(request.workflow_instance_id).bind(invocation_digest).bind(serde_json::to_value(claim)?)
        .bind(serde_json::to_value(&receipt)?).execute(&mut **tx).await?;
    feature.claims.insert(claim_id, receipt.clone());
    feature.active_claim = Some(receipt.clone());
    feature.state = FeatureState::Active;
    save_feature(tx, auth.host_id, &feature).await?;
    Ok(receipt)
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnReservation {
    /// Commit this intent before sending; only this first caller may dispatch.
    Dispatch {
        token: Uuid,
    },
    /// An intent exists without a result. Reconcile; never resend a model turn.
    Uncertain {
        token: Uuid,
    },
    Replay {
        result: Value,
    },
}

pub fn stage_budget_scope(stage: &StageSelector) -> StoreResult<String> {
    stage.validate()?;
    Ok(identity(
        "development-stage-budget/v1",
        &[&serde_json::to_string(stage)?],
    ))
}

pub(crate) async fn check_vm_owner(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    feature: &FeatureRun,
) -> StoreResult<()> {
    let current: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM development_vm_t WHERE host_id=$1 AND vm_id=$2 AND feature_id=$3 AND generation=$4)")
        .bind(host).bind(&feature.vm.vm_id).bind(&feature.feature_run_id).bind(feature.vm.generation as i64)
        .fetch_one(&mut **tx).await?;
    check(
        current && !feature.vm.released && !feature.vm.release_pending,
        "VM reservation no longer current",
    )
}

pub async fn reserve_turn(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
    claim: &StageClaimReceipt,
    charge: &TurnCharge,
    request_digest: &str,
) -> StoreResult<TurnReservation> {
    let mut feature = load_feature(tx, auth, feature_id).await?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    check(
        feature.state == FeatureState::Active
            && feature.active_claim.as_ref() == Some(claim)
            && charge.stage_execution_id == claim.stage_execution_id
            && charge.budget_scope == stage_budget_scope(&feature.allowed_stage)?
            && digest_valid(request_digest),
        "turn has stale stage ownership or invalid digest",
    )?;
    let previous: Option<(Uuid, String, Value, Option<Value>)> = sqlx::query_as(
        "SELECT dispatch_token,request_digest,charge,result FROM development_turn_t WHERE host_id=$1 AND feature_id=$2 AND logical_turn_id=$3")
        .bind(auth.host_id).bind(feature_id).bind(&charge.logical_turn_id).fetch_optional(&mut **tx).await?;
    if let Some((token, digest, old_charge, result)) = previous {
        check(
            digest == request_digest && old_charge == serde_json::to_value(charge)?,
            "turn replay input conflict",
        )?;
        return Ok(match result {
            Some(result) => TurnReservation::Replay { result },
            None => TurnReservation::Uncertain { token },
        });
    }
    let original: Value = sqlx::query_scalar(
        "SELECT request FROM development_stage_t WHERE host_id=$1 AND claim_id=$2",
    )
    .bind(auth.host_id)
    .bind(&claim.claim_id)
    .fetch_one(&mut **tx)
    .await?;
    let original: StageClaim = serde_json::from_value(original)?;
    check(
        (Utc::now().timestamp() as u64) < original.deadline_epoch_seconds,
        "stage deadline expired",
    )?;
    feature
        .budgets
        .charge(charge.clone(), Utc::now().timestamp() as u64)?;
    let token = Uuid::now_v7();
    sqlx::query("INSERT INTO development_turn_t(host_id,feature_id,logical_turn_id,claim_id,dispatch_token,request_digest,charge) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(auth.host_id).bind(feature_id).bind(&charge.logical_turn_id).bind(&claim.claim_id)
        .bind(token).bind(request_digest).bind(serde_json::to_value(charge)?).execute(&mut **tx).await?;
    save_feature(tx, auth.host_id, &feature).await?;
    Ok(TurnReservation::Dispatch { token })
}

/// Invoked by the authenticated execution reconciler, not a worker-facing API.
/// A late result cannot publish into a cancelled/replaced stage.
pub async fn complete_turn(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
    logical_turn_id: &str,
    token: Uuid,
    result: &Value,
) -> StoreResult<()> {
    check(
        serde_json::to_vec(result)?.len() <= 1024 * 1024,
        "turn result exceeds bound",
    )?;
    let feature = load_feature(tx, auth, feature_id).await?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    let claim = feature
        .active_claim
        .as_ref()
        .ok_or(StoreError::Conflict("stage no longer active"))?;
    check(
        feature.state == FeatureState::Active,
        "feature dispatch is fenced",
    )?;
    let previous: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT result FROM development_turn_t WHERE host_id=$1 AND feature_id=$2 AND logical_turn_id=$3 AND dispatch_token=$4 AND claim_id=$5")
        .bind(auth.host_id).bind(feature_id).bind(logical_turn_id).bind(token).bind(&claim.claim_id)
        .fetch_optional(&mut **tx).await?;
    match previous {
        None => {
            return Err(StoreError::Conflict(
                "turn result token or ownership mismatch",
            ));
        }
        Some(Some(previous)) => return check(previous == *result, "turn result replay conflict"),
        Some(None) => {}
    }
    sqlx::query("UPDATE development_turn_t SET result=$5,completed_ts=clock_timestamp() WHERE host_id=$1 AND feature_id=$2 AND logical_turn_id=$3 AND dispatch_token=$4")
        .bind(auth.host_id).bind(feature_id).bind(logical_turn_id).bind(token).bind(result).execute(&mut **tx).await?;
    Ok(())
}

/// Stops admission immediately. An active stage retains its reservation until
/// the execution reconciler proves fencing; timeout is never release evidence.
pub async fn request_cancel(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    feature_id: &str,
    expected_version: u64,
) -> StoreResult<FeatureRun> {
    let mut feature = load_feature(tx, auth, feature_id).await?;
    if matches!(
        feature.state,
        FeatureState::Cancelled | FeatureState::VmReleasePending
    ) {
        return Ok(feature);
    }
    check(
        feature.version == expected_version
            && !matches!(
                feature.state,
                FeatureState::Completed | FeatureState::Failed
            ),
        "cancel version is stale or feature is terminal",
    )?;
    check_vm_owner(tx, auth.host_id, &feature).await?;
    feature.version = feature
        .version
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(StoreError::Conflict("feature version exhausted"))?;
    // An idle feature may release only when every historical owner completed a
    // verified acceptance. Unknown/unaccepted execution retains the reservation.
    let history_settled = feature.claims.values().all(|claim| {
        feature
            .accepted_results
            .iter()
            .any(|result| &result.claim == claim && result.accepted)
    });
    if feature.active_claim.is_none() && history_settled {
        let changed = sqlx::query("UPDATE development_vm_t SET feature_id=NULL,generation=generation+1 WHERE host_id=$1 AND vm_id=$2 AND feature_id=$3 AND generation=$4")
            .bind(auth.host_id).bind(&feature.vm.vm_id).bind(feature_id).bind(feature.vm.generation as i64)
            .execute(&mut **tx).await?.rows_affected();
        check(changed == 1, "VM release ownership changed")?;
        feature.state = FeatureState::Cancelled;
        feature.vm.released = true;
    } else {
        feature.state = FeatureState::VmReleasePending;
        feature.vm.release_pending = true;
    }
    // Revoke native admission in the same transaction as feature cancellation.
    // The Agent observes this through its authenticated authorization poll and
    // requests Controller cleanup. This is not itself proof of cleanup.
    let actions: bool = sqlx::query_scalar(
        "SELECT to_regclass('workflow_ops.workflow_action_authority_t') IS NOT NULL",
    )
    .fetch_one(&mut **tx)
    .await?;
    if actions {
        sqlx::query("UPDATE workflow_action_authority_t a SET active=false FROM development_stage_t s WHERE s.host_id=$1 AND s.feature_id=$2 AND a.host_id=s.host_id AND a.run_id=s.workflow_instance_id")
            .bind(auth.host_id).bind(feature_id).execute(&mut **tx).await?;
    }
    sqlx::query("UPDATE workflow_agent_job_t j SET cancellation_requested_ts=COALESCE(j.cancellation_requested_ts,now()),updated_ts=now() FROM development_stage_t s WHERE j.host_id=$1 AND s.host_id=j.host_id AND s.process_id=j.workflow_process_id AND s.feature_id=$2 AND j.state IN('PENDING','TURN_CREATED','RUNNING')")
        .bind(auth.host_id).bind(feature_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE workflow_invocation_t i SET state='CANCELLED',cancel_requested_ts=COALESCE(cancel_requested_ts,now()),terminal_ts=COALESCE(terminal_ts,now()),user_authorization=NULL,user_authorization_exp=NULL,updated_ts=now(),state_version=state_version+1 FROM development_stage_t s WHERE s.host_id=$1 AND s.feature_id=$2 AND i.host_id=s.host_id AND i.process_id=s.process_id AND i.state IN('ACCEPTED','RUNNING','WAITING')")
        .bind(auth.host_id).bind(feature_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE process_info_t p SET status_code='F',custom_status_code='CANCELLED',completed_ts=now() FROM development_stage_t s WHERE s.host_id=$1 AND s.feature_id=$2 AND p.host_id=s.host_id AND p.process_id=s.process_id AND p.status_code IN('A','W')")
        .bind(auth.host_id).bind(feature_id).execute(&mut **tx).await?;
    sqlx::query("UPDATE task_info_t t SET status_code='F',result_code='CANCELLED',completed_ts=now() FROM development_stage_t s WHERE s.host_id=$1 AND s.feature_id=$2 AND t.host_id=s.host_id AND t.process_id=s.process_id AND t.status_code IN('A','W') AND t.locked='N'")
        .bind(auth.host_id).bind(feature_id).execute(&mut **tx).await?;
    save_feature(tx, auth.host_id, &feature).await?;
    Ok(feature)
}
