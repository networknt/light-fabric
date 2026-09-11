//! Workspace inputs shared by interactive and workflow admission.
//! Host paths, credentials and execution generations are deliberately absent
//! from the browser input. Validation is not authentication: the admission
//! authority must authenticate context and obtain policy from a verified source.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use thiserror::Error;

pub const VERSION: u16 = 1;
pub const MAX_INSTRUCTION_BYTES: usize = 64 * 1024;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContractError {
    #[error("unsupported workspace protocol version")]
    Version,
    #[error("invalid workspace request field: {0}")]
    Field(&'static str),
    #[error("workspace access is not authorized")]
    Unauthorized,
    #[error("workspace membership revision changed")]
    MembershipChanged,
    #[error("workspace intent is not qualified on this runner")]
    UnsupportedIntent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceIntent {
    Inspect,
    Implement,
    Review,
    ImplementAndReview,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum TaskSelection {
    New {
        description: String,
    },
    Existing {
        #[serde(rename = "taskId")]
        task_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceRequest {
    pub schema_version: u16,
    pub request_id: String,
    pub workspace_id: String,
    pub expected_membership_revision: String,
    pub task: TaskSelection,
    pub intent: WorkspaceIntent,
    pub expected_checkpoint_digest: Option<String>,
    pub instruction: String,
}

impl WorkspaceRequest {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != VERSION {
            return Err(ContractError::Version);
        }
        for (key, value) in [
            ("requestId", &self.request_id),
            ("workspaceId", &self.workspace_id),
        ] {
            if !identifier(value) {
                return Err(ContractError::Field(key));
            }
        }
        if !digest(&self.expected_membership_revision) {
            return Err(ContractError::Field("expectedMembershipRevision"));
        }
        if self.instruction.trim().is_empty() || self.instruction.len() > MAX_INSTRUCTION_BYTES {
            return Err(ContractError::Field("instruction"));
        }
        match &self.task {
            TaskSelection::New { description }
                if description.trim().is_empty() || description.len() > 512 =>
            {
                return Err(ContractError::Field("description"));
            }
            TaskSelection::Existing { task_id } if !identifier(task_id) => {
                return Err(ContractError::Field("taskId"));
            }
            _ => {}
        }
        if self
            .expected_checkpoint_digest
            .as_ref()
            .is_some_and(|value| !digest(value))
        {
            return Err(ContractError::Field("expectedCheckpointDigest"));
        }
        if self.intent == WorkspaceIntent::Review
            && (matches!(self.task, TaskSelection::New { .. })
                || self.expected_checkpoint_digest.is_none())
        {
            return Err(ContractError::Field(
                "review requires an existing checkpoint",
            ));
        }
        if matches!(self.task, TaskSelection::New { .. })
            && self.expected_checkpoint_digest.is_some()
        {
            return Err(ContractError::Field("new task cannot have a checkpoint"));
        }
        Ok(())
    }

    /// Stable hash of typed input. Field order comes from structs, never a hash map.
    pub fn input_digest(&self) -> Result<String, ContractError> {
        self.validate()?;
        Ok(sha256(
            &serde_json::to_vec(self).expect("serializable contract"),
        ))
    }

    /// Scope the idempotency key to the authenticated caller, not a model name.
    pub fn admission_key(&self, context: &AdmissionContext) -> Result<String, ContractError> {
        self.validate()?;
        context.validate()?;
        Ok(sha256(
            &serde_json::to_vec(&(
                &context.host_id,
                &context.environment,
                &context.subject,
                &context.agent_id,
                &self.workspace_id,
                &self.request_id,
            ))
            .expect("serializable scope"),
        ))
    }
}

/// Constructed by the transport's authenticated admission layer, never decoded
/// from the browser request. Agent identity does not substitute for user access.
#[derive(Debug, Clone)]
pub struct AdmissionContext {
    pub host_id: String,
    pub environment: String,
    pub subject: String,
    pub agent_id: String,
    pub runner_id: String,
    pub coding_turn_authorized: bool,
}
impl AdmissionContext {
    pub fn validate(&self) -> Result<(), ContractError> {
        if [
            &self.host_id,
            &self.environment,
            &self.subject,
            &self.agent_id,
            &self.runner_id,
        ]
        .iter()
        .any(|v| v.is_empty() || v.len() > 255 || v.chars().any(char::is_control))
        {
            return Err(ContractError::Unauthorized);
        }
        Ok(())
    }
}

/// Published policy must be authenticated before constructing an admission.
/// The runner separately checks that local membership matches this revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceAccessPolicy {
    pub schema_version: u16,
    pub workspace_id: String,
    pub host_id: String,
    pub environment: String,
    pub runner_id: String,
    pub membership_revision: String,
    pub authorization_revision: u64,
    pub subjects: BTreeSet<String>,
    pub agents: BTreeSet<String>,
    pub intents: BTreeSet<WorkspaceIntent>,
}
impl WorkspaceAccessPolicy {
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.schema_version != VERSION {
            return Err(ContractError::Version);
        }
        if !identifier(&self.workspace_id)
            || !digest(&self.membership_revision)
            || self.authorization_revision == 0
            || self.subjects.is_empty()
            || self.agents.is_empty()
            || self.intents.is_empty()
            || [&self.host_id, &self.environment, &self.runner_id]
                .into_iter()
                .chain(self.subjects.iter())
                .chain(self.agents.iter())
                .any(|s| {
                    s.trim() != s
                        || s.is_empty()
                        || s.len() > 255
                        || s.chars().any(char::is_control)
                })
        {
            return Err(ContractError::Field("workspace binding"));
        }
        Ok(())
    }

    pub fn authorize(
        &self,
        request: &WorkspaceRequest,
        context: &AdmissionContext,
        qualified_intents: &BTreeSet<WorkspaceIntent>,
    ) -> Result<(), ContractError> {
        self.validate()?;
        request.validate()?;
        context.validate()?;
        if self.schema_version != VERSION {
            return Err(ContractError::Version);
        }
        if !context.coding_turn_authorized
            || self.workspace_id != request.workspace_id
            || self.host_id != context.host_id
            || self.environment != context.environment
            || self.runner_id != context.runner_id
            || !self.subjects.contains(&context.subject)
            || !self.agents.contains(&context.agent_id)
            || !self.intents.contains(&request.intent)
        {
            return Err(ContractError::Unauthorized);
        }
        if !digest(&self.membership_revision)
            || self.membership_revision != request.expected_membership_revision
        {
            return Err(ContractError::MembershipChanged);
        }
        if !qualified_intents.contains(&request.intent) {
            return Err(ContractError::UnsupportedIntent);
        }
        Ok(())
    }
}

/// Server-created execution envelope. This type is never accepted as a Chat
/// request. The runner verifies its policy against its owner-managed binding.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceExecutionSpec {
    pub request: WorkspaceRequest,
    pub binding: WorkspaceAccessPolicy,
    pub subject: String,
    pub agent_id: String,
}
impl WorkspaceExecutionSpec {
    pub fn context(&self) -> AdmissionContext {
        AdmissionContext {
            host_id: self.binding.host_id.clone(),
            environment: self.binding.environment.clone(),
            subject: self.subject.clone(),
            agent_id: self.agent_id.clone(),
            runner_id: self.binding.runner_id.clone(),
            coding_turn_authorized: true,
        }
    }
    pub fn validate(&self) -> Result<(), ContractError> {
        self.binding
            .authorize(&self.request, &self.context(), &standalone_intents())
    }
}

pub fn standalone_intents() -> BTreeSet<WorkspaceIntent> {
    BTreeSet::from([WorkspaceIntent::Inspect, WorkspaceIntent::Implement])
}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'-' | b'_'))
}
pub fn digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (WorkspaceRequest, AdmissionContext, WorkspaceAccessPolicy) {
        let request = WorkspaceRequest {
            schema_version: VERSION,
            request_id: "request-1".into(),
            workspace_id: "personal".into(),
            expected_membership_revision: sha256(b"membership"),
            task: TaskSelection::New {
                description: "Explain config publication".into(),
            },
            intent: WorkspaceIntent::Inspect,
            expected_checkpoint_digest: None,
            instruction: "Explain config publication".into(),
        };
        let context = AdmissionContext {
            host_id: "host".into(),
            environment: "dev".into(),
            subject: "steve".into(),
            agent_id: "codex".into(),
            runner_id: "runner".into(),
            coding_turn_authorized: true,
        };
        let policy = WorkspaceAccessPolicy {
            schema_version: VERSION,
            workspace_id: request.workspace_id.clone(),
            host_id: context.host_id.clone(),
            environment: context.environment.clone(),
            runner_id: context.runner_id.clone(),
            membership_revision: request.expected_membership_revision.clone(),
            authorization_revision: 1,
            subjects: BTreeSet::from([context.subject.clone()]),
            agents: BTreeSet::from([context.agent_id.clone()]),
            intents: BTreeSet::from([WorkspaceIntent::Inspect]),
        };
        (request, context, policy)
    }
    #[test]
    fn admission_requires_user_agent_host_environment_runner_policy_and_qualification() {
        let (request, context, policy) = fixture();
        assert!(
            policy
                .authorize(&request, &context, &policy.intents)
                .is_ok()
        );
        for changed in ["host", "environment", "subject", "agent", "runner", "turn"] {
            let mut bad = context.clone();
            match changed {
                "host" => bad.host_id = "other".into(),
                "environment" => bad.environment = "prod".into(),
                "subject" => bad.subject = "other".into(),
                "agent" => bad.agent_id = "other".into(),
                "runner" => bad.runner_id = "other".into(),
                _ => bad.coding_turn_authorized = false,
            }
            assert_eq!(
                policy.authorize(&request, &bad, &policy.intents),
                Err(ContractError::Unauthorized)
            );
        }
        assert_eq!(
            policy.authorize(&request, &context, &BTreeSet::new()),
            Err(ContractError::UnsupportedIntent)
        );
        let mut changed = request.clone();
        changed.expected_membership_revision = sha256(b"new");
        assert_eq!(
            policy.authorize(&changed, &context, &policy.intents),
            Err(ContractError::MembershipChanged)
        );
    }
    #[test]
    fn input_rejects_unknown_fields_paths_and_review_without_evidence() {
        let (request, _, _) = fixture();
        let mut value = serde_json::to_value(&request).unwrap();
        value["workspaceRoot"] = "/home/steve".into();
        assert!(serde_json::from_value::<WorkspaceRequest>(value).is_err());
        let mut changed = request.clone();
        changed.workspace_id = "../personal".into();
        assert!(changed.validate().is_err());
        changed = request.clone();
        changed.intent = WorkspaceIntent::Review;
        assert!(changed.validate().is_err());
        changed.task = TaskSelection::Existing {
            task_id: "task-1".into(),
        };
        changed.expected_checkpoint_digest = Some(sha256(b"checkpoint"));
        assert!(changed.validate().is_ok());
    }
    #[test]
    fn retries_have_stable_scoped_keys_but_changed_input_has_different_digest() {
        let (request, context, _) = fixture();
        let mut changed = request.clone();
        changed.instruction = "Different instruction".into();
        assert_eq!(
            request.admission_key(&context),
            changed.admission_key(&context)
        );
        assert_ne!(request.input_digest(), changed.input_digest());
        let mut other = context.clone();
        other.subject = "other".into();
        assert_ne!(
            request.admission_key(&context),
            request.admission_key(&other)
        );
        assert_eq!(
            request.input_digest(),
            serde_json::from_str::<WorkspaceRequest>(&serde_json::to_string(&request).unwrap())
                .unwrap()
                .input_digest()
        );
    }
}
