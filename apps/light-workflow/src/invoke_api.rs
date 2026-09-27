//! Gateway-internal admission for approved workflow-backed Tools.
use crate::invocation::AcceptOutcome;
use crate::rule_api::{
    AdmissionProfile, ApiError, InvocationIdentity, RuleApiState, authenticate_invoke, load_status,
    read_admissible_pinned_binding, start_invocation_with_stage, wait_for_terminal_until,
};
use crate::run_credential::SealedRunCredential;
use axum::{
    Json,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
use workflow_invocation_contract::{
    CANONICAL_INPUT_PROFILE, CONTRACT_VERSION, CancellationPolicy, ErrorCode, ExecutionClass,
    IdempotencyBinding, IdempotencyKind, InvocationBudget, InvocationMode, InvocationState,
    InvocationStatus, StartInvocationRequest, canonical_sha256,
};
use zeroize::Zeroizing;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InvokeInput {
    stable_tool_ref: Uuid,
    expected_binding_digest: String,
    expected_definition_digest: String,
    input: Value,
    idempotency_key: Option<String>,
    parent_action_id: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Limits {
    maximum_concurrent_runs: i64,
    maximum_concurrent_runs_per_user: i64,
    starts_per_minute: i64,
    starts_per_minute_per_user: i64,
}

pub struct InvokeAdmission {
    revision_id: Uuid,
    binding_digest: String,
    limits: Limits,
    sealed: SealedRunCredential,
    vault: Arc<crate::run_credential::RunCredentialVault>,
    token: Zeroizing<String>,
    token_exp: i64,
}

fn cancellation(value: &str) -> Result<CancellationPolicy, ApiError> {
    match value {
        "before-effects-only" => Ok(CancellationPolicy::BeforeEffectsOnly),
        "cooperative" => Ok(CancellationPolicy::Cooperative),
        "disabled" => Ok(CancellationPolicy::Disabled),
        _ => Err(ApiError::input_invalid(
            "binding cancellation policy is invalid",
        )),
    }
}

fn class(value: &str) -> Result<ExecutionClass, ApiError> {
    match value {
        "interactive" => Ok(ExecutionClass::Interactive),
        "standard" => Ok(ExecutionClass::Standard),
        "batch" => Ok(ExecutionClass::Batch),
        _ => Err(ApiError::input_invalid(
            "binding execution class is invalid",
        )),
    }
}

fn scope_digest(
    kind: &str,
    identity: &InvocationIdentity,
    tool: Uuid,
    key: &str,
) -> Result<String, ApiError> {
    canonical_sha256(&json!({"v":1,"kind":kind,"hostId":identity.host_id,
        "toolId":tool,"endUserSubject":identity.end_user_subject,"key":key}))
    .map_err(|_| ApiError::input_invalid("idempotency scope is invalid"))
}

fn selected_key<'a>(
    kind: IdempotencyKind,
    input_digest: &'a str,
    supplied: Option<&'a str>,
) -> Result<&'a str, ApiError> {
    if kind == IdempotencyKind::Derived {
        return Ok(input_digest);
    }
    supplied
        .filter(|key| !key.is_empty() && key.len() <= 256 && !key.chars().any(char::is_control))
        .ok_or_else(|| ApiError::input_invalid("idempotencyKey is required for this binding"))
}

#[derive(Debug, PartialEq, Eq)]
enum ReservationDecision {
    Fresh,
    Replay,
    Conflict,
}

fn reservation_decision(
    terminal: bool,
    replay_until: chrono::DateTime<Utc>,
    same: bool,
    now: chrono::DateTime<Utc>,
) -> ReservationDecision {
    if terminal && replay_until <= now {
        ReservationDecision::Fresh
    } else if same {
        ReservationDecision::Replay
    } else {
        ReservationDecision::Conflict
    }
}

fn budget_from_revision(bounds: &Value, delegation: &Value) -> Result<InvocationBudget, ApiError> {
    let u = |name: &str| {
        bounds.get(name).and_then(Value::as_u64).ok_or_else(|| {
            ApiError::input_invalid(format!("binding runtimeBounds.{name} is invalid"))
        })
    };
    Ok(InvocationBudget {
        maximum_task_attempts: u32::try_from(u("maximumTaskAttempts")?)
            .map_err(|_| ApiError::input_invalid("task attempt limit is invalid"))?,
        maximum_nested_calls: u32::try_from(u("maximumNestedCalls")?)
            .map_err(|_| ApiError::input_invalid("nested call limit is invalid"))?,
        maximum_delegation_depth: u16::try_from(
            delegation
                .get("maximumDelegationDepth")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        )
        .map_err(|_| ApiError::input_invalid("delegation limit is invalid"))?,
        maximum_parallelism: u16::try_from(u("maximumParallelism")?)
            .map_err(|_| ApiError::input_invalid("parallelism limit is invalid"))?,
        maximum_request_bytes: u("maximumRequestBytes")?,
        maximum_intermediate_bytes: u("maximumIntermediateBytes")?,
        maximum_result_bytes: u("maximumResultBytes")?,
        maximum_cost_units: u("maximumCostUnits")?,
    })
}

pub fn reject_parent_action(parent: Option<Uuid>) -> Result<(), ApiError> {
    if parent.is_some() {
        Err(ApiError::policy_denied(
            "nested workflow invocation is unavailable",
        ))
    } else {
        Ok(())
    }
}

pub fn verify_gateway_pins(
    expected_binding: &str,
    actual_binding: &str,
    expected_definition: &str,
    actual_definition: &str,
) -> Result<(), ApiError> {
    if expected_binding != actual_binding || expected_definition != actual_definition {
        Err(ApiError::definition_mismatch(
            "Gateway binding or definition pin does not match Workflow",
        ))
    } else {
        Ok(())
    }
}

pub fn verify_caller_policy(policy: &Value, claims: &Value) -> Result<(), ApiError> {
    if policy.get("anyRole").is_some() && policy.get("anyRole").and_then(Value::as_array).is_none()
    {
        return Err(ApiError::policy_denied("binding caller policy is invalid"));
    }
    if let Some(roles) = policy.get("anyRole").and_then(Value::as_array) {
        let verified_roles = ["role", "roles"]
            .into_iter()
            .filter_map(|name| claims.get(name))
            .flat_map(|value| match value {
                Value::String(text) => text.split_whitespace().collect::<Vec<_>>(),
                Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
                _ => Vec::new(),
            })
            .collect::<std::collections::HashSet<_>>();
        if !roles.is_empty()
            && !roles
                .iter()
                .filter_map(Value::as_str)
                .any(|role| verified_roles.contains(role))
        {
            return Err(ApiError::policy_denied(
                "caller does not satisfy the binding role policy",
            ));
        }
    }
    Ok(())
}

