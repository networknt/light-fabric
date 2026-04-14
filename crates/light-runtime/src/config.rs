use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapConfig {
    #[serde(default = "default_host")]
    pub host: String,
    pub service_id: Option<String>,
    pub product_id: Option<String>,
    pub product_version: Option<String>,
    pub api_id: Option<String>,
    pub api_version: Option<String>,
    pub env_tag: Option<String>,
    #[serde(default = "default_accept_header")]
    pub accept_header: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout: u64,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout: u64,
    pub config_server_uri: Option<String>,
    #[serde(default)]
    pub authorization: Option<String>,
    pub bootstrap_cert_path: Option<PathBuf>,
    pub bootstrap_key_path: Option<PathBuf>,
    pub bootstrap_ca_cert_path: Option<PathBuf>,
    pub external_config_dir: Option<PathBuf>,
}

impl std::fmt::Debug for BootstrapConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BootstrapConfig")
            .field("host", &self.host)
            .field("service_id", &self.service_id)
            .field("product_id", &self.product_id)
            .field("product_version", &self.product_version)
            .field("api_id", &self.api_id)
            .field("api_version", &self.api_version)
            .field("env_tag", &self.env_tag)
            .field("accept_header", &self.accept_header)
            .field("timeout", &self.timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("config_server_uri", &self.config_server_uri)
            .field("authorization", &self.authorization.as_ref().map(|v| if v.is_empty() { "" } else { "********" }))
            .field("bootstrap_cert_path", &self.bootstrap_cert_path)
            .field("bootstrap_key_path", &self.bootstrap_key_path)
            .field("bootstrap_ca_cert_path", &self.bootstrap_ca_cert_path)
            .field("external_config_dir", &self.external_config_dir)
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub ip: String,
    pub http_port: u16,
    pub enable_http: bool,
    pub https_port: u16,
    pub enable_https: bool,
    #[serde(default)]
    pub tls_cert_path: Option<PathBuf>,
    #[serde(default)]
    pub tls_key_path: Option<PathBuf>,
    pub service_id: String,
    #[serde(default)]
    pub enable_registry: bool,
    #[serde(default)]
    pub start_on_registry_failure: bool,
    #[serde(default)]
    pub dynamic_port: bool,
    #[serde(default = "default_environment")]
    pub environment: String,
    #[serde(default = "default_shutdown_graceful_period_ms")]
    pub shutdown_graceful_period: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            ip: "0.0.0.0".to_string(),
            http_port: 8080,
            enable_http: true,
            https_port: 8443,
            enable_https: false,
            tls_cert_path: None,
            tls_key_path: None,
            service_id: "com.networknt.service-1.0.0".to_string(),
            enable_registry: false,
            start_on_registry_failure: false,
            dynamic_port: false,
            environment: String::new(),
            shutdown_graceful_period: default_shutdown_graceful_period_ms(),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortalRegistryConfig {
    pub portal_url: String,
    #[serde(default)]
    pub portal_token: String,
    #[serde(default)]
    pub controller_discovery_token: String,
}

impl std::fmt::Debug for PortalRegistryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalRegistryConfig")
            .field("portal_url", &self.portal_url)
            .field("portal_token", if self.portal_token.is_empty() { &"" } else { &"********" })
            .field("controller_discovery_token", if self.controller_discovery_token.is_empty() { &"" } else { &"********" })
            .finish()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    #[serde(default = "default_verify_hostname")]
    pub verify_hostname: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            verify_hostname: default_verify_hostname(),
        }
    }
}

pub(crate) fn default_verify_hostname() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeConfig {
    pub bootstrap: BootstrapConfig,
    pub server: ServerConfig,
    pub client: Option<ClientConfig>,
    pub portal_registry: Option<PortalRegistryConfig>,
    pub service_identity: ServiceIdentity,
    pub config_dir: PathBuf,
    pub external_config_dir: PathBuf,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceIdentity {
    pub service_id: String,
    pub version: String,
    pub env_tag: Option<String>,
    pub tags: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct RemoteBootstrapResult {
    pub values_yaml: Option<String>,
    pub cached_files: Vec<PathBuf>,
}

pub(crate) fn default_host() -> String {
    "lightapi.net".to_string()
}

pub(crate) fn default_accept_header() -> String {
    "application/json".to_string()
}

pub(crate) fn default_timeout_ms() -> u64 {
    3_000
}

pub(crate) fn default_connect_timeout_ms() -> u64 {
    3_000
}

pub(crate) fn default_environment() -> String {
    String::new()
}

pub(crate) fn default_shutdown_graceful_period_ms() -> u64 {
    2_000
}
