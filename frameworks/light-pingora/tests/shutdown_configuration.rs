const LIGHT_PINGORA_MANIFEST: &str = include_str!("../Cargo.toml");
const LIGHT_PINGORA_SOURCE: &str = include_str!("../src/lib.rs");

#[test]
fn pingora_uses_registry_release_with_zero_internal_shutdown_periods() {
    assert!(LIGHT_PINGORA_MANIFEST.contains("pingora = { version = \"=0.8.1\""));
    assert!(LIGHT_PINGORA_SOURCE.contains("server_conf.grace_period_seconds = Some(0);"));
    assert!(
        LIGHT_PINGORA_SOURCE.contains("server_conf.graceful_shutdown_timeout_seconds = Some(0);")
    );
}
