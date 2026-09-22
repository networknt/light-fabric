//! Renewal proof of possession.
//!
//! Renewal is authenticated by the **old key and still-valid certificate only**, and it replaces
//! both the key pair and the certificate. No token and no user is involved. An expired workload
//! identity must enroll again.
//!
//! A certificate is public, so presenting one proves nothing by itself. The
//! caller instead signs a fixed message with the certificate's *private* key:
//!
//! ```text
//! light-identity-issuer/renew/v1
//! <env tag>
//! <lowercase hex SHA-256 of the new CSR (DER)>
//! <unix timestamp, seconds>
//! <nonce>
//! ```
//!
//! (fields separated by a single `\n`, no trailing newline). The issuer checks
//! that the presented certificate chains to its CA, verifies the signature with
//! the certificate's public key, and requires a fresh timestamp and a nonce it
//! has not seen. The proof is an application-layer signature rather than mTLS
//! so the issuer can validate proof of possession without coupling renewal to an mTLS listener.
//!
//! Supported proof-key algorithms: ECDSA P-256 (SHA-256), ECDSA P-384
//! (SHA-384), and Ed25519. Signatures are ASN.1 DER for ECDSA, as `rcgen` and
//! `ring` produce.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use ring::{digest, signature};
use x509_parser::oid_registry::{
    OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY, OID_NIST_EC_P384, OID_SIG_ED25519,
};
use x509_parser::x509::SubjectPublicKeyInfo;

use crate::IssuerError;

const PROOF_CONTEXT: &str = "light-identity-issuer/renew/v1";

/// How far a proof's timestamp may differ from the issuer's clock, in either
/// direction. Also the window a used nonce must be remembered for.
pub const MAX_PROOF_SKEW_SECONDS: i64 = 300;

const MIN_NONCE_LEN: usize = 16;
const MAX_NONCE_LEN: usize = 128;

/// The proof of possession that accompanies a renewal request.
#[derive(Clone, Debug)]
pub struct RenewalProof {
    /// Unix seconds at which the caller built the proof.
    pub timestamp: i64,
    /// A caller-chosen random value, unique per renewal attempt.
    pub nonce: String,
    /// Signature over [`renewal_message`] made with the *old* private key.
    pub signature: Vec<u8>,
}

/// The exact bytes a caller must sign with its old private key. Public so a
/// client builds the message with the same function the issuer verifies.
pub fn renewal_message(env_tag: &str, csr_der: &[u8], timestamp: i64, nonce: &str) -> Vec<u8> {
    let csr_digest = digest::digest(&digest::SHA256, csr_der);
    let mut hex = String::with_capacity(64);
    for byte in csr_digest.as_ref() {
        let _ = write!(hex, "{byte:02x}");
    }
    format!("{PROOF_CONTEXT}\n{env_tag}\n{hex}\n{timestamp}\n{nonce}").into_bytes()
}

fn nonce_is_well_formed(nonce: &str) -> bool {
    (MIN_NONCE_LEN..=MAX_NONCE_LEN).contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Verify `signature` over `message` with the certificate's public key.
fn verify_possession(
    key: &SubjectPublicKeyInfo,
    message: &[u8],
    signature_bytes: &[u8],
) -> Result<(), IssuerError> {
    let algorithm: &'static dyn signature::VerificationAlgorithm =
        if key.algorithm.algorithm == OID_SIG_ED25519 {
            &signature::ED25519
        } else if key.algorithm.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY {
            let curve = key
                .algorithm
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.as_oid().ok());
            match curve {
                Some(curve) if curve == OID_EC_P256 => &signature::ECDSA_P256_SHA256_ASN1,
                Some(curve) if curve == OID_NIST_EC_P384 => &signature::ECDSA_P384_SHA384_ASN1,
                _ => return Err(IssuerError::UnsupportedKeyAlgorithm),
            }
        } else {
            return Err(IssuerError::UnsupportedKeyAlgorithm);
        };

    signature::UnparsedPublicKey::new(algorithm, &key.subject_public_key.data)
        .verify(message, signature_bytes)
        .map_err(|_| IssuerError::PossessionProofInvalid)
}

