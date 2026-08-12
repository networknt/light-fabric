//! Phase 0 wire and persistence contracts for workflow-backed MCP tools.
//!
//! This crate deliberately contains no HTTP server or gateway dispatch code.
//! It freezes the cross-service vocabulary, validates fail-closed invariants,
//! and supplies canonical input digests shared by Portal, gateway, and the
//! workflow invocation service.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use std::{cmp::Ordering, collections::HashSet, fmt};
use thiserror::Error;
use uuid::Uuid;

pub const CONTRACT_VERSION: u16 = 1;
pub const CANONICAL_INPUT_PROFILE: &str = "rfc8785-safe-json-v1";
pub const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InvocationState {
    Accepted,
    Running,
    Waiting,
    Compensating,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EffectState {
    #[default]
    None,
    Possible,
    Confirmed,
}

impl InvocationState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InvocationMode {
    #[default]
    Sync,
    Async,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionClass {
    Interactive,
    Standard,
    Batch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResultTextMode {
    CompactJson,
    Summary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IdempotencyKind {
    #[serde(alias = "derived")]
    Derived,
    #[serde(alias = "explicit")]
    Explicit,
    #[serde(alias = "business")]
    Business,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CancellationPolicy {
    #[default]
    BeforeEffectsOnly,
    Cooperative,
    Disabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SchemaCompatibility {
    Exact,
    BackwardCompatible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BindingLifecycle {
    Draft,
    Active,
    Retired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    WorkflowInputInvalid,
    WorkflowStartRejected,
    WorkflowDefinitionMismatch,
    WorkflowTimeout,
    WorkflowCancelled,
    WorkflowTaskFailed,
    WorkflowOutputInvalid,
    WorkflowOutputInvalidAfterEffect,
    WorkflowPolicyDenied,
    WorkflowCapacityExhausted,
    WorkflowInvocationUnavailable,
    WorkflowIdempotencyConflict,
    WorkflowBudgetExhausted,
    WorkflowBudgetExhaustedAfterEffect,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationBudget {
    pub maximum_task_attempts: u32,
    pub maximum_nested_calls: u32,
    pub maximum_delegation_depth: u16,
    pub maximum_parallelism: u16,
    pub maximum_request_bytes: u64,
    pub maximum_intermediate_bytes: u64,
    pub maximum_result_bytes: u64,
    pub maximum_cost_units: u64,
}

/// Immutable publication snapshot consumed by both gateway projection and the
/// invocation service. Authorization remains attached to the logical tool;
/// the workflow/version fields are dispatch identity only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowToolBindingSnapshot {
    pub contract_version: u16,
    pub binding_id: Uuid,
    pub host_id: Uuid,
    pub stable_tool_ref: Uuid,
    pub logical_tool_name: String,
    pub authorization_endpoint_key: String,
    pub workflow_definition_id: Uuid,
    pub workflow_version: String,
    pub definition_digest: String,
    pub input_schema_digest: String,
    pub output_schema_digest: String,
    pub policy_digest: String,
    pub response_policy_digest: String,
    pub mode: InvocationMode,
    pub root_execution_class: ExecutionClass,
    pub result_text_mode: ResultTextMode,
    pub cancellation_policy: CancellationPolicy,
    pub result_replay_ms: u64,
    pub maximum_delegation_depth: u16,
    pub lifecycle: BindingLifecycle,
}

impl WorkflowToolBindingSnapshot {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.contract_version != CONTRACT_VERSION {
            return Err(ContractError::UnsupportedVersion(self.contract_version));
        }
        if self.logical_tool_name.trim().is_empty()
            || self.authorization_endpoint_key.trim().is_empty()
            || self.workflow_version.trim().is_empty()
        {
            return Err(ContractError::InvalidBinding);
        }
        for digest in [
            &self.definition_digest,
            &self.input_schema_digest,
            &self.output_schema_digest,
            &self.policy_digest,
            &self.response_policy_digest,
        ] {
            validate_digest(digest)?;
        }
        if self.mode == InvocationMode::Sync
            && self.root_execution_class != ExecutionClass::Interactive
        {
            return Err(ContractError::SyncExecutionClass);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowToolDependencySnapshot {
    pub contract_version: u16,
    pub host_id: Uuid,
    pub outer_binding_id: Uuid,
    pub nested_stable_tool_ref: Uuid,
    pub nested_version_target_id: Uuid,
    pub nested_contract_digest: String,
    pub compatibility: SchemaCompatibility,
    pub logical_authorization_endpoint_key: String,
}

impl WorkflowToolDependencySnapshot {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.contract_version != CONTRACT_VERSION {
            return Err(ContractError::UnsupportedVersion(self.contract_version));
        }
        validate_digest(&self.nested_contract_digest)?;
        if self.logical_authorization_endpoint_key.trim().is_empty() {
            return Err(ContractError::InvalidBinding);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowToolBindingPublishedEvent {
    pub event_version: u16,
    pub event_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub binding: WorkflowToolBindingSnapshot,
    pub dependencies: Vec<WorkflowToolDependencySnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowToolBindingRetiredEvent {
    pub event_version: u16,
    pub event_id: Uuid,
    pub occurred_at: DateTime<Utc>,
    pub host_id: Uuid,
    pub binding_id: Uuid,
    pub emergency_revocation: bool,
    pub impact_report_digest: String,
}

impl InvocationBudget {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.maximum_task_attempts == 0
            || self.maximum_parallelism == 0
            || self.maximum_request_bytes == 0
            || self.maximum_intermediate_bytes == 0
            || self.maximum_result_bytes == 0
        {
            return Err(ContractError::InvalidBudget);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IdempotencyBinding {
    pub kind: IdempotencyKind,
    pub scoped_key_digest: String,
    pub input_digest: String,
    pub in_flight_until: DateTime<Utc>,
    pub result_replay_until: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StartInvocationRequest {
    pub contract_version: u16,
    pub workflow_instance_id: Uuid,
    pub stable_tool_ref: Uuid,
    pub workflow_definition_id: Uuid,
    pub workflow_version: String,
    pub definition_digest: String,
    pub schema_digest: String,
    pub policy_digest: String,
    pub response_policy_digest: String,
    pub mode: InvocationMode,
    #[serde(default)]
    pub cancellation_policy: CancellationPolicy,
    pub execution_class: ExecutionClass,
    pub permit_depth: u16,
    pub deadline_ts: DateTime<Utc>,
    pub canonical_input_profile: String,
    pub normalized_input_digest: String,
    pub input: Value,
    pub caller_claims: Value,
    pub idempotency: IdempotencyBinding,
    pub budget: InvocationBudget,
    pub correlation_id: String,
}

impl StartInvocationRequest {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), ContractError> {
        if self.contract_version != CONTRACT_VERSION {
            return Err(ContractError::UnsupportedVersion(self.contract_version));
        }
        if self.canonical_input_profile != CANONICAL_INPUT_PROFILE {
            return Err(ContractError::UnsupportedCanonicalProfile(
                self.canonical_input_profile.clone(),
            ));
        }
        for digest in [
            &self.definition_digest,
            &self.schema_digest,
            &self.policy_digest,
            &self.response_policy_digest,
            &self.normalized_input_digest,
            &self.idempotency.scoped_key_digest,
            &self.idempotency.input_digest,
        ] {
            validate_digest(digest)?;
        }
        if self.deadline_ts <= now || self.idempotency.in_flight_until < self.deadline_ts {
            return Err(ContractError::InvalidDeadline);
        }
        if self.idempotency.result_replay_until < self.idempotency.in_flight_until {
            return Err(ContractError::InvalidReplayWindow);
        }
        if self.permit_depth > self.budget.maximum_delegation_depth {
            return Err(ContractError::DelegationDepth);
        }
        if self.mode == InvocationMode::Sync && self.execution_class != ExecutionClass::Interactive
        {
            return Err(ContractError::SyncExecutionClass);
        }
        if self.correlation_id.trim().is_empty() {
            return Err(ContractError::MissingCorrelationId);
        }
        if !self.caller_claims.is_object() {
            return Err(ContractError::InvalidCallerClaims);
        }
        self.budget.validate()?;
        let actual = canonical_sha256(&self.input)?;
        if actual != self.normalized_input_digest || actual != self.idempotency.input_digest {
            return Err(ContractError::InputDigestMismatch);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationError {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_instance_id: Option<Uuid>,
    pub correlation_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationStatus {
    pub contract_version: u16,
    pub workflow_instance_id: Uuid,
    pub stable_tool_ref: Uuid,
    pub definition_digest: String,
    pub state: InvocationState,
    pub state_version: i64,
    pub accepted_ts: DateTime<Utc>,
    pub updated_ts: DateTime<Utc>,
    pub deadline_ts: DateTime<Utc>,
    pub retryable: bool,
    #[serde(default)]
    pub effect_state: EffectState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub non_cancellable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<InvocationError>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AsyncInvocationHandle {
    pub workflow_instance_id: Uuid,
    pub status: InvocationState,
    pub submitted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct McpTextContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowMcpResult {
    pub content: Vec<McpTextContent>,
    pub structured_content: Value,
    pub is_error: bool,
}

impl WorkflowMcpResult {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.content.len() != 1
            || self.content[0].content_type != "text"
            || !self.structured_content.is_object()
        {
            return Err(ContractError::InvalidMcpResult);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkflowDelegationClaims {
    pub contract_version: u16,
    pub token_id: Uuid,
    pub invocation_id: Uuid,
    pub host_id: Uuid,
    pub principal_subject: String,
    pub end_user_subject: String,
    pub outer_stable_tool_ref: Uuid,
    pub allowed_nested_tool_refs: Vec<Uuid>,
    pub audiences: Vec<String>,
    pub operations: Vec<String>,
    pub data_boundary_digest: String,
    pub policy_digest: String,
    pub execution_class: ExecutionClass,
    pub permit_depth: u16,
    pub remaining_delegation_depth: u16,
    pub budget_ledger_id: Uuid,
    pub budget_generation: u64,
    pub deadline_ts: DateTime<Utc>,
}

impl WorkflowDelegationClaims {
    pub fn validate_nested(
        &self,
        expected_host: Uuid,
        nested_tool: Uuid,
        expected_depth: u16,
        now: DateTime<Utc>,
    ) -> Result<(), ContractError> {
        if self.contract_version != CONTRACT_VERSION {
            return Err(ContractError::UnsupportedVersion(self.contract_version));
        }
        if self.host_id != expected_host {
            return Err(ContractError::TenantMismatch);
        }
        if self.deadline_ts <= now {
            return Err(ContractError::InvalidDeadline);
        }
        if self.permit_depth != expected_depth || self.remaining_delegation_depth == 0 {
            return Err(ContractError::DelegationDepth);
        }
        if !self.allowed_nested_tool_refs.contains(&nested_tool) {
            return Err(ContractError::NestedToolDenied);
        }
        validate_digest(&self.data_boundary_digest)?;
        validate_digest(&self.policy_digest)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BudgetReservationRequest {
    pub reservation_id: Uuid,
    pub ledger_id: Uuid,
    pub generation: u64,
    pub fencing_token: u64,
    pub task_attempts: u32,
    pub nested_calls: u32,
    pub bytes: u64,
    pub cost_units: u64,
}

#[derive(Debug, Error)]
pub enum ContractError {
    #[error("unsupported workflow invocation contract version {0}")]
    UnsupportedVersion(u16),
    #[error("unsupported canonical input profile `{0}`")]
    UnsupportedCanonicalProfile(String),
    #[error("digest must use sha256 followed by 64 lowercase hexadecimal characters")]
    InvalidDigest,
    #[error("deadline or in-flight idempotency window is invalid")]
    InvalidDeadline,
    #[error("completed-result replay must not expire before in-flight deduplication")]
    InvalidReplayWindow,
    #[error("invocation budget contains an invalid zero bound")]
    InvalidBudget,
    #[error("delegation depth is invalid")]
    DelegationDepth,
    #[error("synchronous root invocation must use the interactive execution class")]
    SyncExecutionClass,
    #[error("correlationId is required")]
    MissingCorrelationId,
    #[error("callerClaims must be an object")]
    InvalidCallerClaims,
    #[error("prepared workflow start is missing a durable identity or initial task")]
    InvalidPreparedStart,
    #[error("workflow tool binding is incomplete or internally inconsistent")]
    InvalidBinding,
    #[error("workflow MCP result must have one text item and object structuredContent")]
    InvalidMcpResult,
    #[error("normalized input digest does not match canonical input")]
    InputDigestMismatch,
    #[error("tenant binding does not match")]
    TenantMismatch,
    #[error("nested tool is outside the delegated allowlist")]
    NestedToolDenied,
    #[error("JSON contains a duplicate object key `{0}`")]
    DuplicateKey(String),
    #[error("number is outside the interoperable safe-integer profile")]
    UnsupportedNumber,
    #[error("canonical JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn validate_digest(value: &str) -> Result<(), ContractError> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return Err(ContractError::InvalidDigest);
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ContractError::InvalidDigest);
    }
    Ok(())
}

/// Parse JSON while rejecting duplicate keys before object construction loses
/// that information.
pub fn parse_strict_json(input: &str) -> Result<Value, ContractError> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let value = StrictValue::deserialize(&mut deserializer)?.0;
    deserializer.end()?;
    validate_safe_numbers(&value)?;
    Ok(value)
}

pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, ContractError> {
    validate_safe_numbers(value)?;
    let mut output = Vec::new();
    write_canonical(value, &mut output)?;
    Ok(output)
}

pub fn canonical_sha256(value: &Value) -> Result<String, ContractError> {
    Ok(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(canonical_json_bytes(value)?))
    ))
}

/// Claims that change on every token refresh and therefore must not take part
/// in the disclosure ceiling digest. Both the gateway and the invocation
/// service normalize through this list so the accepted digest stays stable for
/// the lifetime of an invocation.
const VOLATILE_SUBJECT_CLAIMS: &[&str] = &[
    "exp",
    "iat",
    "nbf",
    "jti",
    "nonce",
    "auth_time",
    "sid",
    "at_hash",
    "c_hash",
];

/// Strips volatile JWT claims so a refreshed token still digests to the
/// disclosure ceiling recorded at acceptance.
pub fn stable_subject_claims(claims: &Value) -> Value {
    let Some(claims) = claims.as_object() else {
        return claims.clone();
    };
    Value::Object(
        claims
            .iter()
            .filter(|(name, _)| !VOLATILE_SUBJECT_CLAIMS.contains(&name.as_str()))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect(),
    )
}

fn validate_safe_numbers(value: &Value) -> Result<(), ContractError> {
    match value {
        Value::Number(number) => validate_number(number),
        Value::Array(items) => items.iter().try_for_each(validate_safe_numbers),
        Value::Object(object) => object.values().try_for_each(validate_safe_numbers),
        _ => Ok(()),
    }
}

fn validate_number(number: &Number) -> Result<(), ContractError> {
    if let Some(value) = number.as_i64() {
        return (value.unsigned_abs() <= MAX_SAFE_INTEGER as u64)
            .then_some(())
            .ok_or(ContractError::UnsupportedNumber);
    }
    if let Some(value) = number.as_u64() {
        return (value <= MAX_SAFE_INTEGER as u64)
            .then_some(())
            .ok_or(ContractError::UnsupportedNumber);
    }
    // Phase 0 intentionally narrows RFC 8785 to interoperable safe integers.
    // Decimal business values must arrive as schema-declared strings until an
    // ECMAScript-compatible binary64 formatter is qualified across runtimes.
    Err(ContractError::UnsupportedNumber)
}

fn write_canonical(value: &Value, output: &mut Vec<u8>) -> Result<(), ContractError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(true) => output.extend_from_slice(b"true"),
        Value::Bool(false) => output.extend_from_slice(b"false"),
        Value::Number(number) => output.extend_from_slice(number.to_string().as_bytes()),
        Value::String(string) => {
            output.extend_from_slice(serde_json::to_string(string)?.as_bytes())
        }
        Value::Array(items) => {
            output.push(b'[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical(item, output)?;
            }
            output.push(b']');
        }
        Value::Object(object) => {
            output.push(b'{');
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by(|(left, _), (right, _)| utf16_cmp(left, right));
            for (index, (key, item)) in entries.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(serde_json::to_string(key)?.as_bytes());
                output.push(b':');
                write_canonical(item, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn utf16_cmp(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> de::Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: de::SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element::<StrictValue>()? {
            values.push(value.0);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: de::MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut object = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("duplicate object key `{key}`")));
            }
            let value = map.next_value::<StrictValue>()?;
            object.insert(key, value.0);
        }
        Ok(StrictValue(Value::Object(object)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use serde_json::json;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    #[test]
    fn subject_claims_digest_survives_the_gateway_to_service_seam() {
        // The gateway digests the stable claims for the header and sends the
        // same normalized object as `caller_claims`; the invocation service
        // re-normalizes what it receives. Both sides must land on one digest,
        // and a refreshed token must not move it.
        let issued = json!({
            "sub": "user-1", "roles": ["reader"], "tenant": "t-1",
            "exp": 1_700_000_600u64, "iat": 1_700_000_000u64, "jti": "a"
        });
        let refreshed = json!({
            "sub": "user-1", "roles": ["reader"], "tenant": "t-1",
            "exp": 1_700_003_600u64, "iat": 1_700_003_000u64, "jti": "b"
        });

        let header_digest = canonical_sha256(&stable_subject_claims(&issued)).unwrap();
        let body = stable_subject_claims(&issued);
        let accepted_digest = canonical_sha256(&stable_subject_claims(&body)).unwrap();
        assert_eq!(header_digest, accepted_digest);

        assert_eq!(
            header_digest,
            canonical_sha256(&stable_subject_claims(&refreshed)).unwrap()
        );
        assert_ne!(
            header_digest,
            canonical_sha256(&stable_subject_claims(
                &json!({"sub": "user-1", "roles": [], "tenant": "t-1"})
            ))
            .unwrap()
        );
    }

    #[test]
    fn canonicalization_sorts_by_utf16_and_preserves_array_order() {
        let value = parse_strict_json(r#"{"\uE000":1,"😀":2,"a":[3,2,1]}"#).unwrap();
        assert_eq!(
            String::from_utf8(canonical_json_bytes(&value).unwrap()).unwrap(),
            r#"{"a":[3,2,1],"😀":2,"":1}"#
        );
    }

    #[test]
    fn duplicate_null_and_number_profiles_fail_closed() {
        assert!(parse_strict_json(r#"{"a":null,"a":1}"#).is_err());
        assert!(parse_strict_json("9007199254740992").is_err());
        assert!(parse_strict_json("-9223372036854775808").is_err());
        assert!(parse_strict_json("1.25").is_err());
        assert_ne!(
            canonical_sha256(&parse_strict_json(r#"{}"#).unwrap()).unwrap(),
            canonical_sha256(&parse_strict_json(r#"{"a":null}"#).unwrap()).unwrap()
        );
    }

    #[test]
    fn start_contract_validates_pins_and_input_digest() {
        let now = Utc::now();
        let input = json!({"customerId":"C-1","count":1});
        let input_digest = canonical_sha256(&input).unwrap();
        let request = StartInvocationRequest {
            contract_version: CONTRACT_VERSION,
            workflow_instance_id: Uuid::now_v7(),
            stable_tool_ref: Uuid::now_v7(),
            workflow_definition_id: Uuid::now_v7(),
            workflow_version: "1.0.0".into(),
            definition_digest: digest('a'),
            schema_digest: digest('b'),
            policy_digest: digest('c'),
            response_policy_digest: digest('d'),
            mode: InvocationMode::Sync,
            cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
            execution_class: ExecutionClass::Interactive,
            permit_depth: 0,
            deadline_ts: now + Duration::seconds(30),
            canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
            normalized_input_digest: input_digest.clone(),
            input,
            caller_claims: json!({"sub": "user-1", "roles": ["reader"]}),
            idempotency: IdempotencyBinding {
                kind: IdempotencyKind::Derived,
                scoped_key_digest: digest('e'),
                input_digest,
                in_flight_until: now + Duration::seconds(60),
                result_replay_until: now + Duration::seconds(90),
            },
            budget: InvocationBudget {
                maximum_task_attempts: 8,
                maximum_nested_calls: 8,
                maximum_delegation_depth: 1,
                maximum_parallelism: 1,
                maximum_request_bytes: 65_536,
                maximum_intermediate_bytes: 262_144,
                maximum_result_bytes: 131_072,
                maximum_cost_units: 0,
            },
            correlation_id: "corr-1".into(),
        };
        assert!(request.validate(now).is_ok());
    }
}
