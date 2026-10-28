//! The pinned SDK owns invocation polling, handler execution and response/error
//! posting. Only pre-run Init reporting is framework-owned.
use crate::{
    init::{InitInput, Snapshot, initialize},
    init_error::{self, InitError},
};
use lambda_runtime::{Error, LambdaEvent, Runtime, service_fn};
use serde_json::Value;
use std::sync::Arc;
pub async fn run(snapshot: Arc<Snapshot>) -> Result<(), Error> {
    run_with_guard(snapshot, None).await
}
pub async fn run_with_guard(
    snapshot: Arc<Snapshot>,
    guard: Option<crate::service::RequestGuard>,
) -> Result<(), Error> {
    let runtime = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let authority = std::env::var("AWS_LAMBDA_RUNTIME_API").map_err(|_| {
            InitError::InvalidConfiguration("runtime: missing AWS_LAMBDA_RUNTIME_API")
        })?;
        init_error::endpoint(&authority, "/")?;
        crate::init::Environment::from_env()?;
        let handler = service_fn(move |event: LambdaEvent<Value>| {
            let snapshot = snapshot.clone();
            let guard = guard.clone();
            async move {
                // Fixed subprocess-only injection; absent from production builds.
                #[cfg(test)]
                if std::env::var("WP9_RUNTIME_PROBE").as_deref() == Ok("internal") {
                    return Err::<Value, Error>("internal: test-only invariant failure".into());
                }
                crate::service::handle_with_guard(snapshot, event, guard.as_ref()).await
            }
        });
        Ok::<_, InitError>(Runtime::new(handler))
    }))
    .unwrap_or(Err(InitError::Panic));
    let runtime = match runtime {
        Ok(runtime) => runtime,
        Err(error) => init_error::report_init_failure(&error).await,
    };
    runtime.run().await
}
/// Process entry for WP10's future app. Pre-run failures cannot enter polling.
/// Input preparation runs inside the redacted panic/error boundary, including
/// environment parsing. The application must not prepare inputs before this call.
pub async fn run_initialized<F>(prepare: F) -> Result<(), Error>
where
    F: FnOnce() -> Result<InitInput, InitError>,
{
    run_initialized_with_guard(prepare, None).await
}
pub async fn run_initialized_with_guard<F>(
    prepare: F,
    guard: Option<crate::service::RequestGuard>,
) -> Result<(), Error>
where
    F: FnOnce() -> Result<InitInput, InitError>,
{
    crate::telemetry::install_panic_redaction();
    if let Err(error) = crate::telemetry::initialize() {
        init_error::report_init_failure(&error).await;
    }
    let input = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(prepare))
        .unwrap_or(Err(InitError::Panic))
    {
        Ok(input) => input,
        Err(error) => init_error::report_init_failure(&error).await,
    };
    let snapshot = match initialize(input).await {
        Ok(s) => s,
        Err(e) => init_error::report_init_failure(&e).await,
    };
    run_with_guard(snapshot, guard).await.map_err(|_| {
        tracing::error!(target:"light_lambda::runtime",message="runtime transport terminated");
        Error::from("runtime transport terminated")
    })
}
/// Allows a thin future app to route environment/input preparation failures to
/// the same single exit path without embedding a production binary in WP9.
pub async fn fail(error: InitError) -> ! {
    init_error::report_init_failure(&error).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Command, time::Duration};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[tokio::test]
    async fn init_child() {
        let Ok(mode) = std::env::var("WP9_INIT_PROBE") else {
            return;
        };
        static FILES: &[config_loader::EmbeddedConfigFile] = &[
            config_loader::EmbeddedConfigFile {
                name: "server.yml",
                content: "serviceId: test-only\nenableRegistry: false\nip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\n",
            },
            config_loader::EmbeddedConfigFile {
                name: "light-lambda.yml",
                content: "stage: ''\n",
            },
        ];
        run_initialized(|| {
            if mode == "input-panic" {
                panic!("test-only-init-secret-marker");
            }
            if mode == "input-error" {
                return Err(InitError::InvalidConfiguration(
                    "environment: missing Lambda identity",
                ));
            }
            let mut input = crate::init::InitInput::from_env(
                FILES,
                serverless_aws::target::GatewayIdentity {
                    function_name: "test-only-gateway".into(),
                    region: "us-east-1".into(),
                    account_id: "123456789012".into(),
                },
            )?;
            input.panic_during_preparation = mode == "panic";
            Ok(input)
        })
        .await
        .unwrap();
        panic!("unexpected success");
    }
    #[tokio::test]
    async fn init_failures_and_panic_exit_once_without_polling_or_secret_output() {
        for mode in [
            "accepted",
            "rejected",
            "hang",
            "unset",
            "refused",
            "panic",
            "invalid",
            "input-panic",
            "input-error",
        ] {
            let dir = tempfile::tempdir().unwrap();
            if mode == "invalid" {
                std::fs::write(
                    dir.path().join("light-lambda.yml"),
                    "stage: test\nmaxRequestBytes: test-only-init-secret-marker\n",
                )
                .unwrap();
            }
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = if ["unset", "refused"].contains(&mode) {
                drop(listener);
                None
            } else {
                Some(tokio::spawn(async move {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 2048];
                    loop {
                        let n = stream.read(&mut buffer).await.unwrap();
                        assert!(n > 0);
                        bytes.extend_from_slice(&buffer[..n]);
                        if let Some(end) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let length = String::from_utf8_lossy(&bytes[..end])
                                .lines()
                                .find_map(|s| {
                                    s.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|v| v.parse::<usize>().ok())
                                })
                                .unwrap();
                            if bytes.len() >= end + 4 + length {
                                break;
                            }
                        }
                    }
                    if mode == "hang" {
                        tokio::time::sleep(Duration::from_millis(2200)).await;
                    } else {
                        let status = if mode == "rejected" { 503 } else { 202 };
                        stream.write_all(format!("HTTP/1.1 {status} Test\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").as_bytes()).await.unwrap();
                    }
                    assert!(
                        tokio::time::timeout(Duration::from_millis(100), listener.accept())
                            .await
                            .is_err()
                    );
                    bytes
                }))
            };
            let path = dir.path().to_owned();
            let start = std::time::Instant::now();
            let child = tokio::task::spawn_blocking(move || {
                let mut cmd = Command::new(std::env::current_exe().unwrap());
                cmd.args(["--exact", "runtime::tests::init_child", "--nocapture"])
                    .env_clear()
                    .env("WP9_INIT_PROBE", mode)
                    .env("RUST_LOG", "trace")
                    .env("LIGHT_LAMBDA_CONFIG_DIR", path)
                    .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
                    .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
                    .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128");
                if mode != "unset" {
                    cmd.env("AWS_LAMBDA_RUNTIME_API", address.to_string());
                }
                cmd.output().unwrap()
            });
            let output = tokio::time::timeout(Duration::from_secs(4), child)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(output.status.code(), Some(1));
            assert!(start.elapsed() < Duration::from_secs(3));
            for data in [&output.stdout, &output.stderr] {
                assert!(!String::from_utf8_lossy(data).contains("test-only-init-secret-marker"));
            }
            assert!(output.stderr.is_empty());
            if let Some(server) = server {
                let bytes = server.await.unwrap();
                let text = String::from_utf8(bytes).unwrap();
                assert!(text.starts_with("POST /2018-06-01/runtime/init/error HTTP/1.1"));
                assert!(!text.contains("test-only-init-secret-marker"));
                let payload = text.split("\r\n\r\n").nth(1).unwrap();
                assert!(payload.len() <= 1024);
                let payload: Value = serde_json::from_str(payload).unwrap();
                if mode == "panic" || mode == "input-panic" {
                    assert_eq!(payload["errorType"], "Runtime.InitPanic");
                    assert!(
                        text.to_ascii_lowercase()
                            .contains("lambda-runtime-function-error-type: runtime.initpanic")
                    );
                } else if mode == "input-error" {
                    assert_eq!(
                        payload["errorMessage"],
                        "environment: missing Lambda identity"
                    );
                } else if mode != "invalid" {
                    assert_eq!(
                        payload["errorMessage"],
                        "configuration: missing light-lambda.stage"
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn runtime_child() {
        let Ok(mode) = std::env::var("WP9_RUNTIME_PROBE") else {
            return;
        };
        crate::telemetry::initialize().unwrap();
        let (snapshot, calls, finalized) = super::protocol::snapshot(&mode);
        let result = run(snapshot).await;
        tracing::info!(target:"light_lambda::runtime_test", dispatches=calls.load(std::sync::atomic::Ordering::SeqCst), finalizations=finalized.load(std::sync::atomic::Ordering::SeqCst), message="test-only completed counts");
        assert!(result.is_err());
    }
    #[tokio::test]
    async fn real_sdk_polling_and_framework_posting_are_strict_and_payload_safe() {
        super::protocol::matrix().await;
    }
    #[tokio::test]
    async fn sdk_response_http_500_resumes_polling_without_redispatch() {
        super::protocol::http500_continues().await;
    }
}

#[cfg(test)]
#[path = "runtime_protocol.rs"]
mod protocol;
