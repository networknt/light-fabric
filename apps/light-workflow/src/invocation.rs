//! Direct transactional workflow acceptance shared by the Phase 1 HTTP
//! facade and its qualification tests.

use chrono::Utc;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use thiserror::Error;
use uuid::Uuid;
use workflow_invocation_contract::{
    CancellationPolicy, ContractError, ExecutionClass, IdempotencyKind, InvocationMode,
    StartInvocationRequest, canonical_sha256, stable_subject_claims,
};

#[derive(Debug)]
pub struct AuthenticatedInvocationContext<'a> {
    pub host_id: Uuid,
    pub principal_subject: &'a str,
    pub end_user_subject: &'a str,
    pub update_user: &'a str,
}

#[derive(Debug)]
pub struct PreparedInvocationStart<'a> {
    pub binding_id: Uuid,
    pub process_id: Uuid,
    pub initial_task_id: Uuid,
    pub application_id: &'a str,
    pub initial_task_name: &'a str,
    pub initial_task_type: &'a str,
    pub definition_snapshot: &'a Value,
    pub execution_placement: &'a str,
    pub task_policy_digest: &'a str,
    pub public_output_schema: Option<&'a Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcceptOutcome {
    Accepted {
        workflow_instance_id: Uuid,
    },
    Replay {
        workflow_instance_id: Uuid,
        generation: i64,
    },
}

#[derive(Debug, Error)]
pub enum InvocationAcceptError {
    #[error("workflow invocation contract is invalid: {0}")]
    Contract(#[from] ContractError),
    #[error("WORKFLOW_IDEMPOTENCY_CONFLICT")]
    IdempotencyConflict,
    #[error("workflow invocation persistence failed: {0}")]
    Database(#[from] sqlx::Error),
}

/// Persists the idempotency reservation, process, initial task, invocation,
/// budget ledger, and audit event in the caller's transaction.
pub async fn accept_invocation(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    request: &StartInvocationRequest,
    prepared: &PreparedInvocationStart<'_>,
) -> Result<AcceptOutcome, InvocationAcceptError> {
    request.validate(Utc::now())?;
    validate_prepared(prepared)?;

    let (outcome, instance_id, generation): (String, Uuid, i64) = sqlx::query_as(
        "SELECT outcome,accepted_workflow_instance_id,accepted_generation
           FROM workflow_claim_idempotency_v1(
             $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
    )
    .bind(auth.host_id)
    .bind(Uuid::now_v7())
    .bind(&request.idempotency.scoped_key_digest)
    .bind(idempotency_kind(request.idempotency.kind))
    .bind(request.stable_tool_ref)
    .bind(auth.principal_subject)
    .bind(auth.end_user_subject)
    .bind(request.workflow_instance_id)
    .bind(&request.definition_digest)
    .bind(&request.normalized_input_digest)
    .bind(request.idempotency.in_flight_until)
    .bind(request.idempotency.result_replay_until)
    .fetch_one(&mut **tx)
    .await?;

    match outcome.as_str() {
        "REPLAY" => {
            return Ok(AcceptOutcome::Replay {
                workflow_instance_id: instance_id,
                generation,
            });
        }
        "CONFLICT" => return Err(InvocationAcceptError::IdempotencyConflict),
        "ACCEPTED" if instance_id == request.workflow_instance_id => {}
        _ => return Err(sqlx::Error::Protocol("invalid idempotency outcome".into()).into()),
    }

    let definition_digest = bare_digest(&request.definition_digest);
    let policy_digest = bare_digest(&request.policy_digest);
    let subject_claims = stable_subject_claims(&request.caller_claims);
    let accepted_subject_claims_digest = canonical_sha256(&subject_claims)?;
    sqlx::query(
        "INSERT INTO process_info_t(
           host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,
           status_code,ex_trigger_ts,input_data,context_data,definition_snapshot,
           definition_digest,policy_digest,source_event_id,execution_profile_id,
           deadline_ts,update_user)
         VALUES($1,$2,$3,$4,$5,'Workflow','A',CURRENT_TIMESTAMP,$6,$6,$7,$8,$9,$10,'host',$11,$12)",
    )
    .bind(auth.host_id)
    .bind(prepared.process_id)
    .bind(request.workflow_definition_id)
    .bind(request.workflow_instance_id.to_string())
    .bind(prepared.application_id)
    .bind(&request.input)
    .bind(prepared.definition_snapshot)
    .bind(definition_digest)
    .bind(policy_digest)
    .bind(format!(
        "workflow-invocation:{}",
        request.workflow_instance_id
    ))
    .bind(request.deadline_ts)
    .bind(auth.update_user)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO task_info_t(
           host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,
           status_code,locked,priority,deadline_ts,task_input,
           execution_placement,task_policy_digest,update_user,execution_class)
         VALUES($1,$2,$3,$4,$5,$6,'A','N',$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(auth.host_id)
    .bind(prepared.initial_task_id)
    .bind(prepared.initial_task_type)
    .bind(prepared.process_id)
    .bind(request.workflow_instance_id.to_string())
    .bind(prepared.initial_task_name)
    .bind(if request.execution_class == ExecutionClass::Interactive {
        100
    } else {
        1
    })
    .bind(request.deadline_ts)
    .bind(&request.input)
    .bind(prepared.execution_placement)
    .bind(prepared.task_policy_digest)
    .bind(auth.update_user)
    .bind(execution_class(request.execution_class))
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO workflow_invocation_t(
           host_id,workflow_instance_id,binding_id,process_id,stable_tool_ref,
           wf_def_id,workflow_version,definition_digest,schema_digest,
           policy_digest,response_policy_digest,principal_subject,end_user_subject,
           subject_claims,input,input_digest,canonical_input_profile,invocation_mode,
           execution_class,permit_depth,state,correlation_id,deadline_ts,cancellation_policy,
           response_policy_snapshot)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,'ACCEPTED',$21,$22,$23,$24)",
    )
    .bind(auth.host_id)
    .bind(request.workflow_instance_id)
    .bind(prepared.binding_id)
    .bind(prepared.process_id)
    .bind(request.stable_tool_ref)
    .bind(request.workflow_definition_id)
    .bind(&request.workflow_version)
    .bind(&request.definition_digest)
    .bind(&request.schema_digest)
    .bind(&request.policy_digest)
    .bind(&request.response_policy_digest)
    .bind(auth.principal_subject)
    .bind(auth.end_user_subject)
    .bind(&subject_claims)
    .bind(&request.input)
    .bind(&request.normalized_input_digest)
    .bind(&request.canonical_input_profile)
    .bind(invocation_mode(request.mode))
    .bind(execution_class(request.execution_class))
    .bind(i32::from(request.permit_depth))
    .bind(&request.correlation_id)
    .bind(request.deadline_ts)
    .bind(cancellation_policy(request.cancellation_policy))
    .bind(json!({
        "responsePolicyDigest": request.response_policy_digest,
        "acceptedSubjectClaimsDigest": accepted_subject_claims_digest,
        "acceptedSubjectClaims": subject_claims,
        "publicOutputSchema": prepared.public_output_schema
    }))
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO workflow_invocation_budget_t(
           host_id,ledger_id,workflow_instance_id,task_attempt_limit,
           nested_call_limit,request_byte_limit,byte_limit,result_byte_limit,cost_unit_limit,deadline_ts)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(auth.host_id)
    .bind(Uuid::now_v7())
    .bind(request.workflow_instance_id)
    .bind(i64::from(request.budget.maximum_task_attempts))
    .bind(i64::from(request.budget.maximum_nested_calls))
    .bind(saturating_i64(request.budget.maximum_request_bytes))
    .bind(saturating_i64(request.budget.maximum_intermediate_bytes))
    .bind(saturating_i64(request.budget.maximum_result_bytes))
    .bind(saturating_i64(request.budget.maximum_cost_units))
    .bind(request.deadline_ts)
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO workflow_invocation_audit_outbox_t(
           host_id,event_id,workflow_instance_id,event_type,payload,correlation_id)
         VALUES($1,$2,$3,'WorkflowInvocationAccepted',$4,$5)",
    )
    .bind(auth.host_id)
    .bind(Uuid::now_v7())
    .bind(request.workflow_instance_id)
    .bind(json!({
        "workflowInstanceId": request.workflow_instance_id,
        "stableToolRef": request.stable_tool_ref,
        "definitionDigest": request.definition_digest,
        "state": "ACCEPTED"
    }))
    .bind(&request.correlation_id)
    .execute(&mut **tx)
    .await?;

