use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use url::{Host, Url};

pub const PROXY_FILE: &str = "proxy.yml";
pub const PROXY_MODULE_ID: &str = "light-pingora/proxy";
pub const PROXY_CONFIG_NAME: &str = "proxy";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub http2_enabled: bool,
    #[serde(default = "default_hosts")]
    pub hosts: String,
    #[serde(default = "default_connections_per_thread")]
    pub connections_per_thread: usize,
    #[serde(default = "default_max_request_time")]
    pub max_request_time: u64,
    #[serde(default = "default_rewrite_host_header")]
    pub rewrite_host_header: bool,
    #[serde(default)]
    pub reuse_x_forwarded: bool,
    #[serde(default = "default_max_connection_retries")]
    pub max_connection_retries: usize,
    #[serde(default)]
    pub max_queue_size: usize,
    #[serde(default)]
    pub forward_jwt_claims: bool,
    #[serde(default)]
    pub metrics_injection: bool,
    #[serde(default = "default_metrics_name")]
    pub metrics_name: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            http2_enabled: false,
            hosts: default_hosts(),
            connections_per_thread: default_connections_per_thread(),
            max_request_time: default_max_request_time(),
            rewrite_host_header: default_rewrite_host_header(),
            reuse_x_forwarded: false,
            max_connection_retries: default_max_connection_retries(),
            max_queue_size: 0,
            forward_jwt_claims: false,
            metrics_injection: false,
            metrics_name: default_metrics_name(),
        }
    }
}

impl ProxyConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            hosts: String::new(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyTarget {
    pub address: String,
    pub tls: bool,
    pub sni: String,
    pub host_header: String,
    pub path_prefix: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyRoute {
    pub config: ProxyConfig,
    pub targets: Vec<ProxyTarget>,
}

impl ProxyRoute {
    pub fn select(&self, index: usize) -> Option<ProxyTarget> {
        if self.targets.is_empty() {
            return None;
        }
        Some(self.targets[index % self.targets.len()].clone())
    }

    pub fn rewrite_host_header(&self) -> bool {
        self.config.rewrite_host_header
    }
}

pub fn load_proxy_route(
    runtime_config: &RuntimeConfig,
) -> Result<Option<ProxyRoute>, RuntimeError> {
    let config = match runtime_config
        .module_registry
        .load_config::<ProxyConfig>(runtime_config, PROXY_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == PROXY_FILE => {
            let config = ProxyConfig::disabled();
            register_proxy_config(runtime_config, &config, false)?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };

    if !config.enabled {
        register_proxy_config(runtime_config, &config, false)?;
        return Ok(None);
    }

    let targets = parse_proxy_targets(&config.hosts)?;
    if targets.is_empty() {
        return Err(RuntimeError::Unsupported(
            "proxy.hosts must contain at least one HTTP or HTTPS upstream when proxy is enabled"
                .to_string(),
        ));
    }

    register_proxy_config(runtime_config, &config, true)?;
    Ok(Some(ProxyRoute { config, targets }))
}

fn register_proxy_config(
    runtime_config: &RuntimeConfig,
    config: &ProxyConfig,
    active: bool,
) -> Result<(), RuntimeError> {
    runtime_config.module_registry.register_loaded_config(
        PROXY_MODULE_ID,
        PROXY_CONFIG_NAME,
        ModuleKind::Framework,
        config,
        [],
        active,
        Some(config.enabled),
        true,
    )?;
    Ok(())
}

pub fn parse_proxy_targets(hosts: &str) -> Result<Vec<ProxyTarget>, RuntimeError> {
    let mut targets = Vec::new();
    for raw_host in hosts.split(',') {
        let raw_host = raw_host.trim();
        if raw_host.is_empty() {
            continue;
        }
        targets.push(parse_proxy_target(raw_host)?);
    }
    Ok(targets)
}

fn parse_proxy_target(raw_host: &str) -> Result<ProxyTarget, RuntimeError> {
    let url = Url::parse(raw_host)
        .map_err(|e| RuntimeError::Unsupported(format!("invalid proxy host `{raw_host}`: {e}")))?;
    let tls = match url.scheme() {
        "http" => false,
        "https" => true,
        scheme => {
            return Err(RuntimeError::Unsupported(format!(
                "proxy host `{raw_host}` uses unsupported scheme `{scheme}`"
            )));
        }
    };
    if url.username() != "" || url.password().is_some() {
        return Err(RuntimeError::Unsupported(format!(
            "proxy host `{raw_host}` must not contain user info"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(RuntimeError::Unsupported(format!(
            "proxy host `{raw_host}` must not contain query or fragment"
        )));
    }

    let host = url.host().ok_or_else(|| {
        RuntimeError::Unsupported(format!("proxy host `{raw_host}` is missing a host"))
    })?;
    let host_for_authority = host_for_authority(host);
    let sni = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().ok_or_else(|| {
        RuntimeError::Unsupported(format!("proxy host `{raw_host}` is missing a port"))
    })?;
    let address = format!("{host_for_authority}:{port}");
    let host_header = match url.port() {
        Some(_) => address.clone(),
        None => host_for_authority,
    };
    let path_prefix = normalize_path_prefix(url.path());

    Ok(ProxyTarget {
        address,
        tls,
        sni,
        host_header,
        path_prefix,
    })
}

fn host_for_authority(host: Host<&str>) -> String {
    match host {
        Host::Domain(domain) => domain.to_string(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    }
}

fn normalize_path_prefix(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        path.to_string()
    }
}

fn default_enabled() -> bool {
    true
}

fn default_hosts() -> String {
    "http://localhost:8080".to_string()
}

fn default_connections_per_thread() -> usize {
    20
}

fn default_max_request_time() -> u64 {
    1000
}

fn default_rewrite_host_header() -> bool {
    true
}

fn default_max_connection_retries() -> usize {
    3
}

fn default_metrics_name() -> String {
    "proxy-response".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig,
        ServiceIdentity,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn runtime_config(config_dir: &TempDir) -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: config_dir.path().join("external"),
            resolved_values: HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn parses_comma_separated_http_and_https_hosts() {
        let targets = parse_proxy_targets("http://127.0.0.1:8080, https://api.example.com/base")
            .expect("parse proxy targets");

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].address, "127.0.0.1:8080");
        assert!(!targets[0].tls);
        assert_eq!(targets[0].host_header, "127.0.0.1:8080");
        assert_eq!(targets[1].address, "api.example.com:443");
        assert!(targets[1].tls);
        assert_eq!(targets[1].host_header, "api.example.com");
        assert_eq!(targets[1].path_prefix, "/base");
    }

    #[test]
    fn rejects_unsupported_proxy_host_scheme() {
        let error = parse_proxy_targets("tcp://127.0.0.1:8080")
            .expect_err("unsupported scheme should fail");

        assert!(error.to_string().contains("unsupported scheme"));
    }

    #[test]
    fn missing_proxy_yml_registers_disabled_module() {
        let config_dir = TempDir::new().expect("config temp dir");
        let runtime = runtime_config(&config_dir);

        let route = load_proxy_route(&runtime).expect("missing proxy config should be disabled");

        assert!(route.is_none());
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == PROXY_MODULE_ID && !entry.active)
        );
    }
}
