use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use config_loader::ConfigLoader;
use portal_registry::{
    PortalRegistryClient, RegistrationBuilder, RegistrationState, RegistryHandler,
};
use serde::de::DeserializeOwned;
use serde_yaml::Value;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{info, warn};
use url::Url;

use crate::config::{
    BootstrapConfig, ClientConfig, PortalRegistryConfig, RemoteBootstrapResult, RuntimeConfig,
    ServerConfig, ServiceIdentity, default_accept_header, default_environment,
};
use crate::transport::{BoundTransport, TransportRuntime};

const CONFIG_SERVER_CONFIGS_CONTEXT_ROOT: &str = "/config-server/configs";
const CONFIG_SERVER_CERTS_CONTEXT_ROOT: &str = "/config-server/certs";
const CONFIG_SERVER_FILES_CONTEXT_ROOT: &str = "/config-server/files";
const STARTUP_FILE: &str = "startup.yml";
const VALUES_FILE: &str = "values.yml";
const CLIENT_FILE: &str = "client.yml";
const SERVER_FILE: &str = "server.yml";
const PORTAL_REGISTRY_FILE: &str = "portal-registry.yml";
const CONFIG_PASSWORD_ENV: &str = "light_4j_config_password";
const LIGHT_ENV_ENV: &str = "light-env";
const CONFIG_SERVER_URI_ENV: &str = "light-config-server-uri";
const PORTAL_AUTH_ENV: &str = "light_portal_authorization";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    BootstrapLocal,
    BootstrapRemoteOrFallback,
    BuildRuntime,
    BindListeners,
    RegisterController,
    Ready,
    Stopped,
}

#[derive(Debug, Clone, Copy)]
pub struct RegistrationPolicy {
    pub enabled: bool,
    pub start_on_failure: bool,
}

#[async_trait]
pub trait Module: Send + Sync {
    fn name(&self) -> &'static str;

    async fn on_runtime_built(&self, _config: &RuntimeConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn on_server_bound(&self, _config: &RuntimeConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn on_ready(&self, _config: &RuntimeConfig) -> Result<(), RuntimeError> {
        Ok(())
    }

    async fn on_shutdown(&self, _config: &RuntimeConfig) -> Result<(), RuntimeError> {
        Ok(())
    }
}

pub struct LightRuntimeBuilder<T>
where
    T: TransportRuntime,
{
    transport: T,
    config_dir: PathBuf,
    external_config_dir: Option<PathBuf>,
    modules: Vec<Arc<dyn Module>>,
    registration_timeout: Duration,
    registry_handler: Arc<dyn RegistryHandler>,
}

impl<T> LightRuntimeBuilder<T>
where
    T: TransportRuntime,
{
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            config_dir: PathBuf::from("config"),
            external_config_dir: None,
            modules: Vec::new(),
            registration_timeout: Duration::from_secs(5),
            registry_handler: Arc::new(NoopRegistryHandler),
        }
    }

    pub fn with_config_dir(mut self, config_dir: impl Into<PathBuf>) -> Self {
        self.config_dir = config_dir.into();
        self
    }

    pub fn with_external_config_dir(mut self, external_config_dir: impl Into<PathBuf>) -> Self {
        self.external_config_dir = Some(external_config_dir.into());
        self
    }

    pub fn with_module(mut self, module: Arc<dyn Module>) -> Self {
        self.modules.push(module);
        self
    }

    pub fn with_registration_timeout(mut self, registration_timeout: Duration) -> Self {
        self.registration_timeout = registration_timeout;
        self
    }

    pub fn with_registry_handler(mut self, handler: Arc<dyn RegistryHandler>) -> Self {
        self.registry_handler = handler;
        self
    }

    pub fn build(self) -> LightRuntime<T> {
        LightRuntime {
            transport: self.transport,
            config_dir: self.config_dir,
            external_config_dir: self.external_config_dir,
            modules: self.modules,
            registration_timeout: self.registration_timeout,
            registry_handler: self.registry_handler,
            state: LifecycleState::BootstrapLocal,
        }
    }
}

pub struct LightRuntime<T>
where
    T: TransportRuntime,
{
    transport: T,
    config_dir: PathBuf,
    external_config_dir: Option<PathBuf>,
    modules: Vec<Arc<dyn Module>>,
    registration_timeout: Duration,
    registry_handler: Arc<dyn RegistryHandler>,
    state: LifecycleState,
}

