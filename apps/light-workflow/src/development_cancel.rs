//! Cancellation retains the VM until all known work is positively fenced.
use crate::{development_store::*, invocation::AuthenticatedInvocationContext};
use development_workflow_contract::FeatureState;
use execution_runner_protocol::{ExecutionSubject, RequestCancellation};
use light_client::workflow_job_transport::Report;
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Called before the generic invocation row is locked: development mutations
/// consistently lock feature before invocation. Old stage IDs never cancel a
/// newer active claim, even when they belong to the same authenticated owner.
pub async fn cancel_invocation(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    instance: Uuid,
) -> StoreResult<bool> {
    let feature_id: Option<String> = sqlx::query_scalar(
        "SELECT feature_id FROM development_stage_t WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(auth.host_id)
    .bind(instance)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(id) = feature_id else {
        return Ok(false);
    };
    let feature = load_feature(tx, auth, &id).await?;
    if feature
        .active_claim
        .as_ref()
        .is_none_or(|claim| claim.workflow_instance_id != instance.to_string())
        || matches!(
            feature.state,
            FeatureState::Completed | FeatureState::Cancelled | FeatureState::VmReleasePending
        )
    {
        return Ok(true);
    }
    let (policy, effects): (String, String) = sqlx::query_as(
        "SELECT cancellation_policy,effect_state FROM workflow_invocation_t
         WHERE host_id=$1 AND workflow_instance_id=$2 AND principal_subject=$3 AND end_user_subject=$4 FOR UPDATE",
    ).bind(auth.host_id).bind(instance).bind(auth.principal_subject).bind(auth.end_user_subject)
        .fetch_one(&mut **tx).await?;
    let reason = match policy.as_str() {
        "DISABLED" => Some("CANCELLATION_DISABLED"),
        "COOPERATIVE" => None,
        _ if effects != "none" => Some("EFFECT_ALREADY_POSSIBLE_OR_CONFIRMED"),
        _ => None,
    };
    if let Some(reason) = reason {
        sqlx::query("UPDATE workflow_invocation_t SET non_cancellable_reason=$3,updated_ts=now(),state_version=state_version+1 WHERE host_id=$1 AND workflow_instance_id=$2")
            .bind(auth.host_id).bind(instance).bind(reason).execute(&mut **tx).await?;
    } else {
        // Development cancellation does not fabricate compensation or unlock
        // in-flight native work. The separate reconciler requires positive proof.
        request_cancel(tx, auth, &id, feature.version).await?;
    }
    Ok(true)
}

pub fn validate_cleanup(report: &Report, service: &str) -> StoreResult<()> {
    let proof = report
        .cleanup
        .as_ref()
        .ok_or(StoreError::Conflict("cleanup proof missing"))?;
    match proof.get("kind").and_then(Value::as_str) {
        Some("not-dispatched") => check(
            proof.get("jobId").and_then(Value::as_str) == Some(report.job_id.to_string().as_str()),
            "non-dispatch proof belongs to another job",
        ),
        Some("controller") => {
            let receipt: RequestCancellation = serde_json::from_value(
                proof
                    .get("receipt")
                    .cloned()
                    .ok_or(StoreError::Conflict("Controller cleanup receipt missing"))?,
            )?;
            check(
                receipt.confirmed
                    && receipt.host_id == report.host_id
                    && receipt.origin_service_id == service
                    && matches!(receipt.subject,ExecutionSubject::AgentTurn{session_id,subject_id,turn_id}
                    if session_id==report.job_id && subject_id==turn_id && receipt.request_id==turn_id),
                "Controller cleanup receipt is unconfirmed or belongs to another job",
            )
        }
        _ => Err(StoreError::Conflict("unsupported cleanup proof")),
    }
}

pub async fn finalize(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    id: &str,
) -> StoreResult<bool> {
    let mut feature = load_feature(tx, auth, id).await?;
    if feature.state != FeatureState::VmReleasePending {
        return Ok(false);
    }
    let unsafe_work:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM development_stage_t s JOIN task_info_t t ON t.host_id=s.host_id AND t.process_id=s.process_id LEFT JOIN workflow_agent_job_t j ON j.host_id=t.host_id AND j.workflow_task_id=t.task_id WHERE s.host_id=$1 AND s.feature_id=$2 AND (t.locked='Y' OR t.status_code IN('A','W') OR (j.job_id IS NOT NULL AND (j.report IS NULL OR j.state NOT IN('SUCCEEDED','FAILED','CANCELLED','UNKNOWN'))) OR (j.job_id IS NULL AND (t.scheduling_request_id IS NOT NULL OR t.task_type NOT IN('set','assert','switch','ask')) AND NOT EXISTS(SELECT 1 FROM development_execution_fence_t f WHERE f.host_id=t.host_id AND f.task_id=t.task_id AND f.claim_id=s.claim_id)))) OR EXISTS(SELECT 1 FROM development_turn_t WHERE host_id=$1 AND feature_id=$2 AND task_id IS NULL AND result IS NULL) OR EXISTS(SELECT 1 FROM workflow_task_effect_t e JOIN development_stage_t s ON s.host_id=e.host_id AND s.workflow_instance_id=e.workflow_instance_id WHERE s.host_id=$1 AND s.feature_id=$2 AND (e.effect_state<>'confirmed' OR e.result IS NULL))")
        .bind(auth.host_id).bind(id).fetch_one(&mut **tx).await?;
    if unsafe_work {
        return Ok(false);
    }
    let actions: bool = sqlx::query_scalar(
        "SELECT to_regclass('workflow_ops.workflow_action_dispatch_t') IS NOT NULL",
    )
    .fetch_one(&mut **tx)
    .await?;
    if actions {
        let uncertain:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_action_dispatch_t d JOIN workflow_action_permit_t p ON p.host_id=d.host_id AND p.action_id=d.action_id JOIN development_stage_t s ON s.host_id=p.host_id AND s.workflow_instance_id=p.run_id WHERE s.host_id=$1 AND s.feature_id=$2 AND d.state NOT IN('NOT_INITIATED','SUCCEEDED','FAILED'))")
            .bind(auth.host_id).bind(id).fetch_one(&mut **tx).await?;
        if uncertain {
            return Ok(false);
        }
    }
    let changed=sqlx::query("UPDATE development_vm_t SET feature_id=NULL,generation=generation+1 WHERE host_id=$1 AND vm_id=$2 AND feature_id=$3 AND generation=$4")
        .bind(auth.host_id).bind(&feature.vm.vm_id).bind(id).bind(feature.vm.generation as i64)
        .execute(&mut **tx).await?.rows_affected();
    check(changed == 1, "cancellation VM owner generation changed")?;
    feature.state = FeatureState::Cancelled;
    feature.vm.released = true;
    feature.vm.release_pending = false;
    feature.active_claim = None;
    feature.version = feature
        .version
        .checked_add(1)
        .filter(|v| *v <= i64::MAX as u64)
        .ok_or(StoreError::Conflict("feature version exhausted"))?;
    save_feature(tx, auth.host_id, &feature).await?;
    Ok(true)
}

