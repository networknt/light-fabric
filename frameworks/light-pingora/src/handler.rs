use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub const HANDLER_FILE: &str = "handler.yml";
pub const HANDLER_MODULE_ID: &str = "light-pingora/handler";
pub const HANDLER_CONFIG_NAME: &str = "handler";

pub trait PingoraHandler: Send + Sync {
    fn id(&self) -> &'static str;
}

pub type PingoraHandlerFactory =
    for<'a> fn(&HandlerBuildContext<'a>) -> Result<Arc<dyn PingoraHandler>, RuntimeError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PingoraHandlerKind {
    Core,
    Security,
    Observability,
    Traffic,
    Application,
    Plugin,
}

#[derive(Clone, Copy)]
pub struct PingoraHandlerDescriptor {
    pub id: &'static str,
    pub kind: PingoraHandlerKind,
    pub factory: PingoraHandlerFactory,
}

impl fmt::Debug for PingoraHandlerDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PingoraHandlerDescriptor")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

pub struct HandlerBuildContext<'a> {
    pub runtime_config: &'a RuntimeConfig,
    pub declaration: &'a HandlerDeclaration,
}

impl HandlerBuildContext<'_> {
    pub fn config_file<'a>(&'a self, default_file: &'a str) -> &'a str {
        self.declaration.config.as_deref().unwrap_or(default_file)
    }

    pub fn load_config<T>(&self, default_file: &str) -> Result<T, RuntimeError>
    where
        T: DeserializeOwned,
    {
        self.runtime_config
            .module_registry
            .load_config(self.runtime_config, self.config_file(default_file))
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub handlers: Vec<HandlerDeclaration>,
    #[serde(default)]
    pub chains: BTreeMap<String, Vec<String>>,
    #[serde(default, rename = "defaultHandlers")]
    pub default_handlers: Vec<String>,
}

impl HandlerConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            handlers: Vec::new(),
            chains: BTreeMap::new(),
            default_handlers: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerDeclaration {
    pub id: String,
    #[serde(default)]
    pub config: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerModuleConfig {
    pub enabled: bool,
    pub handlers: Vec<HandlerDeclaration>,
    pub chains: BTreeMap<String, Vec<String>>,
    #[serde(rename = "defaultHandlers")]
    pub default_handlers: Vec<String>,
    pub active_handlers: Vec<String>,
}

impl HandlerModuleConfig {
    fn new(config: &HandlerConfig, active_handlers: Vec<String>) -> Self {
        Self {
            enabled: config.enabled,
            handlers: config.handlers.clone(),
            chains: config.chains.clone(),
            default_handlers: config.default_handlers.clone(),
            active_handlers,
        }
    }
}

#[derive(Clone)]
pub struct ActiveHandlerSet {
    config: HandlerConfig,
    active_handler_ids: Vec<String>,
    handlers: Vec<Arc<dyn PingoraHandler>>,
}

impl ActiveHandlerSet {
    pub fn config(&self) -> &HandlerConfig {
        &self.config
    }

    pub fn active_handler_ids(&self) -> &[String] {
        &self.active_handler_ids
    }

    pub fn handlers(&self) -> &[Arc<dyn PingoraHandler>] {
        &self.handlers
    }
}

impl fmt::Debug for ActiveHandlerSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActiveHandlerSet")
            .field("enabled", &self.config.enabled)
            .field("active_handler_ids", &self.active_handler_ids)
            .field("handlers", &self.handlers.len())
            .finish()
    }
}

#[derive(Debug, Default, Clone)]
pub struct PingoraHandlerRegistry {
    descriptors: BTreeMap<String, PingoraHandlerDescriptor>,
}

