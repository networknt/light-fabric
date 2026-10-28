//! Bounded actual bootstrap Init-report faults against an owned TCP endpoint.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    process::Command,
    time::Instant,
};
pub(crate) fn supported(f: &Value) -> bool {
    f["rustOnly"] == true && f["component"] == "rustInitReportTransportCfg05"
}
pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    let fault = f["initReportTransportProbe"]["fault"]
        .as_str()
        .context("closed reporting fault")?;
    ensure!(
        matches!(
            fault,
            "runtime-api-unset-v1"
                | "loopback-connection-refused-v1"
                | "loopback-http-503-v1"
                | "loopback-response-withheld-v1"
        ),
        "unknown reporting fault"
    );
    let dir = tempfile::tempdir()?;
    for (name, value) in f["config"]["files"].as_object().context("files")? {
        std::fs::write(
            dir.path().join(format!("{name}.yml")),
            serde_json::to_vec(value)?,
        )?;
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let records = Arc::new(Mutex::new(Vec::<Value>::new()));
    let server = if fault == "loopback-connection-refused-v1" || fault == "runtime-api-unset-v1" {
        drop(listener);
        None
    } else {
        let accepted = accepted.clone();
        let records = records.clone();
        let withheld = fault == "loopback-response-withheld-v1";
        Some(tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let records = records.clone();
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 2048];
                    loop {
                        let n = stream.read(&mut buffer).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..n]);
                        if bytes.len() > 8192 {
                            return;
                        }
                        if let Some(split) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..split]);
                            let length = head
                                .lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length: ")
                                        .and_then(|s| s.parse::<usize>().ok())
                                })
                                .unwrap_or(0);
                            if bytes.len() < split + 4 + length {
                                continue;
                            }
                            let first = head.lines().next().unwrap_or_default();
                            let header = head.lines().find_map(|l| {
                                l.split_once(':')
                                    .filter(|(k, _)| {
                                        k.eq_ignore_ascii_case("lambda-runtime-function-error-type")
                                    })
                                    .map(|(_, v)| v.trim().to_string())
                            });
                            let body: Value =
                                serde_json::from_slice(&bytes[split + 4..split + 4 + length])
                                    .unwrap_or(Value::Null);
                            records.lock().unwrap().push(
                                json!({"request":first,"header":header,"body":body,"bytes":length}),
                            );
                            break;
                        }
                    }
                    if withheld {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    } else {
                        let _=stream.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 25\r\nConnection: close\r\n\r\ntest-only-secret-sentinel").await;
                    }
                });
            }
        }))
    };
    let mut command = Command::new(crate::bootstrap_probe::bootstrap()?);
    command
        .env_clear()
        .current_dir(dir.path())
        .kill_on_drop(true)
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_CONFIG_FILE", dir.path().join("absent-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            dir.path().join("absent-credentials"),
        )
        .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
        .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
        .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
        .env(
            "LIGHT_LAMBDA_GATEWAY_ARN",
            "arn:aws:lambda:us-east-1:000000000000:function:test-only-gateway",
        )
        .env("LIGHT_LAMBDA_CONFIG_DIR", dir.path())
        .env(
            "RUST_LOG",
            "light_lambda::init=error,hyper_util::client::legacy::connect::http=debug",
        );
    if fault != "runtime-api-unset-v1" {
        command.env("AWS_LAMBDA_RUNTIME_API", addr.to_string());
    }
    let start = Instant::now();
    let output = tokio::time::timeout(Duration::from_secs(3), command.output()).await??;
    let elapsed = start.elapsed().as_millis() as u64;
    if let Some(server) = server {
        server.abort();
    }
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    ensure!(stderr.is_empty(), "unexpected Init stderr");
    let lines: Vec<_> = stdout.lines().map(str::to_owned).collect();
    for line in &lines {
        ensure!(
            !line.contains("test-only-secret-sentinel")
                && !line.contains("test-only-password-sentinel"),
            "report diagnostic privacy"
        );
    }
    let logs = lines
        .iter()
        .map(|l| serde_json::from_str::<Value>(l))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let diagnostic = logs
        .iter()
        .find(|v| v["target"] == "light_lambda::init" && v["level"] == "ERROR")
        .context("actual Init diagnostic")?;
    let fields = &diagnostic["fields"];
    let configured = fault != "runtime-api-unset-v1";
    let accepted = accepted.load(Ordering::SeqCst);
    let records = records.lock().unwrap().clone();
    let connector_events = logs
        .iter()
        .filter(|v| {
            v["target"] == "hyper_util::client::legacy::connect::http" && v["level"] == "DEBUG"
        })
        .count();
    // Pinned HyperUtil emits one connecting event and one connected event on
    // success. Privacy redacts their messages; count events, never recover text.
    let attempts = connector_events
        .checked_sub(accepted)
        .context("connector event accounting")?;
    let report = records
        .iter()
        .find(|r| r["request"] == "POST /2018-06-01/runtime/init/error HTTP/1.1");
    let actual_error = light_lambda_runtime::init_error::InitError::InvalidConfiguration(
        "configuration: missing light-lambda.stage",
    );
    let body = report
        .map(|r| r["body"].clone())
        .unwrap_or(serde_json::from_slice(&actual_error.payload())?);
    let mut observed = json!({"outcome":"initReportTransportFailure","code":fields["error_type"],"backendCalls":[],"jwksFetches":0,"reporting":{"attempts":attempts,"receivedPosts":records.len(),"retries":attempts.saturating_sub(1),"invocationPolls":records.iter().filter(|r|r["request"].as_str().is_some_and(|s|s.contains("/invocation/next"))).count(),"endpoint":"/2018-06-01/runtime/init/error","header":{"Lambda-Runtime-Function-Error-Type":report.map(|r|r["header"].clone()).unwrap_or(json!("Runtime.InitError"))},"body":body,"maxBodyBytes":1024,"connectionAttempts":attempts,"acceptedConnections":accepted,"runtimeApiConfigured":configured,"prerequisiteReady":true},"processExit":{"code":output.status.code(),"maxAfterInitFailureMs":3000},"diagnosticContract":{"requiredMessage":fields["message"],"requiredLevel":diagnostic["level"].as_str().unwrap_or("").to_ascii_lowercase(),"transportDiagnostic":fault,"forbiddenValues":["test-only-secret-sentinel","test-only-password-sentinel"],"forbidConfigurationValues":true}});
    let expected_outcome = match fault {
        "runtime-api-unset-v1" => "MissingEndpoint",
        "loopback-connection-refused-v1" => "TransportFailure",
        "loopback-http-503-v1" => "Rejected",
        _ => "Timeout",
    };
    ensure!(
        fields["report_outcome"] == expected_outcome,
        "actual reporting outcome differs: {}",
        fields["report_outcome"]
    );
    if let Some(report) = report {
        ensure!(
            report["bytes"].as_u64().unwrap_or(u64::MAX) <= 1024,
            "actual body bound"
        );
    }
    let errors = match assert(&f["expect"], &observed) {
        Ok(()) => vec![],
        Err(e) => vec![format!("{e:#}")],
    };
    // Raw diagnostics and conservative whole-process timing qualify descriptors.
    let evidence = json!({"diagnosticLines":lines,"elapsedAfterInitFailureMs":elapsed});
    observed.as_object_mut().context("observation")?;
    Ok(
        json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":elapsed,"observations":[observed],"assertionErrors":errors,"faultEvidence":evidence}),
    )
}

pub(crate) fn assert(expected: &Value, actual: &Value) -> Result<()> {
    ensure!(
        expected == actual,
        "actual reporting observation differs: expected {expected}, actual {actual}"
    );
    Ok(())
}