pub async fn reconcile(pool: &PgPool) -> Result<u64, sqlx::Error> {
    let rows:Vec<(Uuid,String,String,String)>=sqlx::query_as("SELECT host_id,feature_id,principal_subject,end_user_subject FROM development_feature_t WHERE record->>'state'='vm-release-pending' ORDER BY updated_ts LIMIT 32")
        .fetch_all(pool).await?;
    let mut count = 0;
    for (host, id, principal, user) in rows {
        let auth = AuthenticatedInvocationContext {
            host_id: host,
            principal_subject: &principal,
            end_user_subject: &user,
            update_user: "cancellation-reconciler",
            user_authorization: None,
            user_authorization_exp: None,
        };
        let mut tx = pool.begin().await?;
        match finalize(&mut tx, &auth, &id).await {
            Ok(changed) => {
                tx.commit().await?;
                count += u64::from(changed);
            }
            Err(error) => {
                tracing::warn!("development cancellation remains fenced: {error}");
            }
        }
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn cleanup_receipt_requires_confirmed_exact_host_service_and_job() {
        let host = Uuid::now_v7();
        let job = Uuid::now_v7();
        let turn = Uuid::now_v7();
        let mut receipt = RequestCancellation {
            host_id: host,
            request_id: turn,
            origin_service_id: "agent".into(),
            subject: ExecutionSubject::AgentTurn {
                session_id: job,
                subject_id: turn,
                turn_id: turn,
            },
            confirmed: true,
        };
        let mut report = Report {
            host_id: host,
            job_id: job,
            state: "CANCELLED".into(),
            output: None,
            error: None,
            cleanup: Some(json!({"kind":"controller","receipt":receipt})),
        };
        assert!(validate_cleanup(&report, "agent").is_ok());
        assert!(validate_cleanup(&report, "other-agent").is_err());
        receipt.confirmed = false;
        report.cleanup = Some(json!({"kind":"controller","receipt":receipt}));
        assert!(validate_cleanup(&report, "agent").is_err());
        report.cleanup = Some(json!({"kind":"not-dispatched","jobId":Uuid::now_v7()}));
        assert!(validate_cleanup(&report, "agent").is_err());
        report.cleanup = None;
        assert!(validate_cleanup(&report, "agent").is_err());
    }
}
