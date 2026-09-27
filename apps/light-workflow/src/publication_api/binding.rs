//! Immutable Tool binding revisions. The definition version and Tool head are
//! locked in that order; each accepted operation commits its receipt with its
//! revision, children, decisions and head change.

use super::*;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::collections::HashSet;
use workflow_core::models::{
    task::{CallTaskDefinition, TaskDefinition},
    workflow::WorkflowDefinition,
};
use workflow_invocation_contract::{ErrorCode, InvocationBudget, InvocationMode};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Binding {
    source_binding_id: Uuid,
    tool_id: Uuid,
    tool_name: String,
    wf_def_id: Uuid,
    workflow_version: String,
    definition_digest: String,
    schema_digest: String,
    invocation_mode: String,
    sync_wait_ms: i32,
    total_deadline_ms: i32,
    execution_class: String,
    result_text_mode: String,
    cancellation_policy: String,
    idempotency_policy: IdempotencyPolicy,
    delegation_policy: DelegationPolicy,
    runtime_bounds: RuntimeBounds,
    admission_limits: AdmissionLimits,
    caller_policy: CallerPolicy,
    tool_annotations: ToolAnnotations,
    policy_digest: String,
    response_policy_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_schema: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    output_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IdempotencyPolicy {
    kind: String,
    result_replay_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DelegationPolicy {
    maximum_delegation_depth: u16,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeBounds {
    maximum_task_attempts: u32,
    maximum_nested_calls: u32,
    maximum_parallelism: u16,
    maximum_request_bytes: u64,
    maximum_intermediate_bytes: u64,
    maximum_result_bytes: u64,
    maximum_cost_units: u64,
}
impl Binding {
    fn budget(&self) -> InvocationBudget {
        let b = &self.runtime_bounds;
        InvocationBudget {
            maximum_task_attempts: b.maximum_task_attempts,
            maximum_nested_calls: b.maximum_nested_calls,
            maximum_delegation_depth: self.delegation_policy.maximum_delegation_depth,
            maximum_parallelism: b.maximum_parallelism,
            maximum_request_bytes: b.maximum_request_bytes,
            maximum_intermediate_bytes: b.maximum_intermediate_bytes,
            maximum_result_bytes: b.maximum_result_bytes,
            maximum_cost_units: b.maximum_cost_units,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdmissionLimits {
    maximum_concurrent_runs: u32,
    maximum_concurrent_runs_per_user: u32,
    starts_per_minute: u32,
    starts_per_minute_per_user: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CallerPolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    any_role: Option<Vec<String>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ToolAnnotations {
    read_only: bool,
    destructive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Dependency {
    nested_tool_id: Uuid,
    nested_tool_version: String,
    contract_digest: String,
    compatibility_policy: String,
    authorization_tool_name: String,
    authorization_endpoint_key: String,
    authorization_policy_digest: String,
    lifecycle_status: String,
    dispatch_target: Value,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EndpointTarget {
    endpoint_ref: String,
    endpoint_uri: String,
    allowed_methods: Vec<String>,
    authorization_policy_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    resolution_document: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublishInput {
    host_id: Uuid,
    binding: Binding,
    dependencies: Vec<Dependency>,
    endpoint_targets: Vec<EndpointTarget>,
    expected_aggregate_version: i64,
    operation_id: Uuid,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RetireInput {
    host_id: Uuid,
    tool_id: Uuid,
    expected_aggregate_version: i64,
    operation_id: Uuid,
}

fn invalid(message: impl Into<String>) -> ApiError {
    error(
        StatusCode::BAD_REQUEST,
        ErrorCode::WorkflowInputInvalid,
        message,
    )
}

async fn reject_negative_expected_version(
    mut tx: Transaction<'_, Postgres>, host: Uuid, operation: Uuid, tool: &str,
) -> Result<Value, ApiError> {
    let message = "expectedAggregateVersion must be nonnegative";
    let evidence = json!({"version":1,"discriminator":"negativeExpectedAggregateVersion",
        "operationId":operation,"toolName":tool});
    operation_finish(&mut tx, host, operation, &json!({"_requestRejection": {
        "version":1,"discriminator":"negativeExpectedAggregateVersion",
        "operationId":operation,"toolName":tool,"code":"WORKFLOW_INPUT_INVALID",
        "message":message,"evidence":evidence
    }})).await?;
    tx.commit().await.map_err(database_error)?;
    Err(invalid(message).with_details(json!({"requestValidation":evidence})))
}
async fn reject_request_fields(
    mut tx: Transaction<'_, Postgres>, host: Uuid, operation: Uuid,
    discriminator: &str, section: &str, failure: ApiError,
) -> Result<Value, ApiError> {
    let message = failure.message().to_owned();
    let evidence = json!({"version":2,"discriminator":discriminator,
        "operationId":operation,"toolName":"workflow_binding_publish",
        "requestSection":section});
    operation_finish(&mut tx, host, operation, &json!({"_requestRejection": {
        "code":"WORKFLOW_INPUT_INVALID", "message":message, "evidence":evidence
    }})).await?;
    tx.commit().await.map_err(database_error)?;
    Err(failure.with_details(json!({"requestValidation":evidence})))
}
fn limit(name: &str, required: u64, configured: u64) -> ApiError {
    error(
        StatusCode::CONFLICT,
        ErrorCode::WorkflowBindingLimitExceeded,
        format!("{name} requires {required}; configured {configured}"),
    )
}
fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn check_digest(value: &str, name: &str) -> Result<(), ApiError> {
    if valid_digest(value) {
        Ok(())
    } else {
        Err(invalid(format!("{name} is not a canonical SHA-256 digest")))
    }
}
fn bounded_nonblank(value: &str, max: usize, name: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.chars().count() > max {
        Err(invalid(format!(
            "{name} must be nonblank and at most {max} characters"
        )))
    } else {
        Ok(())
    }
}
fn positive(value: u64, name: &str) -> Result<(), ApiError> {
    if value == 0 {
        Err(invalid(format!("{name} must be positive")))
    } else {
        Ok(())
    }
}

fn validate_binding(binding: &Binding) -> Result<(), ApiError> {
    bounded_nonblank(&binding.tool_name, 128, "toolName")?;
    bounded_nonblank(&binding.workflow_version, 64, "workflowVersion")?;
    for (name, value) in [
        ("definitionDigest", &binding.definition_digest),
        ("schemaDigest", &binding.schema_digest),
        ("policyDigest", &binding.policy_digest),
        ("responsePolicyDigest", &binding.response_policy_digest),
    ] {
        check_digest(value, name)?;
    }
    if binding.invocation_mode != "sync"
        || binding.execution_class != "interactive"
        || binding.result_text_mode != "compact-json"
    {
        return Err(invalid(
            "binding requires sync, interactive and compact-json settings",
        ));
    }
    if !(1..=20_000).contains(&binding.sync_wait_ms)
        || binding.total_deadline_ms < binding.sync_wait_ms
        || binding.total_deadline_ms > 30_000
    {
        return Err(invalid(
            "syncWaitMs must be 1..20000 and at most totalDeadlineMs 30000",
        ));
    }
    if !matches!(
        binding.cancellation_policy.as_str(),
        "before-effects-only" | "cooperative" | "disabled"
    ) {
        return Err(invalid("cancellationPolicy is unsupported"));
    }
    if !matches!(
        binding.idempotency_policy.kind.as_str(),
        "derived" | "explicit" | "business"
    ) {
        return Err(invalid("idempotencyPolicy.kind is unsupported"));
    }
    if binding.delegation_policy.maximum_delegation_depth > 8 {
        return Err(invalid("maximumDelegationDepth exceeds 8"));
    }
    let budget = &binding.runtime_bounds;
    for (name, value) in [
        (
            "maximumTaskAttempts",
            u64::from(budget.maximum_task_attempts),
        ),
        ("maximumNestedCalls", u64::from(budget.maximum_nested_calls)),
        ("maximumParallelism", u64::from(budget.maximum_parallelism)),
        ("maximumRequestBytes", budget.maximum_request_bytes),
        (
            "maximumIntermediateBytes",
            budget.maximum_intermediate_bytes,
        ),
        ("maximumResultBytes", budget.maximum_result_bytes),
        ("maximumCostUnits", budget.maximum_cost_units),
    ] {
        positive(value, name)?;
    }
    let a = &binding.admission_limits;
    for (name, value) in [
        ("maximumConcurrentRuns", a.maximum_concurrent_runs),
        (
            "maximumConcurrentRunsPerUser",
            a.maximum_concurrent_runs_per_user,
        ),
        ("startsPerMinute", a.starts_per_minute),
        ("startsPerMinutePerUser", a.starts_per_minute_per_user),
    ] {
        positive(u64::from(value), name)?;
    }
    if a.maximum_concurrent_runs_per_user > a.maximum_concurrent_runs
        || a.starts_per_minute_per_user > a.starts_per_minute
    {
        return Err(invalid("per-user admission limit exceeds its total limit"));
    }
    if let Some(roles) = &binding.caller_policy.any_role {
        if roles.is_empty() || roles.len() > 32 {
            return Err(invalid("callerPolicy.anyRole requires 1..32 roles"));
        }
        let mut seen = HashSet::new();
        for role in roles {
            bounded_nonblank(role, 128, "callerPolicy.anyRole")?;
            if !seen.insert(role) {
                return Err(invalid("duplicate callerPolicy.anyRole"));
            }
        }
    }
    if binding.tool_annotations.read_only && binding.tool_annotations.destructive {
        return Err(invalid("a read-only Tool cannot be destructive"));
    }
    if let Some(schema) = &binding.input_schema {
        if !schema.is_object() {
            return Err(invalid("inputSchema must be an object"));
        }
    }
    if let Some(schema) = &binding.output_schema {
        if !schema.is_object() {
            return Err(invalid("outputSchema must be an object"));
        }
    }
    Ok(())
}

fn normalize_payload(input: &mut PublishInput) -> Result<(), ApiError> {
    if input.dependencies.len() > 256 || input.endpoint_targets.len() > 256 {
        return Err(invalid("binding reach exceeds 256 records"));
    }
    let mut dependency_keys = HashSet::new();
    let mut authorization_names = HashSet::new();
    for dep in &input.dependencies {
        bounded_nonblank(&dep.nested_tool_version, 64, "nestedToolVersion")?;
        bounded_nonblank(&dep.authorization_tool_name, 126, "authorizationToolName")?;
        bounded_nonblank(
            &dep.authorization_endpoint_key,
            255,
            "authorizationEndpointKey",
        )?;
        check_digest(&dep.contract_digest, "contractDigest")?;
        check_digest(
            &dep.authorization_policy_digest,
            "authorizationPolicyDigest",
        )?;
        if !matches!(
            dep.compatibility_policy.as_str(),
            "exact" | "follow-compatible"
        ) || !matches!(
            dep.lifecycle_status.as_str(),
            "active" | "superseded" | "retirement-candidate" | "revoked"
        ) || !dep.dispatch_target.is_object()
        {
            return Err(invalid(
                "dependency policy, lifecycle or dispatchTarget is invalid",
            ));
        }
        if !dependency_keys.insert((dep.nested_tool_id, dep.nested_tool_version.clone()))
            || !authorization_names.insert(dep.authorization_tool_name.clone())
        {
            return Err(invalid("duplicate dependency set key"));
        }
    }
    input.dependencies.sort_by(|a, b| {
        (
            &a.authorization_tool_name,
            a.nested_tool_id,
            &a.nested_tool_version,
        )
            .cmp(&(
                &b.authorization_tool_name,
                b.nested_tool_id,
                &b.nested_tool_version,
            ))
    });
    let mut endpoint_refs = HashSet::new();
    for target in &mut input.endpoint_targets {
        bounded_nonblank(&target.endpoint_ref, 255, "endpointRef")?;
        bounded_nonblank(&target.endpoint_uri, 4096, "endpointUri")?;
        check_digest(
            &target.authorization_policy_digest,
            "authorizationPolicyDigest",
        )?;
        if !endpoint_refs.insert(target.endpoint_ref.clone()) {
            return Err(invalid("duplicate endpointRef"));
        }
        let uri = reqwest::Url::parse(&target.endpoint_uri)
            .map_err(|_| invalid("endpointUri is invalid"))?;
        if !matches!(uri.scheme(), "http" | "https")
            || uri.host_str().is_none()
            || !uri.username().is_empty()
            || uri.password().is_some()
            || target
                .endpoint_uri
                .split("://")
                .nth(1)
                .unwrap_or("")
                .split('/')
                .next()
                .unwrap_or("")
                .contains("${")
        {
            return Err(invalid(
                "endpointUri violates configured destination policy",
            ));
        }
        if target.allowed_methods.is_empty() {
            return Err(invalid("allowedMethods is empty"));
        }
        target.allowed_methods = target
            .allowed_methods
            .iter()
            .map(|method| method.to_ascii_uppercase())
            .collect();
        if target.allowed_methods.iter().any(|method| {
            !matches!(
                method.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE"
            )
        }) {
            return Err(invalid(
                "allowedMethods contains an unsupported HTTP method",
            ));
        }
        target.allowed_methods.sort();
        target.allowed_methods.dedup();
        if let Some(document) = &mut target.resolution_document {
            if !document.is_object() {
                return Err(invalid("resolutionDocument must be an object"));
            }
            if let Some(environments) = document
                .get_mut("environments")
                .and_then(Value::as_array_mut)
            {
                environments.sort_by(|a, b| a.to_string().cmp(&b.to_string()));
            }
        }
    }
    input
        .endpoint_targets
        .sort_by(|a, b| a.endpoint_ref.cmp(&b.endpoint_ref));
    Ok(())
}

fn omit_null_fields(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            for child in map.values_mut() {
                omit_null_fields(child);
            }
        }
        Value::Array(values) => {
            for child in values {
                omit_null_fields(child);
            }
        }
        _ => {}
    }
}
fn digests(input: &PublishInput) -> Result<(String, String), ApiError> {
    let mut binding = serde_json::to_value(&input.binding).map_err(|e| invalid(e.to_string()))?;
    binding.as_object_mut().unwrap().remove("sourceBindingId");
    omit_null_fields(&mut binding);
    let mut reach = json!({"binding":binding,"dependencies":input.dependencies,"endpointTargets":input.endpoint_targets});
    omit_null_fields(&mut reach);
    let binding_digest = digest_value(&reach)?;
    let b = &input.binding;
    let mut approval = json!({"invocationMode":b.invocation_mode,"syncWaitMs":b.sync_wait_ms,
        "totalDeadlineMs":b.total_deadline_ms,"executionClass":b.execution_class,
        "cancellationPolicy":b.cancellation_policy,"idempotencyPolicy":b.idempotency_policy,
        "delegationPolicy":b.delegation_policy,"runtimeBounds":b.runtime_bounds,
        "admissionLimits":b.admission_limits,"callerPolicy":b.caller_policy,
        "toolAnnotations":b.tool_annotations});
    omit_null_fields(&mut approval);
    Ok((binding_digest, digest_value(&approval)?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Effect {
    Read,
    Write,
    Destructive,
    Human,
}
#[derive(Debug, Clone)]
pub(crate) struct TaskEffect {
    pub name: String,
    pub kind: String,
    pub target: String,
    pub effect: Effect,
    pub evidence_digest: Option<String>,
}
fn nested_tool_name(call: &workflow_core::models::task::McpArguments) -> Option<&str> {
    call.tool.as_deref().or_else(|| {
        (call.method.as_deref() == Some("tools/call"))
            .then(|| {
                call.parameters
                    .as_ref()
                    .and_then(|p| p.get("name"))
                    .and_then(Value::as_str)
            })
            .flatten()
    })
}
pub(crate) fn classify_tasks(
    definition: &WorkflowDefinition,
    dependencies: &[Dependency],
) -> Result<Vec<TaskEffect>, ApiError> {
    fn visit(
        prefix: Option<&str>,
        entries: &[std::collections::HashMap<String, TaskDefinition>],
        deps: &[Dependency],
        out: &mut Vec<TaskEffect>,
    ) -> Result<(), ApiError> {
        for entry in entries {
            let (name, task) = entry
                .iter()
                .next()
                .ok_or_else(|| invalid("empty task entry"))?;
            let qualified = prefix.map_or_else(|| name.clone(), |p| format!("{p}::{name}"));
            let (kind, target, effect, metadata) = match task {
                TaskDefinition::Ask(ask) => (
                    "ask".to_string(),
                    String::new(),
                    Effect::Human,
                    ask.common.metadata.as_ref(),
                ),
                TaskDefinition::Call(CallTaskDefinition::Http(call)) => {
                    let method = call.with.method.to_ascii_uppercase();
                    let effect = if matches!(method.as_str(), "GET" | "HEAD") {
                        Effect::Read
                    } else {
                        Effect::Write
                    };
                    ("http".into(), method, effect, call.common.metadata.as_ref())
                }
                TaskDefinition::Call(CallTaskDefinition::Mcp(call)) => {
                    let tool = nested_tool_name(&call.with)
                        .ok_or_else(|| invalid("MCP task has no pinned Tool"))?;
                    let dep = deps
                        .iter()
                        .find(|d| d.authorization_tool_name == tool)
                        .ok_or_else(|| {
                            invalid(format!("nested MCP task {tool} has no dependency"))
                        })?;
                    let t = &dep.dispatch_target;
                    let effect =
                        if t.get("humanApprovalRequired").and_then(Value::as_bool) == Some(true) {
                            Effect::Human
                        } else if t.get("destructive").and_then(Value::as_bool) == Some(true) {
                            Effect::Destructive
                        } else if t.get("readOnly").and_then(Value::as_bool) == Some(true) {
                            Effect::Read
                        } else {
                            Effect::Write
                        };
                    (
                        "mcp".into(),
                        tool.to_owned(),
                        effect,
                        call.common.metadata.as_ref(),
                    )
                }
                TaskDefinition::Call(_) => {
                    return Err(invalid("unsupported call kind in a synchronous Tool"));
                }
                TaskDefinition::Fork(fork) => {
                    out.push(TaskEffect {
                        name: qualified.clone(),
                        kind: "fork".into(),
                        target: String::new(),
                        effect: Effect::Read,
                        evidence_digest: None,
                    });
                    visit(Some(&qualified), &fork.fork.branches.entries, deps, out)?;
                    continue;
                }
                _ => ("local".into(), String::new(), Effect::Read, None),
            };
            let evidence_digest = metadata
                .and_then(|m| m.get("approvalEvidenceDigest"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            out.push(TaskEffect {
                name: qualified,
                kind,
                target,
                effect,
                evidence_digest,
            });
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(None, &definition.do_.entries, dependencies, &mut out)?;
    Ok(out)
}
pub(crate) fn enforce_effect_matrix(
    tasks: &[TaskEffect],
    annotations: &ToolAnnotations,
    replay_ms: u64,
) -> Result<(), ApiError> {
    if !annotations.read_only && replay_ms < 600_000 {
        return Err(invalid("write Tool resultReplayMs must be at least 600000"));
    }
    for task in tasks {
        match task.effect {
            Effect::Human => {
                return Err(invalid(format!(
                    "human task {} is not supported by synchronous Tools",
                    task.name
                )));
            }
            Effect::Destructive if !annotations.destructive => {
                return Err(invalid(format!(
                    "destructive task {} requires a destructive Tool",
                    task.name
                )));
            }
            Effect::Write | Effect::Destructive if annotations.read_only => {
                return Err(invalid(format!(
                    "write task {} is not allowed by a read-only Tool",
                    task.name
                )));
            }
            Effect::Write | Effect::Destructive => {
                let evidence = task.evidence_digest.as_deref().ok_or_else(|| {
                    invalid(format!(
                        "write task {} has no approvalEvidenceDigest",
                        task.name
                    ))
                })?;
                check_digest(evidence, "approvalEvidenceDigest")?;
            }
            Effect::Read => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaticRequirements {
    fork_width: u64,
    attempts: u64,
    nested_calls: u64,
    cost_units: u64,
}
pub(crate) fn static_requirements(
    definition: &WorkflowDefinition,
) -> Result<StaticRequirements, ApiError> {
    fn width(entries: &[std::collections::HashMap<String, TaskDefinition>]) -> u64 {
        entries
            .iter()
            .filter_map(|e| e.values().next())
            .map(|task| match task {
                TaskDefinition::Fork(f) => u64::try_from(f.fork.branches.entries.len())
                    .unwrap_or(u64::MAX)
                    .max(width(&f.fork.branches.entries)),
                _ => 1,
            })
            .max()
            .unwrap_or(1)
    }
    let (attempts, nested_calls, cost_units) = crate::rule_api::budget_envelope(definition, false)?;
    Ok(StaticRequirements {
        fork_width: width(&definition.do_.entries),
        attempts,
        nested_calls,
        cost_units,
    })
}
fn check_static_fit(required: &StaticRequirements, binding: &Binding) -> Result<(), ApiError> {
    let budget = &binding.runtime_bounds;
    for (name, need, have) in [
        (
            "maximumParallelism",
            required.fork_width,
            u64::from(budget.maximum_parallelism),
        ),
        (
            "maximumNestedCalls",
            required.nested_calls,
            u64::from(budget.maximum_nested_calls),
        ),
        (
            "maximumTaskAttempts",
            required.attempts,
            u64::from(budget.maximum_task_attempts),
        ),
        (
            "maximumCostUnits",
            required.cost_units,
            budget.maximum_cost_units,
        ),
    ] {
        if need > have {
            return Err(limit(name, need, have));
        }
    }
    let minimum_ms = required.attempts.saturating_mul(100);
    if minimum_ms > binding.total_deadline_ms as u64 {
        return Err(limit(
            "totalDeadlineMs",
            minimum_ms,
            binding.total_deadline_ms as u64,
        ));
    }
    Ok(())
}

fn validate_payload(
    input: &PublishInput,
    definition_text: &str,
    maximum_parallelism: usize,
) -> Result<Vec<TaskEffect>, ApiError> {
    let definition: WorkflowDefinition = serde_yaml::from_str(definition_text)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let tasks = classify_tasks(&definition, &input.dependencies)?;
    enforce_effect_matrix(
        &tasks,
        &input.binding.tool_annotations,
        input.binding.idempotency_policy.result_replay_ms,
    )?;
    let refs: HashSet<&str> = input
        .endpoint_targets
        .iter()
        .map(|t| t.endpoint_ref.as_str())
        .collect();
    for task in &tasks {
        if task.kind == "http" {
            let task_definition =
                crate::rule_api::find_task_recursive(&definition.do_.entries, &task.name)
                    .ok_or_else(|| invalid("HTTP task disappeared from definition"))?;
            if let TaskDefinition::Call(CallTaskDefinition::Http(call)) = task_definition {
                let metadata = call.common.metadata.as_ref();
                let endpoint = metadata
                    .and_then(|m| m.get("endpointRef"))
                    .and_then(Value::as_str)
                    .or_else(|| {
                        metadata
                            .and_then(|m| m.get("workflowTool"))
                            .and_then(|v| v.get("capabilityRef"))
                            .and_then(Value::as_str)
                    })
                    .ok_or_else(|| {
                        invalid(format!("HTTP task {} has no endpointRef", task.name))
                    })?;
                if !refs.contains(endpoint)
                    || !input.endpoint_targets.iter().any(|t| {
                        t.endpoint_ref == endpoint && t.allowed_methods.contains(&task.target)
                    })
                {
                    return Err(invalid(format!(
                        "HTTP task {} has no matching endpoint target and method",
                        task.name
                    )));
                }
            }
        }
    }
    for task in &tasks {
        if task.kind == "mcp" {
            let dependency = input
                .dependencies
                .iter()
                .find(|d| d.authorization_tool_name == task.target)
                .ok_or_else(|| invalid("MCP dependency missing"))?;
            if !matches!(
                dependency.lifecycle_status.as_str(),
                "active" | "superseded"
            ) {
                return Err(invalid(format!(
                    "nested MCP dependency {} is not active",
                    task.target
                )));
            }
            if dependency
                .dispatch_target
                .get("contractDigest")
                .and_then(Value::as_str)
                .is_some_and(|digest| digest != dependency.contract_digest)
            {
                return Err(invalid(format!(
                    "nested MCP dependency {} contract digest drifted",
                    task.target
                )));
            }
            let nested_depth = dependency
                .dispatch_target
                .get("maximumDelegationDepth")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if nested_depth.saturating_add(1)
                > u64::from(input.binding.delegation_policy.maximum_delegation_depth)
            {
                return Err(limit(
                    "maximumDelegationDepth",
                    nested_depth + 1,
                    u64::from(input.binding.delegation_policy.maximum_delegation_depth),
                ));
            }
        }
    }
    let requirements = static_requirements(&definition)?;
    check_static_fit(&requirements, &input.binding)?;
    crate::rule_api::validate_orchestration_definition(
        &definition,
        InvocationMode::Sync,
        &input.binding.budget(),
        maximum_parallelism,
    )?;
    Ok(tasks)
}

#[derive(Clone)]
struct CarryOverBasis {
    binding_id: Uuid,
    host_id: Uuid,
    tool_id: Uuid,
    wf_def_id: Uuid,
    owner_user: Option<Uuid>,
    owner_position: Option<String>,
    approval_digest: String,
    dependencies: Vec<Dependency>,
    endpoints: Vec<EndpointTarget>,
    tasks: Vec<TaskEffect>,
}

fn write_task_keys(tasks: &[TaskEffect]) -> HashSet<(String, String, String)> {
    tasks
        .iter()
        .filter(|task| matches!(task.effect, Effect::Write | Effect::Destructive))
        .map(|task| (task.name.clone(), task.kind.clone(), task.target.clone()))
        .collect()
}
fn dependency_keys(dependencies: &[Dependency]) -> HashSet<(Uuid, String, String, String)> {
    dependencies
        .iter()
        .map(|dep| {
            (
                dep.nested_tool_id,
                dep.nested_tool_version.clone(),
                dep.contract_digest.clone(),
                dep.authorization_tool_name.clone(),
            )
        })
        .collect()
}
fn endpoint_keys(targets: &[EndpointTarget]) -> HashSet<(String, String, Vec<String>)> {
    targets
        .iter()
        .map(|target| {
            let mut methods = target.allowed_methods.clone();
            methods.sort();
            methods.dedup();
            (
                target.endpoint_ref.clone(),
                target.endpoint_uri.clone(),
                methods,
            )
        })
        .collect()
}

fn carry_over_allowed(
    basis: &CarryOverBasis,
    input: &PublishInput,
    approval_digest: &str,
    tasks: &[TaskEffect],
    requirements: &StaticRequirements,
    owner_user: Option<Uuid>,
    owner_position: Option<&str>,
    version_approval: &str,
) -> Result<(), &'static str> {
    if (basis.host_id, basis.tool_id, basis.wf_def_id)
        != (
            input.host_id,
            input.binding.tool_id,
            input.binding.wf_def_id,
        )
    {
        return Err("active revision belongs to a different host, Tool or definition");
    }
    if basis.owner_user != owner_user || basis.owner_position.as_deref() != owner_position {
        return Err("definition owner changed since active approval");
    }
    if basis.approval_digest != approval_digest {
        return Err("approval policy changed");
    }
    if !dependency_keys(&input.dependencies).is_subset(&dependency_keys(&basis.dependencies))
        || !endpoint_keys(&input.endpoint_targets).is_subset(&endpoint_keys(&basis.endpoints))
    {
        return Err("dependency or endpoint reach expanded");
    }
    if !write_task_keys(tasks).is_subset(&write_task_keys(&basis.tasks)) {
        return Err("write-task set expanded");
    }
    if check_static_fit(requirements, &input.binding).is_err() {
        return Err("new definition exceeds approved static limits");
    }
    if version_approval != "carryOver" {
        return Err("definition version requires reapproval");
    }
    Ok(())
}

async fn carry_over_basis(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    revision: Uuid,
) -> Result<CarryOverBasis, ApiError> {
    let (tool_id,wf_def_id,owner_user,owner_position,approval_digest,definition_text):
        (Uuid,Uuid,Option<Uuid>,Option<String>,String,String) = sqlx::query_as(
        "SELECT b.tool_id,b.wf_def_id,b.owner_user_id,b.owner_position_id,b.approval_digest,v.definition
           FROM workflow_tool_binding_t b JOIN wf_definition_version_t v
             ON v.host_id=b.host_id AND v.wf_def_id=b.wf_def_id AND v.version=b.workflow_version
          WHERE b.host_id=$1 AND b.binding_id=$2 AND b.revision_status='approved'")
        .bind(host).bind(revision).fetch_one(&mut **tx).await.map_err(database_error)?;
    let dep_rows = sqlx::query("SELECT nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target FROM workflow_tool_dependency_t WHERE host_id=$1 AND outer_binding_id=$2")
        .bind(host).bind(revision).fetch_all(&mut **tx).await.map_err(database_error)?;
    let dependencies: Vec<Dependency> = dep_rows
        .into_iter()
        .map(|row| {
            Ok(Dependency {
                nested_tool_id: row.try_get("nested_tool_id").map_err(database_error)?,
                nested_tool_version: row.try_get("nested_tool_version").map_err(database_error)?,
                contract_digest: row.try_get("contract_digest").map_err(database_error)?,
                compatibility_policy: row
                    .try_get("compatibility_policy")
                    .map_err(database_error)?,
                authorization_tool_name: row
                    .try_get("authorization_tool_name")
                    .map_err(database_error)?,
                authorization_endpoint_key: row
                    .try_get("authorization_endpoint_key")
                    .map_err(database_error)?,
                authorization_policy_digest: row
                    .try_get("authorization_policy_digest")
                    .map_err(database_error)?,
                lifecycle_status: row.try_get("lifecycle_status").map_err(database_error)?,
                dispatch_target: row.try_get("dispatch_target").map_err(database_error)?,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let target_rows = sqlx::query("SELECT endpoint_ref,endpoint_uri,allowed_methods,authorization_policy_digest,resolution_document FROM workflow_endpoint_target_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(revision).fetch_all(&mut **tx).await.map_err(database_error)?;
    let endpoints: Vec<EndpointTarget> = target_rows
        .into_iter()
        .map(|row| {
            Ok(EndpointTarget {
                endpoint_ref: row.try_get("endpoint_ref").map_err(database_error)?,
                endpoint_uri: row.try_get("endpoint_uri").map_err(database_error)?,
                allowed_methods: row.try_get("allowed_methods").map_err(database_error)?,
                authorization_policy_digest: row
                    .try_get("authorization_policy_digest")
                    .map_err(database_error)?,
                resolution_document: row.try_get("resolution_document").map_err(database_error)?,
            })
        })
        .collect::<Result<_, ApiError>>()?;
    let definition: WorkflowDefinition = serde_yaml::from_str(&definition_text)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let tasks = classify_tasks(&definition, &dependencies)?;
    Ok(CarryOverBasis {
        binding_id: revision,
        host_id: host,
        tool_id,
        wf_def_id,
        owner_user,
        owner_position,
        approval_digest,
        dependencies,
        endpoints,
        tasks,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublicationStatus {
    Approved,
    PendingApproval,
}
fn publication_status(owner: bool, carry_over: bool) -> PublicationStatus {
    if owner || carry_over {
        PublicationStatus::Approved
    } else {
        PublicationStatus::PendingApproval
    }
}
fn aggregate_conflict(expected: i64, current: i64) -> ApiError {
    crate::rule_api::publication_error(
        StatusCode::CONFLICT,
        ErrorCode::VersionConflict,
        "expectedAggregateVersion does not match Tool publication head",
        Some(json!({"expectedAggregateVersion":expected,"aggregateVersion":current})),
    )
}
async fn head_lock(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    tool: Uuid,
) -> Result<(i64, Option<Uuid>, Option<Uuid>), ApiError> {
    sqlx::query("INSERT INTO workflow_tool_publication_t(host_id,tool_id) VALUES($1,$2) ON CONFLICT(host_id,tool_id) DO NOTHING")
        .bind(host).bind(tool).execute(&mut **tx).await.map_err(database_error)?;
    sqlx::query_as("SELECT aggregate_version,active_binding_id,pending_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2 FOR UPDATE")
        .bind(host).bind(tool).fetch_one(&mut **tx).await.map_err(database_error)
}
async fn decision(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    tool: Uuid,
    binding: Uuid,
    action: &str,
    actor: &str,
    digest: &str,
    operation: Uuid,
) -> Result<Uuid, ApiError> {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO workflow_tool_binding_decision_t(host_id,decision_id,tool_id,binding_id,action,actor,approval_digest,operation_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(host).bind(id).bind(tool).bind(binding).bind(action).bind(actor).bind(digest).bind(operation)
        .execute(&mut **tx).await.map_err(database_error)?;
    Ok(id)
}
async fn supersede(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    tool: Uuid,
    binding: Uuid,
    actor: &str,
    operation: Uuid,
) -> Result<(), ApiError> {
    let digest: String = sqlx::query_scalar(
        "SELECT approval_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(binding)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='superseded',active=false WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(binding).execute(&mut **tx).await.map_err(database_error)?;
    decision(
        tx,
        host,
        tool,
        binding,
        "supersede",
        actor,
        &digest,
        operation,
    )
    .await?;
    Ok(())
}
async fn insert_revision(
    tx: &mut Transaction<'_, Postgres>,
    input: &PublishInput,
    binding_digest: &str,
    approval_digest: &str,
    status: &str,
    actor: &str,
    approved_by: &str,
    owner_user: Option<Uuid>,
    owner_position: &Option<String>,
    aggregate_version: i64,
    tasks: &[TaskEffect],
    carry_over_denied_reason: Option<&str>,
) -> Result<Uuid, ApiError> {
    let b = &input.binding;
    let revision = Uuid::new_v4();
    let active = status == "approved";
    let idempotency =
        serde_json::to_value(&b.idempotency_policy).map_err(|e| invalid(e.to_string()))?;
    let delegation =
        serde_json::to_value(&b.delegation_policy).map_err(|e| invalid(e.to_string()))?;
    let bounds = serde_json::to_value(&b.runtime_bounds).map_err(|e| invalid(e.to_string()))?;
    let admission =
        serde_json::to_value(&b.admission_limits).map_err(|e| invalid(e.to_string()))?;
    let caller = serde_json::to_value(&b.caller_policy).map_err(|e| invalid(e.to_string()))?;
    let annotations =
        serde_json::to_value(&b.tool_annotations).map_err(|e| invalid(e.to_string()))?;
    sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,aggregate_version,active,update_user,update_ts,policy_digest,tool_name,source_binding_id,revision_status,binding_digest,approval_digest,owner_user_id,owner_position_id,requested_by,requested_ts,approved_by,approved_ts,cancellation_policy,admission_limits,caller_policy,tool_annotations,input_schema,output_schema,carry_over_denied_reason) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,CURRENT_TIMESTAMP,$20,$21,$22,$23,$24,$25,$26,$27,$28,CURRENT_TIMESTAMP,$29,CASE WHEN $18 THEN CURRENT_TIMESTAMP ELSE NULL END,$30,$31,$32,$33,$34,$35,$36)")
        .bind(input.host_id).bind(revision).bind(b.tool_id).bind(b.wf_def_id).bind(&b.workflow_version)
        .bind(&b.definition_digest).bind(&b.schema_digest).bind(&b.invocation_mode).bind(b.sync_wait_ms)
        .bind(b.total_deadline_ms).bind(&b.execution_class).bind(&b.result_text_mode).bind(idempotency)
        .bind(delegation).bind(&b.response_policy_digest).bind(bounds).bind(aggregate_version).bind(active)
        .bind(actor).bind(&b.policy_digest).bind(&b.tool_name).bind(b.source_binding_id).bind(status)
        .bind(binding_digest).bind(approval_digest).bind(owner_user).bind(owner_position).bind(actor)
        .bind(if active {Some(approved_by)} else {None}).bind(&b.cancellation_policy).bind(admission).bind(caller)
        .bind(annotations).bind(&b.input_schema).bind(&b.output_schema)
        .bind(carry_over_denied_reason).execute(&mut **tx).await.map_err(database_error)?;
    for dep in &input.dependencies {
        sqlx::query("INSERT INTO workflow_tool_dependency_t(host_id,outer_binding_id,nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target,retention_until,active,update_user,update_ts) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,NULL,true,$12,CURRENT_TIMESTAMP)")
            .bind(input.host_id).bind(revision).bind(dep.nested_tool_id).bind(&dep.nested_tool_version)
            .bind(&dep.contract_digest).bind(&dep.compatibility_policy).bind(&dep.authorization_tool_name)
            .bind(&dep.authorization_endpoint_key).bind(&dep.authorization_policy_digest)
            .bind(&dep.lifecycle_status).bind(&dep.dispatch_target).bind(actor)
            .execute(&mut **tx).await.map_err(database_error)?;
    }
    for target in &input.endpoint_targets {
        sqlx::query("INSERT INTO workflow_endpoint_target_t(host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,authorization_policy_digest,active,update_user,update_ts,resolution_document) VALUES($1,$2,$3,$4,$5,$6,true,$7,CURRENT_TIMESTAMP,$8)")
            .bind(input.host_id).bind(revision).bind(&target.endpoint_ref).bind(&target.endpoint_uri)
            .bind(&target.allowed_methods).bind(&target.authorization_policy_digest).bind(actor)
            .bind(&target.resolution_document).execute(&mut **tx).await.map_err(database_error)?;
    }
    if active {
        write_task_evidence(
            tx,
            input.host_id,
            revision,
            tasks,
            owner_user,
            owner_position,
        )
        .await?;
    }
    Ok(revision)
}
async fn write_task_evidence(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    revision: Uuid,
    tasks: &[TaskEffect],
    owner_user: Option<Uuid>,
    owner_position: &Option<String>,
) -> Result<(), ApiError> {
    let approved_by = owner_user
        .map(|id| id.to_string())
        .or_else(|| owner_position.clone())
        .ok_or_else(|| invalid("definition has no owner for write-task evidence"))?;
    for task in tasks
        .iter()
        .filter(|task| matches!(task.effect, Effect::Write | Effect::Destructive))
    {
        sqlx::query("INSERT INTO workflow_tool_approval_evidence_t(host_id,binding_id,task_name,evidence_digest,approved_by,active) VALUES($1,$2,$3,$4,$5,true)")
            .bind(host).bind(revision).bind(&task.name).bind(task.evidence_digest.as_deref())
            .bind(&approved_by).execute(&mut **tx).await.map_err(database_error)?;
    }
    Ok(())
}

pub(super) async fn publish(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<Value, ApiError> {
    let identity = authenticated_user(state, headers, args, settings).await?;
    let positions = verified_positions(&identity.caller_claims);
    let actor = verified_user_id(&identity)?.to_owned();
    with_matching_host(args, identity.host_id, || {
        publish_binding_verified(
            &state.pool,
            args,
            &actor,
            &positions,
            state.runtime_config.load().config.maximum_parallelism,
        )
    })
    .await
}

/// The caller and publisher assertion must be verified before this store boundary.
pub async fn publish_binding_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
    maximum_parallelism: usize,
) -> Result<Value, ApiError> {
    let mut input: PublishInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    if let Some(receipt) = operation_begin(
        &mut tx,
        input.host_id,
        input.operation_id,
        "workflow_binding_publish",
        args,
    )
    .await?
    {
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if input.expected_aggregate_version < 0 {
        return reject_negative_expected_version(tx, input.host_id, input.operation_id,
            "workflow_binding_publish").await;
    }
    if let Err(failure) = validate_binding(&input.binding) {
        return reject_request_fields(tx, input.host_id, input.operation_id,
            "bindingFields", "binding", failure).await;
    }
    if let Err(failure) = normalize_payload(&mut input) {
        return reject_request_fields(tx, input.host_id, input.operation_id,
            "bindingReach", "reach", failure).await;
    }
    let (binding_digest, approval_digest) = digests(&input)?;
    let b = &input.binding;
    let version:Option<(String,String,String,String,String)>=sqlx::query_as("SELECT definition,definition_digest,schema_digest,version_status,binding_approval FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR SHARE")
        .bind(input.host_id).bind(b.wf_def_id).bind(&b.workflow_version).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (definition_text, stored_digest, stored_schema, status, version_approval) = version
        .ok_or_else(|| {
            ApiError::definition_mismatch("published definition version is unavailable")
        })?;
    if status != "active" {
        return Err(error(
            StatusCode::CONFLICT,
            ErrorCode::WorkflowDefinitionRetired,
            "definition version is retired",
        ));
    }
    if stored_digest != b.definition_digest || stored_schema != b.schema_digest {
        return Err(ApiError::definition_mismatch(
            "binding definition or schema digest does not match its version",
        ));
    }
    let tasks = validate_payload(&input, &definition_text, maximum_parallelism)?;
    let parsed_definition: WorkflowDefinition = serde_yaml::from_str(&definition_text)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let requirements = static_requirements(&parsed_definition)?;
    let (owner_user,owner_position):(Option<Uuid>,Option<String>)=sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2 FOR SHARE")
        .bind(input.host_id).bind(b.wf_def_id).fetch_one(&mut *tx).await.map_err(database_error)?;
    let (current, active, pending) = head_lock(&mut tx, input.host_id, b.tool_id).await?;
    if current != input.expected_aggregate_version {
        return Err(aggregate_conflict(
            input.expected_aggregate_version,
            current,
        ));
    }
    for existing in [active, pending].into_iter().flatten() {
        let row:Option<(String,String,Uuid,String,String,String,Option<String>)>=sqlx::query_as("SELECT binding_digest,revision_status,source_binding_id,workflow_version,definition_digest,approval_digest,carry_over_denied_reason FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
            .bind(input.host_id).bind(existing).fetch_optional(&mut *tx).await.map_err(database_error)?;
        if let Some((digest, revision_status, source, version, definition, approval, denial)) = row
        {
            if digest == binding_digest {
                let status = if revision_status == "approved" {
                    "active"
                } else {
                    "pendingApproval"
                };
                let mut receipt = json!({"result":"unchanged","status":status,"toolId":b.tool_id,"bindingId":existing,
                    "sourceBindingId":source,"workflowVersion":version,"definitionDigest":definition,
                    "bindingDigest":digest,"approvalDigest":approval,"aggregateVersion":current});
                if status == "pendingApproval" {
                    if let Some(reason) = denial {
                        receipt["carryOverDeniedReason"] = json!(reason);
                    }
                }
                operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
                tx.commit().await.map_err(database_error)?;
                return Ok(receipt);
            }
        }
    }
    let owner = is_definition_owner(actor, positions, owner_user, owner_position.as_deref());
    let basis = match active {
        Some(revision) => Some(carry_over_basis(&mut tx, input.host_id, revision).await?),
        None => None,
    };
    let carry_result = basis
        .as_ref()
        .map_or(Err("no active approval to carry over"), |basis| {
            carry_over_allowed(
                basis,
                &input,
                &approval_digest,
                &tasks,
                &requirements,
                owner_user,
                owner_position.as_deref(),
                &version_approval,
            )
        });
    let carry_over = !owner && carry_result.is_ok();
    let status = publication_status(owner, carry_over);
    let carry_over_denied_reason = if status == PublicationStatus::PendingApproval {
        carry_result.as_ref().err().copied()
    } else {
        None
    };
    let approved_by = owner_user
        .map(|id| id.to_string())
        .or_else(|| owner_position.clone())
        .unwrap_or_else(|| actor.to_string());
    let revision_status = if status == PublicationStatus::Approved {
        "approved"
    } else {
        "pendingApproval"
    };
    if let Some(old) = pending {
        supersede(
            &mut tx,
            input.host_id,
            b.tool_id,
            old,
            actor,
            input.operation_id,
        )
        .await?;
    }
    if status == PublicationStatus::Approved {
        if let Some(old) = active {
            supersede(
                &mut tx,
                input.host_id,
                b.tool_id,
                old,
                actor,
                input.operation_id,
            )
            .await?;
        }
    }
    let next = current
        .checked_add(1)
        .ok_or_else(|| invalid("aggregate version overflow"))?;
    let revision = insert_revision(
        &mut tx,
        &input,
        &binding_digest,
        &approval_digest,
        revision_status,
        actor,
        if owner { actor } else { &approved_by },
        owner_user,
        &owner_position,
        next,
        &tasks,
        carry_over_denied_reason,
    )
    .await?;
    let decision_id = if status == PublicationStatus::Approved {
        Some(
            decision(
                &mut tx,
                input.host_id,
                b.tool_id,
                revision,
                if carry_over {
                    "carryOver"
                } else {
                    "selfApprove"
                },
                if carry_over { &approved_by } else { actor },
                &approval_digest,
                input.operation_id,
            )
            .await?,
        )
    } else {
        None
    };
    let new_active = if status == PublicationStatus::Approved {
        Some(revision)
    } else {
        active
    };
    let new_pending = if status == PublicationStatus::Approved {
        None
    } else {
        Some(revision)
    };
    sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=$3,pending_binding_id=$4,aggregate_version=$5,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND tool_id=$2")
        .bind(input.host_id).bind(b.tool_id).bind(new_active).bind(new_pending).bind(next)
        .execute(&mut *tx).await.map_err(database_error)?;
    let mut receipt = json!({"result":"published","status":if status==PublicationStatus::Approved {"active"}else{"pendingApproval"},
        "toolId":b.tool_id,"bindingId":revision,"sourceBindingId":b.source_binding_id,
        "workflowVersion":b.workflow_version,"definitionDigest":b.definition_digest,
        "bindingDigest":binding_digest,"approvalDigest":approval_digest,"aggregateVersion":next});
    if let Some(decision_id) = decision_id {
        receipt["decisionId"] = json!(decision_id);
    }
    if carry_over {
        sqlx::query("UPDATE workflow_tool_binding_t SET approval_basis_id=$3 WHERE host_id=$1 AND binding_id=$2")
            .bind(input.host_id).bind(revision).bind(basis.as_ref().unwrap().binding_id)
            .execute(&mut *tx).await.map_err(database_error)?;
    } else if let Some(reason) = carry_over_denied_reason {
        receipt["carryOverDeniedReason"] = json!(reason);
    }
    operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

pub(super) async fn retire(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
    settings: Option<&ActionSettings>,
) -> Result<Value, ApiError> {
    let identity = authenticated_user(state, headers, args, settings).await?;
    let positions = verified_positions(&identity.caller_claims);
    let actor = verified_user_id(&identity)?.to_owned();
    with_matching_host(args, identity.host_id, || {
        retire_binding_verified(&state.pool, args, &actor, &positions)
    })
    .await
}

/// Retire after user, publisher, Gateway and body Host authentication.
pub async fn retire_binding_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
) -> Result<Value, ApiError> {
    let input: RetireInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    if let Some(receipt) = operation_begin(
        &mut tx,
        input.host_id,
        input.operation_id,
        "workflow_binding_retire",
        args,
    )
    .await?
    {
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if input.expected_aggregate_version < 0 {
        return reject_negative_expected_version(tx, input.host_id, input.operation_id,
            "workflow_binding_retire").await;
    }
    // The preliminary reads take no locks. Lock every version previously used
    // by this Tool in stable order before taking the head lock. This includes
    // both pins when active and pending revisions use different versions.
    // A concurrent publication changes aggregate_version, so the check after
    // the head lock rejects any preliminary snapshot that became stale.
    let pin:Option<(Uuid,String)>=sqlx::query_as("SELECT wf_def_id,workflow_version FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY active DESC,requested_ts DESC NULLS LAST LIMIT 1")
        .bind(input.host_id).bind(input.tool_id).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (wf, _version) = pin.ok_or_else(|| invalid("Tool has no published binding revision"))?;
    let versions:Vec<(Uuid,String)>=sqlx::query_as("SELECT DISTINCT wf_def_id,workflow_version FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY wf_def_id,workflow_version")
        .bind(input.host_id).bind(input.tool_id).fetch_all(&mut *tx).await.map_err(database_error)?;
    for (definition, version) in &versions {
        sqlx::query("SELECT version_status FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR UPDATE")
            .bind(input.host_id).bind(definition).bind(version).fetch_one(&mut *tx).await.map_err(database_error)?;
    }
    let (current, active, pending) = head_lock(&mut tx, input.host_id, input.tool_id).await?;
    if current != input.expected_aggregate_version {
        return Err(aggregate_conflict(
            input.expected_aggregate_version,
            current,
        ));
    }
    let (owner_user,owner_position):(Option<Uuid>,Option<String>)=sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2")
        .bind(input.host_id).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let requester: Option<String> = if let Some(active) = active {
        sqlx::query_scalar(
            "SELECT requested_by FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(input.host_id)
        .bind(active)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
    } else {
        None
    };
    if !is_definition_owner(actor, positions, owner_user, owner_position.as_deref())
        && requester.as_deref() != Some(actor)
    {
        return Err(ApiError::policy_denied(
            "only the definition owner or active requester may retire the Tool",
        ));
    }
    if active.is_none() && pending.is_none() {
        let previous:Option<(Uuid,String,String,Uuid,String,chrono::DateTime<chrono::Utc>)>=sqlx::query_as(
            "SELECT b.binding_id,b.binding_digest,b.revision_status,d.decision_id,d.actor,d.decided_ts
               FROM workflow_tool_binding_t b
               JOIN LATERAL (
                 SELECT decision_id,actor,decided_ts FROM workflow_tool_binding_decision_t d
                  WHERE d.host_id=b.host_id AND d.binding_id=b.binding_id
                    AND ((b.revision_status='retired' AND d.action='retire')
                      OR (b.revision_status='withdrawn' AND d.action='withdraw'))
                  ORDER BY decided_ts DESC,decision_id DESC LIMIT 1
               ) d ON true
              WHERE b.host_id=$1 AND b.tool_id=$2
                AND b.revision_status IN ('retired','withdrawn')
              ORDER BY b.requested_ts DESC,b.binding_id DESC LIMIT 1")
            .bind(input.host_id).bind(input.tool_id).fetch_optional(&mut *tx).await.map_err(database_error)?;
        let (revision, digest, status, decision_id, decided_by, decided_ts) =
            previous.ok_or_else(|| invalid("Tool has no retired binding decision"))?;
        let receipt = json!({"result":"unchanged","toolId":input.tool_id,"bindingId":revision,
            "revisionStatus":status,"bindingDigest":digest,"aggregateVersion":current,
            "decisionId":decision_id,"decidedBy":decided_by,"decidedTs":decided_ts});
        operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    let mut receipt = None;
    for (revision, action, status) in [
        (active, "retire", "retired"),
        (pending, "withdraw", "withdrawn"),
    ] {
        if let Some(revision) = revision {
            let (binding_digest,approval_digest):(String,String)=sqlx::query_as("SELECT binding_digest,approval_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
                .bind(input.host_id).bind(revision).fetch_one(&mut *tx).await.map_err(database_error)?;
            sqlx::query("UPDATE workflow_tool_binding_t SET revision_status=$3,active=false WHERE host_id=$1 AND binding_id=$2")
                .bind(input.host_id).bind(revision).bind(status).execute(&mut *tx).await.map_err(database_error)?;
            let id = decision(
                &mut tx,
                input.host_id,
                input.tool_id,
                revision,
                action,
                actor,
                &approval_digest,
                input.operation_id,
            )
            .await?;
            let decided_ts: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
                "SELECT decided_ts FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND decision_id=$2",
            )
            .bind(input.host_id).bind(id).fetch_one(&mut *tx).await.map_err(database_error)?;
            if receipt.is_none() {
                receipt = Some(
                    json!({"result":"retired","toolId":input.tool_id,"bindingId":revision,
                "revisionStatus":status,"bindingDigest":binding_digest,"aggregateVersion":current+1,
                "decisionId":id,"decidedBy":actor,"decidedTs":decided_ts}),
                );
            }
        }
    }
    sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=NULL,pending_binding_id=NULL,aggregate_version=aggregate_version+1,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND tool_id=$2")
        .bind(input.host_id).bind(input.tool_id).execute(&mut *tx).await.map_err(database_error)?;
    let receipt = receipt.unwrap();
    operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewInput {
    host_id: Uuid,
    binding_id: Uuid,
    expected_binding_digest: String,
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    comment: Option<String>,
    operation_id: Uuid,
}

pub(super) async fn user_identity(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<(String, Vec<String>), ApiError> {
    let (identity, _) = crate::rule_api::authenticate(state, headers).await?;
    assert_body_host(args, identity.host_id)?;
    Ok((
        verified_user_id(&identity)?.to_string(),
        verified_positions(&identity.caller_claims),
    ))
}

fn camel_row(value: Value, columns: &[(&str, &str)]) -> Value {
    let mut source = value.as_object().cloned().unwrap_or_default();
    let mut result = serde_json::Map::new();
    for (db_name, json_name) in columns {
        if let Some(value) = source.remove(*db_name) {
            if !value.is_null() {
                result.insert((*json_name).to_string(), value);
            }
        }
    }
    Value::Object(result)
}

async fn stored_input(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    revision: Uuid,
) -> Result<PublishInput, ApiError> {
    let row: Value = sqlx::query_scalar(
        "SELECT to_jsonb(b) FROM workflow_tool_binding_t b WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(revision)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let binding = camel_row(
        row,
        &[
            ("source_binding_id", "sourceBindingId"),
            ("tool_id", "toolId"),
            ("tool_name", "toolName"),
            ("wf_def_id", "wfDefId"),
            ("workflow_version", "workflowVersion"),
            ("definition_digest", "definitionDigest"),
            ("schema_digest", "schemaDigest"),
            ("invocation_mode", "invocationMode"),
            ("sync_wait_ms", "syncWaitMs"),
            ("total_deadline_ms", "totalDeadlineMs"),
            ("execution_class", "executionClass"),
            ("result_text_mode", "resultTextMode"),
            ("cancellation_policy", "cancellationPolicy"),
            ("idempotency_policy", "idempotencyPolicy"),
            ("delegation_policy", "delegationPolicy"),
            ("runtime_bounds", "runtimeBounds"),
            ("admission_limits", "admissionLimits"),
            ("caller_policy", "callerPolicy"),
            ("tool_annotations", "toolAnnotations"),
            ("policy_digest", "policyDigest"),
            ("response_policy_digest", "responsePolicyDigest"),
            ("input_schema", "inputSchema"),
            ("output_schema", "outputSchema"),
        ],
    );
    let binding: Binding = serde_json::from_value(binding).map_err(|e| invalid(e.to_string()))?;
    let dep_rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(d) FROM workflow_tool_dependency_t d WHERE host_id=$1 AND outer_binding_id=$2 ORDER BY authorization_tool_name,nested_tool_id,nested_tool_version")
        .bind(host).bind(revision).fetch_all(&mut **tx).await.map_err(database_error)?;
    let dependencies = dep_rows
        .into_iter()
        .map(|row| {
            serde_json::from_value(camel_row(
                row,
                &[
                    ("nested_tool_id", "nestedToolId"),
                    ("nested_tool_version", "nestedToolVersion"),
                    ("contract_digest", "contractDigest"),
                    ("compatibility_policy", "compatibilityPolicy"),
                    ("authorization_tool_name", "authorizationToolName"),
                    ("authorization_endpoint_key", "authorizationEndpointKey"),
                    ("authorization_policy_digest", "authorizationPolicyDigest"),
                    ("lifecycle_status", "lifecycleStatus"),
                    ("dispatch_target", "dispatchTarget"),
                ],
            ))
            .map_err(|e| invalid(e.to_string()))
        })
        .collect::<Result<Vec<Dependency>, _>>()?;
    let target_rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(t) FROM workflow_endpoint_target_t t WHERE host_id=$1 AND binding_id=$2 ORDER BY endpoint_ref")
        .bind(host).bind(revision).fetch_all(&mut **tx).await.map_err(database_error)?;
    let endpoint_targets = target_rows
        .into_iter()
        .map(|row| {
            serde_json::from_value(camel_row(
                row,
                &[
                    ("endpoint_ref", "endpointRef"),
                    ("endpoint_uri", "endpointUri"),
                    ("allowed_methods", "allowedMethods"),
                    ("authorization_policy_digest", "authorizationPolicyDigest"),
                    ("resolution_document", "resolutionDocument"),
                ],
            ))
            .map_err(|e| invalid(e.to_string()))
        })
        .collect::<Result<Vec<EndpointTarget>, _>>()?;
    Ok(PublishInput {
        host_id: host,
        binding,
        dependencies,
        endpoint_targets,
        expected_aggregate_version: 0,
        operation_id: Uuid::nil(),
    })
}

async fn review_decision(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    tool: Uuid,
    revision: Uuid,
    action: &str,
    actor: &str,
    comment: Option<&str>,
    approval_digest: &str,
    operation: Uuid,
) -> Result<(Uuid, chrono::DateTime<chrono::Utc>), ApiError> {
    let id = Uuid::new_v4();
    let decided_ts: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "INSERT INTO workflow_tool_binding_decision_t(host_id,decision_id,tool_id,binding_id,action,actor,comment,approval_digest,operation_id)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING decided_ts")
        .bind(host).bind(id).bind(tool).bind(revision).bind(action).bind(actor).bind(comment)
        .bind(approval_digest).bind(operation).fetch_one(&mut **tx).await.map_err(database_error)?;
    Ok((id, decided_ts))
}

pub(super) async fn decide(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, ApiError> {
    let (actor, positions) = user_identity(state, headers, args).await?;
    decide_verified(
        &state.pool,
        args,
        &actor,
        &positions,
        state.runtime_config.load().config.maximum_parallelism,
    )
    .await
}
pub async fn decide_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
    maximum_parallelism: usize,
) -> Result<Value, ApiError> {
    let input: ReviewInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    check_digest(&input.expected_binding_digest, "expectedBindingDigest")?;
    let action = input
        .decision
        .as_deref()
        .ok_or_else(|| invalid("decision is required"))?;
    if !matches!(action, "approve" | "reject") {
        return Err(invalid("decision must be approve or reject"));
    }
    if action == "reject" && input.comment.as_deref().is_none_or(|c| c.trim().is_empty()) {
        return Err(invalid("rejection requires a comment"));
    }
    let mut tx = pool.begin().await.map_err(database_error)?;
    let pin: Option<(Uuid,Uuid,String)> = sqlx::query_as("SELECT tool_id,wf_def_id,workflow_version FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(input.host_id).bind(input.binding_id).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (tool, wf, version) =
        pin.ok_or_else(|| ApiError::definition_mismatch("binding changed since review"))?;
    let (definition_text, version_status):(String,String) = sqlx::query_as("SELECT definition,version_status FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR SHARE")
        .bind(input.host_id).bind(wf).bind(&version).fetch_one(&mut *tx).await.map_err(database_error)?;
    let (owner_user,owner_position):(Option<Uuid>,Option<String>) = sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2 FOR SHARE")
        .bind(input.host_id).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let owner = is_definition_owner(actor, positions, owner_user, owner_position.as_deref());
    let recorded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2)")
        .bind(input.host_id).bind(input.operation_id).fetch_one(&mut *tx).await.map_err(database_error)?;
    if !recorded && !owner {
        return Err(ApiError::policy_denied(
            "only the current definition owner may decide",
        ));
    }
    if let Some(receipt) = operation_begin(
        &mut tx,
        input.host_id,
        input.operation_id,
        "workflow_binding_decide",
        args,
    )
    .await?
    {
        let original_actor: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND operation_id=$2 AND actor=$3)")
            .bind(input.host_id).bind(input.operation_id).bind(actor).fetch_one(&mut *tx).await.map_err(database_error)?;
        if !original_actor {
            return Err(ApiError::policy_denied(
                "only the original decision actor may replay",
            ));
        }
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if !owner {
        return Err(ApiError::policy_denied(
            "only the current definition owner may decide",
        ));
    }
    let (current, active, pending) = head_lock(&mut tx, input.host_id, tool).await?;
    let (digest,status,approval_digest):(String,String,String) = sqlx::query_as(
        "SELECT binding_digest,revision_status,approval_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(input.host_id).bind(input.binding_id).fetch_one(&mut *tx).await.map_err(database_error)?;
    if pending != Some(input.binding_id)
        || status != "pendingApproval"
        || digest != input.expected_binding_digest
    {
        return Err(ApiError::definition_mismatch(
            "binding changed since review",
        ));
    }
    if action == "approve" {
        if version_status != "active" {
            return Err(error(
                StatusCode::CONFLICT,
                ErrorCode::WorkflowDefinitionRetired,
                "definition version is retired",
            ));
        }
        let stored = stored_input(&mut tx, input.host_id, input.binding_id).await?;
        let tasks = validate_payload(&stored, &definition_text, maximum_parallelism)?;
        if let Some(old) = active {
            supersede(&mut tx, input.host_id, tool, old, actor, input.operation_id).await?;
        }
        sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='approved',active=true,approved_by=$3,approved_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND binding_id=$2")
            .bind(input.host_id).bind(input.binding_id).bind(actor).execute(&mut *tx).await.map_err(database_error)?;
        write_task_evidence(
            &mut tx,
            input.host_id,
            input.binding_id,
            &tasks,
            owner_user,
            &owner_position,
        )
        .await?;
    } else {
        sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='rejected',active=false WHERE host_id=$1 AND binding_id=$2")
            .bind(input.host_id).bind(input.binding_id).execute(&mut *tx).await.map_err(database_error)?;
    }
    let (decision_id, decided_ts) = review_decision(
        &mut tx,
        input.host_id,
        tool,
        input.binding_id,
        action,
        actor,
        input.comment.as_deref(),
        &approval_digest,
        input.operation_id,
    )
    .await?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| invalid("aggregate version overflow"))?;
    sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=$3,pending_binding_id=NULL,aggregate_version=$4,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND tool_id=$2")
        .bind(input.host_id).bind(tool).bind(if action=="approve" {Some(input.binding_id)} else {active})
        .bind(next).execute(&mut *tx).await.map_err(database_error)?;
    let receipt = json!({"result":if action=="approve" {"approved"}else{"rejected"},
        "toolId":tool,"bindingId":input.binding_id,"revisionStatus":if action=="approve" {"approved"}else{"rejected"},
        "bindingDigest":digest,"aggregateVersion":next,"decisionId":decision_id,
        "decidedBy":actor,"decidedTs":decided_ts});
    operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

pub(super) async fn revoke(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, ApiError> {
    let (actor, positions) = user_identity(state, headers, args).await?;
    revoke_verified(&state.pool, args, &actor, &positions).await
}
pub async fn revoke_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
) -> Result<Value, ApiError> {
    let input: ReviewInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    check_digest(&input.expected_binding_digest, "expectedBindingDigest")?;
    if input.comment.as_deref().is_none_or(|c| c.trim().is_empty()) {
        return Err(invalid("revocation requires a comment"));
    }
    let mut tx = pool.begin().await.map_err(database_error)?;
    let pin: Option<(Uuid,Uuid,String)> = sqlx::query_as("SELECT tool_id,wf_def_id,workflow_version FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(input.host_id).bind(input.binding_id).fetch_optional(&mut *tx).await.map_err(database_error)?;
    let (tool, wf, version) =
        pin.ok_or_else(|| ApiError::definition_mismatch("binding changed since review"))?;
    sqlx::query("SELECT version_status FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3 FOR SHARE")
        .bind(input.host_id).bind(wf).bind(&version).fetch_one(&mut *tx).await.map_err(database_error)?;
    let (owner_user,owner_position):(Option<Uuid>,Option<String>) = sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2 FOR SHARE")
        .bind(input.host_id).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let owner = is_definition_owner(actor, positions, owner_user, owner_position.as_deref());
    let recorded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2)")
        .bind(input.host_id).bind(input.operation_id).fetch_one(&mut *tx).await.map_err(database_error)?;
    if !recorded && !owner {
        return Err(ApiError::policy_denied(
            "only the current definition owner may revoke",
        ));
    }
    if let Some(receipt) = operation_begin(
        &mut tx,
        input.host_id,
        input.operation_id,
        "workflow_binding_revoke",
        args,
    )
    .await?
    {
        let original_actor: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND operation_id=$2 AND actor=$3)")
            .bind(input.host_id).bind(input.operation_id).bind(actor).fetch_one(&mut *tx).await.map_err(database_error)?;
        if !original_actor {
            return Err(ApiError::policy_denied(
                "only the original decision actor may replay",
            ));
        }
        tx.commit().await.map_err(database_error)?;
        return Ok(receipt);
    }
    if !owner {
        return Err(ApiError::policy_denied(
            "only the current definition owner may revoke",
        ));
    }
    let (current, active, pending) = head_lock(&mut tx, input.host_id, tool).await?;
    let (digest,status,approval_digest):(String,String,String) = sqlx::query_as(
        "SELECT binding_digest,revision_status,approval_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(input.host_id).bind(input.binding_id).fetch_one(&mut *tx).await.map_err(database_error)?;
    if active != Some(input.binding_id)
        || status != "approved"
        || digest != input.expected_binding_digest
    {
        return Err(ApiError::definition_mismatch(
            "binding changed since review",
        ));
    }
    sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='revoked',active=false WHERE host_id=$1 AND binding_id=$2")
        .bind(input.host_id).bind(input.binding_id).execute(&mut *tx).await.map_err(database_error)?;
    let (decision_id, decided_ts) = review_decision(
        &mut tx,
        input.host_id,
        tool,
        input.binding_id,
        "revoke",
        actor,
        input.comment.as_deref(),
        &approval_digest,
        input.operation_id,
    )
    .await?;
    let next = current
        .checked_add(1)
        .ok_or_else(|| invalid("aggregate version overflow"))?;
    sqlx::query("UPDATE workflow_tool_publication_t SET active_binding_id=NULL,pending_binding_id=$3,aggregate_version=$4,updated_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND tool_id=$2")
        .bind(input.host_id).bind(tool).bind(pending).bind(next).execute(&mut *tx).await.map_err(database_error)?;
    let receipt = json!({"result":"revoked","toolId":tool,"bindingId":input.binding_id,
        "revisionStatus":"revoked","bindingDigest":digest,"aggregateVersion":next,
        "decisionId":decision_id,"decidedBy":actor,"decidedTs":decided_ts});
    operation_finish(&mut tx, input.host_id, input.operation_id, &receipt).await?;
    tx.commit().await.map_err(database_error)?;
    Ok(receipt)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GetInput {
    host_id: Uuid,
    #[serde(default)]
    binding_id: Option<Uuid>,
    #[serde(default)]
    tool_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListInput {
    host_id: Uuid,
    #[serde(default)]
    wf_def_id: Option<Uuid>,
    #[serde(default)]
    status: Option<String>,
    role: String,
    limit: i64,
    #[serde(default)]
    cursor: Option<String>,
}

async fn revision_json(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    revision: Uuid,
) -> Result<Value, ApiError> {
    let input = stored_input(tx, host, revision).await?;
    let row = sqlx::query(
        "SELECT revision_status,binding_digest,approval_digest,requested_by,requested_ts,
        approved_by,approved_ts,owner_user_id,owner_position_id,update_ts
        FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(revision)
    .fetch_one(&mut **tx)
    .await
    .map_err(database_error)?;
    let definition_text: String = sqlx::query_scalar("SELECT definition FROM wf_definition_version_t WHERE host_id=$1 AND wf_def_id=$2 AND version=$3")
        .bind(host).bind(input.binding.wf_def_id).bind(&input.binding.workflow_version)
        .fetch_one(&mut **tx).await.map_err(database_error)?;
    let definition: WorkflowDefinition = serde_yaml::from_str(&definition_text)
        .map_err(|e| ApiError::definition_mismatch(e.to_string()))?;
    let tasks = classify_tasks(&definition, &input.dependencies)?;
    let write_tasks: Vec<Value> = tasks
        .iter()
        .filter(|task| matches!(task.effect, Effect::Write | Effect::Destructive))
        .map(|task| {
            json!({"taskName":task.name,"kind":task.kind,"target":task.target,
            "approvalEvidenceDigest":task.evidence_digest})
        })
        .collect();
    let mut owner = serde_json::Map::new();
    let owner_user: Option<Uuid> = row.try_get("owner_user_id").map_err(database_error)?;
    let owner_position: Option<String> =
        row.try_get("owner_position_id").map_err(database_error)?;
    if let Some(value) = owner_user {
        owner.insert("userId".into(), json!(value));
    }
    if let Some(value) = owner_position {
        owner.insert("positionId".into(), json!(value));
    }
    let requested_ts: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("requested_ts").map_err(database_error)?;
    let update_ts: chrono::DateTime<chrono::Utc> =
        row.try_get("update_ts").map_err(database_error)?;
    let mut result = json!({"bindingId":revision,"toolId":input.binding.tool_id,
        "toolName":input.binding.tool_name,"wfDefId":input.binding.wf_def_id,
        "workflowVersion":input.binding.workflow_version,
        "revisionStatus":row.try_get::<String,_>("revision_status").map_err(database_error)?,
        "definitionDigest":input.binding.definition_digest,
        "bindingDigest":row.try_get::<String,_>("binding_digest").map_err(database_error)?,
        "approvalDigest":row.try_get::<String,_>("approval_digest").map_err(database_error)?,
        "requestedBy":row.try_get::<String,_>("requested_by").map_err(database_error)?,
        "requestedTs":requested_ts.unwrap_or(update_ts),"owner":owner,
        "binding":input.binding,"dependencies":input.dependencies,
        "endpointTargets":input.endpoint_targets,"writeTasks":write_tasks});
    let approved_by: Option<String> = row.try_get("approved_by").map_err(database_error)?;
    let approved_ts: Option<chrono::DateTime<chrono::Utc>> =
        row.try_get("approved_ts").map_err(database_error)?;
    if let Some(value) = approved_by {
        result["approvedBy"] = json!(value);
    }
    if let Some(value) = approved_ts {
        result["approvedTs"] = json!(value);
    }
    Ok(result)
}

pub(super) async fn get(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, ApiError> {
    let (actor, positions) = user_identity(state, headers, args).await?;
    get_verified(&state.pool, args, &actor, &positions).await
}
pub async fn get_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
) -> Result<Value, ApiError> {
    let input: GetInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    if input.binding_id.is_some() == input.tool_id.is_some() {
        return Err(invalid("exactly one of bindingId or toolId is required"));
    }
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let revision: Option<Uuid> = if let Some(id) = input.binding_id {
        sqlx::query_scalar(
            "SELECT binding_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(input.host_id)
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(database_error)?
    } else {
        sqlx::query_scalar("SELECT binding_id FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY requested_ts DESC NULLS LAST,binding_id DESC LIMIT 1")
            .bind(input.host_id).bind(input.tool_id.unwrap()).fetch_optional(&mut *tx).await.map_err(database_error)?
    };
    let revision =
        revision.ok_or_else(|| ApiError::definition_mismatch("binding revision is unavailable"))?;
    let (tool, wf): (Uuid, Uuid) = sqlx::query_as(
        "SELECT tool_id,wf_def_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(input.host_id)
    .bind(revision)
    .fetch_one(&mut *tx)
    .await
    .map_err(database_error)?;
    let (owner_user,owner_position):(Option<Uuid>,Option<String>) = sqlx::query_as("SELECT owner_user_id,owner_position_id FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2")
        .bind(input.host_id).bind(wf).fetch_one(&mut *tx).await.map_err(database_error)?;
    let requester: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 AND requested_by=$3)")
        .bind(input.host_id).bind(tool).bind(actor).fetch_one(&mut *tx).await.map_err(database_error)?;
    if !is_definition_owner(actor, positions, owner_user, owner_position.as_deref()) && !requester {
        return Err(ApiError::policy_denied(
            "binding is visible only to its owner or requester",
        ));
    }
    let (aggregate,active):(i64,Option<Uuid>) = sqlx::query_as("SELECT aggregate_version,active_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(input.host_id).bind(tool).fetch_one(&mut *tx).await.map_err(database_error)?;
    let revision_value = revision_json(&mut tx, input.host_id, revision).await?;
    let decision_rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(d) FROM workflow_tool_binding_decision_t d WHERE host_id=$1 AND tool_id=$2 ORDER BY decided_ts,decision_id")
        .bind(input.host_id).bind(tool).fetch_all(&mut *tx).await.map_err(database_error)?;
    let decisions: Vec<Value> = decision_rows
        .into_iter()
        .map(|row| {
            camel_row(
                row,
                &[
                    ("decision_id", "decisionId"),
                    ("binding_id", "bindingId"),
                    ("action", "action"),
                    ("actor", "actor"),
                    ("comment", "comment"),
                    ("approval_digest", "approvalDigest"),
                    ("decided_ts", "decidedTs"),
                    ("operation_id", "operationId"),
                ],
            )
        })
        .collect();
    let mut view =
        json!({"revision":revision_value,"aggregateVersion":aggregate,"decisions":decisions});
    if let Some(active) = active.filter(|active| *active != revision) {
        view["activeRevision"] = revision_json(&mut tx, input.host_id, active).await?;
    }
    if view["revision"]["revisionStatus"] == "pendingApproval" {
        let denial: Option<String> = sqlx::query_scalar("SELECT carry_over_denied_reason FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
            .bind(input.host_id).bind(revision).fetch_one(&mut *tx).await.map_err(database_error)?;
        if let Some(reason) = denial {
            view["carryOverDeniedReason"] = json!(reason);
        }
    }
    tx.commit().await.map_err(database_error)?;
    Ok(view)
}

pub(super) async fn list(
    state: &RuleApiState,
    headers: &HeaderMap,
    args: &Value,
) -> Result<Value, ApiError> {
    let (actor, positions) = user_identity(state, headers, args).await?;
    list_verified(&state.pool, args, &actor, &positions).await
}
pub async fn list_verified(
    pool: &sqlx::PgPool,
    args: &Value,
    actor: &str,
    positions: &[String],
) -> Result<Value, ApiError> {
    let input: ListInput =
        serde_json::from_value(args.clone()).map_err(|e| invalid(e.to_string()))?;
    if !matches!(input.role.as_str(), "owner" | "requester") || !(1..=200).contains(&input.limit) {
        return Err(invalid("role or limit is invalid"));
    }
    let cursor = input
        .cursor
        .as_deref()
        .map(|cursor| {
            let (ts, id) = cursor
                .split_once('|')
                .ok_or_else(|| invalid("invalid cursor"))?;
            let ts = chrono::DateTime::parse_from_rfc3339(ts)
                .map_err(|_| invalid("invalid cursor timestamp"))?
                .with_timezone(&chrono::Utc);
            let id = id
                .parse::<Uuid>()
                .map_err(|_| invalid("invalid cursor binding id"))?;
            Ok::<_, ApiError>((ts, id))
        })
        .transpose()?;
    let mut tx = pool.begin().await.map_err(database_error)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(database_error)?;
    let user_id = actor.parse::<Uuid>().ok();
    let rows = sqlx::query(
        "SELECT b.binding_id,b.tool_id,b.tool_name,b.wf_def_id,b.workflow_version,
        b.revision_status,b.requested_by,b.requested_ts,b.approved_by,b.approved_ts
        FROM workflow_tool_binding_t b
        JOIN wf_definition_t d ON d.host_id=b.host_id AND d.wf_def_id=b.wf_def_id
        WHERE b.host_id=$1 AND b.requested_ts IS NOT NULL
          AND (($2='owner' AND (d.owner_user_id=$3 OR d.owner_position_id=ANY($4)))
            OR ($2='requester' AND b.requested_by=$5))
          AND ($6::uuid IS NULL OR b.wf_def_id=$6)
          AND ($7::text IS NULL OR b.revision_status=$7)
          AND ($8::timestamptz IS NULL OR (b.requested_ts,b.binding_id)<($8,$9))
        ORDER BY b.requested_ts DESC,b.binding_id DESC LIMIT $10",
    )
    .bind(input.host_id)
    .bind(&input.role)
    .bind(user_id)
    .bind(positions)
    .bind(actor)
    .bind(input.wf_def_id)
    .bind(&input.status)
    .bind(cursor.map(|c| c.0))
    .bind(cursor.map(|c| c.1))
    .bind(input.limit + 1)
    .fetch_all(&mut *tx)
    .await
    .map_err(database_error)?;
    let more = rows.len() as i64 > input.limit;
    let mut items = Vec::new();
    for row in rows.into_iter().take(input.limit as usize) {
        let approved_by: Option<String> = row.try_get("approved_by").map_err(database_error)?;
        let approved_ts: Option<chrono::DateTime<chrono::Utc>> =
            row.try_get("approved_ts").map_err(database_error)?;
        let mut item = json!({"bindingId":row.try_get::<Uuid,_>("binding_id").map_err(database_error)?,
            "toolId":row.try_get::<Uuid,_>("tool_id").map_err(database_error)?,
            "toolName":row.try_get::<String,_>("tool_name").map_err(database_error)?,
            "wfDefId":row.try_get::<Uuid,_>("wf_def_id").map_err(database_error)?,
            "workflowVersion":row.try_get::<String,_>("workflow_version").map_err(database_error)?,
            "revisionStatus":row.try_get::<String,_>("revision_status").map_err(database_error)?,
            "requestedBy":row.try_get::<String,_>("requested_by").map_err(database_error)?,
            "requestedTs":row.try_get::<chrono::DateTime<chrono::Utc>,_>("requested_ts").map_err(database_error)?});
        if let Some(value) = approved_by {
            item["approvedBy"] = json!(value);
        }
        if let Some(value) = approved_ts {
            item["approvedTs"] = json!(value);
        }
        items.push(item);
    }
    let mut output = json!({"items":items});
    if more {
        if let Some(last) = items.last() {
            output["nextCursor"] = json!(format!(
                "{}|{}",
                last["requestedTs"].as_str().unwrap(),
                last["bindingId"].as_str().unwrap()
            ));
        }
    }
    if input.role == "owner" && input.wf_def_id.is_none() {
        let counts: Vec<(String,i64)> = sqlx::query_as("SELECT b.revision_status,count(*) FROM workflow_tool_binding_t b JOIN wf_definition_t d ON d.host_id=b.host_id AND d.wf_def_id=b.wf_def_id WHERE b.host_id=$1 AND (d.owner_user_id=$2 OR d.owner_position_id=ANY($3)) AND b.requested_ts IS NOT NULL GROUP BY b.revision_status")
            .bind(input.host_id).bind(user_id).bind(positions).fetch_all(&mut *tx).await.map_err(database_error)?;
        output["counts"] = Value::Object(
            counts
                .into_iter()
                .map(|(status, count)| (status, json!(count)))
                .collect(),
        );
    }
    tx.commit().await.map_err(database_error)?;
    Ok(output)
}

pub async fn pinned_dependencies(
    pool: &sqlx::PgPool,
    host: Uuid,
    binding: Uuid,
) -> Result<Vec<Dependency>, ApiError> {
    let rows=sqlx::query("SELECT nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target FROM workflow_tool_dependency_t WHERE host_id=$1 AND outer_binding_id=$2")
        .bind(host).bind(binding).fetch_all(pool).await.map_err(database_error)?;
    rows.into_iter()
        .map(|row| {
            Ok(Dependency {
                nested_tool_id: row.try_get("nested_tool_id").map_err(database_error)?,
                nested_tool_version: row.try_get("nested_tool_version").map_err(database_error)?,
                contract_digest: row.try_get("contract_digest").map_err(database_error)?,
                compatibility_policy: row
                    .try_get("compatibility_policy")
                    .map_err(database_error)?,
                authorization_tool_name: row
                    .try_get("authorization_tool_name")
                    .map_err(database_error)?,
                authorization_endpoint_key: row
                    .try_get("authorization_endpoint_key")
                    .map_err(database_error)?,
                authorization_policy_digest: row
                    .try_get("authorization_policy_digest")
                    .map_err(database_error)?,
                lifecycle_status: row.try_get("lifecycle_status").map_err(database_error)?,
                dispatch_target: row.try_get("dispatch_target").map_err(database_error)?,
            })
        })
        .collect()
}
pub(crate) async fn pinned_task_effects(
    pool: &sqlx::PgPool,
    host: Uuid,
    binding: Uuid,
    definition: &WorkflowDefinition,
    maximum_depth: u16,
) -> Result<Vec<TaskEffect>, ApiError> {
    let (annotations,policy):(Value,Value)=sqlx::query_as("SELECT tool_annotations,idempotency_policy FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(binding).fetch_one(pool).await.map_err(database_error)?;
    let annotations: ToolAnnotations =
        serde_json::from_value(annotations).map_err(|e| invalid(e.to_string()))?;
    let policy: IdempotencyPolicy =
        serde_json::from_value(policy).map_err(|e| invalid(e.to_string()))?;
    let dependencies = pinned_dependencies(pool, host, binding).await?;
    let tasks = classify_tasks(definition, &dependencies)?;
    enforce_effect_matrix(&tasks, &annotations, policy.result_replay_ms)?;
    for task in tasks.iter().filter(|task| task.kind == "mcp") {
        let dep = dependencies
            .iter()
            .find(|d| d.authorization_tool_name == task.target)
            .ok_or_else(|| ApiError::definition_mismatch("nested MCP dependency is missing"))?;
        if !matches!(dep.lifecycle_status.as_str(), "active" | "superseded") {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP Tool {} is revoked or unavailable",
                task.target
            )));
        }
        if dep
            .dispatch_target
            .get("contractDigest")
            .and_then(Value::as_str)
            .is_some_and(|digest| digest != dep.contract_digest)
        {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP Tool {} contract digest drifted",
                task.target
            )));
        }
        let depth = dep
            .dispatch_target
            .get("maximumDelegationDepth")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if depth.saturating_add(1) > u64::from(maximum_depth) {
            return Err(ApiError::definition_mismatch(format!(
                "nested MCP Tool {} exceeds delegation depth",
                task.target
            )));
        }
    }
    Ok(tasks)
}
pub async fn pinned_evidence(
    pool: &sqlx::PgPool,
    host: Uuid,
    binding: Uuid,
    definition: &WorkflowDefinition,
    maximum_depth: u16,
) -> Result<(), ApiError> {
    let tasks = pinned_task_effects(pool, host, binding, definition, maximum_depth).await?;
    for task in tasks
        .iter()
        .filter(|task| matches!(task.effect, Effect::Write | Effect::Destructive))
    {
        let evidence = task
            .evidence_digest
            .as_deref()
            .ok_or_else(|| ApiError::definition_mismatch("write approval evidence is missing"))?;
        let approved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_tool_approval_evidence_t WHERE host_id=$1 AND binding_id=$2 AND task_name=$3 AND evidence_digest=$4 AND active)")
            .bind(host).bind(binding).bind(&task.name).bind(evidence).fetch_one(pool).await.map_err(database_error)?;
        if !approved {
            return Err(ApiError::definition_mismatch(format!(
                "write task {} has no active publication approval evidence",
                task.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn example() -> PublishInput {
        let examples: Value =
            serde_json::from_str(include_str!("../../contracts/workflow-admin/examples.json"))
                .unwrap();
        serde_json::from_value(examples["workflow_binding_publish"]["input"].clone()).unwrap()
    }
    fn task(effect: Effect) -> TaskEffect {
        TaskEffect {
            name: "step".into(),
            kind: "http".into(),
            target: "POST".into(),
            effect,
            evidence_digest: Some(format!("sha256:{}", "a".repeat(64))),
        }
    }
    #[test]
    fn carry_over_checks_every_d13_clause() {
        let input = example();
        let (_, approval_digest) = digests(&input).unwrap();
        let owner = Uuid::new_v4();
        let tasks = vec![task(Effect::Write)];
        let requirements = StaticRequirements {
            fork_width: 1,
            attempts: 1,
            nested_calls: 0,
            cost_units: 1,
        };
        let basis = CarryOverBasis {
            binding_id: Uuid::new_v4(),
            host_id: input.host_id,
            tool_id: input.binding.tool_id,
            wf_def_id: input.binding.wf_def_id,
            owner_user: Some(owner),
            owner_position: None,
            approval_digest: approval_digest.clone(),
            dependencies: input.dependencies.clone(),
            endpoints: input.endpoint_targets.clone(),
            tasks: tasks.clone(),
        };
        let allowed = |basis: &CarryOverBasis,
                       input: &PublishInput,
                       digest: &str,
                       tasks: &[TaskEffect],
                       requirements: &StaticRequirements,
                       approval: &str| {
            carry_over_allowed(
                basis,
                input,
                digest,
                tasks,
                requirements,
                Some(owner),
                None,
                approval,
            )
        };
        assert!(
            allowed(
                &basis,
                &input,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_ok()
        );
        let mut changed = input.clone();
        changed.host_id = Uuid::new_v4();
        assert!(
            allowed(
                &basis,
                &changed,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = input.clone();
        changed.binding.tool_id = Uuid::new_v4();
        assert!(
            allowed(
                &basis,
                &changed,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = input.clone();
        changed.binding.wf_def_id = Uuid::new_v4();
        assert!(
            allowed(
                &basis,
                &changed,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = basis.clone();
        changed.owner_user = Some(Uuid::new_v4());
        assert!(
            allowed(
                &changed,
                &input,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        assert!(
            allowed(
                &basis,
                &input,
                "different",
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = input.clone();
        if let Some(dep) = changed.dependencies.first_mut() {
            dep.contract_digest = format!("sha256:{}", "f".repeat(64));
        } else {
            changed.endpoint_targets.push(EndpointTarget {
                endpoint_ref: "extra".into(),
                endpoint_uri: "https://example.org".into(),
                allowed_methods: vec!["GET".into()],
                authorization_policy_digest: format!("sha256:{}", "f".repeat(64)),
                resolution_document: None,
            });
        }
        assert!(
            allowed(
                &basis,
                &changed,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = input.clone();
        changed.endpoint_targets.push(EndpointTarget {
            endpoint_ref: "expanded".into(),
            endpoint_uri: "https://example.org".into(),
            allowed_methods: vec!["HEAD".into()],
            authorization_policy_digest: format!("sha256:{}", "e".repeat(64)),
            resolution_document: None,
        });
        assert!(
            allowed(
                &basis,
                &changed,
                &approval_digest,
                &tasks,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let mut changed = tasks.clone();
        changed.push(TaskEffect {
            name: "new".into(),
            ..task(Effect::Write)
        });
        assert!(
            allowed(
                &basis,
                &input,
                &approval_digest,
                &changed,
                &requirements,
                "carryOver"
            )
            .is_err()
        );
        let changed = StaticRequirements {
            attempts: u64::MAX,
            ..requirements.clone()
        };
        assert!(
            allowed(
                &basis,
                &input,
                &approval_digest,
                &tasks,
                &changed,
                "carryOver"
            )
            .is_err()
        );
        assert!(
            allowed(
                &basis,
                &input,
                &approval_digest,
                &tasks,
                &requirements,
                "reapprove"
            )
            .is_err()
        );
    }
    #[test]
    fn field_rules_reject_each_invalid_binding_family() {
        let base = example();
        validate_binding(&base.binding).unwrap();
        let mut bad = base.binding.clone();
        bad.tool_name = " ".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.workflow_version = "x".repeat(65);
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.definition_digest = "SHA256:BAD".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.invocation_mode = "async".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.execution_class = "standard".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.result_text_mode = "summary".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.sync_wait_ms = 0;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.total_deadline_ms = 31_000;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.total_deadline_ms = bad.sync_wait_ms - 1;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.cancellation_policy = "BEFORE_EFFECTS_ONLY".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.idempotency_policy.kind = "unknown".into();
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.delegation_policy.maximum_delegation_depth = 9;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.runtime_bounds.maximum_task_attempts = 0;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.admission_limits.maximum_concurrent_runs_per_user = 21;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.admission_limits.starts_per_minute_per_user = 121;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.caller_policy.any_role = Some(vec![" ".into()]);
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.tool_annotations.destructive = true;
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.sync_wait_ms = 20_001;
        assert!(validate_binding(&bad).is_err());
        for field in [
            "maximumNestedCalls",
            "maximumParallelism",
            "maximumRequestBytes",
            "maximumIntermediateBytes",
            "maximumResultBytes",
            "maximumCostUnits",
        ] {
            let mut bad = base.binding.clone();
            match field {
                "maximumNestedCalls" => bad.runtime_bounds.maximum_nested_calls = 0,
                "maximumParallelism" => bad.runtime_bounds.maximum_parallelism = 0,
                "maximumRequestBytes" => bad.runtime_bounds.maximum_request_bytes = 0,
                "maximumIntermediateBytes" => bad.runtime_bounds.maximum_intermediate_bytes = 0,
                "maximumResultBytes" => bad.runtime_bounds.maximum_result_bytes = 0,
                _ => bad.runtime_bounds.maximum_cost_units = 0,
            }
            assert!(validate_binding(&bad).is_err(), "{field}");
        }
        for field in [
            "maximumConcurrentRuns",
            "maximumConcurrentRunsPerUser",
            "startsPerMinute",
            "startsPerMinutePerUser",
        ] {
            let mut bad = base.binding.clone();
            match field {
                "maximumConcurrentRuns" => bad.admission_limits.maximum_concurrent_runs = 0,
                "maximumConcurrentRunsPerUser" => {
                    bad.admission_limits.maximum_concurrent_runs_per_user = 0
                }
                "startsPerMinute" => bad.admission_limits.starts_per_minute = 0,
                _ => bad.admission_limits.starts_per_minute_per_user = 0,
            }
            assert!(validate_binding(&bad).is_err(), "{field}");
        }
        let mut bad = base.binding.clone();
        bad.caller_policy.any_role = Some(vec!["reader".into(), "reader".into()]);
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.input_schema = Some(json!([]));
        assert!(validate_binding(&bad).is_err());
        let mut bad = base.binding.clone();
        bad.output_schema = Some(json!([]));
        assert!(validate_binding(&bad).is_err());
    }
    #[test]
    fn payload_field_rules_reject_invalid_reach() {
        let mut base = example();
        base.endpoint_targets.push(EndpointTarget {
            endpoint_ref: "target-a".into(),
            endpoint_uri: "https://example.invalid/a".into(),
            allowed_methods: vec!["GET".into()],
            authorization_policy_digest: format!("sha256:{}", "a".repeat(64)),
            resolution_document: None,
        });
        let mut bad = base.clone();
        bad.dependencies[0].compatibility_policy = "backwardCompatible".into();
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base.clone();
        bad.dependencies[0].lifecycle_status = "ACTIVE".into();
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base.clone();
        bad.dependencies[0].dispatch_target = json!("not an object");
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base.clone();
        bad.endpoint_targets[0].endpoint_uri = "file:///tmp/a".into();
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base.clone();
        bad.endpoint_targets[0].allowed_methods = vec!["TRACE".into()];
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base.clone();
        bad.endpoint_targets[0].allowed_methods.clear();
        assert!(normalize_payload(&mut bad).is_err());
        let mut bad = base;
        bad.endpoint_targets[0].resolution_document = Some(json!([]));
        assert!(normalize_payload(&mut bad).is_err());
    }
    #[test]
    fn effect_matrix_all_cells_and_evidence() {
        let read_only = ToolAnnotations {
            read_only: true,
            destructive: false,
        };
        let write = ToolAnnotations {
            read_only: false,
            destructive: false,
        };
        let destructive = ToolAnnotations {
            read_only: false,
            destructive: true,
        };
        for (effect, expected) in [
            (Effect::Read, [true, true, true]),
            (Effect::Write, [false, true, true]),
            (Effect::Destructive, [false, false, true]),
            (Effect::Human, [false, false, false]),
        ] {
            for (index, annotations) in [&read_only, &write, &destructive].into_iter().enumerate() {
                assert_eq!(
                    enforce_effect_matrix(&[task(effect.clone())], annotations, 600_000).is_ok(),
                    expected[index],
                    "effect {effect:?} outer Tool {index}"
                );
            }
        }
        assert!(enforce_effect_matrix(&[task(Effect::Write)], &write, 599_999).is_err());
        assert!(enforce_effect_matrix(&[], &write, 599_999).is_err());
        assert!(enforce_effect_matrix(&[task(Effect::Read)], &write, 599_999).is_err());
        assert!(enforce_effect_matrix(&[task(Effect::Read)], &read_only, 0).is_ok());
        let mut missing = task(Effect::Write);
        missing.evidence_digest = None;
        assert!(enforce_effect_matrix(&[missing], &write, 600_000).is_err());
    }
    #[test]
    fn digests_sort_reach_omit_nulls_and_reject_duplicate_keys() {
        let mut a = example();
        normalize_payload(&mut a).unwrap();
        let original = digests(&a).unwrap();
        let mut b = a.clone();
        b.binding.source_binding_id = Uuid::new_v4();
        b.operation_id = Uuid::new_v4();
        b.expected_aggregate_version = 100;
        b.binding.input_schema = None;
        assert_eq!(digests(&b).unwrap(), original);
        b.binding.tool_name.push_str("-edited");
        let changed = digests(&b).unwrap();
        assert_ne!(changed.0, original.0);
        assert_eq!(changed.1, original.1);
        b.dependencies.push(b.dependencies[0].clone());
        assert!(normalize_payload(&mut b).is_err());
        let mut c = a.clone();
        c.endpoint_targets.push(EndpointTarget {
            endpoint_ref: "a".into(),
            endpoint_uri: "https://example.invalid/a".into(),
            allowed_methods: vec!["get".into(), "HEAD".into(), "GET".into()],
            authorization_policy_digest: format!("sha256:{}", "a".repeat(64)),
            resolution_document: None,
        });
        normalize_payload(&mut c).unwrap();
        assert_eq!(c.endpoint_targets[0].allowed_methods, vec!["GET", "HEAD"]);
        let once = digests(&c).unwrap();
        c.endpoint_targets[0].allowed_methods.reverse();
        normalize_payload(&mut c).unwrap();
        assert_eq!(digests(&c).unwrap(), once);
        c.endpoint_targets.push(c.endpoint_targets[0].clone());
        assert!(normalize_payload(&mut c).is_err());
    }
    #[test]
    fn static_requirements_use_budget_envelope_and_name_fit_limit() {
        let definition:WorkflowDefinition=serde_yaml::from_str("document: {dsl: '1.0.3', namespace: step04, name: simple, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - finish:\n      set: {status: done}\n      end: true\n").unwrap();
        let required = static_requirements(&definition).unwrap();
        assert_eq!(required.attempts, 1);
        assert_eq!(required.fork_width, 1);
        let mut binding = example().binding;
        binding.runtime_bounds.maximum_task_attempts = 0;
        let error = check_static_fit(&required, &binding).unwrap_err();
        assert!(format!("{error:?}").contains("maximumTaskAttempts"));
        assert!(format!("{error:?}").contains("requires 1"));
    }
    #[test]
    fn publication_status_uses_owner_or_carry_over() {
        assert_eq!(publication_status(true, false), PublicationStatus::Approved);
        assert_eq!(publication_status(false, true), PublicationStatus::Approved);
        assert_eq!(
            publication_status(false, false),
            PublicationStatus::PendingApproval
        );
    }
}
