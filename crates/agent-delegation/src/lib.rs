use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::Utc;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use thiserror::Error;
use uuid::Uuid;

pub const TOKEN_PREFIX: &str = "lad1";
pub const MIN_SECRET_BYTES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DelegationKind {
    ToolsList,
    ToolCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DelegationClaims {
    pub token_id: Uuid,
    pub kind: DelegationKind,
    pub issuer: String,
    pub audience: String,
    pub caller_subject: String,
    pub caller_claims: Value,
    pub agent_actor: String,
    pub host_id: Uuid,
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub action_attempt_id: Option<Uuid>,
    pub tool_ref: Option<Uuid>,
    pub tool_alias: Option<String>,
    pub destination: Option<String>,
    pub data_boundary_digest: String,
    pub policy_digest: String,
    pub replay_id: Uuid,
    pub issued_at: i64,
    pub expires_at: i64,
}

impl DelegationClaims {
    pub fn validate_binding(
        &self,
        audience: &str,
        kind: DelegationKind,
        tool_alias: Option<&str>,
        now: i64,
    ) -> Result<(), DelegationError> {
        if self.audience != audience {
            return Err(DelegationError::Audience);
        }
        if self.kind != kind {
            return Err(DelegationError::Kind);
        }
        if self.issued_at > now + 30
            || self.expires_at <= now
            || self.expires_at - self.issued_at > 300
        {
            return Err(DelegationError::Expired);
        }
        match kind {
            DelegationKind::ToolsList
                if self.action_attempt_id.is_some() || self.tool_alias.is_some() =>
            {
                return Err(DelegationError::Binding);
            }
            DelegationKind::ToolCall
                if self.action_attempt_id.is_none()
                    || self.tool_ref.is_none()
                    || self.tool_alias.as_deref() != tool_alias =>
            {
                return Err(DelegationError::Binding);
            }
            _ => {}
        }
        if self.policy_digest.is_empty() || self.data_boundary_digest.is_empty() {
            return Err(DelegationError::Binding);
        }
        Ok(())
    }
}

pub struct DelegationSigner {
    secret: Vec<u8>,
    issuer: String,
}

impl DelegationSigner {
    pub fn new(
        secret: impl AsRef<[u8]>,
        issuer: impl Into<String>,
    ) -> Result<Self, DelegationError> {
        if secret.as_ref().len() < MIN_SECRET_BYTES {
            return Err(DelegationError::WeakSecret);
        }
        Ok(Self {
            secret: secret.as_ref().to_vec(),
            issuer: issuer.into(),
        })
    }
    pub fn mint(&self, mut claims: DelegationClaims) -> Result<String, DelegationError> {
        claims.issuer = self.issuer.clone();
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let input = format!("{TOKEN_PREFIX}.{payload}");
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| DelegationError::WeakSecret)?;
        mac.update(input.as_bytes());
        Ok(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        ))
    }
}

pub struct DelegationVerifier {
    secret: Vec<u8>,
    issuer: String,
    audience: String,
}

impl DelegationVerifier {
    pub fn new(
        secret: impl AsRef<[u8]>,
        issuer: impl Into<String>,
        audience: impl Into<String>,
    ) -> Result<Self, DelegationError> {
        if secret.as_ref().len() < MIN_SECRET_BYTES {
            return Err(DelegationError::WeakSecret);
        }
        Ok(Self {
            secret: secret.as_ref().to_vec(),
            issuer: issuer.into(),
            audience: audience.into(),
        })
    }
    pub fn verify(
        &self,
        token: &str,
        kind: DelegationKind,
        tool_alias: Option<&str>,
    ) -> Result<DelegationClaims, DelegationError> {
        let claims = self.verify_token(token)?;
        claims.validate_binding(&self.audience, kind, tool_alias, Utc::now().timestamp())?;
        Ok(claims)
    }

    pub fn verify_token(&self, token: &str) -> Result<DelegationClaims, DelegationError> {
        let mut parts = token.split('.');
        let prefix = parts.next().ok_or(DelegationError::Malformed)?;
        let payload = parts.next().ok_or(DelegationError::Malformed)?;
        let signature = parts.next().ok_or(DelegationError::Malformed)?;
        if prefix != TOKEN_PREFIX || parts.next().is_some() {
            return Err(DelegationError::Malformed);
        }
        let input = format!("{prefix}.{payload}");
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| DelegationError::Malformed)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(&self.secret)
            .map_err(|_| DelegationError::WeakSecret)?;
        mac.update(input.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| DelegationError::Signature)?;
        let claims: DelegationClaims = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(payload)
                .map_err(|_| DelegationError::Malformed)?,
        )?;
        if claims.issuer != self.issuer {
            return Err(DelegationError::Issuer);
        }
        if claims.audience != self.audience {
            return Err(DelegationError::Audience);
        }
        if claims.issued_at > Utc::now().timestamp() + 30
            || claims.expires_at <= Utc::now().timestamp()
            || claims.expires_at - claims.issued_at > 300
        {
            return Err(DelegationError::Expired);
        }
        Ok(claims)
    }
}

#[derive(Debug, Error)]
pub enum DelegationError {
    #[error("delegation secret must contain at least 32 bytes")]
    WeakSecret,
    #[error("malformed delegation token")]
    Malformed,
    #[error("invalid delegation signature")]
    Signature,
    #[error("unexpected delegation issuer")]
    Issuer,
    #[error("unexpected delegation audience")]
    Audience,
    #[error("unexpected delegation kind")]
    Kind,
    #[error("delegation is expired or has an invalid lifetime")]
    Expired,
    #[error("delegation binding does not match the request")]
    Binding,
    #[error("delegation JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    fn claims(kind: DelegationKind) -> DelegationClaims {
        let now = Utc::now().timestamp();
        DelegationClaims {
            token_id: Uuid::now_v7(),
            kind,
            issuer: String::new(),
            audience: "light-gateway".into(),
            caller_subject: "user".into(),
            caller_claims: serde_json::json!({"roles":["reader"]}),
            agent_actor: "agent".into(),
            host_id: Uuid::now_v7(),
            session_id: Uuid::now_v7(),
            turn_id: Uuid::now_v7(),
            action_attempt_id: (kind == DelegationKind::ToolCall).then(Uuid::now_v7),
            tool_ref: (kind == DelegationKind::ToolCall).then(Uuid::now_v7),
            tool_alias: (kind == DelegationKind::ToolCall).then(|| "read".into()),
            destination: None,
            data_boundary_digest: "sha256:boundary".into(),
            policy_digest: "sha256:policy".into(),
            replay_id: Uuid::now_v7(),
            issued_at: now,
            expires_at: now + 60,
        }
    }
    #[test]
    fn token_is_action_and_tool_scoped_and_tamper_evident() {
        let secret = b"01234567890123456789012345678901";
        let token = DelegationSigner::new(secret, "light-agent")
            .unwrap()
            .mint(claims(DelegationKind::ToolCall))
            .unwrap();
        let verifier = DelegationVerifier::new(secret, "light-agent", "light-gateway").unwrap();
        assert!(
            verifier
                .verify(&token, DelegationKind::ToolCall, Some("read"))
                .is_ok()
        );
        assert!(matches!(
            verifier.verify(&token, DelegationKind::ToolCall, Some("write")),
            Err(DelegationError::Binding)
        ));
        assert!(matches!(
            verifier.verify(&(token + "x"), DelegationKind::ToolCall, Some("read")),
            Err(DelegationError::Signature)
        ));
    }
}
