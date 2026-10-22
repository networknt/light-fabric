use super::*;
use crate::{msal_auth::policy_tests::jwks_server, spa_trust::SpaTokenPolicy};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const KEYS: &str = include_str!("../tests/fixtures/msal/signing-key.jwks.json");
const KEY: &[u8] = include_bytes!("../tests/fixtures/msal/signing-key.pem");
const OTHER: &[u8] = include_bytes!("../tests/fixtures/msal/other-signing-key.pem");

fn token(csrf: &str, kid: &str, expiry: u64, mode: Mode) -> String {
    let mut claims = json!({"iss":"https://internal.example","aud":"portal","sub":"fixture","csrf":csrf,"exp":expiry});
    if matches!(mode, Mode::WrongAudience) {
        claims["aud"] = json!("another-app");
    }
    if matches!(mode, Mode::WrongCsrf) {
        claims["csrf"] = json!("not-the-requested-binding");
    }
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    encode(
        &header,
        &claims,
        &EncodingKey::from_rsa_pem(if matches!(mode, Mode::BadSignature) {
            OTHER
        } else {
            KEY
        })
        .unwrap(),
    )
    .unwrap()
}
#[derive(Clone, Copy)]
pub(crate) enum Mode {
    Valid,
    NewKey,
    WrongAudience,
    WrongCsrf,
    BadSignature,
}

