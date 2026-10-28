mod support;
use serde_json::json;
use serverless_aws::{invoke::InvokeOptions, limits::Limits};
use serverless_core::{Backend, ExchangeState, Target};
use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};
use support::*;
#[derive(Clone)]
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
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
#[tokio::test]
async fn all_targets_trace_never_exposes_payload_and_retains_approved_metadata() {
    const SECRET: &str = "SECRET-MARKER-7f3a9c";
    let capture = Capture(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    for error in [Some("Handled"), Some("Unhandled"), None] {
        let payload = if error.is_some() {
            serde_json::to_vec(&json!({"errorMessage":SECRET,"stackTrace":[SECRET],"unexpected":SECRET,"errorType":format!("{SECRET}/disallowed")})).unwrap()
        } else {
            format!("{{unparseable {SECRET}").into_bytes()
        };
        let mut reply = Reply::raw(200, payload);
        reply.error = error;
        reply.log_result = Some(SECRET.into());
        let fake = Fake::start(vec![reply]).await;
        let b = fake.backend(InvokeOptions::default(), Limits::default());
        let mut e = event();
        e["body"] = json!(SECRET);
        let response = b
            .dispatch(
                &invocation(e),
                &ExchangeState::default(),
                &Target::Function("FnX".into()),
            )
            .await
            .unwrap_err()
            .0
            .response();
        assert!(!String::from_utf8_lossy(response.body.as_ref().unwrap()).contains(SECRET));
        assert_eq!(fake.requests(), 1);
    }
    // A closed numeric-loopback port is a connect-stage refusal, not a read
    // failure after delivery. Count SDK attempt starts separately from calls.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let offset = capture.0.lock().unwrap().len();
    let b = backend(
        &endpoint,
        InvokeOptions {
            max_retries: 1,
            ..Default::default()
        },
        Limits::default(),
    );
    let error = b
        .dispatch(
            &invocation(event()),
            &ExchangeState::default(),
            &Target::Function("FnX".into()),
        )
        .await
        .unwrap_err();
    assert_eq!(error.0.code, "ERR14014");
    let refusal = String::from_utf8(capture.0.lock().unwrap()[offset..].to_vec()).unwrap();
    assert_eq!(refusal.matches("beginning attempt #").count(), 2);
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(!logs.contains(SECRET), "sensitive payload in TRACE output");
    for permitted in [
        "test-only-request",
        "FnX",
        "Handled",
        "Unhandled",
        "payloadBytes",
        "<redacted>",
    ] {
        assert!(logs.contains(permitted), "missing metadata {permitted}");
    }
    assert!(
        logs.contains("aws_smithy"),
        "SDK TRACE must actually be captured"
    );
    assert!(!logs.contains("test-only-secret"));
    assert!(
        !logs.contains("test-only-access"),
        "SDK access-ID leak locations: {:?}",
        logs.lines()
            .filter(|s| s.contains("test-only-access"))
            .map(|s| s.replace("test-only-access", "<redacted-access-id>"))
            .collect::<Vec<_>>()
    );
    assert!(!logs.contains("test-only-session"));
}
