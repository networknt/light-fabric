//! Credential-free dispatch receipts. Missing or failed writes never prove zero dispatch.
use crate::{EvidenceClass, EvidenceRecord, StoreError, valid_digest};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DISPATCH_CONTRACT_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchPhase {
    Started,
    Attempt,
    Handoff,
    Terminal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DispatchObservation {
    pub request_audit_id: Uuid,
    pub dispatch_phase: DispatchPhase,
    pub dispatch_sequence: u32,
    pub upstream_attempt_count: u32,
    pub upstream_handoff_count: u32,
    pub observation_complete: bool,
    pub deployment_config_digest: String,
    pub observer_contract_version: u16,
    pub completion_phase: String,
}

impl DispatchObservation {
    pub fn validate(&self, record: &EvidenceRecord) -> Result<(), StoreError> {
        let event = match self.dispatch_phase {
            DispatchPhase::Started => "gateway.dispatch.observation.started",
            DispatchPhase::Attempt => "gateway.upstream.attempt",
            DispatchPhase::Handoff => "gateway.upstream.handoff",
            DispatchPhase::Terminal => "gateway.dispatch.observation.terminal",
        };
        if self.request_audit_id.is_nil()
            || !valid_digest(&self.deployment_config_digest)
            || self.observer_contract_version != DISPATCH_CONTRACT_VERSION
            || record.event_class != EvidenceClass::RequiredAudit
            || record.event_type != event
            || record.method != "GET"
            || record.endpoint != "/github/repos/*@get"
            || record.correlation_digest.is_none()
            || self.upstream_handoff_count > self.upstream_attempt_count
            || u64::from(self.dispatch_sequence)
                != u64::from(self.upstream_attempt_count)
                    + u64::from(self.upstream_handoff_count)
                    + u64::from(self.dispatch_phase == DispatchPhase::Terminal)
            || (self.dispatch_phase == DispatchPhase::Started && self.dispatch_sequence != 0)
            || (self.dispatch_phase != DispatchPhase::Terminal && self.observation_complete)
            || !matches!(
                self.completion_phase.as_str(),
                "in_progress" | "response" | "error"
            )
            || (self.dispatch_phase != DispatchPhase::Terminal
                && self.completion_phase != "in_progress")
        {
            return Err(StoreError::Scope(
                "invalid dispatch observation contract".into(),
            ));
        }
        Ok(())
    }
}
