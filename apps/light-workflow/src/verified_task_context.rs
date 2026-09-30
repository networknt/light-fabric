//! Immutable caller/task projection over existing invocation/action authority.
//! No endpoint registry: Gateway/mcp-router own target routing.
use crate::verified_caller::{Error, VerifiedUser, denied};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
use workflow_action::{Binding, ledger::Ledger};
use workflow_invocation_contract::canonical_sha256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "camelCase", deny_unknown_fields)]
pub enum ToolAuthority {
    NativeDefinition {
        definition_revision: i64,
        grant_set_revision: i64,
        grant_set_digest: String,
        grant_id: Uuid,
        grant_generation: i64,
        tool_version: String,
        lightapi_digest: String,
        environment: String,
    },
    ToolBinding {
        binding_id: Uuid,
        binding_digest: String,
        tool_version: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Context {
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub run_id: Uuid,
    pub definition_id: Uuid,
    pub definition_digest: String,
    pub lease_owner: Uuid,
    pub lease_fence: i64,
    pub alias: String,
    pub tool_ref: Uuid,
    pub contract_digest: String,
    pub business_digest: String,
    pub effective_deadline: DateTime<Utc>,
    pub creator: VerifiedUser,
    pub authority: ToolAuthority,
}

/// Select exactly one task from the accepted process snapshot. Repeated task
/// names are ambiguous and fail closed; business arguments never select pins.
fn task_pin<'a>(value: &'a Value, name: &str, matches: &mut Vec<&'a Value>) {
    match value {
        Value::Array(values) => {
            for v in values {
                task_pin(v, name, matches);
            }
        }
        Value::Object(values) => {
            for (key, v) in values {
                if key == name && v.get("call").and_then(Value::as_str) == Some("mcp") {
                    matches.push(v);
                }
                if key == "do" || key == "fork" || key == "branches" {
                    task_pin(v, name, matches);
                }
            }
        }
        _ => {}
    }
}

/// Caller holds invocation/budget/action/permit locks before entering here.
pub async fn project(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    alias: &str,
    business: &Value,
) -> Result<Context, Error> {
    let row=sqlx::query("SELECT i.workflow_instance_id,i.wf_def_id,i.definition_digest,i.binding_id,i.principal_subject,i.end_user_subject,LEAST(a.deadline,CASE WHEN i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1' THEN COALESCE(p.deadline_ts,a.deadline) ELSE i.deadline_ts END,t.deadline_ts,CASE WHEN v.profile='capture-v1' THEN p.started_ts+interval '600 seconds' ELSE a.deadline END) AS effective_deadline,p.definition_snapshot,p.status_code AS process_status,p.deadline_ts AS process_deadline,t.wf_task_id,t.wf_instance_id,t.active,t.status_code,t.locked,t.lease_owner,t.lease_fencing_token,t.lease_expires_ts,t.deadline_ts,clock_timestamp() AS now,v.creator FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id JOIN workflow_ops.task_info_t t ON t.host_id=p.host_id AND t.process_id=p.process_id JOIN workflow_ops.workflow_verified_invocation_t v ON v.host_id=i.host_id AND v.run_id=i.workflow_instance_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE i.host_id=$1 AND i.process_id=$2 AND t.task_id=$3 FOR SHARE OF p,t,v")
        .bind(host).bind(process).bind(task).fetch_optional(&mut **tx).await?.ok_or_else(denied)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if row.get::<DateTime<Utc>, _>("effective_deadline") <= now {
        return Err(denied());
    }
    let run: Uuid = row.get("workflow_instance_id");
    let creator: VerifiedUser = serde_json::from_value(row.get("creator"))?;
    if !row.get::<bool, _>("active")
        || row.get::<&str, _>("status_code") != "A"
        || row.get::<&str, _>("locked") != "Y"
        || row.get::<&str, _>("process_status") != "A"
        || row.get::<String, _>("wf_instance_id") != run.to_string()
        || row
            .get::<Option<DateTime<Utc>>, _>("lease_expires_ts")
            .is_none_or(|d| d <= now)
        || row
            .get::<Option<DateTime<Utc>>, _>("deadline_ts")
            .is_some_and(|d| d <= now)
        || row
            .get::<Option<DateTime<Utc>>, _>("process_deadline")
            .is_some_and(|d| d <= now)
        || creator.host_id != host
        || creator.user_id.to_string() != row.get::<String, _>("end_user_subject")
        || creator.principal != row.get::<String, _>("principal_subject")
        || creator.purpose != "user"
    {
        return Err(denied());
    }
    let definition_id: Uuid = row.get("wf_def_id");
    let definition_digest: String = row.get("definition_digest");
    let snapshot: Value = row.get("definition_snapshot");
    let mut tasks = Vec::new();
    task_pin(&snapshot, &row.get::<String, _>("wf_task_id"), &mut tasks);
    if tasks.len() != 1 || tasks[0].pointer("/with/tool").and_then(Value::as_str) != Some(alias) {
        return Err(denied());
    }
    let (tool_ref, contract_digest, authority) = match row.get::<Option<Uuid>, _>("binding_id") {
        Some(binding_id) => {
            let pins=sqlx::query("SELECT b.binding_digest,b.definition_digest,d.nested_tool_id,d.nested_tool_version,d.contract_digest FROM workflow_ops.workflow_tool_binding_t b JOIN workflow_ops.workflow_tool_dependency_t d ON d.host_id=b.host_id AND d.outer_binding_id=b.binding_id WHERE b.host_id=$1 AND b.binding_id=$2 AND b.revision_status<>'revoked' AND d.authorization_tool_name=$3 AND d.active AND d.lifecycle_status<>'revoked' FOR SHARE OF b,d")
                .bind(host).bind(binding_id).bind(alias).fetch_all(&mut **tx).await?;
            if pins.len() != 1 || pins[0].get::<String, _>("definition_digest") != definition_digest
            {
                return Err(denied());
            }
            (
                pins[0].get("nested_tool_id"),
                pins[0].get("contract_digest"),
                ToolAuthority::ToolBinding {
                    binding_id,
                    binding_digest: pins[0].get("binding_digest"),
                    tool_version: pins[0].get("nested_tool_version"),
                },
            )
        }
        None => {
            let pin = tasks[0]
                .pointer("/metadata/workflowTool")
                .ok_or_else(denied)?;
            let text = |key: &str| {
                pin.get(key)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or_else(denied)
            };
            let tool: Uuid = text("toolId")?.parse()?;
            let version = text("toolVersion")?;
            let digest = text("lightapiDigest")?;
            let environment = text("environment")?;
            let contract = text("contractDigest")?;
            if !workflow_action::is_digest(contract) || !workflow_action::is_digest(digest) {
                return Err(denied());
            }
            let pins=sqlx::query("SELECT d.source_revision,d.definition,s.source_revision AS grant_revision,s.grant_set_digest,g.grant_id,g.aggregate_version FROM workflow_ops.wf_definition_t d JOIN workflow_ops.workflow_definition_grant_sync_t s ON s.host_id=d.host_id AND s.wf_def_id=d.wf_def_id JOIN workflow_ops.workflow_tool_grant_t g ON g.host_id=d.host_id AND g.wf_def_id=d.wf_def_id WHERE d.host_id=$1 AND d.wf_def_id=$2 AND d.active AND g.active AND g.tool_id=$3 AND g.tool_version=$4 AND g.lightapi_digest=$5 AND $6=ANY(g.allowed_environments) FOR SHARE OF d,s,g")
                .bind(host).bind(definition_id).bind(tool).bind(version).bind(digest).bind(environment).fetch_all(&mut **tx).await?;
            if pins.len() != 1 {
                return Err(denied());
            }
            let current: Value = serde_yaml::from_str(&pins[0].get::<String, _>("definition"))?;
            if canonical_sha256(&current)? != definition_digest {
                return Err(denied());
            }
            (
                tool,
                contract.to_owned(),
                ToolAuthority::NativeDefinition {
                    definition_revision: pins[0].get("source_revision"),
                    grant_set_revision: pins[0].get("grant_revision"),
                    grant_set_digest: pins[0].get("grant_set_digest"),
                    grant_id: pins[0].get("grant_id"),
                    grant_generation: pins[0].get("aggregate_version"),
                    tool_version: version.to_owned(),
                    lightapi_digest: digest.to_owned(),
                    environment: environment.to_owned(),
                },
            )
        }
    };
    Ok(Context {
        process_id: process,
        task_id: task,
        run_id: run,
        definition_id,
        definition_digest,
        lease_owner: row
            .get::<Option<Uuid>, _>("lease_owner")
            .ok_or_else(denied)?,
        lease_fence: row.get("lease_fencing_token"),
        alias: alias.to_owned(),
        tool_ref,
        contract_digest,
        business_digest: canonical_sha256(business)?,
        effective_deadline: row.get("effective_deadline"),
        creator,
        authority,
    })
}

pub async fn install(
    tx: &mut Transaction<'_, Postgres>,
    ledger: &Ledger,
    binding: &Binding,
    context: &Context,
) -> Result<(), Error> {
    ledger.install_permit_in(tx, binding, 10).await?;
    let current = project(
        tx,
        binding.host_id,
        context.process_id,
        context.task_id,
        &context.alias,
        &serde_json::json!({}),
    )
    .await?;
    let mut expected = context.clone();
    expected.business_digest = current.business_digest.clone();
    if current != expected
        || binding.run_id != context.run_id
        || binding.attempt_id != context.task_id
        || binding.tool_ref != context.tool_ref
        || binding.contract_digest != context.contract_digest
        || binding.user_id != context.creator.user_id
        || binding.deadline != context.effective_deadline
    {
        return Err(denied());
    }
    sqlx::query("INSERT INTO workflow_ops.workflow_verified_task_context_t(host_id,action_id,run_id,task_id,context) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING")
        .bind(binding.host_id).bind(binding.action_id).bind(binding.run_id).bind(binding.attempt_id).bind(serde_json::to_value(context)?).execute(&mut **tx).await?;
    let stored:Value=sqlx::query_scalar("SELECT context FROM workflow_ops.workflow_verified_task_context_t WHERE host_id=$1 AND action_id=$2")
        .bind(binding.host_id).bind(binding.action_id).fetch_optional(&mut **tx).await?.ok_or_else(denied)?;
    if serde_json::from_value::<Context>(stored)? != *context {
        return Err(denied());
    }
    Ok(())
}

/// Caller commits its effect/idempotency journal in this same transaction.
pub async fn receive(
    tx: &mut Transaction<'_, Postgres>,
    ledger: &Ledger,
    user: &VerifiedUser,
    action: Uuid,
    alias: &str,
    tool_ref: Uuid,
    contract: &str,
    business: &Value,
) -> Result<Context, Error> {
    let binding = ledger.receiver_binding_in(tx, user.host_id, action).await?;
    if binding.user_id != user.user_id
        || binding.claims_digest != user.claims_digest
        || binding.tool_ref != tool_ref
        || binding.contract_digest != contract
    {
        return Err(denied());
    }
    let value:Value=sqlx::query_scalar("SELECT context FROM workflow_ops.workflow_verified_task_context_t WHERE host_id=$1 AND action_id=$2 FOR SHARE")
        .bind(user.host_id).bind(action).fetch_optional(&mut **tx).await?.ok_or_else(denied)?;
    let context: Context = serde_json::from_value(value)?;
    let current = project(
        tx,
        user.host_id,
        context.process_id,
        binding.attempt_id,
        alias,
        business,
    )
    .await?;
    if current != context
        || context.run_id != binding.run_id
        || context.tool_ref != tool_ref
        || context.contract_digest != contract
        || context.creator.user_id != user.user_id
    {
        return Err(denied());
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if now >= context.effective_deadline || now.timestamp() >= user.expires_at {
        return Err(denied());
    }
    Ok(context)
}

/// Native adapters supply their fixed tool identity/contract, never arguments.
/// The returned transaction MUST contain the effect before it is committed.
pub async fn receive_native(
    state: &crate::rule_api::RuleApiState,
    headers: &axum::http::HeaderMap,
    alias: &str,
    tool: Uuid,
    contract: &str,
    business: &Value,
) -> Result<(Transaction<'static, Postgres>, Context, DateTime<Utc>), Error> {
    let single = |name: &str| {
        let values = headers.get_all(name).iter().collect::<Vec<_>>();
        if values.len() != 1 {
            return Err(denied());
        }
        values[0].to_str().map(str::to_owned).map_err(|_| denied())
    };
    let bearer = |value: String| {
        value
            .strip_prefix("Bearer ")
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(denied)
    };
    let scope = light_security::token_purpose::verify_with_purpose(
        &state.invocation_security,
        &bearer(single("x-scope-token")?)?,
        light_security::token_purpose::TokenUse::App,
        &[],
    )
    .await
    .map_err(|_| denied())?;
    let host = scope
        .host
        .as_deref()
        .or_else(|| scope.claims.get("hostId").and_then(Value::as_str))
        .or_else(|| scope.claims.get("host_id").and_then(Value::as_str))
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(denied)?;
    let user = crate::verified_caller::verify_user(
        &state.invocation_security,
        &bearer(single("authorization")?)?,
        host,
    )
    .await?;
    // Shared scope verification also enforces the configured Gateway service
    // and environment. Strict user verification happened before its fallback.
    let (identity, _) = crate::rule_api::authenticate(state, headers)
        .await
        .map_err(|_| denied())?;
    if identity.host_id != host {
        return Err(denied());
    }
    let action: Uuid = single("x-workflow-action")?.parse()?;
    let mut tx = state.pool.begin().await?;
    let context = receive(
        &mut tx,
        &Ledger::new(state.pool.clone()),
        &user,
        action,
        alias,
        tool,
        contract,
        business,
    )
    .await?;
    let scope_exp = scope
        .claims
        .get("exp")
        .and_then(Value::as_i64)
        .and_then(|s| DateTime::from_timestamp(s, 0))
        .ok_or_else(denied)?;
    let user_exp = DateTime::from_timestamp(user.expires_at, 0).ok_or_else(denied)?;
    let lease_exp: DateTime<Utc> = sqlx::query_scalar(
        "SELECT lease_expires_ts FROM workflow_ops.task_info_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(host)
    .bind(context.task_id)
    .fetch_one(&mut *tx)
    .await?;
    let effective = context
        .effective_deadline
        .min(scope_exp)
        .min(user_exp)
        .min(lease_exp);
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    if now >= effective {
        return Err(denied());
    }
    Ok((tx, context, effective))
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct TestGate {
    pub pause: std::sync::atomic::AtomicBool,
    pub pause_after: std::sync::atomic::AtomicBool,
    pub arrived: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[cfg(test)]
pub(crate) async fn test_sink(
    state: &crate::rule_api::RuleApiState,
    headers: &axum::http::HeaderMap,
    alias: &str,
    args: &Value,
) -> Result<Value, Error> {
    if let Some(gate) = state.test_receiver_gate.as_ref()
        && gate.pause.swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    // Test-only, fixed contract: cannot be registered by production builds.
    if args
        .as_object()
        .is_none_or(|o| o.len() != 1 || !o.get("issueUrl").is_some_and(Value::is_string))
    {
        return Err(denied());
    }
    let tool = Uuid::from_u128(0x22222222222242228222222222222222);
    let contract = format!("sha256:{}", "2".repeat(64));
    let (mut tx, context, effective) =
        receive_native(state, headers, alias, tool, &contract, args).await?;
    if let Some(gate) = state.test_receiver_gate.as_ref()
        && gate
            .pause_after
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    sqlx::query("INSERT INTO p02_test_effect_t(host_id,run_id,task_id,creator,business_digest) SELECT $1,$2,$3,$4,$5 WHERE clock_timestamp()<$6 ON CONFLICT DO NOTHING")
        .bind(context.creator.host_id).bind(context.run_id).bind(context.task_id).bind(serde_json::to_value(&context.creator)?).bind(&context.business_digest).bind(effective).execute(&mut *tx).await?;
    let creator:Value=sqlx::query_scalar("SELECT creator FROM p02_test_effect_t WHERE host_id=$1 AND run_id=$2 AND task_id=$3 AND clock_timestamp()<$4")
        .bind(context.creator.host_id).bind(context.run_id).bind(context.task_id).bind(effective).fetch_one(&mut *tx).await?;
    let value = serde_json::json!({"structuredContent":{"creator":creator,"runId":context.run_id,"taskId":context.task_id}});
    // The authenticated native operation journals its transport result under
    // the same effect fence. A producer can recover reply loss without another
    // Gateway dispatch or a second permit consumption.
    let result = serde_json::json!({"resultType":"complete","content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false});
    sqlx::query("INSERT INTO workflow_ops.workflow_verified_task_result_t(host_id,action_id,result) SELECT $1,$2,$3 WHERE clock_timestamp()<$4 ON CONFLICT DO NOTHING")
        .bind(context.creator.host_id).bind(single_action(headers)?).bind(result).bind(effective).execute(&mut *tx).await?;
    let recorded:Value=sqlx::query_scalar("SELECT result FROM workflow_ops.workflow_verified_task_result_t WHERE host_id=$1 AND action_id=$2 AND clock_timestamp()<$3")
        .bind(context.creator.host_id).bind(single_action(headers)?).bind(effective).fetch_one(&mut *tx).await?;
    if recorded["structuredContent"] != value {
        return Err(denied());
    }
    tx.commit().await?;
    Ok(value)
}

#[cfg(test)]
fn single_action(headers: &axum::http::HeaderMap) -> Result<Uuid, Error> {
    Ok(headers
        .get("x-workflow-action")
        .ok_or_else(denied)?
        .to_str()?
        .parse()?)
}
