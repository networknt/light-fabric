//! Transport-neutral contracts shared by native `light-agent` A2A and the
//! external-integration `light-a2a` service.

use a2a_protocol::A2aOperation;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use uuid::Uuid;

pub const A2A_SPEC_V1_TAG: &str = "v1.0.1";
pub const A2A_SPEC_V1_COMMIT: &str = "3303592588e388e62e0f69f701af531d2f4e3991";
pub const A2A_SPEC_V03_TAG: &str = "v0.3.0";
pub const A2A_SPEC_V03_COMMIT: &str = "210f03d426e2f2fa92000e14ef0de3b7ba15aee5";
pub const A2A_TCK_VERSION: &str = "1.0.0";
pub const A2A_TCK_COMMIT: &str = "5996b79f9cefa6fc390980e383e358a66fb9e49e";
pub const CANONICAL_PROJECTION_PROFILE: &str = "light-a2a-projection-json-v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeIdentity {
    pub host: String,
    pub service_id: String,
    pub env_tag: String,
}

impl RuntimeIdentity {
    pub fn new(host: &str, service_id: &str, env_tag: &str) -> Result<Self, A2aError> {
        let identity = Self {
            host: canonical_host(host)?,
            service_id: service_id.trim().to_string(),
            env_tag: env_tag.trim().to_string(),
        };
        if identity.service_id.is_empty() || identity.env_tag.is_empty() {
            return Err(A2aError::InvalidRuntimeIdentity);
        }
        Ok(identity)
    }

    pub fn validate_against(
        &self,
        host: &str,
        service_id: &str,
        env_tag: &str,
    ) -> Result<(), A2aError> {
        if self != &Self::new(host, service_id, env_tag)? {
            return Err(A2aError::InvalidRuntimeIdentity);
        }
        Ok(())
    }
}

pub fn canonical_host(value: &str) -> Result<String, A2aError> {
    let host = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty()
        || host.contains("://")
        || host.contains('/')
        || host.contains('?')
        || host.contains('#')
        || host.chars().any(char::is_whitespace)
    {
        return Err(A2aError::InvalidRuntimeIdentity);
    }
    Ok(host)
}

/// Deterministic JSON for Portal runtime projections. Object keys are sorted;
/// arrays retain order. The profile deliberately does not claim RFC 8785/JCS.
pub fn canonical_projection_json(value: &Value) -> Result<Vec<u8>, A2aError> {
    fn write_value(value: &Value, output: &mut Vec<u8>) -> Result<(), A2aError> {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                output.extend(serde_json::to_vec(value).map_err(|_| A2aError::Canonicalization)?)
            }
            Value::Array(values) => {
                output.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    write_value(value, output)?;
                }
                output.push(b']');
            }
            Value::Object(values) => {
                output.push(b'{');
                let mut entries = values.iter().collect::<Vec<_>>();
                entries.sort_by(|left, right| left.0.cmp(right.0));
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        output.push(b',');
                    }
                    output.extend(serde_json::to_vec(key).map_err(|_| A2aError::Canonicalization)?);
                    output.push(b':');
                    write_value(value, output)?;
                }
                output.push(b'}');
            }
        }
        Ok(())
    }

    let mut output = Vec::new();
    write_value(value, &mut output)?;
    Ok(output)
}

pub fn canonical_projection_digest(value: &Value) -> Result<String, A2aError> {
    let digest = Sha256::digest(canonical_projection_json(value)?);
    Ok(format_sha256(digest))
}

pub fn request_digest(value: &[u8]) -> String {
    format_sha256(Sha256::digest(value))
}

