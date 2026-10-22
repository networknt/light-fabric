//! Bounded CSRF values and MSAL access-token binding. Never format credentials in errors.
use crate::HandlerRejection;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};
use subtle::ConstantTimeEq;

pub(crate) const MAX_CSRF_BYTES: usize = 512;

pub(crate) fn invalid_csrf() -> HandlerRejection {
    HandlerRejection::new(401, "ERR10039", "Invalid session-bound CSRF value")
}

pub(crate) fn bounded(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CSRF_BYTES
        && value.bytes().all(|b| b.is_ascii_graphic())
}

pub(crate) fn equal(left: &str, right: &str) -> bool {
    bounded(left) && bounded(right) && bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

pub(crate) fn nonce() -> Result<String, HandlerRejection> {
    nonce_with_rng(&mut OsRng)
}

fn nonce_with_rng(rng: &mut impl RngCore) -> Result<String, HandlerRejection> {
    let mut bytes = [0; 32];
    rng.try_fill_bytes(&mut bytes)
        .map_err(|_| HandlerRejection::new(500, "ERR10001", "CSRF randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Secret values use the normal runtime substitution/decryption pipeline.
/// Both typed Debug and module diagnostics must redact the entire key map.
#[derive(Clone, Default, Serialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct CsrfKeys(pub BTreeMap<String, String>);

impl fmt::Debug for CsrfKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CsrfKeys([REDACTED])")
    }
}

impl<'de> Deserialize<'de> for CsrfKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Map(BTreeMap<String, String>),
            Text(String),
        }
        let input = Input::deserialize(deserializer)
            .map_err(|_| serde::de::Error::custom("invalid CSRF key map"))?;
        let map = match input {
            Input::Map(map) => map,
            Input::Text(text) if text.is_empty() => BTreeMap::new(),
            Input::Text(text) => serde_yaml::from_str(&text)
                .map_err(|_| serde::de::Error::custom("invalid CSRF key map"))?,
        };
        Ok(Self(map))
    }
}

pub(crate) struct CsrfSigner {
    active: String,
    keys: BTreeMap<String, Vec<u8>>,
}

