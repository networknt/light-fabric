//! Over the real HTTP surface: a CLI-shaped caller completes first issuance and
//! renewal exactly as a deployed process would serve them, and the negative
//! cases that matter for security are refused.
//!
//! The pairing-grant path is used for first issuance because it needs no JWKS.
//! The Portal-token path, with its claim binding, is covered in the crate's own
//! unit tests.
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use jsonwebtoken::{DecodingKey, EncodingKey, Header, encode};
use light_identity_issuer::{
    CaMaterial, CombinedAuthorizer, EnvPolicy, FileSpentTokens, InMemoryPairingCodes,
    InMemoryRevocationList, IssuerError, IssuerPolicy, OnDiskCaSigner, PairingGrantAuthorizer,
    PortalTokenAuthorizer, SpentTokenStore, WorkloadIssuer, renewal_message,
};
use light_identity_issuer_service::{http, jwks::JwksCache};
use rcgen::{CertificateParams, KeyPair, SanType, SigningKey};
use serde_json::{Value, json};
use tower::util::ServiceExt;
use uuid::Uuid;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

fn csr_for(key: &KeyPair) -> Vec<u8> {
    CertificateParams::new(Vec::<String>::new())
        .expect("leaf params")
        .serialize_request(key)
        .expect("csr")
        .der()
        .to_vec()
}

fn test_state(pairing_stub_enabled: bool) -> Arc<http::AppState> {
    let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
    let pairing_codes = Arc::new(InMemoryPairingCodes::new());
    let authorizer = CombinedAuthorizer::new(
        PortalTokenAuthorizer::new("light-oauth", "light-fabric", JwksCache::empty_for_tests()),
        PairingGrantAuthorizer::new(Arc::clone(&pairing_codes)),
    );
    let issuer = WorkloadIssuer::new(
        OnDiskCaSigner::new(material),
        IssuerPolicy::new()
            .with_env(
                "loc",
                EnvPolicy::new(Duration::from_secs(3_600), Duration::from_secs(600)),
            )
            // Encoded certificate timestamps have second precision, so this is expired by the
            // time the renewal request is handled while still passing non-zero validation.
            .with_env(
                "instant",
                EnvPolicy::new(Duration::from_nanos(1), Duration::ZERO),
            ),
        InMemoryRevocationList::new(),
        authorizer,
    );
    Arc::new(http::AppState {
        issuer,
        pairing_codes,
        pairing_stub_enabled,
    })
}

async fn post(state: &Arc<http::AppState>, path: &str, body: Value) -> (StatusCode, Value) {
    let response = http::router(Arc::clone(state))
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("request"),
        )
        .await
        .expect("router serves request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        // Framework-level rejections (for example an unknown field) are plain
        // text, not JSON.
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
    };
    (status, value)
}

