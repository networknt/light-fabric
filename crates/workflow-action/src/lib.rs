//! Server-owned workflow action contracts. References select records; they are
//! never bearer authority. No access/refresh token belongs in these records.
pub mod guard;
pub mod ledger;
mod owners;
mod runtime;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_DISPATCH_LEASE_MS: u64 = 5_000;
pub const ACTION_REFERENCE_HEADER: &str = "x-workflow-action";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Owner {
    pub gateway_service: String,
    pub replica: Uuid,
    pub boot: Uuid,
    pub fencing_generation: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub host_id: Uuid,
    pub user_id: Uuid,
    pub grant_id: Uuid,
    pub run_id: Uuid,
    pub action_id: Uuid,
    pub attempt_id: Uuid,
    pub calling_app: String,
    pub request_digest: String,
    /// Conservative outbound payload reservation, including protocol encoding.
    pub request_bytes: u64,
    pub response_byte_limit: u64,
    pub cost_unit_limit: u64,
    pub tool_ref: Uuid,
    pub target: String,
    pub contract_digest: String,
    pub policy_digest: String,
    pub disclosure_digest: String,
    pub claims_digest: String,
    pub grant_generation: i64,
    pub run_generation: i64,
    pub budget_generation: i64,
    pub action_generation: i64,
    pub execution_class: ExecutionClass,
    pub depth: u16,
    pub maximum_depth: u16,
    pub parent_action_id: Option<Uuid>,
    pub deadline: DateTime<Utc>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionClass {
    Interactive,
    Standard,
    Batch,
}

impl Binding {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            self.host_id,
            self.user_id,
            self.grant_id,
            self.run_id,
            self.action_id,
            self.attempt_id,
            self.tool_ref,
        ]
        .iter()
        .any(Uuid::is_nil)
            || self.calling_app.trim().is_empty()
            || self.target.trim().is_empty()
            || [
                self.grant_generation,
                self.run_generation,
                self.budget_generation,
                self.action_generation,
            ]
            .iter()
            .any(|v| *v <= 0)
            || self
                .request_bytes
                .checked_add(self.response_byte_limit)
                .is_none_or(|n| n > i64::MAX as u64)
            || self.cost_unit_limit > i64::MAX as u64
            || self.depth > self.maximum_depth
            || (self.depth == 0) != self.parent_action_id.is_none()
            || [
                &self.request_digest,
                &self.contract_digest,
                &self.policy_digest,
                &self.disclosure_digest,
                &self.claims_digest,
            ]
            .iter()
            .any(|s| !is_digest(s))
        {
            return Err("invalid action binding");
        }
        Ok(())
    }
    /// Child authority is inherited from a stored parent, never an inbound depth
    /// header. Private target identity/digest are filled by the dependency registry.
    pub fn validate_child_of(&self, parent: &Self) -> Result<(), &'static str> {
        self.validate()?;
        if self.parent_action_id != Some(parent.action_id)
            || parent.depth.checked_add(1) != Some(self.depth)
            || self.maximum_depth != parent.maximum_depth
            || self.execution_class != parent.execution_class
            || self.host_id != parent.host_id
            || self.user_id != parent.user_id
            || self.grant_id != parent.grant_id
            || self.run_id != parent.run_id
            || self.grant_generation != parent.grant_generation
            || self.run_generation != parent.run_generation
            || self.budget_generation != parent.budget_generation
            || self.policy_digest != parent.policy_digest
            || self.disclosure_digest != parent.disclosure_digest
            || self.deadline > parent.deadline
        {
            return Err("child action escapes parent authority");
        }
        Ok(())
    }
}

pub fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|s| {
        s.len() == 64
            && s.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    })
}

/// Hash exact request bytes and routing inputs with length framing. Callers must
/// pass the bytes actually sent, not a model summary or an independently encoded
/// JSON object. Hop-by-hop credentials are deliberately excluded.
pub fn request_digest(method: &str, target: &str, tool: &str, body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"workflow-action-request-v1\0");
    for part in [method.as_bytes(), target.as_bytes(), tool.as_bytes(), body] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    format!("sha256:{}", hex::encode(hash.finalize()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Decision {
    pub binding: Binding,
    pub decision_id: Uuid,
    pub owner: Owner,
    pub generation: i64,
    /// Relative allowance only. Gateway measures from *before* authorize; never
    /// compare this database timestamp to a timestamp on another host.
    pub lease_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DispatchState {
    Authorized,
    SendIntent,
    NotInitiated,
    Uncertain,
    Succeeded,
    Failed,
}
impl DispatchState {
    pub fn may_have_executed(self) -> bool {
        matches!(
            self,
            Self::SendIntent | Self::Uncertain | Self::Succeeded | Self::Failed
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Completion {
    pub decision: Decision,
    pub outcome: DispatchState,
    /// Artifact/receipt digest only; no arbitrary response, credentials or URL.
    pub evidence_digest: Option<String>,
}

/// A qualified receiver can settle only an already uncertain effect. The
/// receiver identity and its allowed tool set are authenticated by Workflow's
/// dedicated mTLS API before this value reaches the ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reconciliation {
    pub host_id: Uuid,
    pub action_id: Uuid,
    pub generation: i64,
    pub outcome: DispatchState,
    pub evidence_digest: String,
}

/// Gateway supplies observations from its verified caller and selected route.
/// The opaque reference selects server-owned authority; it cannot provide depth,
/// class, grant, budget generations, or other authorization claims.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionReference {
    pub host_id: Uuid,
    pub action_id: Uuid,
    pub calling_app: String,
    pub request_digest: String,
    pub tool_ref: Uuid,
    pub target: String,
    pub contract_digest: String,
}
impl ActionReference {
    pub fn matches(&self, b: &Binding) -> bool {
        self.host_id == b.host_id
            && self.action_id == b.action_id
            && self.calling_app == b.calling_app
            && self.request_digest == b.request_digest
            && self.tool_ref == b.tool_ref
            && self.target == b.target
            && self.contract_digest == b.contract_digest
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayRegistration {
    pub gateway_service: String,
    pub replica: Uuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegisterOwner {
    pub gateway_service: String,
    pub replica: Uuid,
    pub boot: Uuid,
}

impl ActionReference {
    pub fn from_binding(b: &Binding) -> Self {
        Self {
            host_id: b.host_id,
            action_id: b.action_id,
            calling_app: b.calling_app.clone(),
            request_digest: b.request_digest.clone(),
            tool_ref: b.tool_ref,
            target: b.target.clone(),
            contract_digest: b.contract_digest.clone(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StatusRequest {
    pub reference: ActionReference,
    pub owner: Owner,
}
