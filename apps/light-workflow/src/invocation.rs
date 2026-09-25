//! Direct transactional workflow acceptance shared by the Phase 1 HTTP
//! facade and its qualification tests.

use chrono::{DateTime, Utc};
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
    pub user_authorization: Option<&'a str>,
    pub user_authorization_exp: Option<i64>,
}

#[derive(Debug)]
pub struct PreparedInvocationStart<'a> {
    pub binding_id: Option<Uuid>,
    pub process_id: Uuid,
    pub initial_task_id: Uuid,
    pub application_id: &'a str,
    pub initial_task_name: &'a str,
    pub initial_task_type: &'a str,
    pub definition_snapshot: &'a Value,
    pub execution_placement: &'a str,
    pub execution_profile_id: &'a str,
    /// Selected by a trusted Workflow entry point; never taken from request JSON.
    pub admission_profile: &'a str,
    pub policy_snapshot_id: Option<Uuid>,
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
    if crate::development_store::is_development_definition(prepared.definition_snapshot) {
        return Err(sqlx::Error::Protocol(
            "development stage requires atomic feature claim".into(),
        )
        .into());
    }
    accept_invocation_in(tx, auth, request, prepared).await
}

/// Only the guarded development store may bypass the generic-entry prohibition.
pub(crate) async fn accept_invocation_in(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedInvocationContext<'_>,
    request: &StartInvocationRequest,
    prepared: &PreparedInvocationStart<'_>,
) -> Result<AcceptOutcome, InvocationAcceptError> {
    let accepted_at = Utc::now();
    request.validate(accepted_at)?;
    validate_prepared(prepared)?;
    if prepared.admission_profile == "portal_execution"
        && !workflow_invocation_contract::authority_lease_allowed(
            workflow_invocation_contract::AuthorityLeaseProfile::PortalExecutionV1,
            request.mode,
            u64::try_from((request.deadline_ts - accepted_at).num_milliseconds()).unwrap_or(0),
        )
    {
        return Err(ContractError::InvalidDeadline.into());
    }
    let durable_private_wait =
        prepared.admission_profile == "portal_execution" && request.parent_action_id.is_none();
    let declared_deadline = if prepared.admission_profile == "portal_execution" {
        declared_workflow_deadline(prepared.definition_snapshot, accepted_at)?
    } else {
        None
    };
    let process_deadline = if durable_private_wait {
        declared_deadline
    } else {
        Some(declared_deadline.map_or(request.deadline_ts, |deadline| {
            deadline.min(request.deadline_ts)
        }))
    };

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
           definition_digest,policy_digest,source_event_id,execution_profile_id,policy_snapshot_id,
           deadline_ts,update_user)
         VALUES($1,$2,$3,$4,$5,'Workflow','A',CURRENT_TIMESTAMP,$6,$6,$7,$8,$9,$10,$11,$12,$13,$14)",
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
    .bind(prepared.execution_profile_id)
    .bind(prepared.policy_snapshot_id)
    .bind(process_deadline)
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
    .bind(process_deadline)
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
           subject_claims,user_authorization,user_authorization_exp,input,input_digest,canonical_input_profile,invocation_mode,
           execution_class,permit_depth,state,correlation_id,deadline_ts,cancellation_policy,
           response_policy_snapshot)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,'ACCEPTED',$23,$24,$25,$26)",
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
    .bind(auth.user_authorization)
    .bind(auth.user_authorization_exp)
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
        "acceptedAdmissionProfile": prepared.admission_profile,
        "privateExecutionProfile": durable_private_wait.then(|| json!({
            "version": 1,
            "profile": "portal_execution",
            "waitPolicy": "durable_until_explicit_timeout_or_terminal",
            "executionAuthority": "bounded_renewable_recheck_on_resume",
            "budgetAccounting": "durable_consumed_total_never_reset_on_renewal",
            "explicitTaskDeadlineAt": process_deadline
        })),
        "acceptedSubjectClaimsDigest": accepted_subject_claims_digest,
        "acceptedSubjectClaims": subject_claims,
        "publicOutputSchema": prepared.public_output_schema
    }))
    .execute(&mut **tx)
    .await?;

    sqlx::query(
        "INSERT INTO workflow_invocation_budget_t(
           host_id,ledger_id,workflow_instance_id,task_attempt_limit,
           nested_call_limit,request_byte_limit,byte_limit,result_byte_limit,cost_unit_limit,deadline_ts,lifetime_version)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
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
    .bind(process_deadline)
    .bind((durable_private_wait && process_deadline.is_none()).then_some(1_i16))
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
    if (prepared.admission_profile == "workflow_backed" && prepared.binding_id.is_none())
        || prepared.binding_id.is_some_and(|value| value.is_nil())
        || prepared.process_id.is_nil()
        || prepared.initial_task_id.is_nil()
        || prepared.initial_task_name.trim().is_empty()
        || prepared.initial_task_type.trim().is_empty()
        || !matches!(
            prepared.admission_profile,
            "workflow_backed" | "portal_execution"
        )
    {
        return Err(ContractError::InvalidPreparedStart);
    }
    Ok(())
}