pub struct RunningRuntime<T>
where
    T: TransportRuntime,
{
    pub state: LifecycleState,
    pub config: RuntimeConfig,
    pub transport: BoundTransport<T::Handle>,
    transport_runtime: T,
    registration_task: Option<JoinHandle<()>>,
    modules: Vec<Arc<dyn Module>>,
}

impl<T> RunningRuntime<T>
where
    T: TransportRuntime,
{
    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        if let Some(task) = self.registration_task.take() {
            task.abort();
        }

        self.transport_runtime
            .stop(&mut self.transport.handle)
            .await?;

        for module in &self.modules {
            module.on_shutdown(&self.config).await?;
        }

        Ok(())
    }
}

impl<T> LightRuntime<T>
where
    T: TransportRuntime,
{
    pub async fn start(mut self) -> Result<RunningRuntime<T>, RuntimeError> {
        self.state = LifecycleState::BootstrapLocal;
        let (bootstrap, bootstrap_client) = self.load_bootstrap_config()?;
        let external_config_dir = self.resolve_external_config_dir(&bootstrap);

        self.state = LifecycleState::BootstrapRemoteOrFallback;
        let remote_result = self
            .bootstrap_remote_if_needed(&bootstrap, bootstrap_client.as_ref(), &external_config_dir)
            .await?;

        self.state = LifecycleState::BuildRuntime;
        let runtime_config =
            self.build_runtime_config(bootstrap, bootstrap_client, external_config_dir, remote_result)?;

        for module in &self.modules {
            module.on_runtime_built(&runtime_config).await?;
        }

        self.state = LifecycleState::BindListeners;
        let transport = self.transport.bind(&runtime_config).await?;

        for module in &self.modules {
            module.on_server_bound(&runtime_config).await?;
        }

        self.state = LifecycleState::RegisterController;
        let registration_task = match self
            .register_controller_if_needed(&runtime_config, &transport.metadata)
            .await
        {
            Ok(task) => task,
            Err(error) => {
                let mut transport_handle = transport.handle;
                self.transport.stop(&mut transport_handle).await?;
                return Err(error);
            }
        };

        self.state = LifecycleState::Ready;
        for module in &self.modules {
            module.on_ready(&runtime_config).await?;
        }

        Ok(RunningRuntime {
            state: self.state,
            config: runtime_config,
            transport: BoundTransport {
                handle: transport.handle,
                metadata: transport.metadata,
            },
            transport_runtime: self.transport,
            registration_task,
            modules: self.modules,
        })
    }

    fn load_bootstrap_config(&self) -> Result<(BootstrapConfig, Option<ClientConfig>), RuntimeError> {
        let values = load_bootstrap_values(&self.config_dir)?;
        let password = std::env::var(CONFIG_PASSWORD_ENV).ok();
        let loader = ConfigLoader::from_values(values, password.as_deref(), None)?;

        let startup_path = self.config_dir.join(STARTUP_FILE);
        let mut config = if startup_path.exists() {
            self.load_typed_config::<BootstrapConfig>(&loader, STARTUP_FILE)?
        } else {
            BootstrapConfig::default()
        };

        if config.accept_header.is_empty() {
            config.accept_header = default_accept_header();
        }
        if config.env_tag.is_none() {
            config.env_tag = std::env::var(LIGHT_ENV_ENV).ok();
        }
        if config.config_server_uri.is_none() {
            config.config_server_uri = std::env::var(CONFIG_SERVER_URI_ENV).ok();
        }
        if config.authorization.is_none() {
            config.authorization = std::env::var(PORTAL_AUTH_ENV).ok();
        }

        let client_config = self.try_load_typed_config::<ClientConfig>(&loader, CLIENT_FILE)?;

        Ok((config, client_config))
    }

    fn resolve_external_config_dir(&self, bootstrap: &BootstrapConfig) -> PathBuf {
        self.external_config_dir
            .clone()
            .or_else(|| bootstrap.external_config_dir.clone())
            .unwrap_or_else(|| self.config_dir.clone())
    }

    async fn bootstrap_remote_if_needed(
        &self,
        bootstrap: &BootstrapConfig,
        client_config: Option<&ClientConfig>,
        external_config_dir: &Path,
    ) -> Result<RemoteBootstrapResult, RuntimeError> {
        let Some(config_server_uri) = bootstrap.config_server_uri.as_deref() else {
            return Ok(RemoteBootstrapResult::default());
        };

        fs::create_dir_all(external_config_dir)?;
        let client = build_config_server_client(bootstrap, client_config)?;
        let query = build_query_params(bootstrap);

        match fetch_remote_values(&client, config_server_uri, &query, bootstrap).await {
            Ok(values_yaml) => {
                let values_path = external_config_dir.join(VALUES_FILE);
                fs::write(&values_path, values_yaml.as_bytes())?;

                let mut result = RemoteBootstrapResult {
                    values_yaml: Some(values_yaml),
                    cached_files: vec![values_path],
                };

                for context_root in [
                    CONFIG_SERVER_CERTS_CONTEXT_ROOT,
                    CONFIG_SERVER_FILES_CONTEXT_ROOT,
                ] {
                    let files = fetch_remote_files(
                        &client,
                        config_server_uri,
                        context_root,
                        &query,
                        bootstrap,
                        external_config_dir,
                    )
                    .await?;
                    result.cached_files.extend(files);
                }

                Ok(result)
            }
            Err(error) => {
                if external_config_dir.join(VALUES_FILE).exists() {
                    warn!(
                        "remote bootstrap failed; continuing with cached local config: {:?}",
                        error
                    );
                    Ok(RemoteBootstrapResult::default())
                } else {
                    Err(error)
                }
            }
        }
    }

    fn build_runtime_config(
        &self,
        bootstrap: BootstrapConfig,
        client: Option<ClientConfig>,
        external_config_dir: PathBuf,
        remote_result: RemoteBootstrapResult,
    ) -> Result<RuntimeConfig, RuntimeError> {
        let values = load_values_map(
            &self.config_dir,
            &external_config_dir,
            remote_result.values_yaml,
        )?;
        let password = std::env::var(CONFIG_PASSWORD_ENV).ok();
        let loader = ConfigLoader::from_values(values, password.as_deref(), None)?;

        let server = self.load_typed_config::<ServerConfig>(&loader, SERVER_FILE)?;
        let client = match client {
            Some(c) => Some(c),
            None => self.try_load_typed_config::<ClientConfig>(&loader, CLIENT_FILE)?,
        };
        let portal_registry =
            self.try_load_typed_config::<PortalRegistryConfig>(&loader, PORTAL_REGISTRY_FILE)?;
        let env_tag = bootstrap
            .env_tag
            .clone()
            .or_else(|| (!server.environment.is_empty()).then(|| server.environment.clone()));
        let service_identity = ServiceIdentity {
            service_id: bootstrap
                .service_id
                .clone()
                .unwrap_or_else(|| server.service_id.clone()),
            version: derive_service_version(
                bootstrap
                    .service_id
                    .as_deref()
                    .unwrap_or(server.service_id.as_str()),
            ),
            env_tag,
            tags: HashMap::new(),
        };

        Ok(RuntimeConfig {
            bootstrap,
            server,
            client,
            portal_registry,
            service_identity,
            config_dir: self.config_dir.clone(),
            external_config_dir,
        })
    }

    fn load_typed_config<V>(
        &self,
        loader: &ConfigLoader,
        file_name: &str,
    ) -> Result<V, RuntimeError>
    where
        V: DeserializeOwned,
    {
        let mut paths = Vec::new();
        let base_path = self.config_dir.join(file_name);
        if base_path.exists() {
            paths.push(base_path);
        }
        let overlay_path = self
            .external_config_dir
            .clone()
            .unwrap_or_else(|| self.config_dir.clone())
            .join(file_name);
        if overlay_path.exists() && !paths.iter().any(|path| path == &overlay_path) {
            paths.push(overlay_path);
        }

        if paths.is_empty() {
            return Err(RuntimeError::MissingConfig(file_name.to_string()));
        }

        let merged = loader.load_merged_files(paths.iter().map(PathBuf::as_path))?;
        let parsed = serde_yaml::from_value(merged)?;
        Ok(parsed)
    }

    fn try_load_typed_config<V>(
        &self,
        loader: &ConfigLoader,
        file_name: &str,
    ) -> Result<Option<V>, RuntimeError>
    where
        V: DeserializeOwned,
    {
        let base_path = self.config_dir.join(file_name);
        let overlay_path = self
            .external_config_dir
            .clone()
            .unwrap_or_else(|| self.config_dir.clone())
            .join(file_name);

        if !base_path.exists() && !overlay_path.exists() {
            return Ok(None);
        }

        self.load_typed_config(loader, file_name).map(Some)
    }

    async fn register_controller_if_needed(
        &self,
        runtime_config: &RuntimeConfig,
        metadata: &crate::transport::ResolvedServerMetadata,
    ) -> Result<Option<JoinHandle<()>>, RuntimeError> {
        let policy = RegistrationPolicy {
            enabled: runtime_config.server.enable_registry,
            start_on_failure: runtime_config.server.start_on_registry_failure,
        };
        if !policy.enabled {
            return Ok(None);
        }

        let portal_registry = runtime_config
            .portal_registry
            .clone()
            .ok_or_else(|| RuntimeError::MissingConfig(PORTAL_REGISTRY_FILE.to_string()))?;
        let portal_url = Url::parse(&portal_registry.portal_url)?;
        let ws_url = to_microservice_ws_url(&portal_url)?;
        let token = portal_token(&portal_registry).ok_or(RuntimeError::MissingPortalToken)?;
        let registration = RegistrationBuilder::new(
            &runtime_config.service_identity.service_id,
            &runtime_config.service_identity.version,
            &metadata.protocol,
            &metadata.address,
            metadata.port,
        )
        .with_jwt(&token)
        .build();

        let client = Arc::new(
            PortalRegistryClient::new(&ws_url, registration, Arc::clone(&self.registry_handler))
                .map_err(|e| {
                    RuntimeError::Unsupported(format!(
                        "failed to build portal registry client: {e}"
                    ))
                })?,
        );
        let mut registration_rx = client.subscribe_registration();
        let task_client = Arc::clone(&client);
        let registration_task = tokio::spawn(async move {
            task_client.run().await;
        });

        let wait_for_registration = async {
            loop {
                let current = registration_rx.borrow().clone();
                if matches!(current, RegistrationState::Registered { .. }) {
                    return Ok::<(), RuntimeError>(());
                }

                registration_rx
                    .changed()
                    .await
                    .map_err(|_| RuntimeError::RegistrationChannelClosed)?;
            }
        };

        match timeout(self.registration_timeout, wait_for_registration).await {
            Ok(result) => result?,
            Err(_) => {
                if policy.start_on_failure {
                    warn!("controller registration timed out; continuing with background retries");
                } else {
                    registration_task.abort();
                    return Err(RuntimeError::RegistrationTimeout(self.registration_timeout));
                }
            }
        }

        info!(
            "controller registration enabled for {}",
            runtime_config.service_identity.service_id
        );
        Ok(Some(registration_task))
    }
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("config error: {0}")]
    Config(String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),
    #[error("missing required config file `{0}`")]
    MissingConfig(String),
    #[error("missing portal registration token")]
    MissingPortalToken,
    #[error("registration timed out after {0:?}")]
    RegistrationTimeout(Duration),
    #[error("registration channel closed unexpectedly")]
    RegistrationChannelClosed,
    #[error("transport runtime does not support this configuration: {0}")]
    Unsupported(String),
}

