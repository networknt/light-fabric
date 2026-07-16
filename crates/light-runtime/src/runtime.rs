use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use config_loader::{
    ConfigLoader, EmbeddedConfigFile, embedded_config_file, existing_config_paths,
    load_config_from_sources, load_values_from_sources,
};
use portal_registry::{
    ControlCandidate, PortalRegistryClient, RegistrationBuilder, RegistrationState, RegistryHandler,
};
use serde::de::DeserializeOwned;
use serde_yaml::Value;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tracing::{error, info, warn};
use url::Url;

use crate::cache::CacheRegistry;
use crate::config::{
    BootstrapConfig, ClientConfig, ControlCandidateConfig, DirectRegistryConfig,
    PortalRegistryConfig, RemoteBootstrapResult, RuntimeConfig, ServerConfig, ServiceIdentity,
    default_accept_header, default_environment,
};
use crate::logging::{
    LogFileAccess, LogStreamBroadcaster, LoggingControl, register_logging_module,
};
use crate::module_registry::{ModuleRegistry, ReloadContext, RuntimeMcpHandler};
use crate::transport::{BoundTransport, TransportRuntime};

const CONFIG_SERVER_CONFIGS_CONTEXT_ROOT: &str = "/config-server/configs";
const CONFIG_SERVER_CERTS_CONTEXT_ROOT: &str = "/config-server/certs";
const CONFIG_SERVER_FILES_CONTEXT_ROOT: &str = "/config-server/files";
const STARTUP_FILE: &str = "startup.yml";
const VALUES_FILE: &str = "values.yml";
const CLIENT_FILE: &str = "client.yml";
const SERVER_FILE: &str = "server.yml";
const PORTAL_REGISTRY_FILE: &str = "portal-registry.yml";
const DIRECT_REGISTRY_FILE: &str = "direct-registry.yml";
const DIRECT_REGISTRY_DIRECT_URLS_KEY: &str = "direct-registry.directUrls";
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
    default_config_dir: Option<PathBuf>,
    embedded_config: &'static [EmbeddedConfigFile],
    modules: Vec<Arc<dyn Module>>,
    module_registry: Arc<ModuleRegistry>,
    cache_registry: Option<Arc<CacheRegistry>>,
    logging_control: Option<Arc<LoggingControl>>,
    log_stream: Option<Arc<LogStreamBroadcaster>>,
    log_file_access: Option<Arc<LogFileAccess>>,
    registration_timeout: Duration,
    registry_handler: Arc<dyn RegistryHandler>,
    registry_client: Option<Arc<PortalRegistryClient>>,
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
            default_config_dir: None,
            embedded_config: &[],
            modules: Vec::new(),
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            logging_control: None,
            log_stream: None,
            log_file_access: None,
            registration_timeout: Duration::from_secs(5),
            registry_handler: Arc::new(NoopRegistryHandler),
            registry_client: None,
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

    pub fn with_default_config_dir(mut self, default_config_dir: impl Into<PathBuf>) -> Self {
        self.default_config_dir = Some(default_config_dir.into());
        self
    }

    pub fn with_embedded_config(mut self, embedded_config: &'static [EmbeddedConfigFile]) -> Self {
        self.embedded_config = embedded_config;
        self
    }

    pub fn with_module(mut self, module: Arc<dyn Module>) -> Self {
        self.modules.push(module);
        self
    }

    pub fn with_module_registry(mut self, module_registry: Arc<ModuleRegistry>) -> Self {
        self.module_registry = module_registry;
        self
    }

    pub fn with_cache_registry(mut self, cache_registry: Arc<CacheRegistry>) -> Self {
        self.cache_registry = Some(cache_registry);
        self
    }

    pub fn with_logging_control(mut self, logging_control: Arc<LoggingControl>) -> Self {
        self.logging_control = Some(logging_control);
        self
    }

    pub fn with_log_stream(mut self, log_stream: Arc<LogStreamBroadcaster>) -> Self {
        self.log_stream = Some(log_stream);
        self
    }

    pub fn with_log_file_access(mut self, log_file_access: Arc<LogFileAccess>) -> Self {
        self.log_file_access = Some(log_file_access);
        self
    }

    pub fn with_optional_log_file_access(
        mut self,
        log_file_access: Option<Arc<LogFileAccess>>,
    ) -> Self {
        self.log_file_access = log_file_access;
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

    pub fn with_registry_client(mut self, client: Arc<PortalRegistryClient>) -> Self {
        self.registry_client = Some(client);
        self
    }

    pub fn build(self) -> LightRuntime<T> {
        LightRuntime {
            transport: self.transport,
            config_dir: self.config_dir,
            external_config_dir: self.external_config_dir,
            default_config_dir: self.default_config_dir,
            embedded_config: self.embedded_config,
            modules: self.modules,
            module_registry: self.module_registry,
            cache_registry: self.cache_registry,
            logging_control: self.logging_control,
            log_stream: self.log_stream,
            log_file_access: self.log_file_access,
            registration_timeout: self.registration_timeout,
            registry_handler: self.registry_handler,
            registry_client: self.registry_client,
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
    default_config_dir: Option<PathBuf>,
    embedded_config: &'static [EmbeddedConfigFile],
    modules: Vec<Arc<dyn Module>>,
    module_registry: Arc<ModuleRegistry>,
    cache_registry: Option<Arc<CacheRegistry>>,
    logging_control: Option<Arc<LoggingControl>>,
    log_stream: Option<Arc<LogStreamBroadcaster>>,
    log_file_access: Option<Arc<LogFileAccess>>,
    registration_timeout: Duration,
    registry_handler: Arc<dyn RegistryHandler>,
    registry_client: Option<Arc<PortalRegistryClient>>,
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
    pub module_registry: Arc<ModuleRegistry>,
    pub cache_registry: Option<Arc<CacheRegistry>>,
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

impl RuntimeConfig {
    pub async fn reload_context(&self) -> Result<ReloadContext, RuntimeError> {
        let remote_result = fetch_remote_bootstrap_if_needed(
            &self.bootstrap,
            self.client.as_ref(),
            &self.external_config_dir,
        )
        .await?;
        let values = load_values_from_sources(
            self.embedded_config,
            self.default_config_dir.as_deref(),
            &self.config_dir,
            Some(&self.external_config_dir),
            remote_result.values_yaml.as_deref(),
        )?;
        let mut runtime_config = self.clone();
        if let Some(value) = values.get(DIRECT_REGISTRY_DIRECT_URLS_KEY) {
            runtime_config.direct_registry = DirectRegistryConfig {
                direct_urls: parse_direct_registry_urls_value(value)?,
            };
        }
        validate_direct_registry_config(&runtime_config.direct_registry)?;
        runtime_config.resolved_values = values;

        if runtime_config.client.is_some() {
            let password = std::env::var(CONFIG_PASSWORD_ENV).ok();
            let loader = ConfigLoader::from_values(
                runtime_config.resolved_values.clone(),
                password.as_deref(),
                None,
            )?;
            if let Some(merged) = load_config_from_sources(
                &loader,
                runtime_config.embedded_config,
                runtime_config.default_config_dir.as_deref(),
                &runtime_config.config_dir,
                Some(&runtime_config.external_config_dir),
                CLIENT_FILE,
            )? {
                let client = serde_yaml::from_value::<ClientConfig>(merged)?;
                runtime_config.client = Some(client);
            }
        }

        Ok(ReloadContext::new(runtime_config))
    }
}

impl<T> LightRuntime<T>
where
    T: TransportRuntime,
{
    pub async fn start(mut self) -> Result<RunningRuntime<T>, RuntimeError> {
        init_rustls_provider();
        self.state = LifecycleState::BootstrapLocal;
        let (bootstrap, bootstrap_client) = self.load_bootstrap_config()?;
        let external_config_dir = self.resolve_external_config_dir(&bootstrap);

        self.state = LifecycleState::BootstrapRemoteOrFallback;
        let remote_result = self
            .bootstrap_remote_if_needed(&bootstrap, bootstrap_client.as_ref(), &external_config_dir)
            .await?;

        self.state = LifecycleState::BuildRuntime;
        let runtime_config = self.build_runtime_config(
            bootstrap,
            bootstrap_client,
            external_config_dir,
            remote_result,
        )?;
        self.module_registry
            .register_runtime_configs(&runtime_config)?;
        if let Some(logging_control) = self.logging_control.as_ref() {
            register_logging_module(
                &self.module_registry,
                &runtime_config,
                Arc::clone(logging_control),
            )?;
        }

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
            module_registry: self.module_registry,
            cache_registry: self.cache_registry,
        })
    }

    fn load_bootstrap_config(
        &self,
    ) -> Result<(BootstrapConfig, Option<ClientConfig>), RuntimeError> {
        let values = load_bootstrap_values(
            self.embedded_config,
            self.default_config_dir.as_deref(),
            &self.config_dir,
        )?;
        let password = std::env::var(CONFIG_PASSWORD_ENV).ok();
        let loader = ConfigLoader::from_values(values, password.as_deref(), None)?;

        let mut config = self
            .try_load_bootstrap_typed_config::<BootstrapConfig>(&loader, STARTUP_FILE)?
            .unwrap_or_default();

        if config.accept_header.is_empty() {
            config.accept_header = default_accept_header();
        }
        if config.env_tag.is_none() {
            config.env_tag = get_env_value(LIGHT_ENV_ENV);
        }
        if config.config_server_uri.is_none() {
            config.config_server_uri = get_env_value(CONFIG_SERVER_URI_ENV);
        }
        if let Some(env_authorization) = get_env_value(PORTAL_AUTH_ENV)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        {
            config.authorization = Some(env_authorization);
        }

        let client_config =
            self.try_load_bootstrap_typed_config::<ClientConfig>(&loader, CLIENT_FILE)?;

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
        fetch_remote_bootstrap_if_needed(bootstrap, client_config, external_config_dir).await
    }

    fn build_runtime_config(
        &self,
        bootstrap: BootstrapConfig,
        _client: Option<ClientConfig>,
        external_config_dir: PathBuf,
        remote_result: RemoteBootstrapResult,
    ) -> Result<RuntimeConfig, RuntimeError> {
        let values = load_values_from_sources(
            self.embedded_config,
            self.default_config_dir.as_deref(),
            &self.config_dir,
            Some(&external_config_dir),
            remote_result.values_yaml.as_deref(),
        )?;
        let password = std::env::var(CONFIG_PASSWORD_ENV).ok();
        let loader = ConfigLoader::from_values(values.clone(), password.as_deref(), None)?;

        let server = self.load_typed_config::<ServerConfig>(&loader, SERVER_FILE)?;
        let client = self.try_load_typed_config::<ClientConfig>(&loader, CLIENT_FILE)?;
        let portal_registry =
            self.try_load_typed_config::<PortalRegistryConfig>(&loader, PORTAL_REGISTRY_FILE)?;
        let direct_registry = self.load_direct_registry_config(&loader, &values)?;
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
        let registry_client = self.build_registry_client_for_runtime(
            &bootstrap,
            &server,
            &client,
            &portal_registry,
            &service_identity,
        )?;

        Ok(RuntimeConfig {
            bootstrap,
            server,
            client,
            portal_registry,
            direct_registry,
            service_identity,
            config_dir: self.config_dir.clone(),
            external_config_dir,
            resolved_values: values,
            default_config_dir: self.default_config_dir.clone(),
            embedded_config: self.embedded_config,
            module_registry: Arc::clone(&self.module_registry),
            cache_registry: self.cache_registry.clone(),
            registry_client,
        })
    }

    fn build_registry_client_for_runtime(
        &self,
        bootstrap: &BootstrapConfig,
        server: &ServerConfig,
        client_config: &Option<ClientConfig>,
        portal_registry: &Option<PortalRegistryConfig>,
        service_identity: &ServiceIdentity,
    ) -> Result<Option<Arc<PortalRegistryClient>>, RuntimeError> {
        if !server.enable_registry {
            return Ok(self.registry_client.clone());
        }
        if let Some(client) = &self.registry_client {
            return Ok(Some(Arc::clone(client)));
        }

        let Some(portal_registry) = portal_registry.as_ref() else {
            return Ok(None);
        };
        let portal_url = Url::parse(&portal_registry.portal_url)?;
        let ws_url = to_microservice_ws_url(&portal_url)?;
        let token = portal_token(portal_registry).ok_or(RuntimeError::MissingPortalToken)?;
        let mut registration = RegistrationBuilder::new(
            &service_identity.service_id,
            &service_identity.version,
            "http",
            server
                .advertised_address
                .as_deref()
                .unwrap_or(server.ip.as_str()),
            0,
        )
        .with_jwt(&token);
        if let Some(env_tag) = service_identity.env_tag.as_deref() {
            registration = registration.with_env(env_tag);
        }
        let mut client =
            PortalRegistryClient::new(&ws_url, registration.build(), Arc::new(NoopRegistryHandler))
                .map_err(|e| {
                    RuntimeError::Unsupported(format!(
                        "failed to build portal registry client: {e}"
                    ))
                })?;
        client = client
            .with_control_candidates(resolve_control_candidates(portal_registry)?)
            .map_err(|e| {
                RuntimeError::Unsupported(format!("invalid portal registry controlCandidates: {e}"))
            })?;
        if let Some((_ca_cert_path, ca_certificate)) =
            read_portal_registry_ca_certificate(bootstrap, client_config.as_ref())?
        {
            client = client.with_ca_certificate(ca_certificate);
        }
        let verify_hostname = client_config
            .as_ref()
            .map(|config| config.tls.verify_hostname)
            .unwrap_or(true);
        client = client.with_verify_hostname(verify_hostname);
        Ok(Some(Arc::new(client)))
    }

    fn load_direct_registry_config(
        &self,
        loader: &ConfigLoader,
        values: &HashMap<String, Value>,
    ) -> Result<DirectRegistryConfig, RuntimeError> {
        let config = match self
            .try_load_typed_config::<DirectRegistryConfig>(loader, DIRECT_REGISTRY_FILE)?
        {
            Some(config) => config,
            None => DirectRegistryConfig {
                direct_urls: values
                    .get(DIRECT_REGISTRY_DIRECT_URLS_KEY)
                    .map(parse_direct_registry_urls_value)
                    .transpose()?
                    .unwrap_or_default(),
            },
        };
        validate_direct_registry_config(&config)?;
        Ok(config)
    }

    fn load_typed_config<V>(
        &self,
        loader: &ConfigLoader,
        file_name: &str,
    ) -> Result<V, RuntimeError>
    where
        V: DeserializeOwned,
    {
        let external_config_dir = self
            .external_config_dir
            .clone()
            .unwrap_or_else(|| self.config_dir.clone());
        let merged = load_config_from_sources(
            loader,
            self.embedded_config,
            self.default_config_dir.as_deref(),
            &self.config_dir,
            Some(&external_config_dir),
            file_name,
        )?
        .ok_or_else(|| RuntimeError::MissingConfig(file_name.to_string()))?;
        let parsed = serde_yaml::from_value(merged)?;
        Ok(parsed)
    }

    fn try_load_bootstrap_typed_config<V>(
        &self,
        loader: &ConfigLoader,
        file_name: &str,
    ) -> Result<Option<V>, RuntimeError>
    where
        V: DeserializeOwned,
    {
        let default_config_dir = self.default_config_dir.as_deref();
        let searched_paths =
            bootstrap_search_paths(default_config_dir, &self.config_dir, file_name);
        let existing_paths =
            existing_config_paths(default_config_dir, &self.config_dir, None, file_name);
        let embedded_available = embedded_config_file(self.embedded_config, file_name).is_some();
        let selected_source = existing_paths
            .last()
            .map(|path| path_for_log(path))
            .or_else(|| embedded_available.then(|| format!("embedded:{file_name}")))
            .unwrap_or_else(|| "<missing>".to_string());

        info!(
            config_file = file_name,
            working_dir = %current_dir_for_log(),
            searched_paths = ?paths_for_log(&searched_paths),
            existing_files = ?paths_for_log(&existing_paths),
            external_config_dir = %optional_path_for_log(self.external_config_dir.as_deref()),
            external_config_dir_used = false,
            embedded_available,
            selected_source = %selected_source,
            "bootstrap config source selection"
        );

        let Some(merged) = load_config_from_sources(
            loader,
            self.embedded_config,
            default_config_dir,
            &self.config_dir,
            None,
            file_name,
        )?
        else {
            return Ok(None);
        };

        let parsed = serde_yaml::from_value(merged)?;
        Ok(Some(parsed))
    }

    fn try_load_typed_config<V>(
        &self,
        loader: &ConfigLoader,
        file_name: &str,
    ) -> Result<Option<V>, RuntimeError>
    where
        V: DeserializeOwned,
    {
        let external_config_dir = self
            .external_config_dir
            .clone()
            .unwrap_or_else(|| self.config_dir.clone());
        let Some(merged) = load_config_from_sources(
            loader,
            self.embedded_config,
            self.default_config_dir.as_deref(),
            &self.config_dir,
            Some(&external_config_dir),
            file_name,
        )?
        else {
            return Ok(None);
        };

        let parsed = serde_yaml::from_value(merged)?;
        Ok(Some(parsed))
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
        let control_candidates = resolve_control_candidates(&portal_registry)?;
        let mut registration = RegistrationBuilder::new(
            &runtime_config.service_identity.service_id,
            &runtime_config.service_identity.version,
            &metadata.protocol,
            &metadata.address,
            metadata.port,
        )
        .with_jwt(&token);

        if let Some(env_tag) = runtime_config.service_identity.env_tag.as_deref() {
            registration = registration.with_env(env_tag);
        }

        let registration = registration.build();
        let ca_certificate = read_portal_registry_ca_certificate(
            &runtime_config.bootstrap,
            runtime_config.client.as_ref(),
        )?;
        let verify_hostname = runtime_config
            .client
            .as_ref()
            .map(|c| c.tls.verify_hostname)
            .unwrap_or(true);
        if !verify_hostname {
            warn!(
                "TLS hostname verification is disabled for the portal-registry client; this weakens server identity validation"
            );
        }
        let ca_cert_path = ca_certificate
            .as_ref()
            .map(|(path, _)| path.display().to_string())
            .unwrap_or_else(|| "<none>".to_string());
        info!(
            controller_url = %ws_url,
            verify_hostname,
            ca_cert_path = %ca_cert_path,
            ca_cert_configured = ca_certificate.is_some(),
            "portal-registry TLS configuration"
        );
        let ca_certificate = ca_certificate.map(|(_, certificate)| certificate);

        let shared_registry_client = runtime_config
            .registry_client
            .as_ref()
            .or(self.registry_client.as_ref());
        let client = if let Some(client) = shared_registry_client {
            client.set_registration_params(registration).await;
            client
                .set_control_candidates(control_candidates)
                .await
                .map_err(|e| {
                    RuntimeError::Unsupported(format!(
                        "invalid portal registry controlCandidates: {e}"
                    ))
                })?;
            client
                .configure_connection(&ws_url, ca_certificate, verify_hostname)
                .await
                .map_err(|e| {
                    RuntimeError::Unsupported(format!(
                        "failed to configure portal registry client: {e}"
                    ))
                })?;
            Arc::clone(client)
        } else {
            let mut client = PortalRegistryClient::new(
                &ws_url,
                registration,
                Arc::clone(&self.registry_handler),
            )
            .map_err(|e| {
                RuntimeError::Unsupported(format!("failed to build portal registry client: {e}"))
            })?;
            client = client
                .with_control_candidates(control_candidates)
                .map_err(|e| {
                    RuntimeError::Unsupported(format!(
                        "invalid portal registry controlCandidates: {e}"
                    ))
                })?;

            if let Some(ca_certificate) = ca_certificate {
                client = client.with_ca_certificate(ca_certificate);
            }
            client = client.with_verify_hostname(verify_hostname);
            Arc::new(client)
        };
        let mut runtime_handler = RuntimeMcpHandler::new(
            Arc::clone(&self.module_registry),
            runtime_config.clone(),
            Arc::clone(&self.registry_handler),
        );
        if let Some(cache_registry) = self.cache_registry.as_ref() {
            runtime_handler = runtime_handler.with_cache_registry(Arc::clone(cache_registry));
        }
        if let Some(logging_control) = self.logging_control.as_ref() {
            runtime_handler = runtime_handler.with_logging_control(Arc::clone(logging_control));
        }
        if let Some(log_stream) = self.log_stream.as_ref() {
            runtime_handler =
                runtime_handler.with_log_stream(Arc::clone(log_stream), client.notifier());
        }
        if let Some(log_file_access) = self.log_file_access.as_ref() {
            runtime_handler = runtime_handler.with_log_file_access(Arc::clone(log_file_access));
        }
        let registry_handler: Arc<dyn RegistryHandler> = Arc::new(runtime_handler);
        client.set_handler(registry_handler).await;
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

async fn fetch_remote_bootstrap_if_needed(
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
                match fetch_remote_files(
                    &client,
                    config_server_uri,
                    context_root,
                    &query,
                    bootstrap,
                    external_config_dir,
                )
                .await
                {
                    Ok(files) => result.cached_files.extend(files),
                    Err(error) => warn!(
                        context_root,
                        config_server_uri,
                        query = ?query,
                        error = ?error,
                        "remote bootstrap file download failed; continuing with available local/cache files"
                    ),
                }
            }

            Ok(result)
        }
        Err(error) => {
            error!(
                config_server_uri,
                query = ?query,
                error = ?error,
                "remote bootstrap values download failed; attempting to continue with local/cache config"
            );
            Ok(RemoteBootstrapResult::default())
        }
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
    let mut request = client_config
        .map(|client| client.request.clone())
        .unwrap_or_default();
    request.connect_timeout = bootstrap.connect_timeout;
    request.timeout = bootstrap.timeout;

    let tls = config_server_tls_config(bootstrap, client_config);
    if !tls.verify_hostname {
        warn!(
            "TLS hostname verification is disabled for the config-server client; this weakens server identity validation"
        );
    }

    light_client::build_reqwest_client(&request, &tls, light_client::EndpointOptions::default())
        .map_err(|e| RuntimeError::Unsupported(e.to_string()))
}

fn config_server_tls_config(
    bootstrap: &BootstrapConfig,
    client_config: Option<&ClientConfig>,
) -> light_client::ClientTlsConfig {
    let mut tls = client_config
        .map(|client| client.tls.clone())
        .unwrap_or_default();
    if tls
        .ca_cert_path
        .as_ref()
        .map_or(true, |path| path.as_os_str().is_empty())
    {
        tls.ca_cert_path = bootstrap.bootstrap_ca_cert_path.clone();
    }
    tls
}

fn portal_registry_ca_cert_path(
    bootstrap: &BootstrapConfig,
    client_config: Option<&ClientConfig>,
) -> Option<PathBuf> {
    client_config
        .and_then(|client| client.tls.ca_cert_path.clone())
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            bootstrap
                .bootstrap_ca_cert_path
                .clone()
                .filter(|path| !path.as_os_str().is_empty())
        })
}