fn declared_workflow_deadline(
    snapshot: &Value,
    accepted_at: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>, sqlx::Error> {
    use workflow_core::models::duration::OneOfDurationOrIso8601Expression;
    use workflow_core::models::timeout::OneOfTimeoutDefinitionOrReference;
    use workflow_core::models::workflow::WorkflowDefinition;

    let definition: WorkflowDefinition = serde_json::from_value(snapshot.clone())
        .map_err(|_| sqlx::Error::Protocol("private workflow definition is invalid".into()))?;
    let Some(configured) = definition.timeout.as_ref() else {
        return Ok(None);
    };
    let timeout = match configured {
        OneOfTimeoutDefinitionOrReference::Timeout(timeout) => timeout,
        OneOfTimeoutDefinitionOrReference::Reference(reference) => definition
            .use_
            .as_ref()
            .and_then(|use_| use_.timeouts.as_ref())
            .and_then(|timeouts| timeouts.get(reference))
            .ok_or_else(|| {
                sqlx::Error::Protocol("private workflow timeout reference is missing".into())
            })?,
    };
    let duration_ms = match &timeout.after {
        OneOfDurationOrIso8601Expression::Duration(duration) => duration.total_milliseconds(),
        OneOfDurationOrIso8601Expression::Iso8601Expression(expression) => {
            parse_private_duration_ms(expression).ok_or_else(|| {
                sqlx::Error::Protocol("private workflow timeout duration is unsupported".into())
            })?
        }
    };
    let milliseconds = i64::try_from(duration_ms)
        .ok()
        .filter(|milliseconds| *milliseconds > 0)
        .ok_or_else(|| sqlx::Error::Protocol("private workflow timeout is invalid".into()))?;
    accepted_at
        .checked_add_signed(chrono::Duration::milliseconds(milliseconds))
        .map(Some)
        .ok_or_else(|| sqlx::Error::Protocol("private workflow timeout overflows".into()))
}

fn parse_private_duration_ms(expression: &str) -> Option<u64> {
    let rest = expression.strip_prefix('P')?;
    let (days, time) = rest.split_once('T').unwrap_or((rest, ""));
    let days = if days.is_empty() {
        0
    } else {
        days.strip_suffix('D')?.parse::<u64>().ok()?
    };
    let mut total = days.checked_mul(86_400_000)?;
    let mut digits = String::new();
    for character in time.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let amount = digits.parse::<u64>().ok()?;
        digits.clear();
        let multiplier = match character {
            'H' => 3_600_000,
            'M' => 60_000,
            'S' => 1_000,
            _ => return None,
        };
        total = total.checked_add(amount.checked_mul(multiplier)?)?;
    }
    (digits.is_empty() && total > 0).then_some(total)
}

fn bare_digest(value: &str) -> &str {
    value.strip_prefix("sha256:").unwrap_or(value)
}

#[cfg(test)]
mod private_profile_tests {
    use super::*;
    use chrono::Duration;
    use sqlx::postgres::PgPoolOptions;
    use workflow_invocation_contract::{
        CANONICAL_INPUT_PROFILE, CONTRACT_VERSION, ExecutionClass, IdempotencyBinding,
        InvocationBudget,
    };