pub(crate) struct OAuthFixture {
    pub(crate) url: String,
    pub(crate) requests: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for OAuthFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub(crate) async fn oauth(mode: Mode) -> OAuthFixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let task = tokio::spawn(async move {
        let mut valid = "old".to_owned();
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut bytes = Vec::new();
            let form = loop {
                let mut buffer = [0; 4096];
                let n = socket.read(&mut buffer).await.unwrap();
                assert!(n > 0);
                bytes.extend_from_slice(&buffer[..n]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break url::form_urlencoded::parse(&bytes[end + 4..end + 4 + length])
                            .into_owned()
                            .collect::<BTreeMap<_, _>>();
                    }
                }
            };
            let request_no = count.fetch_add(1, Ordering::SeqCst) + 1;
            let (status, body) = if form.get("refresh_token").is_some_and(|v| v == &valid)
                || form
                    .get("grant_type")
                    .is_some_and(|v| v == TOKEN_EXCHANGE_GRANT)
            {
                // The old credential is invalidated before the client can verify the JWT.
                valid = format!("rotated-{request_no}");
                let access = token(
                    &form["csrf"],
                    if matches!(mode, Mode::NewKey) {
                        "rotated"
                    } else {
                        "msal-test"
                    },
                    now_seconds() + 3600,
                    mode,
                );
                (
                    200,
                    json!({"access_token":access,"refresh_token":valid,"expires_in":3600})
                        .to_string(),
                )
            } else {
                (400, json!({"error":"invalid_grant"}).to_string())
            };
            socket.write_all(format!("HTTP/1.1 {status} Result\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    OAuthFixture {
        url,
        requests,
        task,
    }
}

pub(crate) async fn runtime(
    uri: &str,
    client: reqwest::Client,
    oauth: &OAuthFixture,
) -> SpaSessionRuntime {
    let cookies = SpaCookieConfig {
        cookie_domain: "localhost".into(),
        cookie_path: "/".into(),
        cookie_secure: true,
        session_timeout: 3600,
        remember_me_timeout: 86400,
        cookie_same_site: CookieSameSite::Lax,
        renew_before_seconds: 90,
        refresh_single_flight_wait_ms: 5000,
        refresh_single_flight_cache_ms: 60000,
        refresh_single_flight_max_entries: 10,
        cookie_timeout_uri: String::new(),
    };
    let mut trust = SpaTokenTrust::new(
        SpaTokenPolicy {
            issuer: "https://internal.example".into(),
            audience: "portal".into(),
            jwks_uri: uri.into(),
            ..Default::default()
        },
        TokenRole::Internal,
    )
    .unwrap();
    Arc::get_mut(&mut trust.source).unwrap().client = client;
    trust.source.seed_test_keys(KEYS);
    let mut config = ClientTokenConfig::default();
    config.oauth.token.server_url = Some(oauth.url.clone());
    config.oauth.token.refresh_token.client_id = "fixture".into();
    config.oauth.token.refresh_token.client_secret = "fixture-secret".into();
    config.oauth.token.token_exchange.client_id = "exchange-fixture".into();
    config.oauth.token.token_exchange.client_secret = "fixture-secret".into();
    SpaSessionRuntime::new(cookies, SpaTokenClient::new(config, None), trust)
}

#[tokio::test]
async fn refresh_alias_at_capacity_preserves_concurrent_original_requests() {
    for capacity in [1, 2] {
        let oauth = oauth(Mode::Valid).await;
        let mut config = ClientTokenConfig::default();
        config.oauth.token.server_url = Some(oauth.url.clone());
        config.oauth.token.refresh_token.client_id = "fixture".into();
        config.oauth.token.refresh_token.client_secret = "fixture-secret".into();
        let client = SpaTokenClient::new(config, None);
        let refresh = RefreshSingleFlight::new(60_000, capacity);
        if capacity == 2 {
            // At capacity, "old" sorts before this unrelated cached grant.
            refresh.entries.lock().await.insert(
                ("zzz-unrelated".into(), Some("verified".into())),
                RefreshResult {
                    response: serde_json::from_value(json!({"access_token":"unrelated"})).unwrap(),
                    csrf: "verified".into(),
                    completed_at_millis: now_millis(),
                },
            );
        }
        let (first, concurrent) = tokio::join!(
            refresh.renew(&client, "old", Some("verified")),
            refresh.renew(&client, "old", Some("verified")),
        );
        assert!(first.is_ok());
        assert!(
            concurrent.is_ok(),
            "original credential lost at capacity {capacity}"
        );
        let first = first.unwrap();
        assert_eq!(concurrent.unwrap().response, first.response);
        let alias = refresh
            .renew(&client, "rotated-1", Some("verified"))
            .await
            .unwrap();
        assert_eq!(alias.response, first.response);
        assert_eq!(oauth.requests.load(Ordering::SeqCst), 1);
        assert_eq!(refresh.entries.lock().await.len(), capacity);
    }
}

#[tokio::test]
async fn refresh_alias_respects_csrf_partition_and_grant_expiry() {
    let oauth = oauth(Mode::Valid).await;
    let mut config = ClientTokenConfig::default();
    config.oauth.token.server_url = Some(oauth.url.clone());
    config.oauth.token.refresh_token.client_id = "fixture".into();
    config.oauth.token.refresh_token.client_secret = "fixture-secret".into();
    let client = SpaTokenClient::new(config, None);
    let refresh = RefreshSingleFlight::new(60_000, 1);
    refresh
        .renew(&client, "old", Some("verified"))
        .await
        .unwrap();
    let other = refresh.renew(&client, "rotated-1", None).await.unwrap();
    assert_ne!(other.csrf, "verified");
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 2);
    assert_eq!(refresh.entries.lock().await.len(), 1);
    // Expiring a grant must expire its rotated credential lookup as well.
    for grant in refresh.entries.lock().await.values_mut() {
        grant.completed_at_millis = 0;
    }
    let renewed = refresh.renew(&client, "rotated-2", None).await.unwrap();
    assert_eq!(renewed.response.refresh_token.as_deref(), Some("rotated-3"));
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 3);
}

