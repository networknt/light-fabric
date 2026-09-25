//! At-rest source bearer envelope for app-owned LONG credential tables.
//! Each app keeps this keyring in its own restricted secret mount.
use crate::long_binding::LongClientError;
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::Engine as _;
use serde::Deserialize;
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
use zeroize::Zeroizing;

pub struct SourceKeyring {
    active: String,
    keys: BTreeMap<String, Zeroizing<[u8; 32]>>,
}

pub struct SealedLongSource {
    pub key_id: String,
    pub ciphertext: Vec<u8>,
}

impl SourceKeyring {
    pub async fn load(path: &Path) -> Result<Self, LongClientError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct File {
            active_key_id: String,
            keys: BTreeMap<String, String>,
        }
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_| LongClientError::Store)?;
        let file: File = serde_json::from_slice(&bytes).map_err(|_| LongClientError::Evidence)?;
        let keys = file
            .keys
            .into_iter()
            .map(|(id, value)| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(|_| LongClientError::Evidence)?;
                let key: [u8; 32] = bytes.try_into().map_err(|_| LongClientError::Evidence)?;
                Ok((id, Zeroizing::new(key)))
            })
            .collect::<Result<BTreeMap<_, _>, LongClientError>>()?;
        if !keys.contains_key(&file.active_key_id) {
            return Err(LongClientError::Evidence);
        }
        Ok(Self {
            active: file.active_key_id,
            keys,
        })
    }

    fn aad(holder_client_id: &str, binding: Uuid, key_id: &str) -> Vec<u8> {
        format!("long-owner:v1:{holder_client_id}:{binding}:{key_id}").into_bytes()
    }

    pub fn seal(
        &self,
        holder_client_id: &str,
        binding: Uuid,
        token: &str,
    ) -> Result<SealedLongSource, LongClientError> {
        if holder_client_id.is_empty()
            || binding.is_nil()
            || token.is_empty()
            || token.len() > 16_384
        {
            return Err(LongClientError::Evidence);
        }
        let cipher = Aes256Gcm::new_from_slice(&self.keys[&self.active][..])
            .map_err(|_| LongClientError::Evidence)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let body = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: token.as_bytes(),
                    aad: &Self::aad(holder_client_id, binding, &self.active),
                },
            )
            .map_err(|_| LongClientError::Evidence)?;
        Ok(SealedLongSource {
            key_id: self.active.clone(),
            ciphertext: [nonce.as_slice(), body.as_slice()].concat(),
        })
    }

    pub fn open(
        &self,
        holder_client_id: &str,
        binding: Uuid,
        key_id: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<String>, LongClientError> {
        if ciphertext.len() < 28 || ciphertext.len() > 16_412 {
            return Err(LongClientError::Evidence);
        }
        let cipher =
            Aes256Gcm::new_from_slice(&self.keys.get(key_id).ok_or(LongClientError::Evidence)?[..])
                .map_err(|_| LongClientError::Evidence)?;
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&ciphertext[..12]),
                Payload {
                    msg: &ciphertext[12..],
                    aad: &Self::aad(holder_client_id, binding, key_id),
                },
            )
            .map_err(|_| LongClientError::Evidence)?;
        let text = String::from_utf8(plain).map_err(|_| LongClientError::Evidence)?;
        Ok(Zeroizing::new(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_is_bound_to_holder_and_binding() {
        let path = std::env::temp_dir().join(format!("long-keyring-{}.json", Uuid::new_v4()));
        let content = serde_json::json!({"activeKeyId":"one","keys":{
            "one":base64::engine::general_purpose::STANDARD.encode([7u8; 32])}});
        tokio::fs::write(&path, serde_json::to_vec(&content).unwrap())
            .await
            .unwrap();
        let keys = SourceKeyring::load(&path).await.unwrap();
        tokio::fs::remove_file(&path).await.unwrap();
        let binding = Uuid::new_v4();
        let sealed = keys.seal("agent-client", binding, "owner-source").unwrap();
        assert!(!sealed.ciphertext.windows(12).any(|v| v == b"owner-source"));
        assert_eq!(
            *keys
                .open("agent-client", binding, &sealed.key_id, &sealed.ciphertext)
                .unwrap(),
            "owner-source"
        );
        assert!(
            keys.open("other-client", binding, &sealed.key_id, &sealed.ciphertext)
                .is_err()
        );
        assert!(
            keys.open(
                "agent-client",
                Uuid::new_v4(),
                &sealed.key_id,
                &sealed.ciphertext
            )
            .is_err()
        );
    }
}
