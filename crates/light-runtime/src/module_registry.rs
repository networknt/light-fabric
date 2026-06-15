use crate::cache::CacheRegistry;
use crate::config::RuntimeConfig;
use crate::logging::LoggingControl;
use crate::runtime::{RuntimeError, load_merged_config};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use config_loader::ConfigLoader;
use portal_registry::RegistryHandler;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

const MASKED_VALUE: &str = "*";
pub const CLIENT_MODULE_ID: &str = "light-client/client";
pub const CLIENT_CONFIG_NAME: &str = "client";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModuleKind {
    Core,
    Framework,
    Application,
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "value")]
pub enum MaskSpec {
    Key(String),
    Path(String),
}

impl MaskSpec {
    pub fn key(value: impl Into<String>) -> Self {
        Self::Key(value.into())
    }

    pub fn path(value: impl Into<String>) -> Self {
        Self::Path(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub completed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleEntry {
    pub module_id: String,
    pub config_name: String,
    pub kind: ModuleKind,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub reloadable: bool,
    pub config: JsonValue,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub masks: Vec<MaskSpec>,
    pub loaded_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reload: Option<ReloadStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleSummary {
    pub module_id: String,
    pub config_name: String,
    pub kind: ModuleKind,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    pub reloadable: bool,
    pub loaded_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reload: Option<ReloadStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadSkipped {
    pub module_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadFailed {
    pub module_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReloadModulesResult {
    /// Java-compatible alias consumed by portal-view today.
    pub modules: Vec<String>,
    pub reloaded: Vec<String>,
    pub skipped: Vec<ReloadSkipped>,
    pub failed: Vec<ReloadFailed>,
}

#[derive(Debug, Clone)]
pub struct ReloadContext {
    pub runtime_config: RuntimeConfig,
}

impl ReloadContext {
    pub fn new(runtime_config: RuntimeConfig) -> Self {
        Self { runtime_config }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadOutcome {
    pub message: Option<String>,
}

impl ReloadOutcome {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
        }
    }
}

#[async_trait]
pub trait ReloadableModule: Send + Sync {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError>;
}

pub struct ConfigManager<T> {
    current: RwLock<Arc<T>>,
}

impl<T> ConfigManager<T> {
    pub fn new(config: T) -> Self {
        Self {
            current: RwLock::new(Arc::new(config)),
        }
    }

    pub fn load(&self) -> Arc<T> {
        self.current
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub fn store(&self, config: T) -> Arc<T> {
        let config = Arc::new(config);
        *self.current.write().unwrap_or_else(|err| err.into_inner()) = Arc::clone(&config);
        config
    }
}

impl<T> fmt::Debug for ConfigManager<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigManager").finish_non_exhaustive()
    }
}

#[derive(Default)]
pub struct ModuleRegistry {
    entries: RwLock<BTreeMap<String, ModuleEntry>>,
    reloaders: RwLock<BTreeMap<String, Arc<dyn ReloadableModule>>>,
    mask_config_properties: RwLock<bool>,
}

impl fmt::Debug for ModuleRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleRegistry")
            .field("entries", &self.entries_read().len())
            .field("reloaders", &self.reloaders_read().len())
            .finish_non_exhaustive()
    }
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
            reloaders: RwLock::new(BTreeMap::new()),
            mask_config_properties: RwLock::new(true),
        }
    }

    pub fn set_mask_config_properties(&self, enabled: bool) {
        *self.mask_write() = enabled;
    }

    pub fn register_config(
        &self,
        module_id: impl Into<String>,
        config_name: impl Into<String>,
        kind: ModuleKind,
        mut config: JsonValue,
        masks: impl IntoIterator<Item = MaskSpec>,
        active: bool,
        enabled: Option<bool>,
        reloadable: bool,
    ) -> ModuleEntry {
        let mut masks = masks.into_iter().collect::<Vec<_>>();
        masks.extend(default_masks());

        if *self.mask_read() {
            apply_masks(&mut config, &masks);
        }

        let entry = ModuleEntry {
            module_id: module_id.into(),
            config_name: config_name.into(),
            kind,
            active,
            enabled,
            reloadable,
            config,
            masks,
            loaded_at: Utc::now(),
            last_reload: None,
        };

        self.entries_write()
            .insert(entry.module_id.clone(), entry.clone());
        entry
    }

    pub fn register_loaded_config<T>(
        &self,
        module_id: impl Into<String>,
        config_name: impl Into<String>,
        kind: ModuleKind,
        config: &T,
        masks: impl IntoIterator<Item = MaskSpec>,
        active: bool,
        enabled: Option<bool>,
        reloadable: bool,
    ) -> Result<ModuleEntry, RuntimeError>
    where
        T: Serialize,
    {
        Ok(self.register_config(
            module_id,
            config_name,
            kind,
            serde_json::to_value(config)?,
            masks,
            active,
            enabled,
            reloadable,
        ))
    }

    pub fn register_reloader(
        &self,
        module_id: impl Into<String>,
        reloader: Arc<dyn ReloadableModule>,
    ) {
        let module_id = module_id.into();
        self.reloaders_write()
            .insert(module_id.clone(), Arc::clone(&reloader));
        if let Some(entry) = self.entries_write().get_mut(&module_id) {
            entry.reloadable = true;
        }
    }

    pub fn load_config<T>(
        &self,
        runtime_config: &RuntimeConfig,
        file_name: &str,
    ) -> Result<T, RuntimeError>
    where
        T: DeserializeOwned,
    {
        let password = std::env::var("light_4j_config_password").ok();
        let loader = ConfigLoader::from_values(
            runtime_config.resolved_values.clone(),
            password.as_deref(),
            None,
        )?;

        let merged = load_merged_config(
            &loader,
            runtime_config.embedded_config,
            runtime_config.default_config_dir.as_deref(),
            &runtime_config.config_dir,
            &runtime_config.external_config_dir,
            file_name,
        )?
        .ok_or_else(|| RuntimeError::MissingConfig(file_name.to_string()))?;
        let parsed = serde_yaml::from_value::<T>(merged)?;
        Ok(parsed)
    }

    pub fn load_registered<T>(
        &self,
        runtime_config: &RuntimeConfig,
        file_name: &str,
        module_id: impl Into<String>,
        config_name: impl Into<String>,
        kind: ModuleKind,
        masks: impl IntoIterator<Item = MaskSpec>,
        enabled: Option<bool>,
        reloadable: bool,
    ) -> Result<T, RuntimeError>
    where
        T: DeserializeOwned + Serialize,
    {
        let parsed = self.load_config::<T>(runtime_config, file_name)?;
        self.register_loaded_config(
            module_id,
            config_name,
            kind,
            &parsed,
            masks,
            true,
            enabled,
            reloadable,
        )?;
        Ok(parsed)
    }

    pub fn register_runtime_configs(&self, config: &RuntimeConfig) -> Result<(), RuntimeError> {
        self.set_mask_config_properties(mask_config_properties(config));

        self.register_config(
            "light-runtime/startup",
            "startup",
            ModuleKind::Core,
            serde_json::to_value(&config.bootstrap)?,
            [MaskSpec::key("authorization")],
            true,
            Some(true),
            false,
        );

        self.register_config(
            "light-runtime/server",
            "server",
            ModuleKind::Core,
            serde_json::to_value(&config.server)?,
            [],
            true,
            Some(true),
            false,
        );

        if let Some(client) = &config.client {
            self.register_config(
                CLIENT_MODULE_ID,
                CLIENT_CONFIG_NAME,
                ModuleKind::Core,
                serde_json::to_value(client)?,
                client_config_masks(),
                true,
                Some(true),
                true,
            );
            self.register_reloader(CLIENT_MODULE_ID, Arc::new(ClientReloader));
        }

        if let Some(portal_registry) = &config.portal_registry {
            self.register_config(
                "light-runtime/portal-registry",
                "portal-registry",
                ModuleKind::Core,
                serde_json::to_value(portal_registry)?,
                [
                    MaskSpec::key("portalToken"),
                    MaskSpec::key("controllerDiscoveryToken"),
                ],
                true,
                Some(config.server.enable_registry),
                false,
            );
        }

        self.register_config(
            "light-runtime/direct-registry",
            "direct-registry",
            ModuleKind::Core,
            serde_json::to_value(&config.direct_registry)?,
            [],
            true,
            Some(!config.direct_registry.direct_urls.is_empty()),
            true,
        );
        self.register_reloader(
            "light-runtime/direct-registry",
            Arc::new(DirectRegistryReloader),
        );

        Ok(())
    }

    pub fn entries(&self) -> Vec<ModuleEntry> {
        self.entries_read().values().cloned().collect()
    }

    pub fn module_summaries(&self) -> Vec<ModuleSummary> {
        self.entries_read()
            .values()
            .map(|entry| ModuleSummary {
                module_id: entry.module_id.clone(),
                config_name: entry.config_name.clone(),
                kind: entry.kind,
                active: entry.active,
                enabled: entry.enabled,
                reloadable: entry.reloadable,
                loaded_at: entry.loaded_at,
                last_reload: entry.last_reload.clone(),
            })
            .collect()
    }

    pub fn module_ids(&self) -> Vec<String> {
        self.entries_read().keys().cloned().collect()
    }

    pub fn component_configs(&self) -> BTreeMap<String, JsonValue> {
        self.entries_read()
            .values()
            .filter(|entry| entry.active)
            .map(|entry| (entry.config_name.clone(), entry.config.clone()))
            .collect()
    }

    pub fn server_info(&self, config: &RuntimeConfig) -> JsonValue {
        json!({
            "deployment": {
                "apiVersion": config.service_identity.version,
                "frameworkVersion": env!("CARGO_PKG_VERSION")
            },
            "environment": {
                "host": {
                    "ip": config.server.advertised_address.as_deref().unwrap_or(config.server.ip.as_str()),
                    "hostname": hostname()
                },
                "runtime": {
                    "availableProcessors": std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
                },
                "system": {
                    "osName": std::env::consts::OS,
                    "osArch": std::env::consts::ARCH
                }
            },
            "security": {},
            "component": self.component_configs(),
            "capabilities": self.capability_summary(),
            "plugin": {},
            "plugins": [],
            "modules": self.module_summaries()
        })
    }

    fn capability_summary(&self) -> JsonValue {
        let entries = self.entries_read();
        let active_modules = entries
            .values()
            .filter(|entry| entry.active)
            .map(|entry| entry.module_id.clone())
            .collect::<Vec<_>>();
        let handler_config = entries
            .get("light-pingora/handler")
            .filter(|entry| entry.active)
            .map(|entry| &entry.config);
        let chains = handler_config
            .and_then(|config| config.get("chains"))
            .and_then(JsonValue::as_object)
            .map(|chains| chains.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();

        json!({
            "activeModules": active_modules,
            "traffic": {
                "proxy": active_module(&entries, "light-pingora/proxy"),
                "router": active_module(&entries, "light-pingora/router"),
                "pathPrefixService": active_module(&entries, "light-pingora/path-prefix-service"),
                "token": active_module(&entries, "light-pingora/token"),
                "pathResource": active_module(&entries, "light-pingora/path-resource"),
                "virtualHost": active_module(&entries, "light-pingora/virtual-host")
            },
            "handlers": {
                "active": handler_config
                    .and_then(|config| config.get("activeHandlers"))
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "chains": chains,
                "paths": handler_config
                    .and_then(|config| config.get("paths"))
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "defaultHandlers": handler_config
                    .and_then(|config| config.get("defaultHandlers"))
                    .cloned()
                    .unwrap_or_else(|| json!([]))
            },
            "hosts": entries
                .get("light-pingora/virtual-host")
                .filter(|entry| entry.active)
                .and_then(|entry| entry.config.get("hosts"))
                .cloned()
                .unwrap_or_else(|| json!([])),
            "pathResource": entries
                .get("light-pingora/path-resource")
                .filter(|entry| entry.active)
                .map(|entry| entry.config.clone())
                .unwrap_or_else(|| json!(null))
        })
    }

    pub async fn reload_modules(
        &self,
        ctx: ReloadContext,
        requested_modules: &[String],
    ) -> ReloadModulesResult {
        // Update direct-registry config if present in the fresh context.
        if let Ok(direct_val) = serde_json::to_value(&ctx.runtime_config.direct_registry) {
            if let Some(entry) = self
                .entries_write()
                .get_mut("light-runtime/direct-registry")
            {
                entry.config = direct_val;
                entry.enabled = Some(!ctx.runtime_config.direct_registry.direct_urls.is_empty());
                entry.loaded_at = Utc::now();
            }
        }

        // Update client config if present in the fresh context.
        if let Some(client) = &ctx.runtime_config.client {
            if let Ok(mut client_val) = serde_json::to_value(client) {
                if let Some(entry) = self.entries_write().get_mut(CLIENT_MODULE_ID) {
                    if *self.mask_read() {
                        apply_masks(&mut client_val, &entry.masks);
                    }
                    entry.config = client_val;
                    entry.loaded_at = Utc::now();
                }
            }
        }

        let module_ids = self.module_ids();
        let target_modules = if requested_modules.is_empty()
            || requested_modules
                .first()
                .is_some_and(|id| requested_modules.len() == 1 && is_all_marker(id))
        {
            module_ids
        } else {
            requested_modules.to_vec()
        };

        let mut result = ReloadModulesResult::default();
        for module_id in target_modules {
            let Some(entry) = self.entry(&module_id) else {
                result.failed.push(ReloadFailed {
                    module_id,
                    message: "module not found".to_string(),
                });
                continue;
            };

            if !entry.reloadable {
                self.set_last_reload(
                    &module_id,
                    "skipped",
                    Some("module requires restart".to_string()),
                );
                result.skipped.push(ReloadSkipped {
                    module_id,
                    reason: "requiresRestart".to_string(),
                });
                continue;
            }

            let Some(reloader) = self.reloader(&module_id) else {
                self.set_last_reload(
                    &module_id,
                    "skipped",
                    Some("reload implementation is not registered".to_string()),
                );
                result.skipped.push(ReloadSkipped {
                    module_id,
                    reason: "reloadNotImplemented".to_string(),
                });
                continue;
            };

            match reloader.reload(ctx.clone()).await {
                Ok(outcome) => {
                    self.set_last_reload(&module_id, "success", outcome.message);
                    result.reloaded.push(module_id.clone());
                    result.modules.push(module_id);
                }
                Err(error) => {
                    let message = error.to_string();
                    self.set_last_reload(&module_id, "failed", Some(message.clone()));
                    result.failed.push(ReloadFailed { module_id, message });
                }
            }
        }
        result
    }

    fn entry(&self, module_id: &str) -> Option<ModuleEntry> {
        self.entries_read().get(module_id).cloned()
    }

    fn reloader(&self, module_id: &str) -> Option<Arc<dyn ReloadableModule>> {
        self.reloaders_read().get(module_id).cloned()
    }

    fn set_last_reload(&self, module_id: &str, status: &str, message: Option<String>) {
        if let Some(entry) = self.entries_write().get_mut(module_id) {
            entry.last_reload = Some(ReloadStatus {
                status: status.to_string(),
                message,
                completed_at: Utc::now(),
            });
        }
    }

    fn entries_read(&self) -> RwLockReadGuard<'_, BTreeMap<String, ModuleEntry>> {
        self.entries.read().unwrap_or_else(|err| err.into_inner())
    }

    fn entries_write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, ModuleEntry>> {
        self.entries.write().unwrap_or_else(|err| err.into_inner())
    }

    fn reloaders_read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Arc<dyn ReloadableModule>>> {
        self.reloaders.read().unwrap_or_else(|err| err.into_inner())
    }

    fn reloaders_write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Arc<dyn ReloadableModule>>> {
        self.reloaders
            .write()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn mask_read(&self) -> RwLockReadGuard<'_, bool> {
        self.mask_config_properties
            .read()
            .unwrap_or_else(|err| err.into_inner())
    }

    fn mask_write(&self) -> RwLockWriteGuard<'_, bool> {
        self.mask_config_properties
            .write()
            .unwrap_or_else(|err| err.into_inner())
    }
}

pub struct RuntimeMcpHandler {
    registry: Arc<ModuleRegistry>,
    config: RuntimeConfig,
    delegate: Arc<dyn RegistryHandler>,
    cache_registry: Option<Arc<CacheRegistry>>,
    logging_control: Option<Arc<LoggingControl>>,
}

impl RuntimeMcpHandler {
    pub fn new(
        registry: Arc<ModuleRegistry>,
        config: RuntimeConfig,
        delegate: Arc<dyn RegistryHandler>,
    ) -> Self {
        Self {
            registry,
            config,
            delegate,
            cache_registry: None,
            logging_control: None,
        }
    }

