mod security_support;
use security_support::*;
use serverless_core::{handlers::jwt::JwtHandler, unsupported::validate_security, *};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[tokio::test]
async fn trace_logs_never_contain_tokens_claim_secret_or_test_logging_override() {
    let capture = Capture::default();
    let writer = capture.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .unwrap();
    let (server, key) = server(false).await;
    let mut cfg = config();
    cfg.log_jwt_token = true;
    let settings = validate_security(&serde_json::to_value(&cfg).unwrap(), true).unwrap();
    let jwt = Arc::new(
        JwtHandler::new(
            settings,
            Some(Arc::new(runtime_with_client(cfg, serde_json::json!({
                "request":{"timeout":2000,"connectTimeout":2000},
                "oauth":{"token":{"key":{"server_url":server.url,"uri":"/jwks","enableHttp2":false,
                    "client_id":"test-only-client","client_secret":"SECRET-MARKER-CLIENT-WP9a"}}}
            })))),
            vec![],
        )
        .unwrap(),
    );
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(jwt, HandlerId::Jwt, probe.clone());
    let token = signed(&key, claims("redaction-user"));
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    denied(
        &pipe,
        &probe,
        invocation(Some("Bearer MALFORMED-TOKEN-MARKER-WP9a"), "/x"),
        401,
        "ERR10000",
    )
    .await;
    let mut scoped = invocation(Some(&format!("Bearer {token}")), "/x");
    scoped.headers.insert(
        "x-scope-token",
        "Bearer MALFORMED-SCOPE-MARKER-WP9a".parse().unwrap(),
    );
    denied(&pipe, &probe, scoped, 401, "ERR10003").await;
    let mut c = claims("expired-user");
    c["exp"] = serde_json::json!(jwks::now() - 120);
    let expired = signed(&key, c);
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {expired}")), "/x"),
        401,
        "ERR10001",
    )
    .await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        !logs.is_empty(),
        "capture must observe actual shared verification logs"
    );
    for marker in [
        &token,
        &expired,
        "SECRET-MARKER-WP9a",
        "MALFORMED-TOKEN-MARKER-WP9a",
        "MALFORMED-SCOPE-MARKER-WP9a",
        "SECRET-MARKER-CLIENT-WP9a",
    ] {
        assert!(!logs.contains(marker), "redaction marker must be absent");
    }
    use base64::Engine;
    assert!(!logs.contains(&base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&key)));
    // Print only after all redaction assertions; --nocapture preserves safe TRACE evidence.
    println!("{logs}");
}
