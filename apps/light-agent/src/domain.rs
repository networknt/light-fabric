use agent_core::{
    AgentActionAttemptId, AgentSessionId, AgentTurnId, PolicySnapshot, sha256_digest,
};
use agent_materializer::MaterializationManifest;
use agent_runtime_protocol::{
    AgentWorkerExecutionSpec, AttemptBrokerGrant, BrokerOperation, EnterpriseGatewayConfig,
    GatewayAttemptBinding,
};
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;

use crate::agent_config::{
    AgentEdgeRunnerBindingPolicy, AgentModelRatePolicy, AgentQuotaPolicy, AgentServicePoolPolicy,
};
use crate::governed_model::GATEWAY_PROVIDER_ID;

use coding_agent_runtime::{
    CodingAdapterContract, CodingFixtureRequest, CodingTurnSpec, ImmutableRepositoryInput,
};
use execution_client::ExecutionClient;
use execution_runner_protocol::{
    CleanupRequestSubmission, CommandExecutionSpec, ExecutionInputSubmission,
    ExecutionRequirements, ExecutionResultView, HostExposure, IsolationBoundary,
    SchedulingRequestSubmission, canonical_sha256,
};

#[derive(Clone)]
pub struct AgentRepository {
    pool: PgPool,
    authority: Option<Arc<AgentRuntimeAuthority>>,
    execution: Option<Arc<ExecutionClient>>,
}

#[derive(Debug, Clone)]
pub struct AgentRuntimeAuthority {
    pub host_id: Uuid,
    pub agent_def_id: Uuid,
    pub definition_version: i64,
    pub publication_id: Uuid,
    pub content_digest: String,
    pub definition_digest: String,
    pub environment: String,
    pub service_id: String,
    pub instance_id: Uuid,
    pub policy_snapshot_id: Uuid,
    pub policy_version: i64,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub model_provider: String,
    pub model_name: String,
    pub quota_policies: Vec<AgentQuotaPolicy>,
    pub model_rates: Vec<AgentModelRatePolicy>,
    pub service_pools: Vec<AgentServicePoolPolicy>,
    pub edge_runner_bindings: Vec<AgentEdgeRunnerBindingPolicy>,
}

pub struct SessionSpec {
    pub host_id: Uuid,
    pub session_id: AgentSessionId,
    pub principal_id: String,
    pub user_id: Option<Uuid>,
    pub agent_def_id: Uuid,
    pub definition_version: i64,
    pub model_provider: String,
    pub model_name: String,
    pub maximum_active_sessions: u64,
    pub bank_id: Option<Uuid>,
    pub policy: PolicySnapshot,
    pub idle_expires_at: DateTime<Utc>,
    pub maximum_expires_at: DateTime<Utc>,
    pub resume_handle_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedTurn {
    pub turn_id: AgentTurnId,
    pub turn_sequence: i64,
    pub duplicate: bool,
    pub policy_digest: String,
    pub data_boundary_digest: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EdgeActionSpec {
    pub edge_binding_id: Uuid,
    pub action: String,
    pub arguments: Value,
    pub schema_digest: String,
    pub approval_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct CodingAdapterRuntime {
    pub contract: CodingAdapterContract,
    pub qualification: coding_agent_runtime::CodingAdapterQualification,
    pub model: String,
    pub enterprise_gateway: Option<crate::agent_config::CodingGatewayPolicy>,
}

fn validate_edge_arguments(path: &str, schema: &Value, value: &Value) -> Result<()> {
    let schema_object = schema
        .as_object()
        .context("edge action schema must be an object")?;
    const SUPPORTED: &[&str] = &[
        "type",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "enum",
        "minLength",
        "maxLength",
        "minimum",
        "maximum",
        "minItems",
        "maxItems",
    ];
    if let Some(keyword) = schema_object
        .keys()
        .find(|key| !SUPPORTED.contains(&key.as_str()))
    {
        bail!("edge action schema uses unsupported keyword {keyword}")
    }
    if let Some(values) = schema.get("enum").and_then(Value::as_array)
        && !values.contains(value)
    {
        bail!("{path} is not an allowed value")
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        let valid = match kind {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "number" => value.is_number(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "null" => value.is_null(),
            _ => bail!("edge action schema type {kind} is unsupported"),
        };
        if !valid {
            bail!("{path} must be {kind}")
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        if schema.get("additionalProperties").and_then(Value::as_bool) == Some(false) {
            for key in object.keys() {
                if !properties.is_some_and(|p| p.contains_key(key)) {
                    bail!("{path} contains unsupported field {key}")
                }
            }
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) || object.get(key).is_some_and(Value::is_null) {
                    bail!("{path} is missing required field {key}")
                }
            }
        }
        if let Some(properties) = properties {
            for (key, child_schema) in properties {
                if let Some(child) = object.get(key) {
                    validate_edge_arguments(&format!("{path}.{key}"), child_schema, child)?;
                }
            }
        }
    }
    if let Some(items) = schema.get("items")
        && let Some(array) = value.as_array()
    {
        if let Some(min) = schema.get("minItems").and_then(Value::as_u64)
            && array.len() < min as usize
        {
            bail!("{path} contains fewer than {min} items")
        }
        if let Some(max) = schema.get("maxItems").and_then(Value::as_u64)
            && array.len() > max as usize
        {
            bail!("{path} contains more than {max} items")
        }
        for (index, child) in array.iter().enumerate() {
            validate_edge_arguments(&format!("{path}[{index}]"), items, child)?;
        }
    }
    if let Some(text) = value.as_str() {
        if let Some(min) = schema.get("minLength").and_then(Value::as_u64)
            && text.chars().count() < min as usize
        {
            bail!("{path} is shorter than {min} characters")
        }
        if let Some(max) = schema.get("maxLength").and_then(Value::as_u64)
            && text.chars().count() > max as usize
        {
            bail!("{path} is longer than {max} characters")
        }
    }
    if let Some(number) = value.as_f64() {
        if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
            && number < min
        {
            bail!("{path} is below minimum {min}")
        }
        if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
            && number > max
        {
            bail!("{path} exceeds maximum {max}")
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct TurnRuntimeResolution {
    pub host_id: Uuid,
    pub turn_id: AgentTurnId,
    pub session_id: AgentSessionId,
    pub agent_def_id: Uuid,
    pub definition_version: i64,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub product_profile_digest: String,
    pub model_provider: String,
    pub model_name: String,
    pub service_pool_id: Option<Uuid>,
    pub service_pool_compatibility_digest: Option<String>,
}

#[derive(Debug, Clone)]
struct PoolAssignment {
    pool_id: Uuid,
    compatibility_digest: String,
    maximum_concurrency: i32,
}

fn resolve_pool(
    pools: &[AgentServicePoolPolicy],
    host: Uuid,
    boundary: &str,
    profile: &str,
) -> Result<Option<PoolAssignment>> {
    if pools.is_empty() {
        return Ok(None);
    }
    let host_key = host.to_string();
    let candidates = pools
        .iter()
        .filter(|pool| pool.enabled)
        .filter(|pool| {
            let dimensions = pool.compatibility_dimensions.as_object();
            dimensions.is_some_and(|dimensions| {
                dimensions.get("tenant").and_then(Value::as_str) == Some(host_key.as_str())
                    && dimensions.get("dataBoundary").and_then(Value::as_str) == Some(boundary)
                    && dimensions.get("profile").and_then(Value::as_str) == Some(profile)
            })
        })
        .collect::<Vec<_>>();
    let [pool] = candidates.as_slice() else {
        bail!("agent definition must have exactly one live compatible service-pool projection")
    };
    let dimensions = &pool.compatibility_dimensions;
    let object = dimensions
        .as_object()
        .context("service-pool compatibility dimensions must be an object")?;
    for required in [
        "tenant",
        "identity",
        "modelCredential",
        "region",
        "dataBoundary",
        "network",
        "retention",
        "profile",
    ] {
        if object
            .get(required)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            bail!("service-pool compatibility dimension {required} is missing")
        }
    }
    let computed = execution_runner_protocol::canonical_sha256(dimensions)?;
    if pool.compatibility_digest != computed {
        bail!("service-pool compatibility digest mismatch")
    }
    Ok(Some(PoolAssignment {
        pool_id: pool.pool_id,
        compatibility_digest: pool.compatibility_digest.clone(),
        maximum_concurrency: pool.maximum_concurrency,
    }))
}

async fn enforce_quotas(
    tx: &mut Transaction<'_, Postgres>,
    policies: &[AgentQuotaPolicy],
    host: Uuid,
    principal: &str,
    agent: Uuid,
    profile: &str,
    provider: &str,
    pool: Option<Uuid>,
    turn_id: Option<Uuid>,
    session_admission: bool,
    tokens: i64,
    cost: i64,
    cost_authoritative: bool,
) -> Result<()> {
    let keys = [
        ("HOST", host.to_string()),
        ("PRINCIPAL", principal.to_string()),
        ("AGENT", agent.to_string()),
        ("PROFILE", profile.to_string()),
        ("PROVIDER", provider.to_string()),
        ("POOL", pool.map(|v| v.to_string()).unwrap_or_default()),
    ];
    for (kind, key) in keys {
        if key.is_empty() {
            continue;
        }
        for q in policies
            .iter()
            .filter(|policy| policy.enabled && policy.scope_kind == kind && policy.scope_key == key)
        {
            if session_admission {
                if let Some(max) = q.maximum_active_sessions {
                    let active:i64=match kind {"HOST"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND state='ACTIVE'").bind(host).fetch_one(&mut **tx).await?,
                    "PRINCIPAL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND principal_id=$2 AND state='ACTIVE'").bind(host).bind(principal).fetch_one(&mut **tx).await?,
                    "AGENT"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND agent_def_id=$2 AND state='ACTIVE'").bind(host).bind(agent).fetch_one(&mut **tx).await?,
                    "PROFILE"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE s.host_id=$1 AND p.product_profile_digest=$2 AND s.state='ACTIVE'").bind(host).bind(profile).fetch_one(&mut **tx).await?,
                    "PROVIDER"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND model_provider=$2 AND state='ACTIVE'").bind(host).bind(provider).fetch_one(&mut **tx).await?,
                    "POOL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND service_pool_id=$2 AND state='ACTIVE'").bind(host).bind(pool).fetch_one(&mut **tx).await?, _=>0};
                    if active >= i64::from(max) {
                        bail!("agent session quota exceeded for {kind}:{key}")
                    }
                }
            } else {
                if let Some(max) = q.maximum_queued_turns {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state='QUEUED' AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent queued-turn quota exceeded for {kind}:{key}")
                    }
                }
                if let Some(max) = q.maximum_running_turns {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL') AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent running-turn quota exceeded for {kind}:{key}")
                    }
                }
                let token_max = q.token_budget_per_window;
                let cost_max = q.cost_budget_micros_per_window;
                if cost_max.is_some() && !cost_authoritative {
                    bail!(
                        "agent cost quota requires an active authoritative model rate for {provider}"
                    )
                }
                if token_max.is_some() || cost_max.is_some() {
                    let quota = q.quota_id;
                    let window = q.window_seconds;
                    let ok:Option<Uuid>=sqlx::query_scalar("INSERT INTO agent_quota_usage_t(host_id,quota_id,window_start_ts,quota_policy_version,quota_policy_digest,reserved_tokens,reserved_cost_micros) VALUES($1,$2,to_timestamp(floor(extract(epoch FROM now())/$3)*$3),$4,$5,$6,$7) ON CONFLICT(host_id,quota_id,window_start_ts) DO UPDATE SET reserved_tokens=agent_quota_usage_t.reserved_tokens+$6,reserved_cost_micros=agent_quota_usage_t.reserved_cost_micros+$7,updated_ts=now() WHERE agent_quota_usage_t.quota_policy_version=$4 AND agent_quota_usage_t.quota_policy_digest=$5 AND ($8::bigint IS NULL OR agent_quota_usage_t.reserved_tokens+agent_quota_usage_t.consumed_tokens+$6<=$8) AND ($9::bigint IS NULL OR agent_quota_usage_t.reserved_cost_micros+agent_quota_usage_t.consumed_cost_micros+$7<=$9) RETURNING quota_id")
                        .bind(host).bind(quota).bind(window).bind(q.policy_version).bind(&q.policy_digest).bind(tokens).bind(cost).bind(token_max).bind(cost_max).fetch_optional(&mut **tx).await?;
                    if ok.is_none() {
                        bail!("agent token or cost quota exceeded for {kind}:{key}")
                    }
                    let turn_id = turn_id.context("turn quota reservation requires a turn id")?;
                    sqlx::query("INSERT INTO agent_quota_reservation_t(host_id,quota_id,turn_id,window_start_ts,reserved_tokens,reserved_cost_micros) VALUES($1,$2,$3,to_timestamp(floor(extract(epoch FROM now())/$4)*$4),$5,$6)")
                        .bind(host).bind(quota).bind(turn_id).bind(window).bind(tokens).bind(cost).execute(&mut **tx).await?;
                }
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum QuotaSettlement {
    Trusted {
        tokens: i64,
        cost_micros: i64,
        source: &'static str,
        evidence_digest: String,
    },
    ReservationCeiling,
    Release,
}

async fn reconcile_turn_quota_usage(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    turn_id: Uuid,
    settlement: &QuotaSettlement,
) -> Result<()> {
    let reservations = sqlx::query(
        "SELECT quota_id,window_start_ts,reserved_tokens,reserved_cost_micros
         FROM agent_quota_reservation_t
         WHERE host_id=$1 AND turn_id=$2 AND reconciled_ts IS NULL
         FOR UPDATE",
    )
    .bind(host_id)
    .bind(turn_id)
    .fetch_all(&mut **tx)
    .await?;
    for reservation in reservations {
        let quota_id: Uuid = reservation.try_get("quota_id")?;
        let window_start: DateTime<Utc> = reservation.try_get("window_start_ts")?;
        let reserved_tokens: i64 = reservation.try_get("reserved_tokens")?;
        let reserved_cost: i64 = reservation.try_get("reserved_cost_micros")?;
        let (actual_tokens, actual_cost_micros, source, evidence_digest) = match settlement {
            QuotaSettlement::Trusted {
                tokens,
                cost_micros,
                source,
                evidence_digest,
            } => (
                (*tokens).max(0),
                (*cost_micros).max(0),
                *source,
                Some(evidence_digest.as_str()),
            ),
            QuotaSettlement::ReservationCeiling => {
                (reserved_tokens, reserved_cost, "reservation-ceiling", None)
            }
            QuotaSettlement::Release => (0, 0, "released-no-effect", None),
        };
        sqlx::query(
            "UPDATE agent_quota_usage_t SET
               reserved_tokens=GREATEST(0,reserved_tokens-$4),
               reserved_cost_micros=GREATEST(0,reserved_cost_micros-$5),
               consumed_tokens=consumed_tokens+$6,
               consumed_cost_micros=consumed_cost_micros+$7,updated_ts=now()
             WHERE host_id=$1 AND quota_id=$2 AND window_start_ts=$3",
        )
        .bind(host_id)
        .bind(quota_id)
        .bind(window_start)
        .bind(reserved_tokens)
        .bind(reserved_cost)
        .bind(actual_tokens)
        .bind(actual_cost_micros)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE agent_quota_reservation_t SET actual_tokens=$4,
               actual_cost_micros=$5,accounting_source=$6,usage_evidence_digest=$7,
               reconciled_ts=now(),updated_ts=now()
             WHERE host_id=$1 AND quota_id=$2 AND turn_id=$3 AND reconciled_ts IS NULL",
        )
        .bind(host_id)
        .bind(quota_id)
        .bind(turn_id)
        .bind(actual_tokens)
        .bind(actual_cost_micros)
        .bind(source)
        .bind(evidence_digest)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn trusted_runner_quota_settlement(result: &Value) -> Option<QuotaSettlement> {
    let evidence = result.get("evidence")?.as_object()?;
    let tokens = evidence
        .get("trustedBrokerConsumedTokens")?
        .as_str()?
        .parse::<i64>()
        .ok()?;
    let cost_micros = evidence
        .get("trustedBrokerConsumedCostMicros")?
        .as_str()?
        .parse::<i64>()
        .ok()?;
    if tokens < 0 || cost_micros < 0 {
        return None;
    }
    let evidence_digest = execution_runner_protocol::canonical_sha256(&json!({
        "executionId": result.get("executionId"),
        "tokens": tokens,
        "costMicros": cost_micros,
        "requests": evidence.get("trustedBrokerConsumedRequests")
    }))
    .ok()?;
    Some(QuotaSettlement::Trusted {
        tokens,
        cost_micros,
        source: "runner-broker",
        evidence_digest,
    })
}

fn token_cost_micros(tokens: i64, rate_micros_per_million: i64) -> i64 {
    if tokens <= 0 || rate_micros_per_million <= 0 {
        return 0;
    }
    let product = i128::from(tokens).saturating_mul(i128::from(rate_micros_per_million));
    let rounded = product.saturating_add(999_999) / 1_000_000;
    i64::try_from(rounded).unwrap_or(i64::MAX)
}

impl AgentRepository {
    pub fn with_authority(pool: PgPool, authority: AgentRuntimeAuthority) -> Self {
        Self {
            pool,
            authority: Some(Arc::new(authority)),
            execution: None,
        }
    }

    pub fn with_execution_authority(
        pool: PgPool,
        authority: AgentRuntimeAuthority,
        execution: ExecutionClient,
    ) -> Self {
        Self {
            pool,
            authority: Some(Arc::new(authority)),
            execution: Some(Arc::new(execution)),
        }
    }

    pub async fn schedule_edge_action(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        spec: &EdgeActionSpec,
    ) -> Result<Uuid> {
        let argument_bytes = serde_json::to_vec(&spec.arguments)?;
        if spec.action.is_empty()
            || spec.action.len() > 126
            || !spec.arguments.is_object()
            || argument_bytes.len() > 64 * 1024
        {
            bail!("edge action name or arguments are invalid")
        }
        let authority = self
            .authority
            .as_ref()
            .context("edge execution requires immutable Agent authority")?;
        let binding = authority
            .edge_runner_bindings
            .iter()
            .find(|binding| {
                binding.edge_binding_id == spec.edge_binding_id
                    && binding.enabled
                    && binding.expires_at > Utc::now()
                    && binding
                        .allowed_actions
                        .iter()
                        .any(|action| action == &spec.action)
            })
            .context("no live projected edge runner binding authorizes this action")?;
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,s.principal_id
          FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
          WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION')
          FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0)
            .fetch_optional(&mut *tx).await?.context("no live Agent turn authorizes this action")?;
        let policy: String = row.try_get("policy_digest")?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let principal: String = row.try_get("principal_id")?;
        if principal != binding.principal_id {
            bail!("projected edge runner binding does not match the session principal")
        }
        let required_features = binding.required_capabilities.clone();
        let compatibility = binding.compatibility_digest.clone();
        let action_policy = binding
            .action_policies
            .get(&spec.action)
            .cloned()
            .context("projected edge action has no pinned policy")?;
        let schema = action_policy
            .get("schema")
            .context("edge action policy has no schema")?;
        let configured_schema_digest = action_policy
            .get("schemaDigest")
            .and_then(Value::as_str)
            .context("edge action policy has no schema digest")?;
        let computed_schema_digest = sha256_digest(&serde_json::to_vec(schema)?);
        if configured_schema_digest != computed_schema_digest
            || spec.schema_digest != configured_schema_digest
        {
            bail!("edge action schema digest does not match the binding")
        }
        validate_edge_arguments("$", schema, &spec.arguments)?;
        let stable_tool_ref = Uuid::parse_str(
            action_policy
                .get("stableToolRef")
                .and_then(Value::as_str)
                .context("edge action policy has no stable tool reference")?,
        )?;
        let effect_class = action_policy
            .get("effectClass")
            .and_then(Value::as_str)
            .context("edge action policy has no effect class")?;
        if !matches!(
            effect_class,
            "read-only" | "local-mutation" | "external-effect"
        ) {
            bail!("edge action effect class is invalid")
        }
        let approval_required = action_policy
            .get("approvalRequired")
            .and_then(Value::as_bool)
            .context("edge action policy has no approval requirement")?;
        if effect_class != "read-only" && !approval_required {
            bail!("mutating edge actions must require approval")
        }
        let argument_digest = sha256_digest(&argument_bytes);
        let action_subject_digest = sha256_digest(spec.action.as_bytes());
        let action_attempt_id = if approval_required {
            let approval_id = spec
                .approval_id
                .context("edge action approval is required")?;
            let approved=sqlx::query("SELECT a.consumed_action_attempt_id
              FROM agent_approval_t a JOIN agent_action_attempt_t x ON x.host_id=a.host_id AND x.action_attempt_id=a.consumed_action_attempt_id
              WHERE a.host_id=$1 AND a.approval_id=$2 AND a.turn_id=$3 AND a.state='APPROVED' AND a.expires_ts>now()
                AND a.subject_digest=$4 AND a.input_digest=$5 AND a.policy_digest=$6
                AND x.state='READY' AND x.stable_tool_ref=$7 AND x.schema_digest=$8 AND x.argument_digest=$5 AND x.policy_digest=$6
              FOR UPDATE OF a,x")
                .bind(host_id).bind(approval_id).bind(turn_id.0).bind(&action_subject_digest).bind(&argument_digest).bind(&policy).bind(stable_tool_ref).bind(&spec.schema_digest)
                .fetch_optional(&mut *tx).await?.context("edge action approval is unavailable, expired, or does not bind the exact action")?;
            let attempt: Uuid = approved.try_get("consumed_action_attempt_id")?;
            let changed=sqlx::query("UPDATE agent_action_attempt_t SET state='DISPATCHED',effect_class=$3,updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND state='READY'")
                .bind(host_id).bind(attempt).bind(effect_class).execute(&mut *tx).await?;
            if changed.rows_affected() != 1 {
                bail!("approved edge action attempt was already consumed")
            }
            attempt
        } else {
            if spec.approval_id.is_some() {
                bail!("read-only edge action cannot consume an unrelated approval")
            }
            let attempt = Uuid::now_v7();
            sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state) VALUES($1,$2,$3,$2,1,$4,$5,'runner',$6,$7,$8,$9,'DISPATCHED')")
                .bind(host_id).bind(attempt).bind(turn_id.0).bind(stable_tool_ref).bind(&spec.action).bind(&spec.schema_digest).bind(&policy).bind(&argument_digest).bind(effect_class).execute(&mut *tx).await?;
            attempt
        };
        let requirements = ExecutionRequirements {
            action_kind: format!("edge.{}", spec.action),
            minimum_boundary: IsolationBoundary::UserNamespace,
            maximum_host_exposure: HostExposure::ExplicitMounts,
            network_enabled: true,
            credential_classes: vec![],
            persistent_workspace: false,
            required_features,
            policy_digest: policy.clone(),
            compatibility_digest: compatibility.clone(),
        };
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "personal-edge-action-v1".into(),
            template_version: 1,
            template_digest:
                "sha256:ae5c8ce6e21f5270cce087e8ae0fcf8a95df83569ee993adb7650f98e6dce033".into(),
            executable: "/usr/local/bin/light-edge-action".into(),
            arguments: vec![
                "--action".into(),
                spec.action.clone(),
                "--arguments-json".into(),
                serde_json::to_string(&spec.arguments)?,
            ],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            wall_clock_timeout_ms: 120_000,
            stdout_limit_bytes: 1024 * 1024,
            stderr_limit_bytes: 1024 * 1024,
            network_enabled: true,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        let request_id = action_attempt_id;
        let origin_reference_digest = format!(
            "sha256:{}",
            canonical_sha256(&(
                host_id,
                session_id.0,
                turn_id.0,
                action_attempt_id,
                spec.edge_binding_id,
                argument_digest.as_str(),
                policy.as_str(),
            ))?
        );
        let approval_evidence_digest = spec
            .approval_id
            .map(|approval_id| {
                canonical_sha256(&(
                    approval_id,
                    action_subject_digest.as_str(),
                    argument_digest.as_str(),
                    policy.as_str(),
                ))
                .map(|digest| format!("sha256:{digest}"))
            })
            .transpose()?;
        let request = SchedulingRequestSubmission {
            request_id,
            idempotency_key: format!("edge-action:{action_attempt_id}"),
            origin_kind: "agent".into(),
            origin_instance_id: instance_id.to_string(),
            subject_kind: "agent-action".into(),
            subject_id: action_attempt_id,
            process_id: None,
            task_id: None,
            agent_session_id: Some(session_id.0),
            agent_turn_id: Some(turn_id.0),
            agent_action_id: Some(action_attempt_id),
            policy_snapshot_id: snapshot,
            policy_digest: policy.clone(),
            normalized_requirements: serde_json::to_value(requirements)?,
            execution_spec: serde_json::to_value(command)?,
            resolved_policy: json!({
                "policyDigest": policy,
                "edgeBindingId": spec.edge_binding_id,
                "edgeBindingDigest": binding.digest,
                "edgeBindingRevocationEpoch": binding.revocation_epoch,
                "actionPolicy": action_policy,
            }),
            definition_digest: authority.definition_digest.clone(),
            fairness_key: format!("agent:{principal}"),
            priority: 0,
            workflow_reference_digest: None,
            origin_reference_digest,
            approval_id: spec.approval_id,
            approval_evidence_digest,
            pinned_runner_id: Some(binding.runner_id.clone()),
            pinned_backend_id: Some(binding.backend_id.clone()),
            edge_binding_id: Some(spec.edge_binding_id),
            edge_binding_compatibility_digest: Some(compatibility),
            edge_binding_revocation_epoch: Some(
                i64::try_from(binding.revocation_epoch)
                    .context("edge binding revocation epoch is too large")?,
            ),
            inputs: Vec::new(),
        };
        Self::enqueue_execution_request(&mut tx, host_id, &request).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2").bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(action_attempt_id)
    }
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            authority: None,
            execution: None,
        }
    }

    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }

    async fn enqueue_execution_request(
        tx: &mut Transaction<'_, Postgres>,
        host_id: Uuid,
        request: &SchedulingRequestSubmission,
    ) -> Result<()> {
        let payload = serde_json::to_value(request)?;
        let payload_digest = format!("sha256:{}", canonical_sha256(&payload)?);
        let inserted = sqlx::query(
            "INSERT INTO agent_execution_outbox_t(
                host_id,dispatch_id,request_id,command_kind,command_payload,payload_digest
             ) VALUES($1,$2,$3,'REQUEST',$4,$5)
             ON CONFLICT(host_id,request_id,command_kind) DO UPDATE
             SET updated_ts=agent_execution_outbox_t.updated_ts
             WHERE agent_execution_outbox_t.payload_digest=EXCLUDED.payload_digest",
        )
        .bind(host_id)
        .bind(Uuid::now_v7())
        .bind(request.request_id)
        .bind(payload)
        .bind(payload_digest)
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() != 1 {
            bail!("execution request idempotency key was reused with another payload")
        }
        Ok(())
    }

    async fn enqueue_cleanup_request(
        tx: &mut Transaction<'_, Postgres>,
        host_id: Uuid,
        request: &CleanupRequestSubmission,
    ) -> Result<()> {
        let payload = serde_json::to_value(request)?;
        let payload_digest = format!("sha256:{}", canonical_sha256(&payload)?);
        let inserted = sqlx::query(
            "INSERT INTO agent_execution_outbox_t(
                host_id,dispatch_id,request_id,command_kind,command_payload,payload_digest
             ) VALUES($1,$2,$3,'CLEANUP',$4,$5)
             ON CONFLICT(host_id,request_id,command_kind) DO UPDATE
             SET updated_ts=agent_execution_outbox_t.updated_ts
             WHERE agent_execution_outbox_t.payload_digest=EXCLUDED.payload_digest",
        )
        .bind(host_id)
        .bind(Uuid::now_v7())
        .bind(request.cleanup_request_id)
        .bind(payload)
        .bind(payload_digest)
        .execute(&mut **tx)
        .await?;
        if inserted.rows_affected() != 1 {
            bail!("execution cleanup idempotency key was reused with another payload")
        }
        Ok(())
    }