    #[test]
    fn private_lifetime_uses_declared_timeout_without_seven_day_default() {
        let accepted = Utc::now();
        let plain: Value =
            serde_yaml::from_str(include_str!("../examples/human-approval.yaml")).unwrap();
        assert_eq!(declared_workflow_deadline(&plain, accepted).unwrap(), None);
        let mut explicit = plain.clone();
        explicit["timeout"] = json!({"after":"P10D"});
        assert_eq!(
            declared_workflow_deadline(&explicit, accepted).unwrap(),
            Some(accepted + Duration::days(10))
        );
        let mut malformed = plain;
        malformed["timeout"] = json!({"after":"P1M"});
        assert!(declared_workflow_deadline(&malformed, accepted).is_err());
    }

    #[tokio::test]
    #[ignore = "requires WORKFLOW_ROLE_TEST_DATABASE_URL for isolated PostgreSQL component schema"]
    async fn private_runner_start_persists_operational_placement() {
        let url = std::env::var("WORKFLOW_ROLE_TEST_DATABASE_URL")
            .expect("WORKFLOW_ROLE_TEST_DATABASE_URL is required");
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("workflow_step03_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(&format!("SET search_path TO {schema}"))
            .execute(&pool)
            .await
            .unwrap();
        for statement in [
            "CREATE TABLE process_info_t (host_id uuid,process_id uuid,wf_def_id uuid,wf_instance_id text,app_id text,process_type text,status_code text,ex_trigger_ts timestamptz,input_data jsonb,context_data jsonb,definition_snapshot jsonb,definition_digest text,policy_digest text,source_event_id text,execution_profile_id text,policy_snapshot_id uuid,deadline_ts timestamptz,update_user text)",
            "CREATE TABLE task_info_t (host_id uuid,task_id uuid,task_type text,process_id uuid,wf_instance_id text,wf_task_id text,status_code text,locked text,priority integer,deadline_ts timestamptz,task_input jsonb,execution_placement text,task_policy_digest text,update_user text,execution_class text)",
            "CREATE TABLE workflow_invocation_t (host_id uuid,workflow_instance_id uuid,binding_id uuid,process_id uuid,stable_tool_ref uuid,wf_def_id uuid,workflow_version text,definition_digest text,schema_digest text,policy_digest text,response_policy_digest text,principal_subject text,end_user_subject text,subject_claims jsonb,user_authorization text,user_authorization_exp bigint,input jsonb,input_digest text,canonical_input_profile text,invocation_mode text,execution_class text,permit_depth integer,state text,correlation_id text,deadline_ts timestamptz,cancellation_policy text,response_policy_snapshot jsonb)",
            "CREATE TABLE workflow_invocation_budget_t (host_id uuid,ledger_id uuid,workflow_instance_id uuid,task_attempt_limit bigint,nested_call_limit bigint,request_byte_limit bigint,byte_limit bigint,result_byte_limit bigint,cost_unit_limit bigint,deadline_ts timestamptz,lifetime_version smallint)",
            "CREATE TABLE workflow_invocation_audit_outbox_t (host_id uuid,event_id uuid,workflow_instance_id uuid,event_type text,payload jsonb,correlation_id text)",
            "CREATE FUNCTION workflow_claim_idempotency_v1(uuid,uuid,text,text,uuid,text,text,uuid,text,text,timestamptz,timestamptz) RETURNS TABLE(outcome text,accepted_workflow_instance_id uuid,accepted_generation bigint) LANGUAGE sql AS 'SELECT ''ACCEPTED''::text,$8,1::bigint'",
        ] {
            sqlx::query(statement).execute(&pool).await.unwrap();
        }
        let now = Utc::now();
        let input = json!({"message":"hello"});
        let input_digest = canonical_sha256(&input).unwrap();
        let digest = |ch: char| format!("sha256:{}", ch.to_string().repeat(64));
        let request = StartInvocationRequest {
            renewable_grant_id: None,
            parent_action_id: None,
            contract_version: CONTRACT_VERSION,
            workflow_instance_id: Uuid::now_v7(),
            stable_tool_ref: Uuid::now_v7(),
            workflow_definition_id: Uuid::now_v7(),
            workflow_version: "1.0.0".into(),
            definition_digest: digest('a'),
            schema_digest: digest('b'),
            policy_digest: digest('c'),
            response_policy_digest: digest('d'),
            mode: InvocationMode::Async,
            cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
            execution_class: ExecutionClass::Standard,
            permit_depth: 0,
            deadline_ts: now + Duration::minutes(10),
            canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
            normalized_input_digest: input_digest.clone(),
            input,
            caller_claims: json!({"sub":"member"}),
            idempotency: IdempotencyBinding {
                kind: IdempotencyKind::Derived,
                scoped_key_digest: digest('e'),
                input_digest,
                in_flight_until: now + Duration::minutes(11),
                result_replay_until: now + Duration::minutes(12),
            },
            budget: InvocationBudget {
                maximum_task_attempts: 10,
                maximum_nested_calls: 10,
                maximum_delegation_depth: 1,
                maximum_parallelism: 1,
                maximum_request_bytes: 65_536,
                maximum_intermediate_bytes: 65_536,
                maximum_result_bytes: 65_536,
                maximum_cost_units: 100,
            },
            correlation_id: "step03-component".into(),
        };
        let definition: Value =
            serde_yaml::from_str(include_str!("../examples/run-shell-mock-v1.yaml")).unwrap();
        let snapshot_id = Uuid::now_v7();
        let prepared = PreparedInvocationStart {
            binding_id: Some(Uuid::now_v7()),
            process_id: Uuid::now_v7(),
            initial_task_id: Uuid::now_v7(),
            application_id: "step03-test",
            initial_task_name: "printMessage",
            initial_task_type: "run",
            definition_snapshot: &definition,
            execution_placement: "runner",
            execution_profile_id: "mock-ephemeral",
            admission_profile: "portal_execution",
            policy_snapshot_id: Some(snapshot_id),
            task_policy_digest: "policy-component",
            public_output_schema: None,
        };
        let auth = AuthenticatedInvocationContext {
            host_id: Uuid::now_v7(),
            principal_subject: "workflow-test",
            end_user_subject: "member",
            update_user: "workflow-test",
            user_authorization: None,
            user_authorization_exp: None,
        };
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            accept_invocation(&mut tx, &auth, &request, &prepared)
                .await
                .unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        let placement: (String, String) = sqlx::query_as(
            "SELECT execution_placement,task_type FROM task_info_t WHERE task_id=$1",
        )
        .bind(prepared.initial_task_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(placement, ("runner".into(), "run".into()));
        let process: (String, Option<Uuid>, Option<chrono::DateTime<Utc>>) = sqlx::query_as("SELECT execution_profile_id,policy_snapshot_id,deadline_ts FROM process_info_t WHERE process_id=$1")
            .bind(prepared.process_id).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(process, ("mock-ephemeral".into(), Some(snapshot_id), None));
        let stored_profile: String = sqlx::query_scalar(
            "SELECT response_policy_snapshot->>'acceptedAdmissionProfile' FROM workflow_invocation_t WHERE process_id=$1",
        )
        .bind(prepared.process_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(stored_profile, "portal_execution");
        let lifetime: Value = sqlx::query_scalar(
            "SELECT response_policy_snapshot->'privateExecutionProfile' FROM workflow_invocation_t WHERE process_id=$1",
        )
        .bind(prepared.process_id).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(lifetime["version"], 1);
        assert_eq!(
            lifetime["waitPolicy"],
            "durable_until_explicit_timeout_or_terminal"
        );
        let budget: (Option<chrono::DateTime<Utc>>, Option<i16>) = sqlx::query_as(
            "SELECT deadline_ts,lifetime_version FROM workflow_invocation_budget_t WHERE workflow_instance_id=$1",
        ).bind(request.workflow_instance_id).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(budget, (None, Some(1)));
        let mut explicit_definition = definition.clone();
        explicit_definition["timeout"] = json!({"after":"P10D"});
        let mut explicit_request = request.clone();
        explicit_request.workflow_instance_id = Uuid::now_v7();
        explicit_request.idempotency.scoped_key_digest = digest('f');
        let explicit_prepared = PreparedInvocationStart {
            process_id: Uuid::now_v7(),
            initial_task_id: Uuid::now_v7(),
            definition_snapshot: &explicit_definition,
            ..prepared
        };
        assert!(matches!(
            accept_invocation(&mut tx, &auth, &explicit_request, &explicit_prepared)
                .await
                .unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        let explicit_deadline: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT deadline_ts FROM process_info_t WHERE process_id=$1")
                .bind(explicit_prepared.process_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(explicit_deadline > now + Duration::days(9));
        let budget_deadline: chrono::DateTime<Utc> = sqlx::query_scalar(
            "SELECT deadline_ts FROM workflow_invocation_budget_t WHERE workflow_instance_id=$1",
        )
        .bind(explicit_request.workflow_instance_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(budget_deadline, explicit_deadline);
        let mut child_request = explicit_request.clone();
        child_request.workflow_instance_id = Uuid::now_v7();
        child_request.idempotency.scoped_key_digest = digest('9');
        child_request.parent_action_id = Some(Uuid::now_v7());
        child_request.permit_depth = 1;
        let child_prepared = PreparedInvocationStart {
            process_id: Uuid::now_v7(),
            initial_task_id: Uuid::now_v7(),
            ..explicit_prepared
        };
        assert!(matches!(
            accept_invocation(&mut tx, &auth, &child_request, &child_prepared)
                .await
                .unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        let child_deadline: chrono::DateTime<Utc> =
            sqlx::query_scalar("SELECT deadline_ts FROM process_info_t WHERE process_id=$1")
                .bind(child_prepared.process_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert!(
            (child_deadline - child_request.deadline_ts)
                .num_milliseconds()
                .abs()
                < 1
        );
        tx.rollback().await.unwrap();
        sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
            .execute(&pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires disposable workflow_step04_test PostgreSQL database with Workflow migrations 0001, 0007, 0012 and 0013"]
    async fn native_acceptance_real_idempotency_keeps_one_process_and_task() {
        let url = std::env::var("WORKFLOW_ROLE_TEST_DATABASE_URL").unwrap();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops, pg_catalog")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(database, "workflow_step04_test");
        let (host, definition_id, binding_id, tool_id, owner) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query(
            "INSERT INTO workflow_ops.wf_definition_t
            (host_id,wf_def_id,namespace,name,version,definition)
            VALUES($1,$2,'step05','native-idempotency','1.0.0','{}')",
        )
        .bind(host)
        .bind(definition_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workflow_ops.workflow_tool_binding_t
            (host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,
             schema_digest,policy_digest,response_policy_digest,invocation_mode,sync_wait_ms,
             total_deadline_ms,execution_class,result_text_mode,idempotency_policy,
             delegation_policy,runtime_bounds)
            VALUES($1,$2,$3,$4,'1.0.0',$5,$5,$5,$5,'async',1000,3600000,
             'standard','compact-json','{}','{}','{}')",
        )
        .bind(host)
        .bind(binding_id)
        .bind(tool_id)
        .bind(definition_id)
        .bind(&digest)
        .execute(&pool)
        .await
        .unwrap();
        let now = Utc::now();
        let input = json!({"message":"same"});
        let input_digest = canonical_sha256(&input).unwrap();
        let owner_string = owner.to_string();
        let request = StartInvocationRequest {
            renewable_grant_id: None,
            parent_action_id: None,
            contract_version: CONTRACT_VERSION,
            workflow_instance_id: Uuid::now_v7(),
            stable_tool_ref: tool_id,
            workflow_definition_id: definition_id,
            workflow_version: "1.0.0".into(),
            definition_digest: digest.clone(),
            schema_digest: digest.clone(),
            policy_digest: digest.clone(),
            response_policy_digest: digest.clone(),
            mode: InvocationMode::Async,
            cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
            execution_class: ExecutionClass::Standard,
            permit_depth: 0,
            deadline_ts: now + Duration::hours(1),
            canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
            normalized_input_digest: input_digest.clone(),
            input,
            caller_claims: json!({"uid":owner_string,"host":host.to_string()}),
            idempotency: IdempotencyBinding {
                kind: IdempotencyKind::Explicit,
                scoped_key_digest: canonical_sha256(&json!({"key":"same"})).unwrap(),
                input_digest,
                in_flight_until: now + Duration::hours(1),
                result_replay_until: now + Duration::days(1),
            },
            budget: InvocationBudget {
                maximum_task_attempts: 10,
                maximum_nested_calls: 10,
                maximum_delegation_depth: 8,
                maximum_parallelism: 1,
                maximum_request_bytes: 65_536,
                maximum_intermediate_bytes: 65_536,
                maximum_result_bytes: 65_536,
                maximum_cost_units: 100,
            },
            correlation_id: "step05-concurrent".into(),
        };
        let definition: Value =
            serde_yaml::from_str(include_str!("../examples/human-approval.yaml")).unwrap();
        let prepared = |process_id, initial_task_id| PreparedInvocationStart {
            binding_id: Some(binding_id),
            process_id,
            initial_task_id,
            application_id: "step05-test",
            initial_task_name: "initial",
            initial_task_type: "ask",
            definition_snapshot: &definition,
            execution_placement: "host",
            execution_profile_id: "host",
            admission_profile: "portal_execution",
            policy_snapshot_id: None,
            task_policy_digest: digest.trim_start_matches("sha256:"),
            public_output_schema: None,
        };
        let auth = AuthenticatedInvocationContext {
            host_id: host,
            principal_subject: "portal-user",
            end_user_subject: &owner_string,
            update_user: "step05-test",
            user_authorization: None,
            user_authorization_exp: None,
        };
        let mut replay_request = request.clone();
        replay_request.workflow_instance_id = Uuid::now_v7();
        let first_prepared = prepared(Uuid::now_v7(), Uuid::now_v7());
        let replay_prepared = prepared(Uuid::now_v7(), Uuid::now_v7());
        let first = async {
            let mut tx = pool.begin().await.unwrap();
            let outcome = accept_invocation(&mut tx, &auth, &request, &first_prepared)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            outcome
        };
        let second = async {
            let mut tx = pool.begin().await.unwrap();
            let outcome = accept_invocation(&mut tx, &auth, &replay_request, &replay_prepared)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            outcome
        };
        let (a, b) = tokio::join!(first, second);
        assert_eq!(
            usize::from(matches!(a, AcceptOutcome::Accepted { .. }))
                + usize::from(matches!(b, AcceptOutcome::Accepted { .. })),
            1
        );
        assert_eq!(
            usize::from(matches!(a, AcceptOutcome::Replay { .. }))
                + usize::from(matches!(b, AcceptOutcome::Replay { .. })),
            1
        );
        for table in ["workflow_invocation_t", "process_info_t", "task_info_t"] {
            let count: i64 = sqlx::query_scalar(&format!(
                "SELECT count(*) FROM workflow_ops.{table} WHERE host_id=$1"
            ))
            .bind(host)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(count, 1, "{table}");
        }
        let mut conflicting = request.clone();
        conflicting.workflow_instance_id = Uuid::now_v7();
        conflicting.input = json!({"message":"different"});
        conflicting.normalized_input_digest = canonical_sha256(&conflicting.input).unwrap();
        conflicting.idempotency.input_digest = conflicting.normalized_input_digest.clone();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            accept_invocation(
                &mut tx,
                &auth,
                &conflicting,
                &prepared(Uuid::now_v7(), Uuid::now_v7())
            )
            .await,
            Err(InvocationAcceptError::IdempotencyConflict)
        ));
        tx.rollback().await.unwrap();
        let mut rolled_back = request.clone();
        rolled_back.workflow_instance_id = Uuid::now_v7();
        rolled_back.idempotency.scoped_key_digest =
            canonical_sha256(&json!({"key":"rollback"})).unwrap();
        let mut tx = pool.begin().await.unwrap();
        assert!(matches!(
            accept_invocation(
                &mut tx,
                &auth,
                &rolled_back,
                &prepared(Uuid::now_v7(), Uuid::now_v7())
            )
            .await
            .unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        tx.rollback().await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM workflow_ops.workflow_invocation_t WHERE host_id=$1",
        )
        .bind(host)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
    }
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
