//! Dependency characterization, NOT acceptance of unsafe SDK behavior and NOT
//! framework/L1 qualification. The production framework must close both gaps.
use std::{process::Command, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[tokio::test]
async fn sdk_child() {
    if std::env::var("WP9_SDK_PROBE").as_deref() != Ok("1") {
        return;
    }
    tracing_subscriber::fmt()
        .json()
        .with_max_level(tracing::Level::TRACE)
        .init();
    let result = lambda_runtime::run(lambda_runtime::service_fn(
        |_: lambda_runtime::LambdaEvent<serde_json::Value>| async {
            Ok::<_, lambda_runtime::Error>(serde_json::json!({"statusCode":200,"body":"ok"}))
        },
    ))
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn pinned_sdk_continues_after_http_500_and_exposes_raw_event_at_trace() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut paths = Vec::new();
        for index in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0; 2048];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|s| s == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&data[..end]);
                    let length = headers
                        .lines()
                        .find_map(|s| {
                            s.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if data.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            paths.push(
                String::from_utf8_lossy(&data)
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned(),
            );
            let (status, body, headers) = if index == 0 {
                (
                    200,
                    "{\"private\":\"test-only-WP9-raw-body-marker\"}",
                    "lambda-runtime-aws-request-id: test-only-request\r\nlambda-runtime-deadline-ms: 9999999999999\r\nlambda-runtime-invoked-function-arn: arn:aws:lambda:us-east-1:123456789012:function:test-only-gateway\r\n",
                )
            } else if index == 2 {
                (
                    500,
                    "",
                    "lambda-runtime-aws-request-id: test-only-stop\r\nlambda-runtime-deadline-ms: 9999999999999\r\n",
                )
            } else {
                (500, "", "")
            };
            stream.write_all(format!("HTTP/1.1 {status} Test\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        paths
    });
    let child = tokio::task::spawn_blocking(move || {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "sdk_child", "--nocapture"])
            .env_clear()
            .env("WP9_SDK_PROBE", "1")
            .env("AWS_LAMBDA_RUNTIME_API", addr.to_string())
            .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
            .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
            .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
            .output()
            .unwrap()
    });
    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    let paths = server.await.unwrap();
    assert_eq!(paths[0], "GET /2018-06-01/runtime/invocation/next HTTP/1.1");
    assert_eq!(
        paths[1],
        "POST /2018-06-01/runtime/invocation/test-only-request/response HTTP/1.1"
    );
    assert_eq!(
        paths[2], paths[0],
        "SDK polled again after response HTTP 500"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("test-only-WP9-raw-body-marker"));
}
