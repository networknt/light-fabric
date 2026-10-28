//! Isolated real subscriber proof; never changes the parent test subscriber.
use crate::telemetry::{self, DiagnosticLevel};

#[test]
#[ignore = "subprocess-only actual subscriber probe"]
fn subscriber_probe() {
    if std::env::var("LOGGER_SUBSCRIBER_TEST_CHILD").as_deref() != Ok("1") {
        return;
    }
    telemetry::initialize().unwrap();
    assert_eq!(telemetry::installed_filter(), Some("info"));
    telemetry::set_diagnostic_level(DiagnosticLevel::Debug).unwrap();
    assert_eq!(telemetry::installed_filter(), Some("debug"));
    tracing::debug!(target:"light_lambda::logger_set_test", message="LOGGER_REAL_DEBUG_VISIBLE");
    telemetry::set_diagnostic_level(DiagnosticLevel::Info).unwrap();
    tracing::debug!(target:"light_lambda::logger_set_test", message="LOGGER_REAL_DEBUG_MUST_BE_HIDDEN");
    telemetry::set_diagnostic_level(DiagnosticLevel::Trace).unwrap();
    tracing::trace!(target:"light_security::logger_set_probe", credential="logger-private-sentinel",payload="logger-private-sentinel",message="logger-private-sentinel");
    let _ = std::panic::catch_unwind(|| panic!("logger-panic-private-sentinel"));
    assert!(telemetry::initialize().is_err());
    assert_eq!(telemetry::installed_filter(), Some("trace"));
    tracing::info!(target:"audit", message="LOGGER_AUDIT_STILL_VISIBLE");
}

#[test]
fn actual_reload_privacy_and_panic_redaction() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "--ignored",
            "logger_set_tests::subscriber_probe",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env("LOGGER_SUBSCRIBER_TEST_CHILD", "1")
        .env("RUST_LOG", "info")
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    let err = String::from_utf8(output.stderr).unwrap();
    if let Some(root) = std::env::var_os("WP10_L1_EVIDENCE_DIR") {
        let root = std::path::PathBuf::from(root).join("logger-set-trace-privacy");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("stdout.log"), &text).unwrap();
        std::fs::write(root.join("stderr.log"), &err).unwrap();
    }
    assert!(output.status.success(), "{text} {err}");
    assert!(text.contains("LOGGER_REAL_DEBUG_VISIBLE"));
    assert!(!text.contains("LOGGER_REAL_DEBUG_MUST_BE_HIDDEN"));
    assert!(
        text.contains("light_security::logger_set_probe")
            && text.contains("<redacted dependency diagnostic>")
    );
    assert!(text.contains("panic payload redacted") && text.contains("LOGGER_AUDIT_STILL_VISIBLE"));
    for secret in ["logger-private-sentinel", "logger-panic-private-sentinel"] {
        assert!(!text.contains(secret) && !err.contains(secret));
    }
}