async fn request(access: Option<&str>, refresh: &str, origin: Option<&str>) -> Session {
    let access = access
        .map(|v| format!("accessToken={v}; "))
        .unwrap_or_default();
    let origin = origin
        .map(|v| format!("Origin: {v}\r\n"))
        .unwrap_or_default();
    let raw = format!(
        "POST /operation HTTP/1.1\r\nHost: localhost\r\nCookie: {access}refreshToken={refresh}; csrf=verified\r\nX-CSRF-TOKEN: verified\r\n{origin}Content-Length: 0\r\n\r\n"
    );
    let mut session = Session::new_h1(Box::new(std::io::Cursor::new(raw.into_bytes())));
    assert!(session.as_downstream_mut().read_request().await.unwrap());
    session
}
fn only_rotated_refresh(headers: &[(String, String)]) {
    let cookies = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case("set-cookie"))
        .collect::<Vec<_>>();
    assert_eq!(cookies.len(), 1);
    assert!(cookies[0].1.starts_with("refreshToken=rotated-"));
    assert!(cookies[0].1.contains("HttpOnly"));
    assert!(cookies[0].1.contains("Secure"));
    assert!(cookies[0].1.contains("Max-Age=3600"));
}
fn published_rotation() -> String {
    let mut keys: JsonValue = serde_json::from_str(KEYS).unwrap();
    let mut next = keys["keys"][0].clone();
    next["kid"] = json!("rotated");
    keys["keys"].as_array_mut().unwrap().push(next);
    keys.to_string()
}

#[tokio::test]
async fn proactive_uncertainty_keeps_rotated_refresh_and_valid_original_without_retry_after() {
    let keys = jwks_server(KEYS).await;
    let oauth = oauth(Mode::NewKey).await;
    let runtime = runtime(&keys.uri, keys.client.clone(), &oauth).await;
    let original = token("verified", "msal-test", now_seconds() + 30, Mode::NewKey);
    let mut session = request(Some(&original), "old", None).await;
    let SpaSessionOutcome::Continue {
        auth: Some(_),
        response_headers,
    } = runtime.validate_or_refresh(&mut session).await.unwrap()
    else {
        panic!("valid original must continue")
    };
    assert!(session.req_header().headers["authorization"] == format!("Bearer {original}"));
    only_rotated_refresh(&response_headers);
    assert!(
        !response_headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("retry-after"))
    );
    // A retry with the new credential reuses the pending result, not an invalidated old refresh.
    let mut retry = request(Some(&original), "rotated-1", None).await;
    runtime.validate_or_refresh(&mut retry).await.unwrap();
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 1);
    *keys.body.write().await = published_rotation();
    runtime.trust.source.allow_test_refresh();
    let mut recovered = request(Some(&original), "rotated-1", None).await;
    let SpaSessionOutcome::Continue {
        response_headers, ..
    } = runtime.validate_or_refresh(&mut recovered).await.unwrap()
    else {
        panic!("must recover")
    };
    assert!(
        response_headers
            .iter()
            .any(|(n, v)| n == "set-cookie" && v.starts_with("csrf=verified;"))
    );
    assert!(recovered.req_header().headers["authorization"] != format!("Bearer {original}"));
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .token_client
            .refresh_token("old", "verified")
            .await
            .unwrap_err()
            .status,
        401
    );
}

#[tokio::test]
async fn expired_original_uncertainty_never_forwards_and_retains_only_rotated_refresh() {
    let keys = jwks_server(KEYS).await;
    let oauth = oauth(Mode::NewKey).await;
    let runtime = runtime(&keys.uri, keys.client, &oauth).await;
    let original = token("verified", "msal-test", now_seconds() - 1, Mode::NewKey);
    let mut session = request(Some(&original), "old", None).await;
    let error = runtime
        .validate_or_refresh(&mut session)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 503);
    only_rotated_refresh(&error.headers);
    assert!(
        error
            .headers
            .iter()
            .any(|(n, v)| n == "retry-after" && v == "1")
    );
    assert!(!session.req_header().headers.contains_key("authorization"));
}

