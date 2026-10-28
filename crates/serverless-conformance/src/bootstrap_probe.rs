//! Actual built application Init-error reporting against an owned Runtime API.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, process::Command};

pub(crate) fn bootstrap() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LIGHT_LAMBDA_CONFORMANCE_BOOTSTRAP") {
        return Ok(path.into());
    }
    let executable = std::env::current_exe()?;
    let parent = executable.parent().context("runner executable directory")?;
    let target = if parent.file_name().is_some_and(|s| s == "deps") {
        parent.parent().context("target directory")?
    } else {
        parent
    };
    Ok(target.join("bootstrap"))
}
pub(crate) async fn report(f: &Value, directory: &Path) -> Result<Value> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let records = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn receive(
        State(records): State<Arc<Mutex<Vec<Value>>>>,
        request: Request,
    ) -> http::StatusCode {
        let (parts, body) = request.into_parts();
        let bytes = match to_bytes(body, 4096).await {
            Ok(bytes) => bytes,
            Err(_) => return http::StatusCode::PAYLOAD_TOO_LARGE,
        };
        records.lock().unwrap().push(json!({"path":parts.uri.path(),"method":parts.method.as_str(),"header":parts.headers.get("lambda-runtime-function-error-type").and_then(|v| v.to_str().ok()),"body":String::from_utf8_lossy(&bytes)}));
        http::StatusCode::ACCEPTED
    }
    let task = tokio::spawn(
        axum::serve(
            listener,
            Router::new().fallback(receive).with_state(records.clone()),
        )
        .into_future(),
    );
    let name = f["runtimeIdentity"]["functionName"]
        .as_str()
        .or_else(|| f["context"]["functionName"].as_str())
        .unwrap_or("offline-validator");
    let mut command = Command::new(bootstrap()?);
    command
        .env_clear()
        .current_dir(directory)
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_CONFIG_FILE", directory.join("no-aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            directory.join("no-aws-credentials"),
        )
        .env("AWS_LAMBDA_FUNCTION_NAME", name)
        .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
        .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
        .env(
            "LIGHT_LAMBDA_GATEWAY_ARN",
            format!("arn:aws:lambda:us-east-1:000000000000:function:{name}"),
        )
        .env("LIGHT_LAMBDA_CONFIG_DIR", directory)
        .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
        .env("AWS_LAMBDA_RUNTIME_API", address.to_string());
    if f["insecureTestConfigPrerequisite"] == "allow-v1" {
        command.env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG", "1");
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            task.abort();
            return Err(error)
                .context("build light-lambda bootstrap before running reporting fixtures");
        }
    };
    let status = tokio::time::timeout(Duration::from_secs(6), child.wait()).await;
    task.abort();
    let status = status.context("bounded actual bootstrap Init exit")??;
    let records = records.lock().unwrap().clone();
    let reports: Vec<_> = records
        .iter()
        .filter(|r| r["path"] == "/2018-06-01/runtime/init/error")
        .collect();
    let report = reports
        .first()
        .context("actual bootstrap did not report Init error")?;
    ensure!(report["method"] == "POST", "Init report method");
    let mut observed =
        json!({"endpoint":report["path"],"reportCount":reports.len(),"exitCode":status.code()});
    if f["expect"]["initFailureReporting"].get("body").is_some() {
        let body = report["body"].as_str().context("actual Init body")?;
        ensure!(body.len() <= 1024, "Init body exceeds bound");
        for forbidden in ["test-only-secret-sentinel", "test-only-password-sentinel"] {
            ensure!(
                !body.contains(forbidden),
                "Init body leaked forbidden value"
            );
        }
        let parsed: Value = serde_json::from_str(body)?;
        observed["header"] = json!({"Lambda-Runtime-Function-Error-Type":report["header"]});
        observed["body"] =
            json!({"errorType":parsed["errorType"],"errorMessage":parsed["errorMessage"]});
        observed["maxBodyBytes"] = json!(1024);
        observed["forbiddenBodyValues"] =
            json!(["test-only-secret-sentinel", "test-only-password-sentinel"]);
        observed["invocationLoopEntered"] = json!(
            records
                .iter()
                .any(|r| r["path"] == "/2018-06-01/runtime/invocation/next")
        );
    }
    Ok(observed)
}
