//! Bridge from Controller-accepted runner results to feature turn/fence state.
//! These functions are internal workflow execution operations, not HTTP handlers.
use crate::{
    development_store::*, invocation::AuthenticatedInvocationContext, repositories::TerminalAttempt,
};
use development_workflow_contract::{FeatureState, StageClaimReceipt, TurnCharge};
use execution_runner_protocol::{
    CleanupState, ExecutionSubject, NormalizedExecutionResult, OriginKind,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
use workflow_invocation_contract::canonical_sha256;

/// Bind a logical model turn to exactly one durable workflow task before send.
pub async fn reserve_task_turn(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    claim: &StageClaimReceipt,
    task: Uuid,
    charge: &TurnCharge,
    digest: &str,
) -> StoreResult<TurnReservation> {
    let process: Uuid = claim
        .process_id
        .parse()
        .map_err(|_| StoreError::Conflict("invalid stage process"))?;
    let feature: String = sqlx::query_scalar("SELECT feature_id FROM development_stage_t WHERE host_id=$1 AND claim_id=$2 AND process_id=$3")
        .bind(auth.host_id).bind(&claim.claim_id).bind(process).fetch_one(&mut **tx).await?;
    load_feature(tx, auth, &feature).await?;
    let conflict:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM development_turn_t WHERE host_id=$1 AND ((task_id=$2 AND logical_turn_id<>$3) OR (feature_id=$4 AND logical_turn_id=$3 AND task_id IS NOT NULL AND task_id<>$2)))")
        .bind(auth.host_id).bind(task).bind(&charge.logical_turn_id).bind(&feature).fetch_one(&mut **tx).await?;
    check(!conflict, "turn/task binding conflict")?;
    let owned: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_info_t WHERE host_id=$1 AND task_id=$2 AND process_id=$3)")
        .bind(auth.host_id).bind(task).bind(process).fetch_one(&mut **tx).await?;
    check(owned, "turn task belongs to another stage")?;
    let result = reserve_turn(tx, auth, &feature, claim, charge, digest).await?;
    let updated=sqlx::query("UPDATE development_turn_t SET task_id=$4 WHERE host_id=$1 AND feature_id=$2 AND logical_turn_id=$3 AND (task_id IS NULL OR task_id=$4)")
        .bind(auth.host_id).bind(feature).bind(&charge.logical_turn_id).bind(task).execute(&mut **tx).await?.rows_affected();
    check(updated == 1, "turn is already bound to another task")?;
    Ok(result)
}

