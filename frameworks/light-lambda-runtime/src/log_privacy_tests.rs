use super::Privacy;
use serde_json::Value;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tracing_core::Dispatch;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
fn capture(level: tracing::Level, emit: impl FnOnce()) -> (String, Vec<Value>) {
    let writer = Capture::default();
    // Stronger than the installed formatter: context must remain private even
    // when a consumer explicitly asks to render current and ancestor spans.
    let inner = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_max_level(level)
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(writer.clone())
        .finish();
    tracing::subscriber::with_default(Privacy(Dispatch::new(inner)), emit);
    let text = String::from_utf8(writer.0.lock().unwrap().clone()).unwrap();
    let rows = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (text, rows)
}

#[test]
fn jwks_allowlist_requires_exact_fields_types_and_finite_values_at_every_level() {
    for level in [
        tracing::Level::INFO,
        tracing::Level::DEBUG,
        tracing::Level::TRACE,
    ] {
        let (text, rows) = capture(level, || {
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=1u64);
            tracing::info!(target:"light_security::jwks_fetch", fetch_id=u64::MAX, event="completed");
            tracing::info!(target:"light_security::jwks_fetch", event="interrupted_or_failed", fetch_id=2u64);
            tracing::info!(target:"light_security::jwks_fetch", event="PRIVATE_EVENT", fetch_id=3u64);
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id="PRIVATE_ID");
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=-1i64);
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=1.5f64);
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=true);
            tracing::info!(target:"light_security::jwks_fetch", event=?"PRIVATE_DEBUG", fetch_id=4u64);
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=5u64, authorization="PRIVATE_TOKEN");
            tracing::info!(target:"light_security::jwks_fetch", event="started");
            tracing::info!(target:"light_security::jwks_fetch", fetch_id=6u64);
            tracing::info!(target:"light_security::jwks_fetch_extra", event="started", fetch_id=7u64, secret="PRIVATE_PROVIDER");
            tracing::info!(target:"audit", approved_audit_field="retained");
        });
        assert_eq!(rows.len(), 14);
        assert_eq!(rows[0]["fields"]["fetch_id"], 1);
        assert_eq!(rows[1]["fields"]["fetch_id"], u64::MAX);
        assert_eq!(rows[2]["fields"]["event"], "interrupted_or_failed");
        for row in &rows[3..13] {
            assert!(
                row["fields"]
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|v| v == "<redacted dependency diagnostic>"),
                "{row}"
            );
        }
        assert_eq!(rows[13]["fields"]["approved_audit_field"], "retained");
        assert!(!text.contains("PRIVATE_"), "{text}");
    }
}

#[test]
fn jwks_diagnostics_never_inherit_sensitive_contextual_or_explicit_span_fields() {
    for level in [
        tracing::Level::INFO,
        tracing::Level::DEBUG,
        tracing::Level::TRACE,
    ] {
        let (text, rows) = capture(level, || {
            let outer = tracing::info_span!(
                "sensitive_parent",
                authorization = "PRIVATE_TOKEN",
                provider = "PRIVATE_URL"
            );
            let _entered = outer.enter();
            let inner = tracing::info_span!("sensitive_child", payload = "PRIVATE_BODY");
            let _inner = inner.enter();
            let before = tracing::Span::current().id();
            tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=1u64);
            tracing::info!(target:"light_security::jwks_fetch", parent:&outer, event="completed", fetch_id=1u64);
            tracing::info!(target:"light_security::jwks_fetch", parent:&inner, event="PRIVATE_ERROR", fetch_id=1u64);
            assert_eq!(tracing::Span::current().id(), before);
        });
        assert_eq!(rows.len(), 3);
        assert!(!text.contains("PRIVATE_"), "span context leaked: {text}");
        for row in rows {
            assert_eq!(
                row["span"],
                serde_json::json!({"name":"safe_diagnostic"}),
                "{row}"
            );
            assert_eq!(
                row["spans"],
                serde_json::json!([{"name":"safe_diagnostic"}]),
                "{row}"
            );
        }
    }
}

#[test]
fn sdk_dependency_request_ids_remain_redacted_at_trace() {
    let (text, rows) = capture(tracing::Level::TRACE, || {
        tracing::debug!(target:"aws_sdk_lambda::operation::invoke", request_id=?Some("PRIVATE_NOT_UUID"));
        tracing::trace!(target:"aws_smithy_runtime::client::orchestrator", response="x-amzn-requestid: PRIVATE_NOT_UUID");
        tracing::trace!(target:"aws_smithy_runtime::client::orchestrator", output_or_error="_request_id: PRIVATE_NOT_UUID");
    });
    assert_eq!(rows.len(), 3);
    assert!(!text.contains("PRIVATE_"));
    assert!(rows.iter().all(|row| {
        row["fields"]
            .as_object()
            .unwrap()
            .values()
            .all(|v| v == "<redacted dependency diagnostic>")
    }));
}
