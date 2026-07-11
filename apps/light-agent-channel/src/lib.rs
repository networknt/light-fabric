use chrono::{DateTime, Timelike, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::{BTreeSet, HashSet};
use thiserror::Error;
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelBinding {
    pub binding_id: Uuid,
    pub host_id: Uuid,
    pub principal_id: String,
    pub agent_def_id: Uuid,
    pub adapter_id: String,
    pub external_identity: String,
    pub allowed_destinations: BTreeSet<String>,
    pub group_allowed: bool,
    pub maximum_attachment_bytes: u64,
    pub quiet_start_hour: u8,
    pub quiet_end_hour: u8,
    pub revoked_at: Option<DateTime<Utc>>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WebhookEnvelope {
    pub event_id: String,
    pub external_identity: String,
    pub destination: String,
    pub group: bool,
    pub timestamp: DateTime<Utc>,
    pub text: String,
    pub attachment_bytes: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedTurn {
    pub idempotency_key: String,
    pub binding_id: Uuid,
    pub principal_id: String,
    pub agent_def_id: Uuid,
    pub origin_ref: String,
    pub response_destination: String,
    pub text: String,
}
pub struct WebhookVerifier {
    secret: Vec<u8>,
    seen: HashSet<String>,
    maximum_skew_seconds: i64,
    maximum_payload_bytes: usize,
}
impl WebhookVerifier {
    pub fn new(secret: Vec<u8>) -> Result<Self, ChannelError> {
        if secret.len() < 32 {
            return Err(ChannelError::Signature);
        }
        Ok(Self {
            secret,
            seen: HashSet::new(),
            maximum_skew_seconds: 300,
            maximum_payload_bytes: 1024 * 1024,
        })
    }
    pub fn verify_and_normalize(
        &mut self,
        b: &ChannelBinding,
        raw: &[u8],
        signature_hex: &str,
        now: DateTime<Utc>,
    ) -> Result<NormalizedTurn, ChannelError> {
        if raw.len() > self.maximum_payload_bytes {
            return Err(ChannelError::Limit);
        }
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.secret).map_err(|_| ChannelError::Signature)?;
        mac.update(raw);
        let sig = hex_decode(signature_hex)?;
        mac.verify_slice(&sig)
            .map_err(|_| ChannelError::Signature)?;
        // Parsing happens only after signature verification and exclusively
        // from the authenticated bytes. Callers cannot substitute a separately
        // constructed envelope after presenting a valid signature.
        let e: WebhookEnvelope = serde_json::from_slice(raw).map_err(|_| ChannelError::Payload)?;
        if (now - e.timestamp).num_seconds().abs() > self.maximum_skew_seconds
            || !self.seen.insert(e.event_id.clone())
        {
            return Err(ChannelError::Replay);
        }
        if b.revoked_at.is_some()
            || b.external_identity != e.external_identity
            || !b.allowed_destinations.contains(&e.destination)
            || e.group && !b.group_allowed
        {
            return Err(ChannelError::Binding);
        }
        if e.attachment_bytes > b.maximum_attachment_bytes || e.text.len() > 64 * 1024 {
            return Err(ChannelError::Limit);
        }
        Ok(NormalizedTurn {
            idempotency_key: format!("{}:{}", b.adapter_id, e.event_id),
            binding_id: b.binding_id,
            principal_id: b.principal_id.clone(),
            agent_def_id: b.agent_def_id,
            origin_ref: e.event_id,
            response_destination: e.destination,
            text: e.text,
        })
    }
}
pub fn quiet_hours(binding: &ChannelBinding, now: DateTime<Utc>) -> bool {
    let h = now.hour() as u8;
    if binding.quiet_start_hour <= binding.quiet_end_hour {
        h >= binding.quiet_start_hour && h < binding.quiet_end_hour
    } else {
        h >= binding.quiet_start_hour || h < binding.quiet_end_hour
    }
}
fn hex_decode(v: &str) -> Result<Vec<u8>, ChannelError> {
    if v.len() != 64 {
        return Err(ChannelError::Signature);
    }
    (0..v.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&v[i..i + 2], 16).map_err(|_| ChannelError::Signature))
        .collect()
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ChannelError {
    #[error("invalid webhook signature")]
    Signature,
    #[error("replayed or stale webhook")]
    Replay,
    #[error("channel identity or destination mismatch")]
    Binding,
    #[error("channel payload limit exceeded")]
    Limit,
    #[error("signed webhook payload is malformed")]
    Payload,
}
#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> ChannelBinding {
        ChannelBinding {
            binding_id: Uuid::new_v4(),
            host_id: Uuid::nil(),
            principal_id: "p".into(),
            agent_def_id: Uuid::nil(),
            adapter_id: "webhook-v1".into(),
            external_identity: "user".into(),
            allowed_destinations: BTreeSet::from(["dm".into()]),
            group_allowed: false,
            maximum_attachment_bytes: 10,
            quiet_start_hour: 22,
            quiet_end_hour: 7,
            revoked_at: None,
        }
    }
    #[test]
    fn signature_identity_destination_and_replay_fail_closed() {
        let secret = vec![7; 32];
        let now = Utc::now();
        let event = WebhookEnvelope {
            event_id: "one".into(),
            external_identity: "user".into(),
            destination: "dm".into(),
            group: false,
            timestamp: now,
            text: "hi".into(),
            attachment_bytes: 0,
        };
        let raw = serde_json::to_vec(&event).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(&raw);
        let sig = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let mut v = WebhookVerifier::new(secret).unwrap();
        assert!(v.verify_and_normalize(&binding(), &raw, &sig, now).is_ok());
        assert_eq!(
            v.verify_and_normalize(&binding(), &raw, &sig, now),
            Err(ChannelError::Replay)
        );
    }

    #[test]
    fn signed_bytes_are_the_only_envelope_authority() {
        let secret = vec![9; 32];
        let now = Utc::now();
        let raw = serde_json::to_vec(&WebhookEnvelope {
            event_id: "signed".into(),
            external_identity: "attacker".into(),
            destination: "dm".into(),
            group: false,
            timestamp: now,
            text: "ignored substitute cannot be supplied".into(),
            attachment_bytes: 0,
        })
        .unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&secret).unwrap();
        mac.update(&raw);
        let sig = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        assert_eq!(
            WebhookVerifier::new(secret)
                .unwrap()
                .verify_and_normalize(&binding(), &raw, &sig, now),
            Err(ChannelError::Binding)
        );
    }
}