    pub fn with_cache_registry(mut self, cache_registry: Arc<CacheRegistry>) -> Self {
        self.cache_registry = Some(cache_registry);
        self
    }

    pub fn with_logging_control(mut self, logging_control: Arc<LoggingControl>) -> Self {
        self.logging_control = Some(logging_control);
        self
    }
}

#[async_trait]
impl RegistryHandler for RuntimeMcpHandler {
    async fn handle_notification(&self, method: &str, params: JsonValue) {
        self.delegate.handle_notification(method, params).await;
    }

    async fn handle_request(&self, method: &str, params: JsonValue) -> JsonValue {
        match method {
            "tools/list" => json!({ "tools": runtime_tools() }),
            "tools/call" => {
                let Some(name) = params.get("name").and_then(JsonValue::as_str) else {
                    return json!({
                        "status": "error",
                        "message": "tools/call requires params.name"
                    });
                };
                match name {
                    "get_service_info" => self.registry.server_info(&self.config),
                    "get_modules" => json!({ "modules": self.registry.module_ids() }),
                    "list_caches" => self.list_caches(),
                    "get_cache_entries" => self.get_cache_entries(params.get("arguments")).await,
                    "clear_cache" => self.clear_cache(params.get("arguments")).await,
                    "get_logging_filter" => self.get_logging_filter(),
                    "set_logging_filter" => self.set_logging_filter(params.get("arguments")),
                    "reload_modules" => match parse_reload_modules(params.get("arguments")) {
                        Ok(modules) => {
                            let result = match self.config.reload_context().await {
                                Ok(ctx) => self.registry.reload_modules(ctx, &modules).await,
                                Err(error) => reload_context_failure(&modules, error.to_string()),
                            };
                            serde_json::to_value(result).unwrap_or_else(|error| {
                                json!({
                                    "status": "error",
                                    "message": error.to_string()
                                })
                            })
                        }
                        Err(message) => json!({
                            "status": "error",
                            "message": message
                        }),
                    },
                    _ => self.delegate.handle_request(method, params).await,
                }
            }
            _ => self.delegate.handle_request(method, params).await,
        }
    }
}

impl RuntimeMcpHandler {
    fn list_caches(&self) -> JsonValue {
        let Some(cache_registry) = self.cache_registry.as_ref() else {
            return unsupported_cache_response(None);
        };

        json!({
            "supported": true,
            "status": "success",
            "caches": cache_registry.names()
        })
    }

