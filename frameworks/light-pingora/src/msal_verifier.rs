//! Compatibility name for the dedicated snapshot-owned RS256 signing source.
pub(super) use crate::spa_jwks::JwksSource as MsalVerifier;

#[cfg(test)]
mod tests {
    use super::super::policy_tests::{claims, jwks_server, sign};
    use super::*;
    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn skipped_unknown_key_refresh_is_uncertainty_not_confirmed_absence() {
        let server =
            jwks_server(include_str!("../tests/fixtures/msal/signing-key.jwks.json")).await;
        let mut verifier = MsalVerifier::new(&server.uri).unwrap();
        verifier.client = server.client;
        verifier.bootstrap().await.unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("not-yet-looked-up".into());
        let key =
            EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/msal/signing-key.pem"))
                .unwrap();
        let token = jsonwebtoken::encode(&header, &claims(), &key).unwrap();
        assert_eq!(verifier.verify(&token, 60).await.unwrap_err().status, 503);
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    }

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
            assert_eq!(verifier.verify(&unknown, 60).await.unwrap_err().status, 503);
        }
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        verifier.verify(&sign(&claims()), 60).await.unwrap();
        *server.body.write().await =
            include_str!("../tests/fixtures/msal/other-signing-key.jwks.json").into();
        verifier.allow_test_refresh();
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
                503
            );
        }
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        // Enforce the body limit even without relying on a trustworthy content length.
        *server.body.write().await = " ".repeat((1024 * 1024) + 1);
        verifier.allow_test_refresh();
        assert_eq!(verifier.bootstrap().await.unwrap_err().status, 503);
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }
}
