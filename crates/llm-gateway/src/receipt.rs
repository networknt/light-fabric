use agent_runtime_protocol::{
    GatewayUsageReceiptClaims, SignedGatewayUsageReceipt, canonical_json_bytes,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

pub const MIN_RECEIPT_SECRET_BYTES: usize = 32;

pub struct UsageReceiptSigner {
    key_id: String,
    secret: Vec<u8>,
}

impl UsageReceiptSigner {
    pub fn new(key_id: impl Into<String>, secret: impl AsRef<[u8]>) -> Result<Self, ReceiptError> {
        let key_id = key_id.into();
        if key_id.is_empty()
            || key_id.len() > 128
            || key_id.chars().any(char::is_whitespace)
            || secret.as_ref().len() < MIN_RECEIPT_SECRET_BYTES
        {
            return Err(ReceiptError::InvalidKey);
        }
        Ok(Self {
            key_id,
            secret: secret.as_ref().to_vec(),
        })
    }

    pub fn sign(
        &self,
        claims: GatewayUsageReceiptClaims,
        now: DateTime<Utc>,
    ) -> Result<SignedGatewayUsageReceipt, ReceiptError> {
        claims.validate(now)?;
        let signature = self.signature(&claims)?;
        Ok(SignedGatewayUsageReceipt {
            claims,
            key_id: self.key_id.clone(),
            signature,
        })
    }

    pub fn verify(
        &self,
        receipt: &SignedGatewayUsageReceipt,
        expected_binding_digest: &str,
        now: DateTime<Utc>,
    ) -> Result<(), ReceiptError> {
        receipt.claims.validate(now)?;
        if receipt.key_id != self.key_id
            || receipt.claims.binding.digest()? != expected_binding_digest
        {
            return Err(ReceiptError::Binding);
        }
        let supplied = URL_SAFE_NO_PAD
            .decode(&receipt.signature)
            .map_err(|_| ReceiptError::Signature)?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).map_err(|_| ReceiptError::InvalidKey)?;
        mac.update(&canonical_json_bytes(&receipt.claims)?);
        mac.verify_slice(&supplied)
            .map_err(|_| ReceiptError::Signature)
    }

    fn signature(&self, claims: &GatewayUsageReceiptClaims) -> Result<String, ReceiptError> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).map_err(|_| ReceiptError::InvalidKey)?;
        mac.update(&canonical_json_bytes(claims)?);
        Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
    }
}

#[derive(Debug, Error)]
pub enum ReceiptError {
    #[error("usage receipt signing key is invalid")]
    InvalidKey,
    #[error("usage receipt binding does not match the admitted attempt")]
    Binding,
    #[error("usage receipt signature is invalid")]
    Signature,
    #[error(transparent)]
    Protocol(#[from] agent_runtime_protocol::ProtocolError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId};
    use agent_runtime_protocol::{GatewayAttemptBinding, GatewayUsageReceiptClaims};
    use uuid::Uuid;

    fn claims(now: DateTime<Utc>) -> GatewayUsageReceiptClaims {
        GatewayUsageReceiptClaims {
            schema_version: 1,
            receipt_id: Uuid::new_v4(),
            binding: GatewayAttemptBinding {
                audience: "llm-gateway".into(),
                host_id: Uuid::new_v4(),
                end_user_subject: "user-7".into(),
                principal_subject: "user-7".into(),
                workload_actor: "light-agent/worker-3".into(),
                workflow_id: Some(Uuid::new_v4()),
                session_id: AgentSessionId::new(),
                turn_id: AgentTurnId::new(),
                action_attempt_id: AgentActionAttemptId::new(),
                policy_digest: format!("sha256:{}", "1".repeat(64)),
                data_boundary_digest: format!("sha256:{}", "2".repeat(64)),
                route_alias: "coding-reviewer".into(),
                billing_subject: "cost-center-19".into(),
                budget_policy_id: "engineering-standard".into(),
                correlation_id: Uuid::new_v4(),
            },
            logical_request_id: Uuid::new_v4(),
            provider_attempt_id: Uuid::new_v4(),
            input_tokens: 120,
            cached_input_tokens: 20,
            output_tokens: 80,
            reasoning_output_tokens: 30,
            charged_cost_micros: 912,
            usage_complete: true,
            issued_at: now,
            expires_at: now + chrono::Duration::minutes(2),
        }
    }

    #[test]
    fn signed_receipt_binds_user_actor_workflow_turn_route_attempt_and_cost() {
        let now = Utc::now();
        let signer = UsageReceiptSigner::new("usage-2026-09", [7_u8; 32]).unwrap();
        let receipt = signer.sign(claims(now), now).unwrap();
        let binding = receipt.claims.binding.digest().unwrap();
        signer.verify(&receipt, &binding, now).unwrap();

        let mut altered = receipt.clone();
        altered.claims.binding.billing_subject = "different-user".into();
        assert!(matches!(
            signer.verify(&altered, &binding, now),
            Err(ReceiptError::Binding)
        ));
        assert!(
            signer
                .verify(&receipt, &format!("sha256:{}", "0".repeat(64)), now)
                .is_err()
        );
    }
}