    async fn get_cache_entries(&self, arguments: Option<&JsonValue>) -> JsonValue {
        let name = match parse_cache_name(arguments, "get_cache_entries") {
            Ok(name) => name,
            Err(message) => {
                return json!({
                    "status": "error",
                    "message": message
                });
            }
        };
        let Some(cache_registry) = self.cache_registry.as_ref() else {
            return unsupported_cache_response(Some(&name));
        };

        match cache_registry.entries_summary(&name).await {
            Some(entries) => json!({
                "supported": true,
                "status": "success",
                "name": name,
                "entries": entries
            }),
            None => cache_not_found_response(&name),
        }
    }

    async fn clear_cache(&self, arguments: Option<&JsonValue>) -> JsonValue {
        let name = match parse_cache_name(arguments, "clear_cache") {
            Ok(name) => name,
            Err(message) => {
                return json!({
                    "status": "error",
                    "message": message
                });
            }
        };
        let Some(cache_registry) = self.cache_registry.as_ref() else {
            return unsupported_cache_response(Some(&name));
        };

        match cache_registry.clear(&name).await {
            Some(outcome) => json!({
                "supported": true,
                "status": "success",
                "name": name,
                "beforeSize": outcome.before_size,
                "afterSize": outcome.after_size
            }),
            None => cache_not_found_response(&name),
        }
    }

