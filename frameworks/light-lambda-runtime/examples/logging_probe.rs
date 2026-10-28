//! Subprocess-only qualification support; not a Lambda app or bootstrap binary.
fn main() {
    light_lambda_runtime::telemetry::initialize().unwrap();
    tracing::debug!(target:"light_lambda::init",message="test-only-debug");
    tracing::info!(target:"light_lambda::init",message="test-only-info");
    tracing::warn!(target:"light_lambda::init",message="test-only-warn");
    tracing::error!(target:"light_lambda::init",message="test-only-error");
    if std::env::args().any(|s| s == "privacy") {
        tracing::trace!(target:"light_security::test_probe",credential="test-only-secret-sentinel",message="test-only-secret-sentinel");
    }
}
