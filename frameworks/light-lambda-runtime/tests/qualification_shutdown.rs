//! Loopback API tests only; no signals sent and no application/precision child.
use light_lambda_runtime::qualification_shutdown::{
    CLEANUP_CAP, QualificationShutdown, Refusal, ShutdownState, event_deadline,
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::Instant,
};

async fn fake(
    responses: Vec<String>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let records = Arc::new(Mutex::new(Vec::new()));
    let seen = records.clone();
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 1024];
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                assert!(bytes.len() < 8192);
                if let Some(index) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..index]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|s| s.parse::<usize>().ok())
                        .unwrap_or(0);
                    if bytes.len() >= index + 4 + length {
                        break;
                    }
                }
            }
            seen.lock().unwrap().push(String::from_utf8(bytes).unwrap());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (address, records, task)
}
fn response(status: u16, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} Test\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
fn registered() -> String {
    response(
        200,
        "lambda-extension-identifier: synthetic-private-identifier\r\n",
        "{}",
    )
}
fn later() -> Instant {
    Instant::now() + Duration::from_secs(3)
}

#[tokio::test]
async fn actual_registration_and_shutdown_event_are_single_owned_requests() {
    let ms = (std::time::SystemTime::now() + Duration::from_secs(2))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let body =
        format!(r#"{{"eventType":"SHUTDOWN","shutdownReason":"SPINDOWN","deadlineMs":{ms}}}"#);
    let (authority, records, server) = fake(vec![registered(), response(200, "", &body)]).await;
    let state = ShutdownState::default();
    let bridge = QualificationShutdown::register(&authority, later(), state.clone())
        .await
        .unwrap();
    bridge.wait(std::future::pending()).await;
    server.await.unwrap();
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records[0].starts_with("POST /2020-01-01/extension/register "));
    assert!(records[0].contains(r#"{"events":["SHUTDOWN"]}"#));
    assert!(records[1].starts_with("GET /2020-01-01/extension/event/next "));
    assert!(
        records
            .iter()
            .all(|r| !r.to_ascii_lowercase().contains("authorization:"))
    );
    assert!(state.failure().is_none());
    let stop = Instant::now();
    assert!(state.cleanup_deadline(stop) <= stop + CLEANUP_CAP);
}

#[tokio::test]
async fn registration_refusals_are_typed_without_response_or_header_contents() {
    for raw in [
        response(403, "", "synthetic-private-body"),
        response(302, "location: http://127.0.0.1/\r\n", "{}"),
        response(200, "", "{}"),
        response(200, "lambda-extension-identifier: unsafe/value\r\n", "{}"),
        response(
            200,
            "lambda-extension-identifier: first\r\nlambda-extension-identifier: second\r\n",
            "{}",
        ),
        response(200, "lambda-extension-identifier: safe\r\n", "null"),
        response(
            200,
            "lambda-extension-identifier: safe\r\n",
            &"x".repeat(4097),
        ),
    ] {
        let (authority, records, server) = fake(vec![raw]).await;
        let error = QualificationShutdown::register(&authority, later(), ShutdownState::default())
            .await
            .err()
            .unwrap();
        assert!(!format!("{error:?}").contains("synthetic"));
        server.await.unwrap();
        assert_eq!(records.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn wrong_event_and_transport_failures_make_final_state_fail() {
    for raw in [
        response(500, "", "private"),
        response(200, "", r#"{"eventType":"INVOKE"}"#),
        response(200, "", "not-json"),
    ] {
        let (authority, _, server) = fake(vec![registered(), raw]).await;
        let state = ShutdownState::default();
        let bridge = QualificationShutdown::register(&authority, later(), state.clone())
            .await
            .unwrap();
        bridge.wait(std::future::pending()).await;
        assert_eq!(state.failure(), Some(Refusal::Protocol));
        server.await.unwrap();
    }
    let (authority, _, server) = fake(vec![registered()]).await;
    let state = ShutdownState::default();
    let bridge = QualificationShutdown::register(&authority, later(), state.clone())
        .await
        .unwrap();
    server.await.unwrap();
    bridge.wait(std::future::pending()).await;
    assert_eq!(state.failure(), Some(Refusal::Transport));
}

#[tokio::test]
async fn synthetic_signal_cancels_owned_poll_and_does_not_poison_fresh_state() {
    let (authority, records, server) = fake(vec![registered()]).await;
    let state = ShutdownState::default();
    let bridge = QualificationShutdown::register(&authority, later(), state.clone())
        .await
        .unwrap();
    server.await.unwrap();
    bridge.wait(async { Ok(()) }).await;
    assert!(state.failure().is_none());
    assert_eq!(records.lock().unwrap().len(), 1);
    state.record(Err(Refusal::Protocol));
    assert!(state.failure().is_none());
    let fresh = ShutdownState::default();
    fresh.record(Err(Refusal::Signal));
    fresh.record(Ok(None));
    assert_eq!(fresh.failure(), Some(Refusal::Signal));
}

#[test]
fn event_clock_reserve_expiry_and_malformed_fields_are_exact() {
    let wall = UNIX_EPOCH + Duration::from_secs(100);
    let now = Instant::now();
    let event = |ms: u64| {
        format!(r#"{{"eventType":"SHUTDOWN","shutdownReason":"SPINDOWN","deadlineMs":{ms}}}"#)
    };
    assert_eq!(
        event_deadline(event(100100).as_bytes(), wall, now).unwrap(),
        now + Duration::from_millis(50)
    );
    assert_eq!(
        event_deadline(event(99999).as_bytes(), wall, now).unwrap(),
        now
    );
    let state = ShutdownState::default();
    state.record(Ok(Some(now)));
    assert_eq!(state.cleanup_deadline(now + Duration::from_millis(1)), now);
    for body in [
        r#"{"eventType":"SHUTDOWN","shutdownReason":"x","deadlineMs":null}"#,
        r#"{"eventType":"SHUTDOWN","shutdownReason":"x","deadlineMs":-1}"#,
        r#"{"eventType":"SHUTDOWN","shutdownReason":"x","deadlineMs":1.5}"#,
        r#"{"eventType":"SHUTDOWN","deadlineMs":100000}"#,
    ] {
        assert_eq!(
            event_deadline(body.as_bytes(), wall, now),
            Err(Refusal::Protocol)
        );
    }
    // A representable far-future event cannot increase the cap.
    let state = ShutdownState::default();
    let future = event_deadline(event(u64::MAX).as_bytes(), wall, now);
    match future {
        Ok(deadline) => {
            state.record(Ok(Some(deadline)));
            assert_eq!(state.cleanup_deadline(now), now + CLEANUP_CAP);
        }
        Err(e) => assert_eq!(e, Refusal::Protocol),
    }
}

#[tokio::test]
async fn invalid_authority_and_expired_registration_do_not_gain_time() {
    assert_eq!(
        QualificationShutdown::register("http://bad/path", later(), ShutdownState::default())
            .await
            .err(),
        Some(Refusal::Configuration)
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    assert_eq!(
        QualificationShutdown::register(
            &listener.local_addr().unwrap().to_string(),
            Instant::now(),
            ShutdownState::default()
        )
        .await
        .err(),
        Some(Refusal::Registration)
    );
}

#[tokio::test]
async fn external_name_is_fixed_and_internal_shutdown_refusal_stays_failure() {
    let (authority, records, server) = fake(vec![registered()]).await;
    let _bridge =
        QualificationShutdown::register_external(&authority, later(), ShutdownState::default())
            .await
            .unwrap();
    server.await.unwrap();
    assert_eq!(records.lock().unwrap().len(), 1);
    assert!(
        records.lock().unwrap()[0]
            .to_ascii_lowercase()
            .contains("lambda-extension-name: validator-shutdown-bridge")
    );
    let (authority, records, server) = fake(vec![response(
        400,
        "",
        r#"{"error":"ShutdownEventNotSupportedForInternalExtension"}"#,
    )])
    .await;
    let error = QualificationShutdown::register(&authority, later(), ShutdownState::default())
        .await
        .err()
        .unwrap();
    assert_eq!(error, Refusal::Registration);
    server.await.unwrap();
    assert_eq!(records.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn external_receiver_preserves_validated_platform_deadline_and_reason() {
    let ms = (std::time::SystemTime::now() + Duration::from_secs(2))
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let body =
        format!(r#"{{"eventType":"SHUTDOWN","shutdownReason":"SPINDOWN","deadlineMs":{ms}}}"#);
    let (authority, records, server) = fake(vec![registered(), response(200, "", &body)]).await;
    let bridge =
        QualificationShutdown::register_external(&authority, later(), ShutdownState::default())
            .await
            .unwrap();
    assert_eq!(bridge.external_event().await.unwrap(), body.as_bytes());
    server.await.unwrap();
    assert_eq!(records.lock().unwrap().len(), 2);
}