fn format_sha256(digest: impl IntoIterator<Item = u8>) -> String {
    let mut encoded = String::with_capacity(71);
    encoded.push_str("sha256:");
    for byte in digest {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inbound => "INBOUND",
            Self::Outbound => "OUTBOUND",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

impl TaskState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Submitted => "SUBMITTED",
            Self::Working => "WORKING",
            Self::InputRequired => "INPUT_REQUIRED",
            Self::AuthRequired => "AUTH_REQUIRED",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Canceled => "CANCELED",
            Self::Rejected => "REJECTED",
        }
    }

    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Canceled | Self::Rejected
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizedInvocation {
    pub host_id: Uuid,
    pub audience: String,
    pub principal_subject: String,
    pub caller_agent_ref: String,
    pub target_agent_ref: String,
    pub binding_id: Uuid,
    pub policy_digest: String,
    pub publication_id: Uuid,
    pub direction: Direction,
    pub idempotency_key: String,
    pub request_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outbound: Option<OutboundInvocationConstraints>,
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboundInvocationConstraints {
    pub delegation_id: Uuid,
    pub environment: String,
    pub data_boundary_digest: String,
    pub delegation_depth: u16,
    pub maximum_delegation_depth: u16,
    pub remaining_budget_units: u64,
    pub deadline: DateTime<Utc>,
    #[serde(default)]
    pub call_chain: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
}

impl AuthorizedInvocation {
    pub fn validate(&self, expected_audience: &str, now: DateTime<Utc>) -> Result<(), A2aError> {
        if self.audience != expected_audience {
            return Err(A2aError::WrongAudience);
        }
        if self.expires_at <= now || self.issued_at > now + chrono::Duration::minutes(1) {
            return Err(A2aError::Expired);
        }
        if self.principal_subject.trim().is_empty()
            || self.caller_agent_ref.trim().is_empty()
            || self.target_agent_ref.trim().is_empty()
            || self.idempotency_key.trim().is_empty()
            || !self.policy_digest.starts_with("sha256:")
            || !self.request_digest.starts_with("sha256:")
        {
            return Err(A2aError::InvalidInvocation);
        }
        match (self.direction, self.outbound.as_ref()) {
            (Direction::Outbound, Some(constraints)) => {
                if constraints.environment.trim().is_empty()
                    || !constraints.data_boundary_digest.starts_with("sha256:")
                    || constraints.data_boundary_digest.len() != 71
                    || constraints.delegation_depth > constraints.maximum_delegation_depth
                    || constraints.remaining_budget_units == 0
                    || constraints.deadline <= now
                    || constraints.deadline > self.expires_at
                    || constraints
                        .call_chain
                        .iter()
                        .any(|item| item.trim().is_empty())
                    || constraints.call_chain.iter().collect::<BTreeSet<_>>().len()
                        != constraints.call_chain.len()
                    || constraints
                        .call_chain
                        .iter()
                        .any(|item| item == &self.target_agent_ref)
                    || constraints
                        .skill_id
                        .as_ref()
                        .is_some_and(|value| value.trim().is_empty())
                {
                    return Err(A2aError::InvalidInvocation);
                }
            }
            (Direction::Outbound, None) | (Direction::Inbound, Some(_)) => {
                return Err(A2aError::InvalidInvocation);
            }
            (Direction::Inbound, None) => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationAuthority {
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: String,
    pub directions: BTreeSet<Direction>,
    pub operations: BTreeSet<A2aOperation>,
    #[serde(default)]
    pub principal_prefixes: Vec<String>,
}

impl InvocationAuthority {
    pub fn authorize(
        &self,
        invocation: &AuthorizedInvocation,
        operation: A2aOperation,
    ) -> Result<(), A2aError> {
        if invocation.binding_id != self.binding_id
            || invocation.publication_id != self.publication_id
            || invocation.policy_digest != self.policy_digest
            || !self.directions.contains(&invocation.direction)
            || !self.operations.contains(&operation)
            || (!self.principal_prefixes.is_empty()
                && !self
                    .principal_prefixes
                    .iter()
                    .any(|prefix| invocation.principal_subject.starts_with(prefix)))
        {
            return Err(A2aError::PolicyDenied);
        }
        Ok(())
    }
}

pub fn sign_authorized_invocation(
    invocation: &AuthorizedInvocation,
    body: &[u8],
    key: &[u8],
) -> Result<(String, String), A2aError> {
    if key.len() < 32 || invocation.request_digest != request_digest(body) {
        return Err(A2aError::InvalidInvocation);
    }
    let context = serde_json::to_vec(invocation).map_err(|_| A2aError::InvalidInvocation)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| A2aError::InvalidInvocation)?;
    mac.update(&context);
    mac.update(body);
    Ok((
        URL_SAFE_NO_PAD.encode(context),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()),
    ))
}

pub fn verify_authorized_invocation(
    encoded_context: &str,
    encoded_signature: &str,
    body: &[u8],
    key: &[u8],
    expected_audience: &str,
    now: DateTime<Utc>,
) -> Result<AuthorizedInvocation, A2aError> {
    if key.len() < 32 {
        return Err(A2aError::InvalidInvocation);
    }
    let context = URL_SAFE_NO_PAD
        .decode(encoded_context)
        .map_err(|_| A2aError::InvalidInvocation)?;
    let signature = URL_SAFE_NO_PAD
        .decode(encoded_signature)
        .map_err(|_| A2aError::InvalidInvocation)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| A2aError::InvalidInvocation)?;
    mac.update(&context);
    mac.update(body);
    mac.verify_slice(&signature)
        .map_err(|_| A2aError::InvalidInvocation)?;
    let invocation = serde_json::from_slice::<AuthorizedInvocation>(&context)
        .map_err(|_| A2aError::InvalidInvocation)?;
    if invocation.request_digest != request_digest(body) {
        return Err(A2aError::InvalidInvocation);
    }
    invocation.validate(expected_audience, now)?;
    Ok(invocation)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TaskSnapshot {
    pub task_id: Uuid,
    pub context_id: Uuid,
    pub state: TaskState,
    pub direction: Direction,
    pub target_agent_ref: String,
    pub result: Option<Value>,
    pub error: Option<Value>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactDescriptor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtifactVisibility {
    TaskOwner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactDescriptor {
    pub artifact_id: Uuid,
    pub logical_name: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub content_digest: String,
    pub visibility: ArtifactVisibility,
    pub retention_deadline: DateTime<Utc>,
    pub provenance_digest: String,
}

impl ArtifactDescriptor {
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), A2aError> {
        if self.logical_name.trim().is_empty()
            || self.media_type.trim().is_empty()
            || self.size_bytes == 0
            || !self.content_digest.starts_with("sha256:")
            || !self.provenance_digest.starts_with("sha256:")
            || self.retention_deadline <= now
        {
            return Err(A2aError::InvalidArtifact);
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum A2aError {
    #[error("A2A runtime identity must be the canonical host, serviceId, and envTag triple")]
    InvalidRuntimeIdentity,
    #[error("A2A projection cannot be canonicalized")]
    Canonicalization,
    #[error("A2A artifact descriptor violates lifecycle policy")]
    InvalidArtifact,
    #[error("A2A invocation has the wrong audience")]
    WrongAudience,
    #[error("A2A fine-grained policy denied the invocation")]
    PolicyDenied,
    #[error("A2A invocation is expired or not yet valid")]
    Expired,
    #[error("A2A invocation is malformed")]
    InvalidInvocation,
    #[error("A2A idempotency key was replayed with different content")]
    Replay,
    #[error("A2A task ownership does not match the authorized invocation")]
    WrongTaskOwner,
    #[error("A2A task is not cancellable")]
    NotCancellable,
    #[error("A2A task was not found")]
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation() -> AuthorizedInvocation {
        let now = Utc::now();
        AuthorizedInvocation {
            host_id: Uuid::new_v4(),
            audience: "light-a2a".into(),
            principal_subject: "user:1".into(),
            caller_agent_ref: "agent:caller".into(),
            target_agent_ref: "agent:target".into(),
            binding_id: Uuid::new_v4(),
            policy_digest: format!("sha256:{}", "a".repeat(64)),
            publication_id: Uuid::new_v4(),
            direction: Direction::Inbound,
            idempotency_key: "message-1".into(),
            request_digest: format!("sha256:{}", "b".repeat(64)),
            outbound: None,
            issued_at: now,
            expires_at: now + chrono::Duration::minutes(5),
        }
    }

    #[test]
    fn audience_and_expiry_fail_closed() {
        let now = Utc::now();
        let mut value = invocation();
        assert_eq!(value.validate("light-a2a", now), Ok(()));
        value.audience = "light-agent".into();
        assert_eq!(
            value.validate("light-a2a", now),
            Err(A2aError::WrongAudience)
        );
        value.audience = "light-a2a".into();
        value.expires_at = now;
        assert_eq!(value.validate("light-a2a", now), Err(A2aError::Expired));
    }

    #[test]
    fn outbound_invocation_requires_bounded_non_looping_delegation() {
        let now = Utc::now();
        let mut value = invocation();
        value.direction = Direction::Outbound;
        assert_eq!(
            value.validate("light-a2a", now),
            Err(A2aError::InvalidInvocation)
        );
        value.outbound = Some(OutboundInvocationConstraints {
            delegation_id: Uuid::new_v4(),
            environment: "dev".into(),
            data_boundary_digest: format!("sha256:{}", "c".repeat(64)),
            delegation_depth: 1,
            maximum_delegation_depth: 4,
            remaining_budget_units: 1024,
            deadline: now + chrono::Duration::minutes(1),
            call_chain: vec![value.caller_agent_ref.clone()],
            skill_id: Some("account.lookup".into()),
        });
        assert!(value.validate("light-a2a", now).is_ok());
        value
            .outbound
            .as_mut()
            .unwrap()
            .call_chain
            .push(value.target_agent_ref.clone());
        assert_eq!(
            value.validate("light-a2a", now),
            Err(A2aError::InvalidInvocation)
        );
    }

    #[test]
    fn signed_context_binds_the_exact_body_and_audience() {
        let key = [7_u8; 32];
        let body = br#"{"jsonrpc":"2.0"}"#;
        let mut value = invocation();
        value.request_digest = request_digest(body);
        let (context, signature) = sign_authorized_invocation(&value, body, &key).unwrap();
        assert_eq!(
            verify_authorized_invocation(&context, &signature, body, &key, "light-a2a", Utc::now())
                .unwrap(),
            value
        );
        assert!(
            verify_authorized_invocation(
                &context,
                &signature,
                br#"{"jsonrpc":"changed"}"#,
                &key,
                "light-a2a",
                Utc::now()
            )
            .is_err()
        );
    }

    #[test]
    fn invocation_authority_is_operation_and_principal_scoped() {
        let value = invocation();
        let authority = InvocationAuthority {
            binding_id: value.binding_id,
            publication_id: value.publication_id,
            policy_digest: value.policy_digest.clone(),
            directions: [Direction::Inbound].into_iter().collect(),
            operations: [A2aOperation::GetTask].into_iter().collect(),
            principal_prefixes: vec!["user:".into()],
        };
        assert!(authority.authorize(&value, A2aOperation::GetTask).is_ok());
        assert_eq!(
            authority.authorize(&value, A2aOperation::CancelTask),
            Err(A2aError::PolicyDenied)
        );
    }

    #[test]
    fn runtime_identity_is_canonical_and_has_no_instance_uuid() {
        let identity = RuntimeIdentity::new(
            " Agent.Example.COM. ",
            "com.networknt.agent.account-1.0.0",
            "dev",
        )
        .unwrap();
        assert_eq!(identity.host, "agent.example.com");
        assert!(
            identity
                .validate_against(
                    "agent.example.com",
                    "com.networknt.agent.account-1.0.0",
                    "dev"
                )
                .is_ok()
        );
        assert!(RuntimeIdentity::new("https://agent.example.com", "service", "dev").is_err());
    }

    #[test]
    fn projection_digest_sorts_objects_but_preserves_array_order() {
        let value = serde_json::json!({"z": [2, 1], "a": {"d": true, "b": "x"}});
        assert_eq!(
            String::from_utf8(canonical_projection_json(&value).unwrap()).unwrap(),
            r#"{"a":{"b":"x","d":true},"z":[2,1]}"#
        );
        assert_eq!(canonical_projection_digest(&value).unwrap().len(), 71);
    }

    #[test]
    fn phase_zero_golden_projection_digest_is_stable() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../contracts/a2a/phase0/canonical-projection.json"
        ))
        .unwrap();
        assert_eq!(
            canonical_projection_digest(&fixture).unwrap(),
            "sha256:c76a0601c0970a66b6addde5b9220ad167b508b59afc2535194b8766582ab9dd"
        );
    }

    #[test]
    fn phase_zero_upstream_revisions_are_machine_pinned() {
        let baseline: Value =
            serde_json::from_str(include_str!("../../../contracts/a2a/phase0/baseline.json"))
                .unwrap();
        assert_eq!(baseline["a2aSpecifications"]["1.0"]["tag"], A2A_SPEC_V1_TAG);
        assert_eq!(
            baseline["a2aSpecifications"]["1.0"]["commit"],
            A2A_SPEC_V1_COMMIT
        );
        assert_eq!(
            baseline["a2aSpecifications"]["0.3"]["tag"],
            A2A_SPEC_V03_TAG
        );
        assert_eq!(
            baseline["a2aSpecifications"]["0.3"]["commit"],
            A2A_SPEC_V03_COMMIT
        );
        assert_eq!(baseline["tck"]["version"], A2A_TCK_VERSION);
        assert_eq!(baseline["tck"]["commit"], A2A_TCK_COMMIT);
        assert_eq!(
            baseline["canonicalizationProfile"],
            CANONICAL_PROJECTION_PROFILE
        );
    }
}