pub fn require_vault(
    vault: Option<Arc<crate::run_credential::RunCredentialVault>>,
) -> Result<Arc<crate::run_credential::RunCredentialVault>, ApiError> {
    vault.ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::WorkflowStartRejected,
            "run credential vault is not configured",
        )
    })
}

pub(crate) async fn invoke(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
) -> Result<Value, ApiError> {
    invoke_with_interlude(state, headers, arguments, std::future::ready(())).await
}

async fn invoke_with_interlude<I: std::future::Future<Output = ()>>(
    state: RuleApiState,
    headers: HeaderMap,
    arguments: Value,
    interlude: I,
) -> Result<Value, ApiError> {
    let input: InvokeInput = serde_json::from_value(arguments)
        .map_err(|_| ApiError::input_invalid("invalid workflow_invoke arguments"))?;
    let (identity, generation) = authenticate_invoke(&state, &headers).await?;
    reject_parent_action(input.parent_action_id)?;
    if input.stable_tool_ref.is_nil()
        || !valid_digest(&input.expected_binding_digest)
        || !valid_digest(&input.expected_definition_digest)
        || !input.input.is_object()
    {
        return Err(ApiError::input_invalid(
            "workflow invoke identity, pins or input is invalid",
        ));
    }
    let row = read_admissible_pinned_binding(&state.pool, identity.host_id, input.stable_tool_ref)
        .await?;
    let binding_digest: String = row.try_get("binding_digest").map_err(ApiError::database)?;
    let revision_id: Uuid = row.try_get("binding_id").map_err(ApiError::database)?;
    let definition_digest: String = row
        .try_get("binding_definition_digest")
        .map_err(ApiError::database)?;
    verify_gateway_pins(
        &input.expected_binding_digest,
        &binding_digest,
        &input.expected_definition_digest,
        &definition_digest,
    )?;
    let policy: Value = row.try_get("caller_policy").map_err(ApiError::database)?;
    verify_caller_policy(&policy, &identity.caller_claims)?;
    let now = Utc::now();
    let deadline_ms: i32 = row
        .try_get("total_deadline_ms")
        .map_err(ApiError::database)?;
    let deadline = now
        .checked_add_signed(Duration::milliseconds(i64::from(deadline_ms)))
        .ok_or_else(|| ApiError::input_invalid("binding deadline is invalid"))?;
    let kind: Value = row
        .try_get("idempotency_policy")
        .map_err(ApiError::database)?;
    let kind_name = kind
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::input_invalid("binding idempotency policy is invalid"))?;
    let kind = match kind_name {
        "derived" => IdempotencyKind::Derived,
        "explicit" => IdempotencyKind::Explicit,
        "business" => IdempotencyKind::Business,
        _ => {
            return Err(ApiError::input_invalid(
                "binding idempotency policy is invalid",
            ));
        }
    };
    let input_digest = canonical_sha256(&input.input)
        .map_err(|_| ApiError::input_invalid("workflow input is invalid"))?;
    let key = selected_key(kind, &input_digest, input.idempotency_key.as_deref())?;
    let scoped_key_digest = scope_digest(kind_name, &identity, input.stable_tool_ref, key)?;
    let bounds: Value = row.try_get("runtime_bounds").map_err(ApiError::database)?;
    let delegation: Value = row
        .try_get("delegation_policy")
        .map_err(ApiError::database)?;
    let budget = budget_from_revision(&bounds, &delegation)?;
    let run = Uuid::now_v7();
    let vault = require_vault(state.run_credential_vault.clone())?;
    if let Some(replayed) = precheck(
        &state,
        &identity,
        &scoped_key_digest,
        input.stable_tool_ref,
        &definition_digest,
        &input_digest,
        &vault,
    )
    .await?
    {
        return finish_invoke(&state, &headers, &identity, &generation, replayed).await;
    }
    let token = original_user_token(&identity.user_authorization)?;
    let sealed = vault
        .seal(run, token, identity.user_authorization_exp)
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::WorkflowStartRejected,
                "run credential vault is unavailable",
            )
        })?;
    let request = StartInvocationRequest {
        renewable_grant_id: None,
        parent_action_id: None,
        contract_version: CONTRACT_VERSION,
        workflow_instance_id: run,
        stable_tool_ref: input.stable_tool_ref,
        workflow_definition_id: row.try_get("wf_def_id").map_err(ApiError::database)?,
        workflow_version: row
            .try_get("workflow_version")
            .map_err(ApiError::database)?,
        definition_digest,
        schema_digest: row.try_get("schema_digest").map_err(ApiError::database)?,
        policy_digest: row.try_get("policy_digest").map_err(ApiError::database)?,
        response_policy_digest: row
            .try_get("response_policy_digest")
            .map_err(ApiError::database)?,
        mode: match row
            .try_get::<String, _>("invocation_mode")
            .map_err(ApiError::database)?
            .as_str()
        {
            "sync" => InvocationMode::Sync,
            "async" => InvocationMode::Async,
            _ => {
                return Err(ApiError::input_invalid(
                    "binding invocation mode is invalid",
                ));
            }
        },
        cancellation_policy: cancellation(
            &row.try_get::<String, _>("cancellation_policy")
                .map_err(ApiError::database)?,
        )?,
        execution_class: class(
            &row.try_get::<String, _>("execution_class")
                .map_err(ApiError::database)?,
        )?,
        permit_depth: 0,
        deadline_ts: deadline,
        canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
        normalized_input_digest: input_digest.clone(),
        input: input.input,
        caller_claims: identity.caller_claims.clone(),
        idempotency: IdempotencyBinding {
            kind,
            scoped_key_digest,
            input_digest,
            in_flight_until: deadline,
            result_replay_until: deadline,
        },
        budget,
        correlation_id: headers
            .get("x-correlation-id")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .map(str::to_owned)
            .unwrap_or_else(|| Uuid::now_v7().to_string()),
    };
    let limits: Value = row
        .try_get("admission_limits")
        .map_err(ApiError::database)?;
    interlude.await;
    let (_, Json(status)) = start_invocation_with_stage(
        State(state.clone()),
        None,
        None,
        headers.clone(),
        request,
        None,
        None,
        AdmissionProfile::WorkflowBacked,
        None,
        Some(InvokeAdmission::new(
            revision_id,
            binding_digest,
            limits,
            vault.clone(),
            sealed,
            token,
            identity.user_authorization_exp,
        )?),
    )
    .await?;
    finish_invoke(&state, &headers, &identity, &generation, status).await
}