impl CsrfSigner {
    pub(crate) fn new(active: &str, configured: &CsrfKeys) -> Result<Self, &'static str> {
        let valid_id = |id: &str| {
            !id.is_empty()
                && id.len() <= 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        };
        if !valid_id(active) || configured.0.is_empty() || configured.0.len() > 16 {
            return Err("msal-auth CSRF signing configuration is missing or invalid");
        }
        let mut keys = BTreeMap::new();
        for (id, encoded) in &configured.0 {
            if !valid_id(id) || encoded.len() > 171 {
                return Err("msal-auth CSRF signing configuration is invalid");
            }
            let bytes = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| "msal-auth CSRF signing configuration is invalid")?;
            if !(32..=128).contains(&bytes.len()) {
                return Err("msal-auth CSRF signing configuration is invalid");
            }
            keys.insert(id.clone(), bytes);
        }
        if !keys.contains_key(active) {
            return Err("msal-auth CSRF active key is not configured");
        }
        Ok(Self {
            active: active.to_owned(),
            keys,
        })
    }

    fn mac(&self, metadata: &str, key: &[u8], access_token: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
        mac.update(b"light-fabric/msal-auth/csrf/v1\0");
        mac.update(&(metadata.len() as u32).to_be_bytes());
        mac.update(metadata.as_bytes());
        mac.update(&Sha256::digest(access_token.as_bytes()));
        mac
    }

    pub(crate) fn sign(&self, access_token: &str) -> Result<String, HandlerRejection> {
        let metadata = format!("v1.{}.{}", self.active, nonce()?);
        let tag = self
            .mac(&metadata, &self.keys[&self.active], access_token)
            .finalize()
            .into_bytes();
        Ok(format!("{metadata}.{}", URL_SAFE_NO_PAD.encode(tag)))
    }

    pub(crate) fn verify(&self, value: &str, access_token: &str) -> Result<(), HandlerRejection> {
        if !bounded(value) || access_token.is_empty() {
            return Err(invalid_csrf());
        }
        let parts: Vec<_> = value.split('.').collect();
        if parts.len() != 4 || parts[0] != "v1" || parts[2].len() != 43 || parts[3].len() != 43 {
            return Err(invalid_csrf());
        }
        let key = self.keys.get(parts[1]).ok_or_else(invalid_csrf)?;
        if URL_SAFE_NO_PAD
            .decode(parts[2])
            .map_err(|_| invalid_csrf())?
            .len()
            != 32
        {
            return Err(invalid_csrf());
        }
        let tag = URL_SAFE_NO_PAD
            .decode(parts[3])
            .map_err(|_| invalid_csrf())?;
        let metadata = value.rsplit_once('.').ok_or_else(invalid_csrf)?.0;
        self.mac(metadata, key, access_token)
            .verify_slice(&tag)
            .map_err(|_| invalid_csrf())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(active: &str, entries: &[(&str, u8)]) -> CsrfSigner {
        CsrfSigner::new(
            active,
            &CsrfKeys(
                entries
                    .iter()
                    .map(|(id, byte)| ((*id).into(), URL_SAFE_NO_PAD.encode([*byte; 32])))
                    .collect(),
            ),
        )
        .unwrap()
    }

    #[test]
    fn signing_is_bound_to_exact_token_and_replica_key_ring() {
        let first = keys("one", &[("one", 1)]);
        let replica = keys("one", &[("one", 1)]);
        let value = first.sign("access token A").unwrap();
        assert!(replica.verify(&value, "access token A").is_ok());
        assert!(replica.verify(&value, "access token B").is_err());
        assert!(replica.verify(&value, "access token A ").is_err());
        assert_ne!(first.sign("access token A").unwrap(), value);
        for bad in [
            "attacker".to_owned(),
            format!("{value}.extra"),
            value.replace("v1.", "v2."),
            value.replace(".one.", ".unknown."),
            "x".repeat(513),
            String::new(),
        ] {
            let error = replica.verify(&bad, "access token A").unwrap_err();
            assert!(!error.message.contains("access token"));
        }
        // Every byte of both the nonce and tag is authenticated.
        let parts: Vec<_> = value.split('.').collect();
        for index in [2, 3] {
            let mut bytes = URL_SAFE_NO_PAD.decode(parts[index]).unwrap();
            bytes[0] ^= 1;
            let mut modified = parts.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
            modified[index] = URL_SAFE_NO_PAD.encode(bytes);
            assert!(
                replica
                    .verify(&modified.join("."), "access token A")
                    .is_err()
            );
        }
    }

    #[test]
    fn rotation_accepts_only_explicit_previous_keys() {
        let old = keys("old", &[("old", 1)]);
        let rotating = keys("new", &[("old", 1), ("new", 2)]);
        let retired = keys("new", &[("new", 2)]);
        let value = old.sign("token").unwrap();
        assert!(rotating.verify(&value, "token").is_ok());
        assert!(retired.verify(&value, "token").is_err());
        let new_value = rotating.sign("token").unwrap();
        assert!(new_value.starts_with("v1.new."));
        assert!(retired.verify(&new_value, "token").is_ok());
        assert!(old.verify(&new_value, "token").is_err());
    }

    #[test]
    fn key_validation_and_debug_are_secret_free() {
        for (active, encoded) in [
            ("missing", URL_SAFE_NO_PAD.encode([7; 32])),
            ("one", URL_SAFE_NO_PAD.encode([7; 31])),
            ("one", "private-invalid-secret".into()),
        ] {
            let config = CsrfKeys(BTreeMap::from([("one".into(), encoded.clone())]));
            let error = CsrfSigner::new(active, &config).err().unwrap();
            assert!(!error.contains(&encoded));
            assert!(!format!("{config:?}").contains(&encoded));
        }
        assert!(CsrfSigner::new("", &CsrfKeys::default()).is_err());
    }

    #[test]
    fn nonce_has_256_random_bits_and_rng_failure_propagates() {
        let value = nonce().unwrap();
        assert_eq!(value.len(), 43);
        assert_eq!(URL_SAFE_NO_PAD.decode(&value).unwrap().len(), 32);
        struct Failing;
        impl RngCore for Failing {
            fn next_u32(&mut self) -> u32 {
                panic!("no fallback")
            }
            fn next_u64(&mut self) -> u64 {
                panic!("no fallback")
            }
            fn fill_bytes(&mut self, _: &mut [u8]) {
                panic!("no fallback")
            }
            fn try_fill_bytes(&mut self, _: &mut [u8]) -> Result<(), rand::Error> {
                Err(rand::Error::new(std::io::Error::other("unavailable")))
            }
        }
        assert_eq!(nonce_with_rng(&mut Failing).unwrap_err().status, 500);
    }
}