impl PingoraHandlerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(mut self, descriptor: PingoraHandlerDescriptor) -> Self {
        self.try_register(descriptor)
            .expect("duplicate pingora handler descriptor");
        self
    }

    pub fn try_register(
        &mut self,
        descriptor: PingoraHandlerDescriptor,
    ) -> Result<(), RuntimeError> {
        if descriptor.id.trim().is_empty() {
            return Err(RuntimeError::Unsupported(
                "handler descriptor id must not be empty".to_string(),
            ));
        }
        if self.descriptors.contains_key(descriptor.id) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate handler descriptor `{}`",
                descriptor.id
            )));
        }
        self.descriptors
            .insert(descriptor.id.to_string(), descriptor);
        Ok(())
    }

    pub fn contains(&self, id: &str) -> bool {
        self.descriptors.contains_key(id)
    }

    pub fn build_active_handlers(
        &self,
        runtime_config: &RuntimeConfig,
        config: HandlerConfig,
    ) -> Result<ActiveHandlerSet, RuntimeError> {
        if !config.enabled {
            return Ok(ActiveHandlerSet {
                config,
                active_handler_ids: Vec::new(),
                handlers: Vec::new(),
            });
        }

        let declarations = declaration_map(&config)?;
        for declaration in &config.handlers {
            if !self.contains(&declaration.id) {
                return Err(RuntimeError::Unsupported(format!(
                    "handler `{}` is declared in handler.yml but is not registered in light-gateway",
                    declaration.id
                )));
            }
        }

        let referenced = referenced_handlers(&config, &declarations)?;
        let mut active_handler_ids = Vec::new();
        let mut handlers = Vec::new();

        for declaration in &config.handlers {
            if !declaration.enabled || !referenced.contains(&declaration.id) {
                continue;
            }
            let descriptor = self.descriptors.get(&declaration.id).ok_or_else(|| {
                RuntimeError::Unsupported(format!(
                    "handler `{}` is referenced but is not registered in light-gateway",
                    declaration.id
                ))
            })?;
            let context = HandlerBuildContext {
                runtime_config,
                declaration,
            };
            let handler = (descriptor.factory)(&context)?;
            active_handler_ids.push(declaration.id.clone());
            handlers.push(handler);
        }

        Ok(ActiveHandlerSet {
            config,
            active_handler_ids,
            handlers,
        })
    }
}

pub fn load_active_handlers(
    runtime_config: &RuntimeConfig,
    registry: &PingoraHandlerRegistry,
) -> Result<ActiveHandlerSet, RuntimeError> {
    let config = match runtime_config
        .module_registry
        .load_config::<HandlerConfig>(runtime_config, HANDLER_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == HANDLER_FILE => HandlerConfig::disabled(),
        Err(error) => return Err(error),
    };
    let active_handlers = registry.build_active_handlers(runtime_config, config)?;
    let module_config = HandlerModuleConfig::new(
        active_handlers.config(),
        active_handlers.active_handler_ids().to_vec(),
    );
    runtime_config.module_registry.register_loaded_config(
        HANDLER_MODULE_ID,
        HANDLER_CONFIG_NAME,
        ModuleKind::Framework,
        &module_config,
        [],
        active_handlers.config().enabled,
        Some(active_handlers.config().enabled),
        false,
    )?;
    Ok(active_handlers)
}

fn default_enabled() -> bool {
    true
}

fn declaration_map(
    config: &HandlerConfig,
) -> Result<BTreeMap<String, &HandlerDeclaration>, RuntimeError> {
    let mut declarations = BTreeMap::new();
    for declaration in &config.handlers {
        let id = declaration.id.trim();
        if id.is_empty() {
            return Err(RuntimeError::Unsupported(
                "handler declaration id must not be empty".to_string(),
            ));
        }
        if declarations.insert(id.to_string(), declaration).is_some() {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate handler declaration `{id}` in handler.yml"
            )));
        }
    }
    Ok(declarations)
}

fn referenced_handlers(
    config: &HandlerConfig,
    declarations: &BTreeMap<String, &HandlerDeclaration>,
) -> Result<BTreeSet<String>, RuntimeError> {
    let mut referenced = BTreeSet::new();
    let mut visiting = Vec::new();

    for chain_name in config.chains.keys() {
        collect_chain_handlers(
            chain_name,
            config,
            declarations,
            &mut visiting,
            &mut referenced,
        )?;
    }

    for item in &config.default_handlers {
        collect_exec_item_handlers(item, config, declarations, &mut visiting, &mut referenced)?;
    }

    Ok(referenced)
}

fn collect_chain_handlers(
    chain_name: &str,
    config: &HandlerConfig,
    declarations: &BTreeMap<String, &HandlerDeclaration>,
    visiting: &mut Vec<String>,
    referenced: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    if visiting.iter().any(|item| item == chain_name) {
        let mut path = visiting.clone();
        path.push(chain_name.to_string());
        return Err(RuntimeError::Unsupported(format!(
            "recursive handler chain reference: {}",
            path.join(" -> ")
        )));
    }

    let exec = config.chains.get(chain_name).ok_or_else(|| {
        RuntimeError::Unsupported(format!("unknown handler chain `{chain_name}`"))
    })?;
    visiting.push(chain_name.to_string());
    for item in exec {
        collect_exec_item_handlers(item, config, declarations, visiting, referenced)?;
    }
    visiting.pop();
    Ok(())
}

