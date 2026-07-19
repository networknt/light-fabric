use agent_core::{AgentSessionId, AgentTurnId, PolicySnapshot, sha256_digest};
use agent_materializer::MaterializationManifest;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgListener};
use uuid::Uuid;

use crate::governed_model::GATEWAY_PROVIDER_ID;

use coding_agent_runtime::{CodingFixtureRequest, CodingTurnSpec, ImmutableRepositoryInput};
use execution_runner_protocol::{
    CommandExecutionSpec, ExecutionRequirements, HostExposure, IsolationBoundary,
};

#[derive(Clone)]
pub struct AgentRepository {
    pool: PgPool,
}

async fn resolve_agent_model_binding(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    agent_def_id: Uuid,
    definition_version: i64,
) -> Result<(String, String)> {
    let row = sqlx::query(
        "SELECT d.model_alias_id,d.model_policy_id,d.model_provider,d.model_name,
                direct.alias_name direct_alias,
                (SELECT count(*) FROM llm_model_policy_binding_t b
                   JOIN llm_public_alias_t a ON a.host_id=b.host_id
                    AND a.public_alias_id=b.public_alias_id
                  WHERE b.host_id=d.host_id AND b.model_policy_id=d.model_policy_id
                    AND b.subject_type='AGENT' AND b.subject_id=d.agent_def_id::text
                    AND b.agent_default IS TRUE AND b.active IS TRUE
                    AND a.active IS TRUE AND a.lifecycle_status='ACTIVE') policy_default_count,
                (SELECT max(a.alias_name) FROM llm_model_policy_binding_t b
                   JOIN llm_public_alias_t a ON a.host_id=b.host_id
                    AND a.public_alias_id=b.public_alias_id
                  WHERE b.host_id=d.host_id AND b.model_policy_id=d.model_policy_id
                    AND b.subject_type='AGENT' AND b.subject_id=d.agent_def_id::text
                    AND b.agent_default IS TRUE AND b.active IS TRUE
                    AND a.active IS TRUE AND a.lifecycle_status='ACTIVE') policy_alias
           FROM agent_definition_t d
           LEFT JOIN llm_public_alias_t direct ON direct.host_id=d.host_id
            AND direct.public_alias_id=d.model_alias_id AND direct.active IS TRUE
            AND direct.lifecycle_status='ACTIVE'
          WHERE d.host_id=$1 AND d.agent_def_id=$2 AND d.aggregate_version=$3",
    )
    .bind(host_id)
    .bind(agent_def_id)
    .bind(definition_version)
    .fetch_one(&mut **tx)
    .await?;
    let alias_id: Option<Uuid> = row.try_get("model_alias_id")?;
    let policy_id: Option<Uuid> = row.try_get("model_policy_id")?;
    match (alias_id, policy_id) {
        (Some(_), None) => {
            let alias: Option<String> = row.try_get("direct_alias")?;
            Ok((
                GATEWAY_PROVIDER_ID.to_string(),
                alias
                    .filter(|value| !value.trim().is_empty())
                    .context("governed model alias is inactive or unauthorized")?,
            ))
        }
        (None, Some(_)) => {
            let count: i64 = row.try_get("policy_default_count")?;
            let alias: Option<String> = row.try_get("policy_alias")?;
            if count != 1 {
                bail!("governed model policy must resolve exactly one agent-default alias")
            }
            Ok((
                GATEWAY_PROVIDER_ID.to_string(),
                alias
                    .filter(|value| !value.trim().is_empty())
                    .context("governed model policy default is inactive or unauthorized")?,
            ))
        }
        (None, None) => {
            let provider: Option<String> = row.try_get("model_provider")?;
            let model: Option<String> = row.try_get("model_name")?;
            let provider = provider.filter(|value| !value.trim().is_empty());
            let model = model.filter(|value| !value.trim().is_empty());
            match (provider, model) {
                (Some(provider), Some(model)) => Ok((provider, model)),
                _ => {
                    bail!("agent has neither a governed alias nor a complete legacy model binding")
                }
            }
        }
        (Some(_), Some(_)) => bail!("agent has conflicting governed model selectors"),
    }
}

pub struct SessionSpec {
    pub host_id: Uuid,
    pub session_id: AgentSessionId,
    pub principal_id: String,
    pub user_id: Option<Uuid>,
    pub agent_def_id: Uuid,
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
pub struct PiCodingRuntime {
    pub compatibility_digest: String,
    pub template_digest: String,
    pub pi_digest: String,
    pub provider: String,
    pub model: String,
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
}

async fn resolve_pool(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    agent: Uuid,
    version: i64,
    policy: &str,
    boundary: &str,
    profile: &str,
) -> Result<Option<PoolAssignment>> {
    let configured: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_service_pool_t WHERE host_id=$1")
            .bind(host)
            .fetch_one(&mut **tx)
            .await?;
    let row = sqlx::query(
        "SELECT a.pool_id,a.compatibility_digest,p.compatibility_digest pool_digest,
            p.compatibility_dimensions FROM agent_pool_assignment_t a JOIN agent_service_pool_t p
              ON p.host_id=a.host_id AND p.pool_id=a.pool_id AND p.enabled=TRUE
            WHERE a.host_id=$1 AND a.agent_def_id=$2 AND a.agent_definition_version=$3
              AND a.policy_digest=$4 AND a.revoked_ts IS NULL FOR UPDATE OF a,p",
    )
    .bind(host)
    .bind(agent)
    .bind(version)
    .bind(policy)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        if configured > 0 {
            bail!("agent definition has no live compatible service-pool assignment")
        }
        return Ok(None);
    };
    let assignment: String = row.try_get("compatibility_digest")?;
    let pool: String = row.try_get("pool_digest")?;
    let dimensions: Value = row.try_get("compatibility_dimensions")?;
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
    let host_key = host.to_string();
    if object.get("tenant").and_then(Value::as_str) != Some(host_key.as_str())
        || object.get("dataBoundary").and_then(Value::as_str) != Some(boundary)
        || object.get("profile").and_then(Value::as_str) != Some(profile)
    {
        bail!("service-pool tenant, data boundary, or profile mismatch")
    }
    let computed = execution_runner_protocol::canonical_sha256(&dimensions)?;
    if assignment != pool || pool != computed {
        bail!("service-pool compatibility digest mismatch")
    }
    Ok(Some(PoolAssignment {
        pool_id: row.try_get("pool_id")?,
        compatibility_digest: pool,
    }))
}