    fn get_logging_filter(&self) -> JsonValue {
        let Some(logging_control) = self.logging_control.as_ref() else {
            return unsupported_logging_response();
        };
        logging_control.status_json()
    }

    fn set_logging_filter(&self, arguments: Option<&JsonValue>) -> JsonValue {
        let Some(logging_control) = self.logging_control.as_ref() else {
            return unsupported_logging_response();
        };
        let filter = match parse_logging_filter(arguments) {
            Ok(filter) => filter,
            Err(message) => {
                return json!({
                    "status": "error",
                    "message": message
                });
            }
        };

        match logging_control.set_filter(filter, "mcp:set_logging_filter") {
            Ok(state) => json!({
                "status": "success",
                "filter": state.filter,
                "source": state.source
            }),
            Err(error) => json!({
                "status": "error",
                "message": error.to_string()
            }),
        }
    }
}

fn runtime_tools() -> JsonValue {
    json!([
        {
            "name": "get_service_info",
            "description": "Retrieve masked runtime configuration and metadata for this service instance.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "get_modules",
            "description": "Retrieve registered runtime module IDs.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "reload_modules",
            "description": "Reload selected runtime modules, or all modules when omitted. Non-reloadable modules are reported as skipped.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "modules": {
                        "type": "array",
                        "items": { "type": "string" }
                    }
                }
            }
        },
        {
            "name": "get_logging_filter",
            "description": "Retrieve the current runtime logging filter.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "set_logging_filter",
            "description": "Validate and apply a new runtime logging filter without restarting the service.",
            "inputSchema": {
                "type": "object",
                "required": ["filter"],
                "properties": {
                    "filter": { "type": "string" }
                }
            }
        },
        {
            "name": "list_caches",
            "description": "Retrieve available runtime caches.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "get_cache_entries",
            "description": "Retrieve summarized entries from a named runtime cache.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" }
                }
            }
        },
        {
            "name": "clear_cache",
            "description": "Clear all entries from a named runtime cache.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" }
                }
            }
        }
    ])
}

