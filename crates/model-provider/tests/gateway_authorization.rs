use model_provider::{compatible::CompatibleProvider, gateway_authorization::*, traits::Provider};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
struct Authority {
    user: &'static str,
    exp: i64,
    calls: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl GatewayAuthorization for Authority {
    async fn credentials(&self) -> anyhow::Result<GatewayRequestCredentials> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayRequestCredentials {
            user_token: self.user.into(),
            workload_token: "agent-secret".into(),
            user_expires_at: self.exp,
            workload_expires_at: self.exp,
        })
    }
}
#[tokio::test]
async fn gateway_pairs_are_request_scoped_rechecked_and_never_redirected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let seen = Arc::new(AtomicUsize::new(0));
    let count = seen.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![];
            let mut buffer = [0; 8192];
            loop {
                let size = socket.read(&mut buffer).await.unwrap();
                assert!(size > 0);
                bytes.extend_from_slice(&buffer[..size]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            let lower = request.to_lowercase();
            assert!(lower.contains("x-scope-token: bearer agent-secret"));
            assert!(!request.contains("registry-secret"));
            let user = if request.contains("user-a") {
                "user-a"
            } else {
                "user-b"
            };
            assert!(lower.contains(&format!("authorization: bearer {user}")));
            assert!(request.contains(&format!("\"content\":\"{user}\"")));
            let call = count.fetch_add(1, Ordering::SeqCst);
            let response = if call == 2 {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://{address}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
            } else {
                let body = r#"{"choices":[{"message":{"content":"ok"}}]}"#;
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let exp = chrono::Utc::now().timestamp() + 60;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let provider = |user, expires| {
        CompatibleProvider::new_with_client(
            "gateway",
            &format!("http://{address}/v1"),
            Some("registry-secret"),
            client.clone(),
        )
        .with_gateway_authorization(Arc::new(Authority {
            user,
            exp: expires,
            calls: calls.clone(),
        }))
    };
    let a = provider("user-a", exp);
    let b = provider("user-b", exp);
    let (x, y) = tokio::join!(
        a.chat_with_system(None, "user-a", "alias", 0.0),
        b.chat_with_system(None, "user-b", "alias", 0.0)
    );
    assert_eq!(x.unwrap(), "ok");
    assert_eq!(y.unwrap(), "ok");
    assert!(
        a.chat_with_system(None, "user-a", "alias", 0.0)
            .await
            .unwrap_err()
            .to_string()
            .contains("307")
    );
    server.await.unwrap();
    assert_eq!(seen.load(Ordering::SeqCst), 3);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let expired = provider("user-a", chrono::Utc::now().timestamp());
    let error = expired
        .chat_with_system(None, "user-a", "alias", 0.0)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<GatewayCredentialError>(),
        Some(GatewayCredentialError::AuthenticationRequired)
    ));
    let mut changed = provider("user-a", exp);
    changed.base_url = "http://127.0.0.1:1".into();
    assert!(
        changed
            .chat_with_system(None, "user-a", "alias", 0.0)
            .await
            .unwrap_err()
            .to_string()
            .contains("destination changed")
    );
}

#[tokio::test]
async fn legacy_provider_preserves_single_authorization_header() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        loop {
            let size = socket.read(&mut buffer).await.unwrap();
            assert!(size > 0);
            bytes.extend_from_slice(&buffer[..size]);
            if bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8(bytes).unwrap().to_lowercase();
        assert!(request.contains("authorization: bearer legacy-key"));
        assert!(!request.contains("x-scope-token"));
        let body = r#"{"choices":[{"message":{"content":"legacy-ok"}}]}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
    });
    let provider = CompatibleProvider::new_with_client(
        "legacy",
        &format!("http://{address}/v1"),
        Some("legacy-key"),
        reqwest::Client::new(),
    );
    assert_eq!(
        provider
            .chat_with_system(None, "hello", "alias", 0.0)
            .await
            .unwrap(),
        "legacy-ok"
    );
    server.await.unwrap();
}
