//! Immutable local configuration and Lambda runtime integration.
pub mod admin_policy;
pub mod audit_policy;
pub mod bundle_snapshot;
pub mod cors_policy;
pub mod factor_policy;
pub mod head_policy;
pub mod http_router;
pub mod init;
pub mod init_error;
pub mod limit_policy;
mod local_activation;
mod log_privacy;
pub mod logger_set_policy;
#[cfg(test)]
mod logger_set_tests;
pub mod metrics_policy;
pub mod qualification_shutdown;
mod remaining_policy;
pub mod router_java_worker;
pub mod router_policy;
pub mod router_program;
pub mod router_rewrite_policy;
pub mod runtime;
pub mod sanitizer_policy;
mod security;
pub mod service;
mod service_admission;
pub mod specification_policy;
pub mod telemetry;
pub mod token_policy;
pub mod transformer_adapter;
pub mod transformer_policy;
pub mod transformer_program;
pub mod validator_failure_policy;
pub mod validator_init_diagnostic;
pub mod validator_ipc;
pub mod validator_policy;

pub mod required_validator_profile;

pub mod owned_runtime;

/// Opt-in qualification bridge; ordinary startup never calls it.
pub mod qualification_extension_bridge;

/// Qualification-only switches are absent from ordinary builds. Deployable
/// artifacts must not enable the explicit insecure-test-config feature.
#[doc(hidden)]
pub fn insecure_test_flag(name: &str) -> bool {
    #[cfg(feature = "insecure-test-config")]
    {
        std::env::var(name).as_deref() == Ok("1")
    }
    #[cfg(not(feature = "insecure-test-config"))]
    {
        let _ = name;
        false
    }
}

#[cfg(all(test, not(feature = "insecure-test-config")))]
mod production_switch_tests {
    #[test]
    fn production_build_ignores_insecure_environment_switches() {
        if std::env::var("REVIEW_SWITCH_CHILD").as_deref() == Ok("1") {
            for name in [
                "LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG",
                "LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS",
            ] {
                assert_eq!(std::env::var(name).as_deref(), Ok("1"));
                assert!(!super::insecure_test_flag(name));
            }
            return;
        }
        assert!(std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "production_switch_tests::production_build_ignores_insecure_environment_switches"])
            .env("REVIEW_SWITCH_CHILD", "1")
            .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG", "1")
            .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
            .status().unwrap().success());
    }
}
