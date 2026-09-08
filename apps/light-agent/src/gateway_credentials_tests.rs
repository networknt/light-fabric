use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[tokio::test]
async fn gateway_credentials_renew_rotate_fail_closed_and_isolate_turns() {
    use axum::{Json, Router, extract::State, routing::get, routing::post};
    #[derive(Clone)]
    struct Issuer {
        calls: Arc<AtomicUsize>,
        fail: Arc<AtomicBool>,
        bodies: Arc<Mutex<Vec<String>>>,
        claims: serde_json::Value,
    }
    const KEY: &[u8] = b"phase3-test-signing-key-32-bytes-long";
    fn sign(claims: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"phase3"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let data = format!("{header}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(KEY).unwrap();
        mac.update(data.as_bytes());
        format!(
            "{data}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }
    async fn token(
        State(state): State<Issuer>,
        headers: axum::http::HeaderMap,
        body: String,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        state.calls.fetch_add(1, Ordering::SeqCst);
        assert!(body.contains("grant_type=client_credentials"));
        assert!(body.contains("scope=llm.invoke"));
        state
            .bodies
            .lock()
            .await
            .push(headers["authorization"].to_str().unwrap().to_string());
        if state.fail.load(Ordering::SeqCst) {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "do not log secret",
            )
                .into_response();
        }
        let mut claims = state.claims;
        let now = chrono::Utc::now().timestamp();
        claims["exp"] = (now + 4).into();
        claims["iat"] = now.into();
        Json(json!({"token_type":"Bearer","access_token":sign(&claims)})).into_response()
    }
    let dir = tempfile::tempdir().unwrap();
    let host = uuid::Uuid::now_v7().to_string();
    let client_id = uuid::Uuid::now_v7();
    let calls = Arc::new(AtomicUsize::new(0));
    let fail = Arc::new(AtomicBool::new(false));
    let state = Issuer {
        calls: calls.clone(),
        fail: fail.clone(),
        bodies: Arc::new(Mutex::new(Vec::new())),
        claims: json!({"iss":"issuer","aud":"gateway","sub":client_id,"client_id":client_id,"host":host,"env":"test","scp":["llm.invoke"],"routeAlias":"assistant"}),
    };
    let bodies = state.bodies.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = Router::new().route("/token", post(token)).route("/keys",get(|| async { Json(json!({"keys":[{"kty":"oct","kid":"phase3","alg":"HS256","k":URL_SAFE_NO_PAD.encode(KEY)}]})) })).with_state(state);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    std::fs::write(
        dir.path().join("security.yml"),
        "enableVerifyJwt: true\nignoreJwtExpiry: true\nbootstrapFromKeyService: true\n",
    )
    .unwrap();
    let runtime = light_runtime::RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: Some(
            serde_yaml::from_str(&format!(
                "oauth:\n  token:\n    key:\n      server_url: http://{address}\n      uri: /keys\n"
            ))
            .unwrap(),
        ),
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: dir.path().into(),
        external_config_dir: dir.path().into(),
        resolved_values: Default::default(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Default::default(),
        cache_registry: None,
        registry_client: None,
    };
    let security = Arc::new(
        light_security::load_security_runtime(&runtime, true)
            .unwrap()
            .unwrap(),
    );
    security.bootstrap().await.unwrap();
    let secret = dir.path().join("secret");
    std::fs::write(&secret, "first-secret").unwrap();
    // Production policy validation requires HTTPS and /run/secrets. This private unit fixture uses loopback only.
    let policy = DualTokenPolicy {
        schema_version: 1,
        profile: "user-agent-dual-token-v1".into(),
        gateway_url: "https://gateway/v1".into(),
        user_issuer: "issuer".into(),
        user_audience: "gateway".into(),
        workload_issuer: "issuer".into(),
        workload_audience: "gateway".into(),
        token_endpoint: format!("http://{address}/token"),
        client_id,
        client_secret_file: secret.to_string_lossy().into(),
        scopes: vec!["llm.invoke".into()],
        refresh_before_seconds: 1,
        route_alias: "assistant".into(),
    };
    let credentials = Arc::new(WorkloadCredentials::new(
        policy,
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        security,
        host,
        "test".into(),
    ));
    let claims = json!({"iss":"issuer","aud":"gateway","exp":chrono::Utc::now().timestamp()+60});
    let a = credentials.for_turn("Bearer user-a", &claims).unwrap();
    let b = credentials.for_turn("Bearer user-b", &claims).unwrap();
    let (a_result, b_result) = tokio::join!(a.credentials(), b.credentials());
    let a_result = a_result.unwrap();
    let b_result = b_result.unwrap();
    assert_eq!(a_result.user_token, "user-a");
    assert_eq!(b_result.user_token, "user-b");
    assert_eq!(a_result.workload_token, b_result.workload_token);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Advance across a real issued-token expiry; the mounted secret is reopened.
    std::fs::write(&secret, "rotated-secret").unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    let next = a.credentials().await.unwrap();
    assert_ne!(next.workload_token, a_result.workload_token);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        bodies.lock().await.last().unwrap(),
        &format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:rotated-secret"))
        )
    );
    fail.store(true, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let fallback = a.credentials().await.unwrap();
    assert_eq!(fallback.workload_token, next.workload_token);
    assert_eq!(fallback.workload_expires_at, next.workload_expires_at);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let (x, y) = tokio::join!(a.credentials(), b.credentials());
    assert!(x.is_err() && y.is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    fail.store(false, Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    assert!(a.credentials().await.is_ok());
    let expiring = credentials
        .for_turn(
            "Bearer expired-user",
            &json!({"iss":"issuer","aud":"gateway","exp":chrono::Utc::now().timestamp()+1}),
        )
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let before = calls.load(Ordering::SeqCst);
    let error = expiring.credentials().await.err().unwrap();
    assert!(matches!(
        error.downcast_ref::<GatewayCredentialError>(),
        Some(GatewayCredentialError::AuthenticationRequired)
    ));
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert!(!error.to_string().contains("expired-user"));
    server.abort();
}

#[test]
fn gateway_user_profile_requires_audience_and_current_claims() {
    let now = chrono::Utc::now().timestamp();
    let valid = json!({"iss":"issuer","aud":["agent","gateway"],"exp":now+60,"nbf":now,"iat":now});
    assert_eq!(
        check_claims(&valid, "issuer", "gateway", now).unwrap(),
        now + 60
    );
    for (claim, value) in [
        ("iss", json!("other")),
        ("aud", json!("portal-only")),
        ("exp", json!(now)),
        ("exp", json!(null)),
        ("nbf", json!(now + 1)),
        ("iat", json!(now + 1)),
    ] {
        let mut invalid = valid.clone();
        invalid[claim] = value;
        assert!(
            check_claims(&invalid, "issuer", "gateway", now).is_err(),
            "{claim}"
        );
    }
}
