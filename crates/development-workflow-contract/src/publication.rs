//! Workflow-owned external-effect plans. These contracts do not authenticate a
//! caller or execute GitHub requests. The host must verify retained content and
//! atomically persist each transition before dispatching an effect.
use crate::{ArtifactRef, ContractError, Result, digest_valid, fingerprint, identity, require};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Destination {
    Issue {
        title: String,
    },
    Comment {
        issue: u64,
    },
    Document {
        branch: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_blob: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationPlan {
    pub feature_id: String,
    /// Stable Workflow-defined slot, not a model-supplied idempotency key.
    pub slot: String,
    pub revision: u64,
    pub candidate_digest: String,
    pub repository: String,
    pub destination: Destination,
    /// Exact retained bytes; the dispatcher must verify this digest before use.
    pub content: ArtifactRef,
}

/// Trusted pinned configuration, never taken from the publication request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationPolicy {
    pub repositories: BTreeSet<String>,
    pub document_branches: BTreeSet<String>,
    pub document_paths: BTreeSet<String>,
    pub allow_issues: bool,
    pub allow_comments: bool,
}

/// Fixed provider input, selected and authenticated by Workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationDelivery {
    pub key: String,
    pub plan: PublicationPlan,
    pub body: String,
}
impl PublicationDelivery {
    pub fn request_digest(&self) -> Result<String> {
        fingerprint(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProviderPublicationReceipt {
    pub key: String,
    pub request_digest: String,
    pub provider_id: String,
    pub resource_url: String,
    pub commit: Option<String>,
}

impl PublicationPlan {
    pub fn validate(&self, policy: &PublicationPolicy, accepted_candidate: &str) -> Result<()> {
        require(
            !self.feature_id.is_empty()
                && self.feature_id.len() <= 128
                && !self.slot.is_empty()
                && self.slot.len() <= 128
                && self
                    .slot
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && self.revision > 0,
            "invalid publication slot",
        )?;
        require(
            digest_valid(&self.candidate_digest) && self.candidate_digest == accepted_candidate,
            "publication candidate is not accepted",
        )?;
        self.content.validate()?;
        let parts: Vec<_> = self.repository.split('/').collect();
        require(
            parts.len() == 2
                && parts.iter().all(|s| {
                    !s.is_empty()
                        && *s != "."
                        && *s != ".."
                        && s.bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                })
                && policy.repositories.contains(&self.repository),
            "publication repository is not pinned",
        )?;
        match &self.destination {
            Destination::Issue { title } => require(
                policy.allow_issues
                    && !title.trim().is_empty()
                    && title.len() <= 256
                    && !title.chars().any(char::is_control),
                "issue publication denied",
            )?,
            Destination::Comment { issue } => require(
                policy.allow_comments && *issue > 0,
                "comment publication denied",
            )?,
            Destination::Document {
                branch,
                path,
                expected_blob,
            } => {
                require(
                    policy.document_branches.contains(branch) && safe_branch(branch),
                    "document branch is not pinned",
                )?;
                require(
                    policy.document_paths.contains(path) && safe_path(path),
                    "document path is not pinned",
                )?;
                require(
                    expected_blob
                        .as_ref()
                        .is_none_or(|value| super::git_oid(value)),
                    "invalid document compare-and-swap blob",
                )?;
            }
        }
        Ok(())
    }

    pub fn effect_id(&self) -> String {
        // Changing the body/target must conflict with the existing slot, not
        // silently allocate another external effect after a lost response.
        identity(
            "publication-effect/v1",
            &[&self.feature_id, &self.slot, &self.revision.to_string()],
        )
    }
}

fn safe_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.contains(['\\', '%', '?', '#'])
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != ".." && part != ".git")
}

fn safe_branch(value: &str) -> bool {
    safe_path(value)
        && !matches!(value, "main" | "master" | "develop" | "HEAD")
        && !value.starts_with('-')
        && !value.starts_with("refs/")
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains([' ', '~', '^', ':', '*', '['])
        && value
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with('.') && !part.ends_with(".lock"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum EffectState {
    Prepared,
    InFlight,
    /// Unknown is never permission to POST again. Reconcile the remote effect.
    Unknown,
    Confirmed {
        provider_id: String,
        verification: ArtifactRef,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicationEffect {
    pub id: String,
    pub request_digest: String,
    pub state: EffectState,
}

impl PublicationEffect {
    pub fn prepare(
        plan: &PublicationPlan,
        policy: &PublicationPolicy,
        candidate: &str,
    ) -> Result<Self> {
        plan.validate(policy, candidate)?;
        Ok(Self {
            id: plan.effect_id(),
            request_digest: fingerprint(plan)?,
            state: EffectState::Prepared,
        })
    }

    pub fn check_replay(&self, plan: &PublicationPlan) -> Result<()> {
        require(
            self.id == plan.effect_id() && self.request_digest == fingerprint(plan)?,
            "publication replay changed immutable request",
        )
    }

    pub fn begin(&mut self) -> Result<()> {
        require(
            self.state == EffectState::Prepared,
            "publication must reconcile before redispatch",
        )?;
        self.state = EffectState::InFlight;
        Ok(())
    }

    pub fn mark_unknown(&mut self) -> Result<()> {
        require(
            matches!(self.state, EffectState::InFlight | EffectState::Unknown),
            "effect is not in flight",
        )?;
        self.state = EffectState::Unknown;
        Ok(())
    }

    /// Call only after verifying the provider's immutable target/content proof.
    pub fn confirm(&mut self, provider_id: String, verification: ArtifactRef) -> Result<()> {
        verification.validate()?;
        require(
            !provider_id.is_empty() && provider_id.len() <= 512,
            "invalid publication provider identity",
        )?;
        let next = EffectState::Confirmed {
            provider_id,
            verification,
        };
        match &self.state {
            EffectState::InFlight | EffectState::Unknown => self.state = next,
            old if old == &next => {}
            _ => return Err(ContractError("publication confirmation conflict")),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PublicationPlan, PublicationPolicy) {
        (
            PublicationPlan {
                feature_id: "feature".into(),
                slot: "design-document".into(),
                revision: 1,
                candidate_digest: format!("sha256:{}", "a".repeat(64)),
                repository: "networknt/light-agent".into(),
                destination: Destination::Document {
                    branch: "qualification/phase1-publication".into(),
                    path: "qualification-design.md".into(),
                    expected_blob: None,
                },
                content: ArtifactRef {
                    id: "artifact".into(),
                    digest: format!("sha256:{}", "b".repeat(64)),
                },
            },
            PublicationPolicy {
                repositories: BTreeSet::from(["networknt/light-agent".into()]),
                document_branches: BTreeSet::from(["qualification/phase1-publication".into()]),
                document_paths: BTreeSet::from(["qualification-design.md".into()]),
                allow_issues: true,
                allow_comments: true,
            },
        )
    }
    #[test]
    fn uncertain_effect_never_redispatches_and_confirmation_is_immutable() {
        let (plan, policy) = fixture();
        let mut effect =
            PublicationEffect::prepare(&plan, &policy, &plan.candidate_digest).unwrap();
        assert!(
            effect
                .confirm("remote".into(), plan.content.clone())
                .is_err()
        );
        effect.begin().unwrap();
        assert!(effect.begin().is_err());
        effect.mark_unknown().unwrap();
        let mut recovered: PublicationEffect =
            serde_json::from_str(&serde_json::to_string(&effect).unwrap()).unwrap();
        recovered.check_replay(&plan).unwrap();
        assert!(recovered.begin().is_err());
        recovered
            .confirm("remote".into(), plan.content.clone())
            .unwrap();
        recovered
            .confirm("remote".into(), plan.content.clone())
            .unwrap();
        assert!(
            recovered
                .confirm("different".into(), plan.content.clone())
                .is_err()
        );
        assert!(recovered.begin().is_err());
    }
    #[test]
    fn changed_content_or_target_conflicts_instead_of_allocating_new_effect() {
        let (plan, policy) = fixture();
        let effect = PublicationEffect::prepare(&plan, &policy, &plan.candidate_digest).unwrap();
        let mut changed = plan.clone();
        changed.content.digest = format!("sha256:{}", "c".repeat(64));
        assert_eq!(plan.effect_id(), changed.effect_id());
        assert!(effect.check_replay(&changed).is_err());
        changed = plan.clone();
        changed.repository = "networknt/other".into();
        assert_eq!(plan.effect_id(), changed.effect_id());
        assert!(effect.check_replay(&changed).is_err());
        assert!(changed.validate(&policy, &plan.candidate_digest).is_err());
        assert!(plan.validate(&policy, &changed.content.digest).is_err());
    }
    #[test]
    fn pinned_destinations_still_reject_unsafe_paths_and_integration_branches() {
        let (mut plan, mut policy) = fixture();
        for path in [
            "../escape",
            ".git/config",
            "/absolute",
            "a//b",
            "a/%2e%2e/b",
        ] {
            policy.document_paths.insert(path.into());
            if let Destination::Document { path: p, .. } = &mut plan.destination {
                *p = path.into();
            }
            assert!(plan.validate(&policy, &plan.candidate_digest).is_err());
        }
        for branch in [
            "master",
            "develop",
            "main",
            "refs/heads/x",
            "a..b",
            "x.lock",
            "x@{1}",
        ] {
            policy.document_branches.insert(branch.into());
            plan.destination = Destination::Document {
                branch: branch.into(),
                path: "qualification-design.md".into(),
                expected_blob: None,
            };
            assert!(plan.validate(&policy, &plan.candidate_digest).is_err());
        }
    }
    #[test]
    fn issue_and_comment_permissions_are_independent() {
        let (mut plan, mut policy) = fixture();
        plan.destination = Destination::Issue {
            title: "qualification".into(),
        };
        plan.validate(&policy, &plan.candidate_digest).unwrap();
        policy.allow_issues = false;
        assert!(plan.validate(&policy, &plan.candidate_digest).is_err());
        plan.destination = Destination::Comment { issue: 1 };
        plan.validate(&policy, &plan.candidate_digest).unwrap();
        policy.allow_comments = false;
        assert!(plan.validate(&policy, &plan.candidate_digest).is_err());
    }
}