/// Check freshness and the possession signature for a renewal. Does **not**
/// record the nonce; the caller does that after every other check passes, so a
/// request that fails for another reason cannot burn a nonce.
pub(crate) fn verify_renewal_proof(
    presented_certificate_der: &[u8],
    env_tag: &str,
    csr_der: &[u8],
    proof: &RenewalProof,
    now_unix: i64,
) -> Result<(), IssuerError> {
    if (now_unix - proof.timestamp).abs() > MAX_PROOF_SKEW_SECONDS {
        return Err(IssuerError::PossessionProofStale);
    }
    if !nonce_is_well_formed(&proof.nonce) {
        return Err(IssuerError::PossessionProofInvalid);
    }
    let (_, certificate) = x509_parser::parse_x509_certificate(presented_certificate_der)
        .map_err(|err| IssuerError::InvalidPresentedCertificate(err.to_string()))?;
    let message = renewal_message(env_tag, csr_der, proof.timestamp, &proof.nonce);
    verify_possession(certificate.public_key(), &message, &proof.signature)
}

/// Remembers nonces from verified renewal requests so a captured request
/// cannot be replayed inside its timestamp window.
///
/// In memory: an issuer restart forgets them, so a request captured inside the
/// last five minutes could be replayed once after a restart. The signed CSR
/// digest means a replay only yields a certificate for the *original* new key,
/// which the replaying party does not hold.
#[derive(Default)]
pub(crate) struct RenewalReplayGuard {
    seen: Mutex<HashMap<String, i64>>,
}

impl RenewalReplayGuard {
    /// Returns `true` and records the nonce the first time it is seen inside
    /// its window; `false` if it was already used.
    pub(crate) fn record(&self, nonce: &str, now_unix: i64) -> bool {
        let mut seen = self.seen.lock().expect("renewal replay guard lock");
        // A proof's timestamp is within `MAX_PROOF_SKEW_SECONDS` of now, so it
        // stays acceptable for at most twice that; keep the nonce that long.
        seen.retain(|_, expires| *expires >= now_unix);
        if seen.contains_key(nonce) {
            return false;
        }
        seen.insert(nonce.to_string(), now_unix + 2 * MAX_PROOF_SKEW_SECONDS);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_binds_env_csr_timestamp_and_nonce() {
        let base = renewal_message("dev", b"csr-a", 1_700_000_000, "nonce-0123456789ab");
        assert_ne!(
            base,
            renewal_message("prod", b"csr-a", 1_700_000_000, "nonce-0123456789ab")
        );
        assert_ne!(
            base,
            renewal_message("dev", b"csr-b", 1_700_000_000, "nonce-0123456789ab")
        );
        assert_ne!(
            base,
            renewal_message("dev", b"csr-a", 1_700_000_001, "nonce-0123456789ab")
        );
        assert_ne!(
            base,
            renewal_message("dev", b"csr-a", 1_700_000_000, "nonce-0123456789ac")
        );
    }

    #[test]
    fn nonce_shape_is_enforced() {
        assert!(nonce_is_well_formed("abcdefghijklmnop"));
        assert!(!nonce_is_well_formed("short"));
        assert!(!nonce_is_well_formed(&"a".repeat(MAX_NONCE_LEN + 1)));
        assert!(!nonce_is_well_formed("has a newline\nxxxxxxxx"));
    }

    #[test]
    fn replay_guard_accepts_a_nonce_once_and_forgets_it_after_the_window() {
        let guard = RenewalReplayGuard::default();
        assert!(guard.record("nonce-a", 1_000));
        assert!(!guard.record("nonce-a", 1_001));
        assert!(guard.record("nonce-b", 1_001));
        assert!(guard.record("nonce-a", 1_000 + 2 * MAX_PROOF_SKEW_SECONDS + 1));
    }
}
