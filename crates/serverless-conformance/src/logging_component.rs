//! Process-owned production telemetry initialization and literal probe events.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
fn epoch() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64)
}
pub(crate) fn supported(f: &Value) -> bool {
    f["rustOnly"] == true && f["component"] == "rustInitLoggingCfg01"
}
pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    crate::init_logging::validate(f)?;
    let current = std::env::current_exe()?;
    let parent = current.parent().context("runner location")?;
    let parent = if parent.file_name().is_some_and(|p| p == "deps") {
        parent.parent().context("target")?
    } else {
        parent
    };
    let dir = tempfile::tempdir()?;
    let mut command = tokio::process::Command::new(parent.join("serverless-conformance"));
    command
        .arg("__logging-component")
        .arg(f["initLoggingProbe"]["mode"].as_str().context("mode")?)
        .env_clear()
        .current_dir(dir.path())
        .kill_on_drop(true);
    match f["initLoggingProbe"]["mode"].as_str() {
        Some("configured-debug-v1") => {
            command.env("RUST_LOG", "debug");
        }
        Some("configured-error-v1") => {
            command.env("RUST_LOG", "error");
        }
        Some("default-info-v1") => {}
        _ => anyhow::bail!("unknown logging mode"),
    }
    let start = epoch()?;
    let execution = tokio::time::timeout(Duration::from_secs(5), command.output()).await??;
    let end = epoch()?;
    ensure!(
        execution.status.success(),
        "logging component process failed"
    );
    let writes = std::fs::read_dir(dir.path())?
        .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
        .collect::<std::io::Result<Vec<_>>>()?;
    let actual = json!({"outcome":"initLogging","stdout":String::from_utf8(execution.stdout)?,"stderr":String::from_utf8(execution.stderr)?,"fileWrites":writes,"processWindow":{"startEpochMs":start,"endEpochMs":end}});
    let errors = match assert(&f["expect"], &actual) {
        Ok(()) => vec![],
        Err(e) => vec![format!("{e:#}")],
    };
    Ok(
        json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":end-start,"observations":[actual],"assertionErrors":errors}),
    )
}
pub(crate) fn command(args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 2
            && matches!(
                args[1].as_str(),
                "configured-debug-v1" | "configured-error-v1" | "default-info-v1"
            ),
        "private logging component arguments"
    );
    light_lambda_runtime::telemetry::initialize().map_err(|e| anyhow::anyhow!(e.message()))?;
    tracing::trace!(target:"light_lambda::init", message="test-only-trace");
    tracing::debug!(target:"light_lambda::init", message="test-only-debug");
    tracing::info!(target:"light_lambda::init", message="test-only-info");
    tracing::warn!(target:"light_lambda::init", message="test-only-warn");
    tracing::error!(target:"light_lambda::init", message="test-only-error");
    Ok(())
}
pub(crate) fn assert(expected: &Value, actual: &Value) -> Result<()> {
    ensure!(
        actual["outcome"] == "initLogging"
            && actual["stderr"] == expected["stderr"]
            && actual["fileWrites"] == expected["fileWrites"],
        "logging process contract"
    );
    let stdout = actual["stdout"].as_str().context("actual stdout")?;
    for forbidden in expected["stdoutContract"]["forbiddenSubstrings"]
        .as_array()
        .context("forbidden strings")?
    {
        ensure!(
            !stdout.contains(forbidden.as_str().context("forbidden string")?),
            "forbidden logging output"
        );
    }
    let mut records = vec![];
    for line in stdout.lines() {
        let mut record: Value = serde_json::from_str(line)?;
        let fields = record.as_object().context("log record")?;
        ensure!(
            fields.len() == 4
                && ["timestamp", "level", "target", "fields"]
                    .iter()
                    .all(|k| fields.contains_key(*k)),
            "log record shape"
        );
        let timestamp = chrono::DateTime::parse_from_rfc3339(
            record["timestamp"].as_str().context("timestamp")?,
        )?
        .timestamp_millis();
        ensure!(
            timestamp
                >= actual["processWindow"]["startEpochMs"]
                    .as_i64()
                    .context("process start")?
                && timestamp
                    <= actual["processWindow"]["endEpochMs"]
                        .as_i64()
                        .context("process end")?,
            "timestamp outside actual process window"
        );
        record.as_object_mut().unwrap().remove("timestamp");
        records.push(record);
    }
    ensure!(
        json!(records) == expected["stdoutContract"]["records"],
        "logging levels/order/records differ"
    );
    Ok(())
}
