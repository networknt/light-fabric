use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use rsa::{Oaep, RsaPrivateKey, pkcs1::DecodeRsaPrivateKey, pkcs8::DecodePrivateKey};
use sha2::Sha256;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AsymmetricError {
    #[error("input must start with 'CRYPT:RSA:'")]
    InvalidPrefix,
    #[error("invalid base64: {0}")]
    Base64Error(#[from] base64::DecodeError),
    #[error("key error: {0}")]
    KeyError(String),
    #[error("decryption failed: {0}")]
    DecryptError(#[from] rsa::Error),
}

pub const CRYPT_RSA_PREFIX: &str = "CRYPT:RSA:";

pub struct AsymmetricDecryptor {
    private_key: RsaPrivateKey,
}

impl AsymmetricDecryptor {
    pub fn from_pem(pem: &str) -> Result<Self, AsymmetricError> {
        let private_key = RsaPrivateKey::from_pkcs8_pem(pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem)) // Fallback to PKCS#1 if PKCS#8 fails
            .map_err(|e| {
                AsymmetricError::KeyError(format!("Failed to load private key: {:?}", e))
            })?;
        Ok(Self { private_key })
    }

    pub fn decrypt(&self, input: &str) -> Result<String, AsymmetricError> {
        if !input.starts_with(CRYPT_RSA_PREFIX) {
            return Err(AsymmetricError::InvalidPrefix);
        }

        let ciphertext_b64 = &input[CRYPT_RSA_PREFIX.len()..];
        let ciphertext = BASE64.decode(ciphertext_b64)?;

        // Using OAEP with SHA-256 as our cross-language standard
        let padding = Oaep::new::<Sha256>();
        let decrypted_bytes = self.private_key.decrypt(padding, &ciphertext)?;

        String::from_utf8(decrypted_bytes)
            .map_err(|e| AsymmetricError::KeyError(format!("Invalid UTF-8: {:?}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::thread_rng;
    use rsa::{RsaPublicKey, pkcs8::EncodePrivateKey};

    #[test]
    fn decrypts_rsa_oaep_payload_from_generated_keypair() {
        let mut rng = thread_rng();
        let private_key = RsaPrivateKey::new(&mut rng, 2048).expect("private key");
        let public_key = RsaPublicKey::from(&private_key);
        let pem = private_key
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("pkcs8 pem");
        let decryptor = AsymmetricDecryptor::from_pem(pem.as_str()).expect("decryptor");

        let plaintext = b"secret";
        let ciphertext = public_key
            .encrypt(&mut rng, Oaep::new::<Sha256>(), plaintext)
            .expect("encrypt");
        let payload = format!("CRYPT:RSA:{}", BASE64.encode(ciphertext));

        let decrypted = decryptor.decrypt(&payload).expect("decrypt");
        assert_eq!(decrypted, "secret");
    }

    #[test]
    fn test_asymmetric_parsing_fail() {
        let pem = "-----BEGIN PRIVATE KEY-----\n...\n-----END PRIVATE KEY-----";
        assert!(AsymmetricDecryptor::from_pem(pem).is_err());
    }
}
