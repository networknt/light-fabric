use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use thiserror::Error;

const SIGNATURE_PREFIX: &str = "base64:";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EvidenceSignatureError {
    #[error("Ed25519 seed must contain exactly 32 bytes")]
    InvalidSeed,
    #[error("invalid Ed25519 public key for `{0}`")]
    InvalidPublicKey(String),
    #[error("unknown evidence signer key `{0}`")]
    UnknownKey(String),
    #[error("evidence signature is not canonical base64")]
    InvalidSignatureEncoding,
    #[error("evidence signature verification failed")]
    VerificationFailed,
    #[error("canonical evidence serialization failed: {0}")]
    Serialization(String),
}

pub struct Ed25519EvidenceSigner {
    key_id: String,
    key_pair: Ed25519KeyPair,
}

impl Ed25519EvidenceSigner {
    pub fn from_seed(
        key_id: impl Into<String>,
        seed: &[u8],
    ) -> Result<Self, EvidenceSignatureError> {
        let key_pair = Ed25519KeyPair::from_seed_unchecked(seed)
            .map_err(|_| EvidenceSignatureError::InvalidSeed)?;
        Ok(Self {
            key_id: key_id.into(),
            key_pair,
        })
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn public_key(&self) -> Vec<u8> {
        self.key_pair.public_key().as_ref().to_vec()
    }

    pub fn sign_bytes(&self, value: &[u8]) -> String {
        format!(
            "{SIGNATURE_PREFIX}{}",
            STANDARD.encode(self.key_pair.sign(value).as_ref())
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedEvidenceKeySet {
    version: String,
    keys: BTreeMap<String, Vec<u8>>,
    digest: String,
}

impl TrustedEvidenceKeySet {
    pub fn new(
        version: impl Into<String>,
        keys: BTreeMap<String, Vec<u8>>,
    ) -> Result<Self, EvidenceSignatureError> {
        for (key_id, key) in &keys {
            if key.len() != 32 {
                return Err(EvidenceSignatureError::InvalidPublicKey(key_id.clone()));
            }
        }
        let version = version.into();
        let digest = sha256_hex(
            &canonical_json_bytes(&serde_json::json!({
                "version": version,
                "keys": keys
                    .iter()
                    .map(|(id, key)| (id, STANDARD.encode(key)))
                    .collect::<BTreeMap<_, _>>()
            }))
            .map_err(|error| EvidenceSignatureError::Serialization(error.to_string()))?,
        );
        Ok(Self {
            version,
            keys,
            digest,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn verify(
        &self,
        key_id: &str,
        message: &[u8],
        signature: &str,
    ) -> Result<(), EvidenceSignatureError> {
        let public_key = self
            .keys
            .get(key_id)
            .ok_or_else(|| EvidenceSignatureError::UnknownKey(key_id.to_string()))?;
        let encoded = signature
            .strip_prefix(SIGNATURE_PREFIX)
            .ok_or(EvidenceSignatureError::InvalidSignatureEncoding)?;
        let signature = STANDARD
            .decode(encoded)
            .map_err(|_| EvidenceSignatureError::InvalidSignatureEncoding)?;
        UnparsedPublicKey::new(&ED25519, public_key)
            .verify(message, &signature)
            .map_err(|_| EvidenceSignatureError::VerificationFailed)
    }
}

pub fn canonical_json_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&canonicalize(&serde_json::to_value(value)?))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(object) => {
            let mut output = Map::new();
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                output.insert(key.clone(), canonicalize(&object[key]));
            }
            Value::Object(output)
        }
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_set_digest_is_order_independent() {
        let mut first = BTreeMap::new();
        first.insert("b".to_string(), vec![2; 32]);
        first.insert("a".to_string(), vec![1; 32]);
        let second = BTreeMap::from([
            ("a".to_string(), vec![1; 32]),
            ("b".to_string(), vec![2; 32]),
        ]);
        assert_eq!(
            TrustedEvidenceKeySet::new("2026-q3", first)
                .unwrap()
                .digest(),
            TrustedEvidenceKeySet::new("2026-q3", second)
                .unwrap()
                .digest()
        );
    }
}
