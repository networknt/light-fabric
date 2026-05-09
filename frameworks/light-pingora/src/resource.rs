use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const PATH_RESOURCE_FILE: &str = "path-resource.yml";
pub const PATH_RESOURCE_LEGACY_FILE: &str = "path-resource.yaml";
pub const PATH_RESOURCE_MODULE_ID: &str = "light-pingora/path-resource";
pub const PATH_RESOURCE_CONFIG_NAME: &str = "path-resource";
pub const VIRTUAL_HOST_FILE: &str = "virtual-host.yml";
pub const VIRTUAL_HOST_LEGACY_FILE: &str = "virtual-host.yaml";
pub const VIRTUAL_HOST_MODULE_ID: &str = "light-pingora/virtual-host";
pub const VIRTUAL_HOST_CONFIG_NAME: &str = "virtual-host";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathResourceConfig {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub base: String,
    #[serde(default)]
    pub prefix: bool,
    #[serde(default = "default_transfer_min_size")]
    pub transfer_min_size: u64,
    #[serde(default)]
    pub directory_listing_enabled: bool,
}

impl Default for PathResourceConfig {
    fn default() -> Self {
        Self {
            path: String::new(),
            base: String::new(),
            prefix: false,
            transfer_min_size: default_transfer_min_size(),
            directory_listing_enabled: false,
        }
    }
}