impl From<config_loader::ConfigError> for RuntimeError {
    fn from(value: config_loader::ConfigError) -> Self {
        Self::Config(value.to_string())
    }
}

struct NoopRegistryHandler;

#[async_trait]
impl RegistryHandler for NoopRegistryHandler {}

fn build_config_server_client(
    bootstrap: &BootstrapConfig,
    client_config: Option<&ClientConfig>,
) -> Result<reqwest::Client, RuntimeError> {
    let mut client_builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(bootstrap.connect_timeout))
        .timeout(Duration::from_millis(bootstrap.timeout));

    if let Some(client) = client_config {
        if !client.verify_hostname {
            warn!(
                "TLS hostname verification is disabled for the config-server client; this weakens server identity validation"
            );
            client_builder = client_builder.danger_accept_invalid_hostnames(true);
        }
    }

    if let Some(ca_cert_path) = &bootstrap.bootstrap_ca_cert_path {
        let cert = fs::read(ca_cert_path)?;
        let ca = reqwest::Certificate::from_pem(&cert)
            .map_err(|e| RuntimeError::Unsupported(format!("invalid bootstrap CA cert: {e}")))?;
        client_builder = client_builder.add_root_certificate(ca);
    }

    client_builder.build().map_err(RuntimeError::Http)
}

