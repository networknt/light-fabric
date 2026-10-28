use light_lambda_runtime::init_error::{InitError, ReportOutcome, report};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn reporting_transport_matrix_is_single_attempt_bounded_and_redacted() {
    let error = InitError::InvalidConfiguration("configuration: missing light-lambda.stage");
    assert_eq!(report(&error, None).await, ReportOutcome::MissingEndpoint);
    assert_eq!(
        report(&error, Some("http://invalid/secret")).await,
        ReportOutcome::InvalidEndpoint
    );
    for (status, expected) in [
        (202, ReportOutcome::Accepted),
        (503, ReportOutcome::Rejected),
        (0, ReportOutcome::Timeout),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let (a, p, c) = (accepts.clone(), posts.clone(), captured.clone());
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                a.fetch_add(1, Ordering::SeqCst);
                let p = p.clone();
                let c = c.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 2048];
                    loop {
                        let n = stream.read(&mut buf).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                        if let Some(end) = request.windows(4).position(|s| s == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&request[..end]);
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|v| v.parse::<usize>().ok())
                                })
                                .unwrap();
                            if request.len() >= end + 4 + len {
                                break;
                            }
                        }
                    }
                    p.fetch_add(1, Ordering::SeqCst);
                    *c.lock().unwrap() = request;
                    if status == 0 {
                        tokio::time::sleep(Duration::from_secs(4)).await;
                    } else {
                        stream.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
                    }
                });
            }
        });
        let start = Instant::now();
        assert_eq!(report(&error, Some(&addr.to_string())).await, expected);
        assert!(start.elapsed() < Duration::from_secs(3));
        if status == 0 {
            assert!(start.elapsed() >= Duration::from_millis(1900));
        }
        assert_eq!(accepts.load(Ordering::SeqCst), 1);
        assert_eq!(posts.load(Ordering::SeqCst), 1);
        let bytes = captured.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with("POST /2018-06-01/runtime/init/error HTTP/1.1"));
        assert!(
            text.to_ascii_lowercase()
                .contains("lambda-runtime-function-error-type: runtime.initerror")
        );
        let body = text.split("\r\n\r\n").nth(1).unwrap();
        assert!(body.len() <= 1024);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap()["errorMessage"],
            error.message()
        );
        server.abort();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    assert_eq!(
        report(&error, Some(&addr.to_string())).await,
        ReportOutcome::TransportFailure
    );
}

#[test]
fn reporting_payload_is_bounded_even_after_json_escaping() {
    let value = InitError::InvalidConfiguration("configuration: missing light-lambda.stage");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&value.payload()).unwrap()["errorType"],
        "InvalidConfiguration"
    );
    assert_eq!(InitError::Panic.error_type(), "Runtime.InitPanic");
    // A static diagnostic supplied by framework integration is still envelope bounded.
    let long = Box::leak("\"\\\n".repeat(2000).into_boxed_str());
    let payload = InitError::InvalidConfiguration(long).payload();
    assert!(payload.len() <= 1024);
    assert!(serde_json::from_slice::<serde_json::Value>(&payload).is_ok());
}