fn read_portal_registry_ca_certificate(
    bootstrap: &BootstrapConfig,
    client_config: Option<&ClientConfig>,
) -> Result<Option<(PathBuf, Vec<u8>)>, RuntimeError> {
    let Some(path) = portal_registry_ca_cert_path(bootstrap, client_config) else {
        return Ok(None);
    };
    let certificate = fs::read(&path)?;
    Ok(Some((path, certificate)))
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
            config_server_authorization_header(bootstrap.authorization.as_deref()),
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
            config_server_authorization_header(bootstrap.authorization.as_deref()),
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

fn config_server_authorization_header(authorization: Option<&str>) -> String {
    let Some(authorization) = authorization else {
        return String::new();
    };
    let trimmed = authorization.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let first = parts.next().unwrap_or_default();
    let rest = parts.next();

    if first.eq_ignore_ascii_case("Bearer") {
        return rest
            .map(str::trim_start)
            .filter(|credentials| !credentials.is_empty())
            .map(|credentials| format!("Bearer {credentials}"))
            .unwrap_or_default();
    }

    if rest.is_some() {
        trimmed.to_string()
    } else {
        format!("Bearer {trimmed}")
    }
}

fn load_bootstrap_values(
    embedded_config: &[EmbeddedConfigFile],
    default_config_dir: Option<&Path>,
    config_dir: &Path,
) -> Result<HashMap<String, Value>, RuntimeError> {
    Ok(load_values_from_sources(
        embedded_config,
        default_config_dir,
        config_dir,
        None,
        None,
    )?)
}

fn bootstrap_search_paths(
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    file_name: &str,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(default_config_dir) = default_config_dir {
        paths.push(default_config_dir.join(file_name));
    }
    paths.push(config_dir.join(file_name));
    paths
}

fn current_dir_for_log() -> String {
    std::env::current_dir()
        .map(|path| path_for_log(&path))
        .unwrap_or_else(|error| format!("<unavailable: {error}>"))
}

fn optional_path_for_log(path: Option<&Path>) -> String {
    path.map(path_for_log)
        .unwrap_or_else(|| "<unset>".to_string())
}

fn path_for_log(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn paths_for_log(paths: &[PathBuf]) -> Vec<String> {
    paths.iter().map(|path| path_for_log(path)).collect()
}

#[cfg(test)]
fn load_values_map(
    embedded_config: &[EmbeddedConfigFile],
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: &Path,
    remote_values_yaml: Option<String>,
) -> Result<HashMap<String, Value>, RuntimeError> {
    Ok(load_values_from_sources(
        embedded_config,
        default_config_dir,
        config_dir,
        Some(external_config_dir),
        remote_values_yaml.as_deref(),
    )?)
}

pub(crate) fn load_merged_config(
    loader: &ConfigLoader,
    embedded_config: &[EmbeddedConfigFile],
    default_config_dir: Option<&Path>,
    config_dir: &Path,
    external_config_dir: &Path,
    file_name: &str,
) -> Result<Option<Value>, RuntimeError> {
    Ok(load_config_from_sources(
        loader,
        embedded_config,
        default_config_dir,
        config_dir,
        Some(external_config_dir),
        file_name,
    )?)
}

fn parse_direct_registry_urls_value(
    value: &Value,
) -> Result<BTreeMap<String, String>, RuntimeError> {
    match value {
        Value::Null => Ok(BTreeMap::new()),
        Value::String(raw) => {
            let raw = raw.trim();
            if raw.is_empty() {
                return Ok(BTreeMap::new());
            }
            let parsed = serde_yaml::from_str::<Value>(raw)?;
            parse_direct_registry_urls_value(&parsed)
        }
        Value::Mapping(map) => {
            let mut entries = BTreeMap::new();
            for (key, value) in map {
                let key = key.as_str().ok_or_else(|| {
                    RuntimeError::Unsupported(
                        "direct-registry.directUrls keys must be strings".to_string(),
                    )
                })?;
                let value = value.as_str().ok_or_else(|| {
                    RuntimeError::Unsupported(format!(
                        "direct-registry.directUrls `{key}` value must be a string"
                    ))
                })?;
                entries.insert(key.to_string(), value.to_string());
            }
            Ok(entries)
        }
        other => Err(RuntimeError::Unsupported(format!(
            "unsupported direct-registry.directUrls value: {other:?}"
        ))),
    }
}

fn validate_direct_registry_config(config: &DirectRegistryConfig) -> Result<(), RuntimeError> {
    for (key, url) in &config.direct_urls {
        if key.trim().is_empty() {
            return Err(RuntimeError::Unsupported(
                "direct-registry.directUrls keys must not be empty".to_string(),
            ));
        }
        let url = url.trim();
        if url.is_empty() {
            return Err(RuntimeError::Unsupported(format!(
                "direct-registry.directUrls `{key}` value must not be empty"
            )));
        }
        let parsed = Url::parse(url).map_err(|error| {
            RuntimeError::Unsupported(format!(
                "direct-registry.directUrls `{key}` value `{url}` is invalid: {error}"
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(RuntimeError::Unsupported(format!(
                "direct-registry.directUrls `{key}` value `{url}` must use http or https"
            )));
        }
        if parsed.host().is_none() {
            return Err(RuntimeError::Unsupported(format!(
                "direct-registry.directUrls `{key}` value `{url}` is missing a host"
            )));
        }
    }
    Ok(())
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
    let base_path = portal_url.path().trim_end_matches('/');
    let microservice_path = if base_path.is_empty() {
        "/ws/microservice".to_string()
    } else {
        format!("{base_path}/ws/microservice")
    };
    ws_url.set_path(&microservice_path);
    ws_url.set_query(None);
    ws_url.set_fragment(None);
    Ok(ws_url.to_string())
}

fn portal_token(config: &PortalRegistryConfig) -> Option<String> {
    get_env_value(PORTAL_AUTH_ENV)
        .filter(|value| !value.trim().is_empty())
        .map(|value| strip_bearer_prefix(&value))
        .or_else(|| {
            (!config.portal_token.trim().is_empty())
                .then(|| strip_bearer_prefix(&config.portal_token))
        })
}

fn resolve_control_candidates(
    config: &PortalRegistryConfig,
) -> Result<Vec<ControlCandidate>, RuntimeError> {
    let Some(configured) = config.control_candidates.as_ref() else {
        return Ok(vec![ControlCandidate::legacy_json()]);
    };
    if configured.is_empty() {
        return Err(RuntimeError::Unsupported(
            "portal-registry controlCandidates must not be empty".to_string(),
        ));
    }

    let mut candidates = Vec::with_capacity(configured.len());
    for ControlCandidateConfig {
        transport,
        wire_profile,
        negotiation,
    } in configured
    {
        let candidate = match (
            transport.as_str(),
            wire_profile.as_str(),
            negotiation.as_str(),
        ) {
            ("websocket", "legacy-json", "legacy") => ControlCandidate::legacy_json(),
            ("websocket", profile, "explicit")
                if Some(profile) == ControlCandidate::runtime_rkyv_v1().profile_token() =>
            {
                ControlCandidate::runtime_rkyv_v1()
            }
            _ => {
                return Err(RuntimeError::Unsupported(format!(
                    "unsupported portal-registry control candidate transport={transport:?} wireProfile={wire_profile:?} negotiation={negotiation:?}"
                )));
            }
        };
        if candidates.contains(&candidate) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate portal-registry control candidate {candidate:?}"
            )));
        }
        candidates.push(candidate);
    }
    Ok(candidates)
}

fn get_env_value(key: &str) -> Option<String> {
    let normalized = key.to_uppercase().replace(['-', '.'], "_");
    std::env::var(&normalized)
        .ok()
        .or_else(|| std::env::var(key).ok())
}

fn strip_bearer_prefix(token: &str) -> String {
    token
        .strip_prefix("Bearer ")
        .or_else(|| token.strip_prefix("bearer "))
        .unwrap_or(token)
        .to_string()
}

fn init_rustls_provider() {
    if let Err(e) = rustls::crypto::ring::default_provider().install_default() {
        warn!("rustls crypto provider was already installed or failed to install: {e:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{BoundTransport, ResolvedServerMetadata, TransportRuntime};
    use async_trait::async_trait;
    use std::sync::{LazyLock, Mutex};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    static ENV_TEST_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

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
    fn absent_control_candidates_preserves_legacy_websocket_default() {
        let config: PortalRegistryConfig =
            serde_yaml::from_str("portalUrl: https://controller.example.com\nportalToken: token\n")
                .expect("portal-registry config");

        assert_eq!(
            resolve_control_candidates(&config).expect("default candidates"),
            vec![ControlCandidate::legacy_json()]
        );
    }

    #[test]
    fn ordered_control_candidates_accept_only_exact_n2_profiles() {
        let explicit_profile = ControlCandidate::runtime_rkyv_v1()
            .profile_token()
            .expect("explicit profile token");
        let config: PortalRegistryConfig = serde_yaml::from_str(&format!(
            r#"
portalUrl: https://controller.example.com
portalToken: token
controlCandidates:
  - transport: websocket
    wireProfile: {explicit_profile}
    negotiation: explicit
  - transport: websocket
    wireProfile: legacy-json
    negotiation: legacy
"#,
        ))
        .expect("portal-registry config");

        assert_eq!(
            resolve_control_candidates(&config).expect("configured candidates"),
            vec![
                ControlCandidate::runtime_rkyv_v1(),
                ControlCandidate::legacy_json(),
            ]
        );

        for yaml in [
            "controlCandidates: []".to_string(),
            format!("controlCandidates: [{{transport: webtransport, wireProfile: {explicit_profile}, negotiation: explicit}}]"),
            "controlCandidates: [{transport: websocket, wireProfile: unknown, negotiation: explicit}]".to_string(),
            "controlCandidates: [{transport: websocket, wireProfile: legacy-json, negotiation: legacy}, {transport: websocket, wireProfile: legacy-json, negotiation: legacy}]".to_string(),
        ] {
            let config: PortalRegistryConfig = serde_yaml::from_str(&format!(
                "portalUrl: https://controller.example.com\nportalToken: token\n{yaml}\n"
            ))
            .expect("syntactically valid portal-registry config");
            assert!(
                resolve_control_candidates(&config).is_err(),
                "configuration must fail: {yaml}"
            );
        }
    }

    #[test]
    fn portal_auth_env_uses_shell_friendly_uppercase_name() {
        let _guard = ENV_TEST_MUTEX.lock().expect("env test mutex");
        unsafe {
            std::env::remove_var(PORTAL_AUTH_ENV);
            std::env::set_var("LIGHT_PORTAL_AUTHORIZATION", "Bearer test-token");
        }

        let value = get_env_value(PORTAL_AUTH_ENV);

        unsafe {
            std::env::remove_var("LIGHT_PORTAL_AUTHORIZATION");
        }

        assert_eq!(value.as_deref(), Some("Bearer test-token"));
    }

    #[test]
    fn bootstrap_config_uses_shell_friendly_uppercase_env_fallbacks() {
        let _guard = ENV_TEST_MUTEX.lock().expect("env test mutex");
        unsafe {
            std::env::remove_var(LIGHT_ENV_ENV);
            std::env::remove_var("LIGHT_ENV");
            std::env::remove_var(CONFIG_SERVER_URI_ENV);
            std::env::remove_var("LIGHT_CONFIG_SERVER_URI");
            std::env::set_var("LIGHT_ENV", "qa");
            std::env::set_var("LIGHT_CONFIG_SERVER_URI", "https://config.example.com");
        }

        let config_dir = TempDir::new().expect("config temp dir");
        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build();
        let (bootstrap, _) = runtime.load_bootstrap_config().expect("bootstrap config");

        unsafe {
            std::env::remove_var("LIGHT_ENV");
            std::env::remove_var("LIGHT_CONFIG_SERVER_URI");
        }

        assert_eq!(bootstrap.env_tag.as_deref(), Some("qa"));
        assert_eq!(
            bootstrap.config_server_uri.as_deref(),
            Some("https://config.example.com")
        );
    }

    #[test]
    fn config_server_authorization_header_accepts_bearer_and_bare_tokens() {
        assert_eq!(
            config_server_authorization_header(Some("Bearer test-token")),
            "Bearer test-token"
        );
        assert_eq!(
            config_server_authorization_header(Some("bearer test-token")),
            "Bearer test-token"
        );
        assert_eq!(
            config_server_authorization_header(Some("  test-token  ")),
            "Bearer test-token"
        );
        assert_eq!(config_server_authorization_header(None), "");
        assert_eq!(config_server_authorization_header(Some("   ")), "");
    }

    #[test]
    fn config_server_authorization_header_leaves_other_schemes_unchanged() {
        assert_eq!(
            config_server_authorization_header(Some("Basic credentials")),
            "Basic credentials"
        );
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
    fn merges_values_from_default_local_external_and_remote_sources() {
        let embedded = [EmbeddedConfigFile {
            name: VALUES_FILE,
            content: "embeddedOnly: yes\na: embedded\n",
        }];
        let default_dir = TempDir::new().expect("default config temp dir");
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(
            default_dir.path().join(VALUES_FILE),
            "a: default\nb: default\ndefaultOnly: true\n",
        )
        .expect("write default values");
        fs::write(config_dir.path().join(VALUES_FILE), "a: 1\nb: local\n")
            .expect("write local values");
        fs::write(
            external_dir.path().join(VALUES_FILE),
            "b: remote\nc: true\n",
        )
        .expect("write external values");

        let values = load_values_map(
            &embedded,
            Some(default_dir.path()),
            config_dir.path(),
            external_dir.path(),
            Some("b: config-server\nremoteOnly: 42\n".to_string()),
        )
        .expect("values map");

        assert_eq!(values["a"], Value::Number(1.into()));
        assert_eq!(values["b"], Value::String("config-server".to_string()));
        assert_eq!(values["c"], Value::Bool(true));
        assert_eq!(values["embeddedOnly"], Value::String("yes".to_string()));
        assert_eq!(values["defaultOnly"], Value::Bool(true));
        assert_eq!(values["remoteOnly"], Value::Number(42.into()));
    }

    #[test]
    fn runtime_config_uses_embedded_server_template_when_file_is_absent() {
        static EMBEDDED: &[EmbeddedConfigFile] = &[EmbeddedConfigFile {
            name: SERVER_FILE,
            content: r#"
ip: ${server.ip:0.0.0.0}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.embedded-1.0.0}
enableRegistry: ${server.enableRegistry:false}
startOnRegistryFailure: ${server.startOnRegistryFailure:false}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
"#,
        }];
        let config_dir = TempDir::new().expect("config temp dir");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_embedded_config(EMBEDDED)
            .with_config_dir(config_dir.path())
            .build();
        let config = runtime
            .build_runtime_config(
                BootstrapConfig::default(),
                None,
                config_dir.path().join("external"),
                RemoteBootstrapResult {
                    values_yaml: Some(
                        "server.httpPort: 9090\nserver.serviceId: com.networknt.remote-2.0.0\n"
                            .to_string(),
                    ),
                    cached_files: Vec::new(),
                },
            )
            .expect("runtime config");

        assert_eq!(config.server.http_port, 9090);
        assert_eq!(config.server.service_id, "com.networknt.remote-2.0.0");
        assert_eq!(config.embedded_config.len(), 1);
    }

    #[test]
    fn bootstrap_config_uses_local_template_over_default_template() {
        let default_dir = TempDir::new().expect("default config temp dir");
        let config_dir = TempDir::new().expect("config temp dir");
        fs::write(
            default_dir.path().join(STARTUP_FILE),
            r#"
host: ${startup.host:default.lightapi.net}
serviceId: ${startup.serviceId:com.networknt.default-1.0.0}
envTag: ${startup.envTag:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${configServer.uri:https://default-config-server:8435}
"#,
        )
        .expect("write default startup");
        fs::write(
            config_dir.path().join(STARTUP_FILE),
            r#"
serviceId: com.networknt.overlay-1.0.0
configServerUri: https://overlay-config-server:8435
"#,
        )
        .expect("write overlay startup");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_default_config_dir(default_dir.path())
            .with_config_dir(config_dir.path())
            .build();
        let (bootstrap, _) = runtime.load_bootstrap_config().expect("bootstrap config");

        assert_eq!(bootstrap.host, "lightapi.net");
        assert_eq!(
            bootstrap.service_id.as_deref(),
            Some("com.networknt.overlay-1.0.0")
        );
        assert_eq!(
            bootstrap.config_server_uri.as_deref(),
            Some("https://overlay-config-server:8435")
        );
    }

    #[test]
    fn bootstrap_config_ignores_external_startup_cache() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external config temp dir");
        fs::write(
            config_dir.path().join(STARTUP_FILE),
            r#"
host: ca.my-host.com
configServerUri: https://my-domain.com
bootstrapCaCertPath: /config/server.pem
"#,
        )
        .expect("write local startup");
        fs::write(
            external_dir.path().join(STARTUP_FILE),
            r#"
host: ${startup.host:dev.lightapi.net}
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
envTag: ${server.environment:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${light-config-server-uri:https://local.localhost}
bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
"#,
        )
        .expect("write cached startup");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let (bootstrap, _) = runtime.load_bootstrap_config().expect("bootstrap config");

        assert_eq!(bootstrap.host, "ca.my-host.com");
        assert_eq!(
            bootstrap.config_server_uri.as_deref(),
            Some("https://my-domain.com")
        );
        assert_eq!(
            bootstrap.bootstrap_ca_cert_path.as_deref(),
            Some(Path::new("/config/server.pem"))
        );
    }

    #[test]
    fn runtime_config_exposes_resolved_values() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(
            config_dir.path().join(SERVER_FILE),
            r#"
ip: ${server.ip:0.0.0.0}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.test-1.0.0}
"#,
        )
        .expect("write server config");
        fs::write(
            config_dir.path().join(VALUES_FILE),
            r#"
server.ip: 127.0.0.1
shared: local
direct-registry.directUrls:
  com.networknt.petstore-1.0.0: https://petstore:9443
"#,
        )
        .expect("write local values");
        fs::write(
            external_dir.path().join(VALUES_FILE),
            "shared: external\nexternalOnly: true\n",
        )
        .expect("write external values");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build();
        let config = runtime
            .build_runtime_config(
                BootstrapConfig::default(),
                None,
                external_dir.path().to_path_buf(),
                RemoteBootstrapResult {
                    values_yaml: Some("shared: remote\nremoteOnly: 42\n".to_string()),
                    cached_files: Vec::new(),
                },
            )
            .expect("build runtime config");

        assert_eq!(
            config.resolved_values["server.ip"],
            Value::String("127.0.0.1".to_string())
        );
        assert_eq!(
            config.resolved_values["shared"],
            Value::String("remote".to_string())
        );
        assert_eq!(config.resolved_values["externalOnly"], Value::Bool(true));
        assert_eq!(
            config.resolved_values["remoteOnly"],
            Value::Number(42.into())
        );
        assert_eq!(
            config.direct_registry.direct_urls["com.networknt.petstore-1.0.0"],
            "https://petstore:9443"
        );
    }

    #[test]
    fn runtime_config_uses_default_server_template_without_local_server_file() {
        let default_dir = TempDir::new().expect("default config temp dir");
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(
            default_dir.path().join(SERVER_FILE),
            r#"
ip: ${server.ip:0.0.0.0}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.default-1.0.0}
enableRegistry: ${server.enableRegistry:false}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:dev}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
"#,
        )
        .expect("write default server template");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_default_config_dir(default_dir.path())
            .with_config_dir(config_dir.path())
            .build();
        let config = runtime
            .build_runtime_config(
                BootstrapConfig::default(),
                None,
                external_dir.path().to_path_buf(),
                RemoteBootstrapResult {
                    values_yaml: Some(
                        "server.httpPort: 9090\nserver.serviceId: com.networknt.remote-2.0.0\n"
                            .to_string(),
                    ),
                    cached_files: Vec::new(),
                },
            )
            .expect("build runtime config");

        assert_eq!(config.server.http_port, 9090);
        assert_eq!(config.server.service_id, "com.networknt.remote-2.0.0");
        assert_eq!(
            config.default_config_dir.as_deref(),
            Some(default_dir.path())
        );
    }

    #[test]
    fn runtime_config_client_is_reloaded_from_merged_values() {
        let default_dir = TempDir::new().expect("default config temp dir");
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");

        fs::write(
            default_dir.path().join(CLIENT_FILE),
            r#"
oauth:
  token:
    key:
      serverUrl: ${client.tokenKeyServerUrl:}
      uri: ${client.tokenKeyUri:/oauth2/key}
"#,
        )
        .expect("write default client template");

        fs::write(
            default_dir.path().join(SERVER_FILE),
            r#"
ip: ${server.ip:0.0.0.0}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.default-1.0.0}
enableRegistry: ${server.enableRegistry:false}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:dev}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
"#,
        )
        .expect("write default server template");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_default_config_dir(default_dir.path())
            .with_config_dir(config_dir.path())
            .build();

        let bootstrap_client = ClientConfig::default();

        let config = runtime
            .build_runtime_config(
                BootstrapConfig::default(),
                Some(bootstrap_client),
                external_dir.path().to_path_buf(),
                RemoteBootstrapResult {
                    values_yaml: Some(
                        "client.tokenKeyServerUrl: https://remote-oauth\n\
                         client.tokenKeyUri: /oauth2/custom/keys\n"
                            .to_string(),
                    ),
                    cached_files: Vec::new(),
                },
            )
            .expect("build runtime config");

        let client_config = config.client.as_ref().expect("client config present");
        assert_eq!(
            client_config.oauth.token.key.server_url.as_deref(),
            Some("https://remote-oauth")
        );
        assert_eq!(
            client_config.oauth.token.key.uri.as_str(),
            "/oauth2/custom/keys"
        );
    }

    #[test]
    fn gateway_client_template_resolves_with_default_placeholders() {
        let loader = ConfigLoader::new("", None, None).expect("loader");
        let file = EmbeddedConfigFile {
            name: CLIENT_FILE,
            content: include_str!("../../../apps/light-gateway/config/client.yml"),
        };
        let mut value = loader
            .load_embedded_file(&file)
            .expect("load gateway client template");

        loader
            .resolve_value(&mut value)
            .expect("resolve gateway client template placeholders");
        let client: ClientConfig =
            serde_yaml::from_value(value).expect("parse gateway client template");

        assert_eq!(client.tls.tls_version, None);
        assert_eq!(client.request.error_threshold, 2);
        assert_eq!(client.request.reset_timeout, 7_000);
        assert_eq!(client.oauth.sign.uri, "/oauth2/sign");
        assert_eq!(client.oauth.deref.uri, "/oauth2/deref");
    }

    #[tokio::test]
    async fn reload_context_fetches_remote_values_into_external_cache() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind config server");
        let addr = listener.local_addr().expect("config server addr");
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut buffer = [0_u8; 4096];
                let bytes = stream.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..bytes]);
                let (content_type, body) = if request.starts_with("GET /config-server/configs") {
                    ("application/yaml", "gateway.healthPath: /remote\n")
                } else {
                    ("application/json", "{}")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let runtime_config = RuntimeConfig {
            bootstrap: BootstrapConfig {
                config_server_uri: Some(format!("http://{addr}")),
                authorization: Some("Bearer token".to_string()),
                accept_header: default_accept_header(),
                timeout: crate::config::default_timeout_ms(),
                connect_timeout: crate::config::default_connect_timeout_ms(),
                ..BootstrapConfig::default()
            },
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: DirectRegistryConfig {
                direct_urls: BTreeMap::from([(
                    "com.networknt.controller-1.0.0".to_string(),
                    "https://controller:8438".to_string(),
                )]),
            },
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: external_dir.path().to_path_buf(),
            resolved_values: HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        };

        let ctx = runtime_config
            .reload_context()
            .await
            .expect("reload context");

        assert_eq!(
            ctx.runtime_config.resolved_values["gateway.healthPath"],
            Value::String("/remote".to_string())
        );
        assert_eq!(
            ctx.runtime_config.direct_registry.direct_urls["com.networknt.controller-1.0.0"],
            "https://controller:8438"
        );
        assert!(external_dir.path().join(VALUES_FILE).exists());
        server.await.expect("config server task");
    }

    #[tokio::test]
    async fn remote_bootstrap_http_error_uses_available_cache() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind config server");
        let addr = listener.local_addr().expect("config server addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await.expect("read request");
            let body = "config server unavailable";
            let response = format!(
                "HTTP/1.1 503 Service Unavailable\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(
            external_dir.path().join(VALUES_FILE),
            "server.serviceId: com.networknt.cached-1.0.0\n",
        )
        .expect("write cached values");
        let bootstrap = BootstrapConfig {
            config_server_uri: Some(format!("http://{addr}")),
            accept_header: default_accept_header(),
            timeout: crate::config::default_timeout_ms(),
            connect_timeout: crate::config::default_connect_timeout_ms(),
            ..BootstrapConfig::default()
        };

        let result = fetch_remote_bootstrap_if_needed(&bootstrap, None, external_dir.path())
            .await
            .expect("http error should use available cache");

        assert!(result.values_yaml.is_none());
        assert!(result.cached_files.is_empty());
        assert!(external_dir.path().join(VALUES_FILE).exists());
        server.await.expect("config server task");
    }

    #[tokio::test]
    async fn remote_bootstrap_connection_failure_uses_cached_values() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind unused config server port");
        let addr = listener.local_addr().expect("config server addr");
        drop(listener);

        let external_dir = TempDir::new().expect("external temp dir");
        fs::write(
            external_dir.path().join(VALUES_FILE),
            "server.serviceId: com.networknt.cached-1.0.0\n",
        )
        .expect("write cached values");
        let bootstrap = BootstrapConfig {
            config_server_uri: Some(format!("http://{addr}")),
            accept_header: default_accept_header(),
            timeout: crate::config::default_timeout_ms(),
            connect_timeout: crate::config::default_connect_timeout_ms(),
            ..BootstrapConfig::default()
        };

        let result = fetch_remote_bootstrap_if_needed(&bootstrap, None, external_dir.path())
            .await
            .expect("connection failure should use cached values");

        assert!(result.values_yaml.is_none());
        assert!(result.cached_files.is_empty());
        assert!(external_dir.path().join(VALUES_FILE).exists());
    }

    #[tokio::test]
    async fn remote_bootstrap_file_download_error_keeps_downloaded_values() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind config server");
        let addr = listener.local_addr().expect("config server addr");
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("accept request");
                let mut buffer = [0_u8; 4096];
                let bytes = stream.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..bytes]);
                let (status, content_type, body) =
                    if request.starts_with("GET /config-server/configs") {
                        ("200 OK", "application/yaml", "server.httpPort: 9090\n")
                    } else {
                        (
                            "500 Internal Server Error",
                            "text/plain",
                            "file download failed",
                        )
                    };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
        });

        let external_dir = TempDir::new().expect("external temp dir");
        let bootstrap = BootstrapConfig {
            config_server_uri: Some(format!("http://{addr}")),
            accept_header: default_accept_header(),
            timeout: crate::config::default_timeout_ms(),
            connect_timeout: crate::config::default_connect_timeout_ms(),
            ..BootstrapConfig::default()
        };

        let result = fetch_remote_bootstrap_if_needed(&bootstrap, None, external_dir.path())
            .await
            .expect("file download error should keep downloaded values");

        assert_eq!(
            result.values_yaml.as_deref(),
            Some("server.httpPort: 9090\n")
        );
        assert_eq!(
            result.cached_files,
            vec![external_dir.path().join(VALUES_FILE)]
        );
        assert_eq!(
            fs::read_to_string(external_dir.path().join(VALUES_FILE)).expect("read values"),
            "server.httpPort: 9090\n"
        );
        server.await.expect("config server task");
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
    fn microservice_ws_url_uses_root_path_for_plain_portal_url() {
        let portal_url = Url::parse("https://controller:8438").expect("portal url");

        let ws_url = to_microservice_ws_url(&portal_url).expect("microservice ws url");

        assert_eq!(ws_url, "wss://controller:8438/ws/microservice");
    }

    #[test]
    fn microservice_ws_url_preserves_portal_url_base_path() {
        let portal_url = Url::parse("https://dev-apmh-api-platform.networknt.com/controlplane/v2")
            .expect("portal url");

        let ws_url = to_microservice_ws_url(&portal_url).expect("microservice ws url");

        assert_eq!(
            ws_url,
            "wss://dev-apmh-api-platform.networknt.com/controlplane/v2/ws/microservice"
        );
    }

    #[test]
    fn microservice_ws_url_preserves_base_path_without_trailing_slash() {
        let portal_url = Url::parse("https://dev-apmh-api-platform.networknt.com/controlplane/v2/")
            .expect("portal url");

        let ws_url = to_microservice_ws_url(&portal_url).expect("microservice ws url");

        assert_eq!(
            ws_url,
            "wss://dev-apmh-api-platform.networknt.com/controlplane/v2/ws/microservice"
        );
    }

    #[test]
    fn microservice_ws_url_strips_query_and_fragment() {
        let portal_url = Url::parse("http://controller:8080/controlplane/v2?tenant=apmh#registry")
            .expect("portal url");

        let ws_url = to_microservice_ws_url(&portal_url).expect("microservice ws url");

        assert_eq!(
            ws_url,
            "ws://controller:8080/controlplane/v2/ws/microservice"
        );
    }

    #[test]
    fn microservice_ws_url_rejects_unsupported_scheme() {
        let portal_url = Url::parse("ftp://controller/controlplane/v2").expect("portal url");

        let error = to_microservice_ws_url(&portal_url).expect_err("unsupported scheme");

        assert!(
            error
                .to_string()
                .contains("unsupported portal URL scheme `ftp`")
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

        let (_bootstrap, client_config) =
            runtime.load_bootstrap_config().expect("bootstrap config");

        assert_eq!(client_config.map(|c| c.tls.verify_hostname), Some(false));
    }

    #[test]
    fn load_bootstrap_config_reads_nested_client_tls_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        fs::write(
            config_dir.path().join(CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
"#,
        )
        .expect("write client config");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build();

        let (_bootstrap, client_config) =
            runtime.load_bootstrap_config().expect("bootstrap config");

        let tls = client_config.map(|c| c.tls).expect("client tls");
        assert!(!tls.verify_hostname);
    }

    #[test]
    fn load_bootstrap_config_prefers_nested_client_tls_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        fs::write(
            config_dir.path().join(CLIENT_FILE),
            r#"
verifyHostname: false
tls:
  verifyHostname: true
"#,
        )
        .expect("write client config");

        let runtime = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build();

        let (_bootstrap, client_config) =
            runtime.load_bootstrap_config().expect("bootstrap config");

        assert_eq!(client_config.map(|c| c.tls.verify_hostname), Some(true));
    }

    #[test]
    fn config_server_tls_falls_back_to_bootstrap_ca_when_client_ca_is_empty() {
        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(PathBuf::from("config/ca.pem")),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(PathBuf::new());
        client_config.tls.verify_hostname = false;

        let tls = config_server_tls_config(&bootstrap, Some(&client_config));

        assert_eq!(tls.ca_cert_path, Some(PathBuf::from("config/ca.pem")));
        assert!(!tls.verify_hostname);
    }

    #[test]
    fn portal_registry_ca_prefers_client_ca_path() {
        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(PathBuf::from("config/bootstrap-ca.pem")),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(PathBuf::from("config/client-ca.pem"));

        let ca_cert_path = portal_registry_ca_cert_path(&bootstrap, Some(&client_config));

        assert_eq!(ca_cert_path, Some(PathBuf::from("config/client-ca.pem")));
    }

    #[test]
    fn portal_registry_ca_falls_back_to_bootstrap_ca_when_client_ca_is_empty() {
        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(PathBuf::from("config/bootstrap-ca.pem")),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(PathBuf::new());

        let ca_cert_path = portal_registry_ca_cert_path(&bootstrap, Some(&client_config));

        assert_eq!(ca_cert_path, Some(PathBuf::from("config/bootstrap-ca.pem")));
    }

    #[test]
    fn portal_registry_ca_returns_none_when_no_ca_path_is_configured() {
        let bootstrap = BootstrapConfig::default();
        let client_config = ClientConfig::default();

        let ca_cert_path = portal_registry_ca_cert_path(&bootstrap, Some(&client_config));

        assert_eq!(ca_cert_path, None);
    }

    #[test]
    fn portal_registry_ca_reads_client_ca_path() {
        let config_dir = TempDir::new().expect("config temp dir");
        let bootstrap_ca_path = config_dir.path().join("bootstrap-ca.pem");
        let client_ca_path = config_dir.path().join("client-ca.pem");
        fs::write(&bootstrap_ca_path, b"bootstrap-ca").expect("write bootstrap ca");
        fs::write(&client_ca_path, b"client-ca").expect("write client ca");

        let bootstrap = BootstrapConfig {
            bootstrap_ca_cert_path: Some(bootstrap_ca_path),
            ..BootstrapConfig::default()
        };
        let mut client_config = ClientConfig::default();
        client_config.tls.ca_cert_path = Some(client_ca_path.clone());

        let (ca_cert_path, certificate) =
            read_portal_registry_ca_certificate(&bootstrap, Some(&client_config))
                .expect("read portal-registry ca")
                .expect("portal-registry ca");

        assert_eq!(ca_cert_path, client_ca_path);
        assert_eq!(certificate, b"client-ca");
    }

    #[tokio::test]
    async fn start_registers_builtin_runtime_modules() {
        let config_dir = TempDir::new().expect("config temp dir");
        fs::write(
            config_dir.path().join(SERVER_FILE),
            r#"
ip: 127.0.0.1
httpPort: 8080
enableHttp: true
httpsPort: 8443
enableHttps: false
serviceId: com.networknt.test-1.0.0
enableRegistry: false
"#,
        )
        .expect("write server config");

        let running = LightRuntimeBuilder::new(NoopTransport)
            .with_config_dir(config_dir.path())
            .build()
            .start()
            .await
            .expect("start runtime");

        let module_ids = running
            .module_registry
            .module_summaries()
            .into_iter()
            .map(|module| module.module_id)
            .collect::<Vec<_>>();

        assert!(module_ids.contains(&"light-runtime/startup".to_string()));
        assert!(module_ids.contains(&"light-runtime/server".to_string()));

        running.shutdown().await.expect("shutdown runtime");
    }
}
