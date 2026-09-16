//! Feature evidence from the authenticated Agent result mirror. The caller must
//! first validate the Agent peer, job subject, terminal state and cleanup receipt.
use crate::{development_store::*, invocation::AuthenticatedInvocationContext};
use development_workflow_contract::{FeatureState, StageClaimReceipt};
use execution_runner_protocol::NormalizedExecutionResult;
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    Recorded,
    InvalidReview,
}

pub(crate) async fn record(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    job: Uuid,
    agent: Uuid,
    envelope: &Value,
) -> StoreResult<RecordOutcome> {
    let row: Option<(String, Value, String, String, Uuid)> = sqlx::query_as(
        "SELECT s.feature_id,s.receipt,f.principal_subject,f.end_user_subject,j.workflow_task_id
         FROM workflow_agent_job_t j JOIN development_stage_t s
         ON s.host_id=j.host_id AND s.process_id=j.workflow_process_id
         JOIN development_feature_t f ON f.host_id=s.host_id AND f.feature_id=s.feature_id
         WHERE j.host_id=$1 AND j.job_id=$2 AND j.agent_def_id=$3",
    )
    .bind(host)
    .bind(job)
    .bind(agent)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((feature_id, claim, principal, user, task)) = row else {
        return Ok(RecordOutcome::Recorded);
    };
    let auth = AuthenticatedInvocationContext {
        host_id: host,
        principal_subject: &principal,
        end_user_subject: &user,
        update_user: "native-result",
        user_authorization: None,
        user_authorization_exp: None,
    };
    let feature = load_feature(tx, &auth, &feature_id).await?;
    let claim: StageClaimReceipt = serde_json::from_value(claim)?;
    check(
        feature.active_claim.as_ref() == Some(&claim)
            && matches!(
                feature.state,
                FeatureState::Active | FeatureState::VmReleasePending
            ),
        "native result belongs to a superseded feature claim",
    )?;
    let normalized = envelope
        .get("result")
        .ok_or(StoreError::Conflict("native result missing"))?;
    let result: NormalizedExecutionResult = serde_json::from_value(normalized.clone())?;
    let token = envelope
        .get("fencingToken")
        .and_then(Value::as_i64)
        .ok_or(StoreError::Conflict("native fence missing"))?;
    let digest = workflow_invocation_contract::canonical_sha256(normalized)
        .map_err(|_| StoreError::Conflict("native result digest invalid"))?;
    sqlx::query("INSERT INTO development_execution_fence_t(host_id,task_id,claim_id,execution_id,fencing_token,result_digest) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(host_id,task_id) DO NOTHING")
        .bind(host).bind(task).bind(&claim.claim_id).bind(result.execution_id.0).bind(token).bind(&digest)
        .execute(&mut **tx).await?;
    let same:bool=sqlx::query_scalar("SELECT claim_id=$3 AND execution_id=$4 AND fencing_token=$5 AND result_digest=$6 FROM development_execution_fence_t WHERE host_id=$1 AND task_id=$2")
        .bind(host).bind(task).bind(&claim.claim_id).bind(result.execution_id.0).bind(token).bind(digest)
        .fetch_one(&mut **tx).await?;
    check(same, "native result fence replay conflict")?;
    let changed=sqlx::query("UPDATE task_info_t SET accepted_attempt=$3 WHERE host_id=$1 AND task_id=$2 AND scheduling_request_id IS NULL AND (accepted_attempt IS NULL OR accepted_attempt=$3)")
        .bind(host).bind(task).bind(i32::try_from(result.attempt).map_err(|_|StoreError::Conflict("native attempt exceeds bound"))?)
        .execute(&mut **tx).await?.rows_affected();
    check(
        changed == 1,
        "native task already has another execution owner",
    )?;
    let manager_read: bool = sqlx::query_scalar(
        "SELECT input ? 'managerSnapshot' FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2",
    )
    .bind(host)
    .bind(job)
    .fetch_one(&mut **tx)
    .await?;
    if feature.state == FeatureState::Active && !manager_read {
        let (logical,dispatch,charge):(String,Uuid,Value)=sqlx::query_as("SELECT logical_turn_id,dispatch_token,charge FROM development_turn_t WHERE host_id=$1 AND task_id=$2 AND claim_id=$3")
            .bind(host).bind(task).bind(&claim.claim_id).fetch_one(&mut **tx).await?;
        let charge: development_workflow_contract::TurnCharge = serde_json::from_value(charge)?;
        let mut output = result
            .structured_output
            .clone()
            .ok_or(StoreError::Conflict("native structured output missing"))?;
        if charge.kind == development_workflow_contract::TurnKind::Review {
            let input: Value = sqlx::query_scalar(
                "SELECT input FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2",
            )
            .bind(host)
            .bind(job)
            .fetch_one(&mut **tx)
            .await?;
            // Parsing/binding failures are terminal output failures, not failed
            // delivery. Preserve the original output and consumed turn without
            // inserting any finding-ledger receipt. Ownership/fence/DB errors
            // above remain errors and cannot be acknowledged as valid delivery.
            output = match review_output(&input, &output) {
                Ok(output) => output,
                Err(StoreError::Json(_) | StoreError::Conflict(_)) => {
                    complete_turn(tx, &auth, &feature_id, &logical, dispatch, &output).await?;
                    return Ok(RecordOutcome::InvalidReview);
                }
                Err(error) => return Err(error),
            };
        }
        complete_turn(tx, &auth, &feature_id, &logical, dispatch, &output).await?;
        if charge.kind == development_workflow_contract::TurnKind::Review {
            match crate::development_handoff::record_review(tx, &auth, &feature_id, &logical).await
            {
                Ok(_) => {}
                // Schema-valid output may still violate finding-ledger rules.
                // apply_review validates on a private ledger copy before save.
                Err(StoreError::Contract(_)) => return Ok(RecordOutcome::InvalidReview),
                Err(error) => return Err(error),
            }
        }
    }
    Ok(RecordOutcome::Recorded)
}