fn build_query_params(bootstrap: &BootstrapConfig) -> Vec<(String, String)> {
    let mut params = Vec::new();
    params.push(("host".to_string(), bootstrap.host.clone()));

    if let Some(value) = &bootstrap.service_id {
        params.push(("serviceId".to_string(), value.clone()));
    }
    if let Some(value) = &bootstrap.product_id {
        params.push(("productId".to_string(), value.clone()));
    }
    if let Some(value) = &bootstrap.product_version {
        params.push(("productVersion".to_string(), value.clone()));
    }
    if let Some(value) = &bootstrap.api_id {
        params.push(("apiId".to_string(), value.clone()));
    }
    if let Some(value) = &bootstrap.api_version {
        params.push(("apiVersion".to_string(), value.clone()));
    }

    params.push((
        "envTag".to_string(),
        bootstrap
            .env_tag
            .clone()
            .unwrap_or_else(default_environment),
    ));
    params
}

async fn fetch_remote_values(
    client: &reqwest::Client,
    config_server_uri: &str,
    query: &[(String, String)],
    bootstrap: &BootstrapConfig,
) -> Result<String, RuntimeError> {
    let response = client
        .get(format!(
            "{config_server_uri}{CONFIG_SERVER_CONFIGS_CONTEXT_ROOT}"
        ))
        .query(query)
        .header(reqwest::header::ACCEPT, bootstrap.accept_header.clone())
        .header(
            reqwest::header::AUTHORIZATION,
            bootstrap.authorization.clone().unwrap_or_default(),
        )
        .send()
        .await?;

    let response = response.error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response.text().await?;

    if content_type.starts_with("application/yaml") || content_type.starts_with("text/yaml") {
        Ok(body)
    } else if content_type.starts_with("application/json") {
        let json: serde_json::Value = serde_json::from_str(&body)?;
        Ok(serde_yaml::to_string(&json)?)
    } else {
        Err(RuntimeError::Unsupported(format!(
            "unsupported config server content type `{content_type}`"
        )))
    }
}

