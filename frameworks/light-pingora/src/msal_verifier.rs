//! MSAL-only signing trust: no generic key routing or principal cache.
use crate::HandlerRejection;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use light_runtime::RuntimeError;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const MAX_JWKS_BYTES: usize = 1024 * 1024;
const REFRESH_REPRIEVE: Duration = Duration::from_secs(30);
const KEY_TTL: Duration = Duration::from_secs(3600);

#[derive(Default)]
struct KeyState {
    keys: Vec<(String, DecodingKey)>,
    fetched_at: Option<Instant>,
    attempted_at: Option<Instant>,
    unavailable: bool,
}

#[cfg(test)]
mod tests {
    use super::super::policy_tests::{claims, jwks_server, sign};
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn msal_jwks_refresh_is_bounded_and_supports_same_kid_rotation() {
        let server =
            jwks_server(include_str!("../tests/fixtures/msal/signing-key.jwks.json")).await;
        let mut verifier = MsalVerifier::new(&server.uri).unwrap();
        verifier.client = server.client;
        verifier.bootstrap().await.unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("unknown".into());
        let key =
            EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/msal/signing-key.pem"))
                .unwrap();
        let unknown = jsonwebtoken::encode(&header, &claims(), &key).unwrap();
        for _ in 0..5 {
            assert_eq!(verifier.verify(&unknown, 60).await.unwrap_err().status, 401);
        }
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        verifier.verify(&sign(&claims()), 60).await.unwrap();
        *server.body.write().await =
            include_str!("../tests/fixtures/msal/other-signing-key.jwks.json").into();
        verifier.keys.lock().await.attempted_at = Some(Instant::now() - REFRESH_REPRIEVE);
        header.kid = Some("msal-test".into());
        let other = EncodingKey::from_rsa_pem(include_bytes!(
            "../tests/fixtures/msal/other-signing-key.pem"
        ))
        .unwrap();
        let rotated = jsonwebtoken::encode(&header, &claims(), &other).unwrap();
        verifier.verify(&rotated, 60).await.unwrap();
        assert!(verifier.verify(&sign(&claims()), 60).await.is_err());
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn msal_jwks_fetch_failures_are_bounded_and_fail_closed() {
        let server = jwks_server("not JWKS").await;
        let mut verifier = MsalVerifier::new(&server.uri).unwrap();
        verifier.client = server.client;
        for _ in 0..3 {
            assert_eq!(
                verifier
                    .verify(&sign(&claims()), 60)
                    .await
                    .unwrap_err()
                    .status,
                502
            );
        }
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        // Enforce the body limit even without relying on a trustworthy content length.
        *server.body.write().await = " ".repeat(MAX_JWKS_BYTES + 1);
        verifier.keys.lock().await.attempted_at = None;
        assert_eq!(verifier.bootstrap().await.unwrap_err().status, 502);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }
}

pub(super) struct MsalVerifier {
    uri: String,
    pub(super) client: reqwest::Client,
    keys: Mutex<KeyState>,
}

fn invalid() -> HandlerRejection {
    HandlerRejection::unauthorized("Invalid Microsoft access token")
}

fn unavailable() -> HandlerRejection {
    HandlerRejection::new(502, "ERR10056", "Microsoft token verification unavailable")
}

impl MsalVerifier {
    pub(super) fn new(uri: &str) -> Result<Self, RuntimeError> {
        let client = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| RuntimeError::Unsupported("cannot create MSAL JWKS client".into()))?;
        Ok(Self {
            uri: uri.to_owned(),
            client,
            keys: Mutex::new(KeyState::default()),
        })
    }