/// Mint a pairing code and redeem it, returning the issued certificate's JSON
/// and the key it was issued for.
async fn enroll(state: &Arc<http::AppState>, env_tag: &str) -> (Value, KeyPair) {
    let code = Uuid::new_v4().to_string();
    let (status, _) = post(
        state,
        "/v1/pairing-codes",
        json!({
            "code": code,
            "installId": Uuid::new_v4(),
            "serviceId": "com.networknt.light-cli-1.0.0",
            "role": "cli",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let key = KeyPair::generate().expect("key");
    let (status, body) = post(
        state,
        "/v1/csr",
        json!({
            "envTag": env_tag,
            "credential": {"kind": "pairingGrant", "code": code},
            "csrDer": B64.encode(csr_for(&key)),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "first issuance body: {body}");
    (body, key)
}

/// Build a renewal request: a new key and CSR, and a proof signed with `signer`.
fn renewal_request(
    env_tag: &str,
    presented_certificate_der: &str,
    signer: &KeyPair,
    nonce: &str,
    timestamp: i64,
) -> Value {
    let new_key = KeyPair::generate().expect("new key");
    let new_csr = csr_for(&new_key);
    let signature = signer
        .sign(&renewal_message(env_tag, &new_csr, timestamp, nonce))
        .expect("sign proof");
    json!({
        "envTag": env_tag,
        "presentedCertificateDer": presented_certificate_der,
        "csrDer": B64.encode(&new_csr),
        "proof": {
            "timestamp": timestamp,
            "nonce": nonce,
            "signature": B64.encode(signature),
        },
    })
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

fn fresh_nonce() -> String {
    Uuid::new_v4().simple().to_string()
}

#[tokio::test]
async fn first_issuance_then_renewal_over_http() {
    let state = test_state(true);
    let (issued, old_key) = enroll(&state, "loc").await;

    // The response carries the full chain and the identity the issuer decided.
    assert_eq!(issued["serviceId"], "com.networknt.light-cli-1.0.0");
    assert_eq!(issued["role"], "cli");
    assert!(issued["installId"].as_str().is_some());
    let chain = issued["chainPem"].as_str().expect("chainPem");
    assert_eq!(
        chain.matches("BEGIN CERTIFICATE").count(),
        2,
        "leaf then CA"
    );
    assert!(chain.starts_with(issued["certificatePem"].as_str().unwrap().trim_end()));
    assert!(
        chain
            .trim_end()
            .ends_with(issued["caCertificatePem"].as_str().unwrap().trim_end())
    );
    let not_after = time::OffsetDateTime::parse(
        issued["notAfter"].as_str().expect("notAfter"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("notAfter is RFC3339");
    let renew_at = time::OffsetDateTime::parse(
        issued["renewAt"].as_str().expect("renewAt"),
        &time::format_description::well_known::Rfc3339,
    )
    .expect("renewAt is RFC3339");
    assert_eq!(
        not_after - renew_at,
        time::Duration::minutes(10),
        "the configured renewal lead is observable by the workload"
    );

    let (status, renewed) = post(
        &state,
        "/v1/renew",
        renewal_request(
            "loc",
            issued["certificateDer"].as_str().unwrap(),
            &old_key,
            &fresh_nonce(),
            now(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "renewal body: {renewed}");
    assert_eq!(renewed["installId"], issued["installId"]);
    assert_ne!(renewed["certificateDer"], issued["certificateDer"]);
}

#[tokio::test]
async fn a_pairing_code_cannot_be_redeemed_twice() {
    let state = test_state(true);
    let code = "one-shot";
    let (status, _) = post(
        &state,
        "/v1/pairing-codes",
        json!({
            "code": code,
            "installId": Uuid::new_v4(),
            "serviceId": "com.networknt.light-cli-1.0.0",
            "role": "cli",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let request = |key: &KeyPair| {
        json!({
            "envTag": "loc",
            "credential": {"kind": "pairingGrant", "code": code},
            "csrDer": B64.encode(csr_for(key)),
        })
    };
    let (status, _) = post(&state, "/v1/csr", request(&KeyPair::generate().unwrap())).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post(&state, "/v1/csr", request(&KeyPair::generate().unwrap())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_pairing_stub_is_not_served_unless_enabled() {
    let state = test_state(false);
    let (status, _) = post(
        &state,
        "/v1/pairing-codes",
        json!({
            "code": "x",
            "installId": Uuid::new_v4(),
            "serviceId": "com.networknt.light-cli-1.0.0",
            "role": "cli",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn first_issuance_refuses_a_caller_supplied_identity() {
    let state = test_state(true);
    let key = KeyPair::generate().unwrap();
    let (status, _) = post(
        &state,
        "/v1/csr",
        json!({
            "envTag": "loc",
            "credential": {"kind": "pairingGrant", "code": "whatever"},
            "identity": {
                "installId": Uuid::new_v4(),
                "serviceId": "com.networknt.light-gateway-1.0.0",
                "role": "gateway",
            },
            "csrDer": B64.encode(csr_for(&key)),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn renewal_refuses_a_certificate_that_has_expired() {
    let state = test_state(true);
    let (expired, old_key) = enroll(&state, "instant").await;

    let (status, renewed) = post(
        &state,
        "/v1/renew",
        renewal_request(
            "instant",
            expired["certificateDer"].as_str().unwrap(),
            &old_key,
            &fresh_nonce(),
            now(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "renewal body: {renewed}");
    assert!(
        renewed["error"]
            .as_str()
            .is_some_and(|message| message.contains("expired")),
        "{renewed}"
    );
}

#[tokio::test]
async fn renewal_refuses_an_environment_the_certificate_was_not_issued_for() {
    let state = test_state(true);
    for (issued_in, asked_for) in [("loc", "instant"), ("instant", "loc")] {
        let (issued, old_key) = enroll(&state, issued_in).await;
        let (status, body) = post(
            &state,
            "/v1/renew",
            renewal_request(
                asked_for,
                issued["certificateDer"].as_str().unwrap(),
                &old_key,
                &fresh_nonce(),
                now(),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{issued_in} -> {asked_for}: {body}"
        );
    }
}

#[tokio::test]
async fn renewal_refuses_a_proof_made_with_someone_elses_key() {
    let state = test_state(true);
    let (issued, _old_key) = enroll(&state, "loc").await;
    let impostor = KeyPair::generate().unwrap();

    let (status, _) = post(
        &state,
        "/v1/renew",
        renewal_request(
            "loc",
            issued["certificateDer"].as_str().unwrap(),
            &impostor,
            &fresh_nonce(),
            now(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn renewal_refuses_a_replayed_request() {
    let state = test_state(true);
    let (issued, old_key) = enroll(&state, "loc").await;

    let request = renewal_request(
        "loc",
        issued["certificateDer"].as_str().unwrap(),
        &old_key,
        &fresh_nonce(),
        now(),
    );
    let (status, _) = post(&state, "/v1/renew", request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = post(&state, "/v1/renew", request).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn renewal_refuses_a_certificate_the_issuer_never_signed() {
    let state = test_state(true);
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
    params.subject_alt_names = vec![SanType::URI(
        format!(
            "spiffe://lightapi.local/cli/com.networknt.light-cli-1.0.0/{}",
            Uuid::new_v4()
        )
        .try_into()
        .unwrap(),
    )];
    let forged = params.self_signed(&key).unwrap();

    let (status, _) = post(
        &state,
        "/v1/renew",
        renewal_request(
            "loc",
            &B64.encode(forged.der()),
            &key,
            &fresh_nonce(),
            now(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn renewal_requires_a_proof() {
    let state = test_state(true);
    let (issued, _key) = enroll(&state, "loc").await;
    let (status, _) = post(
        &state,
        "/v1/renew",
        json!({
            "envTag": "loc",
            "presentedCertificateDer": issued["certificateDer"],
            "csrDer": B64.encode(csr_for(&KeyPair::generate().unwrap())),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ---- durable spent tokens, over the real router --------------------------------

fn app_token(jti: &str) -> String {
    encode(
        &Header {
            kid: Some("test-kid".into()),
            ..Header::default()
        },
        &json!({
            "iss": "urn:com:networknt:oauth2:v1", "aud": "urn:com.networknt",
            "sub": "client-1", "sid": "com.networknt.light-cli-1.0.0", "env": "dev",
            "iat": 1_700_000_000, "exp": 4_102_444_800i64, "token_use": "app", "jti": jti,
        }),
        &EncodingKey::from_secret(b"durable-secret"),
    )
    .unwrap()
}

/// An issuer that accepts Portal tokens, recording spent ones in `store`.
fn token_state(store: Arc<dyn SpentTokenStore>) -> Arc<http::AppState> {
    let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
    let jwks = JwksCache::empty_for_tests();
    jwks.insert_for_tests("test-kid", DecodingKey::from_secret(b"durable-secret"));
    let pairing_codes = Arc::new(InMemoryPairingCodes::new());
    let authorizer = CombinedAuthorizer::new(
        PortalTokenAuthorizer::new("urn:com:networknt:oauth2:v1", "urn:com.networknt", jwks)
            .with_role_binding("com.networknt.light-cli-", "cli")
            .with_spent_store(store),
        PairingGrantAuthorizer::new(Arc::clone(&pairing_codes)),
    );
    let issuer = WorkloadIssuer::new(
        OnDiskCaSigner::new(material),
        IssuerPolicy::new().with_env(
            "dev",
            EnvPolicy::new(Duration::from_secs(3_600), Duration::from_secs(600)),
        ),
        InMemoryRevocationList::new(),
        authorizer,
    );
    Arc::new(http::AppState {
        issuer,
        pairing_codes,
        pairing_stub_enabled: false,
    })
}

async fn enroll_with(state: &Arc<http::AppState>, token: &str) -> (StatusCode, Value) {
    post(
        state,
        "/v1/csr",
        json!({
            "envTag": "dev",
            "credential": {"kind": "portalToken", "token": token},
            "csrDer": B64.encode(csr_for(&KeyPair::generate().unwrap())),
        }),
    )
    .await
}

#[tokio::test]
async fn a_spent_token_stays_spent_across_an_issuer_restart() {
    let dir = tempfile::TempDir::new().unwrap();
    let journal = dir.path().join("state").join("spent-tokens.jsonl");
    let spent = app_token("jti-restart");

    {
        let first_run = token_state(Arc::new(FileSpentTokens::open(&journal).unwrap()));
        let (status, body) = enroll_with(&first_run, &spent).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    } // the issuer process ends: nothing of it survives but the journal

    let second_run = token_state(Arc::new(FileSpentTokens::open(&journal).unwrap()));
    let (status, body) = enroll_with(&second_run, &spent).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a restart must not hand back a spent token: {body}"
    );
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("token already used"),
        "{body}"
    );

    // A token that was never used still works on the restarted issuer.
    let (status, _) = enroll_with(&second_run, &app_token("jti-fresh")).await;
    assert_eq!(status, StatusCode::OK);
}

struct FailingStore;

impl SpentTokenStore for FailingStore {
    fn try_consume(&self, _key: &str) -> Result<bool, IssuerError> {
        Err(IssuerError::Storage(
            "could not write /data/spent-tokens.jsonl: No space left on device".into(),
        ))
    }
    fn reset(&self, _key: &str) -> Result<(), IssuerError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_storage_failure_is_a_503_that_reveals_no_internals() {
    let state = token_state(Arc::new(FailingStore));
    let (status, body) = enroll_with(&state, &app_token("jti-nodisk")).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "fail closed, not waved through: {body}"
    );
    let message = body["error"].as_str().unwrap();
    assert!(message.contains("can be retried"), "{message}");
    assert!(
        !message.contains("/data") && !message.contains("space"),
        "no paths or disk detail: {message}"
    );
}
