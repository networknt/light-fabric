//! Transport-neutral contracts shared by native `light-agent` A2A and the
//! external-integration `light-a2a` service.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
    pub issued_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
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
        Ok(())
    }
}

pub fn sign_authorized_invocation(
    invocation: &AuthorizedInvocation,
    body: &[u8],
    key: &[u8],
) -> Result<(String, String), A2aError> {
    if key.len() < 32 {
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
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum A2aError {
    #[error("A2A invocation has the wrong audience")]
    WrongAudience,
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
    fn signed_context_binds_the_exact_body_and_audience() {
        let value = invocation();
        let key = [7_u8; 32];
        let body = br#"{"jsonrpc":"2.0"}"#;
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
}