async fn finish_invoke(
    state: &RuleApiState,
    headers: &HeaderMap,
    identity: &InvocationIdentity,
    generation: &Arc<crate::configuration::WorkflowConfigGeneration>,
    mut status: InvocationStatus,
) -> Result<Value, ApiError> {
    let wait_ms: i32 = sqlx::query_scalar(
        "SELECT b.sync_wait_ms FROM workflow_invocation_t i
         JOIN workflow_tool_binding_t b ON b.host_id=i.host_id AND b.binding_id=i.binding_id
         WHERE i.host_id=$1 AND i.workflow_instance_id=$2",
    )
    .bind(identity.host_id)
    .bind(status.workflow_instance_id)
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if !status.state.is_terminal() {
        let remaining = (status.deadline_ts - Utc::now()).num_milliseconds();
        let bounded = i64::from(wait_ms).min(remaining).max(0) as u64;
        if bounded > 0 {
            status = wait_for_terminal_until(state, identity, generation, status, bounded).await?;
        }
    }
    if status.state == InvocationState::Completed {
        return Ok(
            json!({"status":"completed","workflowInstanceId":status.workflow_instance_id,
            "definitionDigest":status.definition_digest,"output":status.public_result.unwrap_or_else(||json!({}))}),
        );
    }
    if status.state.is_terminal() {
        return Err(ApiError::run_failure(status));
    }
    if status.deadline_ts > Utc::now() {
        return Err(ApiError::run_timeout(&status, true));
    }
    let cancelled = crate::rule_api::cancel_invocation(
        State(state.clone()),
        headers.clone(),
        Path(status.workflow_instance_id),
    )
    .await?
    .0;
    if cancelled.state == InvocationState::Completed {
        return Ok(
            json!({"status":"completed","workflowInstanceId":cancelled.workflow_instance_id,
            "definitionDigest":cancelled.definition_digest,"output":cancelled.public_result.unwrap_or_else(||json!({}))}),
        );
    }
    if cancelled.state == InvocationState::Failed {
        return Err(ApiError::run_failure(cancelled));
    }
    Err(ApiError::run_timeout(&cancelled, false))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn original_user_token(value: &str) -> Result<&str, ApiError> {
    let (scheme, token) = value
        .split_once(' ')
        .ok_or_else(|| ApiError::unauthorized("user Bearer authentication is required"))?;
    if !scheme.eq_ignore_ascii_case("Bearer") || token.trim().is_empty() {
        return Err(ApiError::unauthorized(
            "user Bearer authentication is required",
        ));
    }
    Ok(token.trim())
}

async fn precheck(
    state: &RuleApiState,
    identity: &InvocationIdentity,
    scope: &str,
    tool: Uuid,
    definition: &str,
    input: &str,
    vault: &crate::run_credential::RunCredentialVault,
) -> Result<Option<InvocationStatus>, ApiError> {
    let row = sqlx::query("SELECT k.workflow_instance_id,k.stable_tool_ref,k.principal_subject,k.end_user_subject,
            k.definition_digest,k.input_digest,k.result_replay_until,i.state
        FROM workflow_invocation_idempotency_t k
        JOIN workflow_invocation_t i ON i.host_id=k.host_id AND i.workflow_instance_id=k.workflow_instance_id
        WHERE k.host_id=$1 AND k.scope_digest=$2 AND k.active")
        .bind(identity.host_id).bind(scope).fetch_optional(&state.pool).await.map_err(ApiError::database)?;
    let Some(row) = row else { return Ok(None) };
    let status: String = row.try_get("state").map_err(ApiError::database)?;
    let terminal = matches!(status.as_str(), "COMPLETED" | "FAILED" | "CANCELLED");
    let until: chrono::DateTime<Utc> = row
        .try_get("result_replay_until")
        .map_err(ApiError::database)?;
    let same = row
        .try_get::<Uuid, _>("stable_tool_ref")
        .map_err(ApiError::database)?
        == tool
        && row
            .try_get::<String, _>("principal_subject")
            .map_err(ApiError::database)?
            == identity.principal_subject
        && row
            .try_get::<String, _>("end_user_subject")
            .map_err(ApiError::database)?
            == identity.end_user_subject
        && row
            .try_get::<String, _>("definition_digest")
            .map_err(ApiError::database)?
            == definition
        && row
            .try_get::<String, _>("input_digest")
            .map_err(ApiError::database)?
            == input;
    match reservation_decision(terminal, until, same, Utc::now()) {
        ReservationDecision::Fresh => return Ok(None),
        ReservationDecision::Conflict => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                ErrorCode::WorkflowIdempotencyConflict,
                "idempotency key is already bound to different input",
            ));
        }
        ReservationDecision::Replay => {}
    }
    let run: Uuid = row
        .try_get("workflow_instance_id")
        .map_err(ApiError::database)?;
    let status = load_status(&state.pool, identity, run).await?;
    if !terminal {
        let token = original_user_token(&identity.user_authorization)?;
        let sealed = vault
            .seal(run, token, identity.user_authorization_exp)
            .map_err(|_| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    ErrorCode::WorkflowStartRejected,
                    "run credential vault is unavailable",
                )
            })?;
        let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
        sealed
            .refresh_if_later(&mut tx, identity.host_id, run)
            .await
            .map_err(ApiError::database)?;
        tx.commit().await.map_err(ApiError::database)?;
    }
    Ok(Some(status))
}

impl InvokeAdmission {
    pub(crate) fn verify_loaded_revision(
        &self,
        revision_id: Uuid,
        digest: &str,
    ) -> Result<(), ApiError> {
        if revision_id != self.revision_id || digest != self.binding_digest {
            Err(ApiError::definition_mismatch(
                "workflow Tool binding changed during admission",
            ))
        } else {
            Ok(())
        }
    }

