use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId, ResultClass};
use chrono::{DateTime, Utc};
use execution_runner_protocol::{ExecutionId, LeaseId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;
use uuid::Uuid;

pub const PROTOCOL_VERSION: &str = "1.0";
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
    pub protocol_version: String,
    pub actions: BTreeSet<String>,
    pub supports_checkpoint: bool,
    pub maximum_event_bytes: usize,
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
        if self.sequence <= after_sequence {
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
            protocol_version: PROTOCOL_VERSION.into(),
            actions: BTreeSet::from(["run".into()]),
            supports_checkpoint: true,
            maximum_event_bytes: 4096,
        };
        assert_eq!(
            canonical_digest(&capabilities).unwrap(),
            "sha256:99ab258efa84078ad4aff270583e7a818eb8f2e7ebd78b50693a53f60c2329ea"
        );
    }
}