    Ok(AcceptOutcome::Accepted {
        workflow_instance_id: request.workflow_instance_id,
    })
}

fn validate_prepared(prepared: &PreparedInvocationStart<'_>) -> Result<(), ContractError> {
    if prepared.binding_id.is_nil()
        || prepared.process_id.is_nil()
        || prepared.initial_task_id.is_nil()
        || prepared.initial_task_name.trim().is_empty()
        || prepared.initial_task_type.trim().is_empty()
    {
        return Err(ContractError::InvalidPreparedStart);
    }
    Ok(())
}

fn bare_digest(value: &str) -> &str {
    value.strip_prefix("sha256:").unwrap_or(value)
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn idempotency_kind(kind: IdempotencyKind) -> &'static str {
    match kind {
        IdempotencyKind::Derived => "DERIVED",
        IdempotencyKind::Explicit => "EXPLICIT",
        IdempotencyKind::Business => "BUSINESS",
    }
}

fn invocation_mode(mode: InvocationMode) -> &'static str {
    match mode {
        InvocationMode::Sync => "sync",
        InvocationMode::Async => "async",
    }
}

fn cancellation_policy(policy: CancellationPolicy) -> &'static str {
    match policy {
        CancellationPolicy::BeforeEffectsOnly => "BEFORE_EFFECTS_ONLY",
        CancellationPolicy::Cooperative => "COOPERATIVE",
        CancellationPolicy::Disabled => "DISABLED",
    }
}

fn execution_class(class: ExecutionClass) -> &'static str {
    match class {
        ExecutionClass::Interactive => "interactive",
        ExecutionClass::Standard => "standard",
        ExecutionClass::Batch => "batch",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_enum_values_match_storage_contract() {
        assert_eq!(idempotency_kind(IdempotencyKind::Explicit), "EXPLICIT");
        assert_eq!(invocation_mode(InvocationMode::Sync), "sync");
        assert_eq!(execution_class(ExecutionClass::Interactive), "interactive");
        assert_eq!(bare_digest("sha256:abc"), "abc");
    }
}