/// Called only after TaskExecutor accepted the Controller attempt fence, in that
/// same transaction. No Controller acknowledgement may precede this commit.
pub async fn reconcile_runner_result(
    tx: &mut Transaction<'_, Postgres>,
    attempt: &TerminalAttempt,
) -> StoreResult<()> {
    let definition: Option<Value> = sqlx::query_scalar(
        "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(attempt.host_id)
    .bind(attempt.process_id)
    .fetch_one(&mut **tx)
    .await?;
    let Some(definition) = definition.filter(is_development_definition) else {
        return Ok(());
    };
    let result: NormalizedExecutionResult = serde_json::from_value(
        attempt
            .normalized_result
            .clone()
            .ok_or(StoreError::Conflict(
                "development result requires cleanup evidence",
            ))?,
    )?;
    let policy:String=sqlx::query_scalar("SELECT task_policy_digest FROM task_info_t WHERE host_id=$1 AND task_id=$2 AND process_id=$3")
        .bind(attempt.host_id).bind(attempt.task_id).bind(attempt.process_id).fetch_one(&mut **tx).await?;
    check(
        result.cleanup_state == CleanupState::Confirmed
            && result.execution_id.0 == attempt.execution_id
            && result.origin.kind == OriginKind::Workflow
            && result.origin.host_id == attempt.host_id
            && matches!(
                attempt.state.as_str(),
                "SUCCEEDED" | "FAILED" | "CANCELLED" | "TIMED_OUT"
            )
            && i64::from(result.attempt) == i64::from(attempt.attempt_number)
            && result.policy_digest.trim_start_matches("sha256:")
                == policy.trim_start_matches("sha256:")
            && result.definition_digest.trim_start_matches("sha256:")
                == execution_runner_protocol::canonical_sha256(&definition)
                    .map_err(|_| StoreError::Conflict("invalid definition fingerprint"))?
            && serde_json::to_value(result.state)? == json!(attempt.state)
            && matches!(result.subject,ExecutionSubject::WorkflowTask{process_id,task_id,subject_id} if process_id==attempt.process_id && task_id==attempt.task_id && subject_id==attempt.task_id),
        "development result lacks matching confirmed execution cleanup",
    )?;
    let (feature_id,claim_value,principal,user):(String,Value,String,String)=sqlx::query_as("SELECT s.feature_id,s.receipt,f.principal_subject,f.end_user_subject FROM development_stage_t s JOIN development_feature_t f ON f.host_id=s.host_id AND f.feature_id=s.feature_id WHERE s.host_id=$1 AND s.process_id=$2")
        .bind(attempt.host_id).bind(attempt.process_id).fetch_one(&mut **tx).await?;
    let auth = AuthenticatedInvocationContext {
        host_id: attempt.host_id,
        principal_subject: &principal,
        end_user_subject: &user,
        update_user: "development-result-reconciler",
        user_authorization: None,
        user_authorization_exp: None,
    };
    let feature = load_feature(tx, &auth, &feature_id).await?;
    let claim: StageClaimReceipt = serde_json::from_value(claim_value)?;
    check(
        feature.active_claim.as_ref() == Some(&claim)
            && matches!(
                feature.state,
                FeatureState::Active | FeatureState::VmReleasePending
            ),
        "late result from a superseded stage",
    )?;
    let digest = canonical_sha256(
        attempt
            .normalized_result
            .as_ref()
            .expect("validated result"),
    )
    .map_err(|_| StoreError::Conflict("invalid normalized result fingerprint"))?;
    let existing:Option<(Uuid,i64,String)>=sqlx::query_as("SELECT execution_id,fencing_token,result_digest FROM development_execution_fence_t WHERE host_id=$1 AND task_id=$2")
        .bind(attempt.host_id).bind(attempt.task_id).fetch_optional(&mut **tx).await?;
    if let Some((execution, token, old)) = existing {
        check(
            execution == attempt.execution_id && token == attempt.fencing_token && old == digest,
            "execution fence replay conflict",
        )?;
    } else {
        sqlx::query("INSERT INTO development_execution_fence_t(host_id,task_id,claim_id,execution_id,fencing_token,result_digest) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(attempt.host_id).bind(attempt.task_id).bind(&claim.claim_id).bind(attempt.execution_id).bind(attempt.fencing_token).bind(digest).execute(&mut **tx).await?;
    }
    // Cancelled stages may record proof of cleanup but cannot publish late output.
    if feature.state == FeatureState::Active {
        let turn:Option<(String,Uuid)>=sqlx::query_as("SELECT logical_turn_id,dispatch_token FROM development_turn_t WHERE host_id=$1 AND task_id=$2 AND claim_id=$3")
            .bind(attempt.host_id).bind(attempt.task_id).bind(&claim.claim_id).fetch_optional(&mut **tx).await?;
        if let Some((logical, token)) = turn {
            let output = if attempt.state == "SUCCEEDED" {
                result.structured_output.ok_or(StoreError::Conflict(
                    "development turn structured output missing",
                ))?
            } else {
                json!({"state":attempt.state,"error":attempt.normalized_error})
            };
            complete_turn(tx, &auth, &feature_id, &logical, token, &output).await?;
        }
    }
    Ok(())
}

/// Recovery for commit-success / Controller-ack-loss: acknowledge only the exact
/// previously committed task attempt and normalized result, without reapplying it.
pub async fn runner_result_already_recorded(
    tx: &mut Transaction<'_, Postgres>,
    attempt: &TerminalAttempt,
) -> StoreResult<bool> {
    let definition: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT definition_snapshot FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(attempt.host_id)
    .bind(attempt.process_id)
    .fetch_optional(&mut **tx)
    .await?;
    if !definition
        .flatten()
        .as_ref()
        .is_some_and(is_development_definition)
    {
        return Ok(false);
    }
    let Some(result) = attempt.normalized_result.as_ref() else {
        return Ok(false);
    };
    let digest = canonical_sha256(result)
        .map_err(|_| StoreError::Conflict("invalid normalized result fingerprint"))?;
    let found:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM development_execution_fence_t f JOIN task_info_t t ON t.host_id=f.host_id AND t.task_id=f.task_id JOIN development_stage_t s ON s.host_id=f.host_id AND s.claim_id=f.claim_id WHERE f.host_id=$1 AND f.task_id=$2 AND f.execution_id=$3 AND f.fencing_token=$4 AND f.result_digest=$5 AND t.accepted_attempt=$6 AND t.process_id=$7 AND s.process_id=$7)")
        .bind(attempt.host_id).bind(attempt.task_id).bind(attempt.execution_id).bind(attempt.fencing_token).bind(digest).bind(attempt.attempt_number).bind(attempt.process_id).fetch_one(&mut **tx).await?;
    Ok(found)
}
