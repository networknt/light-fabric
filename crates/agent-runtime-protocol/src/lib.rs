use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId, ResultClass};
use chrono::{DateTime, Utc};
use execution_runner_protocol::{ExecutionId, LeaseId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "1.4";
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub transport_nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeCapabilities {
    pub adapter_id: String,
    pub adapter_version: String,
    pub adapter_protocol_version: String,
    pub protocol_version: String,
    pub actions: BTreeSet<String>,
    pub supports_approvals: bool,
    pub supports_checkpoint: bool,
    pub supports_session_reuse: bool,
    pub supports_streaming: bool,
    pub supports_thread_turn_identity: bool,
    pub supports_usage: bool,
    pub maximum_event_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentWorkerExecutionSpec {
    pub schema_version: u16,
    pub template_digest: String,
    pub expected_capability_digest: String,
    pub session_id: AgentSessionId,
    pub turn_id: AgentTurnId,
    pub action_attempt_id: AgentActionAttemptId,
    pub policy_digest: String,
    pub input: Value,
    pub wall_clock_timeout_ms: u64,
    pub maximum_event_bytes: usize,
    pub maximum_stderr_bytes: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<AttemptBrokerGrant>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_gateway: Option<EnterpriseGatewayConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayAttemptBinding {
    pub audience: String,
    pub host_id: Uuid,
    pub end_user_subject: String,
    pub principal_subject: String,
    pub workload_actor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_id: Option<Uuid>,
    pub session_id: AgentSessionId,
    pub turn_id: AgentTurnId,
    pub action_attempt_id: AgentActionAttemptId,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub route_alias: String,
    pub billing_subject: String,
    pub budget_policy_id: String,
    pub correlation_id: Uuid,
}

impl GatewayAttemptBinding {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.host_id.is_nil()
            || self.correlation_id.is_nil()
            || [
                self.audience.as_str(),
                self.end_user_subject.as_str(),
                self.principal_subject.as_str(),
                self.workload_actor.as_str(),
                self.policy_digest.as_str(),
                self.data_boundary_digest.as_str(),
                self.route_alias.as_str(),
                self.billing_subject.as_str(),
                self.budget_policy_id.as_str(),
            ]
            .into_iter()
            .any(|value| {
                value.is_empty() || value.len() > 255 || value.chars().any(char::is_whitespace)
            })
            || !canonical_sha256(&self.policy_digest)
            || !canonical_sha256(&self.data_boundary_digest)
        {
            return Err(ProtocolError::InvalidGatewayBinding);
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, ProtocolError> {
        self.validate()?;
        Ok(canonical_digest(self)?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EnterpriseGatewayConfig {
    pub provider_id: String,
    pub base_url: String,
    pub credential_target: String,
    pub credential_env: String,
    pub binding: GatewayAttemptBinding,
}

impl EnterpriseGatewayConfig {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        self.binding.validate()?;
        let url =
            url::Url::parse(&self.base_url).map_err(|_| ProtocolError::InvalidGatewayBinding)?;
        let loopback = url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
        if self.provider_id != "light_gateway"
            || (url.scheme() != "https" && !loopback)
            || url.path() != "/v1"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || !safe_identifier(&self.credential_target)
            || self.credential_env != "LIGHT_LLM_ATTEMPT_TOKEN"
        {
            return Err(ProtocolError::InvalidGatewayBinding);
        }
        Ok(())
    }
}

fn canonical_sha256(value: &str) -> bool {
    let digest = value.strip_prefix("sha256:").unwrap_or_default();
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrokerOperation {
    ModelInference,
    NetworkRequest,
    CredentialedRequest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptBrokerGrant {
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub route_digest: String,
    pub allowed_operations: BTreeSet<BrokerOperation>,
    pub allowed_targets: BTreeSet<String>,
    pub maximum_requests: u32,
    pub maximum_tokens: u64,
    pub maximum_cost_micros: u64,
    pub maximum_response_bytes: usize,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_binding_digest: Option<String>,
}

impl AttemptBrokerGrant {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), ProtocolError> {
        if self.expires_at <= now {
            return Err(ProtocolError::ExpiredGrant);
        }
        if self.allowed_operations.is_empty()
            || self.allowed_targets.is_empty()
            || self.maximum_requests == 0
            || self.maximum_response_bytes == 0
            || self.policy_digest.is_empty()
            || self.data_boundary_digest.is_empty()
            || self.route_digest.is_empty()
            || self.gateway_binding_digest.as_ref().is_some_and(|value| {
                let digest = value.strip_prefix("sha256:").unwrap_or_default();
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(ProtocolError::InvalidGrant);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptCredentialEnvelope {
    pub schema_version: u16,
    pub credential_id: Uuid,
    pub generation: u64,
    pub token: String,
    pub audience: String,
    pub binding_digest: String,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayUsageReceiptClaims {
    pub schema_version: u16,
    pub receipt_id: Uuid,
    pub binding: GatewayAttemptBinding,
    pub logical_request_id: Uuid,
    pub provider_attempt_id: Uuid,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub charged_cost_micros: u64,
    pub usage_complete: bool,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl GatewayUsageReceiptClaims {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), ProtocolError> {
        self.binding.validate()?;
        if self.schema_version != 1
            || self.receipt_id.is_nil()
            || self.logical_request_id.is_nil()
            || self.provider_attempt_id.is_nil()
            || self.expires_at <= now
            || self.expires_at <= self.issued_at
            || self.expires_at > self.issued_at + chrono::Duration::minutes(5)
        {
            return Err(ProtocolError::InvalidGatewayReceipt);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SignedGatewayUsageReceipt {
    pub claims: GatewayUsageReceiptClaims,
    pub key_id: String,
    pub signature: String,
}

impl AttemptCredentialEnvelope {
    pub fn validate(
        &self,
        audience: &str,
        binding_digest: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ProtocolError> {
        if self.schema_version != 1
            || self.credential_id.is_nil()
            || self.generation == 0
            || self.token.is_empty()
            || self.token.len() > 16 * 1024
            || self.token.contains(['\n', '\r'])
            || self.audience != audience
            || self.binding_digest != binding_digest
            || self.issued_at > now
            || self.expires_at <= now
            || self.expires_at <= self.issued_at
            || self.expires_at > self.issued_at + chrono::Duration::minutes(5)
            || self.revoked_at.is_some()
        {
            return Err(ProtocolError::InvalidGatewayCredential);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerRequest {
    pub request_id: Uuid,
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub operation: BrokerOperation,
    pub target: String,
    pub method: String,
    pub path: String,
    pub body_base64: String,
    pub declared_tokens: u64,
    pub declared_cost_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerResponse {
    pub request_id: Uuid,
    pub status: u16,
    pub body_base64: String,
    pub consumed_requests: u32,
    pub consumed_tokens: u64,
    pub consumed_cost_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RuntimeCommand {
    Hello {
        identity: RuntimeIdentity,
        expected_capability_digest: String,
    },
    Start {
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        action_attempt_id: AgentActionAttemptId,
        policy_digest: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enterprise_gateway: Option<Box<EnterpriseGatewayConfig>>,
        input: Value,
    },
    Cancel {
        reason: String,
    },
    Checkpoint {
        reason: String,
    },
    Resume {
        after_sequence: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RuntimeEventPayload {
    Ready {
        capabilities: RuntimeCapabilities,
    },
    Progress {
        message: String,
    },
    ToolResult {
        tool_ref: Uuid,
        output: Value,
    },
    ApprovalRequested {
        request_id: String,
        kind: String,
        subject: Value,
    },
    Usage {
        input_tokens: u64,
        cached_input_tokens: u64,
        output_tokens: u64,
        reasoning_output_tokens: u64,
        total_tokens: u64,
        authoritative: bool,
    },
    CodingPatch {
        base_revision: String,
        patch: String,
        patch_digest: String,
        changed_paths: Vec<String>,
    },
    Checkpoint {
        reference: String,
        digest: String,
    },
    Terminal {
        class: ResultClass,
        output: Option<Value>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeEvent {
    pub protocol_version: String,
    pub event_id: Uuid,
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub sequence: u64,
    pub occurred_at: DateTime<Utc>,
    pub payload: RuntimeEventPayload,
}

impl RuntimeEvent {
    pub fn validate(
        &self,
        expected: &RuntimeIdentity,
        after_sequence: u64,
    ) -> Result<(), ProtocolError> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(ProtocolError::Version(self.protocol_version.clone()));
        }
        if self.execution_id != expected.execution_id
            || self.lease_id != expected.lease_id
            || self.fencing_token != expected.fencing_token
        {
            return Err(ProtocolError::StaleIdentity);
        }
        if self.sequence != after_sequence.saturating_add(1) {
            return Err(ProtocolError::OutOfOrder {
                after: after_sequence,
                actual: self.sequence,
            });
        }
        if canonical_json_bytes(self)?.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge);
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version {0}")]
    Version(String),
    #[error("stale execution, lease, or fencing identity")]
    StaleIdentity,
    #[error("event sequence {actual} is not after {after}")]
    OutOfOrder { after: u64, actual: u64 },
    #[error("runtime frame exceeds maximum size")]
    FrameTooLarge,
    #[error("attempt broker grant is expired")]
    ExpiredGrant,
    #[error("attempt broker grant is invalid")]
    InvalidGrant,
    #[error("enterprise gateway binding is invalid")]
    InvalidGatewayBinding,
    #[error("attempt-scoped gateway credential is invalid")]
    InvalidGatewayCredential,
    #[error("signed gateway usage receipt is invalid")]
    InvalidGatewayReceipt,
    #[error("canonical JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    fn sort(value: Value) -> Value {
        match value {
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (k, sort(v)))
                    .collect::<BTreeMap<_, _>>()
                    .into_iter()
                    .collect(),
            ),
            Value::Array(values) => Value::Array(values.into_iter().map(sort).collect()),
            other => other,
        }
    }
    serde_json::to_vec(&sort(serde_json::to_value(value)?))
}

pub fn canonical_digest<T: Serialize>(value: &T) -> Result<String, serde_json::Error> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(canonical_json_bytes(value)?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_golden_vector_is_stable() {
        let capabilities = RuntimeCapabilities {
            adapter_id: "mock".into(),
            adapter_version: "1".into(),
            adapter_protocol_version: "mock-v1".into(),
            protocol_version: PROTOCOL_VERSION.into(),
            actions: BTreeSet::from(["run".into()]),
            supports_approvals: false,
            supports_checkpoint: true,
            supports_session_reuse: false,
            supports_streaming: true,
            supports_thread_turn_identity: true,
            supports_usage: false,
            maximum_event_bytes: 4096,
        };
        assert_eq!(
            canonical_digest(&capabilities).unwrap(),
            "sha256:43aa5c2c7369f50f10968d892d862078cf8159cf22a0d43c033f3a97a8ff13e2"
        );
    }

    fn identity() -> RuntimeIdentity {
        RuntimeIdentity {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            fencing_token: 7,
            transport_nonce: "n".repeat(32),
        }
    }

    fn event(identity: &RuntimeIdentity, sequence: u64, message: String) -> RuntimeEvent {
        RuntimeEvent {
            protocol_version: PROTOCOL_VERSION.into(),
            event_id: Uuid::now_v7(),
            execution_id: identity.execution_id,
            lease_id: identity.lease_id,
            fencing_token: identity.fencing_token,
            sequence,
            occurred_at: Utc::now(),
            payload: RuntimeEventPayload::Progress { message },
        }
    }

    #[test]
    fn protocol_rejects_unknown_fields_fencing_gaps_and_oversized_frames() {
        let identity = identity();
        let valid = serde_json::to_value(event(&identity, 1, "ok".into())).unwrap();
        let mut unknown = valid.clone();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), Value::Bool(true));
        assert!(serde_json::from_value::<RuntimeEvent>(unknown).is_err());

        let mut stale = event(&identity, 1, "ok".into());
        stale.fencing_token += 1;
        assert!(matches!(
            stale.validate(&identity, 0),
            Err(ProtocolError::StaleIdentity)
        ));
        assert!(matches!(
            event(&identity, 3, "gap".into()).validate(&identity, 1),
            Err(ProtocolError::OutOfOrder { .. })
        ));
        assert!(matches!(
            event(&identity, 1, "x".repeat(MAX_FRAME_BYTES)).validate(&identity, 0),
            Err(ProtocolError::FrameTooLarge)
        ));
    }

    #[test]
    fn broker_grant_rejects_expiry_and_empty_authority() {
        let now = Utc::now();
        let mut grant = AttemptBrokerGrant {
            policy_digest: "sha256:policy".into(),
            data_boundary_digest: "sha256:data".into(),
            route_digest: "sha256:route".into(),
            allowed_operations: BTreeSet::from([BrokerOperation::ModelInference]),
            allowed_targets: BTreeSet::from(["llm-gateway".into()]),
            maximum_requests: 1,
            maximum_tokens: 1,
            maximum_cost_micros: 1,
            maximum_response_bytes: 1,
            expires_at: now + chrono::Duration::seconds(30),
            gateway_binding_digest: None,
        };
        assert!(grant.validate(now).is_ok());
        grant.expires_at = now;
        assert!(matches!(
            grant.validate(now),
            Err(ProtocolError::ExpiredGrant)
        ));
        grant.expires_at = now + chrono::Duration::seconds(30);
        grant.allowed_targets.clear();
        assert!(matches!(
            grant.validate(now),
            Err(ProtocolError::InvalidGrant)
        ));
    }

    #[test]
    fn attempt_credentials_require_live_versioned_unrevoked_generation() {
        let now = Utc::now();
        let mut credential = AttemptCredentialEnvelope {
            schema_version: 1,
            credential_id: Uuid::new_v4(),
            generation: 1,
            token: "attempt-token".into(),
            audience: "llm-gateway".into(),
            binding_digest: format!("sha256:{}", "a".repeat(64)),
            issued_at: now - chrono::Duration::seconds(1),
            expires_at: now + chrono::Duration::minutes(1),
            revoked_at: None,
        };
        assert!(
            credential
                .validate("llm-gateway", &credential.binding_digest, now)
                .is_ok()
        );
        credential.revoked_at = Some(now);
        assert!(matches!(
            credential.validate("llm-gateway", &credential.binding_digest, now),
            Err(ProtocolError::InvalidGatewayCredential)
        ));
        credential.revoked_at = None;
        credential.generation = 0;
        assert!(
            credential
                .validate("llm-gateway", &credential.binding_digest, now)
                .is_err()
        );
        credential.generation = 2;
        credential.expires_at = now;
        assert!(
            credential
                .validate("llm-gateway", &credential.binding_digest, now)
                .is_err()
        );
    }
}