    pub async fn dispatch_execution_outbox(&self) -> Result<u64> {
        let execution = self
            .execution
            .as_ref()
            .context("Agent execution API client is not configured")?;
        let rows = sqlx::query(
            "SELECT host_id,dispatch_id,request_id,command_kind,command_payload,payload_digest
             FROM agent_execution_outbox_t WHERE state='PENDING' AND next_attempt_ts<=now()
             ORDER BY created_ts,dispatch_id LIMIT 32",
        )
        .fetch_all(&self.pool)
        .await?;
        let mut dispatched = 0;
        for row in rows {
            let host_id: Uuid = row.try_get("host_id")?;
            let dispatch_id: Uuid = row.try_get("dispatch_id")?;
            let request_id: Uuid = row.try_get("request_id")?;
            let kind: String = row.try_get("command_kind")?;
            let payload: Value = row.try_get("command_payload")?;
            let expected_digest: String = row.try_get("payload_digest")?;
            let actual_digest = format!("sha256:{}", canonical_sha256(&payload)?);
            if actual_digest != expected_digest {
                sqlx::query(
                    "UPDATE agent_execution_outbox_t SET state='DEAD',last_error='payload digest mismatch',
                            updated_ts=now() WHERE host_id=$1 AND dispatch_id=$2 AND state='PENDING'",
                )
                .bind(host_id)
                .bind(dispatch_id)
                .execute(&self.pool)
                .await?;
                continue;
            }
            let result = match kind.as_str() {
                "REQUEST" => {
                    let request = serde_json::from_value::<SchedulingRequestSubmission>(payload)?;
                    execution
                        .submit_request(&request)
                        .await
                        .map(|accepted| (accepted == request_id).then_some(accepted))
                        .and_then(|accepted| {
                            accepted.ok_or_else(|| {
                                execution_client::ClientError::Credential(
                                    "execution authority returned another request ID".to_string(),
                                )
                            })
                        })
                        .map(|_| ())
                }
                "CLEANUP" => {
                    let request = serde_json::from_value::<CleanupRequestSubmission>(payload)?;
                    execution
                        .submit_cleanup_request(&request)
                        .await
                        .map(|accepted| (accepted == request_id).then_some(accepted))
                        .and_then(|accepted| {
                            accepted.ok_or_else(|| {
                                execution_client::ClientError::Credential(
                                    "execution authority returned another cleanup request ID"
                                        .to_string(),
                                )
                            })
                        })
                        .map(|_| ())
                }
                _ => unreachable!("outbox command_kind is database constrained"),
            };
            match result {
                Ok(()) => {
                    sqlx::query(
                        "UPDATE agent_execution_outbox_t SET state='DISPATCHED',dispatched_ts=now(),
                                last_error=NULL,updated_ts=now()
                         WHERE host_id=$1 AND dispatch_id=$2 AND state='PENDING'",
                    )
                    .bind(host_id)
                    .bind(dispatch_id)
                    .execute(&self.pool)
                    .await?;
                    dispatched += 1;
                }
                Err(error) => {
                    sqlx::query(
                        "UPDATE agent_execution_outbox_t
                         SET attempt_count=attempt_count+1,
                             state=CASE WHEN attempt_count+1>=20 THEN 'DEAD' ELSE 'PENDING' END,
                             next_attempt_ts=now()+LEAST(interval '5 minutes',
                                 make_interval(secs=>power(2,LEAST(attempt_count+1,8))::int)),
                             last_error=left($3,512),updated_ts=now()
                         WHERE host_id=$1 AND dispatch_id=$2 AND state='PENDING'",
                    )
                    .bind(host_id)
                    .bind(dispatch_id)
                    .bind(error.to_string())
                    .execute(&self.pool)
                    .await?;
                }
            }
        }
        Ok(dispatched)
    }