fn parse_cache_name(arguments: Option<&JsonValue>, tool_name: &str) -> Result<String, String> {
    let Some(arguments) = arguments else {
        return Err(format!("{tool_name} arguments.name is required"));
    };
    let Some(name) = arguments.get("name") else {
        return Err(format!("{tool_name} arguments.name is required"));
    };
    name.as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("{tool_name} arguments.name must be a non-empty string"))
}

fn parse_logging_filter(arguments: Option<&JsonValue>) -> Result<String, String> {
    let Some(arguments) = arguments else {
        return Err("set_logging_filter arguments.filter is required".to_string());
    };
    let Some(filter) = arguments.get("filter") else {
        return Err("set_logging_filter arguments.filter is required".to_string());
    };
    filter
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| "set_logging_filter arguments.filter must be a non-empty string".to_string())
}

fn unsupported_logging_response() -> JsonValue {
    json!({
        "supported": false,
        "status": "unsupported",
        "message": "Runtime logging control is not available on this service."
    })
}

fn unsupported_cache_response(name: Option<&str>) -> JsonValue {
    let mut response = json!({
        "supported": false,
        "status": "unsupported",
        "message": "Cache support is not available on this service."
    });
    if let Some(name) = name {
        response["name"] = JsonValue::String(name.to_string());
    }
    response
}

fn cache_not_found_response(name: &str) -> JsonValue {
    json!({
        "supported": true,
        "status": "not_found",
        "name": name,
        "message": format!("Cache {name} was not found.")
    })
}

fn parse_reload_modules(arguments: Option<&JsonValue>) -> Result<Vec<String>, String> {
    let Some(arguments) = arguments else {
        return Ok(Vec::new());
    };
    if arguments.is_null() {
        return Ok(Vec::new());
    }
    let Some(modules) = arguments.get("modules") else {
        return Ok(Vec::new());
    };
    if modules.is_null() {
        return Ok(Vec::new());
    }
    let Some(values) = modules.as_array() else {
        return Err("reload_modules arguments.modules must be an array".to_string());
    };

    values
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    "reload_modules arguments.modules entries must be non-empty strings".to_string()
                })
        })
        .collect()
}

fn is_all_marker(module_id: &str) -> bool {
    module_id.eq_ignore_ascii_case("all")
}

fn active_module(entries: &BTreeMap<String, ModuleEntry>, module_id: &str) -> bool {
    entries.get(module_id).is_some_and(|entry| entry.active)
}

fn reload_context_failure(requested_modules: &[String], message: String) -> ReloadModulesResult {
    let failed_modules = if requested_modules.is_empty()
        || requested_modules
            .first()
            .is_some_and(|id| requested_modules.len() == 1 && is_all_marker(id))
    {
        vec!["ALL".to_string()]
    } else {
        requested_modules.to_vec()
    };

    ReloadModulesResult {
        failed: failed_modules
            .into_iter()
            .map(|module_id| ReloadFailed {
                module_id,
                message: message.clone(),
            })
            .collect(),
        ..ReloadModulesResult::default()
    }
}

fn mask_config_properties(config: &RuntimeConfig) -> bool {
    ["server.maskConfigProperties", "admin.maskConfigProperties"]
        .iter()
        .find_map(|key| config.resolved_values.get(*key))
        .and_then(serde_yaml_bool)
        .unwrap_or(true)
}

fn serde_yaml_bool(value: &serde_yaml::Value) -> Option<bool> {
    match value {
        serde_yaml::Value::Bool(value) => Some(*value),
        serde_yaml::Value::String(value) => match value.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn default_masks() -> Vec<MaskSpec> {
    [
        "authorization",
        "password",
        "secret",
        "clientSecret",
        "apiKey",
        "token",
        "portalToken",
        "controllerDiscoveryToken",
        "privateKey",
        "tlsKeyPath",
        "bootstrapKeyPath",
    ]
    .into_iter()
    .map(MaskSpec::key)
    .collect()
}

pub fn client_config_masks() -> Vec<MaskSpec> {
    [
        "client_secret",
        "clientSecret",
        "trustStorePass",
        "keyStorePass",
        "keyPass",
        "defaultCertPassword",
        "subjectToken",
        "access_token",
        "refresh_token",
        "id_token",
        "authorization",
    ]
    .into_iter()
    .map(MaskSpec::key)
    .collect()
}

fn apply_masks(value: &mut JsonValue, masks: &[MaskSpec]) {
    for mask in masks {
        match mask {
            MaskSpec::Key(key) => mask_key(value, key),
            MaskSpec::Path(path) => mask_path(value, path),
        }
    }
}

fn mask_key(value: &mut JsonValue, mask: &str) {
    match value {
        JsonValue::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == mask {
                    *value = JsonValue::String(MASKED_VALUE.to_string());
                } else {
                    mask_key(value, mask);
                }
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                mask_key(value, mask);
            }
        }
        _ => {}
    }
}

