use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce, aead::Aead};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DecryptError {
    #[error("input must start with 'CRYPT'")]
    InvalidPrefix,
    #[error("input format must be 'CRYPT:salt:hash' or 'CRYPT:salt:iv:ciphertext'")]
    InvalidFormat,
    #[error("invalid hex string: {0}")]
    HexError(#[from] hex::FromHexError),
    #[error("decryption failed: {0}")]
    CipherError(String),
}

pub const CRYPT_PREFIX: &str = "CRYPT";
const ITERATIONS: u32 = 65536;
const KEY_SIZE: usize = 32; // 256 bits
const IV: [u8; 16] = [0u8; 16];

pub trait Decryptor {
    fn decrypt(&self, input: &str) -> Result<String, DecryptError>;
}

pub struct SymmetricDecryptor {
    password: Vec<u8>,
}

impl SymmetricDecryptor {
    pub fn new(password: &str) -> Self {
        Self {
            password: password.as_bytes().to_vec(),
        }
    }

    fn derive_key(&self, salt: &[u8]) -> [u8; KEY_SIZE] {
        let mut key = [0u8; KEY_SIZE];
        pbkdf2_hmac::<Sha256>(&self.password, salt, ITERATIONS, &mut key);
        key
    }
}

impl Decryptor for SymmetricDecryptor {
    fn decrypt(&self, input: &str) -> Result<String, DecryptError> {
        if !input.starts_with(CRYPT_PREFIX) {
            return Err(DecryptError::InvalidPrefix);
        }

        let parts: Vec<&str> = input.split(':').collect();
        if parts.first() != Some(&CRYPT_PREFIX) || !matches!(parts.len(), 3 | 4) {
            return Err(DecryptError::InvalidFormat);
        }

        let salt = hex::decode(parts[1])?;
        let key = self.derive_key(&salt);
        // Native Java's current format carries a 96-bit GCM IV and appends
        // the 128-bit authentication tag to the ciphertext. The three-part
        // legacy CBC format below remains byte-for-byte compatible.
        if parts.len() == 4 {
            let iv = hex::decode(parts[2])?;
            if iv.len() != 12 {
                return Err(DecryptError::InvalidFormat);
            }
            let ciphertext = hex::decode(parts[3])?;
            let plaintext = Aes256Gcm::new_from_slice(&key)
                .map_err(|_| DecryptError::InvalidFormat)?
                .decrypt(Nonce::from_slice(&iv), ciphertext.as_ref())
                .map_err(|_| DecryptError::CipherError("authentication failed".into()))?;
            return String::from_utf8(plaintext)
                .map_err(|_| DecryptError::CipherError("invalid UTF-8".into()));
        }
        let hash = hex::decode(parts[2])?;

        type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;
        let decryptor = Aes256CbcDec::new(&key.into(), &IV.into());

        let mut buf = hash.to_vec();
        let decrypted_bytes = decryptor
            .decrypt_padded_mut::<Pkcs7>(&mut buf)
            .map_err(|e| DecryptError::CipherError(format!("{:?}", e)))?;

        String::from_utf8(decrypted_bytes.to_vec())
            .map_err(|e| DecryptError::CipherError(format!("Invalid UTF-8: {:?}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockEncryptMut, block_padding::Pkcs7};

    const JAVA_GCM: &str = "CRYPT:01010101010101010101010101010101:020202020202020202020202:384c3e9b2a30679cf57d653304f96af7599f065069b95682eb8275ee23e927a00d53a009e193590afa";

    #[test]
    fn native_java_gcm_vector_authenticates_and_preserves_legacy_cbc() {
        let decryptor = SymmetricDecryptor::new("test-only-master");
        assert_eq!(
            decryptor.decrypt(JAVA_GCM).unwrap(),
            "test-only-decrypted-value"
        );
        let mut tampered = JAVA_GCM.to_owned();
        tampered.pop();
        tampered.push('b');
        for value in [
            tampered,
            JAVA_GCM.replace(":020202020202020202020202:", ":030202020202020202020202:"),
            JAVA_GCM.replace(":020202020202020202020202:", ":02:"),
        ] {
            assert!(decryptor.decrypt(&value).is_err());
        }
        assert!(
            SymmetricDecryptor::new("wrong-test-password")
                .decrypt(JAVA_GCM)
                .is_err()
        );
    }

    #[test]
    fn test_decrypt_consistency() {
        let password = "light";
        let decryptor = SymmetricDecryptor::new(password);
        let plaintext = "secret";
        let salt = hex::decode("ebfab3ef4261185776a026acf72d24ee").expect("salt");
        let key = decryptor.derive_key(&salt);

        type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
        let encryptor = Aes256CbcEnc::new(&key.into(), &IV.into());
        let mut buf = plaintext.as_bytes().to_vec();
        let msg_len = buf.len();
        buf.resize(msg_len + 16, 0);
        let ciphertext = encryptor
            .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
            .expect("encrypt")
            .to_vec();

        let encrypted = format!("CRYPT:{}:{}", hex::encode(salt), hex::encode(ciphertext));
        let decrypted = decryptor.decrypt(&encrypted).expect("decrypt");

        assert_eq!(decrypted, plaintext);
    }
}
