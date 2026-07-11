use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionCompatibility {
    pub host_id: Uuid,
    pub principal_id: String,
    pub agent_def_id: Uuid,
    pub base_revision: String,
    pub adapter_id: String,
    pub adapter_version: String,
    pub backend_id: String,
    pub compatibility_digest: String,
    pub policy_digest: String,
    pub credential_scope_digest: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SessionExpiry {
    pub agent_idle: DateTime<Utc>,
    pub agent_maximum: DateTime<Utc>,
    pub execution_policy: DateTime<Utc>,
    pub backend_native: DateTime<Utc>,
    pub broker_or_credential: DateTime<Utc>,
}
impl SessionExpiry {
    pub fn effective(&self) -> DateTime<Utc> {
        [
            self.agent_idle,
            self.agent_maximum,
            self.execution_policy,
            self.backend_native,
            self.broker_or_credential,
        ]
        .into_iter()
        .min()
        .unwrap()
    }
}
pub fn authorize_reuse(
    expected: &SessionCompatibility,
    actual: &SessionCompatibility,
    expiry: &SessionExpiry,
    cleanup_pending: bool,
) -> Result<(), SessionError> {
    if cleanup_pending || expiry.effective() <= Utc::now() {
        return Err(SessionError::Unavailable);
    }
    if expected != actual {
        return Err(SessionError::Mismatch);
    }
    Ok(())
}
pub fn approval_hold_until(
    requested: DateTime<Utc>,
    expiry: &SessionExpiry,
) -> Result<DateTime<Utc>, SessionError> {
    let value = requested.min(expiry.effective());
    if value <= Utc::now() {
        Err(SessionError::Unavailable)
    } else {
        Ok(value)
    }
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionError {
    #[error("session is expired or cleanup is pending")]
    Unavailable,
    #[error("session compatibility mismatch")]
    Mismatch,
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    fn c() -> SessionCompatibility {
        SessionCompatibility {
            host_id: Uuid::nil(),
            principal_id: "p".into(),
            agent_def_id: Uuid::nil(),
            base_revision: "a".repeat(40),
            adapter_id: "pi".into(),
            adapter_version: "1".into(),
            backend_id: "cube".into(),
            compatibility_digest: "c".into(),
            policy_digest: "p".into(),
            credential_scope_digest: "none".into(),
        }
    }
    fn e() -> SessionExpiry {
        let n = Utc::now();
        SessionExpiry {
            agent_idle: n + Duration::minutes(10),
            agent_maximum: n + Duration::hours(1),
            execution_policy: n + Duration::minutes(20),
            backend_native: n + Duration::minutes(30),
            broker_or_credential: n + Duration::minutes(5),
        }
    }
    #[test]
    fn minimum_expiry_and_every_dimension_fence_reuse() {
        let a = c();
        assert!(authorize_reuse(&a, &a, &e(), false).is_ok());
        let mut b = a.clone();
        b.principal_id = "other".into();
        assert_eq!(
            authorize_reuse(&a, &b, &e(), false),
            Err(SessionError::Mismatch)
        );
        assert!(
            approval_hold_until(Utc::now() + Duration::hours(2), &e()).unwrap()
                <= e().broker_or_credential
        );
    }
}