#[tokio::test]
async fn expiry_is_rechecked_after_slow_verification_before_original_fallback() {
    let keys = jwks_server("not JWKS").await;
    keys.delay_ms.store(1500, Ordering::SeqCst);
    let oauth = oauth(Mode::NewKey).await;
    let runtime = runtime(&keys.uri, keys.client, &oauth).await;
    runtime.trust.source.allow_test_refresh();
    let original = token("verified", "msal-test", now_seconds() + 1, Mode::NewKey);
    let mut session = request(Some(&original), "old", None).await;
    let error = runtime
        .validate_or_refresh(&mut session)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 503);
    only_rotated_refresh(&error.headers);
    assert!(!session.req_header().headers.contains_key("authorization"));
}

#[tokio::test]
async fn confirmed_missing_key_and_invalid_issuance_are_terminal_without_new_credentials() {
    for mode in [
        Mode::NewKey,
        Mode::BadSignature,
        Mode::WrongAudience,
        Mode::WrongCsrf,
    ] {
        let keys = jwks_server(KEYS).await;
        let oauth = oauth(mode).await;
        let runtime = runtime(&keys.uri, keys.client, &oauth).await;
        runtime.trust.source.allow_test_refresh();
        let original = token("verified", "msal-test", now_seconds() + 30, Mode::NewKey);
        let mut session = request(Some(&original), "old", None).await;
        let error = runtime
            .validate_or_refresh(&mut session)
            .await
            .err()
            .unwrap();
        assert_eq!(error.status, 401);
        assert!(!error.headers.iter().any(|(n, _)| n == "set-cookie"));
        assert!(!session.req_header().headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn refresh_only_recovery_requires_trusted_browser_and_never_executes_original_request() {
    let keys = jwks_server(KEYS).await;
    let oauth = oauth(Mode::NewKey).await;
    let runtime = runtime(&keys.uri, keys.client.clone(), &oauth)
        .await
        .with_recovery_origins(vec!["https://portal.example".into()])
        .unwrap();
    for origin in [None, Some("null"), Some("https://attacker.example")] {
        let mut session = request(None, "old", origin).await;
        assert_eq!(
            runtime
                .validate_or_refresh(&mut session)
                .await
                .err()
                .unwrap()
                .status,
            401
        );
    }
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 0);
    let mut session = request(None, "old", Some("https://portal.example")).await;
    let error = runtime
        .validate_or_refresh(&mut session)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 503);
    only_rotated_refresh(&error.headers);
    assert!(!session.req_header().headers.contains_key("authorization"));
    *keys.body.write().await = published_rotation();
    runtime.trust.source.allow_test_refresh();
    let mut retry = request(None, "rotated-1", Some("https://portal.example")).await;
    let SpaSessionOutcome::Respond(response) =
        runtime.validate_or_refresh(&mut retry).await.unwrap()
    else {
        panic!("recovery must not dispatch")
    };
    assert_eq!(response.status, 409);
    assert!(!retry.req_header().headers.contains_key("authorization"));
    assert!(response.headers.iter().any(|(n, v)| n == "set-cookie"
        && v.starts_with("csrf=")
        && !v.starts_with("csrf=verified;")));
    assert_eq!(oauth.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn issuance_uncertainty_preserves_only_opaque_refresh_and_never_unverified_identity() {
    let keys = jwks_server(KEYS).await;
    let oauth = oauth(Mode::NewKey).await;
    let runtime = runtime(&keys.uri, keys.client, &oauth).await;
    let grant = TokenGrantResponse {
        access_token: token("verified", "rotated", now_seconds() + 3600, Mode::NewKey),
        refresh_token: Some("rotated-1".into()),
        expires_in: Some(3600),
        remember: None,
        scope: None,
        extra: HashMap::new(),
    };
    let error = runtime
        .set_login_cookies(&grant, "verified")
        .await
        .unwrap_err();
    assert_eq!(error.status, 503);
    only_rotated_refresh(&error.headers);
}
