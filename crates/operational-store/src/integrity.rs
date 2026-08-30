//! Application-level integrity contracts used after operational data stops
//! relying on Config Server and cross-service foreign keys.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// Stable kinds shared by the admission API, persistence evidence, and
/// reconciliation events. These names are persisted; do not rename them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReferenceKind {
    HostScope,
    AgentDefinition,
    UserPrincipal,
    AgentPolicy,
    QuotaPolicy,
    ServicePool,
    RunnerBinding,
    WorkflowProcess,
    WorkflowTask,
    WorkflowApproval,
    ExecutionSession,
    ExecutionAttempt,
    SchedulingRequest,
    MemoryDirectiveTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReferenceAuthority {
    pub host_id: Uuid,
    pub kind: ReferenceKind,
    pub target_id: Uuid,
    pub version: Option<u64>,
    pub publication_id: Option<Uuid>,
    pub content_digest: String,
    pub audience: String,
    pub revoked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReferenceClaim {
    pub host_id: Uuid,
    pub kind: ReferenceKind,
    pub target_id: Uuid,
    pub version: Option<u64>,
    pub publication_id: Option<Uuid>,
    pub content_digest: String,
    pub audience: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AcceptedReference {
    pub reference_id: Uuid,
    pub host_id: Uuid,
    pub kind: ReferenceKind,
    pub target_id: Uuid,
    pub version: Option<u64>,
    pub publication_id: Option<Uuid>,
    pub content_digest: String,
    pub audience: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReconciliationStatus {
    Current,
    Missing,
    Stale,
    Revoked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReferenceStatusEvent {
    pub event_id: Uuid,
    pub reference_id: Uuid,
    pub host_id: Uuid,
    pub kind: ReferenceKind,
    pub target_id: Uuid,
    pub accepted_digest: String,
    pub observed_digest: Option<String>,
    pub status: ReconciliationStatus,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdmissionError {
    #[error("reference authority is missing")]
    Missing,
    #[error("reference host scope does not match")]
    HostScope,
    #[error("reference target does not match")]
    Target,
    #[error("reference audience does not match")]
    Audience,
    #[error("reference version does not match")]
    Version,
    #[error("reference publication does not match")]
    Publication,
    #[error("reference digest is not canonical SHA-256")]
    InvalidDigest,
    #[error("reference digest is stale")]
    StaleDigest,
    #[error("reference authority is revoked")]
    Revoked,
}

pub fn canonical_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Admit a reference only from an already authenticated, audience-scoped
/// projection. A payload-supplied identifier is never sufficient by itself.
pub fn admit_reference(
    claim: &ReferenceClaim,
    authority: Option<&ReferenceAuthority>,
) -> Result<AcceptedReference, AdmissionError> {
    if !canonical_sha256(&claim.content_digest) {
        return Err(AdmissionError::InvalidDigest);
    }
    let authority = authority.ok_or(AdmissionError::Missing)?;
    if authority.revoked {
        return Err(AdmissionError::Revoked);
    }
    if authority.host_id != claim.host_id {
        return Err(AdmissionError::HostScope);
    }
    if authority.kind != claim.kind || authority.target_id != claim.target_id {
        return Err(AdmissionError::Target);
    }
    if authority.audience != claim.audience {
        return Err(AdmissionError::Audience);
    }
    if authority.version != claim.version {
        return Err(AdmissionError::Version);
    }
    if authority.publication_id != claim.publication_id {
        return Err(AdmissionError::Publication);
    }
    if !canonical_sha256(&authority.content_digest) {
        return Err(AdmissionError::InvalidDigest);
    }
    if authority.content_digest != claim.content_digest {
        return Err(AdmissionError::StaleDigest);
    }
    Ok(AcceptedReference {
        reference_id: Uuid::now_v7(),
        host_id: claim.host_id,
        kind: claim.kind,
        target_id: claim.target_id,
        version: claim.version,
        publication_id: claim.publication_id,
        content_digest: claim.content_digest.clone(),
        audience: claim.audience.clone(),
    })
}

pub fn reconcile_reference(
    accepted: &AcceptedReference,
    authority: Option<&ReferenceAuthority>,
) -> ReconciliationStatus {
    let Some(authority) = authority else {
        return ReconciliationStatus::Missing;
    };
    if authority.revoked {
        return ReconciliationStatus::Revoked;
    }
    if authority.host_id != accepted.host_id
        || authority.kind != accepted.kind
        || authority.target_id != accepted.target_id
        || authority.version != accepted.version
        || authority.publication_id != accepted.publication_id
        || authority.audience != accepted.audience
        || authority.content_digest != accepted.content_digest
    {
        return ReconciliationStatus::Stale;
    }
    ReconciliationStatus::Current
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryReplacement {
    pub constraint: &'static str,
    pub owner: &'static str,
    pub kind: ReferenceKind,
}

/// The executable Phase 2 boundary inventory. It intentionally has the same
/// 24 entries as implementation/phase0/foreign-key-boundary-v1.json.
pub const BOUNDARY_REPLACEMENTS: &[BoundaryReplacement] = &[
    BoundaryReplacement {
        constraint: "agent_memory_bank_t_host_id_agent_def_id_fkey",
        owner: "light-agent-memory",
        kind: ReferenceKind::AgentDefinition,
    },
    BoundaryReplacement {
        constraint: "agent_memory_bank_t_host_id_fkey",
        owner: "agent-store",
        kind: ReferenceKind::HostScope,
    },
    BoundaryReplacement {
        constraint: "agent_memory_bank_t_user_id_fkey",
        owner: "light-agent-memory",
        kind: ReferenceKind::UserPrincipal,
    },
    BoundaryReplacement {
        constraint: "agent_memory_entity_t_user_id_fkey",
        owner: "light-agent-memory",
        kind: ReferenceKind::UserPrincipal,
    },
    BoundaryReplacement {
        constraint: "agent_policy_snapshot_t_host_id_agent_def_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::AgentDefinition,
    },
    BoundaryReplacement {
        constraint: "agent_policy_snapshot_t_host_id_fkey",
        owner: "agent-store",
        kind: ReferenceKind::HostScope,
    },
    BoundaryReplacement {
        constraint: "agent_quota_usage_t_host_id_quota_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::QuotaPolicy,
    },
    BoundaryReplacement {
        constraint: "agent_session_t_host_id_agent_def_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::AgentDefinition,
    },
    BoundaryReplacement {
        constraint: "agent_session_service_pool_fk",
        owner: "light-agent",
        kind: ReferenceKind::ServicePool,
    },
    BoundaryReplacement {
        constraint: "agent_session_t_host_id_fkey",
        owner: "agent-store",
        kind: ReferenceKind::HostScope,
    },
    BoundaryReplacement {
        constraint: "runner_request_edge_binding_fk",
        owner: "controller-rs-runner",
        kind: ReferenceKind::RunnerBinding,
    },
    BoundaryReplacement {
        constraint: "runner_session_t_host_id_fkey",
        owner: "controller-rs-runner",
        kind: ReferenceKind::HostScope,
    },
    BoundaryReplacement {
        constraint: "execution_attempt_t_host_id_process_id_fkey",
        owner: "light-workflow",
        kind: ReferenceKind::WorkflowProcess,
    },
    BoundaryReplacement {
        constraint: "execution_attempt_t_host_id_task_id_fkey",
        owner: "light-workflow",
        kind: ReferenceKind::WorkflowTask,
    },
    BoundaryReplacement {
        constraint: "execution_fixed_action_t_host_id_approval_id_fkey",
        owner: "execution-fixed-action",
        kind: ReferenceKind::WorkflowApproval,
    },
    BoundaryReplacement {
        constraint: "runner_scheduling_request_t_host_id_process_id_fkey",
        owner: "controller-rs-runner",
        kind: ReferenceKind::WorkflowProcess,
    },
    BoundaryReplacement {
        constraint: "runner_scheduling_request_t_host_id_task_id_fkey",
        owner: "controller-rs-runner",
        kind: ReferenceKind::WorkflowTask,
    },
    BoundaryReplacement {
        constraint: "runner_scheduling_request_approval_fk",
        owner: "controller-rs-runner",
        kind: ReferenceKind::WorkflowApproval,
    },
    BoundaryReplacement {
        constraint: "agent_action_attempt_t_host_id_execution_attempt_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::ExecutionAttempt,
    },
    BoundaryReplacement {
        constraint: "agent_approval_t_host_id_consumed_execution_attempt_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::ExecutionAttempt,
    },
    BoundaryReplacement {
        constraint: "agent_session_t_host_id_execution_session_id_fkey",
        owner: "light-agent",
        kind: ReferenceKind::ExecutionSession,
    },
    BoundaryReplacement {
        constraint: "agent_turn_execution_attempt_fk",
        owner: "light-agent",
        kind: ReferenceKind::ExecutionAttempt,
    },
    BoundaryReplacement {
        constraint: "agent_turn_scheduling_request_fk",
        owner: "light-agent",
        kind: ReferenceKind::SchedulingRequest,
    },
    BoundaryReplacement {
        constraint: "agent_memory_directive_t_host_id_bank_id_fkey",
        owner: "light-portal-agent-publication",
        kind: ReferenceKind::MemoryDirectiveTarget,
    },
];

pub const RETAINED_AGENT_MEMORY_CONSTRAINT: &str = "agent_session_t_host_id_bank_id_fkey";

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: char) -> String {
        format!("sha256:{}", seed.to_string().repeat(64))
    }

    fn authority(kind: ReferenceKind) -> ReferenceAuthority {
        ReferenceAuthority {
            host_id: Uuid::now_v7(),
            kind,
            target_id: Uuid::now_v7(),
            version: Some(3),
            publication_id: Some(Uuid::now_v7()),
            content_digest: digest('a'),
            audience: "agent".into(),
            revoked: false,
        }
    }

    fn claim(authority: &ReferenceAuthority) -> ReferenceClaim {
        ReferenceClaim {
            host_id: authority.host_id,
            kind: authority.kind,
            target_id: authority.target_id,
            version: authority.version,
            publication_id: authority.publication_id,
            content_digest: authority.content_digest.clone(),
            audience: authority.audience.clone(),
        }
    }

    #[test]
    fn inventory_is_complete_and_unique() {
        assert_eq!(BOUNDARY_REPLACEMENTS.len(), 24);
        let mut constraints = BOUNDARY_REPLACEMENTS
            .iter()
            .map(|replacement| replacement.constraint)
            .collect::<Vec<_>>();
        constraints.sort_unstable();
        constraints.dedup();
        assert_eq!(constraints.len(), 24);
        assert!(!constraints.contains(&RETAINED_AGENT_MEMORY_CONSTRAINT));
    }

    #[test]
    fn every_boundary_rejects_missing_denied_and_stale_references() {
        for replacement in BOUNDARY_REPLACEMENTS {
            let authority = authority(replacement.kind);
            let valid = claim(&authority);
            assert_eq!(
                admit_reference(&valid, None),
                Err(AdmissionError::Missing),
                "{} accepted a missing authority",
                replacement.constraint
            );

            let mut denied = valid.clone();
            denied.audience = "wrong-audience".into();
            assert_eq!(
                admit_reference(&denied, Some(&authority)),
                Err(AdmissionError::Audience),
                "{} accepted a denied audience",
                replacement.constraint
            );

            let mut stale = valid.clone();
            stale.content_digest = digest('b');
            assert_eq!(
                admit_reference(&stale, Some(&authority)),
                Err(AdmissionError::StaleDigest),
                "{} accepted a stale digest",
                replacement.constraint
            );
        }
    }

    #[test]
    fn every_boundary_reconciles_current_missing_stale_and_revoked() {
        for replacement in BOUNDARY_REPLACEMENTS {
            let authority = authority(replacement.kind);
            let accepted = admit_reference(&claim(&authority), Some(&authority)).unwrap();
            assert_eq!(
                reconcile_reference(&accepted, Some(&authority)),
                ReconciliationStatus::Current,
                "{} did not reconcile as current",
                replacement.constraint
            );
            assert_eq!(
                reconcile_reference(&accepted, None),
                ReconciliationStatus::Missing,
                "{} did not reconcile as missing",
                replacement.constraint
            );

            let mut stale = authority.clone();
            stale.content_digest = digest('b');
            assert_eq!(
                reconcile_reference(&accepted, Some(&stale)),
                ReconciliationStatus::Stale,
                "{} did not reconcile as stale",
                replacement.constraint
            );

            let mut revoked = authority;
            revoked.revoked = true;
            assert_eq!(
                reconcile_reference(&accepted, Some(&revoked)),
                ReconciliationStatus::Revoked,
                "{} did not reconcile as revoked",
                replacement.constraint
            );
        }
    }
}
