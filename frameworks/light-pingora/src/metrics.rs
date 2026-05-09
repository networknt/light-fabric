use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const METRICS_FILE: &str = "metrics.yml";
pub const METRICS_MODULE_ID: &str = "light-pingora/metrics";
pub const METRICS_CONFIG_NAME: &str = "metrics";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub enable_jvm_monitor: bool,
    #[serde(default = "default_server_protocol")]
    pub server_protocol: String,
    #[serde(default = "default_server_host")]
    pub server_host: String,
    #[serde(default = "default_server_path")]
    pub server_path: String,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default = "default_server_name")]
    pub server_name: String,
    #[serde(default = "default_server_user")]
    pub server_user: String,
    #[serde(default = "default_server_pass")]
    pub server_pass: String,
    #[serde(default = "default_report_in_minutes")]
    pub report_in_minutes: u64,
    #[serde(default = "default_product_name")]
    pub product_name: String,
    #[serde(default)]
    pub send_scope_client_id: bool,
    #[serde(default)]
    pub send_caller_id: bool,
    #[serde(default)]
    pub send_issuer: bool,
    #[serde(default)]
    pub issuer_regex: String,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            enable_jvm_monitor: false,
            server_protocol: default_server_protocol(),
            server_host: default_server_host(),
            server_path: default_server_path(),
            server_port: default_server_port(),
            server_name: default_server_name(),
            server_user: default_server_user(),
            server_pass: default_server_pass(),
            report_in_minutes: default_report_in_minutes(),
            product_name: default_product_name(),
            send_scope_client_id: false,
            send_caller_id: false,
            send_issuer: false,
            issuer_regex: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsEvent {
    pub endpoint: String,
    pub method: String,
    pub status: u16,
    pub status_class: &'static str,
    pub duration_ms: u128,
    pub correlation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricCounts {
    pub request: u64,
    pub success: u64,
    pub auth_error: u64,
    pub request_error: u64,
    pub server_error: u64,
}

#[derive(Debug, Default)]
pub struct MetricsRecorder {
    request: AtomicU64,
    success: AtomicU64,
    auth_error: AtomicU64,
    request_error: AtomicU64,
    server_error: AtomicU64,
}

impl MetricsRecorder {
    pub fn record(&self, status: u16) -> MetricCounts {
        self.request.fetch_add(1, Ordering::Relaxed);
        match classify_status(status) {
            "success" => {
                self.success.fetch_add(1, Ordering::Relaxed);
            }
            "auth_error" => {
                self.auth_error.fetch_add(1, Ordering::Relaxed);
            }
            "request_error" => {
                self.request_error.fetch_add(1, Ordering::Relaxed);
            }
            "server_error" => {
                self.server_error.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        self.snapshot()
    }

    pub fn snapshot(&self) -> MetricCounts {
        MetricCounts {
            request: self.request.load(Ordering::Relaxed),
            success: self.success.load(Ordering::Relaxed),
            auth_error: self.auth_error.load(Ordering::Relaxed),
            request_error: self.request_error.load(Ordering::Relaxed),
            server_error: self.server_error.load(Ordering::Relaxed),
        }
    }
}

pub fn load_metrics_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<MetricsConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<MetricsConfig>(runtime_config, METRICS_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == METRICS_FILE => MetricsConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        METRICS_MODULE_ID,
        METRICS_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("serverPass")],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn build_metrics_event(
    endpoint: impl Into<String>,
    method: impl Into<String>,
    status: u16,
    duration: Duration,
    correlation_id: Option<String>,
) -> MetricsEvent {
    MetricsEvent {
        endpoint: endpoint.into(),
        method: method.into(),
        status,
        status_class: classify_status(status),
        duration_ms: duration.as_millis(),
        correlation_id,
    }
}

pub fn classify_status(status: u16) -> &'static str {
    if (200..400).contains(&status) {
        "success"
    } else if status == 401 || status == 403 {
        "auth_error"
    } else if (400..500).contains(&status) {
        "request_error"
    } else if status >= 500 {
        "server_error"
    } else {
        "unknown"
    }
}

fn default_enabled() -> bool {
    true
}

fn default_server_protocol() -> String {
    "http".to_string()
}

fn default_server_host() -> String {
    "localhost".to_string()
}

fn default_server_path() -> String {
    "/apm/metricFeed".to_string()
}

fn default_server_port() -> u16 {
    8086
}

fn default_server_name() -> String {
    "metrics".to_string()
}

fn default_server_user() -> String {
    "admin".to_string()
}

fn default_server_pass() -> String {
    "admin".to_string()
}

fn default_report_in_minutes() -> u64 {
    1
}

fn default_product_name() -> String {
    "http-sidecar".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_status_like_light_4j_metrics() {
        assert_eq!(classify_status(200), "success");
        assert_eq!(classify_status(302), "success");
        assert_eq!(classify_status(401), "auth_error");
        assert_eq!(classify_status(403), "auth_error");
        assert_eq!(classify_status(404), "request_error");
        assert_eq!(classify_status(500), "server_error");
    }

    #[test]
    fn recorder_counts_metric_classes() {
        let recorder = MetricsRecorder::default();

        recorder.record(200);
        recorder.record(403);
        let counts = recorder.record(500);

        assert_eq!(counts.request, 3);
        assert_eq!(counts.success, 1);
        assert_eq!(counts.auth_error, 1);
        assert_eq!(counts.server_error, 1);
    }
}