impl PathResourceConfig {
    fn is_configured(&self) -> bool {
        !self.path.trim().is_empty() || !self.base.trim().is_empty()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualHostConfig {
    #[serde(default)]
    pub hosts: Vec<VirtualHost>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualHost {
    pub domain: String,
    pub path: String,
    pub base: String,
    #[serde(default = "default_transfer_min_size")]
    pub transfer_min_size: u64,
    #[serde(default)]
    pub directory_listing_enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticResourceSet {
    pub path_resource: Option<StaticSite>,
    pub virtual_hosts: BTreeMap<String, StaticSite>,
}

impl StaticResourceSet {
    pub fn empty() -> Self {
        Self {
            path_resource: None,
            virtual_hosts: BTreeMap::new(),
        }
    }

    pub fn resolve_path_resource(&self, request_path: &str) -> StaticResolution {
        self.path_resource
            .as_ref()
            .map(|site| site.resolve(request_path))
            .unwrap_or(StaticResolution::NotFound)
    }

    pub fn resolve_virtual_host(
        &self,
        host_header: Option<&str>,
        request_path: &str,
    ) -> StaticResolution {
        let Some(host) = host_header.and_then(normalize_host_header) else {
            return StaticResolution::NotFound;
        };
        self.virtual_hosts
            .get(&host)
            .map(|site| site.resolve(request_path))
            .unwrap_or(StaticResolution::NotFound)
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StaticSite {
    pub path: String,
    pub base: PathBuf,
    pub prefix: bool,
    pub transfer_min_size: u64,
    pub directory_listing_enabled: bool,
}

impl StaticSite {
    pub fn resolve(&self, request_path: &str) -> StaticResolution {
        let Some(relative) = strip_static_path(&self.path, self.prefix, request_path) else {
            return StaticResolution::NotFound;
        };
        let Some(relative_path) = safe_relative_path(relative) else {
            return StaticResolution::Forbidden;
        };

        let candidate = self.base.join(&relative_path);
        if candidate.is_file() {
            return StaticResolution::file(candidate);
        }
        if candidate.is_dir() {
            let index = candidate.join("index.html");
            if index.is_file() {
                return StaticResolution::file(index);
            }
            return StaticResolution::NotFound;
        }

        if !looks_like_asset(&relative_path) {
            let index = self.base.join("index.html");
            if index.is_file() {
                return StaticResolution::file(index);
            }
        }

        StaticResolution::NotFound
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticResolution {
    File(StaticFile),
    NotFound,
    Forbidden,
}

impl StaticResolution {
    fn file(path: PathBuf) -> Self {
        let content_type = content_type_for_path(&path).to_string();
        let cache_control = cache_control_for_path(&path).to_string();
        Self::File(StaticFile {
            path,
            content_type,
            cache_control,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticFile {
    pub path: PathBuf,
    pub content_type: String,
    pub cache_control: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MissingStaticConfig {
    enabled: bool,
}

pub fn load_static_resources(
    runtime_config: &RuntimeConfig,
) -> Result<StaticResourceSet, RuntimeError> {
    let path_resource = load_path_resource(runtime_config)?;
    let virtual_hosts = load_virtual_hosts(runtime_config)?;
    Ok(StaticResourceSet {
        path_resource,
        virtual_hosts,
    })
}

fn load_path_resource(runtime_config: &RuntimeConfig) -> Result<Option<StaticSite>, RuntimeError> {
    let Some((_, config)) = load_config_any::<PathResourceConfig>(
        runtime_config,
        &[PATH_RESOURCE_FILE, PATH_RESOURCE_LEGACY_FILE],
    )?
    else {
        register_missing_config(
            runtime_config,
            PATH_RESOURCE_MODULE_ID,
            PATH_RESOURCE_CONFIG_NAME,
        )?;
        return Ok(None);
    };

    let site = if config.is_configured() {
        Some(build_path_resource_site(runtime_config, &config)?)
    } else {
        None
    };
    runtime_config.module_registry.register_loaded_config(
        PATH_RESOURCE_MODULE_ID,
        PATH_RESOURCE_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        site.is_some(),
        Some(config.is_configured()),
        true,
    )?;
    Ok(site)
}

fn load_virtual_hosts(
    runtime_config: &RuntimeConfig,
) -> Result<BTreeMap<String, StaticSite>, RuntimeError> {
    let Some((_, config)) = load_config_any::<VirtualHostConfig>(
        runtime_config,
        &[VIRTUAL_HOST_FILE, VIRTUAL_HOST_LEGACY_FILE],
    )?
    else {
        register_missing_config(
            runtime_config,
            VIRTUAL_HOST_MODULE_ID,
            VIRTUAL_HOST_CONFIG_NAME,
        )?;
        return Ok(BTreeMap::new());
    };

    let mut hosts = BTreeMap::new();
    for host in &config.hosts {
        let domain = normalize_domain(&host.domain).ok_or_else(|| {
            RuntimeError::Unsupported("virtual-host.hosts domain must not be empty".to_string())
        })?;
        if hosts
            .insert(
                domain.clone(),
                build_virtual_host_site(runtime_config, host)?,
            )
            .is_some()
        {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate virtual-host domain `{domain}`"
            )));
        }
    }
    runtime_config.module_registry.register_loaded_config(
        VIRTUAL_HOST_MODULE_ID,
        VIRTUAL_HOST_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        !hosts.is_empty(),
        Some(!config.hosts.is_empty()),
        true,
    )?;
    Ok(hosts)
}

fn load_config_any<T>(
    runtime_config: &RuntimeConfig,
    files: &[&str],
) -> Result<Option<(String, T)>, RuntimeError>
where
    T: for<'de> Deserialize<'de>,
{
    for file in files {
        match runtime_config
            .module_registry
            .load_config::<T>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(((*file).to_string(), config))),
            Err(RuntimeError::MissingConfig(missing)) if missing == *file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn register_missing_config(
    runtime_config: &RuntimeConfig,
    module_id: &str,
    config_name: &str,
) -> Result<(), RuntimeError> {
    runtime_config.module_registry.register_loaded_config(
        module_id,
        config_name,
        ModuleKind::Framework,
        &MissingStaticConfig { enabled: false },
        [],
        false,
        Some(false),
        true,
    )?;
    Ok(())
}

fn build_path_resource_site(
    runtime_config: &RuntimeConfig,
    config: &PathResourceConfig,
) -> Result<StaticSite, RuntimeError> {
    if config.path.trim().is_empty() || config.base.trim().is_empty() {
        return Err(RuntimeError::Unsupported(
            "path-resource.path and path-resource.base must both be set when path-resource is configured"
                .to_string(),
        ));
    }
    Ok(StaticSite {
        path: validate_static_path(&config.path, "path-resource.path")?,
        base: resolve_base_path(runtime_config, &config.base),
        prefix: config.prefix,
        transfer_min_size: config.transfer_min_size,
        directory_listing_enabled: config.directory_listing_enabled,
    })
}

fn build_virtual_host_site(
    runtime_config: &RuntimeConfig,
    host: &VirtualHost,
) -> Result<StaticSite, RuntimeError> {
    if host.base.trim().is_empty() {
        return Err(RuntimeError::Unsupported(
            "virtual-host.hosts base must not be empty".to_string(),
        ));
    }
    Ok(StaticSite {
        path: validate_static_path(&host.path, "virtual-host.hosts.path")?,
        base: resolve_base_path(runtime_config, &host.base),
        prefix: true,
        transfer_min_size: host.transfer_min_size,
        directory_listing_enabled: host.directory_listing_enabled,
    })
}

fn validate_static_path(path: &str, name: &str) -> Result<String, RuntimeError> {
    let path = path.trim();
    if !path.starts_with('/') {
        return Err(RuntimeError::Unsupported(format!(
            "{name} `{path}` must start with `/`"
        )));
    }
    if path.len() > 1 {
        Ok(path.trim_end_matches('/').to_string())
    } else {
        Ok("/".to_string())
    }
}

fn resolve_base_path(runtime_config: &RuntimeConfig, base: &str) -> PathBuf {
    let path = PathBuf::from(base);
    if path.is_absolute() {
        path
    } else {
        runtime_config.config_dir.join(path)
    }
}

fn strip_static_path<'a>(site_path: &str, prefix: bool, request_path: &'a str) -> Option<&'a str> {
    if site_path == "/" {
        return Some(request_path.trim_start_matches('/'));
    }
    if request_path == site_path {
        return Some("");
    }
    if prefix {
        let remainder = request_path.strip_prefix(site_path)?;
        return remainder
            .strip_prefix('/')
            .or_else(|| remainder.is_empty().then_some(""));
    }
    None
}

fn safe_relative_path(relative: &str) -> Option<PathBuf> {
    let mut path = PathBuf::new();
    for segment in relative.split('/') {
        if segment.is_empty() {
            continue;
        }
        if segment == "." || segment == ".." || segment.starts_with('.') || segment.contains('\\') {
            return None;
        }
        path.push(segment);
    }
    Some(path)
}

fn looks_like_asset(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains('.'))
}

fn content_type_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("wasm") => "application/wasm",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn cache_control_for_path(path: &Path) -> &'static str {
    if path.file_name().and_then(|name| name.to_str()) == Some("index.html") {
        return "no-cache";
    }
    if is_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn is_hashed_asset(path: &Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| {
            stem.split(['.', '-', '_'])
                .any(|part| part.len() >= 8 && part.chars().all(|ch| ch.is_ascii_hexdigit()))
        })
}

fn normalize_host_header(header: &str) -> Option<String> {
    normalize_domain(header.split(',').next().unwrap_or(header))
}

fn normalize_domain(raw: &str) -> Option<String> {
    let raw = raw.trim().trim_end_matches('.');
    if raw.is_empty() {
        return None;
    }
    let host = if raw.starts_with('[') {
        raw.split(']').next().map(|host| format!("{host}]"))?
    } else {
        raw.split(':').next().unwrap_or(raw).to_string()
    };
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn default_transfer_min_size() -> u64 {
    10_245_760
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig, ServiceIdentity,
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
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: config_dir.path().join("external"),
            resolved_values: HashMap::new(),
            module_registry: Arc::new(ModuleRegistry::new()),
        }
    }

    #[test]
    fn virtual_host_resolves_spa_fallback_and_assets() {
        let config_dir = TempDir::new().expect("config temp dir");
        let dist = config_dir.path().join("dist");
        std::fs::create_dir_all(dist.join("assets")).expect("create dist");
        std::fs::write(dist.join("index.html"), "<html></html>").expect("write index");
        std::fs::write(dist.join("assets/app.1234abcd.js"), "console.log(1);")
            .expect("write asset");
        std::fs::write(
            config_dir.path().join(VIRTUAL_HOST_FILE),
            r#"
hosts:
  - domain: local.localhost
    path: /
    base: dist
"#,
        )
        .expect("write virtual host config");
        let runtime = runtime_config(&config_dir);

        let resources = load_static_resources(&runtime).expect("load static resources");

        match resources.resolve_virtual_host(Some("local.localhost:8443"), "/account/settings") {
            StaticResolution::File(file) => {
                assert_eq!(file.path, dist.join("index.html"));
                assert_eq!(file.content_type, "text/html; charset=utf-8");
                assert_eq!(file.cache_control, "no-cache");
            }
            other => panic!("expected SPA fallback, got {other:?}"),
        }
        match resources.resolve_virtual_host(Some("local.localhost"), "/assets/app.1234abcd.js") {
            StaticResolution::File(file) => {
                assert_eq!(file.path, dist.join("assets/app.1234abcd.js"));
                assert_eq!(file.cache_control, "public, max-age=31536000, immutable");
            }
            other => panic!("expected static asset, got {other:?}"),
        }
        assert_eq!(
            resources.resolve_virtual_host(Some("local.localhost"), "/assets/missing.js"),
            StaticResolution::NotFound
        );
    }

    #[test]
    fn static_resolution_blocks_traversal_and_dotfiles() {
        let site = StaticSite {
            path: "/".to_string(),
            base: PathBuf::from("/tmp/static"),
            prefix: true,
            transfer_min_size: default_transfer_min_size(),
            directory_listing_enabled: false,
        };

        assert_eq!(site.resolve("/../secret.txt"), StaticResolution::Forbidden);
        assert_eq!(site.resolve("/.env"), StaticResolution::Forbidden);
    }

    #[test]
    fn path_resource_yml_can_be_disabled_by_empty_template_defaults() {
        let config_dir = TempDir::new().expect("config temp dir");
        std::fs::write(
            config_dir.path().join(PATH_RESOURCE_FILE),
            "path: ${path-resource.path:}\nbase: ${path-resource.base:}\n",
        )
        .expect("write path resource config");
        let runtime = runtime_config(&config_dir);

        let resources = load_static_resources(&runtime).expect("load static resources");

        assert!(resources.path_resource.is_none());
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == PATH_RESOURCE_MODULE_ID && !entry.active)
        );
    }
}