    pub(super) async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        let mut state = self.keys.lock().await;
        self.refresh(&mut state).await
    }

    async fn fetch(&self) -> Result<Vec<(String, DecodingKey)>, HandlerRejection> {
        let mut response = self
            .client
            .get(&self.uri)
            .send()
            .await
            .map_err(|_| unavailable())?
            .error_for_status()
            .map_err(|_| unavailable())?;
        if response
            .content_length()
            .is_some_and(|size| size > MAX_JWKS_BYTES as u64)
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_JWKS_BYTES.saturating_sub(bytes.len()) {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let set: JwkSet = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        let mut keys = Vec::new();
        for jwk in set.keys {
            // Ignore encryption keys and keys restricted to another algorithm.
            if jwk
                .common
                .key_algorithm
                .is_some_and(|alg| alg != jsonwebtoken::jwk::KeyAlgorithm::RS256)
                || jwk
                    .common
                    .public_key_use
                    .as_ref()
                    .is_some_and(|usage| *usage != jsonwebtoken::jwk::PublicKeyUse::Signature)
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|ops| !ops.contains(&jsonwebtoken::jwk::KeyOperations::Verify))
                || !matches!(
                    jwk.algorithm,
                    jsonwebtoken::jwk::AlgorithmParameters::RSA(_)
                )
            {
                continue;
            }
            let Some(kid) = jwk.common.key_id.as_ref().filter(|kid| !kid.is_empty()) else {
                continue;
            };
            if keys.iter().any(|(existing, _)| existing == kid) {
                return Err(unavailable());
            }
            let key = DecodingKey::from_jwk(&jwk).map_err(|_| unavailable())?;
            keys.push((kid.clone(), key));
        }
        if keys.is_empty() {
            return Err(unavailable());
        }
        Ok(keys)
    }

    async fn refresh(&self, state: &mut KeyState) -> Result<(), HandlerRejection> {
        if state
            .attempted_at
            .is_some_and(|time| time.elapsed() < REFRESH_REPRIEVE)
        {
            return if state.unavailable {
                Err(unavailable())
            } else {
                Ok(())
            };
        }
        state.attempted_at = Some(Instant::now());
        match self.fetch().await {
            Ok(keys) => {
                state.keys = keys;
                state.fetched_at = Some(Instant::now());
                state.unavailable = false;
                Ok(())
            }
            Err(error) => {
                state.unavailable = true;
                Err(error)
            }
        }
    }

    pub(super) async fn verify(
        &self,
        token: &str,
        skew: u64,
    ) -> Result<serde_json::Value, HandlerRejection> {
        let header = decode_header(token).map_err(|_| invalid())?;
        // Check before any key lookup, including when this endpoint's keys are warm.
        if header.alg != Algorithm::RS256 {
            return Err(invalid());
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or_else(invalid)?;
        let key = {
            let mut state = self.keys.lock().await;
            if state
                .fetched_at
                .is_none_or(|time| time.elapsed() >= KEY_TTL)
                || !state.keys.iter().any(|(id, _)| id == &kid)
            {
                self.refresh(&mut state).await?;
            }
            state
                .keys
                .iter()
                .find(|(id, _)| id == &kid)
                .map(|(_, key)| key.clone())
                .ok_or_else(invalid)?
        };
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = skew;
        validation.validate_aud = false; // Exact application policy follows verification.
        validation.validate_nbf = true;
        match decode::<serde_json::Value>(token, &key, &validation) {
            Ok(data) => Ok(data.claims),
            Err(error)
                if matches!(
                    error.kind(),
                    jsonwebtoken::errors::ErrorKind::InvalidSignature
                ) =>
            {
                // Also support rotation that reuses a kid, without allowing bad
                // signatures to cause an unbounded number of endpoint requests.
                let mut state = self.keys.lock().await;
                self.refresh(&mut state).await?;
                let key = state
                    .keys
                    .iter()
                    .find(|(id, _)| id == &kid)
                    .map(|(_, key)| key)
                    .ok_or_else(invalid)?;
                decode::<serde_json::Value>(token, key, &validation)
                    .map(|data| data.claims)
                    .map_err(|_| invalid())
            }
            Err(_) => Err(invalid()),
        }
    }
}