async fn fetch_remote_files(
    client: &reqwest::Client,
    config_server_uri: &str,
    context_root: &str,
    query: &[(String, String)],
    bootstrap: &BootstrapConfig,
    external_config_dir: &Path,
) -> Result<Vec<PathBuf>, RuntimeError> {
    let response = client
        .get(format!("{config_server_uri}{context_root}"))
        .query(query)
        .header(
            reqwest::header::AUTHORIZATION,
            bootstrap.authorization.clone().unwrap_or_default(),
        )
        .send()
        .await?;

    if response.status().as_u16() == 404 {
        return Ok(Vec::new());
    }

    let response = response.error_for_status()?;
    let body = response.text().await?;
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    let files: HashMap<String, String> = serde_json::from_str(&body)?;
    let mut cached_files = Vec::new();
    for (file_name, encoded_content) in files {
        let path = external_config_dir.join(file_name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = BASE64
            .decode(encoded_content.as_bytes())
            .map_err(|e| RuntimeError::Unsupported(format!("invalid base64 file payload: {e}")))?;
        fs::write(&path, content)?;
        cached_files.push(path);
    }

    Ok(cached_files)
}

fn load_bootstrap_values(config_dir: &Path) -> Result<HashMap<String, Value>, RuntimeError> {
    let values_path = config_dir.join(VALUES_FILE);
    if !values_path.exists() {
        return Ok(HashMap::new());
    }

    let content = fs::read_to_string(values_path)?;
    let parsed: HashMap<String, Value> = serde_yaml::from_str(&content)?;
    Ok(parsed)
}

fn load_values_map(
    config_dir: &Path,
    external_config_dir: &Path,
    remote_values_yaml: Option<String>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let mut values = HashMap::new();

    for path in [
        config_dir.join(VALUES_FILE),
        external_config_dir.join(VALUES_FILE),
    ] {
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let parsed: HashMap<String, Value> = serde_yaml::from_str(&content)?;
            values.extend(parsed);
        }
    }

    if let Some(remote_values_yaml) = remote_values_yaml {
        let parsed: HashMap<String, Value> = serde_yaml::from_str(&remote_values_yaml)?;
        values.extend(parsed);
    }

    Ok(values)
}