async fn enforce_quotas(
    tx: &mut Transaction<'_, Postgres>,
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
        let policies=sqlx::query("SELECT quota_id,maximum_active_sessions,maximum_queued_turns,
            maximum_running_turns,token_budget_per_window,cost_budget_micros_per_window,window_seconds
            FROM agent_quota_policy_t WHERE host_id=$1 AND scope_kind=$2 AND scope_key=$3 AND enabled=TRUE FOR UPDATE")
            .bind(host).bind(kind).bind(&key).fetch_all(&mut **tx).await?;
        for q in policies {
            if session_admission {
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_active_sessions")? {
                    let active:i64=match kind {"HOST"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND state='ACTIVE'").bind(host).fetch_one(&mut **tx).await?,
                    "PRINCIPAL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND principal_id=$2 AND state='ACTIVE'").bind(host).bind(principal).fetch_one(&mut **tx).await?,
                    "AGENT"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND agent_def_id=$2 AND state='ACTIVE'").bind(host).bind(agent).fetch_one(&mut **tx).await?,
                    "PROFILE"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE s.host_id=$1 AND p.product_profile_digest=$2 AND s.state='ACTIVE'").bind(host).bind(profile).fetch_one(&mut **tx).await?,
                    "PROVIDER"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t s JOIN agent_definition_t d ON d.host_id=s.host_id AND d.agent_def_id=s.agent_def_id WHERE s.host_id=$1 AND d.model_provider=$2 AND s.state='ACTIVE'").bind(host).bind(provider).fetch_one(&mut **tx).await?,
                    "POOL"=>sqlx::query_scalar("SELECT COUNT(*) FROM agent_session_t WHERE host_id=$1 AND service_pool_id=$2 AND state='ACTIVE'").bind(host).bind(pool).fetch_one(&mut **tx).await?, _=>0};
                    if active >= i64::from(max) {
                        bail!("agent session quota exceeded for {kind}:{key}")
                    }
                }
            } else {
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_queued_turns")? {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state='QUEUED' AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent queued-turn quota exceeded for {kind}:{key}")
                    }
                }
                if let Some(max) = q.try_get::<Option<i32>, _>("maximum_running_turns")? {
                    let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id WHERE t.host_id=$1 AND t.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL') AND ($2<>'PRINCIPAL' OR s.principal_id=$3) AND ($2<>'AGENT' OR s.agent_def_id=$4) AND ($2<>'POOL' OR s.service_pool_id=$5) AND ($2<>'PROVIDER' OR t.model_provider=$6) AND ($2<>'PROFILE' OR p.product_profile_digest=$7)").bind(host).bind(kind).bind(principal).bind(agent).bind(pool).bind(provider).bind(profile).fetch_one(&mut **tx).await?;
                    if count >= i64::from(max) {
                        bail!("agent running-turn quota exceeded for {kind}:{key}")
                    }
                }
                let token_max = q.try_get::<Option<i64>, _>("token_budget_per_window")?;
                let cost_max = q.try_get::<Option<i64>, _>("cost_budget_micros_per_window")?;
                if cost_max.is_some() && !cost_authoritative {
                    bail!(
                        "agent cost quota requires an active authoritative model rate for {provider}"
                    )
                }
                if token_max.is_some() || cost_max.is_some() {
                    let quota: Uuid = q.try_get("quota_id")?;
                    let window: i32 = q.try_get("window_seconds")?;
                    let ok:Option<Uuid>=sqlx::query_scalar("INSERT INTO agent_quota_usage_t(host_id,quota_id,window_start_ts,reserved_tokens,reserved_cost_micros) VALUES($1,$2,to_timestamp(floor(extract(epoch FROM now())/$3)*$3),$4,$5) ON CONFLICT(host_id,quota_id,window_start_ts) DO UPDATE SET reserved_tokens=agent_quota_usage_t.reserved_tokens+$4,reserved_cost_micros=agent_quota_usage_t.reserved_cost_micros+$5,updated_ts=now() WHERE ($6::bigint IS NULL OR agent_quota_usage_t.reserved_tokens+agent_quota_usage_t.consumed_tokens+$4<=$6) AND ($7::bigint IS NULL OR agent_quota_usage_t.reserved_cost_micros+agent_quota_usage_t.consumed_cost_micros+$5<=$7) RETURNING quota_id")
                        .bind(host).bind(quota).bind(window).bind(tokens).bind(cost).bind(token_max).bind(cost_max).fetch_optional(&mut **tx).await?;
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
    /// Resolves the immutable policy published on the active agent definition.
    /// Session admission must never manufacture policy component digests from
    /// request or process-local values.
    pub async fn resolve_published_policy(
        &self,
        host_id: Uuid,
        agent_def_id: Uuid,
    ) -> Result<PolicySnapshot> {
        let row = sqlx::query(
            "SELECT p.policy_snapshot_id,p.definition_digest,p.product_profile_digest,
                    p.model_digest,p.catalog_digest,p.memory_digest,p.execution_digest,
                    p.channel_digest,p.data_boundary_digest,p.resolved_snapshot,p.policy_digest
             FROM agent_definition_t d
             JOIN agent_policy_snapshot_t p ON p.host_id=d.host_id
               AND p.policy_snapshot_id=d.policy_snapshot_id
               AND p.agent_def_id=d.agent_def_id
             WHERE d.host_id=$1 AND d.agent_def_id=$2 AND d.active=TRUE
               AND p.revoked_ts IS NULL",
        )
        .bind(host_id)
        .bind(agent_def_id)
        .fetch_optional(&self.pool)
        .await?
        .context("agent definition has no active published policy snapshot")?;
        let document: Value = row.try_get("resolved_snapshot")?;
        let policy: PolicySnapshot = serde_json::from_value(document)
            .context("published agent policy snapshot document is invalid")?;
        let expected_digest: String = row.try_get("policy_digest")?;
        // PolicySnapshot has a closed, stable field order. Hashing the typed
        // document also avoids depending on PostgreSQL JSONB key ordering.
        let actual_digest = policy_document_digest(&policy)?;
        if actual_digest != expected_digest
            || policy.snapshot_id != row.try_get::<Uuid, _>("policy_snapshot_id")?
            || policy.definition_digest != row.try_get::<String, _>("definition_digest")?
            || policy.product_profile_digest
                != row.try_get::<String, _>("product_profile_digest")?
            || policy.model_digest != row.try_get::<String, _>("model_digest")?
            || policy.catalog_digest != row.try_get::<String, _>("catalog_digest")?
            || policy.memory_digest != row.try_get::<String, _>("memory_digest")?
            || policy.execution_digest != row.try_get::<String, _>("execution_digest")?
            || policy.channel_digest != row.try_get::<String, _>("channel_digest")?
            || policy.data_boundary_digest != row.try_get::<String, _>("data_boundary_digest")?
        {
            bail!("published agent policy snapshot failed canonical binding verification")
        }
        Ok(policy)
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
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,s.principal_id,b.runner_id,b.backend_id,b.compatibility_digest,b.required_capabilities,b.action_policies->$5 AS action_policy
          FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
          JOIN agent_edge_runner_binding_t b ON b.host_id=s.host_id AND b.edge_binding_id=$4 AND b.principal_id=s.principal_id
          WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION')
            AND b.revoked_ts IS NULL AND b.expires_ts>now() AND b.allowed_actions ? $5 AND b.action_policies ? $5 FOR UPDATE OF t,s,b")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).bind(spec.edge_binding_id).bind(&spec.action)
            .fetch_optional(&mut *tx).await?.context("no live principal-bound edge runner authorizes this action")?;
        let policy: String = row.try_get("policy_digest")?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let principal: String = row.try_get("principal_id")?;
        let required_features: Vec<String> =
            serde_json::from_value(row.try_get("required_capabilities")?)?;
        let compatibility: String = row.try_get("compatibility_digest")?;
        let action_policy: Value = row.try_get("action_policy")?;
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
        let request_id = Uuid::now_v7();
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
            compatibility_digest: compatibility,
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
        sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,agent_session_id,agent_turn_id,agent_action_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,state,pinned_runner_id,pinned_backend_id,edge_binding_id) VALUES($1,$2,$3,'agent','light-agent',$4,'agent-action',$5,$6,$7,$5,$8,$9,$10,$11,$12,'PENDING_CAPACITY',$13,$14,$15)")
            .bind(host_id).bind(request_id).bind(format!("edge-action:{action_attempt_id}")).bind(instance_id).bind(action_attempt_id).bind(session_id.0).bind(turn_id.0).bind(snapshot).bind(&policy).bind(serde_json::to_value(requirements)?).bind(serde_json::to_value(command)?).bind(format!("agent:{principal}")).bind(row.try_get::<String,_>("runner_id")?).bind(row.try_get::<String,_>("backend_id")?).bind(spec.edge_binding_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2").bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(action_attempt_id)
    }
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> PgPool {
        self.pool.clone()
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
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("execution_result_ready_v1").await?;
        self.reconcile_execution_results().await?;
        self.reconcile_agent_jobs().await?;
        loop {
            tokio::select! {
                notification = listener.recv() => { notification?; self.reconcile_execution_results().await?; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    self.reconcile_agent_jobs().await?;
                    self.reconcile_execution_results().await?;
                    self.reconcile_expiry_and_cleanup().await?;
                    self.reconcile_projections().await?;
                    let retention_days = std::env::var("LIGHT_AGENT_QUOTA_USAGE_RETENTION_DAYS").ok()
                        .and_then(|value| value.parse::<i32>().ok()).unwrap_or(30).clamp(1, 3650);
                    self.sweep_quota_usage(retention_days, 1_000).await?;
                },
            }
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

    pub async fn reconcile_agent_jobs(&self) -> Result<u64> {
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
                    j.data_boundary_digest,j.deadline_ts,j.token_budget,j.cost_budget_micros,j.delegation_depth,
                    d.aggregate_version,d.policy_snapshot_id,d.model_provider,d.model_name
                 FROM agent_job_t j JOIN agent_definition_t d ON d.host_id=j.host_id AND d.agent_def_id=j.agent_def_id
                 JOIN agent_policy_snapshot_t p ON p.host_id=d.host_id AND p.policy_snapshot_id=d.policy_snapshot_id
                   AND p.policy_digest=j.policy_digest AND p.data_boundary_digest=j.data_boundary_digest AND p.revoked_ts IS NULL
                 WHERE j.state='PENDING' AND j.deadline_ts>now() ORDER BY j.created_ts,j.job_id
                 LIMIT 1 FOR UPDATE OF j SKIP LOCKED")
                .fetch_optional(&mut *tx).await?;
            let Some(row) = row else {
                tx.commit().await?;
                break;
            };
            let host: Uuid = row.try_get("host_id")?;
            let job: Uuid = row.try_get("job_id")?;
            let turn = Uuid::now_v7();
            let deadline: DateTime<Utc> = row.try_get("deadline_ts")?;
            sqlx::query("INSERT INTO agent_session_t(host_id,session_id,principal_id,agent_def_id,
                    agent_definition_version,policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest)
                    VALUES($1,$2,$3,$4,$5,$6,$7,$7,$8) ON CONFLICT(host_id,session_id) DO NOTHING")
                .bind(host).bind(job).bind(format!("workflow-job:{job}"))
                .bind(row.try_get::<Uuid,_>("agent_def_id")?).bind(row.try_get::<i64,_>("aggregate_version")?)
                .bind(row.try_get::<Uuid,_>("policy_snapshot_id")?).bind(deadline)
                .bind(sha256_digest(format!("workflow-job:{job}").as_bytes())).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO agent_turn_t(host_id,turn_id,session_id,turn_sequence,queue_sequence,
                    origin_kind,origin_ref,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,
                    data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,
                    cost_budget_micros,deadline_ts,delegation_depth)
                    VALUES($1,$2,$3,1,1,'workflow',$4,$5,$5,$6,$7,$8,$9,$10,20,$11,$12,$13,$14)")
                .bind(host).bind(turn).bind(job).bind(job.to_string())
                .bind(row.try_get::<String,_>("idempotency_key")?).bind(row.try_get::<Uuid,_>("policy_snapshot_id")?)
                .bind(row.try_get::<String,_>("policy_digest")?).bind(row.try_get::<String,_>("data_boundary_digest")?)
                .bind(row.try_get::<String,_>("model_provider")?).bind(row.try_get::<String,_>("model_name")?)
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
                    cancellation_requested_ts=COALESCE(j.cancellation_requested_ts,now()),
                    terminal_ts=now(),updated_ts=now()
                    FROM task_info_t t,process_info_t p WHERE t.host_id=j.host_id
                      AND t.task_id=j.workflow_task_id AND p.host_id=j.host_id
                      AND p.process_id=j.workflow_process_id
                      AND j.state IN('PENDING','TURN_CREATED','RUNNING')
                      AND (p.status_code<>'A' OR t.status_code IN('F','X'))
                    RETURNING j.host_id,j.turn_id) UPDATE agent_turn_t t SET state='CANCELLED',
                    terminal_error=jsonb_build_object('class','workflow_cancelled'),terminal_ts=now(),updated_ts=now()
                    FROM jobs WHERE t.host_id=jobs.host_id AND t.turn_id=jobs.turn_id
                      AND t.state NOT IN('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .execute(&self.pool).await?;
        sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,
                    execution_session_id,origin_kind,origin_service_id,origin_instance_id,
                    origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,
                    cleanup_deadline_ts,state)
                    SELECT j.host_id,gen_random_uuid(),s.execution_session_id,'agent','light-agent',
                      'workflow-job-reconciler',s.session_id,'agent-turn',j.turn_id,
                      'workflow-job-cancel:'||j.job_id,'workflow-cancelled','light-agent',
                      now()+interval '5 minutes','PENDING'
                    FROM agent_job_t j JOIN agent_session_t s ON s.host_id=j.host_id AND s.session_id=j.job_id
                    WHERE j.cancellation_requested_ts IS NOT NULL AND s.execution_session_id IS NOT NULL
                      AND s.cleanup_state IN('NOT_REQUIRED','CLEANUP_REQUESTED')
                    ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
            .execute(&self.pool).await?;
        sqlx::query("UPDATE agent_session_t s SET state='CLOSING',cleanup_state='CLEANUP_PENDING',updated_ts=now()
                    FROM agent_job_t j WHERE j.host_id=s.host_id AND j.job_id=s.session_id
                      AND j.cancellation_requested_ts IS NOT NULL AND s.execution_session_id IS NOT NULL
                      AND s.state='ACTIVE'").execute(&self.pool).await?;
        Ok(changed + terminal.rows_affected() + cancelled.rows_affected())
    }

    pub async fn reconcile_execution_results(&self) -> Result<u64> {
        let rows = sqlx::query("SELECT a.host_id,a.action_attempt_id,a.turn_id,a.execution_attempt_id FROM agent_action_attempt_t a JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id WHERE a.origin_accepted_ts IS NULL AND e.terminal_ts IS NOT NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        let mut accepted = 0;
        for row in rows {
            accepted += self
                .accept_execution_result(
                    row.try_get("host_id")?,
                    row.try_get("action_attempt_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_attempt_id")?,
                )
                .await? as u64;
        }
        let turns = sqlx::query("SELECT t.host_id,t.turn_id,e.execution_id FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.execution_attempt_id IS NULL AND e.terminal_ts IS NOT NULL AND e.accepted_by_origin_ts IS NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in turns {
            accepted += self
                .accept_coding_turn_result(
                    row.try_get("host_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_id")?,
                )
                .await? as u64;
        }
        Ok(accepted)
    }

    async fn accept_coding_turn_result(
        &self,
        host_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.host_id=$1 AND t.turn_id=$2 AND e.execution_id=$3 AND e.terminal_ts IS NOT NULL FOR UPDATE OF t,e")
            .bind(host_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let session: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?});
        let trusted_usage = row
            .try_get::<Option<Value>, _>("normalized_result")?
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
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2").bind(host_id).bind(execution_id).execute(&mut *tx).await?;
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
        let request_id = Uuid::now_v7();
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
        let execution_spec = serde_json::to_value(&command)?;
        sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,agent_session_id,agent_turn_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,state) VALUES($1,$2,$3,'agent','light-agent',$4,'agent-turn',$5,$6,$5,$7,$8,$9,$10,$11,'PENDING_CAPACITY')")
            .bind(host_id).bind(request_id).bind(format!("coding-turn:{}",turn_id.0)).bind(instance_id).bind(turn_id.0).bind(session_id.0).bind(snapshot).bind(&policy).bind(serde_json::to_value(requirements)?).bind(execution_spec).bind(format!("agent:{principal}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,mount_options) VALUES($1,$2,$3,'repository-bundle',$4,$5,$6,$7,'{}'::jsonb,jsonb_build_object('baseRevision',$8),'{}'::jsonb,jsonb_build_object('state','IMMUTABLE'),$9,'/inputs/repository.bundle',TRUE,FALSE,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb)")
            .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(&repository.artifact_uri).bind(&repository.digest).bind(repository.size as i64).bind(&repository.media_type).bind(&spec.base_revision).bind(format!("{}/inputs",spec.workspace_root)).execute(&mut *tx).await?;
        for package in &manifest.packages {
            let inserted=sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,trust_bundle_id,package_manifest_digest,mount_options) SELECT $1,$2,$3,'skill-package',p.object_reference,p.content_digest,p.size_bytes,p.media_type,jsonb_build_object('signer',p.signer_reference,'signature',p.signature_reference),jsonb_build_object('reference',p.provenance_reference),jsonb_build_object('scanner',p.scanner_reference,'digest',p.scan_digest),jsonb_build_object('state',p.state,'revokedTs',p.revoked_ts),$4,$5,TRUE,FALSE,p.signer_reference,$6,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb FROM skill_package_t p WHERE p.host_id=$1 AND p.package_id=$7 AND p.state='PUBLISHED' AND p.revoked_ts IS NULL AND p.content_digest=$6")
                .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(format!("{}/inputs",spec.workspace_root)).bind(&package.mount_target).bind(&package.content_digest).bind(package.package_id).execute(&mut *tx).await?;
            if inserted.rows_affected() != 1 {
                bail!(
                    "skill package {} became unavailable during admission",
                    package.package_id
                );
            }
        }
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","CODING_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision}),&policy).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn schedule_pi_coding_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        manifest: &MaterializationManifest,
        spec: &CodingTurnSpec,
        repository: &ImmutableRepositoryInput,
        runtime: &PiCodingRuntime,
    ) -> Result<Uuid> {
        spec.validate()?;
        repository.validate(spec)?;
        let manifest_digest = manifest.digest()?;
        if manifest.product_profile != agent_materializer::ProductProfile::Coding
            || spec.materialization_manifest_digest != manifest_digest
            || manifest.runtime_compatibility != runtime.compatibility_digest
            || manifest.writable_roots != spec.writable_roots
        {
            bail!("Pi coding materialization or runtime binding mismatch")
        }
        for digest in [
            runtime.compatibility_digest.as_str(),
            runtime.template_digest.as_str(),
            runtime.pi_digest.as_str(),
        ] {
            let hex = digest.strip_prefix("sha256:").unwrap_or_default();
            if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                bail!("Pi runtime requires canonical SHA-256 digests")
            }
        }
        if runtime.provider.is_empty()
            || runtime.model.is_empty()
            || runtime.provider.starts_with('-')
            || runtime.model.starts_with('-')
        {
            bail!("Pi provider or model binding is invalid")
        }
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,s.principal_id FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id AND p.revoked_ts IS NULL WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state='RECEIVED' FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).fetch_one(&mut *tx).await?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let principal: String = row.try_get("principal_id")?;
        let request_id = Uuid::now_v7();
        let requirements = ExecutionRequirements {
            action_kind: "coding.pi-rpc-v1".into(),
            minimum_boundary: IsolationBoundary::MicroVm,
            maximum_host_exposure: HostExposure::None,
            network_enabled: false,
            credential_classes: vec![],
            persistent_workspace: false,
            required_features: vec![
                "deny-all-egress".into(),
                "immutable-repository-upload".into(),
                "canonical-patch-output".into(),
                "pi-rpc-v1".into(),
            ],
            policy_digest: policy.clone(),
            compatibility_digest: runtime.compatibility_digest.clone(),
        };
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-pi-rpc-v1".into(),
            template_version: 1,
            template_digest: runtime.template_digest.clone(),
            executable: "/usr/local/bin/light-pi-rpc-adapter".into(),
            arguments: vec![
                "--repository".into(),
                "/inputs/repository.bundle".into(),
                "--request-base64".into(),
                spec.encode_argument()?,
                "--pi".into(),
                "/usr/local/bin/pi".into(),
                "--pi-digest".into(),
                runtime.pi_digest.clone(),
                "--provider".into(),
                runtime.provider.clone(),
                "--model".into(),
                runtime.model.clone(),
            ],
            working_directory: "/workspace".into(),
            environment: Default::default(),
            wall_clock_timeout_ms: 120_000,
            stdout_limit_bytes: 16 * 1024 * 1024,
            stderr_limit_bytes: 1024 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,agent_session_id,agent_turn_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,state) VALUES($1,$2,$3,'agent','light-agent',$4,'agent-turn',$5,$6,$5,$7,$8,$9,$10,$11,'PENDING_CAPACITY')")
            .bind(host_id).bind(request_id).bind(format!("coding-pi-turn:{}",turn_id.0)).bind(instance_id).bind(turn_id.0).bind(session_id.0).bind(snapshot).bind(&policy).bind(serde_json::to_value(requirements)?).bind(serde_json::to_value(command)?).bind(format!("agent:{principal}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,mount_options) VALUES($1,$2,$3,'repository-bundle',$4,$5,$6,$7,'{}'::jsonb,jsonb_build_object('baseRevision',$8),'{}'::jsonb,jsonb_build_object('state','IMMUTABLE'),$9,'/inputs/repository.bundle',TRUE,FALSE,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb)")
            .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(&repository.artifact_uri).bind(&repository.digest).bind(repository.size as i64).bind(&repository.media_type).bind(&spec.base_revision).bind(format!("{}/inputs",spec.workspace_root)).execute(&mut *tx).await?;
        if !manifest.packages.is_empty() {
            bail!(
                "Pi coding profile package mounting is not admitted until policy-to-package resolution is server-owned"
            )
        }
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='RECEIVED'")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","PI_CODING_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision,"adapter":"pi-rpc"}),&policy).await?;
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
                sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,execution_session_id,origin_kind,origin_service_id,origin_instance_id,origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,cleanup_deadline_ts,state) VALUES($1,$2,$3,'agent','light-agent','session-reconciler',$4,'agent-turn',$4,$5,'session-expired','light-agent',now()+interval '5 minutes','PENDING') ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
                    .bind(host_id).bind(cleanup_id).bind(execution_session_id).bind(session_id).bind(format!("session-expired:{session_id}")).execute(&mut *tx).await?;
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

    async fn accept_execution_result(
        &self,
        host_id: Uuid,
        action_attempt_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error,e.fencing_token,(r.normalized_requirements->>'actionKind') LIKE 'edge.%' AS terminal_edge FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id JOIN runner_scheduling_request_t r ON r.host_id=e.host_id AND r.request_id=e.request_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 AND e.execution_id=$4 AND e.terminal_ts IS NOT NULL FOR UPDATE OF a,t,e")
            .bind(host_id).bind(action_attempt_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
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
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?,"fencingToken":row.try_get::<i64,_>("fencing_token")?});
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
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(&result).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2 AND terminal_ts IS NOT NULL")
            .bind(host_id).bind(execution_id).execute(&mut *tx).await?;
        if row.try_get::<bool, _>("terminal_edge")? {
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
        persist_policy(&mut tx, spec.host_id, spec.agent_def_id, &spec.policy).await?;
        let policy_digest =
            sha256_digest(&serde_json::to_vec(&serde_json::to_value(&spec.policy)?)?);
        let definition_version: i64 = sqlx::query_scalar(
            "SELECT aggregate_version
            FROM agent_definition_t WHERE host_id=$1 AND agent_def_id=$2",
        )
        .bind(spec.host_id)
        .bind(spec.agent_def_id)
        .fetch_one(&mut *tx)
        .await?;
        let (provider, _) = resolve_agent_model_binding(
            &mut tx,
            spec.host_id,
            spec.agent_def_id,
            definition_version,
        )
        .await?;
        let pool = resolve_pool(
            &mut tx,
            spec.host_id,
            spec.agent_def_id,
            definition_version,
            &policy_digest,
            &spec.policy.data_boundary_digest,
            &spec.policy.product_profile_digest,
        )
        .await?;
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
            enforce_quotas(
                &mut tx,
                spec.host_id,
                &spec.principal_id,
                spec.agent_def_id,
                &spec.policy.product_profile_digest,
                &provider,
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
              service_pool_id,service_pool_compatibility_digest)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)
             ON CONFLICT (host_id,session_id) DO NOTHING",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .bind(&spec.principal_id)
        .bind(spec.user_id)
        .bind(spec.agent_def_id)
        .bind(definition_version)
        .bind(spec.bank_id)
        .bind(spec.policy.snapshot_id)
        .bind(spec.idle_expires_at)
        .bind(spec.maximum_expires_at)
        .bind(&spec.resume_handle_digest)
        .bind(pool.as_ref().map(|p| p.pool_id))
        .bind(pool.as_ref().map(|p| p.compatibility_digest.as_str()))
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            let row = sqlx::query("SELECT principal_id,agent_def_id,agent_definition_version,policy_snapshot_id,state,service_pool_id,service_pool_compatibility_digest FROM agent_session_t WHERE host_id=$1 AND session_id=$2 FOR UPDATE")
                .bind(spec.host_id).bind(spec.session_id.0).fetch_one(&mut *tx).await?;
            let principal: String = row.try_get("principal_id")?;
            let definition: Uuid = row.try_get("agent_def_id")?;
            let state: String = row.try_get("state")?;
            if principal != spec.principal_id
                || definition != spec.agent_def_id
                || row.try_get::<i64, _>("agent_definition_version")? != definition_version
                || row.try_get::<Uuid, _>("policy_snapshot_id")? != spec.policy.snapshot_id
                || state != "ACTIVE"
                || row.try_get::<Option<Uuid>, _>("service_pool_id")?
                    != pool.as_ref().map(|p| p.pool_id)
                || row.try_get::<Option<String>, _>("service_pool_compatibility_digest")?
                    != pool.as_ref().map(|p| p.compatibility_digest.clone())
            {
                bail!("durable agent session ownership or state mismatch");
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn admit_user_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        client_message_id: &str,
        text: &str,
    ) -> Result<AdmittedTurn> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT next_turn_sequence,next_queue_sequence,policy_snapshot_id,
              p.policy_digest,p.data_boundary_digest,p.product_profile_digest,maximum_expires_ts,
              s.principal_id,s.agent_def_id,s.agent_definition_version,s.service_pool_id
              FROM agent_session_t s JOIN agent_policy_snapshot_t p ON p.host_id=s.host_id
                AND p.policy_snapshot_id=s.policy_snapshot_id AND p.revoked_ts IS NULL
              JOIN agent_definition_t d ON d.host_id=s.host_id AND d.agent_def_id=s.agent_def_id
                AND d.aggregate_version=s.agent_definition_version
              WHERE s.host_id=$1 AND s.session_id=$2 AND s.state='ACTIVE'
                AND (s.service_pool_id IS NULL OR EXISTS(SELECT 1 FROM agent_pool_assignment_t a
                  JOIN agent_service_pool_t sp ON sp.host_id=a.host_id AND sp.pool_id=a.pool_id AND sp.enabled=TRUE
                  WHERE a.host_id=s.host_id AND a.agent_def_id=s.agent_def_id
                    AND a.agent_definition_version=s.agent_definition_version AND a.policy_digest=p.policy_digest
                    AND a.pool_id=s.service_pool_id AND a.compatibility_digest=s.service_pool_compatibility_digest
                    AND a.revoked_ts IS NULL)) FOR UPDATE OF s,p",
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
        let definition_version: i64 = row.try_get("agent_definition_version")?;
        let (model_provider, model_name) =
            resolve_agent_model_binding(&mut tx, host_id, agent, definition_version).await?;
        let rate = sqlx::query(
            "SELECT input_cost_micros_per_million,output_cost_micros_per_million
             FROM agent_model_rate_t WHERE host_id=$1 AND provider=$2 AND model=$3
               AND enabled=TRUE AND effective_ts<=now()
               AND (expires_ts IS NULL OR expires_ts>now())
             ORDER BY effective_ts DESC,rate_id DESC LIMIT 1 FOR SHARE",
        )
        .bind(host_id)
        .bind(&model_provider)
        .bind(&model_name)
        .fetch_optional(&mut *tx)
        .await?;
        let input_rate = rate
            .as_ref()
            .map(|row| row.try_get::<i64, _>("input_cost_micros_per_million"))
            .transpose()?
            .unwrap_or(0);
        let output_rate = rate
            .as_ref()
            .map(|row| row.try_get::<i64, _>("output_cost_micros_per_million"))
            .transpose()?
            .unwrap_or(0);
        let turn_id = AgentTurnId::new();
        let token_reservation = 65_536_i64;
        let cost_reservation = token_cost_micros(token_reservation, input_rate.max(output_rate));
        enforce_quotas(
            &mut tx,
            host_id,
            &principal,
            agent,
            &profile,
            &model_provider,
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
            .bind(policy_snapshot_id).bind(&policy_digest).bind(&boundary).bind(&model_provider).bind(&model_name)
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
        Ok(self
            .dispatch_next_turn_fair(host_id)
            .await?
            .and_then(|(selected_session, turn)| (selected_session == session_id).then_some(turn)))
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
             LEFT JOIN agent_service_pool_t p ON p.host_id=t.host_id AND p.pool_id=t.service_pool_id
             WHERE t.host_id=$1 AND t.state='QUEUED' AND s.state='ACTIVE'
               AND s.active_turn_id IS NULL
               AND (t.service_pool_id IS NULL OR (p.enabled=TRUE AND
                 EXISTS(SELECT 1 FROM agent_pool_assignment_t a
                   WHERE a.host_id=t.host_id AND a.agent_def_id=s.agent_def_id
                     AND a.agent_definition_version=s.agent_definition_version
                     AND a.policy_digest=t.policy_digest AND a.pool_id=t.service_pool_id
                     AND a.compatibility_digest=s.service_pool_compatibility_digest
                     AND a.revoked_ts IS NULL) AND
                 (SELECT COUNT(*) FROM agent_turn_t running
                  WHERE running.host_id=t.host_id AND running.service_pool_id=t.service_pool_id
                    AND running.state IN('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION','WAITING_APPROVAL')) < p.maximum_concurrency))
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
                    s.service_pool_compatibility_digest,p.product_profile_digest
             FROM agent_turn_t t JOIN agent_session_t s
               ON s.host_id=t.host_id AND s.session_id=t.session_id AND s.active_turn_id=t.turn_id
             JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id
               AND p.policy_snapshot_id=t.policy_snapshot_id AND p.policy_digest=t.policy_digest
               AND p.revoked_ts IS NULL
             LEFT JOIN agent_service_pool_t sp ON sp.host_id=t.host_id AND sp.pool_id=t.service_pool_id
             WHERE t.host_id=$1 AND t.turn_id=$2 AND t.state='RECEIVED'
               AND t.service_pool_id IS NOT DISTINCT FROM s.service_pool_id
               AND (t.service_pool_id IS NULL OR (sp.enabled=TRUE AND EXISTS(
                 SELECT 1 FROM agent_pool_assignment_t a
                 WHERE a.host_id=t.host_id AND a.agent_def_id=s.agent_def_id
                   AND a.agent_definition_version=s.agent_definition_version
                   AND a.policy_digest=t.policy_digest AND a.pool_id=t.service_pool_id
                   AND a.compatibility_digest=s.service_pool_compatibility_digest
                   AND a.revoked_ts IS NULL)))",
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
    host_id: Uuid,
    agent_def_id: Uuid,
    policy: &PolicySnapshot,
) -> Result<()> {
    let value = serde_json::to_value(policy)?;
    let digest = policy_document_digest(policy)?;
    let inserted = sqlx::query("INSERT INTO agent_policy_snapshot_t(host_id,policy_snapshot_id,agent_def_id,definition_digest,product_profile_digest,model_digest,catalog_digest,memory_digest,execution_digest,channel_digest,data_boundary_digest,resolved_snapshot,policy_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(host_id,policy_snapshot_id) DO NOTHING")
        .bind(host_id).bind(policy.snapshot_id).bind(agent_def_id).bind(&policy.definition_digest).bind(&policy.product_profile_digest).bind(&policy.model_digest).bind(&policy.catalog_digest).bind(&policy.memory_digest).bind(&policy.execution_digest).bind(&policy.channel_digest).bind(&policy.data_boundary_digest).bind(value).bind(digest).execute(&mut **tx).await?;
    if inserted.rows_affected() == 0 {
        let matches: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM agent_policy_snapshot_t
             WHERE host_id=$1 AND policy_snapshot_id=$2 AND agent_def_id=$3
               AND revoked_ts IS NULL AND definition_digest=$4
               AND product_profile_digest=$5 AND model_digest=$6
               AND catalog_digest=$7 AND memory_digest=$8
               AND execution_digest=$9 AND channel_digest=$10
               AND data_boundary_digest=$11 AND resolved_snapshot=$12
               AND policy_digest=$13)",
        )
        .bind(host_id)
        .bind(policy.snapshot_id)
        .bind(agent_def_id)
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