fn collect_exec_item_handlers(
    item: &str,
    config: &HandlerConfig,
    declarations: &BTreeMap<String, &HandlerDeclaration>,
    visiting: &mut Vec<String>,
    referenced: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    if config.chains.contains_key(item) {
        collect_chain_handlers(item, config, declarations, visiting, referenced)?;
        return Ok(());
    }

    let declaration = declarations.get(item).ok_or_else(|| {
        RuntimeError::Unsupported(format!("unknown handler or chain `{item}` in handler.yml"))
    })?;
    if declaration.enabled {
        referenced.insert((*item).to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig, ServiceIdentity,
    };
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    static ACTIVE_FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);
    static UNUSED_FACTORY_CALLS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Deserialize)]
    struct TestHandlerConfig {
        name: String,
    }

    struct TestHandler {
        id: &'static str,
    }

    impl PingoraHandler for TestHandler {
        fn id(&self) -> &'static str {
            self.id
        }
    }

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

    fn descriptor(id: &'static str) -> PingoraHandlerDescriptor {
        fn build(
            ctx: &HandlerBuildContext<'_>,
            counter: &'static AtomicUsize,
            default_config_file: &'static str,
        ) -> Result<Arc<dyn PingoraHandler>, RuntimeError> {
            let config = ctx.load_config::<TestHandlerConfig>(default_config_file)?;
            assert!(!config.name.is_empty());
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(TestHandler {
                id: Box::leak(ctx.declaration.id.clone().into_boxed_str()),
            }))
        }

        match id {
            "active" => PingoraHandlerDescriptor {
                id,
                kind: PingoraHandlerKind::Application,
                factory: |ctx| build(ctx, &ACTIVE_FACTORY_CALLS, "active.yml"),
            },
            "unused" => PingoraHandlerDescriptor {
                id,
                kind: PingoraHandlerKind::Application,
                factory: |ctx| build(ctx, &UNUSED_FACTORY_CALLS, "unused.yml"),
            },
            _ => unreachable!("test descriptor id"),
        }
    }

    #[test]
    fn instantiates_only_handlers_referenced_by_chains() {
        ACTIVE_FACTORY_CALLS.store(0, Ordering::SeqCst);
        UNUSED_FACTORY_CALLS.store(0, Ordering::SeqCst);
        let config_dir = TempDir::new().expect("config dir");
        std::fs::write(
            config_dir.path().join(HANDLER_FILE),
            r#"
enabled: true
handlers:
  - id: active
    config: custom-active.yml
  - id: unused
chains:
  api:
    - active
defaultHandlers:
  - api
"#,
        )
        .expect("write handler.yml");
        std::fs::write(
            config_dir.path().join("custom-active.yml"),
            "name: active\n",
        )
        .expect("write active handler config");

        let runtime = runtime_config(&config_dir);
        let registry = PingoraHandlerRegistry::new()
            .register(descriptor("active"))
            .register(descriptor("unused"));
        let active = load_active_handlers(&runtime, &registry).expect("load active handlers");

        assert_eq!(active.active_handler_ids(), &["active".to_string()]);
        assert_eq!(active.handlers().len(), 1);
        assert_eq!(ACTIVE_FACTORY_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(UNUSED_FACTORY_CALLS.load(Ordering::SeqCst), 0);
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == HANDLER_MODULE_ID && entry.active)
        );
    }

    #[test]
    fn missing_handler_yml_registers_disabled_handler_module() {
        let config_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&config_dir);
        let registry = PingoraHandlerRegistry::new();
        let active = load_active_handlers(&runtime, &registry).expect("missing config is disabled");

        assert!(!active.config().enabled);
        assert!(active.active_handler_ids().is_empty());
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == HANDLER_MODULE_ID && !entry.active)
        );
    }

    #[test]
    fn detects_recursive_chains() {
        let config = HandlerConfig {
            enabled: true,
            handlers: vec![HandlerDeclaration {
                id: "active".to_string(),
                config: None,
                enabled: true,
            }],
            chains: BTreeMap::from([
                ("a".to_string(), vec!["b".to_string()]),
                ("b".to_string(), vec!["a".to_string()]),
            ]),
            default_handlers: vec![],
        };
        let runtime_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&runtime_dir);
        let registry = PingoraHandlerRegistry::new().register(descriptor("active"));

        let error = registry
            .build_active_handlers(&runtime, config)
            .expect_err("recursive chain should fail");

        assert!(error.to_string().contains("recursive handler chain"));
    }

    #[test]
    fn rejects_declared_handler_missing_from_registry() {
        let config = HandlerConfig {
            enabled: true,
            handlers: vec![HandlerDeclaration {
                id: "missing".to_string(),
                config: None,
                enabled: true,
            }],
            chains: BTreeMap::new(),
            default_handlers: vec![],
        };
        let runtime_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&runtime_dir);
        let registry = PingoraHandlerRegistry::new();

        let error = registry
            .build_active_handlers(&runtime, config)
            .expect_err("missing descriptor should fail");

        assert!(
            error
                .to_string()
                .contains("is declared in handler.yml but is not registered")
        );
    }
}