    pub async fn active_turn_ids(&self, host_id: Uuid, turn_ids: &[Uuid]) -> Result<Vec<Uuid>> {
        if turn_ids.is_empty() {
            return Ok(Vec::new());
        }
        Ok(sqlx::query_scalar(
            "SELECT t.turn_id FROM agent_turn_t t JOIN agent_session_t s
               ON s.host_id=t.host_id AND s.session_id=t.session_id AND s.active_turn_id=t.turn_id
             WHERE t.host_id=$1 AND t.turn_id=ANY($2) AND t.state='RECEIVED'",
        )
        .bind(host_id)
        .bind(turn_ids)
        .fetch_all(&self.pool)
        .await?)
    }

    pub fn spawn_result_reconciler(&self) -> tokio::task::JoinHandle<()> {
        let repository = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = repository.listen_and_reconcile().await {
                    tracing::warn!("agent execution-result reconciler disconnected: {error}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        })
    }

    async fn listen_and_reconcile(&self) -> Result<()> {
        loop {
            self.dispatch_execution_outbox().await?;
            self.reconcile_agent_jobs().await?;
            self.reconcile_execution_results().await?;
            self.reconcile_expiry_and_cleanup().await?;
            self.reconcile_projections().await?;
            let retention_days = std::env::var("LIGHT_AGENT_QUOTA_USAGE_RETENTION_DAYS")
                .ok()
                .and_then(|value| value.parse::<i32>().ok())
                .unwrap_or(30)
                .clamp(1, 3650);
            self.sweep_quota_usage(retention_days, 1_000).await?;
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }

    pub async fn sweep_quota_usage(&self, retention_days: i32, batch_size: i64) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM agent_quota_usage_t u WHERE (u.host_id,u.quota_id,u.window_start_ts) IN
             (SELECT q.host_id,q.quota_id,q.window_start_ts FROM agent_quota_usage_t q
              WHERE q.window_start_ts < now()-make_interval(days=>$1)
                AND NOT EXISTS(SELECT 1 FROM agent_quota_reservation_t r
                  WHERE r.host_id=q.host_id AND r.quota_id=q.quota_id
                    AND r.window_start_ts=q.window_start_ts AND r.reconciled_ts IS NULL)
              ORDER BY q.window_start_ts LIMIT $2 FOR UPDATE SKIP LOCKED)",
        )
        .bind(retention_days.clamp(1, 3650))
        .bind(batch_size.clamp(1, 10_000))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn request_job_cancellation(&self, host_id: Uuid, job_id: Uuid) -> Result<bool> {
        let changed = sqlx::query(
            "UPDATE agent_job_t SET cancellation_requested_ts=COALESCE(cancellation_requested_ts,now()),
                    updated_ts=now()
              WHERE host_id=$1 AND job_id=$2
                AND state IN('PENDING','TURN_CREATED','RUNNING')",
        )
        .bind(host_id)
        .bind(job_id)
        .execute(&self.pool)
        .await?;
        Ok(changed.rows_affected() == 1)
    }

    pub async fn reconcile_agent_jobs(&self) -> Result<u64> {
        let authority = self
            .authority
            .as_ref()
            .context("workflow Agent jobs require immutable Config Server authority")?;
        let mut changed = 0;
        changed += sqlx::query("WITH expired AS (UPDATE agent_job_t SET state='FAILED',
                    error=jsonb_build_object('class','deadline_exceeded'),terminal_ts=now(),updated_ts=now()
                    WHERE state IN('PENDING','TURN_CREATED','RUNNING') AND deadline_ts<=now()
                    RETURNING host_id,turn_id) UPDATE agent_turn_t t SET state='CANCELLED',
                    terminal_error=jsonb_build_object('class','deadline_exceeded'),terminal_ts=now(),updated_ts=now()
                    FROM expired WHERE t.host_id=expired.host_id AND t.turn_id=expired.turn_id
                      AND t.state NOT IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?.rows_affected();
        for _ in 0..100 {
            let mut tx = self.pool.begin().await?;
            let row=sqlx::query("SELECT j.host_id,j.job_id,j.agent_def_id,j.idempotency_key,j.policy_digest,
                    j.data_boundary_digest,j.deadline_ts,j.token_budget,j.cost_budget_micros,j.delegation_depth
                 FROM agent_job_t j
                 WHERE j.state='PENDING' AND j.deadline_ts>now()
                   AND j.host_id=$1 AND j.agent_def_id=$2 AND j.policy_digest=$3
                   AND j.data_boundary_digest=$4
                 ORDER BY j.created_ts,j.job_id
                 LIMIT 1 FOR UPDATE OF j SKIP LOCKED")
                .bind(authority.host_id).bind(authority.agent_def_id).bind(&authority.policy_digest)
                .bind(&authority.data_boundary_digest)
                .fetch_optional(&mut *tx).await?;
            let Some(row) = row else {
                tx.commit().await?;
                break;
            };
            let host: Uuid = row.try_get("host_id")?;
            let job: Uuid = row.try_get("job_id")?;
            let turn = Uuid::now_v7();
            let deadline: DateTime<Utc> = row.try_get("deadline_ts")?;
            sqlx::query(
                "INSERT INTO agent_session_t(host_id,session_id,principal_id,agent_def_id,
                    agent_definition_version,policy_snapshot_id,idle_expires_ts,maximum_expires_ts,
                    resume_handle_digest,agent_publication_id,agent_content_digest,
                    agent_definition_digest,model_provider,model_name)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$7,$8,$9,$10,$11,$12,$13)
                    ON CONFLICT(host_id,session_id) DO NOTHING",
            )
            .bind(host)
            .bind(job)
            .bind(format!("workflow-job:{job}"))
            .bind(authority.agent_def_id)
            .bind(authority.definition_version)
            .bind(authority.policy_snapshot_id)
            .bind(deadline)
            .bind(sha256_digest(format!("workflow-job:{job}").as_bytes()))
            .bind(authority.publication_id)
            .bind(&authority.content_digest)
            .bind(&authority.definition_digest)
            .bind(&authority.model_provider)
            .bind(&authority.model_name)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO agent_turn_t(host_id,turn_id,session_id,turn_sequence,queue_sequence,
                    origin_kind,origin_ref,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,
                    data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,
                    cost_budget_micros,deadline_ts,delegation_depth)
                    VALUES($1,$2,$3,1,1,'workflow',$4,$5,$5,$6,$7,$8,$9,$10,20,$11,$12,$13,$14)")
                .bind(host).bind(turn).bind(job).bind(job.to_string())
                .bind(row.try_get::<String,_>("idempotency_key")?).bind(authority.policy_snapshot_id)
                .bind(row.try_get::<String,_>("policy_digest")?).bind(row.try_get::<String,_>("data_boundary_digest")?)
                .bind(&authority.model_provider).bind(&authority.model_name)
                .bind(row.try_get::<i64,_>("token_budget")?).bind(row.try_get::<i64,_>("cost_budget_micros")?)
                .bind(deadline).bind(row.try_get::<i32,_>("delegation_depth")?).execute(&mut *tx).await?;
            sqlx::query(
                "UPDATE agent_job_t SET turn_id=$1,state='TURN_CREATED',updated_ts=now()
                        WHERE host_id=$2 AND job_id=$3 AND state='PENDING'",
            )
            .bind(turn)
            .bind(host)
            .bind(job)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            changed += 1;
        }
        let terminal=sqlx::query("UPDATE agent_job_t j SET state=CASE t.state WHEN 'COMPLETED' THEN 'SUCCEEDED'
                    WHEN 'FAILED' THEN 'FAILED' WHEN 'CANCELLED' THEN 'CANCELLED' ELSE 'UNKNOWN' END,
                    public_output=CASE WHEN t.state='COMPLETED' THEN t.terminal_result END,
                    error=CASE WHEN t.state<>'COMPLETED' THEN t.terminal_error END,
                    terminal_ts=COALESCE(t.terminal_ts,now()),updated_ts=now()
                    FROM agent_turn_t t WHERE t.host_id=j.host_id AND t.turn_id=j.turn_id
                      AND j.state IN('TURN_CREATED','RUNNING') AND t.state IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?;
        let cancelled=sqlx::query("WITH jobs AS (UPDATE agent_job_t j SET state='CANCELLED',
                    error=jsonb_build_object('class','workflow_cancelled'),
                    terminal_ts=now(),updated_ts=now()
                    WHERE j.state IN('PENDING','TURN_CREATED','RUNNING')
                      AND j.cancellation_requested_ts IS NOT NULL
                    RETURNING j.host_id,j.turn_id) UPDATE agent_turn_t t SET state='CANCELLED',
                    terminal_error=jsonb_build_object('class','workflow_cancelled'),terminal_ts=now(),updated_ts=now()
                    FROM jobs WHERE t.host_id=jobs.host_id AND t.turn_id=jobs.turn_id
                      AND t.state NOT IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?;
        let cleanup_rows = sqlx::query(
            "SELECT j.host_id,j.job_id,j.turn_id,s.session_id,s.execution_session_id
             FROM agent_job_t j JOIN agent_session_t s
               ON s.host_id=j.host_id AND s.session_id=j.job_id
             WHERE j.cancellation_requested_ts IS NOT NULL
               AND s.execution_session_id IS NOT NULL
               AND s.cleanup_state IN('NOT_REQUIRED','CLEANUP_REQUESTED')
             ORDER BY j.created_ts,j.job_id LIMIT 100",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in cleanup_rows {
            let host_id: Uuid = row.try_get("host_id")?;
            let job_id: Uuid = row.try_get("job_id")?;
            let turn_id: Uuid = row.try_get("turn_id")?;
            let session_id: Uuid = row.try_get("session_id")?;
            let execution_session_id: Uuid = row.try_get("execution_session_id")?;
            let cleanup_id = Uuid::now_v7();
            let mut tx = self.pool.begin().await?;
            Self::enqueue_cleanup_request(
                &mut tx,
                host_id,
                &CleanupRequestSubmission {
                    cleanup_request_id: cleanup_id,
                    execution_session_id,
                    origin_kind: "agent".into(),
                    origin_instance_id: authority.instance_id.to_string(),
                    origin_session_id: Some(session_id),
                    subject_kind: "agent-turn".into(),
                    subject_id: turn_id,
                    idempotency_key: format!("workflow-job-cancel:{job_id}"),
                    reason: "workflow-cancelled".into(),
                    requested_by: authority.service_id.clone(),
                    cleanup_deadline: Utc::now() + Duration::minutes(5),
                },
            )
            .await?;
            sqlx::query(
                "UPDATE agent_session_t SET state='CLOSING',cleanup_request_id=$3,
                    cleanup_state='CLEANUP_PENDING',updated_ts=now()
                 WHERE host_id=$1 AND session_id=$2 AND state='ACTIVE'",
            )
            .bind(host_id)
            .bind(session_id)
            .bind(cleanup_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(changed + terminal.rows_affected() + cancelled.rows_affected())
    }

    pub async fn reconcile_execution_results(&self) -> Result<u64> {
        let execution = self
            .execution
            .as_ref()
            .context("Agent execution API client is not configured")?;
        let rows = execution.pending_results(100).await?;
        let mut accepted = 0;
        for result in rows {
            let applied = match result.subject_kind.as_str() {
                "agent-action" => self.accept_execution_result(&result).await?,
                "agent-turn" => self.accept_coding_turn_result(&result).await?,
                _ => false,
            };
            if applied {
                execution
                    .acknowledge_result(result.execution_id, result.fencing_token)
                    .await?;
                accepted += 1;
            } else if result.accepted {
                continue;
            }
        }
        Ok(accepted)
    }

    async fn accept_coding_turn_result(&self, result_view: &ExecutionResultView) -> Result<bool> {
        let host_id = result_view.host_id;
        let turn_id = result_view
            .agent_turn_id
            .context("agent-turn execution result has no turn identifier")?;
        if result_view.subject_id != turn_id || !result_view.terminal {
            bail!("agent-turn execution result has inconsistent subject evidence")
        }
        let execution_id = result_view.execution_id;
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.session_id,t.policy_digest,t.execution_attempt_id FROM agent_turn_t t WHERE t.host_id=$1 AND t.turn_id=$2 FOR UPDATE OF t")
            .bind(host_id).bind(turn_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        if row
            .try_get::<Option<Uuid>, _>("execution_attempt_id")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(false);
        }
        let session: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state = result_view.state.as_str();
        let result = json!({"executionId":execution_id,"state":state,"result":result_view.normalized_result,"error":result_view.normalized_error,"fencingToken":result_view.fencing_token});
        let trusted_usage = result_view
            .normalized_result
            .as_ref()
            .and_then(trusted_runner_quota_settlement)
            .unwrap_or(QuotaSettlement::ReservationCeiling);
        append_event(
            &mut tx,
            host_id,
            session,
            Some(turn_id),
            None,
            "runner",
            "CODING_TURN_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET execution_attempt_id=$3,state=CASE WHEN $4='SUCCEEDED' THEN 'COMPLETED' WHEN $4='CANCELLED' THEN 'CANCELLED' WHEN $4='UNKNOWN' THEN 'UNKNOWN' ELSE 'FAILED' END,terminal_result=CASE WHEN $4='SUCCEEDED' THEN $5 ELSE terminal_result END,terminal_error=CASE WHEN $4<>'SUCCEEDED' THEN $5 ELSE terminal_error END,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND execution_attempt_id IS NULL AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id).bind(execution_id).bind(&state).bind(&result).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(host_id).bind(session).bind(turn_id).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id, &trusted_usage).await?;
        sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
            .bind(host_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn schedule_coding_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        manifest: &MaterializationManifest,
        spec: &CodingTurnSpec,
        repository: &ImmutableRepositoryInput,
        fixture: &CodingFixtureRequest,
        compatibility_digest: &str,
    ) -> Result<Uuid> {
        spec.validate()?;
        repository.validate(spec)?;
        fixture.validate()?;
        if &fixture.spec != spec {
            bail!("coding fixture spec differs from the admitted turn spec")
        }
        let manifest_digest = manifest.digest()?;
        if manifest.product_profile != agent_materializer::ProductProfile::Coding
            || spec.materialization_manifest_digest != manifest_digest
        {
            bail!("coding materialization profile or digest mismatch")
        }
        if !manifest.packages.is_empty() {
            bail!("the first Cube coding fixture admits only the immutable repository input")
        }
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,t.data_boundary_digest,s.principal_id FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id AND p.revoked_ts IS NULL WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state IN ('RECEIVED','RUNNING_MODEL') FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).fetch_one(&mut *tx).await?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let principal: String = row.try_get("principal_id")?;
        let request_id = turn_id.0;
        let requirements = ExecutionRequirements {
            action_kind: "coding.fixture".into(),
            minimum_boundary: IsolationBoundary::MicroVm,
            maximum_host_exposure: HostExposure::None,
            network_enabled: false,
            credential_classes: vec![],
            persistent_workspace: false,
            required_features: vec![
                "deny-all-egress".into(),
                "immutable-repository-upload".into(),
                "canonical-patch-output".into(),
            ],
            policy_digest: policy.clone(),
            compatibility_digest: compatibility_digest.into(),
        };
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-coding-fixture-v1".into(),
            template_version: 1,
            template_digest:
                "sha256:503c1f8879addd7dec140d9f2e703e6b7230979188bbd6f7c9e4f941e276a717".into(),
            executable: "/usr/local/bin/light-coding-agent-fixture".into(),
            arguments: vec![
                "--repository".into(),
                "/inputs/repository.bundle".into(),
                "--request-base64".into(),
                fixture.encode_argument()?,
            ],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            wall_clock_timeout_ms: 120_000,
            stdout_limit_bytes: 1024 * 1024,
            stderr_limit_bytes: 1024 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        let authority = self
            .authority
            .as_ref()
            .context("coding execution requires immutable Agent authority")?;
        let request = SchedulingRequestSubmission {
            request_id,
            idempotency_key: format!("coding-turn:{}", turn_id.0),
            origin_kind: "agent".into(),
            origin_instance_id: instance_id.to_string(),
            subject_kind: "agent-turn".into(),
            subject_id: turn_id.0,
            process_id: None,
            task_id: None,
            agent_session_id: Some(session_id.0),
            agent_turn_id: Some(turn_id.0),
            agent_action_id: None,
            policy_snapshot_id: snapshot,
            policy_digest: policy.clone(),
            normalized_requirements: serde_json::to_value(requirements)?,
            execution_spec: serde_json::to_value(command)?,
            resolved_policy: json!({
                "policyDigest": policy,
                "dataBoundaryDigest": row.try_get::<String, _>("data_boundary_digest")?,
                "materializationManifestDigest": manifest_digest,
                "baseRevision": spec.base_revision,
            }),
            definition_digest: authority.definition_digest.clone(),
            fairness_key: format!("agent:{principal}"),
            priority: 0,
            workflow_reference_digest: None,
            origin_reference_digest: format!(
                "sha256:{}",
                canonical_sha256(&(
                    host_id,
                    session_id.0,
                    turn_id.0,
                    &manifest_digest,
                    &spec.base_revision,
                    &policy
                ))?
            ),
            approval_id: None,
            approval_evidence_digest: None,
            pinned_runner_id: None,
            pinned_backend_id: None,
            edge_binding_id: None,
            edge_binding_compatibility_digest: None,
            edge_binding_revocation_epoch: None,
            inputs: vec![ExecutionInputSubmission {
                input_id: Uuid::now_v7(),
                kind: "repository-bundle".into(),
                artifact_uri: repository.artifact_uri.clone(),
                content_digest: repository.digest.clone(),
                size_bytes: repository.size as i64,
                media_type: repository.media_type.clone(),
                signer_binding: None,
                provenance_binding: Some(json!({"baseRevision": spec.base_revision})),
                scanner_binding: None,
                revocation_binding: Some(json!({"state": "IMMUTABLE"})),
                staging_root: format!("{}/inputs", spec.workspace_root),
                mount_target: "/inputs/repository.bundle".into(),
                read_only: true,
                executable: false,
                trust_bundle_id: None,
                trust_bundle_version: None,
                package_manifest_digest: None,
                mount_options: json!(["ro", "nodev", "nosuid", "noexec"]),
            }],
        };
        Self::enqueue_execution_request(&mut tx, host_id, &request).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","CODING_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision}),&policy).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn schedule_coding_adapter_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        manifest: &MaterializationManifest,
        spec: &CodingTurnSpec,
        repository: &ImmutableRepositoryInput,
        runtime: &CodingAdapterRuntime,
    ) -> Result<Uuid> {
        spec.validate()?;
        repository.validate(spec)?;
        runtime.contract.validate()?;
        if runtime.model != spec.model_alias {
            bail!("coding runtime model alias differs from immutable role profile")
        }
        match (
            spec.authentication_profile,
            runtime.enterprise_gateway.is_some(),
        ) {
            (coding_agent_runtime::CodingAuthenticationProfile::PersonalSubscription, false)
            | (coding_agent_runtime::CodingAuthenticationProfile::EnterpriseApi, true) => {}
            _ => bail!("coding authentication profile differs from the admitted runtime route"),
        }
        let manifest_digest = manifest.digest()?;
        if manifest.product_profile != agent_materializer::ProductProfile::Coding
            || spec.materialization_manifest_digest != manifest_digest
            || manifest.runtime_compatibility != runtime.contract.compatibility_digest
            || manifest.writable_roots != spec.writable_roots
        {
            bail!("coding adapter materialization or runtime binding mismatch")
        }
        if runtime.model.is_empty() || runtime.model.starts_with('-') {
            bail!("Codex model binding is invalid")
        }
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,t.data_boundary_digest,t.deadline_ts,s.principal_id FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id AND p.revoked_ts IS NULL WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state='RECEIVED' FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).fetch_one(&mut *tx).await?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let data_boundary: String = row.try_get("data_boundary_digest")?;
        let principal: String = row.try_get("principal_id")?;
        let turn_deadline: DateTime<Utc> = row.try_get("deadline_ts")?;
        let request_id = turn_id.0;
        let enterprise_gateway =
            runtime
                .enterprise_gateway
                .as_ref()
                .map(|gateway| EnterpriseGatewayConfig {
                    provider_id: "light_gateway".into(),
                    base_url: gateway.base_url.clone(),
                    credential_target: gateway.credential_target.clone(),
                    credential_env: "LIGHT_LLM_ATTEMPT_TOKEN".into(),
                    binding: GatewayAttemptBinding {
                        audience: gateway.audience.clone(),
                        host_id,
                        end_user_subject: principal.clone(),
                        principal_subject: principal.clone(),
                        workload_actor: format!("light-agent/{instance_id}"),
                        workflow_id: None,
                        session_id,
                        turn_id,
                        action_attempt_id: AgentActionAttemptId(turn_id.0),
                        policy_digest: policy.clone(),
                        data_boundary_digest: data_boundary.clone(),
                        route_alias: runtime.model.clone(),
                        billing_subject: principal.clone(),
                        budget_policy_id: gateway.budget_policy_id.clone(),
                        correlation_id: turn_id.0,
                    },
                });
        if let Some(gateway) = &enterprise_gateway {
            gateway.validate()?;
        }
        let broker = match (&runtime.enterprise_gateway, &enterprise_gateway) {
            (Some(policy), Some(gateway)) => Some(AttemptBrokerGrant {
                policy_digest: gateway.binding.policy_digest.clone(),
                data_boundary_digest: gateway.binding.data_boundary_digest.clone(),
                route_digest: policy.route_digest.clone(),
                allowed_operations: std::collections::BTreeSet::from([
                    BrokerOperation::CredentialedRequest,
                ]),
                allowed_targets: std::collections::BTreeSet::from([policy
                    .credential_target
                    .clone()]),
                maximum_requests: policy.maximum_requests,
                maximum_tokens: policy.maximum_tokens,
                maximum_cost_micros: policy.maximum_cost_micros,
                maximum_response_bytes: policy.maximum_response_bytes,
                // The grant shares the authoritative turn/lease deadline. It must
                // not introduce a shorter enqueue-relative lifetime.
                expires_at: turn_deadline,
                gateway_binding_digest: Some(gateway.binding.digest()?),
            }),
            _ => None,
        };
        let mut required_features: Vec<String> =
            runtime.contract.required_features.iter().cloned().collect();
        if enterprise_gateway.is_some() {
            required_features.push("enterprise-llm-gateway-v1".into());
            required_features.push("enterprise-api-auth-v1".into());
            required_features.push("restricted-model-egress".into());
            required_features.push("per-attempt-worker-sandbox-v1".into());
        } else {
            required_features.push("personal-subscription-auth-v1".into());
            required_features.push("local-single-user-native-v1".into());
        }
        let requirements = ExecutionRequirements {
            action_kind: runtime.contract.action_kind.clone(),
            minimum_boundary: if enterprise_gateway.is_some() {
                IsolationBoundary::Container
            } else {
                IsolationBoundary::Process
            },
            maximum_host_exposure: HostExposure::ExplicitMounts,
            network_enabled: true,
            credential_classes: enterprise_gateway
                .as_ref()
                .map(|_| vec!["llm-gateway-attempt".into()])
                .unwrap_or_default(),
            persistent_workspace: false,
            required_features,
            policy_digest: policy.clone(),
            compatibility_digest: runtime.contract.compatibility_digest.clone(),
        };
        let command = AgentWorkerExecutionSpec {
            schema_version: 1,
            template_digest: runtime.contract.template_digest.clone(),
            expected_capability_digest: runtime.contract.capability_digest.clone(),
            session_id,
            turn_id,
            action_attempt_id: AgentActionAttemptId(turn_id.0),
            policy_digest: policy.clone(),
            input: json!({
                "codingSpec": spec,
                "materializationManifest": manifest,
                "adapterContract": runtime.contract,
                "adapterQualification": runtime.qualification,
            }),
            wall_clock_timeout_ms: 120_000,
            maximum_event_bytes: 1024 * 1024,
            maximum_stderr_bytes: 1024 * 1024,
            broker,
            enterprise_gateway,
        };
        let authority = self
            .authority
            .as_ref()
            .context("coding adapter execution requires immutable Agent authority")?;
        let request = SchedulingRequestSubmission {
            request_id,
            idempotency_key: format!("coding-adapter-turn:{}", turn_id.0),
            origin_kind: "agent".into(),
            origin_instance_id: instance_id.to_string(),
            subject_kind: "agent-turn".into(),
            subject_id: turn_id.0,
            process_id: None,
            task_id: None,
            agent_session_id: Some(session_id.0),
            agent_turn_id: Some(turn_id.0),
            agent_action_id: None,
            policy_snapshot_id: snapshot,
            policy_digest: policy.clone(),
            normalized_requirements: serde_json::to_value(requirements)?,
            execution_spec: serde_json::to_value(command)?,
            resolved_policy: json!({
                "policyDigest": policy,
                "materializationManifestDigest": manifest_digest,
                "baseRevision": spec.base_revision,
                "codingAdapterContractDigest": runtime.contract.digest()?,
                "adapterId": runtime.contract.adapter_id,
                "adapterVersion": runtime.contract.adapter_version,
                "adapterProtocolVersion": runtime.contract.adapter_protocol_version,
                "runtimeCompatibilityDigest": runtime.contract.compatibility_digest,
                "imageDigest": runtime.contract.image_digest,
                "capabilityDigest": runtime.contract.capability_digest,
                "templateDigest": runtime.contract.template_digest,
            }),
            definition_digest: authority.definition_digest.clone(),
            fairness_key: format!("agent:{principal}"),
            priority: 0,
            workflow_reference_digest: None,
            origin_reference_digest: format!(
                "sha256:{}",
                canonical_sha256(&(
                    host_id,
                    session_id.0,
                    turn_id.0,
                    &manifest_digest,
                    &spec.base_revision,
                    &policy,
                    &runtime.contract.digest()?
                ))?
            ),
            approval_id: None,
            approval_evidence_digest: None,
            pinned_runner_id: None,
            pinned_backend_id: None,
            edge_binding_id: None,
            edge_binding_compatibility_digest: None,
            edge_binding_revocation_epoch: None,
            inputs: vec![ExecutionInputSubmission {
                input_id: Uuid::now_v7(),
                kind: "repository-bundle".into(),
                artifact_uri: repository.artifact_uri.clone(),
                content_digest: repository.digest.clone(),
                size_bytes: repository.size as i64,
                media_type: repository.media_type.clone(),
                signer_binding: None,
                provenance_binding: Some(json!({"baseRevision": spec.base_revision})),
                scanner_binding: None,
                revocation_binding: Some(json!({"state": "IMMUTABLE"})),
                staging_root: format!("{}/inputs", spec.workspace_root),
                mount_target: "/inputs/repository.bundle".into(),
                read_only: true,
                executable: false,
                trust_bundle_id: None,
                trust_bundle_version: None,
                package_manifest_digest: None,
                mount_options: json!(["ro", "nodev", "nosuid", "noexec"]),
            }],
        };
        Self::enqueue_execution_request(&mut tx, host_id, &request).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        if !manifest.packages.is_empty() {
            bail!(
                "coding adapter package mounting is not admitted until policy-to-package resolution is server-owned"
            )
        }
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='RECEIVED'")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","CODING_ADAPTER_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision,"adapterId":runtime.contract.adapter_id,"adapterVersion":runtime.contract.adapter_version,"contractDigest":runtime.contract.digest()?}),&policy).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn reconcile_expiry_and_cleanup(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_approval_t SET state='EXPIRED',decision_ts=now(),decision_reason='approval deadline expired' WHERE state='REQUESTED' AND expires_ts<=now()")
            .execute(&mut *tx).await?;
        let stale = sqlx::query("UPDATE agent_turn_t SET state='UNKNOWN',terminal_error=jsonb_build_object('message','turn deadline expired during reconciliation'),terminal_ts=now(),updated_ts=now() WHERE state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION') AND deadline_ts<=now() RETURNING host_id,session_id,turn_id")
            .fetch_all(&mut *tx).await?;
        let mut freed_hosts = std::collections::BTreeSet::new();
        for row in stale {
            let host_id: Uuid = row.try_get("host_id")?;
            sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(host_id).bind(row.try_get::<Uuid,_>("session_id")?).bind(row.try_get::<Uuid,_>("turn_id")?).execute(&mut *tx).await?;
            freed_hosts.insert(host_id);
        }
        for host_id in freed_hosts {
            sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
                .bind(host_id.to_string())
                .execute(&mut *tx)
                .await?;
        }
        let expired = sqlx::query("UPDATE agent_session_t SET state='EXPIRED',cleanup_state=CASE WHEN execution_session_id IS NULL THEN 'NOT_REQUIRED' ELSE 'CLEANUP_REQUESTED' END,updated_ts=now() WHERE state='ACTIVE' AND LEAST(idle_expires_ts,maximum_expires_ts)<=now() RETURNING host_id,session_id,execution_session_id")
            .fetch_all(&mut *tx).await?;
        for row in expired {
            let host_id: Uuid = row.try_get("host_id")?;
            let session_id: Uuid = row.try_get("session_id")?;
            if let Some(execution_session_id) =
                row.try_get::<Option<Uuid>, _>("execution_session_id")?
            {
                let cleanup_id = Uuid::now_v7();
                let authority = self
                    .authority
                    .as_ref()
                    .context("Agent cleanup requires immutable runtime authority")?;
                Self::enqueue_cleanup_request(
                    &mut tx,
                    host_id,
                    &CleanupRequestSubmission {
                        cleanup_request_id: cleanup_id,
                        execution_session_id,
                        origin_kind: "agent".into(),
                        origin_instance_id: authority.instance_id.to_string(),
                        origin_session_id: Some(session_id),
                        subject_kind: "agent-turn".into(),
                        subject_id: session_id,
                        idempotency_key: format!("session-expired:{session_id}"),
                        reason: "session-expired".into(),
                        requested_by: authority.service_id.clone(),
                        cleanup_deadline: Utc::now() + Duration::minutes(5),
                    },
                )
                .await?;
                sqlx::query("UPDATE agent_session_t SET cleanup_request_id=$3,cleanup_state='CLEANUP_PENDING' WHERE host_id=$1 AND session_id=$2").bind(host_id).bind(session_id).bind(cleanup_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reconcile_projections(&self) -> Result<()> {
        let rows = sqlx::query("SELECT s.host_id,s.session_id,h.bank_id FROM agent_session_t s JOIN agent_session_history_t h ON h.host_id=s.host_id AND h.durable_session_id=s.session_id WHERE h.projection_sequence < (SELECT COALESCE(MAX(e.event_sequence),0) FROM agent_session_event_t e WHERE e.host_id=s.host_id AND e.session_id=s.session_id) LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in rows {
            self.rebuild_history_projection(
                row.try_get("host_id")?,
                AgentSessionId(row.try_get("session_id")?),
                row.try_get("bank_id")?,
            )
            .await?;
        }
        Ok(())
    }

    async fn accept_execution_result(&self, result_view: &ExecutionResultView) -> Result<bool> {
        let host_id = result_view.host_id;
        let action_attempt_id = result_view
            .agent_action_id
            .context("agent-action execution result has no action identifier")?;
        let turn_id = result_view
            .agent_turn_id
            .context("agent-action execution result has no turn identifier")?;
        if result_view.subject_id != action_attempt_id || !result_view.terminal {
            bail!("agent-action execution result has inconsistent subject evidence")
        }
        let execution_id = result_view.execution_id;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,a.execution_attempt_id,t.session_id,t.policy_digest FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 FOR UPDATE OF a,t")
            .bind(host_id).bind(action_attempt_id).bind(turn_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(false);
        }
        if row
            .try_get::<Option<Uuid>, _>("execution_attempt_id")?
            .is_some_and(|existing| existing != execution_id)
        {
            bail!("agent action is bound to another execution attempt")
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state = result_view.state.as_str();
        let result = json!({"executionId":execution_id,"state":state,"result":result_view.normalized_result,"error":result_view.normalized_error,"fencingToken":result_view.fencing_token});
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id),
            Some(action_attempt_id),
            "runner",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET execution_attempt_id=COALESCE(execution_attempt_id,$5),state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(&result).bind(sha256_digest(&serde_json::to_vec(&result)?)).bind(execution_id).execute(&mut *tx).await?;
        if result_view.action_kind.starts_with("edge.") {
            sqlx::query("UPDATE agent_turn_t SET state=CASE $3 WHEN 'SUCCEEDED' THEN 'COMPLETED' WHEN 'CANCELLED' THEN 'CANCELLED' WHEN 'UNKNOWN' THEN 'UNKNOWN' ELSE 'FAILED' END,terminal_result=CASE WHEN $3='SUCCEEDED' THEN $4 ELSE terminal_result END,terminal_error=CASE WHEN $3<>'SUCCEEDED' THEN $4 ELSE terminal_error END,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state IN ('RUNNING_ACTION','WAITING_RECONCILIATION')")
                .bind(host_id).bind(turn_id).bind(&state).bind(&result).execute(&mut *tx).await?;
            sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
                .bind(host_id).bind(session_id).bind(turn_id).execute(&mut *tx).await?;
            reconcile_turn_quota_usage(&mut tx, host_id, turn_id, &QuotaSettlement::Release)
                .await?;
            sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
                .bind(host_id.to_string())
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("UPDATE agent_turn_t SET state=CASE WHEN $3 IN ('SUCCEEDED','FAILED','CANCELLED') THEN 'RUNNING_MODEL' ELSE 'UNKNOWN' END,updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state IN ('RUNNING_ACTION','WAITING_RECONCILIATION')")
                .bind(host_id).bind(turn_id).bind(state).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn create_or_resume_session(&self, spec: &SessionSpec) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let authority = self
            .authority
            .as_ref()
            .context("Agent session admission requires immutable projection authority")?;
        let policy_digest = policy_document_digest(&spec.policy)?;
        if spec.definition_version <= 0
            || spec.model_provider != GATEWAY_PROVIDER_ID
            || spec.model_name.trim().is_empty()
        {
            bail!(
                "session authority must contain a positive definition version and governed gateway model alias"
            )
        }
        if authority.host_id != spec.host_id
            || authority.agent_def_id != spec.agent_def_id
            || authority.definition_version != spec.definition_version
            || authority.policy_snapshot_id != spec.policy.snapshot_id
            || authority.policy_digest != policy_digest
            || authority.model_provider != spec.model_provider
            || authority.model_name != spec.model_name
        {
            bail!("session admission does not match the accepted Agent projection")
        }
        persist_runtime_scope(&mut tx, authority).await?;
        persist_policy(&mut tx, authority, &spec.policy).await?;
        let pool = resolve_pool(
            &authority.service_pools,
            spec.host_id,
            &spec.policy.data_boundary_digest,
            &spec.policy.product_profile_digest,
        )?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!(
                "agent-session:{}:{}",
                spec.host_id, spec.session_id.0
            ))
            .execute(&mut *tx)
            .await?;
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_session_t WHERE host_id=$1 AND session_id=$2)",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            if spec.maximum_active_sessions == 0 || spec.maximum_active_sessions > i64::MAX as u64 {
                bail!("configured active-session limit is invalid")
            }
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
                .bind(format!(
                    "agent-config-active-sessions:{}:{}",
                    spec.host_id, spec.agent_def_id
                ))
                .execute(&mut *tx)
                .await?;
            let active: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM agent_session_t
                 WHERE host_id=$1 AND agent_def_id=$2 AND state='ACTIVE'",
            )
            .bind(spec.host_id)
            .bind(spec.agent_def_id)
            .fetch_one(&mut *tx)
            .await?;
            if active >= spec.maximum_active_sessions as i64 {
                bail!("configured Agent active-session limit exceeded")
            }
            enforce_quotas(
                &mut tx,
                &authority.quota_policies,
                spec.host_id,
                &spec.principal_id,
                spec.agent_def_id,
                &spec.policy.product_profile_digest,
                &spec.model_provider,
                pool.as_ref().map(|p| p.pool_id),
                None,
                true,
                0,
                0,
                true,
            )
            .await?;
        }
        let result = sqlx::query(
            "INSERT INTO agent_session_t
             (host_id,session_id,principal_id,user_id,agent_def_id,agent_definition_version,bank_id,
              policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest,
              service_pool_id,service_pool_compatibility_digest,service_pool_maximum_concurrency,
              agent_publication_id,agent_content_digest,agent_definition_digest,
              user_identity_digest,model_provider,model_name)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20)
             ON CONFLICT (host_id,session_id) DO NOTHING",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .bind(&spec.principal_id)
        .bind(spec.user_id)
        .bind(spec.agent_def_id)
        .bind(spec.definition_version)
        .bind(spec.bank_id)
        .bind(spec.policy.snapshot_id)
        .bind(spec.idle_expires_at)
        .bind(spec.maximum_expires_at)
        .bind(&spec.resume_handle_digest)
        .bind(pool.as_ref().map(|p| p.pool_id))
        .bind(pool.as_ref().map(|p| p.compatibility_digest.as_str()))
        .bind(pool.as_ref().map(|p| p.maximum_concurrency))
        .bind(authority.publication_id)
        .bind(&authority.content_digest)
        .bind(&spec.policy.definition_digest)
        .bind(
            spec.user_id
                .map(|user_id| sha256_digest(user_id.as_bytes())),
        )
        .bind(&spec.model_provider)
        .bind(&spec.model_name)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            let row = sqlx::query("SELECT principal_id,agent_def_id,agent_definition_version,policy_snapshot_id,state,service_pool_id,service_pool_compatibility_digest,service_pool_maximum_concurrency,agent_publication_id,agent_content_digest,agent_definition_digest,user_identity_digest,model_provider,model_name FROM agent_session_t WHERE host_id=$1 AND session_id=$2 FOR UPDATE")
                .bind(spec.host_id).bind(spec.session_id.0).fetch_one(&mut *tx).await?;
            let principal: String = row.try_get("principal_id")?;
            let definition: Uuid = row.try_get("agent_def_id")?;
            let state: String = row.try_get("state")?;
            if principal != spec.principal_id
                || definition != spec.agent_def_id
                || row.try_get::<i64, _>("agent_definition_version")? != spec.definition_version
                || row.try_get::<Uuid, _>("policy_snapshot_id")? != spec.policy.snapshot_id
                || state != "ACTIVE"
                || row.try_get::<Option<Uuid>, _>("service_pool_id")?
                    != pool.as_ref().map(|p| p.pool_id)
                || row.try_get::<Option<String>, _>("service_pool_compatibility_digest")?
                    != pool.as_ref().map(|p| p.compatibility_digest.clone())
                || row.try_get::<Option<i32>, _>("service_pool_maximum_concurrency")?
                    != pool.as_ref().map(|p| p.maximum_concurrency)
                || row.try_get::<Uuid, _>("agent_publication_id")? != authority.publication_id
                || row.try_get::<String, _>("agent_content_digest")? != authority.content_digest
                || row.try_get::<String, _>("agent_definition_digest")?
                    != spec.policy.definition_digest
                || row.try_get::<Option<String>, _>("user_identity_digest")?
                    != spec
                        .user_id
                        .map(|user_id| sha256_digest(user_id.as_bytes()))
                || row.try_get::<String, _>("model_provider")? != spec.model_provider
                || row.try_get::<String, _>("model_name")? != spec.model_name
            {
                bail!("durable agent session ownership or state mismatch");
            }
        }
        pin_reference_evidence(
            &mut tx,
            authority,
            "agent_session_t",
            spec.session_id.0,
            "HOST_SCOPE",
            spec.host_id,
            None,
            Some(authority.publication_id),
            &authority.content_digest,
        )
        .await?;
        pin_reference_evidence(
            &mut tx,
            authority,
            "agent_session_t",
            spec.session_id.0,
            "AGENT_DEFINITION",
            spec.agent_def_id,
            Some(spec.definition_version),
            Some(authority.publication_id),
            &spec.policy.definition_digest,
        )
        .await?;
        pin_reference_evidence(
            &mut tx,
            authority,
            "agent_session_t",
            spec.session_id.0,
            "AGENT_POLICY",
            spec.policy.snapshot_id,
            Some(authority.policy_version),
            Some(authority.publication_id),
            &authority.policy_digest,
        )
        .await?;
        if let Some(user_id) = spec.user_id {
            pin_reference_evidence(
                &mut tx,
                authority,
                "agent_session_t",
                spec.session_id.0,
                "USER_PRINCIPAL",
                user_id,
                None,
                None,
                &sha256_digest(user_id.as_bytes()),
            )
            .await?;
        }
        if let Some(pool) = &pool {
            pin_reference_evidence(
                &mut tx,
                authority,
                "agent_session_t",
                spec.session_id.0,
                "SERVICE_POOL",
                pool.pool_id,
                None,
                Some(authority.publication_id),
                &pool.compatibility_digest,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn bind_session_memory_bank(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        bank_id: Uuid,
    ) -> Result<()> {
        let bound: Option<Uuid> = sqlx::query_scalar(
            "UPDATE agent_session_t
             SET bank_id=$3,
                 session_version=CASE WHEN bank_id IS NULL THEN session_version+1 ELSE session_version END,
                 updated_ts=now()
             WHERE host_id=$1 AND session_id=$2 AND user_id IS NOT NULL
               AND (bank_id IS NULL OR bank_id=$3)
             RETURNING bank_id",
        )
        .bind(host_id)
        .bind(session_id.0)
        .bind(bank_id)
        .fetch_optional(&self.pool)
        .await?;
        if bound != Some(bank_id) {
            bail!("durable agent session memory-bank binding mismatch");
        }
        Ok(())
    }

    pub async fn admit_user_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        client_message_id: &str,
        text: &str,
        model_provider: &str,
        model_name: &str,
        maximum_queued_turns: u64,
        maximum_model_tokens: u64,
    ) -> Result<AdmittedTurn> {
        let mut tx = self.pool.begin().await?;
        let authority = self
            .authority
            .as_ref()
            .context("turn admission requires immutable projection authority")?;
        if authority.host_id != host_id
            || authority.model_provider != model_provider
            || authority.model_name != model_name
        {
            bail!("turn admission does not match the accepted Agent projection")
        }
        let row = sqlx::query(
            "SELECT s.next_turn_sequence,s.next_queue_sequence,s.policy_snapshot_id,
              p.policy_digest,p.data_boundary_digest,p.product_profile_digest,s.maximum_expires_ts,
              s.principal_id,s.agent_def_id,s.agent_definition_version,s.service_pool_id,
              s.agent_publication_id,s.agent_content_digest,s.agent_definition_digest,
              s.model_provider,s.model_name
              FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id
                AND p.policy_snapshot_id=s.policy_snapshot_id AND p.revoked_ts IS NULL
              WHERE s.host_id=$1 AND s.session_id=$2 AND s.state='ACTIVE'
              FOR UPDATE OF s,p",
        )
        .bind(host_id)
        .bind(session_id.0)
        .fetch_optional(&mut *tx)
        .await?
        .context("active agent session not found")?;
        if let Some(existing) = sqlx::query("SELECT turn_id,turn_sequence,policy_digest,data_boundary_digest FROM agent_turn_t WHERE host_id=$1 AND session_id=$2 AND client_message_id=$3")
            .bind(host_id).bind(session_id.0).bind(client_message_id).fetch_optional(&mut *tx).await? {
            tx.commit().await?;
            return Ok(AdmittedTurn { turn_id: AgentTurnId(existing.try_get("turn_id")?), turn_sequence: existing.try_get("turn_sequence")?, duplicate: true, policy_digest: existing.try_get("policy_digest")?, data_boundary_digest: existing.try_get("data_boundary_digest")? });
        }
        let turn_sequence: i64 = row.try_get("next_turn_sequence")?;
        let queue_sequence: i64 = row.try_get("next_queue_sequence")?;
        let policy_snapshot_id: Uuid = row.try_get("policy_snapshot_id")?;
        let policy_digest: String = row.try_get("policy_digest")?;
        let boundary: String = row.try_get("data_boundary_digest")?;
        let maximum: DateTime<Utc> = row.try_get("maximum_expires_ts")?;
        let principal: String = row.try_get("principal_id")?;
        let agent: Uuid = row.try_get("agent_def_id")?;
        let pool: Option<Uuid> = row.try_get("service_pool_id")?;
        let profile: String = row.try_get("product_profile_digest")?;
        if row.try_get::<Uuid, _>("agent_publication_id")? != authority.publication_id
            || row.try_get::<String, _>("agent_content_digest")? != authority.content_digest
            || row.try_get::<String, _>("agent_definition_digest")? != authority.definition_digest
            || row.try_get::<String, _>("model_provider")? != authority.model_provider
            || row.try_get::<String, _>("model_name")? != authority.model_name
            || agent != authority.agent_def_id
            || row.try_get::<i64, _>("agent_definition_version")? != authority.definition_version
            || policy_snapshot_id != authority.policy_snapshot_id
            || policy_digest != authority.policy_digest
        {
            bail!("durable session contains stale Agent projection evidence")
        }
        if maximum_queued_turns == 0
            || maximum_queued_turns > i64::MAX as u64
            || maximum_model_tokens == 0
            || maximum_model_tokens > i64::MAX as u64
        {
            bail!("configured turn limits are invalid")
        }
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("agent-config-queued-turns:{host_id}:{agent}"))
            .execute(&mut *tx)
            .await?;
        let queued: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_turn_t t
             JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
             WHERE t.host_id=$1 AND s.agent_def_id=$2 AND t.state='QUEUED'",
        )
        .bind(host_id)
        .bind(agent)
        .fetch_one(&mut *tx)
        .await?;
        if queued >= maximum_queued_turns as i64 {
            bail!("configured Agent queued-turn limit exceeded")
        }
        if model_provider != GATEWAY_PROVIDER_ID || model_name.trim().is_empty() {
            bail!("turn authority requires a governed llm-gateway alias")
        }
        let now = Utc::now();
        let rate = authority
            .model_rates
            .iter()
            .filter(|rate| {
                rate.enabled
                    && rate.provider == model_provider
                    && rate.model == model_name
                    && rate.effective_at <= now
                    && rate.expires_at.is_none_or(|expires| expires > now)
            })
            .max_by_key(|rate| (rate.effective_at, rate.rate_id));
        let input_rate = rate
            .map(|rate| rate.input_cost_micros_per_million)
            .unwrap_or(0);
        let output_rate = rate
            .map(|rate| rate.output_cost_micros_per_million)
            .unwrap_or(0);
        let turn_id = AgentTurnId::new();
        let token_reservation = maximum_model_tokens as i64;
        let cost_reservation = token_cost_micros(token_reservation, input_rate.max(output_rate));
        enforce_quotas(
            &mut tx,
            &authority.quota_policies,
            host_id,
            &principal,
            agent,
            &profile,
            model_provider,
            pool,
            Some(turn_id.0),
            false,
            token_reservation,
            cost_reservation,
            rate.is_some(),
        )
        .await?;
        let deadline = std::cmp::min(Utc::now() + Duration::minutes(2), maximum);
        sqlx::query("INSERT INTO agent_turn_t (host_id,turn_id,session_id,turn_sequence,queue_sequence,origin_kind,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,cost_budget_micros,quota_input_cost_micros_per_million,quota_output_cost_micros_per_million,deadline_ts,service_pool_id) VALUES ($1,$2,$3,$4,$5,'user',$6,$6,$7,$8,$9,$10,$11,20,$12,$13,$14,$15,$16,$17)")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).bind(turn_sequence).bind(queue_sequence).bind(client_message_id)
            .bind(policy_snapshot_id).bind(&policy_digest).bind(&boundary).bind(model_provider).bind(model_name)
            .bind(token_reservation).bind(cost_reservation).bind(input_rate).bind(output_rate)
            .bind(deadline).bind(pool).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET next_turn_sequence=next_turn_sequence+1,next_queue_sequence=next_queue_sequence+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2")
            .bind(host_id).bind(session_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "user",
            "USER_MESSAGE",
            json!({"text": text}),
            &policy_digest,
        )
        .await?;
        sqlx::query("SELECT pg_notify('agent_turn_queue_v1',$1)")
            .bind(host_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(AdmittedTurn {
            turn_id,
            turn_sequence,
            duplicate: false,
            policy_digest,
            data_boundary_digest: boundary,
        })
    }

    pub async fn activate_next_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
    ) -> Result<Option<AgentTurnId>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("agent-session-dispatch:{host_id}:{}", session_id.0))
            .execute(&mut *tx)
            .await?;
        let turn_id: Option<Uuid> = sqlx::query_scalar(
            "SELECT t.turn_id
             FROM agent_turn_t t
             JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
             WHERE t.host_id=$1 AND t.session_id=$2 AND t.state='QUEUED'
               AND s.state='ACTIVE' AND s.active_turn_id IS NULL
               AND t.service_pool_id IS NOT DISTINCT FROM s.service_pool_id
               AND (t.service_pool_id IS NULL OR (
                 s.service_pool_compatibility_digest IS NOT NULL
                 AND s.service_pool_maximum_concurrency IS NOT NULL AND
                 (SELECT COUNT(*) FROM agent_turn_t running
                  WHERE running.host_id=t.host_id AND running.service_pool_id=t.service_pool_id
                    AND running.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL')) < s.service_pool_maximum_concurrency))
             ORDER BY t.created_ts,t.turn_id
             FOR UPDATE OF t,s SKIP LOCKED LIMIT 1",
        )
        .bind(host_id)
        .bind(session_id.0)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(turn_id) = turn_id else {
            tx.commit().await?;
            return Ok(None);
        };
        let activated = sqlx::query("UPDATE agent_turn_t SET state='RECEIVED',activated_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='QUEUED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        let session = sqlx::query("UPDATE agent_session_t SET active_turn_id=$3,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id IS NULL")
            .bind(host_id).bind(session_id.0).bind(turn_id).execute(&mut *tx).await?;
        if activated.rows_affected() != 1 || session.rows_affected() != 1 {
            bail!("session dispatch lost its turn/session activation fence")
        }
        sqlx::query("UPDATE agent_job_t SET state='RUNNING',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='TURN_CREATED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query("SELECT pg_notify('agent_turn_activated_v1',$1)")
            .bind(turn_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some(AgentTurnId(turn_id)))
    }

    /// Selects one candidate across all sessions using a serialized host-level
    /// dispatch decision. Principals with fewer running turns and the oldest
    /// previous activation win before FIFO creation order is considered.
    pub async fn dispatch_next_turn_fair(
        &self,
        host_id: Uuid,
    ) -> Result<Option<(AgentSessionId, AgentTurnId)>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
            .bind(format!("agent-pool-dispatch:{host_id}"))
            .execute(&mut *tx)
            .await?;
        let candidate = sqlx::query(
            "SELECT t.turn_id,t.session_id
             FROM agent_turn_t t
             JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
             WHERE t.host_id=$1 AND t.state='QUEUED' AND s.state='ACTIVE'
               AND s.active_turn_id IS NULL
               AND t.service_pool_id IS NOT DISTINCT FROM s.service_pool_id
               AND (t.service_pool_id IS NULL OR (
                 s.service_pool_compatibility_digest IS NOT NULL
                 AND s.service_pool_maximum_concurrency IS NOT NULL AND
                 (SELECT COUNT(*) FROM agent_turn_t running
                  WHERE running.host_id=t.host_id AND running.service_pool_id=t.service_pool_id
                    AND running.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL')) < s.service_pool_maximum_concurrency))
             ORDER BY
               (SELECT COUNT(*) FROM agent_turn_t running JOIN agent_session_t rs
                  ON rs.host_id=running.host_id AND rs.session_id=running.session_id
                WHERE running.host_id=t.host_id AND rs.principal_id=s.principal_id
                  AND running.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL')),
               COALESCE((SELECT MAX(previous.activated_ts) FROM agent_turn_t previous
                 JOIN agent_session_t ps ON ps.host_id=previous.host_id AND ps.session_id=previous.session_id
                 WHERE previous.host_id=t.host_id AND ps.principal_id=s.principal_id),to_timestamp(0)),
               t.created_ts,t.turn_id
             FOR UPDATE OF t,s SKIP LOCKED LIMIT 1",
        )
        .bind(host_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(candidate) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        let turn_id: Uuid = candidate.try_get("turn_id")?;
        let session_id: Uuid = candidate.try_get("session_id")?;
        let activated = sqlx::query("UPDATE agent_turn_t SET state='RECEIVED',activated_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='QUEUED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        let session = sqlx::query("UPDATE agent_session_t SET active_turn_id=$3,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id IS NULL")
            .bind(host_id).bind(session_id).bind(turn_id).execute(&mut *tx).await?;
        if activated.rows_affected() != 1 || session.rows_affected() != 1 {
            bail!("fair dispatch lost its turn/session activation fence")
        }
        sqlx::query("UPDATE agent_job_t SET state='RUNNING',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='TURN_CREATED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query("SELECT pg_notify('agent_turn_activated_v1',$1)")
            .bind(turn_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(Some((AgentSessionId(session_id), AgentTurnId(turn_id))))
    }

    pub async fn resolve_turn_runtime(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
    ) -> Result<TurnRuntimeResolution> {
        let row = sqlx::query(
            "SELECT t.session_id,t.policy_digest,t.data_boundary_digest,t.model_provider,t.model_name,
                    t.service_pool_id,s.agent_def_id,s.agent_definition_version,
                    s.service_pool_compatibility_digest,p.product_profile_digest,
                    s.agent_publication_id,s.agent_content_digest,s.agent_definition_digest
             FROM agent_turn_t t JOIN agent_session_t s
               ON s.host_id=t.host_id AND s.session_id=t.session_id AND s.active_turn_id=t.turn_id
             JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id
               AND p.policy_snapshot_id=t.policy_snapshot_id AND p.policy_digest=t.policy_digest
               AND p.revoked_ts IS NULL
             WHERE t.host_id=$1 AND t.turn_id=$2 AND t.state='RECEIVED'
               AND t.service_pool_id IS NOT DISTINCT FROM s.service_pool_id
               AND (t.service_pool_id IS NULL OR
                    (s.service_pool_compatibility_digest IS NOT NULL
                     AND s.service_pool_maximum_concurrency IS NOT NULL))",
        )
        .bind(host_id)
        .bind(turn_id.0)
        .fetch_one(&self.pool)
        .await?;
        let resolution = TurnRuntimeResolution {
            host_id,
            turn_id,
            session_id: AgentSessionId(row.try_get("session_id")?),
            agent_def_id: row.try_get("agent_def_id")?,
            definition_version: row.try_get("agent_definition_version")?,
            policy_digest: row.try_get("policy_digest")?,
            data_boundary_digest: row.try_get("data_boundary_digest")?,
            product_profile_digest: row.try_get("product_profile_digest")?,
            model_provider: row.try_get("model_provider")?,
            model_name: row.try_get("model_name")?,
            service_pool_id: row.try_get("service_pool_id")?,
            service_pool_compatibility_digest: row.try_get("service_pool_compatibility_digest")?,
        };
        let authority = self
            .authority
            .as_ref()
            .context("turn runtime resolution requires immutable projection authority")?;
        if resolution.host_id != authority.host_id
            || resolution.agent_def_id != authority.agent_def_id
            || resolution.definition_version != authority.definition_version
            || resolution.policy_digest != authority.policy_digest
            || row.try_get::<Uuid, _>("agent_publication_id")? != authority.publication_id
            || row.try_get::<String, _>("agent_content_digest")? != authority.content_digest
            || row.try_get::<String, _>("agent_definition_digest")? != authority.definition_digest
        {
            bail!("turn runtime contains stale Agent projection evidence")
        }
        if resolution.model_provider.trim().is_empty() || resolution.model_name.trim().is_empty() {
            bail!("turn has no immutable model provider/runtime binding")
        }
        Ok(resolution)
    }

    pub async fn propose_gateway_action(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        stable_tool_ref: Uuid,
        model_alias: &str,
        arguments: &str,
    ) -> Result<(Uuid, Uuid)> {
        let mut tx = self.pool.begin().await?;
        let policy: String = sqlx::query_scalar("SELECT policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 AND state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION') FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let logical_action_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let tool_ref = stable_tool_ref;
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,gateway_request_id) VALUES($1,$2,$3,$4,1,$5,$6,'gateway',$7,$8,$9,'unknown','DISPATCHED',$10)")
            .bind(host_id).bind(attempt_id).bind(turn_id.0).bind(logical_action_id).bind(tool_ref).bind(model_alias)
            .bind(sha256_digest(model_alias.as_bytes())).bind(&policy).bind(sha256_digest(arguments.as_bytes())).bind(Uuid::now_v7()).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        let session_id = session_id_for_turn(&mut tx, host_id, turn_id.0).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(attempt_id),
            "agent",
            "ACTION_DISPATCHED",
            json!({"modelAlias":model_alias,"placement":"gateway"}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok((attempt_id, tool_ref))
    }

    pub async fn accept_gateway_result(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        action_attempt_id: Uuid,
        succeeded: bool,
        result: Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 FOR UPDATE OF a,t")
            .bind(host_id).bind(action_attempt_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(());
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(action_attempt_id),
            "gateway",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(result.clone()).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_MODEL',updated_ts=now(),terminal_error=CASE WHEN $3 THEN terminal_error ELSE $4 END WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(succeeded).bind((!succeeded).then_some(result)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_approval(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        logical_action_id: Uuid,
        input_digest: &str,
        subject_digest: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT session_id,policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let approval_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_approval_t(host_id,approval_id,turn_id,logical_action_id,subject_digest,input_digest,policy_digest,approver_scope,nonce_digest,expires_ts) VALUES($1,$2,$3,$4,$5,$6,$7,'{}',$8,$9)")
            .bind(host_id).bind(approval_id).bind(turn_id.0).bind(logical_action_id).bind(subject_digest).bind(input_digest).bind(&policy).bind(sha256_digest(Uuid::now_v7().as_bytes())).bind(expires_at).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_APPROVAL',updated_ts=now() WHERE host_id=$1 AND turn_id=$2").bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            None,
            "agent",
            "APPROVAL_REQUESTED",
            json!({"approvalId":approval_id,"logicalActionId":logical_action_id}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok(approval_id)
    }

    pub async fn approve_and_create_fresh_attempt(
        &self,
        host_id: Uuid,
        approval_id: Uuid,
        actor: &str,
        stable_tool_ref: Uuid,
        model_alias: &str,
        placement: &str,
        schema_digest: &str,
        argument_digest: &str,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.turn_id,a.logical_action_id,a.policy_digest,t.session_id FROM agent_approval_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.approval_id=$2 AND a.state='REQUESTED' AND a.expires_ts>now() FOR UPDATE OF a,t")
            .bind(host_id).bind(approval_id).fetch_optional(&mut *tx).await?.context("approval is unavailable or expired")?;
        let turn_id: Uuid = row.try_get("turn_id")?;
        let logical: Uuid = row.try_get("logical_action_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let session: Uuid = row.try_get("session_id")?;
        let attempt_number: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(attempt_number),0)+1 FROM agent_action_attempt_t WHERE host_id=$1 AND turn_id=$2 AND logical_action_id=$3").bind(host_id).bind(turn_id).bind(logical).fetch_one(&mut *tx).await?;
        let attempt_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,approval_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'unknown','READY',$12)")
            .bind(host_id).bind(attempt_id).bind(turn_id).bind(logical).bind(attempt_number).bind(stable_tool_ref).bind(model_alias).bind(placement).bind(schema_digest).bind(&policy).bind(argument_digest).bind(approval_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_approval_t SET state='APPROVED',decision_actor=$3,decision_ts=now(),consumed_action_attempt_id=$4 WHERE host_id=$1 AND approval_id=$2 AND state='REQUESTED'").bind(host_id).bind(approval_id).bind(actor).bind(attempt_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='WAITING_APPROVAL'").bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session,Some(turn_id),Some(attempt_id),"approver","APPROVAL_GRANTED",json!({"approvalId":approval_id,"freshAttempt":attempt_id,"attemptNumber":attempt_number}),&policy).await?;
        tx.commit().await?;
        Ok(attempt_id)
    }

    pub async fn complete_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        response: &str,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        knowledge_evidence: Option<&Value>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT policy_digest,quota_input_cost_micros_per_million,
                    quota_output_cost_micros_per_million
             FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE",
        )
        .bind(host_id)
        .bind(turn_id.0)
        .fetch_one(&mut *tx)
        .await?;
        let policy: String = row.try_get("policy_digest")?;
        let input_rate: i64 = row.try_get("quota_input_cost_micros_per_million")?;
        let output_rate: i64 = row.try_get("quota_output_cost_micros_per_million")?;
        let settlement = match (input_tokens, output_tokens) {
            (Some(input_tokens), Some(output_tokens)) => {
                let input_tokens = input_tokens.max(0);
                let output_tokens = output_tokens.max(0);
                let actual_tokens = input_tokens.saturating_add(output_tokens);
                let actual_cost_micros = token_cost_micros(input_tokens, input_rate)
                    .saturating_add(token_cost_micros(output_tokens, output_rate));
                let evidence_digest = execution_runner_protocol::canonical_sha256(&json!({
                    "turnId": turn_id.0,
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "inputRateMicrosPerMillion": input_rate,
                    "outputRateMicrosPerMillion": output_rate
                }))?;
                QuotaSettlement::Trusted {
                    tokens: actual_tokens,
                    cost_micros: actual_cost_micros,
                    source: "trusted-provider",
                    evidence_digest,
                }
            }
            _ => QuotaSettlement::ReservationCeiling,
        };
        if let Some(evidence) = knowledge_evidence {
            append_event(
                &mut tx,
                host_id,
                session_id.0,
                Some(turn_id.0),
                None,
                "knowledge",
                "KNOWLEDGE_EVIDENCE",
                evidence.clone(),
                &policy,
            )
            .await?;
        }
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "model",
            "MODEL_RESULT",
            json!({"text":response}),
            &policy,
        )
        .await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "system",
            "TURN_COMPLETED",
            json!({}),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET state='COMPLETED',terminal_result=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"text":response})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id.0, &settlement).await?;
        sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
            .bind(host_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn fail_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        reason: &str,
    ) -> Result<()> {
        self.fail_turn_with_settlement(
            host_id,
            session_id,
            turn_id,
            reason,
            QuotaSettlement::Release,
        )
        .await
    }

    pub async fn fail_turn_after_model_dispatch(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        reason: &str,
    ) -> Result<()> {
        self.fail_turn_with_settlement(
            host_id,
            session_id,
            turn_id,
            reason,
            QuotaSettlement::ReservationCeiling,
        )
        .await
    }

    pub async fn cancel_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        actor: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let policy: Option<String> = sqlx::query_scalar(
            "UPDATE agent_turn_t SET state='CANCELLED',terminal_error=$4,terminal_ts=now(),
                    updated_ts=now()
              WHERE host_id=$1 AND session_id=$2 AND turn_id=$3
                AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')
              RETURNING policy_digest",
        )
        .bind(host_id)
        .bind(session_id.0)
        .bind(turn_id.0)
        .bind(json!({"actor": actor, "reason": "A2A cancellation"}))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(policy) = policy else {
            bail!("agent turn is not cancellable")
        };
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            actor,
            "TURN_CANCELLED",
            json!({"source":"a2a"}),
            &policy,
        )
        .await?;
        sqlx::query(
            "UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,
                    updated_ts=now()
              WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3",
        )
        .bind(host_id)
        .bind(session_id.0)
        .bind(turn_id.0)
        .execute(&mut *tx)
        .await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id.0, &QuotaSettlement::Release).await?;
        sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
            .bind(host_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn fail_turn_with_settlement(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        reason: &str,
        settlement: QuotaSettlement,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_turn_t SET state='FAILED',terminal_error=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"message":reason})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        reconcile_turn_quota_usage(&mut tx, host_id, turn_id.0, &settlement).await?;
        sqlx::query("SELECT pg_notify('agent_turn_capacity_v1',$1)")
            .bind(host_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn rebuild_history_projection(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        bank_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let projection_sequence: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(event_sequence),0) FROM agent_session_event_t
             WHERE host_id=$1 AND session_id=$2",
        )
        .bind(host_id)
        .bind(session_id.0)
        .fetch_one(&mut *tx)
        .await?;
        let events = sqlx::query(
            "SELECT e.event_type,e.content
            FROM agent_session_event_t e
            JOIN agent_turn_t t ON t.host_id=e.host_id AND t.turn_id=e.turn_id
            WHERE e.host_id=$1 AND e.session_id=$2
              AND e.event_type IN ('USER_MESSAGE','MODEL_RESULT')
            ORDER BY t.turn_sequence,
              CASE e.event_type WHEN 'USER_MESSAGE' THEN 0 ELSE 1 END,
              e.event_sequence",
        )
        .bind(host_id)
        .bind(session_id.0)
        .fetch_all(&mut *tx)
        .await?;
        let mut messages = Vec::with_capacity(events.len());
        for event in events {
            let kind: String = event.try_get("event_type")?;
            let content: Value = event.try_get("content")?;
            messages.push(json!({"role": if kind == "USER_MESSAGE" {"user"} else {"assistant"}, "content": content.get("text").cloned().unwrap_or(Value::Null)}));
        }
        sqlx::query("INSERT INTO agent_session_history_t(host_id,bank_id,session_id,durable_session_id,messages,projection_sequence) VALUES($1,$2,$3,$3,$4,$5) ON CONFLICT(host_id,bank_id,session_id) DO UPDATE SET messages=EXCLUDED.messages,durable_session_id=EXCLUDED.durable_session_id,projection_sequence=EXCLUDED.projection_sequence,aggregate_version=agent_session_history_t.aggregate_version+1,update_ts=now() WHERE agent_session_history_t.projection_sequence < EXCLUDED.projection_sequence")
            .bind(host_id).bind(bank_id).bind(session_id.0).bind(Value::Array(messages)).bind(projection_sequence).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn persist_policy(
    tx: &mut Transaction<'_, Postgres>,
    authority: &AgentRuntimeAuthority,
    policy: &PolicySnapshot,
) -> Result<()> {
    let value = serde_json::to_value(policy)?;
    let digest = policy_document_digest(policy)?;
    let inserted = sqlx::query("INSERT INTO agent_policy_snapshot_t(host_id,policy_snapshot_id,agent_def_id,agent_definition_version,agent_publication_id,agent_content_digest,definition_digest,product_profile_digest,model_digest,catalog_digest,memory_digest,execution_digest,channel_digest,data_boundary_digest,resolved_snapshot,policy_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16) ON CONFLICT(host_id,policy_snapshot_id) DO NOTHING")
        .bind(authority.host_id).bind(policy.snapshot_id).bind(authority.agent_def_id).bind(authority.definition_version).bind(authority.publication_id).bind(&authority.content_digest).bind(&policy.definition_digest).bind(&policy.product_profile_digest).bind(&policy.model_digest).bind(&policy.catalog_digest).bind(&policy.memory_digest).bind(&policy.execution_digest).bind(&policy.channel_digest).bind(&policy.data_boundary_digest).bind(value).bind(digest).execute(&mut **tx).await?;
    if inserted.rows_affected() == 0 {
        let matches: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_policy_snapshot_t
             WHERE host_id=$1 AND policy_snapshot_id=$2 AND agent_def_id=$3
               AND agent_definition_version=$4 AND agent_publication_id=$5
               AND agent_content_digest=$6 AND revoked_ts IS NULL AND definition_digest=$7
               AND product_profile_digest=$8 AND model_digest=$9
               AND catalog_digest=$10 AND memory_digest=$11
               AND execution_digest=$12 AND channel_digest=$13
               AND data_boundary_digest=$14 AND resolved_snapshot=$15
               AND policy_digest=$16)",
        )
        .bind(authority.host_id)
        .bind(policy.snapshot_id)
        .bind(authority.agent_def_id)
        .bind(authority.definition_version)
        .bind(authority.publication_id)
        .bind(&authority.content_digest)
        .bind(&policy.definition_digest)
        .bind(&policy.product_profile_digest)
        .bind(&policy.model_digest)
        .bind(&policy.catalog_digest)
        .bind(&policy.memory_digest)
        .bind(&policy.execution_digest)
        .bind(&policy.channel_digest)
        .bind(&policy.data_boundary_digest)
        .bind(serde_json::to_value(policy)?)
        .bind(policy_document_digest(policy)?)
        .fetch_one(&mut **tx)
        .await?;
        if !matches {
            bail!("policy snapshot identifier is already bound to different or revoked authority")
        }
    }
    Ok(())
}

async fn persist_runtime_scope(
    tx: &mut Transaction<'_, Postgres>,
    authority: &AgentRuntimeAuthority,
) -> Result<()> {
    let persisted: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO runtime_operational_scope_t(
             host_id,environment,service_id,instance_id,publication_id,
             content_digest,audience,active,last_seen_ts)
         VALUES($1,$2,$3,$4,$5,$6,'agent',TRUE,now())
         ON CONFLICT(host_id,service_id,instance_id) DO UPDATE
            SET last_seen_ts=now()
          WHERE runtime_operational_scope_t.environment=EXCLUDED.environment
            AND runtime_operational_scope_t.publication_id=EXCLUDED.publication_id
            AND runtime_operational_scope_t.content_digest=EXCLUDED.content_digest
            AND runtime_operational_scope_t.audience=EXCLUDED.audience
            AND runtime_operational_scope_t.active
         RETURNING instance_id",
    )
    .bind(authority.host_id)
    .bind(&authority.environment)
    .bind(&authority.service_id)
    .bind(authority.instance_id)
    .bind(authority.publication_id)
    .bind(&authority.content_digest)
    .fetch_optional(&mut **tx)
    .await?;
    if persisted != Some(authority.instance_id) {
        bail!("runtime operational scope is missing, inactive, or stale")
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn pin_reference_evidence(
    tx: &mut Transaction<'_, Postgres>,
    authority: &AgentRuntimeAuthority,
    source_table: &str,
    source_record_id: Uuid,
    reference_kind: &str,
    target_id: Uuid,
    target_version: Option<i64>,
    publication_id: Option<Uuid>,
    content_digest: &str,
) -> Result<()> {
    let pinned: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO operational_reference_evidence_t(
             host_id,reference_id,source_service,source_table,source_record_id,
             reference_kind,target_id,target_version,publication_id,content_digest,
             issuer,audience,state,accepted_ts,reconciled_ts)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$3,'agent','ACCEPTED',now(),now())
         ON CONFLICT(host_id,source_service,source_table,source_record_id,reference_kind)
         DO UPDATE SET reconciled_ts=now()
          WHERE operational_reference_evidence_t.target_id=EXCLUDED.target_id
            AND operational_reference_evidence_t.target_version IS NOT DISTINCT FROM EXCLUDED.target_version
            AND operational_reference_evidence_t.publication_id IS NOT DISTINCT FROM EXCLUDED.publication_id
            AND operational_reference_evidence_t.content_digest=EXCLUDED.content_digest
            AND operational_reference_evidence_t.issuer=EXCLUDED.issuer
            AND operational_reference_evidence_t.audience=EXCLUDED.audience
            AND operational_reference_evidence_t.state='ACCEPTED'
         RETURNING reference_id",
    )
    .bind(authority.host_id)
    .bind(Uuid::now_v7())
    .bind(&authority.service_id)
    .bind(source_table)
    .bind(source_record_id)
    .bind(reference_kind)
    .bind(target_id)
    .bind(target_version)
    .bind(publication_id)
    .bind(content_digest)
    .fetch_optional(&mut **tx)
    .await?;
    if pinned.is_none() {
        bail!("operational reference evidence is missing, revoked, or stale")
    }
    Ok(())
}

fn policy_document_digest(policy: &PolicySnapshot) -> Result<String> {
    // This byte representation is the deployed Portal/Agent contract. Do not
    // switch algorithms without a coordinated snapshot backfill.
    Ok(sha256_digest(&serde_json::to_vec(policy)?))
}

async fn session_id_for_turn(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    turn_id: Uuid,
) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT session_id FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id)
            .bind(turn_id)
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    session_id: Uuid,
    turn_id: Option<Uuid>,
    action_attempt_id: Option<Uuid>,
    actor: &str,
    kind: &str,
    content: Value,
    policy_digest: &str,
) -> Result<()> {
    let digest = sha256_digest(&serde_json::to_vec(&content)?);
    sqlx::query("INSERT INTO agent_session_event_t(host_id,event_id,session_id,event_sequence,turn_id,action_attempt_id,actor_class,event_type,content,content_digest,policy_digest) SELECT $1,$2,$3,COALESCE(MAX(event_sequence),0)+1,$4,$5,$6,$7,$8,$9,$10 FROM agent_session_event_t WHERE host_id=$1 AND session_id=$3")
        .bind(host_id).bind(Uuid::now_v7()).bind(session_id).bind(turn_id).bind(action_attempt_id).bind(actor).bind(kind).bind(content).bind(digest).bind(policy_digest).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn published_policy_digest_preserves_deployed_serialization_contract() {
        let policy = PolicySnapshot {
            snapshot_id: Uuid::nil(),
            definition_digest: sha256_digest(b"definition"),
            product_profile_digest: sha256_digest(b"profile"),
            model_digest: sha256_digest(b"model"),
            catalog_digest: sha256_digest(b"catalog"),
            memory_digest: sha256_digest(b"memory"),
            execution_digest: sha256_digest(b"execution"),
            channel_digest: sha256_digest(b"channel"),
            data_boundary_digest: sha256_digest(b"boundary"),
            tools: BTreeMap::new(),
        };
        let reordered = json!({
            "tools": {},
            "snapshotId": Uuid::nil(),
            "modelDigest": policy.model_digest.clone(),
            "memoryDigest": policy.memory_digest.clone(),
            "executionDigest": policy.execution_digest.clone(),
            "definitionDigest": policy.definition_digest.clone(),
            "dataBoundaryDigest": policy.data_boundary_digest.clone(),
            "catalogDigest": policy.catalog_digest.clone(),
            "channelDigest": policy.channel_digest.clone(),
            "productProfileDigest": policy.product_profile_digest.clone()
        });
        let decoded: PolicySnapshot = serde_json::from_value(reordered).unwrap();
        assert_eq!(
            policy_document_digest(&policy).unwrap(),
            policy_document_digest(&decoded).unwrap()
        );
        assert_eq!(
            policy_document_digest(&policy).unwrap(),
            "sha256:d37c0b58effd3deb733916bdd88162cfdb53165aa8d170786e36c0eb4a043e2e"
        );
        let sorted_value_digest =
            sha256_digest(&serde_json::to_vec(&serde_json::to_value(&policy).unwrap()).unwrap());
        assert_eq!(
            sorted_value_digest,
            "sha256:113e4df29f38520a8e7bb6c8a799761d4cb7dad2ab9204a17238d62f44b403d7"
        );
        assert_ne!(
            policy_document_digest(&policy).unwrap(),
            sorted_value_digest
        );
    }

    #[test]
    fn edge_action_arguments_fail_closed_against_server_schema() {
        let schema = json!({
            "type":"object",
            "properties":{
                "device":{"type":"string","enum":["desk-lamp"]},
                "level":{"type":"integer","minimum":0,"maximum":100}
            },
            "required":["device","level"],
            "additionalProperties":false
        });
        validate_edge_arguments("$", &schema, &json!({"device":"desk-lamp","level":50})).unwrap();
        assert!(
            validate_edge_arguments("$", &schema, &json!({"device":"front-door","level":50}))
                .is_err()
        );
        assert!(
            validate_edge_arguments("$", &schema, &json!({"device":"desk-lamp","level":101}))
                .is_err()
        );
        assert!(
            validate_edge_arguments(
                "$",
                &schema,
                &json!({"device":"desk-lamp","level":50,"shell":"sh"})
            )
            .is_err()
        );
        assert!(
            validate_edge_arguments("$", &json!({"type":"object","oneOf":[]}), &json!({})).is_err()
        );
    }

    #[test]
    fn trusted_quota_usage_requires_runner_owned_evidence_and_rounds_cost_up() {
        assert_eq!(token_cost_micros(1, 1), 1);
        assert_eq!(token_cost_micros(1_000_000, 250), 250);
        assert!(
            trusted_runner_quota_settlement(&json!({
                "usage":{"totalTokens":1,"costMicros":1}
            }))
            .is_none()
        );
        let settlement = trusted_runner_quota_settlement(&json!({
            "executionId":Uuid::now_v7(),
            "evidence":{
                "trustedBrokerConsumedRequests":"2",
                "trustedBrokerConsumedTokens":"101",
                "trustedBrokerConsumedCostMicros":"7"
            }
        }))
        .unwrap();
        assert!(matches!(
            settlement,
            QuotaSettlement::Trusted {
                tokens: 101,
                cost_micros: 7,
                source: "runner-broker",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn durable_admission_is_idempotent_fifo_and_projection_rebuildable() {
        let Ok(url) = std::env::var("LIGHT_AGENT_TEST_DATABASE_URL") else {
            return;
        };
        let use_agent_ops = std::env::var("LIGHT_AGENT_TEST_SCHEMA").as_deref() == Ok("agent_ops");
        let pool = sqlx::postgres::PgPoolOptions::new()
            .after_connect(move |connection, _metadata| {
                Box::pin(async move {
                    if use_agent_ops {
                        sqlx::query("SET search_path TO agent_ops, operational_meta")
                            .execute(connection)
                            .await?;
                    }
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        let host_id = Uuid::now_v7();
        let agent_def_id = Uuid::now_v7();
        let session = AgentSessionId::new();
        let principal_id = Uuid::now_v7();
        let digest = |name: &str| sha256_digest(name.as_bytes());
        let policy = PolicySnapshot {
            snapshot_id: session.0,
            definition_digest: digest("definition"),
            product_profile_digest: digest("profile"),
            model_digest: digest("model"),
            catalog_digest: digest("catalog"),
            memory_digest: digest("memory"),
            execution_digest: digest("execution"),
            channel_digest: digest("channel"),
            data_boundary_digest: digest("boundary"),
            tools: BTreeMap::new(),
        };
        let pool_dimensions = json!({
            "tenant": host_id.to_string(),
            "identity": "phase4-test-identity",
            "modelCredential": "phase4-test-credential",
            "region": "local",
            "dataBoundary": policy.data_boundary_digest.clone(),
            "network": "private",
            "retention": "development",
            "profile": policy.product_profile_digest.clone()
        });
        let pool_digest = execution_runner_protocol::canonical_sha256(&pool_dimensions).unwrap();
        let service_pool = AgentServicePoolPolicy {
            pool_id: Uuid::now_v7(),
            compatibility_dimensions: pool_dimensions,
            compatibility_digest: pool_digest,
            maximum_concurrency: 10,
            enabled: true,
        };
        let quota = AgentQuotaPolicy {
            quota_id: Uuid::now_v7(),
            policy_version: 1,
            policy_digest: digest("quota-policy"),
            scope_kind: "HOST".to_string(),
            scope_key: host_id.to_string(),
            maximum_active_sessions: Some(10),
            maximum_queued_turns: Some(10),
            maximum_running_turns: Some(10),
            token_budget_per_window: Some(1_000_000),
            cost_budget_micros_per_window: Some(1_000_000),
            window_seconds: 3_600,
            enabled: true,
        };
        let model_rate = AgentModelRatePolicy {
            rate_id: Uuid::now_v7(),
            provider: GATEWAY_PROVIDER_ID.to_string(),
            model: "mock".to_string(),
            input_cost_micros_per_million: 100,
            output_cost_micros_per_million: 200,
            effective_at: Utc::now() - Duration::minutes(1),
            expires_at: None,
            aggregate_version: 1,
            digest: digest("model-rate"),
            enabled: true,
        };
        let authority = AgentRuntimeAuthority {
            host_id,
            agent_def_id,
            definition_version: 1,
            publication_id: Uuid::now_v7(),
            content_digest: digest("content"),
            definition_digest: policy.definition_digest.clone(),
            environment: "test".to_string(),
            service_id: "agent-test".to_string(),
            instance_id: Uuid::now_v7(),
            policy_snapshot_id: policy.snapshot_id,
            policy_version: 1,
            policy_digest: policy_document_digest(&policy).unwrap(),
            data_boundary_digest: policy.data_boundary_digest.clone(),
            model_provider: GATEWAY_PROVIDER_ID.to_string(),
            model_name: "mock".to_string(),
            quota_policies: vec![quota],
            model_rates: vec![model_rate],
            service_pools: vec![service_pool],
            edge_runner_bindings: vec![],
        };
        let repository = AgentRepository::with_authority(pool.clone(), authority.clone());
        let wrong_policy_repository = AgentRepository::with_authority(
            pool.clone(),
            AgentRuntimeAuthority {
                policy_digest: digest("wrong-policy"),
                ..authority.clone()
            },
        );
        assert!(
            wrong_policy_repository
                .create_or_resume_session(&SessionSpec {
                    host_id,
                    session_id: AgentSessionId::new(),
                    principal_id: principal_id.to_string(),
                    user_id: Some(principal_id),
                    agent_def_id,
                    definition_version: 1,
                    model_provider: GATEWAY_PROVIDER_ID.to_string(),
                    model_name: "mock".to_string(),
                    maximum_active_sessions: 10,
                    bank_id: None,
                    policy: policy.clone(),
                    idle_expires_at: Utc::now() + Duration::hours(1),
                    maximum_expires_at: Utc::now() + Duration::hours(2),
                    resume_handle_digest: digest("wrong-policy-session"),
                })
                .await
                .is_err()
        );
        let wrong_pool_repository = AgentRepository::with_authority(
            pool.clone(),
            AgentRuntimeAuthority {
                service_pools: vec![AgentServicePoolPolicy {
                    compatibility_digest: digest("wrong-pool"),
                    ..authority.service_pools[0].clone()
                }],
                ..authority.clone()
            },
        );
        assert!(
            wrong_pool_repository
                .create_or_resume_session(&SessionSpec {
                    host_id,
                    session_id: AgentSessionId::new(),
                    principal_id: principal_id.to_string(),
                    user_id: Some(principal_id),
                    agent_def_id,
                    definition_version: 1,
                    model_provider: GATEWAY_PROVIDER_ID.to_string(),
                    model_name: "mock".to_string(),
                    maximum_active_sessions: 10,
                    bank_id: None,
                    policy: policy.clone(),
                    idle_expires_at: Utc::now() + Duration::hours(1),
                    maximum_expires_at: Utc::now() + Duration::hours(2),
                    resume_handle_digest: digest("wrong-pool-session"),
                })
                .await
                .is_err()
        );
        repository
            .create_or_resume_session(&SessionSpec {
                host_id,
                session_id: session,
                principal_id: principal_id.to_string(),
                user_id: Some(principal_id),
                agent_def_id,
                definition_version: 1,
                model_provider: GATEWAY_PROVIDER_ID.to_string(),
                model_name: "mock".to_string(),
                maximum_active_sessions: 10,
                bank_id: None,
                policy: policy.clone(),
                idle_expires_at: Utc::now() + Duration::hours(1),
                maximum_expires_at: Utc::now() + Duration::hours(2),
                resume_handle_digest: digest(&session.to_string()),
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_memory_bank_t(host_id,bank_id,agent_def_id,user_id,bank_name) VALUES($1,$2,$3,$4,'test-history')")
            .bind(host_id).bind(session.0).bind(agent_def_id).bind(principal_id)
            .execute(&pool).await.unwrap();
        repository
            .bind_session_memory_bank(host_id, session, session.0)
            .await
            .unwrap();
        let persisted_bank: Option<Uuid> = sqlx::query_scalar(
            "SELECT bank_id FROM agent_session_t WHERE host_id=$1 AND session_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(persisted_bank, Some(session.0));
        assert!(
            repository
                .bind_session_memory_bank(host_id, session, Uuid::now_v7())
                .await
                .is_err()
        );
        let foreign_job = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_job_t(host_id,job_id,workflow_process_id,workflow_task_id,agent_def_id,
                idempotency_key,input,input_schema_digest,output_schema,policy_digest,data_boundary_digest,
                deadline_ts,token_budget,cost_budget_micros,delegation_depth,state,created_ts)
                VALUES($1,$2,$3,$4,$5,$6,'{}'::jsonb,$7,'{}'::jsonb,$8,$9,$10,1000,1000,0,'PENDING',now()-interval '1 minute')")
            .bind(host_id)
            .bind(foreign_job)
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(format!("foreign-workflow-{foreign_job}"))
            .bind(digest("foreign-input-schema"))
            .bind(digest("foreign-policy"))
            .bind(digest("foreign-boundary"))
            .bind(Utc::now() + Duration::hours(1))
            .execute(&pool)
            .await
            .unwrap();
        let workflow_job = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_job_t(host_id,job_id,workflow_process_id,workflow_task_id,agent_def_id,
                idempotency_key,input,input_schema_digest,output_schema,policy_digest,data_boundary_digest,
                deadline_ts,token_budget,cost_budget_micros,delegation_depth,state)
                VALUES($1,$2,$3,$4,$5,$6,'{}'::jsonb,$7,'{}'::jsonb,$8,$9,$10,1000,1000,0,'PENDING')")
            .bind(host_id)
            .bind(workflow_job)
            .bind(Uuid::now_v7())
            .bind(Uuid::now_v7())
            .bind(agent_def_id)
            .bind(format!("workflow-bankless-{workflow_job}"))
            .bind(digest("workflow-input-schema"))
            .bind(policy_document_digest(&policy).unwrap())
            .bind(&policy.data_boundary_digest)
            .bind(Utc::now() + Duration::hours(1))
            .execute(&pool)
            .await
            .unwrap();
        assert!(repository.reconcile_agent_jobs().await.unwrap() > 0);
        assert!(
            repository
                .request_job_cancellation(host_id, workflow_job)
                .await
                .unwrap()
        );
        let cancellation_requested: bool = sqlx::query_scalar(
            "SELECT cancellation_requested_ts IS NOT NULL FROM agent_job_t WHERE host_id=$1 AND job_id=$2",
        )
        .bind(host_id)
        .bind(workflow_job)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(cancellation_requested);
        let foreign_state: String =
            sqlx::query_scalar("SELECT state FROM agent_job_t WHERE host_id=$1 AND job_id=$2")
                .bind(host_id)
                .bind(foreign_job)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(foreign_state, "PENDING");
        let workflow_bank: Option<Uuid> = sqlx::query_scalar(
            "SELECT bank_id FROM agent_session_t WHERE host_id=$1 AND session_id=$2",
        )
        .bind(host_id)
        .bind(workflow_job)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            workflow_bank, None,
            "workflow-job admission must remain bankless"
        );
        let first = repository
            .admit_user_turn(
                host_id,
                session,
                "message-1",
                "hello",
                GATEWAY_PROVIDER_ID,
                "mock",
                10,
                1_000,
            )
            .await
            .unwrap();
        let duplicate = repository
            .admit_user_turn(
                host_id,
                session,
                "message-1",
                "hello",
                GATEWAY_PROVIDER_ID,
                "mock",
                10,
                1_000,
            )
            .await
            .unwrap();
        let second = repository
            .admit_user_turn(
                host_id,
                session,
                "message-2",
                "again",
                GATEWAY_PROVIDER_ID,
                "mock",
                10,
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert!(duplicate.duplicate);
        assert!(second.turn_sequence > first.turn_sequence);
        let quota_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agent_quota_usage_t WHERE host_id=$1 AND quota_id=$2",
        )
        .bind(host_id)
        .bind(authority.quota_policies[0].quota_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(quota_rows, 1);
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(first.turn_id)
        );
        let runtime = repository
            .resolve_turn_runtime(host_id, first.turn_id)
            .await
            .unwrap();
        assert_eq!(runtime.agent_def_id, agent_def_id);
        assert_eq!(runtime.model_provider, GATEWAY_PROVIDER_ID);
        assert_eq!(runtime.model_name, "mock");
        repository
            .complete_turn(
                host_id,
                session,
                first.turn_id,
                "world",
                Some(1),
                Some(0),
                None,
            )
            .await
            .unwrap();
        repository
            .rebuild_history_projection(host_id, session, session.0)
            .await
            .unwrap();
        let projection = sqlx::query(
            "SELECT messages,projection_sequence FROM agent_session_history_t
             WHERE host_id=$1 AND bank_id=$2 AND session_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            projection.try_get::<Value, _>("messages").unwrap(),
            json!([
                {"role":"user","content":"hello"},
                {"role":"assistant","content":"world"},
                {"role":"user","content":"again"}
            ])
        );
        let maximum_event_sequence: i64 = sqlx::query_scalar(
            "SELECT MAX(event_sequence) FROM agent_session_event_t
             WHERE host_id=$1 AND session_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            projection.try_get::<i64, _>("projection_sequence").unwrap(),
            maximum_event_sequence
        );
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(second.turn_id)
        );
        let approval_id = repository
            .request_approval(
                host_id,
                second.turn_id,
                Uuid::now_v7(),
                &digest("approval-input"),
                &digest("approval-subject"),
                Utc::now() + Duration::minutes(5),
            )
            .await
            .unwrap();
        let action_attempt_id = repository
            .approve_and_create_fresh_attempt(
                host_id,
                approval_id,
                "phase4-approver",
                Uuid::now_v7(),
                "mock",
                "gateway",
                &digest("action-schema"),
                &digest("action-arguments"),
            )
            .await
            .unwrap();
        let approval_state: String = sqlx::query_scalar(
            "SELECT state FROM agent_approval_t WHERE host_id=$1 AND approval_id=$2",
        )
        .bind(host_id)
        .bind(approval_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(approval_state, "APPROVED");
        assert!(sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM agent_action_attempt_t WHERE host_id=$1 AND action_attempt_id=$2)",
        )
        .bind(host_id)
        .bind(action_attempt_id)
        .fetch_one(&pool)
        .await
        .unwrap());
        drop(repository);
        let restarted = AgentRepository::with_authority(pool.clone(), authority.clone());
        restarted
            .rebuild_history_projection(host_id, session, session.0)
            .await
            .unwrap();
        let history_after_restart: Value = sqlx::query_scalar(
            "SELECT messages FROM agent_session_history_t WHERE host_id=$1 AND bank_id=$2 AND session_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(
            history_after_restart
                .as_array()
                .is_some_and(|messages| messages.len() >= 3)
        );
        sqlx::query("DELETE FROM agent_job_t WHERE host_id=$1 AND job_id IN ($2,$3)")
            .bind(host_id)
            .bind(workflow_job)
            .bind(foreign_job)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM agent_session_t WHERE host_id=$1 AND session_id IN ($2,$3)")
            .bind(host_id)
            .bind(session.0)
            .bind(workflow_job)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM agent_policy_snapshot_t WHERE host_id=$1 AND policy_snapshot_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .execute(&pool)
        .await
        .unwrap();
    }
}
