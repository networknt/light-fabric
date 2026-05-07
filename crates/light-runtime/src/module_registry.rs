use crate::config::RuntimeConfig;
use crate::runtime::RuntimeError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use portal_registry::RegistryHandler;
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

const MASKED_VALUE: &str = "*";

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

#[derive(Default)]
pub struct ModuleRegistry {
    entries: RwLock<BTreeMap<String, ModuleEntry>>,
    mask_config_properties: RwLock<bool>,
}

impl ModuleRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(BTreeMap::new()),
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
                "light-runtime/client",
                "client",
                ModuleKind::Core,
                serde_json::to_value(client)?,
                [],
                true,
                Some(true),
                false,
            );
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
            "plugin": {},
            "plugins": [],
            "modules": self.module_summaries()
        })
    }

    fn entries_read(&self) -> RwLockReadGuard<'_, BTreeMap<String, ModuleEntry>> {
        self.entries.read().unwrap_or_else(|err| err.into_inner())
    }

    fn entries_write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, ModuleEntry>> {
        self.entries.write().unwrap_or_else(|err| err.into_inner())
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
        }
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
                    "get_modules" => json!({ "modules": self.registry.module_summaries() }),
                    _ => self.delegate.handle_request(method, params).await,
                }
            }
            _ => self.delegate.handle_request(method, params).await,
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
            "description": "Retrieve registered runtime modules and reloadability metadata.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        }
    ])
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BootstrapConfig, ClientConfig, PortalRegistryConfig, ServerConfig, ServiceIdentity,
    };
    use serde_json::json;
    use std::collections::HashMap;

    struct DelegateHandler;

    #[async_trait]
    impl RegistryHandler for DelegateHandler {}

    fn runtime_config() -> RuntimeConfig {
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
            client: Some(ClientConfig {
                verify_hostname: false,
            }),
            portal_registry: Some(PortalRegistryConfig {
                portal_url: "https://localhost:8438".to_string(),
                portal_token: "portal-secret".to_string(),
                controller_discovery_token: "discovery-secret".to_string(),
            }),
            service_identity: ServiceIdentity {
                service_id: "com.networknt.test-1.0.0".to_string(),
                version: "1.0.0".to_string(),
                env_tag: Some("dev".to_string()),
                tags: HashMap::new(),
            },
            config_dir: "config".into(),
            external_config_dir: "config-cache".into(),
            resolved_values: HashMap::new(),
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
        assert!(!rendered.contains("server-key.pem"));
        assert!(rendered.contains("light-runtime/server"));
        assert!(rendered.contains("portal-registry"));
    }

    #[tokio::test]
    async fn runtime_mcp_handler_exposes_service_info_and_modules() {
        let registry = Arc::new(ModuleRegistry::new());
        let config = runtime_config();
        registry
            .register_runtime_configs(&config)
            .expect("register runtime configs");
        let handler =
            RuntimeMcpHandler::new(Arc::clone(&registry), config, Arc::new(DelegateHandler));

        let tools = handler.handle_request("tools/list", json!({})).await;
        assert_eq!(tools["tools"][0]["name"], "get_service_info");

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
                .is_some_and(|items| !items.is_empty())
        );
    }
}