/// Normalize the native workspace answer for the finding ledger, without
/// modifying the original authenticated execution result or its digest.
fn review_output(input: &Value, output: &Value) -> StoreResult<Value> {
    let expected: development_workflow_contract::ReviewBinding = serde_json::from_value(
        input
            .get("reviewBinding")
            .cloned()
            .ok_or(StoreError::Conflict("review job binding missing"))?,
    )?;
    let review: development_workflow_contract::ReviewResult =
        if let Some(value) = output.get("reviewResult") {
            serde_json::from_value(value.clone())?
        } else {
            let text = output
                .get("finalMessage")
                .and_then(Value::as_str)
                .ok_or(StoreError::Conflict("native review answer missing"))?;
            check(
                text.len() <= 64 * 1024,
                "native review answer exceeds bound",
            )?;
            // Presentation framing is not evidence. Accept exactly one complete
            // JSON fence, never extract a JSON fragment from surrounding prose.
            // The original finalMessage remains unchanged in the receipt.
            let text = text.trim();
            let document = if let Some(fenced) = text.strip_prefix("```json\n") {
                fenced.strip_suffix("\n```").ok_or(StoreError::Conflict(
                    "native review JSON fence is incomplete or has trailing content",
                ))?
            } else {
                text
            };
            serde_json::from_str(document)?
        };
    check(
        review.binding == expected,
        "native review substituted its allocated binding",
    )?;
    let mut normalized = output.as_object().cloned().ok_or(StoreError::Conflict(
        "native review output must be an object",
    ))?;
    normalized.insert("reviewResult".into(), serde_json::to_value(review)?);
    Ok(Value::Object(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_review_answer_preserves_raw_output_and_requires_exact_binding() {
        let binding = json!({"featureRunId":"feature","reviewId":"review","stageExecutionId":"stage",
            "reviewer":"claude","sessionId":"session","candidate":"candidate","repositories":["repo"]});
        let review = json!({"binding":binding,"accepted":true,"evidence":{"id":"artifact","digest":"digest"},
            "existingFindings":[],"newFindings":[]});
        let input = json!({"reviewBinding":binding});
        let output = json!({"finalMessage":review.to_string(),"workspace":{"intent":"review"}});
        let normalized = review_output(&input, &output).unwrap();
        assert_eq!(normalized["reviewResult"], review);
        assert_eq!(normalized["finalMessage"], output["finalMessage"]);
        assert!(output.get("reviewResult").is_none());
        let fenced = json!({"finalMessage":format!("```json\n{review}\n```\n")});
        let normalized_fenced = review_output(&input, &fenced).unwrap();
        assert_eq!(normalized_fenced["reviewResult"], review);
        assert_eq!(normalized_fenced["finalMessage"], fenced["finalMessage"]);
        let native_structured = json!({"finalMessage":"Reviewer commentary is preserved verbatim.","reviewResult":review});
        let normalized_structured = review_output(&input, &native_structured).unwrap();
        assert_eq!(normalized_structured, native_structured);
        for key in [
            "featureRunId",
            "reviewId",
            "stageExecutionId",
            "reviewer",
            "sessionId",
            "candidate",
            "repositories",
        ] {
            let mut wrong = review.clone();
            wrong["binding"][key] = json!("other");
            assert!(
                review_output(&input, &json!({"finalMessage":wrong.to_string()})).is_err(),
                "{key}"
            );
            assert!(
                review_output(
                    &input,
                    &json!({"finalMessage":"Raw commentary", "reviewResult":wrong})
                )
                .is_err(),
                "native structured {key}"
            );
        }
        for text in [
            "not JSON".to_string(),
            format!("preamble\n```json\n{review}\n```"),
            format!("```json\n{review}\n```\ntrailing text"),
            format!("```json\n{review}\n```\n```json\n{review}\n```"),
            format!("```json\n{review}"),
            "x".repeat(65537),
        ] {
            assert!(review_output(&input, &json!({"finalMessage":text})).is_err());
        }
        let mut wrong = review.clone();
        wrong["workerApproved"] = json!(true);
        assert!(review_output(&input, &json!({"finalMessage":wrong.to_string()})).is_err());
    }
}
