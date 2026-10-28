//! Existing runtime unit-worker hook exercises a real preparation panic.
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::to_bytes,
    extract::{Request, State},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, process::Command, time::Instant};
pub(crate) fn supported(f: &Value) -> bool {
    f["rustOnly"] == true && f["id"] == "CFG-05-prepare-local-config-panic-init-contract-v1"
}
async fn binary() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let parent = exe.parent().context("runner")?;
    let deps = if parent.file_name().is_some_and(|n| n == "deps") {
        parent.to_path_buf()
    } else {
        parent.join("deps")
    };
    let mut paths = std::fs::read_dir(deps)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_none()
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("light_lambda_runtime-"))
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    for p in paths {
        let o = Command::new(&p).arg("--list").output().await?;
        if String::from_utf8_lossy(&o.stdout).contains("runtime::tests::init_child:") {
            return Ok(p);
        }
    }
    anyhow::bail!(
        "build runtime unit worker with cargo test -p light-lambda-runtime --lib --no-run"
    )
}
pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    let start = Instant::now();
    let records = Arc::new(Mutex::new(Vec::<Value>::new()));
    async fn receive(
        State(records): State<Arc<Mutex<Vec<Value>>>>,
        request: Request,
    ) -> http::StatusCode {
        let (parts, body) = request.into_parts();
        let bytes = to_bytes(body, 4096).await.unwrap();
        records.lock().unwrap().push(json!({"path":parts.uri.path(),"header":parts.headers.get("lambda-runtime-function-error-type").and_then(|v|v.to_str().ok()),"body":serde_json::from_slice::<Value>(&bytes).unwrap(),"bytes":bytes.len()}));
        http::StatusCode::ACCEPTED
    }
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let task = tokio::spawn(
        axum::serve(
            listener,
            Router::new().fallback(receive).with_state(records.clone()),
        )
        .into_future(),
    );
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        Command::new(binary().await?)
            .args(["--exact", "runtime::tests::init_child", "--nocapture"])
            .env_clear()
            .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
            .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
            .env("AWS_REGION", "us-east-1")
            .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
            .env(
                "LIGHT_LAMBDA_GATEWAY_ARN",
                "arn:aws:lambda:us-east-1:123456789012:function:test-only-gateway",
            )
            .env("WP9_INIT_PROBE", "panic")
            .env("AWS_LAMBDA_RUNTIME_API", address.to_string())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    task.abort();
    let raw = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        !raw.contains("test-only-init-secret-marker")
            && !raw.contains("test-only-secret-sentinel")
            && !raw.contains("test-only-password-sentinel"),
        "panic privacy"
    );
    let records = records.lock().unwrap().clone();
    let report = records
        .iter()
        .find(|r| r["path"] == "/2018-06-01/runtime/init/error")
        .context("actual panic report")?;
    ensure!(
        report["bytes"].as_u64().unwrap() <= 1024,
        "panic body bound"
    );
    let observed = json!({"outcome":"initError","code":report["body"]["errorType"],"backendCalls":[],"jwksFetches":0,"initFailureReporting":{"endpoint":report["path"],"reportCount":records.len(),"exitCode":output.status.code(),"header":{"Lambda-Runtime-Function-Error-Type":report["header"]},"body":report["body"],"maxBodyBytes":1024,"forbiddenBodyValues":["test-only-secret-sentinel","test-only-password-sentinel"],"invocationLoopEntered":records.iter().any(|r|r["path"]=="/2018-06-01/runtime/invocation/next")}});
    let errors = if observed == f["expect"] {
        vec![]
    } else {
        vec![format!(
            "actual preparation panic differs: expected {}, actual {observed}",
            f["expect"]
        )]
    };
    Ok(
        json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":start.elapsed().as_millis() as u64,"observations":[observed],"assertionErrors":errors}),
    )
}
