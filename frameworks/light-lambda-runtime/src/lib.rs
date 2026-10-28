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
