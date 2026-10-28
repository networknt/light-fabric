mod support;
use serde_json::{Value, json};
use serverless_aws::{invoke::InvokeOptions, limits::Limits, v1::build_v1_event};
use serverless_core::{Backend, ExchangeState, Target};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use support::*;
use tokio::time::Instant;
fn target() -> Target {
    Target::Function("FnX".into())
}

#[tokio::test]
async fn invoke_payload_exact_limit_and_one_byte_over_are_checked_before_network() {
    let fake = Fake::start(vec![Reply::ok()]).await;
    let mut inv = invocation(event());
    inv.headers
        .insert("x-finalized", "after-policy".parse().unwrap());
    let n = serde_json::to_vec(
        &build_v1_event(&inv, &ExchangeState::default(), &cfg(), time()).unwrap(),
    )
    .unwrap()
    .len();
    let accepted = fake.backend(
        InvokeOptions::default(),
        Limits {
            max_invoke_payload_bytes: n,
            ..Limits::default()
        },
    );
    accepted
        .dispatch(&inv, &ExchangeState::default(), &target())
        .await
        .unwrap();
    let rejected = fake.backend(
        InvokeOptions::default(),
        Limits {
            max_invoke_payload_bytes: n - 1,
            ..Limits::default()
        },
    );
    let err = rejected
        .dispatch(&inv, &ExchangeState::default(), &target())
        .await
        .unwrap_err();
    assert_eq!(err.0.code, "ERR14013");
    assert_eq!(fake.requests(), 1);
    assert_eq!(fake.connections(), 1);
}

#[tokio::test]
async fn java_operation_caps_bound_calls_and_task_abort_does_not_redispatch() {
    for kind in 0..3 {
        let mut reply = Reply::ok();
        reply.delay = Duration::from_secs(2);
        let fake = Fake::start(vec![Reply::ok(), reply]).await;
        let cap = Some(Duration::from_millis(70));
        let options = match kind {
            0 => InvokeOptions {
                api_call_timeout: cap,
                ..Default::default()
            },
            1 => InvokeOptions {
                read_timeout: cap,
                ..Default::default()
            },
            _ => InvokeOptions {
                write_timeout: cap,
                ..Default::default()
            },
        };
        // Establish and independently count a successful connection before the
        // measured operation. Cold client scheduling is not a dispatch guarantee
        // within a 70 ms operation cap. No clock, cap, or assertion is relaxed.
        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(aws_smithy_http_client::tls::Provider::Rustls(
                aws_smithy_http_client::tls::rustls_provider::CryptoMode::AwsLc,
            ))
            .build_https();
        let warm = backend_with_http_client(
            &fake.endpoint,
            InvokeOptions {
                connection_timeout: cap,
                ..Default::default()
            },
            Limits::default(),
            Some(http.clone()),
        );
        let response = warm
            .dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
            .unwrap();
        assert_eq!(response.body.unwrap(), "ok");
        assert_eq!(fake.requests(), 1, "separately counted warm-up");
        let before = fake.requests();
        let b = backend_with_http_client(&fake.endpoint, options, Limits::default(), Some(http));
        let start = Instant::now();
        assert_eq!(
            b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
                .await
                .unwrap_err()
                .0
                .code,
            "ERR14017"
        );
        assert!(start.elapsed() < Duration::from_millis(300));
        assert_eq!(fake.requests() - before, 1);
        assert_eq!(
            fake.connections(),
            1,
            "the measured operation reuses the warm connection"
        );
    }
    let mut reply = Reply::ok();
    reply.delay = Duration::from_secs(2);
    let fake = Fake::start(vec![reply]).await;
    let b = fake.backend(
        InvokeOptions {
            max_retries: 2,
            ..Default::default()
        },
        Limits::default(),
    );
    let task = tokio::spawn(async move {
        b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
    });
    fake.state.started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(fake.requests(), 1);
    assert_eq!(fake.connections(), 1);
}