fn derive_service_version(service_id: &str) -> String {
    service_id
        .rsplit_once('-')
        .and_then(|(_, suffix)| {
            if suffix.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
                Some(suffix.to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn to_microservice_ws_url(portal_url: &Url) -> Result<String, RuntimeError> {
    let mut ws_url = portal_url.clone();
    let scheme = match portal_url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => {
            return Err(RuntimeError::Unsupported(format!(
                "unsupported portal URL scheme `{other}`"
            )));
        }
    };
    ws_url
        .set_scheme(scheme)
        .map_err(|_| RuntimeError::Unsupported("failed to convert portal URL".to_string()))?;
    ws_url.set_path("/ws/microservice");
    ws_url.set_query(None);
    Ok(ws_url.to_string())
}

fn portal_token(config: &PortalRegistryConfig) -> Option<String> {
    if !config.portal_token.trim().is_empty() {
        Some(strip_bearer_prefix(&config.portal_token))
    } else {
        std::env::var(PORTAL_AUTH_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| strip_bearer_prefix(&value))
    }
}

fn strip_bearer_prefix(token: &str) -> String {
    token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tempfile::TempDir;
    use crate::transport::{BoundTransport, ResolvedServerMetadata, TransportRuntime};

    struct NoopTransport;

    #[async_trait]
    impl TransportRuntime for NoopTransport {
        type Handle = ();

        async fn bind(
            &self,
            _config: &RuntimeConfig,
        ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
            Ok(BoundTransport {
                handle: (),
                metadata: ResolvedServerMetadata::default(),
            })
        }

        async fn stop(&self, _handle: &mut Self::Handle) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    #[test]
    fn builds_light_4j_style_query_parameters() {
        let bootstrap = BootstrapConfig {
            host: "lightapi.net".to_string(),
            service_id: Some("com.networknt.petstore-1.0.0".to_string()),
            product_id: Some("agent".to_string()),
            product_version: Some("1.0.0".to_string()),
            api_id: Some("petstore".to_string()),
            api_version: Some("1.0.0".to_string()),
            env_tag: Some("dev".to_string()),
            ..BootstrapConfig::default()
        };

        let query = build_query_params(&bootstrap);
        assert!(query.contains(&(
            "serviceId".to_string(),
            "com.networknt.petstore-1.0.0".to_string()
        )));
        assert!(query.contains(&("productId".to_string(), "agent".to_string())));
        assert!(query.contains(&("productVersion".to_string(), "1.0.0".to_string())));
        assert!(query.contains(&("apiId".to_string(), "petstore".to_string())));
        assert!(query.contains(&("apiVersion".to_string(), "1.0.0".to_string())));
        assert!(query.contains(&("envTag".to_string(), "dev".to_string())));
    }

    #[test]
    fn merges_values_from_local_and_external_dirs() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(config_dir.path().join(VALUES_FILE), "a: 1\nb: local\n")
            .expect("write local values");
        fs::write(
            external_dir.path().join(VALUES_FILE),
            "b: remote\nc: true\n",
        )
        .expect("write external values");

        let values =
            load_values_map(config_dir.path(), external_dir.path(), None).expect("values map");

        assert_eq!(values["a"], Value::Number(1.into()));
        assert_eq!(values["b"], Value::String("remote".to_string()));
        assert_eq!(values["c"], Value::Bool(true));
    }

    #[test]
    fn derives_service_version_from_service_id_suffix() {
        assert_eq!(
            derive_service_version("com.networknt.petstore-1.0.0"),
            "1.0.0".to_string()
        );
    }

    #[test]
    fn falls_back_to_package_version_when_service_id_has_no_suffix() {
        assert_eq!(
            derive_service_version("com.networknt.petstore"),
            env!("CARGO_PKG_VERSION").to_string()
        );
    }

    #[test]
    fn load_bootstrap_config_reads_client_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        fs::write(
            config_dir.path().join(CLIENT_FILE),
            "verifyHostname: false\n",
        )
        .expect("write client config");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build();

        let (_bootstrap, client_config) = runtime.load_bootstrap_config().expect("bootstrap config");

        assert_eq!(client_config.map(|c| c.verify_hostname), Some(false));
    }
}