    pub async fn fence_revision(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        tool: Uuid,
    ) -> Result<(), ApiError> {
        // Publication and decisions take FOR UPDATE on this head before changing
        // any revision. Hold a conflicting lock through the acceptance commit.
        let row = sqlx::query("SELECT h.active_binding_id,b.binding_digest,b.active,b.revision_status,
                v.version_status FROM workflow_tool_publication_t h
                LEFT JOIN workflow_tool_binding_t b ON b.host_id=h.host_id AND b.binding_id=h.active_binding_id
                LEFT JOIN wf_definition_version_t v ON v.host_id=b.host_id AND v.wf_def_id=b.wf_def_id AND v.version=b.workflow_version
                WHERE h.host_id=$1 AND h.tool_id=$2 FOR SHARE OF h")
            .bind(host).bind(tool).fetch_optional(&mut **tx).await.map_err(ApiError::database)?;
        let Some(row) = row else {
            return Err(ApiError::definition_mismatch(
                "workflow Tool binding changed during admission",
            ));
        };
        let id: Option<Uuid> = row
            .try_get("active_binding_id")
            .map_err(ApiError::database)?;
        let digest: Option<String> = row.try_get("binding_digest").map_err(ApiError::database)?;
        let active: Option<bool> = row.try_get("active").map_err(ApiError::database)?;
        let revision_status: Option<String> =
            row.try_get("revision_status").map_err(ApiError::database)?;
        let version_status: Option<String> =
            row.try_get("version_status").map_err(ApiError::database)?;
        if id != Some(self.revision_id)
            || digest.as_deref() != Some(&self.binding_digest)
            || active != Some(true)
            || revision_status.as_deref() != Some("approved")
            || version_status.as_deref() != Some("active")
        {
            return Err(ApiError::definition_mismatch(
                "workflow Tool binding changed during admission",
            ));
        }
        Ok(())
    }

    pub fn new(
        revision_id: Uuid,
        binding_digest: String,
        limits: Value,
        vault: Arc<crate::run_credential::RunCredentialVault>,
        sealed: SealedRunCredential,
        token: &str,
        token_exp: i64,
    ) -> Result<Self, ApiError> {
        let limits: Limits = serde_json::from_value(limits)
            .map_err(|_| ApiError::input_invalid("binding admission limits are invalid"))?;
        if [
            limits.maximum_concurrent_runs,
            limits.maximum_concurrent_runs_per_user,
            limits.starts_per_minute,
            limits.starts_per_minute_per_user,
        ]
        .iter()
        .any(|value| *value < 1)
        {
            return Err(ApiError::input_invalid(
                "binding admission limits are invalid",
            ));
        }
        Ok(Self {
            revision_id,
            binding_digest,
            limits,
            sealed,
            vault,
            token: Zeroizing::new(token.to_owned()),
            token_exp,
        })
    }

    pub async fn apply(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        end_user_subject: &str,
        request: &StartInvocationRequest,
        outcome: &AcceptOutcome,
    ) -> Result<(), ApiError> {
        let run = match outcome {
            AcceptOutcome::Accepted {
                workflow_instance_id,
            } => *workflow_instance_id,
            AcceptOutcome::Replay {
                workflow_instance_id,
                ..
            } => {
                let state: String = sqlx::query_scalar("SELECT state FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2")
                    .bind(host).bind(workflow_instance_id).fetch_one(&mut **tx).await.map_err(ApiError::database)?;
                if !matches!(state.as_str(), "COMPLETED" | "FAILED" | "CANCELLED") {
                    let replacement = self
                        .vault
                        .seal(*workflow_instance_id, &self.token, self.token_exp)
                        .map_err(|_| {
                            ApiError::new(
                                StatusCode::SERVICE_UNAVAILABLE,
                                ErrorCode::WorkflowStartRejected,
                                "run credential vault is unavailable",
                            )
                        })?;
                    replacement
                        .refresh_if_later(tx, host, *workflow_instance_id)
                        .await
                        .map_err(ApiError::database)?;
                }
                return Ok(());
            }
        };
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text || $2::text,0))")
            .bind(host)
            .bind(request.stable_tool_ref)
            .execute(&mut **tx)
            .await
            .map_err(ApiError::database)?;
        let (open_all, open_user, recent_all, recent_user, oldest_all, oldest_user):
            (i64,i64,i64,i64,Option<chrono::DateTime<Utc>>,Option<chrono::DateTime<Utc>>) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE state NOT IN ('COMPLETED','FAILED','CANCELLED')),
                    count(*) FILTER (WHERE state NOT IN ('COMPLETED','FAILED','CANCELLED') AND end_user_subject=$3),
                    count(*) FILTER (WHERE accepted_ts > CURRENT_TIMESTAMP-interval '60 seconds'),
                    count(*) FILTER (WHERE accepted_ts > CURRENT_TIMESTAMP-interval '60 seconds' AND end_user_subject=$3),
                    min(accepted_ts) FILTER (WHERE accepted_ts > CURRENT_TIMESTAMP-interval '60 seconds'),
                    min(accepted_ts) FILTER (WHERE accepted_ts > CURRENT_TIMESTAMP-interval '60 seconds' AND end_user_subject=$3)
               FROM workflow_invocation_t WHERE host_id=$1 AND stable_tool_ref=$2 AND response_policy_snapshot->>'acceptedAdmissionProfile'='workflow_backed'")
            .bind(host).bind(request.stable_tool_ref).bind(end_user_subject)
            .fetch_one(&mut **tx).await.map_err(ApiError::database)?;
        let retry = if open_all > self.limits.maximum_concurrent_runs
            || open_user > self.limits.maximum_concurrent_runs_per_user
        {
            Some(1000)
        } else if recent_all > self.limits.starts_per_minute
            || recent_user > self.limits.starts_per_minute_per_user
        {
            let oldest = if recent_all > self.limits.starts_per_minute {
                oldest_all
            } else {
                oldest_user
            };
            Some(oldest.map_or(1000, |ts| {
                (ts + Duration::seconds(60) - Utc::now())
                    .num_milliseconds()
                    .max(1000) as u64
            }))
        } else {
            None
        };
        if let Some(retry) = retry {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorCode::WorkflowCapacityExhausted,
                "workflow Tool admission limit exceeded",
            )
            .with_retry_after(retry));
        }
        self.sealed
            .insert(tx, host, run, request.deadline_ts)
            .await
            .map_err(ApiError::database)?;
        let user: Uuid = end_user_subject
            .parse()
            .map_err(|_| ApiError::unauthorized("workflow user identity is invalid"))?;
        sqlx::query("INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit,maximum_depth,credential_kind) VALUES($1,$2,$2,$3,1,1,1,true,$4,$5,$6,'invoke')")
            .bind(host).bind(run).bind(user).bind(request.deadline_ts)
            .bind(i64::from(request.budget.maximum_nested_calls))
            .bind(i32::from(request.budget.maximum_delegation_depth))
            .execute(&mut **tx).await.map_err(ApiError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_strings_are_explicit() {
        assert_eq!(
            cancellation("before-effects-only").unwrap(),
            CancellationPolicy::BeforeEffectsOnly
        );
        assert_eq!(
            cancellation("cooperative").unwrap(),
            CancellationPolicy::Cooperative
        );
        assert_eq!(
            cancellation("disabled").unwrap(),
            CancellationPolicy::Disabled
        );
        assert!(cancellation("BeforeEffectsOnly").is_err());
        assert_eq!(original_user_token("bearer  token ").unwrap(), "token");
        assert!(original_user_token("Basic token").is_err());
    }

    #[test]
    fn revision_bounds_build_the_invocation_budget() {
        let budget = budget_from_revision(
            &json!({
                "maximumTaskAttempts":8,"maximumNestedCalls":7,"maximumParallelism":2,
                "maximumRequestBytes":1024,"maximumIntermediateBytes":2048,
                "maximumResultBytes":4096,"maximumCostUnits":10
            }),
            &json!({"maximumDelegationDepth":1}),
        )
        .unwrap();
        assert_eq!(budget.maximum_task_attempts, 8);
        assert_eq!(budget.maximum_nested_calls, 7);
        assert_eq!(budget.maximum_delegation_depth, 1);
        assert_eq!(budget.maximum_result_bytes, 4096);
        assert_eq!(class("interactive").unwrap(), ExecutionClass::Interactive);
        assert!(class("unknown").is_err());
    }

    #[test]
    fn explicit_key_is_required_and_scope_is_stable_across_clients() {
        assert!(selected_key(IdempotencyKind::Explicit, "digest", None).is_err());
        assert_eq!(
            selected_key(IdempotencyKind::Derived, "digest", None).unwrap(),
            "digest"
        );
        let host = Uuid::new_v4();
        let tool = Uuid::new_v4();
        let identity = |principal: &str| InvocationIdentity {
            host_id: host,
            principal_subject: principal.into(),
            end_user_subject: "user".into(),
            caller_claims_digest: "digest".into(),
            caller_claims: json!({}),
            user_authorization: "Bearer test".into(),
            user_authorization_exp: 100,
        };
        let a = scope_digest("explicit", &identity("client-a"), tool, "key").unwrap();
        let b = scope_digest("explicit", &identity("client-b"), tool, "key").unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a,
            scope_digest("explicit", &identity("client-a"), tool, "other").unwrap()
        );
    }

    #[test]
    fn reservation_precheck_follows_the_d14_table() {
        let now = Utc::now();
        let future = now + Duration::minutes(1);
        let past = now - Duration::seconds(1);
        assert_eq!(
            reservation_decision(false, past, true, now),
            ReservationDecision::Replay
        );
        assert_eq!(
            reservation_decision(false, past, false, now),
            ReservationDecision::Conflict
        );
        assert_eq!(
            reservation_decision(true, future, true, now),
            ReservationDecision::Replay
        );
        assert_eq!(
            reservation_decision(true, future, false, now),
            ReservationDecision::Conflict
        );
        assert_eq!(
            reservation_decision(true, past, true, now),
            ReservationDecision::Fresh
        );
        assert_eq!(
            reservation_decision(true, past, false, now),
            ReservationDecision::Fresh
        );
    }
}