#[tokio::test]
async fn real_sdk_success_exact_payload_and_request_protocol() {
    let fake = Fake::start(vec![Reply::ok()]).await;
    let backend = fake.backend(InvokeOptions::default(), Limits::default());
    let inv = invocation(event());
    let state = ExchangeState::default();
    let expected = build_v1_event(&inv, &state, &cfg(), time()).unwrap();
    let r = backend.dispatch(&inv, &state, &target()).await.unwrap();
    assert_eq!(r.body.unwrap(), "ok");
    assert_eq!(fake.requests(), 1);
    assert_eq!(fake.connections(), 1);
    let calls = fake.state.received.lock().unwrap();
    assert_eq!(calls[0].method, "POST");
    assert!(calls[0].path.starts_with("/2015-03-31/functions/"));
    assert!(calls[0].path.ends_with("/invocations"));
    assert!(calls[0].path.contains("test-only-business"));
    assert_eq!(calls[0].headers["x-amz-invocation-type"], "RequestResponse");
    assert_eq!(calls[0].headers["x-amz-log-type"], "None");
    assert_eq!(
        serde_json::from_slice::<Value>(&calls[0].body).unwrap(),
        expected
    );
    assert_eq!(calls[0].body, serde_json::to_vec(&expected).unwrap());
}
#[tokio::test]
async fn function_errors_null_invalid_and_service_error_mappings_no_retry() {
    let mut handled = Reply::raw(200, br#"{"errorMessage":"test-only"}"#.to_vec());
    handled.error = Some("Handled");
    let mut unhandled = handled.clone();
    unhandled.error = Some("Unhandled");
    for (reply, code) in [
        (handled, "ERR14014"),
        (unhandled, "ERR14014"),
        (Reply::raw(200, b"null".to_vec()), "ERR14015"),
        (Reply::raw(200, vec![]), "ERR14015"),
        (Reply::raw(200, b"{bad".to_vec()), "ERR14015"),
        (Reply::error(404, "ResourceNotFoundException"), "ERR14019"),
        (Reply::error(403, "AccessDeniedException"), "ERR14019"),
        (Reply::error(500, "ServiceException"), "ERR14014"),
        (Reply::error(503, "ServiceException"), "ERR14014"),
    ] {
        let fake = Fake::start(vec![reply]).await;
        let b = fake.backend(
            InvokeOptions {
                max_retries: 2,
                ..Default::default()
            },
            Limits::default(),
        );
        let e = b
            .dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
            .unwrap_err();
        assert_eq!(e.0.code, code);
        assert_eq!(fake.requests(), 1, "{code}");
    }
}
#[tokio::test]
async fn throttle_retry_is_explicit_bounded_and_defaults_to_zero() {
    for code in [
        "TooManyRequestsException",
        "EC2ThrottledException",
        "ENILimitReachedException",
        "SubnetIPAddressLimitReachedException",
    ] {
        let fake = Fake::start(vec![Reply::error(429, code), Reply::ok()]).await;
        let b = fake.backend(
            InvokeOptions {
                max_retries: 1,
                ..Default::default()
            },
            Limits::default(),
        );
        assert!(
            b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
                .await
                .is_ok()
        );
        assert_eq!(fake.requests(), 2);
    }
    let fake = Fake::start(vec![Reply::error(429, "TooManyRequestsException")]).await;
    let b = fake.backend(InvokeOptions::default(), Limits::default());
    assert_eq!(
        b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
            .unwrap_err()
            .0
            .code,
        "ERR14018"
    );
    assert_eq!(fake.requests(), 1);
}
#[tokio::test]
async fn read_timeout_after_delivery_never_retries() {
    let mut reply = Reply::ok();
    reply.delay = Duration::from_secs(2);
    let fake = Fake::start(vec![reply]).await;
    let b = fake.backend(
        InvokeOptions {
            max_retries: 2,
            api_call_attempt_timeout: Some(Duration::from_millis(80)),
            ..Default::default()
        },
        Limits::default(),
    );
    let e = b
        .dispatch(&invocation(event()), &ExchangeState::default(), &target())
        .await
        .unwrap_err();
    assert_eq!(e.0.code, "ERR14017");
    assert_eq!(fake.requests(), 1);
}
#[tokio::test]
async fn exhausted_budget_cancellation_and_payload_cap_make_zero_connections() {
    let fake = Fake::start(vec![Reply::ok()]).await;
    let b = fake.backend(InvokeOptions::default(), Limits::default());
    let mut inv = invocation(event());
    inv.deadline = Instant::now();
    assert_eq!(
        b.dispatch(&inv, &ExchangeState::default(), &target())
            .await
            .unwrap_err()
            .0
            .code,
        "ERR14017"
    );
    let inv = invocation(event());
    inv.cancel.cancel();
    assert_eq!(
        b.dispatch(&inv, &ExchangeState::default(), &target())
            .await
            .unwrap_err()
            .0
            .code,
        "ERR14017"
    );
    let b = fake.backend(
        InvokeOptions::default(),
        Limits {
            max_invoke_payload_bytes: 32,
            ..Limits::default()
        },
    );
    assert_eq!(
        b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
            .unwrap_err()
            .0
            .code,
        "ERR14013"
    );
    assert_eq!(fake.requests(), 0);
    assert_eq!(fake.connections(), 0);
}
#[tokio::test]
async fn cancellation_after_received_request_stops_wait_without_redispatch() {
    let mut reply = Reply::ok();
    reply.delay = Duration::from_secs(2);
    let fake = Fake::start(vec![reply]).await;
    let b = fake.backend(
        InvokeOptions {
            max_retries: 3,
            ..Default::default()
        },
        Limits::default(),
    );
    let inv = invocation(event());
    let cancel = inv.cancel.clone();
    let state = fake.state.clone();
    let task = tokio::spawn(async move {
        state.starts.fetch_add(1, Ordering::SeqCst);
        b.dispatch(&inv, &ExchangeState::default(), &target()).await
    });
    fake.state.started.notified().await;
    cancel.cancel();
    let r = tokio::time::timeout(Duration::from_millis(200), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(r.0.code, "ERR14017");
    assert_eq!(fake.state.starts.load(Ordering::SeqCst), 1);
    assert_eq!(fake.connections(), 1);
    assert_eq!(fake.requests(), 1);
}
#[tokio::test]
async fn remaining_absolute_budget_and_backoff_cannot_reset_deadline() {
    let mut reply = Reply::ok();
    reply.delay = Duration::from_millis(300);
    let fake = Fake::start(vec![reply]).await;
    let b = fake.backend(InvokeOptions::default(), Limits::default());
    let mut inv = invocation(event());
    inv.deadline = Instant::now() + Duration::from_millis(180);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start = Instant::now();
    let e = b
        .dispatch(&inv, &ExchangeState::default(), &target())
        .await
        .unwrap_err();
    assert_eq!(e.0.code, "ERR14017");
    assert!(start.elapsed() < Duration::from_millis(160));
    assert_eq!(fake.requests(), 1);
    let fake = Fake::start(vec![Reply::error(429, "TooManyRequestsException")]).await;
    let b = fake.backend(
        InvokeOptions {
            max_retries: 10,
            ..Default::default()
        },
        Limits::default(),
    );
    let mut inv = invocation(event());
    inv.deadline = Instant::now() + Duration::from_millis(80);
    let start = Instant::now();
    let _ = b.dispatch(&inv, &ExchangeState::default(), &target()).await;
    assert!(start.elapsed() < Duration::from_millis(250));
    assert!(fake.requests() < 11);
}
#[tokio::test]
async fn caller_cannot_choose_target_endpoint_or_spoof_authorizer() {
    let fake = Fake::start(vec![Reply::ok()]).await;
    let b = fake.backend(InvokeOptions::default(), Limits::default());
    let mut e = event();
    e["functionName"] = json!("attacker");
    e["endpointOverride"] = json!("http://attacker.test");
    e["multiValueHeaders"] = json!({"x-amzn-oidc-identity":["attacker"],"authorizer":["attacker"]});
    let inv = invocation(e);
    b.dispatch(&inv, &ExchangeState::default(), &target())
        .await
        .unwrap();
    {
        let calls = fake.state.received.lock().unwrap();
        let v: Value = serde_json::from_slice(&calls[0].body).unwrap();
        assert!(v["requestContext"].get("authorizer").is_none());
        assert!(calls[0].path.contains("test-only-business"));
    }
    assert_eq!(
        b.dispatch(
            &inv,
            &ExchangeState::default(),
            &Target::Function("attacker".into())
        )
        .await
        .unwrap_err()
        .0
        .code,
        "ERR10086"
    );
    assert_eq!(fake.requests(), 1);
}
#[tokio::test]
async fn ambiguous_socket_close_after_full_request_is_not_retried() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count2 = count.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            count2.fetch_add(1, Ordering::SeqCst);
            let mut buf = vec![];
            let mut byte = [0];
            while !buf.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                buf.push(byte[0]);
            }
            let headers = String::from_utf8(buf).unwrap();
            let n = headers
                .lines()
                .find_map(|s| {
                    s.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|s| s.trim().parse::<usize>().ok())
                })
                .unwrap();
            let mut body = vec![0; n];
            socket.read_exact(&mut body).await.unwrap();
            socket.shutdown().await.unwrap();
        }
    });
    let b = backend(
        &endpoint,
        InvokeOptions {
            max_retries: 3,
            ..Default::default()
        },
        Limits::default(),
    );
    assert_eq!(
        b.dispatch(&invocation(event()), &ExchangeState::default(), &target())
            .await
            .unwrap_err()
            .0
            .code,
        "ERR14014"
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    server.abort();
}