fn policy_document_digest(policy: &PolicySnapshot) -> Result<String> {
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
    fn published_policy_digest_is_stable_after_jsonb_key_reordering() {
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
        let pool = PgPool::connect(&url).await.unwrap();
        let host_id = Uuid::now_v7();
        let agent_def_id = Uuid::now_v7();
        let owner = Uuid::now_v7();
        let domain = format!("agent-{}.test", host_id.simple());
        let mut setup = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO org_t(domain,org_name,org_desc,org_owner) VALUES($1,'agent-test','agent-test',$2)").bind(&domain).bind(owner).execute(&mut *setup).await.unwrap();
        sqlx::query(
            "INSERT INTO host_t(host_id,domain,sub_domain,host_owner) VALUES($1,$2,'test',$3)",
        )
        .bind(host_id)
        .bind(&domain)
        .bind(owner)
        .execute(&mut *setup)
        .await
        .unwrap();
        sqlx::query("INSERT INTO api_t(host_id,api_id,api_name,api_status) VALUES($1,'agent','agent','Published')").bind(host_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO api_version_t(host_id,api_version_id,api_id,api_version,api_type,service_id) VALUES($1,$2,'agent','1.0.0','mcp','agent-test')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO agent_definition_t(host_id,agent_def_id,model_provider,model_name) VALUES($1,$2,'mock','mock')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        setup.commit().await.unwrap();
        let repository = AgentRepository::new(pool.clone());
        let session = AgentSessionId::new();
        let principal_id = Uuid::now_v7();
        let digest = |name: &str| sha256_digest(name.as_bytes());
        repository
            .create_or_resume_session(&SessionSpec {
                host_id,
                session_id: session,
                principal_id: principal_id.to_string(),
                user_id: Some(principal_id),
                agent_def_id,
                bank_id: None,
                policy: PolicySnapshot {
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
                },
                idle_expires_at: Utc::now() + Duration::hours(1),
                maximum_expires_at: Utc::now() + Duration::hours(2),
                resume_handle_digest: digest(&session.to_string()),
            })
            .await
            .unwrap();
        sqlx::query("INSERT INTO agent_memory_bank_t(host_id,bank_id,agent_def_id,user_id,bank_name) VALUES($1,$2,$3,$4,'test-history')")
            .bind(host_id).bind(session.0).bind(agent_def_id).bind(principal_id)
            .execute(&pool).await.unwrap();
        sqlx::query("UPDATE agent_definition_t SET policy_snapshot_id=$3 WHERE host_id=$1 AND agent_def_id=$2")
            .bind(host_id)
            .bind(agent_def_id)
            .bind(session.0)
            .execute(&pool)
            .await
            .unwrap();
        let published = repository
            .resolve_published_policy(host_id, agent_def_id)
            .await
            .unwrap();
        assert_eq!(published.snapshot_id, session.0);
        assert_eq!(published.catalog_digest, digest("catalog"));
        let first = repository
            .admit_user_turn(host_id, session, "message-1", "hello")
            .await
            .unwrap();
        let duplicate = repository
            .admit_user_turn(host_id, session, "message-1", "hello")
            .await
            .unwrap();
        let second = repository
            .admit_user_turn(host_id, session, "message-2", "again")
            .await
            .unwrap();
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert!(duplicate.duplicate);
        assert!(second.turn_sequence > first.turn_sequence);
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
        assert_eq!(runtime.model_provider, "mock");
        assert_eq!(runtime.model_name, "mock");
        repository
            .complete_turn(host_id, session, first.turn_id, "world", Some(1), Some(0))
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
        sqlx::query("DELETE FROM agent_session_t WHERE host_id=$1 AND session_id=$2")
            .bind(host_id)
            .bind(session.0)
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
        sqlx::query("DELETE FROM org_t WHERE domain=$1")
            .bind(domain)
            .execute(&pool)
            .await
            .unwrap();
    }
}
