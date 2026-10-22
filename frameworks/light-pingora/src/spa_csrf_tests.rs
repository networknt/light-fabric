use super::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};

async fn request(path: &str, headers: &str, method: &str) -> Session {
    let raw = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n");
    let mut session = Session::new_h1(Box::new(std::io::Cursor::new(raw.into_bytes())));
    assert!(session.as_downstream_mut().read_request().await.unwrap());
    session
}

fn jwt(csrf: &str, exp: u64) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("msal-test".into());
    encode(
        &header,
        &json!({"iss":"https://internal.example", "aud":"portal", "sub":"fixture", "csrf":csrf, "exp":exp}),
        &EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/msal/signing-key.pem")).unwrap(),
    )
    .unwrap()
}

async fn runtime() -> SpaSessionRuntime {
    let cookies = SpaCookieConfig {
        cookie_domain: "localhost".into(),
        cookie_path: "/".into(),
        cookie_secure: true,
        session_timeout: 3600,
        remember_me_timeout: 3600,
        cookie_same_site: CookieSameSite::Lax,
        renew_before_seconds: 90,
        refresh_single_flight_wait_ms: 5000,
        refresh_single_flight_cache_ms: 3000,
        refresh_single_flight_max_entries: 10,
        cookie_timeout_uri: String::new(),
    };
    SpaSessionRuntime::new(
        cookies,
        SpaTokenClient::new(ClientTokenConfig::default(), None),
        SpaTokenTrust::test_internal(),
    )
    .with_websocket_origins(vec!["https://portal.example".into()])
    .unwrap()
}

fn credentials(token: &str, csrf: &str) -> String {
    format!("Cookie: accessToken={token}; csrf={csrf}\r\nX-CSRF-TOKEN: {csrf}\r\n")
}

#[tokio::test]
async fn query_credentials_are_rejected_even_with_a_valid_header() {
    for path in [
        "/api?csrf=secret",
        "/api?csrf=",
        "/api?%63srf=secret",
        "/api?X-CSRF-TOKEN=secret",
    ] {
        for headers in ["", "X-CSRF-TOKEN: valid\r\n"] {
            let session = request(path, headers, "GET").await;
            assert!(request_csrf(&session, &[]).is_err());
            assert!(
                !request_csrf(&session, &[])
                    .unwrap_err()
                    .message
                    .contains("secret")
            );
        }
    }
}

#[tokio::test]
async fn bounded_unambiguous_http_headers_preserve_legacy_signed_claims() {
    let session = request(
        "/api",
        "Cookie: csrf=legacy-uuid-csrf\r\nX-CSRF-TOKEN: legacy-uuid-csrf\r\n",
        "GET",
    )
    .await;
    assert!(validate_csrf(&session, &json!({"csrf":"legacy-uuid-csrf"}), &[]).is_ok());
    for headers in [
        String::new(),
        "X-CSRF-TOKEN: \r\n".into(),
        "X-CSRF-TOKEN: one\r\nX-CSRF-TOKEN: two\r\n".into(),
        format!("X-CSRF-TOKEN: {}\r\n", "x".repeat(513)),
        "X-CSRF-TOKEN: attacker\r\nCookie: csrf=one; csrf=attacker\r\n".into(),
    ] {
        assert!(
            validate_csrf(
                &request("/api", &headers, "GET").await,
                &json!({"csrf":"attacker"}),
                &[]
            )
            .is_err()
        );
    }
}

const UPGRADE: &str = "Upgrade: websocket\r\nConnection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";