#[cfg(test)]
pub(crate) mod handler_postgres_tests {
    use super::*;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use sqlx::{PgPool, postgres::PgPoolOptions};
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn owned_gateway() -> (
        tempfile::TempDir,
        String,
        Arc<tokio::sync::Mutex<Vec<(HeaderMap, Value)>>>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let directory = tempfile::tempdir().unwrap();
        let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert = directory.path().join("cert.pem");
        let key = directory.path().join("key.pem");
        tokio::fs::write(&cert, certificate.cert.pem())
            .await
            .unwrap();
        tokio::fs::write(&key, certificate.signing_key.serialize_pem())
            .await
            .unwrap();
        let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let exchanges = Arc::new(AtomicUsize::new(0));
        let gateway_calls = calls.clone();
        let issuer_calls = exchanges.clone();
        let app = axum::Router::new()
            .route("/mcp", post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let gateway_calls = gateway_calls.clone();
                async move {
                    gateway_calls.lock().await.push((headers, body));
                    Json(json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"ok"}]}}))
                }
            }))
            .route("/oauth2/test/token", post(move |body: String| {
                let issuer_calls = issuer_calls.clone();
                async move {
                    let form: std::collections::HashMap<String, String> = url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    assert_eq!(form.get("grant_type").map(String::as_str), Some("urn:ietf:params:oauth:grant-type:token-exchange"));
                    assert!(form.contains_key("workflow_binding_id"));
                    issuer_calls.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"access_token":"exchanged-user-token","token_type":"Bearer","expires_in":300,"scope":"portal.r portal.w"}))
                }
            }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&cert, &key)
            .await
            .unwrap();
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await
                .unwrap();
        });
        (
            directory,
            format!("https://localhost:{}", address.port()),
            calls,
            exchanges,
            task,
        )
    }

    async fn clear_owned_long_identity() {
        let admin = PgPool::connect(&std::env::var("ADMIN_DATABASE_URL").unwrap())
            .await
            .unwrap();
        sqlx::query("DELETE FROM workflow_ops.workflow_long_identity_t WHERE singleton AND NOT EXISTS (SELECT 1 FROM workflow_ops.workflow_long_credential_t)")
            .execute(&admin).await.unwrap();
        admin.close().await;
    }

    const TEST_KEY: &[u8] = b"step12-invoke-signing-key-32-bytes";
    const DEFINITION: &str = "document: {dsl: '1.0.3', namespace: step12, name: invoke, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - prepare:\n      set: {status: ok}\n      end: true\n";

    fn signed(claims: Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("step12".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(TEST_KEY),
        )
        .unwrap()
    }

    pub(crate) fn headers(host: Uuid, user: Uuid, purpose: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let claims = if purpose == "app" {
            json!({"iss":"step12","aud":"workflow","exp":4102444800u64,
                "token_use":"app","client_id":"portal-ui","sub":user,"host":host})
        } else {
            json!({"iss":"step12","aud":"workflow","exp":4102444800u64,
                "token_use":"user","client_id":"portal-ui","uid":user,
                "user_id":user,"sub":user,"host":host,"roles":["user"]})
        };
        let scope = json!({"iss":"step12","aud":"workflow","exp":4102444800u64,
            "sid":"gateway-a","host":host,"env":"dev"});
        headers.insert(
            "authorization",
            format!("Bearer {}", signed(claims)).parse().unwrap(),
        );
        headers.insert(
            "x-scope-token",
            format!("Bearer {}", signed(scope)).parse().unwrap(),
        );
        headers
    }

    fn issuer_role_headers(host: Uuid, user: Uuid, role: Option<&str>) -> HeaderMap {
        let mut headers = headers(host, user, "user");
        let mut claims = json!({"iss":"step12","aud":"workflow","exp":4102444800u64,
            "token_use":"user","client_id":"portal-ui","uid":user,
            "user_id":user,"sub":user,"host":host});
        if let Some(role) = role {
            claims["role"] = json!(role);
        }
        headers.insert(
            "authorization",
            format!("Bearer {}", signed(claims)).parse().unwrap(),
        );
        headers
    }

    pub(crate) struct Fixture {
        pub(crate) state: RuleApiState,
        pub(crate) pool: PgPool,
        pub(crate) host: Uuid,
        tool: Uuid,
        pub(crate) user: Uuid,
        pub(crate) binding: Uuid,
        definition_digest: String,
        binding_digest: String,
        pub(crate) keyring: std::path::PathBuf,
    }

    async fn insert_binding(
        pool: &PgPool,
        host: Uuid,
        wf: Uuid,
        tool: Uuid,
        binding: Uuid,
        source: Uuid,
        definition_digest: &str,
        binding_digest: &str,
        policy: Value,
        limits: Value,
    ) {
        let digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,active,revision_status,source_binding_id,binding_digest,approval_digest,requested_by,requested_ts,caller_policy,admission_limits,tool_name) VALUES($1,$2,$3,$4,'1.0.0',$5,$6,'sync',1000,30000,'interactive','compact-json',$7,$8,$6,$9,$6,true,'approved',$10,$11,$11,'invoke-test',CURRENT_TIMESTAMP,$12,$13,'invoke-test')")
            .bind(host).bind(binding).bind(tool).bind(wf).bind(definition_digest).bind(&digest)
            .bind(json!({"kind":"derived","resultReplayMs":0}))
            .bind(json!({"maximumDelegationDepth":1}))
            .bind(json!({"maximumTaskAttempts":8,"maximumNestedCalls":8,"maximumParallelism":1,
                "maximumRequestBytes":1048576,"maximumIntermediateBytes":4194304,
                "maximumResultBytes":1048576,"maximumCostUnits":1000}))
            .bind(source).bind(binding_digest).bind(policy).bind(limits)
            .execute(pool).await.unwrap();
    }

    pub(crate) async fn fixture() -> Fixture {
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL is required");
        std::env::var("ADMIN_DATABASE_URL").expect("ADMIN_DATABASE_URL is required");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops, operational_meta")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let host = Uuid::new_v4();
        let wf = Uuid::new_v4();
        let tool = Uuid::new_v4();
        let user = Uuid::new_v4();
        let binding = Uuid::new_v4();
        let definition_digest = crate::publication_api::definition_digest(DEFINITION).unwrap();
        let schema_digest = format!("sha256:{}", "a".repeat(64));
        let binding_digest = format!("sha256:{}", "b".repeat(64));
        sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,active) VALUES($1,$2,'step12','invoke','1.0.0',$3,'PUBLISHED',true)")
            .bind(host).bind(wf).bind(DEFINITION).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO wf_definition_version_t(host_id,wf_def_id,version,definition,definition_digest,schema_digest,published_by) VALUES($1,$2,'1.0.0',$3,$4,$5,'invoke-test')")
            .bind(host).bind(wf).bind(DEFINITION).bind(&definition_digest).bind(&schema_digest)
            .execute(&pool).await.unwrap();
        insert_binding(
            &pool,
            host,
            wf,
            tool,
            binding,
            binding,
            &definition_digest,
            &binding_digest,
            json!({}),
            json!({"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,
                "startsPerMinute":120,"startsPerMinutePerUser":10}),
        )
        .await;
        sqlx::query("INSERT INTO workflow_tool_publication_t(host_id,tool_id,active_binding_id,aggregate_version) VALUES($1,$2,$3,1)")
            .bind(host).bind(tool).bind(binding).execute(&pool).await.unwrap();
        let keyring =
            std::env::temp_dir().join(format!("workflow-invoke-handler-{}.json", Uuid::new_v4()));
        tokio::fs::write(&keyring,r#"{"activeKeyId":"test","keys":{"test":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#).await.unwrap();
        let vault = Arc::new(
            crate::run_credential::RunCredentialVault::load(
                std::path::Path::new("/"),
                Some(&keyring),
            )
            .await
            .unwrap()
            .unwrap(),
        );
        let security =
            light_security::SecurityRuntime::with_test_hs256_key("step12", TEST_KEY).await;
        let state = RuleApiState::for_invoke_test(security, host, pool.clone(), vault);
        Fixture {
            state,
            pool,
            host,
            tool,
            user,
            binding,
            definition_digest,
            binding_digest,
            keyring,
        }
    }

    pub(crate) fn arguments(f: &Fixture) -> Value {
        json!({"stableToolRef":f.tool,
        "expectedBindingDigest":f.binding_digest,"expectedDefinitionDigest":f.definition_digest,
        "input":{}})
    }

    pub(crate) async fn running_run(result: Result<Value, ApiError>) -> Uuid {
        let error = result.expect_err("unexecuted Invoke must return a bounded timeout");
        let response = error.into_response();
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["code"], "WORKFLOW_TIMEOUT", "{body:?}");
        assert_eq!(body["retryable"], true);
        serde_json::from_value(body["workflowInstanceId"].clone()).unwrap()
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn invoke_token_and_bound_mcp_authority_ignore_global_long_configuration() {
        use crate::{bound_mcp, run_authority::PerRunAuthority, run_token::RunTokenSelector};
        use sha2::{Digest, Sha256};
        let f = fixture().await;
        let headers = headers(f.host, f.user, "user");
        let original = headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_owned();
        let run = running_run(invoke(f.state.clone(), headers, arguments(&f)).await).await;
        let process: Uuid = sqlx::query_scalar(
            "SELECT process_id FROM workflow_invocation_t
            WHERE host_id=$1 AND workflow_instance_id=$2",
        )
        .bind(f.host)
        .bind(run)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let agent = Uuid::new_v4();
        let (gateway_dir, gateway_origin, gateway_calls, _, gateway_task) = owned_gateway().await;
        let gateway_url = format!("{gateway_origin}/mcp");
        let nested_tool = Uuid::new_v4();
        let digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query("INSERT INTO workflow_tool_dependency_t(host_id,outer_binding_id,nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target) VALUES($1,$2,$3,'1.0.0',$4,'exact','nested-test','gateway',$4,'active',$5)")
            .bind(f.host).bind(f.binding).bind(nested_tool).bind(&digest)
            .bind(json!({"endpoint":gateway_url,"toolName":"nested-tool"}))
            .execute(&f.pool).await.unwrap();
        let config = bound_mcp::Config {
            gateway_url: gateway_url.clone(),
            service_id: "workflow-test".into(),
            client_identity_file: Default::default(),
            ca_file: "cert.pem".into(),
            scope_token_file: Default::default(),
            maximum_depth: 2,
            request_byte_limit: 1024,
            response_byte_limit: 1024,
            cost_unit_limit: 1,
        };
        for with_long in [false, true] {
            let long = if with_long {
                let config = light_client::config::OAuthWorkflowLongConfig {
                    gateway_url: gateway_origin.clone(),
                    provider_id: "test".into(),
                    client_id: "test-client".into(),
                    client_secret: "test-secret".into(),
                    database_url_file: String::new(),
                    keyring_file: String::new(),
                    ca_file: "cert.pem".into(),
                };
                crate::long_authority::LongAuthority::open(
                    &config,
                    gateway_dir.path(),
                    f.pool.clone(),
                    None,
                    &[],
                )
                .await
                .unwrap()
                .map(Arc::new)
            } else {
                None
            };
            let selector = Arc::new(
                RunTokenSelector::new(
                    f.pool.clone(),
                    f.state.run_credential_vault.clone(),
                    long.clone(),
                    f.state.invocation_security.clone(),
                    60,
                )
                .unwrap(),
            );
            let selected = selector
                .select_run_token(run, f.host, f.user, Utc::now())
                .await
                .unwrap();
            assert_eq!(
                Sha256::digest(selected.as_bytes()),
                Sha256::digest(original.as_bytes())
            );
            let authority = Arc::new(PerRunAuthority::new(
                f.pool.clone(),
                long.clone(),
                selector.clone(),
            ));
            let dispatch = bound_mcp::Runtime::new(
                f.pool.clone(),
                long,
                authority,
                selector,
                "Bearer test-scope".into(),
                &config,
                gateway_dir.path(),
            )
            .await
            .unwrap()
            .with_agent_services(std::collections::BTreeMap::from([(
                "test-agent".into(),
                agent,
            )]));
            assert!(
                dispatch
                    .authorize_agent(f.host, process, agent)
                    .await
                    .is_ok()
            );
            let result = dispatch
                .call(
                    f.host,
                    process,
                    Uuid::new_v4(),
                    "nested-test",
                    json!({"arguments":{"key":"value"}}),
                )
                .await
                .unwrap();
            assert_eq!(result, json!({"content":[{"type":"text","text":"ok"}]}));
            let recorded = gateway_calls.lock().await;
            let (sent_headers, sent_body) = recorded.last().unwrap();
            assert_eq!(
                sent_headers.get("authorization").unwrap().to_str().unwrap(),
                format!("Bearer {original}")
            );
            assert_eq!(
                sent_headers.get("x-scope-token").unwrap(),
                "Bearer test-scope"
            );
            assert_eq!(sent_body["params"]["name"], "nested-tool");
        }
        let bad = f
            .state
            .run_credential_vault
            .as_ref()
            .unwrap()
            .seal(run, "invalid", 4_102_444_801)
            .unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        bad.refresh_if_later(&mut tx, f.host, run).await.unwrap();
        tx.commit().await.unwrap();
        let selector = RunTokenSelector::new(
            f.pool.clone(),
            f.state.run_credential_vault.clone(),
            None,
            f.state.invocation_security.clone(),
            60,
        )
        .unwrap();
        assert!(
            selector
                .select_run_token(run, f.host, f.user, Utc::now())
                .await
                .is_err()
        );
        clear_owned_long_identity().await;
        tokio::fs::remove_file(&f.keyring).await.unwrap();
        gateway_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn long_token_uses_verified_original_then_exchanges_at_margin() {
        use crate::run_token::RunTokenSelector;
        use sha2::{Digest, Sha256};
        let f = fixture().await;
        clear_owned_long_identity().await;
        let (gateway_dir, gateway_origin, _, exchanges, gateway_task) = owned_gateway().await;
        let headers = headers(f.host, f.user, "user");
        let original = headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap()
            .to_owned();
        let run = running_run(invoke(f.state.clone(), headers, arguments(&f)).await).await;
        let binding = Uuid::new_v4();
        let far_deadline = chrono::DateTime::from_timestamp(4_102_444_800, 0).unwrap();
        sqlx::query(
            "UPDATE workflow_action_authority_t SET credential_kind='long',grant_id=$3,
            deadline=$4 WHERE host_id=$1 AND run_id=$2",
        )
        .bind(f.host)
        .bind(run)
        .bind(binding)
        .bind(far_deadline)
        .execute(&f.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE workflow_invocation_t SET deadline_ts=$3
            WHERE host_id=$1 AND workflow_instance_id=$2",
        )
        .bind(f.host)
        .bind(run)
        .bind(far_deadline)
        .execute(&f.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE process_info_t SET deadline_ts=$3 WHERE host_id=$1
            AND process_id=(SELECT process_id FROM workflow_invocation_t
                WHERE host_id=$1 AND workflow_instance_id=$2)",
        )
        .bind(f.host)
        .bind(run)
        .bind(far_deadline)
        .execute(&f.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO workflow_long_identity_t(singleton,gateway_url,provider_id,client_id)
            VALUES(true,$1,'test','test-client') ON CONFLICT DO NOTHING",
        )
        .bind(&gateway_origin)
        .execute(&f.pool)
        .await
        .unwrap();
        let hash = hex::encode(Sha256::digest(original.as_bytes()));
        sqlx::query(
            "INSERT INTO workflow_long_credential_t
            (binding_id,run_id,host_id,owner_user_id,issuer_client_id,registration_key_sha256,
             subject_token_sha256,state,issuer_version,key_id,token_bytes)
            VALUES($1,$2,$3,$4,'test-client',$5,$6,'ACTIVE',1,'plaintext',$7)",
        )
        .bind(binding)
        .bind(run)
        .bind(f.host)
        .bind(f.user)
        .bind("a".repeat(64))
        .bind(hash)
        .bind(original.as_bytes())
        .execute(&f.pool)
        .await
        .unwrap();
        let config = light_client::config::OAuthWorkflowLongConfig {
            gateway_url: gateway_origin,
            provider_id: "test".into(),
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            database_url_file: String::new(),
            keyring_file: String::new(),
            ca_file: "cert.pem".into(),
        };
        let long = Arc::new(
            crate::long_authority::LongAuthority::open(
                &config,
                gateway_dir.path(),
                f.pool.clone(),
                None,
                &[],
            )
            .await
            .unwrap()
            .unwrap(),
        );
        let selector = RunTokenSelector::new(
            f.pool.clone(),
            f.state.run_credential_vault.clone(),
            Some(long),
            f.state.invocation_security.clone(),
            60,
        )
        .unwrap();
        let selected = selector
            .select_run_token(run, f.host, f.user, Utc::now())
            .await
            .unwrap();
        assert_eq!(
            Sha256::digest(selected.as_bytes()),
            Sha256::digest(original.as_bytes())
        );
        let boundary = chrono::DateTime::from_timestamp(4_102_444_740, 0).unwrap();
        let before = boundary - chrono::Duration::seconds(1);
        assert_eq!(
            selector
                .select_run_token(run, f.host, f.user, before)
                .await
                .unwrap(),
            original
        );
        assert_eq!(exchanges.load(Ordering::SeqCst), 0);
        let exchanged = selector
            .select_run_token(run, f.host, f.user, boundary)
            .await
            .unwrap();
        assert_eq!(exchanged, "exchanged-user-token");
        assert_eq!(exchanges.load(Ordering::SeqCst), 1);
        let admin = PgPool::connect(&std::env::var("ADMIN_DATABASE_URL").unwrap())
            .await
            .unwrap();
        sqlx::query("DELETE FROM workflow_ops.workflow_long_credential_t WHERE run_id=$1")
            .bind(run)
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        clear_owned_long_identity().await;
        tokio::fs::remove_file(&f.keyring).await.unwrap();
        gateway_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn signed_app_rejected_and_signed_user_admitted_by_handler() {
        let f = fixture().await;
        let rejected = invoke(
            f.state.clone(),
            headers(f.host, f.user, "app"),
            arguments(&f),
        )
        .await
        .unwrap_err();
        assert_eq!(rejected.into_response().status(), StatusCode::UNAUTHORIZED);
        let before: i64 =
            sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_t WHERE host_id=$1")
                .bind(f.host)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(before, 0);
        let run = running_run(
            invoke(
                f.state.clone(),
                headers(f.host, f.user, "user"),
                arguments(&f),
            )
            .await,
        )
        .await;
        assert!(!run.is_nil());
        let stored:(Uuid,i64)=sqlx::query_as("SELECT i.binding_id,a.action_limit FROM workflow_invocation_t i JOIN workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE i.host_id=$1")
            .bind(f.host).fetch_one(&f.pool).await.unwrap();
        assert_eq!(stored, (f.binding, 8));
        tokio::fs::remove_file(&f.keyring).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn issuer_role_claim_is_checked_by_signed_invoke_handler() {
        let f = fixture().await;
        sqlx::query("UPDATE workflow_tool_binding_t SET caller_policy=$3 WHERE host_id=$1 AND binding_id=$2")
            .bind(f.host)
            .bind(f.binding)
            .bind(json!({"anyRole":["genai-admin"]}))
            .execute(&f.pool)
            .await
            .unwrap();
        let stored_policy: Value = sqlx::query_scalar(
            "SELECT caller_policy FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(f.host)
        .bind(f.binding)
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(stored_policy, json!({"anyRole":["genai-admin"]}));
        for role in [Some("reader"), None] {
            let rejected = invoke(
                f.state.clone(),
                issuer_role_headers(f.host, f.user, role),
                arguments(&f),
            )
            .await
            .unwrap_err();
            let body = axum::body::to_bytes(rejected.into_response().into_body(), usize::MAX)
                .await
                .unwrap();
            let error: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(error["code"], "WORKFLOW_POLICY_DENIED");
        }
        for role in ["genai-admin", "reader genai-admin workflow-user"] {
            let role_headers = issuer_role_headers(f.host, f.user, Some(role));
            let (identity, _) = authenticate_invoke(&f.state, &role_headers).await.unwrap();
            assert_eq!(identity.caller_claims["role"], role);
            verify_caller_policy(&json!({"anyRole":["genai-admin"]}), &identity.caller_claims)
                .unwrap();
            let mut args = arguments(&f);
            args["input"] = json!({"case": role});
            let run = running_run(invoke(f.state.clone(), role_headers, args).await).await;
            assert!(!run.is_nil());
        }
        tokio::fs::remove_file(&f.keyring).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn binding_activation_between_handler_reads_rejects_checked_revision() {
        let f = fixture().await;
        let pool = f.pool.clone();
        let host = f.host;
        let tool = f.tool;
        let wf: Uuid = sqlx::query_scalar(
            "SELECT wf_def_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(host)
        .bind(f.binding)
        .fetch_one(&pool)
        .await
        .unwrap();
        let old = f.binding;
        let new = Uuid::new_v4();
        let definition = f.definition_digest.clone();
        let switched = async move {
            sqlx::query("UPDATE workflow_tool_binding_t SET active=false,revision_status='superseded' WHERE host_id=$1 AND binding_id=$2")
                .bind(host).bind(old).execute(&pool).await.unwrap();
            insert_binding(&pool,host,wf,tool,new,old,&definition,&format!("sha256:{}","c".repeat(64)),
                json!({"anyRole":["admin"]}),json!({"maximumConcurrentRuns":1,
                    "maximumConcurrentRunsPerUser":1,"startsPerMinute":1,"startsPerMinutePerUser":1})).await;
            sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=$3,aggregate_version=aggregate_version+1 WHERE host_id=$1 AND tool_id=$2")
                .bind(host).bind(tool).bind(new).execute(&pool).await.unwrap();
        };
        let rejected = invoke_with_interlude(
            f.state.clone(),
            headers(f.host, f.user, "user"),
            arguments(&f),
            switched,
        )
        .await
        .unwrap_err();
        let body = axum::body::to_bytes(rejected.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "WORKFLOW_DEFINITION_MISMATCH");
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_t WHERE host_id=$1")
                .bind(f.host)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
        tokio::fs::remove_file(&f.keyring).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
    async fn acceptance_lock_rechecks_the_active_revision() {
        let f = fixture().await;
        let vault = f.state.run_credential_vault.as_ref().unwrap().clone();
        let sealed = vault
            .seal(Uuid::new_v4(), "test-token", 4102444800)
            .unwrap();
        let admission = InvokeAdmission::new(
            f.binding,
            f.binding_digest.clone(),
            json!({"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,
                "startsPerMinute":120,"startsPerMinutePerUser":10}),
            vault,
            sealed,
            "test-token",
            4102444800,
        )
        .unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        admission
            .fence_revision(&mut tx, f.host, f.tool)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=NULL WHERE host_id=$1 AND tool_id=$2")
            .bind(f.host).bind(f.tool).execute(&f.pool).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        let error = admission
            .fence_revision(&mut tx, f.host, f.tool)
            .await
            .unwrap_err();
        let body = axum::body::to_bytes(error.into_response().into_body(), usize::MAX)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], "WORKFLOW_DEFINITION_MISMATCH");
        tx.rollback().await.unwrap();
        tokio::fs::remove_file(&f.keyring).await.unwrap();
    }
}
