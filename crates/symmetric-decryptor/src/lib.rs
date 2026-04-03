use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum DecryptError {
    #[error("input must start with 'CRYPT'")]
    InvalidPrefix,
    #[error("input format must be 'CRYPT:salt:hash'")]
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
        if parts.len() != 3 {
            return Err(DecryptError::InvalidFormat);
        }

        let salt = hex::decode(parts[1])?;
        let hash = hex::decode(parts[2])?;

        let key = self.derive_key(&salt);

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