#[tokio::test]
async fn websocket_csrf_requires_a_real_upgrade_and_exact_trusted_origin() {
    let origins = vec!["https://portal.example".to_owned()];
    let headers = format!(
        "{UPGRADE}Origin: https://portal.example\r\nSec-WebSocket-Protocol: light-spa-v1, csrf.bound\r\n"
    );
    assert_eq!(
        request_csrf(&request("/chat", &headers, "GET").await, &origins).unwrap(),
        "bound"
    );
    assert!(request_csrf(&request("/chat", &headers, "GET").await, &[]).is_err());
    for bad in [
        headers.replace("Origin: https://portal.example\r\n", ""),
        headers.replace("https://portal.example", "null"),
        headers.replace("https://portal.example", "https://attacker.example"),
        headers.replace("https://portal.example", "https://portal.example/"),
        headers.replace("Upgrade: websocket", "Upgrade: invalid"),
        headers.replace("Version: 13", "Version: 12"),
        headers.replace("dGhlIHNhbXBsZSBub25jZQ==", "bad"),
        headers.replace("csrf.bound", "csrf.bound, csrf.bound"),
        format!("{headers}X-CSRF-TOKEN: conflicting\r\n"),
        format!("{headers}Origin: https://portal.example\r\n"),
        "Sec-WebSocket-Protocol: csrf.bound\r\n".into(),
    ] {
        assert!(request_csrf(&request("/chat", &bad, "GET").await, &origins).is_err());
    }
    assert!(request_csrf(&request("/chat", &headers, "POST").await, &origins).is_err());
    assert!(validate_websocket_origins(&["https://portal.example/path".into()]).is_err());
}

#[tokio::test]
async fn jwt_sessions_reject_attacker_pairs_and_cross_session_substitution_before_dispatch() {
    let runtime = runtime().await;
    let token = jwt("signed-context", now_seconds() + 3600);
    let mut valid = request("/api", &credentials(&token, "signed-context"), "GET").await;
    assert!(matches!(
        runtime.validate_or_refresh(&mut valid).await.unwrap(),
        SpaSessionOutcome::Continue { auth: Some(_), .. }
    ));
    for csrf in ["attacker", "another-signed-session"] {
        let mut denied = request("/api", &credentials(&token, csrf), "GET").await;
        assert!(runtime.validate_or_refresh(&mut denied).await.is_err());
        assert!(!denied.req_header().headers.contains_key("authorization"));
    }
    for cookie in [
        "refreshToken=opaque; csrf=attacker",
        "csrf=attacker",
        "csrf",
        "accessToken=",
        "accessToken",
        "accessToken=malformed; csrf=attacker",
    ] {
        let mut denied = request(
            "/api",
            &format!("Cookie: {cookie}\r\nX-CSRF-TOKEN: attacker\r\n"),
            "GET",
        )
        .await;
        assert!(runtime.validate_or_refresh(&mut denied).await.is_err());
        assert!(!denied.req_header().headers.contains_key("authorization"));
    }
}

