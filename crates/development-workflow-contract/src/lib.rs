//! Pure Phase 0 contracts. Callers must authenticate actors and atomically persist
//! state and receipts in Phase 1; serialized receipts are not authorization.
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub mod publication;
pub mod review;
pub mod stage;
pub use review::*;
pub use stage::*;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ContractError(pub &'static str);
pub type Result<T> = std::result::Result<T, ContractError>;

pub(crate) fn require(condition: bool, message: &'static str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(ContractError(message))
    }
}

/// Domain-separated, length-framed identifiers; never derived from model prose.
pub fn identity(domain: &str, parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in std::iter::once(domain).chain(parts.iter().copied()) {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("sha256:{:x}", hash.finalize())
}

pub fn digest_valid(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

pub(crate) fn fingerprint<T: Serialize>(value: &T) -> Result<String> {
    // All maps in the wire contracts are BTreeMaps; no arbitrary JSON values.
    let bytes = serde_json::to_string(value).map_err(|_| ContractError("invalid wire value"))?;
    Ok(identity("development-workflow/v1", &[&bytes]))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ArtifactRef {
    pub id: String,
    pub digest: String,
}
impl ArtifactRef {
    pub fn validate(&self) -> Result<()> {
        require(
            !self.id.trim().is_empty() && digest_valid(&self.digest),
            "invalid artifact reference",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct RepositorySnapshot {
    pub base_commit: String,
    pub tree: String,
    pub content_manifest: ArtifactRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CandidateSnapshot {
    pub feature_run_id: String,
    pub stage_execution_id: String,
    pub task_id: String,
    pub candidate_digest: String,
    pub checkpoint_digest: String,
    pub repositories: BTreeMap<String, RepositorySnapshot>,
    /// Receipt of completed durable export, never a runner-local path.
    pub package: ArtifactRef,
}
impl CandidateSnapshot {
    pub fn validate(&self) -> Result<()> {
        require(
            !self.feature_run_id.is_empty()
                && !self.stage_execution_id.is_empty()
                && !self.task_id.is_empty(),
            "missing snapshot binding",
        )?;
        require(
            digest_valid(&self.candidate_digest) && digest_valid(&self.checkpoint_digest),
            "invalid snapshot digest",
        )?;
        require(
            !self.repositories.is_empty(),
            "missing repository snapshots",
        )?;
        self.package.validate()?;
        for (repo, snapshot) in &self.repositories {
            require(
                !repo.is_empty() && git_oid(&snapshot.base_commit) && git_oid(&snapshot.tree),
                "invalid repository snapshot",
            )?;
            snapshot.content_manifest.validate()?;
        }
        Ok(())
    }
}
fn git_oid(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct DeltaReceipt {
    pub before: String,
    pub after: String,
    pub full_delta: ArtifactRef,
    pub changed_paths: BTreeSet<String>,
    pub affected_phases: BTreeSet<String>,
    pub cross_repository_contract: bool,
    pub requirements_or_design: bool,
    pub security_or_public_api: bool,
    pub migration: bool,
    pub uncertain_impact: bool,
    pub outside_fix_scope: bool,
}
impl DeltaReceipt {
    pub fn broad(&self) -> bool {
        self.affected_phases.len() != 1
            || self.cross_repository_contract
            || self.requirements_or_design
            || self.security_or_public_api
            || self.migration
            || self.uncertain_impact
            || self.outside_fix_scope
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ValidationReceipt {
    pub candidate: String,
    pub required_checks: BTreeSet<String>,
    pub passed_checks: BTreeMap<String, ArtifactRef>,
}
impl ValidationReceipt {
    pub fn validate(&self, candidate: &str) -> Result<()> {
        require(
            self.candidate == candidate && digest_valid(candidate),
            "stale validation",
        )?;
        require(
            !self.required_checks.is_empty(),
            "required checks must be declared",
        )?;
        require(
            self.required_checks
                .iter()
                .all(|c| self.passed_checks.contains_key(c)),
            "missing or skipped required check",
        )?;
        for evidence in self.passed_checks.values() {
            evidence.validate()?;
        }
        Ok(())
    }
}