fn mask_path(value: &mut JsonValue, path: &str) {
    let mut current = value;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let Some(map) = current.as_object_mut() else {
            return;
        };
        let Some(next) = map.get_mut(part) else {
            return;
        };
        if parts.peek().is_none() {
            *next = JsonValue::String(MASKED_VALUE.to_string());
            return;
        }
        current = next;
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

struct ClientReloader;

#[async_trait]
impl ReloadableModule for ClientReloader {
    async fn reload(&self, _ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        Ok(ReloadOutcome::success("client.yml reloaded"))
    }
}

struct DirectRegistryReloader;

#[async_trait]
impl ReloadableModule for DirectRegistryReloader {
    async fn reload(&self, _ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        Ok(ReloadOutcome::success("direct-registry reloaded"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{CacheRegistry, MokaRuntimeCache};
    use crate::config::{
        BootstrapConfig, ClientConfig, DirectRegistryConfig, PortalRegistryConfig, ServerConfig,
        ServiceIdentity,
    };
    use serde::Deserialize;
    use serde_json::json;
    use std::collections::HashMap;
    use tempfile::TempDir;

    struct DelegateHandler;

    #[async_trait]
    impl RegistryHandler for DelegateHandler {}

    fn runtime_config() -> RuntimeConfig {
        let mut client_config = ClientConfig::default();
        client_config.tls.verify_hostname = false;
        client_config.oauth.token.client_credentials.client_secret = "client-secret".to_string();

        RuntimeConfig {
            bootstrap: BootstrapConfig {
                authorization: Some("Bearer startup-secret".to_string()),
                bootstrap_key_path: Some("secret-key.pem".into()),
                ..BootstrapConfig::default()
            },
            server: ServerConfig {
                service_id: "com.networknt.test-1.0.0".to_string(),
                enable_registry: true,
                tls_key_path: Some("server-key.pem".into()),
                ..ServerConfig::default()
            },
            client: Some(client_config),
            portal_registry: Some(PortalRegistryConfig {
                portal_url: "https://localhost:8438".to_string(),
                portal_query_url: None,
                portal_token: "portal-secret".to_string(),
                controller_discovery_token: "discovery-secret".to_string(),
            }),
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity {
                service_id: "com.networknt.test-1.0.0".to_string(),
                version: "1.0.0".to_string(),
                env_tag: Some("dev".to_string()),
                tags: HashMap::new(),
            },
            config_dir: "config".into(),
            external_config_dir: "config-cache".into(),
            resolved_values: HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn masks_recursive_keys_and_paths_before_storing_config() {
        let registry = ModuleRegistry::new();

        registry.register_config(
            "test/module",
            "test",
            ModuleKind::Application,
            json!({
                "password": "raw-password",
                "nested": {
                    "clientSecret": "raw-client-secret",
                    "safe": "visible"
                },
                "oauth": {
                    "token": "raw-token"
                }
            }),
            [MaskSpec::path("nested.clientSecret")],
            true,
            Some(true),
            false,
        );

        let rendered = serde_json::to_string(&registry.component_configs()).expect("json");
        assert!(!rendered.contains("raw-password"));
        assert!(!rendered.contains("raw-client-secret"));
        assert!(!rendered.contains("raw-token"));
        assert!(rendered.contains("visible"));
    }

    #[test]
    fn registers_builtin_runtime_configs_with_masked_sensitive_values() {
        let registry = ModuleRegistry::new();
        let config = runtime_config();

        registry
            .register_runtime_configs(&config)
            .expect("register runtime configs");

        let rendered = serde_json::to_string(&registry.server_info(&config)).expect("json");
        assert!(!rendered.contains("startup-secret"));
        assert!(!rendered.contains("portal-secret"));
        assert!(!rendered.contains("discovery-secret"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("server-key.pem"));
        assert!(rendered.contains("light-runtime/server"));
        assert!(rendered.contains(CLIENT_MODULE_ID));
        assert!(rendered.contains("portal-registry"));
    }

    #[test]
    fn server_info_exposes_gateway_capability_summary() {
        let registry = ModuleRegistry::new();
        let config = runtime_config();

        registry.register_config(
            "light-pingora/handler",
            "handler",
            ModuleKind::Framework,
            json!({
                "activeHandlers": ["correlation", "router"],
                "chains": {
                    "api": { "exec": ["correlation", "router"] }
                },
                "paths": [{
                    "path": "/v1/pets",
                    "method": "GET",
                    "exec": ["api"]
                }],
                "defaultHandlers": ["virtual"]
            }),
            [],
            true,
            Some(true),
            true,
        );
        registry.register_config(
            "light-pingora/router",
            "router",
            ModuleKind::Framework,
            json!({}),
            [],
            true,
            Some(true),
            true,
        );
        registry.register_config(
            "light-pingora/virtual-host",
            "virtual-host",
            ModuleKind::Framework,
            json!({ "hosts": [{ "domain": "local.test", "path": "/" }] }),
            [],
            true,
            Some(true),
            true,
        );

        let info = registry.server_info(&config);

        assert_eq!(info["capabilities"]["traffic"]["router"], true);
        assert_eq!(
            info["capabilities"]["handlers"]["active"],
            json!(["correlation", "router"])
        );
        assert_eq!(info["capabilities"]["handlers"]["chains"], json!(["api"]));
        assert_eq!(
            info["capabilities"]["hosts"][0]["domain"],
            json!("local.test")
        );
    }

    #[derive(Debug, Deserialize, Serialize)]
    #[serde(rename_all = "camelCase")]
    struct SampleConfig {
        password: String,
        public_value: String,
    }

    #[test]
    fn load_registered_resolves_and_stores_masked_config() {
        let registry = ModuleRegistry::new();
        let config_dir = TempDir::new().expect("config temp dir");
        let external_config_dir = TempDir::new().expect("external config temp dir");
        std::fs::write(
            config_dir.path().join("sample.yml"),
            "password: ${sample.password}\npublicValue: visible\n",
        )
        .expect("write sample config");
        let mut config = runtime_config();
        config.config_dir = config_dir.path().to_path_buf();
        config.external_config_dir = external_config_dir.path().to_path_buf();
        config.resolved_values.insert(
            "sample.password".to_string(),
            serde_yaml::Value::String("raw-secret".to_string()),
        );

        let loaded: SampleConfig = registry
            .load_registered(
                &config,
                "sample.yml",
                "test/sample",
                "sample",
                ModuleKind::Application,
                [],
                Some(true),
                false,
            )
            .expect("load registered config");

        assert_eq!(loaded.password, "raw-secret");
        let rendered = serde_json::to_string(&registry.component_configs()).expect("json");
        assert!(!rendered.contains("raw-secret"));
        assert!(rendered.contains("visible"));
        assert!(
            registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == "test/sample" && !entry.reloadable)
        );
    }

    #[test]
    fn load_config_uses_default_config_dir_when_local_file_is_absent() {
        let registry = ModuleRegistry::new();
        let default_config_dir = TempDir::new().expect("default config temp dir");
        let config_dir = TempDir::new().expect("config temp dir");
        let external_config_dir = TempDir::new().expect("external config temp dir");
        std::fs::write(
            default_config_dir.path().join("sample.yml"),
            "password: ${sample.password}\npublicValue: from-default\n",
        )
        .expect("write default sample config");

        let mut config = runtime_config();
        config.default_config_dir = Some(default_config_dir.path().to_path_buf());
        config.config_dir = config_dir.path().to_path_buf();
        config.external_config_dir = external_config_dir.path().to_path_buf();
        config.resolved_values.insert(
            "sample.password".to_string(),
            serde_yaml::Value::String("resolved-secret".to_string()),
        );

        let loaded: SampleConfig = registry
            .load_config(&config, "sample.yml")
            .expect("load default config");

        assert_eq!(loaded.password, "resolved-secret");
        assert_eq!(loaded.public_value, "from-default");
    }

    struct TestReloader;

    #[async_trait]
    impl ReloadableModule for TestReloader {
        async fn reload(&self, _ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
            Ok(ReloadOutcome::success("test reloaded"))
        }
    }

    #[tokio::test]
    async fn reload_modules_reports_success_skipped_and_failed_modules() {
        let registry = ModuleRegistry::new();
        registry.register_config(
            "test/restart-required",
            "restart-required",
            ModuleKind::Application,
            json!({ "enabled": true }),
            [],
            true,
            Some(true),
            false,
        );
        registry.register_config(
            "test/reloadable",
            "reloadable",
            ModuleKind::Application,
            json!({ "enabled": true }),
            [],
            true,
            Some(true),
            true,
        );
        registry.register_reloader("test/reloadable", Arc::new(TestReloader));

        let result = registry
            .reload_modules(
                ReloadContext::new(runtime_config()),
                &[
                    "test/reloadable".to_string(),
                    "test/restart-required".to_string(),
                    "test/missing".to_string(),
                ],
            )
            .await;

        assert_eq!(result.modules, vec!["test/reloadable"]);
        assert_eq!(result.reloaded, vec!["test/reloadable"]);
        assert_eq!(result.skipped[0].module_id, "test/restart-required");
        assert_eq!(result.skipped[0].reason, "requiresRestart");
        assert_eq!(result.failed[0].module_id, "test/missing");
        assert!(registry.module_summaries().iter().any(|entry| {
            entry.module_id == "test/restart-required"
                && entry
                    .last_reload
                    .as_ref()
                    .is_some_and(|status| status.status == "skipped")
        }));
        assert!(registry.module_summaries().iter().any(|entry| {
            entry.module_id == "test/reloadable"
                && entry
                    .last_reload
                    .as_ref()
                    .is_some_and(|status| status.status == "success")
        }));
    }

    #[tokio::test]
    async fn reload_modules_updates_logging_filter_from_values() {
        let registry = Arc::new(ModuleRegistry::new());
        let (logging_control, _filter_layer) = crate::logging::LoggingControl::new_for_test("info");
        crate::logging::register_logging_module(
            &registry,
            &runtime_config(),
            Arc::clone(&logging_control),
        )
        .expect("register logging module");

        let mut reload_config = runtime_config();
        reload_config.resolved_values.insert(
            crate::logging::LOGGING_FILTER_KEY.to_string(),
            serde_yaml::Value::String("info,light_gateway=debug".to_string()),
        );

        let result = registry
            .reload_modules(
                ReloadContext::new(reload_config),
                &[crate::logging::LOGGING_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![crate::logging::LOGGING_MODULE_ID]);
        assert_eq!(
            logging_control.current_state().filter,
            "info,light_gateway=debug"
        );
    }

    #[tokio::test]
    async fn reload_modules_updates_direct_registry_config() {
        let registry = ModuleRegistry::new();
        let mut config = runtime_config();
        config
            .direct_registry
            .direct_urls
            .insert("service1".to_string(), "http://localhost:8080".to_string());
        registry
            .register_runtime_configs(&config)
            .expect("register runtime configs");

        let startup_configs = registry.component_configs();
        assert_eq!(
            startup_configs["direct-registry"]["directUrls"],
            json!({"service1": "http://localhost:8080"})
        );

        let mut reload_config = config;
        reload_config
            .direct_registry
            .direct_urls
            .insert("service1".to_string(), "http://localhost:9090".to_string());

        let result = registry
            .reload_modules(ReloadContext::new(reload_config), &["ALL".to_string()])
            .await;

        assert!(
            result
                .reloaded
                .contains(&"light-runtime/direct-registry".to_string())
        );

        let reloaded_configs = registry.component_configs();
        assert_eq!(
            reloaded_configs["direct-registry"]["directUrls"],
            json!({"service1": "http://localhost:9090"})
        );
    }

    #[tokio::test]
    async fn runtime_mcp_handler_exposes_management_tools() {
        let registry = Arc::new(ModuleRegistry::new());
        let config = runtime_config();
        registry
            .register_runtime_configs(&config)
            .expect("register runtime configs");
        let handler =
            RuntimeMcpHandler::new(Arc::clone(&registry), config, Arc::new(DelegateHandler));

        let tools = handler.handle_request("tools/list", json!({})).await;
        assert_eq!(tools["tools"][0]["name"], "get_service_info");
        assert!(
            tools["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .any(|tool| tool["name"] == "reload_modules")
        );
        assert!(
            tools["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .any(|tool| tool["name"] == "clear_cache")
        );

        let info = handler
            .handle_request(
                "tools/call",
                json!({ "name": "get_service_info", "arguments": {} }),
            )
            .await;
        assert!(info.get("component").is_some());
        assert!(info.get("modules").is_some());

        let modules = handler
            .handle_request("tools/call", json!({ "name": "get_modules" }))
            .await;
        assert!(
            modules["modules"]
                .as_array()
                .is_some_and(|items| items.iter().all(JsonValue::is_string))
        );

        let reload = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "reload_modules",
                    "arguments": { "modules": ["ALL"] }
                }),
            )
            .await;
        assert_eq!(
            reload["modules"],
            json!(["light-client/client", "light-runtime/direct-registry"])
        );
        assert!(
            reload["skipped"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
    }

    #[tokio::test]
    async fn runtime_mcp_handler_reports_cache_tools_unsupported_without_registry() {
        let registry = Arc::new(ModuleRegistry::new());
        let config = runtime_config();
        let handler =
            RuntimeMcpHandler::new(Arc::clone(&registry), config, Arc::new(DelegateHandler));

        let list = handler
            .handle_request("tools/call", json!({ "name": "list_caches" }))
            .await;
        assert_eq!(list["supported"], false);
        assert_eq!(list["status"], "unsupported");

        let entries = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "get_cache_entries",
                    "arguments": { "name": "reference-data" }
                }),
            )
            .await;
        assert_eq!(entries["supported"], false);
        assert_eq!(entries["status"], "unsupported");
        assert_eq!(entries["name"], "reference-data");

        let clear = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "clear_cache",
                    "arguments": { "name": "reference-data" }
                }),
            )
            .await;
        assert_eq!(clear["supported"], false);
        assert_eq!(clear["status"], "unsupported");
        assert_eq!(clear["name"], "reference-data");
    }

    #[tokio::test]
    async fn runtime_mcp_handler_gets_and_sets_logging_filter() {
        let registry = Arc::new(ModuleRegistry::new());
        let (logging_control, _filter_layer) = crate::logging::LoggingControl::new_for_test("info");
        let handler = RuntimeMcpHandler::new(
            Arc::clone(&registry),
            runtime_config(),
            Arc::new(DelegateHandler),
        )
        .with_logging_control(Arc::clone(&logging_control));

        let initial = handler
            .handle_request("tools/call", json!({ "name": "get_logging_filter" }))
            .await;
        assert_eq!(initial["status"], "success");
        assert_eq!(initial["filter"], "info");

        let updated = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "set_logging_filter",
                    "arguments": { "filter": "info,light_gateway=debug" }
                }),
            )
            .await;
        assert_eq!(updated["status"], "success");
        assert_eq!(updated["filter"], "info,light_gateway=debug");
        assert_eq!(
            logging_control.current_state().filter,
            "info,light_gateway=debug"
        );
    }

    #[tokio::test]
    async fn runtime_mcp_handler_lists_reads_and_clears_registered_moka_cache() {
        let registry = Arc::new(ModuleRegistry::new());
        let cache_registry = Arc::new(CacheRegistry::new());
        let cache: MokaRuntimeCache<String, JsonValue> = MokaRuntimeCache::new(100);
        cache
            .insert("alpha".to_string(), json!({ "value": 1 }))
            .await;
        cache
            .insert("beta".to_string(), json!({ "value": 2 }))
            .await;
        cache_registry.register("reference-data", cache);

        let handler = RuntimeMcpHandler::new(
            Arc::clone(&registry),
            runtime_config(),
            Arc::new(DelegateHandler),
        )
        .with_cache_registry(Arc::clone(&cache_registry));

        let list = handler
            .handle_request("tools/call", json!({ "name": "list_caches" }))
            .await;
        assert_eq!(list["supported"], true);
        assert_eq!(list["status"], "success");
        assert_eq!(list["caches"], json!(["reference-data"]));

        let entries = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "get_cache_entries",
                    "arguments": { "name": "reference-data" }
                }),
            )
            .await;
        assert_eq!(entries["supported"], true);
        assert_eq!(entries["status"], "success");
        assert_eq!(entries["entries"]["alpha"]["value"], 1);
        assert_eq!(entries["entries"]["beta"]["value"], 2);

        let clear = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "clear_cache",
                    "arguments": { "name": "reference-data" }
                }),
            )
            .await;
        assert_eq!(clear["supported"], true);
        assert_eq!(clear["status"], "success");
        assert_eq!(clear["name"], "reference-data");
        assert_eq!(clear["beforeSize"], 2);
        assert_eq!(clear["afterSize"], 0);

        let entries_after_clear = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "get_cache_entries",
                    "arguments": { "name": "reference-data" }
                }),
            )
            .await;
        assert_eq!(entries_after_clear["entries"], json!({}));
    }
}
