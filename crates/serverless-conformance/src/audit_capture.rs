//! Observe the actual application's asynchronous Audit StdoutSink tracing output.
//! No replacement policy factory or handler implementation is installed.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub(crate) static APPLICATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static LINES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
static OVERFLOW: AtomicBool = AtomicBool::new(false);
struct Capture;
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut lines = LINES.lock().unwrap();
        if lines.len() + bytes.len() > 1_048_576 {
            OVERFLOW.store(true, Ordering::SeqCst);
            return Err(std::io::Error::other("audit capture overflow"));
        }
        lines.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(crate) fn enabled(f: &Value) -> bool {
    crate::application::cases(f)
        .iter()
        .any(|c| c["observeAudit"] == true)
}
pub(crate) fn start() -> Result<()> {
    let installed = INSTALLED.get_or_init(|| {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter("audit=trace,light_lambda::init=warn,light_security=trace,light_lambda_runtime=trace,serverless_aws=trace,light_rule::cel=warn,light_lambda::trace_probe=info,light_lambda::context=info")
            .with_writer(|| Capture)
            .try_init()
            .map_err(|e| e.to_string())
    });
    ensure!(
        installed.is_ok(),
        "cannot observe application audit output: {installed:?}"
    );
    LINES.lock().unwrap().clear();
    OVERFLOW.store(false, Ordering::SeqCst);
    Ok(())
}
pub(crate) fn take() -> Result<Value> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "audit capture overflow");
    let bytes = std::mem::take(&mut *LINES.lock().unwrap());
    let mut records = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line).context("actual audit JSON line")?;
        if event["target"] != "audit" {
            continue;
        }
        // The runtime also emits its own target=audit completion telemetry.
        // It is a different record vocabulary from AuditMiddleware policy data.
        if event["fields"].get("audit").is_none()
            && event["fields"].get("adapter_version").is_some()
        {
            continue;
        }
        let data: Value = serde_json::from_str(
            event["fields"]["audit"]
                .as_str()
                .context("actual audit policy field")?,
        )?;
        let mut record = json!({"data":data,"message":event["fields"]["audit"]});
        if let Some(stack) = event["fields"].get("stackTrace") {
            record["stackTrace"] = stack.clone();
        }
        records.push(record);
    }
    Ok(json!(records))
}
pub(crate) async fn completion() -> Result<Value> {
    // StdoutSink is explicitly best effort and has no acknowledgment API.
    // This bounded observation window does not fabricate a missing record.
    tokio::time::sleep(Duration::from_millis(50)).await;
    take()
}

/// Observe the typed production warning, rather than deriving it from config.
pub(crate) fn configuration_warning() -> Result<Value> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut warnings = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "light_lambda::init"
            && event["fields"]["configuration_key"] == "handler.reportHandlerDuration"
        {
            warnings.push(event["fields"].clone());
        }
    }
    let warning = warnings
        .first()
        .context("missing production configuration warning")?;
    Ok(
        json!({"key":warning["configuration_key"],"mode":warning["mode"],"ignored":warning["ignored"],"warningCount":warnings.len()}),
    )
}

/// Return only exposure booleans. Never copy bearer material to result evidence.
pub(crate) fn security_observation(token: &str) -> Result<Value> {
    ensure!(token.len() >= 10, "invalid signed token witness");
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap();
    let logs = std::str::from_utf8(&bytes)?;
    Ok(json!({"fullTokenExposed":logs.contains(token),
        "tokenFragmentExposed":logs.contains(&token[..10]),
        "sensitiveClaimExposed":logs.contains("test-only-sensitive-claim")}))
}

pub(crate) fn configuration_warning_evidence() -> Result<Value> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut records = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "light_lambda::init"
            && event["fields"]["configuration_key"] == "handler.reportHandlerDuration"
        {
            records.push(json!({"key":event["fields"]["configuration_key"],"message":event["fields"]["message"]}));
        }
    }
    Ok(json!(records))
}

pub(crate) fn configuration_diagnostics() -> Result<Vec<String>> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut diagnostics = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "serverless_aws::alb"
            && event["fields"]["configuration_error"] == "multi_value_required"
        {
            diagnostics.push(std::str::from_utf8(line)?.to_owned());
        }
    }
    Ok(diagnostics)
}

pub(crate) fn backend_diagnostics() -> Result<Value> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut records = vec![];
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "serverless_aws::v1" {
            records.push(event["fields"].clone());
        }
    }
    ensure!(
        records.len() == 1,
        "expected exactly one real FunctionError diagnostic"
    );
    let record = &records[0];
    let fields = record.as_object().context("structured diagnostic event")?;
    ensure!(
        fields.len() == 1 && fields.contains_key("message"),
        "unexpected FunctionError log fields"
    );
    let message = record["message"]
        .as_str()
        .context("actual diagnostic message")?;
    let metadata: Value = serde_json::from_str(message)?;
    let allowed = [
        "requestId",
        "targetAlias",
        "functionError",
        "executedVersion",
        "payloadBytes",
        "errorType",
    ];
    ensure!(metadata.as_object().is_some_and(|m| m.len() == allowed.len() && allowed.iter().all(|k| m.contains_key(*k))), "unsafe diagnostic metadata fields");
    // Preserve the actual production JSON message, including null, verbatim.
    Ok(json!([{"logger":"serverless_aws::v1","level":"WARN","message":message}]))
}

pub(crate) fn rule_failures() -> Result<usize> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut failures = 0;
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "light_rule::cel"
            && (event["fields"]["message"] == "CEL expression evaluation failed"
                || event["fields"]["message"] == "CEL interpreter panic contained")
        {
            failures += 1;
        }
    }
    Ok(failures)
}

pub(crate) fn trace_context() -> Result<Value> {
    ensure!(!OVERFLOW.load(Ordering::SeqCst), "capture overflow");
    let bytes = LINES.lock().unwrap().clone();
    let mut context = Value::Null;
    for line in bytes.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let event: Value = serde_json::from_slice(line)?;
        if event["target"] == "light_lambda::trace_probe" {
            context = event["span"]
                .get("traceability_id")
                .or_else(|| event["span"].get("traceabilityId"))
                .cloned()
                .unwrap_or(Value::Null);
        }
    }
    Ok(context)
}