#[tokio::test]
async fn jwt_logout_requires_signature_and_binding_but_allows_expired_cleanup() {
    let runtime = runtime().await;
    let endpoint = SpaAuthCsrfEndpoint::StatelessLogout;
    for exp in [now_seconds() + 3600, now_seconds() - 3600] {
        let token = jwt("signed-context", exp);
        let valid = request("/logout", &credentials(&token, "signed-context"), "POST").await;
        assert!(
            runtime
                .validate_logout_csrf(&valid, endpoint, true, &[])
                .await
                .is_ok()
        );
        let bad = request("/logout", &credentials(&token, "attacker"), "POST").await;
        assert!(
            runtime
                .validate_logout_csrf(&bad, endpoint, true, &[])
                .await
                .is_err()
        );
    }
    for cookie in [
        "refreshToken=opaque; csrf=attacker",
        "csrf=attacker",
        "csrf",
        "accessToken=",
        "accessToken",
        "accessToken=malformed; csrf=attacker",
    ] {
        let bad = request(
            "/logout",
            &format!("Cookie: {cookie}\r\nX-CSRF-TOKEN: attacker\r\n"),
            "POST",
        )
        .await;
        assert!(
            runtime
                .validate_logout_csrf(&bad, endpoint, true, &[])
                .await
                .is_err()
        );
    }
    let empty = request("/logout", "", "POST").await;
    assert!(
        runtime
            .validate_logout_csrf(&empty, endpoint, true, &[])
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn issuance_rejects_cookie_and_signed_claim_disagreement() {
    let runtime = runtime().await;
    let response = TokenGrantResponse {
        access_token: jwt("signed", now_seconds() + 3600),
        refresh_token: None,
        expires_in: Some(3600),
        remember: None,
        scope: None,
        extra: HashMap::new(),
    };
    assert!(runtime.set_login_cookies(&response, "signed").await.is_ok());
    assert!(
        runtime
            .set_login_cookies(&response, "attacker")
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_authenticated_renewal_preserves_the_claim_and_cookie_pair() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let renewed = jwt("verified-renewal", now_seconds() + 3600);
    let expected = renewed.clone();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        loop {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&buffer[..count]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    let form: BTreeMap<_, _> =
                        url::form_urlencoded::parse(&bytes[end + 4..end + 4 + length])
                            .into_owned()
                            .collect();
                    assert_eq!(
                        form.get("csrf").map(String::as_str),
                        Some("verified-renewal")
                    );
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let body = json!({"access_token": renewed, "refresh_token":"rotated", "expires_in":3600})
            .to_string();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        // Closing the listener also proves callers did not issue a second renewal.
    });
    let mut runtime = runtime().await;
    let mut config = ClientTokenConfig::default();
    config.oauth.token.server_url = Some(format!("http://{address}"));
    config.oauth.token.refresh_token.client_id = "fixture".into();
    config.oauth.token.refresh_token.client_secret = "fixture-secret".into();
    runtime.token_client = Arc::new(SpaTokenClient::new(config, None));
    let runtime = Arc::new(runtime);
    let old = jwt("verified-renewal", now_seconds() + 30);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let runtime = runtime.clone();
        let headers = format!(
            "Cookie: accessToken={old}; refreshToken=refresh; csrf=verified-renewal\r\nX-CSRF-TOKEN: verified-renewal\r\n"
        );
        let expected = expected.clone();
        tasks.push(tokio::spawn(async move {
            let mut session = request("/api", &headers, "POST").await;
            let SpaSessionOutcome::Continue {
                auth: Some(principal),
                response_headers,
            } = runtime.validate_or_refresh(&mut session).await.unwrap()
            else {
                panic!("renewal rejected");
            };
            assert_eq!(principal.claims["csrf"], "verified-renewal");
            assert!(
                response_headers
                    .iter()
                    .any(|(name, value)| name == "set-cookie"
                        && value.starts_with("csrf=verified-renewal;"))
            );
            assert_eq!(
                session.req_header().headers["authorization"],
                format!("Bearer {expected}")
            );
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    server.await.unwrap();
}

#[tokio::test]
async fn exchange_and_stateless_handlers_apply_the_shared_binding_and_logout_policy() {
    use crate::msal_exchange::{MsalExchangeConfig, MsalExchangeRuntime};
    use crate::stateless_auth::{StatelessAuthConfig, StatelessAuthRuntime};
    let exchange = MsalExchangeRuntime::new(
        MsalExchangeConfig::default(),
        runtime().await,
        SpaTokenTrust::test_internal(),
        None,
    );
    let stateless = StatelessAuthRuntime::new(StatelessAuthConfig::default(), runtime().await);
    let token = jwt("verified", now_seconds() + 3600);
    for (path, csrf) in [
        ("/api", "attacker"),
        ("/api?csrf=verified", "verified"),
        ("/api?csrf=attacker", "verified"),
        ("/auth/ms/logout", "attacker"),
    ] {
        let mut session = request(path, &credentials(&token, csrf), "POST").await;
        assert!(exchange.handle_request(&mut session).await.is_err());
        assert!(!session.req_header().headers.contains_key("authorization"));
    }
    for (path, csrf) in [
        ("/api", "attacker"),
        ("/api?csrf=verified", "verified"),
        ("/logout", "attacker"),
    ] {
        let mut session = request(path, &credentials(&token, csrf), "POST").await;
        assert!(
            stateless
                .handle_request(&mut session, "stateless")
                .await
                .is_err()
        );
        assert!(!session.req_header().headers.contains_key("authorization"));
    }
}
