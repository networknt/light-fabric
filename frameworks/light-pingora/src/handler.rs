use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

pub const HANDLER_FILE: &str = "handler.yml";
pub const HANDLER_LEGACY_FILE: &str = "handler.yaml";
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum HandlerMetricsLogLevel {
    Trace,
    #[default]
    Debug,
    Info,
    Warn,
    Error,
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
    pub handler_id: &'a str,
}

impl HandlerBuildContext<'_> {
    pub fn config_file<'a>(&'a self, default_file: &'a str) -> &'a str {
        default_file
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub report_handler_duration: bool,
    #[serde(default)]
    pub handler_metrics_log_level: HandlerMetricsLogLevel,
    #[serde(default = "default_base_path")]
    pub base_path: String,
    #[serde(default)]
    pub handlers: Vec<String>,
    #[serde(default)]
    pub chains: BTreeMap<String, HandlerChain>,
    #[serde(default)]
    pub paths: Vec<HandlerPath>,
    #[serde(default)]
    pub default_handlers: Vec<String>,
    #[serde(default)]
    pub additional_handlers: Vec<String>,
    #[serde(default)]
    pub additional_chains: BTreeMap<String, HandlerChain>,
    #[serde(default)]
    pub additional_paths: Vec<HandlerPath>,
}

impl Default for HandlerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_handler_duration: false,
            handler_metrics_log_level: HandlerMetricsLogLevel::Debug,
            base_path: default_base_path(),
            handlers: Vec::new(),
            chains: BTreeMap::new(),
            paths: Vec::new(),
            default_handlers: Vec::new(),
            additional_handlers: Vec::new(),
            additional_chains: BTreeMap::new(),
            additional_paths: Vec::new(),
        }
    }
}

impl HandlerConfig {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    fn apply_additional(&mut self) -> Result<(), RuntimeError> {
        self.handlers.extend(self.additional_handlers.clone());
        for (name, chain) in self.additional_chains.clone() {
            if self.chains.insert(name.clone(), chain).is_some() {
                return Err(RuntimeError::Unsupported(format!(
                    "duplicate additional handler chain `{name}` in handler.yml"
                )));
            }
        }
        self.paths.extend(self.additional_paths.clone());
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerChain {
    pub exec: Vec<String>,
}

impl<'de> Deserialize<'de> for HandlerChain {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum HandlerChainValue {
            Exec(Vec<String>),
            Object {
                #[serde(default)]
                exec: Vec<String>,
            },
        }

        match HandlerChainValue::deserialize(deserializer)? {
            HandlerChainValue::Exec(exec) => Ok(Self { exec }),
            HandlerChainValue::Object { exec } => Ok(Self { exec }),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerPath {
    pub path: String,
    pub method: String,
    #[serde(default)]
    pub exec: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandlerModuleConfig {
    pub enabled: bool,
    pub report_handler_duration: bool,
    pub handler_metrics_log_level: HandlerMetricsLogLevel,
    pub base_path: String,
    pub handlers: Vec<String>,
    pub chains: BTreeMap<String, HandlerChain>,
    pub paths: Vec<HandlerPath>,
    pub default_handlers: Vec<String>,
    pub additional_handlers: Vec<String>,
    pub additional_chains: BTreeMap<String, HandlerChain>,
    pub additional_paths: Vec<HandlerPath>,
    pub active_handlers: Vec<String>,
}

impl HandlerModuleConfig {
    fn new(config: &HandlerConfig, active_handlers: Vec<String>) -> Self {
        Self {
            enabled: config.enabled,
            report_handler_duration: config.report_handler_duration,
            handler_metrics_log_level: config.handler_metrics_log_level,
            base_path: config.base_path.clone(),
            handlers: config.handlers.clone(),
            chains: config.chains.clone(),
            paths: config.paths.clone(),
            default_handlers: config.default_handlers.clone(),
            additional_handlers: config.additional_handlers.clone(),
            additional_chains: config.additional_chains.clone(),
            additional_paths: config.additional_paths.clone(),
            active_handlers,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathMatch {
    pub path: String,
    pub method: String,
    pub params: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedHandlerChain {
    pub handler_ids: Vec<String>,
    pub path: Option<PathMatch>,
    pub default_chain: bool,
}

impl ResolvedHandlerChain {
    pub fn endpoint(&self, request_path: &str, request_method: &str) -> String {
        let path = self
            .path
            .as_ref()
            .map(|path| path.path.clone())
            .unwrap_or_else(|| request_path.to_string());
        format!("{path}@{}", request_method.to_ascii_lowercase())
    }
}

#[derive(Clone)]
pub struct ActiveHandlerSet {
    config: HandlerConfig,
    acl_guard_paths: Vec<(HandlerPath, bool)>,
    acl_guard_has_aliases: bool,
    acl_guard_active: bool,
    acl_guard_default: bool,
    acl_guard_base_path: String,
    active_handler_ids: Vec<String>,
    handlers: Vec<Arc<dyn PingoraHandler>>,
}

impl ActiveHandlerSet {
    /// Preserve legacy routing/forwarding. Reject aliases that would escape an
    /// ACL-protected literal route into an unprotected effective chain.
    pub fn check_http_acl_path(
        &self,
        path: &str,
        method: &str,
        ids: &[String],
    ) -> Result<(), String> {
        if ids.iter().any(|id| id == "access-control") {
            return canonical_http_path(path).map(|_| ()).map_err(str::to_owned);
        }
        if !self.acl_guard_active {
            return Ok(());
        }
        if !self.acl_guard_has_aliases
            && !path.contains(['%', '\\', ';'])
            && !path.contains("//")
            && !path.split('/').any(|s| matches!(s, "." | ".."))
        {
            return Ok(());
        }
        let alias = http_acl_route_alias(path);
        for (compiled, acl) in &self.acl_guard_paths {
            if handler_path_match_with_base(&self.acl_guard_base_path, compiled, &alias, method)
                .is_some()
            {
                return if *acl {
                    Err("path alias would bypass an access-control handler chain".into())
                } else {
                    Ok(())
                };
            }
        }
        if self.acl_guard_default {
            Err("path alias would bypass the default access-control chain".into())
        } else {
            Ok(())
        }
    }
    pub fn config(&self) -> &HandlerConfig {
        &self.config
    }

    pub fn active_handler_ids(&self) -> &[String] {
        &self.active_handler_ids
    }

    pub fn handlers(&self) -> &[Arc<dyn PingoraHandler>] {
        &self.handlers
    }

    pub fn resolve_handler_ids(
        &self,
        request_path: &str,
        method: &str,
    ) -> Result<Vec<String>, RuntimeError> {
        Ok(self
            .resolve_handler_chain(request_path, method)?
            .handler_ids)
    }

    pub fn resolve_handler_chain(
        &self,
        request_path: &str,
        method: &str,
    ) -> Result<ResolvedHandlerChain, RuntimeError> {
        if !self.config.enabled {
            return Ok(ResolvedHandlerChain {
                handler_ids: Vec::new(),
                path: None,
                default_chain: false,
            });
        }

        let matched = self.config.paths.iter().find_map(|path| {
            handler_path_match(&self.config, path, request_path, method)
                .map(|path_match| (path, path_match))
        });

        let (exec, path, default_chain) = if let Some((handler_path, path_match)) = matched {
            (handler_path.exec.as_slice(), Some(path_match), false)
        } else {
            (self.config.default_handlers.as_slice(), None, true)
        };

        let mut resolved = Vec::new();
        let mut visiting = Vec::new();
        resolve_exec_handlers(exec, &self.config, &mut visiting, &mut resolved)?;
        Ok(ResolvedHandlerChain {
            handler_ids: resolved,
            path,
            default_chain,
        })
    }

    pub fn materialized_path_handler_ids(
        &self,
        path: &HandlerPath,
    ) -> Result<Vec<String>, RuntimeError> {
        let mut resolved = Vec::new();
        resolve_exec_handlers(&path.exec, &self.config, &mut Vec::new(), &mut resolved)?;
        Ok(resolved)
    }

    pub fn materialized_default_handler_ids(&self) -> Result<Vec<String>, RuntimeError> {
        let mut resolved = Vec::new();
        resolve_exec_handlers(
            &self.config.default_handlers,
            &self.config,
            &mut Vec::new(),
            &mut resolved,
        )?;
        Ok(resolved)
    }

    pub fn is_handler_active(&self, handler_id: &str) -> bool {
        self.active_handler_ids.iter().any(|id| id == handler_id)
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
        mut config: HandlerConfig,
    ) -> Result<ActiveHandlerSet, RuntimeError> {
        config.apply_additional()?;
        if !config.enabled {
            return Ok(ActiveHandlerSet {
                config,
                acl_guard_paths: Vec::new(),
                acl_guard_has_aliases: false,
                acl_guard_active: false,
                acl_guard_default: false,
                acl_guard_base_path: String::new(),
                active_handler_ids: Vec::new(),
                handlers: Vec::new(),
            });
        }

        validate_handler_config(&config)?;
        let mut acl_guard_paths = Vec::new();
        let mut acl_guard_has_aliases = false;
        let mut default_ids = Vec::new();
        resolve_exec_handlers(
            &config.default_handlers,
            &config,
            &mut Vec::new(),
            &mut default_ids,
        )?;
        let acl_guard_default = default_ids.iter().any(|id| id == "access-control");
        let mut acl_guard_active = acl_guard_default;
        for path in &config.paths {
            let mut ids = Vec::new();
            resolve_exec_handlers(&path.exec, &config, &mut Vec::new(), &mut ids)?;
            let acl = ids.iter().any(|id| id == "access-control");
            let mut compiled = path.clone();
            if acl {
                acl_guard_active = true;
                compiled.path = canonical_http_pattern(&path.path).map_err(|reason| RuntimeError::Config(format!(
                    "ACL handler route {}@{} has unsupported path syntax: {reason}; use unambiguous literal segments/templates, retaining wildcard HANDLER routing", path.path, path.method)))?;
                acl_guard_has_aliases |= compiled.path != path.path;
            }
            acl_guard_paths.push((compiled, acl));
        }
        let declared_handlers = declared_handlers(&config)?;
        for handler_id in &declared_handlers {
            if !self.contains(handler_id) {
                return Err(RuntimeError::Unsupported(format!(
                    "handler `{handler_id}` is declared in handler.yml but is not registered in light-gateway"
                )));
            }
        }

        let referenced = referenced_handlers(&config, &declared_handlers)?;
        let mut active_handler_ids = Vec::new();
        let mut handlers = Vec::new();

        for handler_id in &config.handlers {
            let handler_id = handler_id.trim();
            if !referenced.contains(handler_id) {
                continue;
            }
            let descriptor = self.descriptors.get(handler_id).ok_or_else(|| {
                RuntimeError::Unsupported(format!(
                    "handler `{handler_id}` is referenced but is not registered in light-gateway"
                ))
            })?;
            let context = HandlerBuildContext {
                runtime_config,
                handler_id,
            };
            let handler = (descriptor.factory)(&context)?;
            active_handler_ids.push(handler_id.to_string());
            handlers.push(handler);
        }

        let acl_guard_base_path = if acl_guard_active {
            canonical_http_path(&config.base_path).map_err(|reason| {
                RuntimeError::Config(format!(
                    "ACL handler.basePath has unsupported path syntax: {reason}"
                ))
            })?
        } else {
            config.base_path.clone()
        };
        acl_guard_has_aliases |= acl_guard_base_path != config.base_path;
        validate_unprotected_acl_alias_routes(
            &config,
            &acl_guard_paths,
            &acl_guard_base_path,
            acl_guard_default,
        )?;
        Ok(ActiveHandlerSet {
            config,
            acl_guard_paths,
            acl_guard_has_aliases,
            acl_guard_active,
            acl_guard_default,
            acl_guard_base_path,
            active_handler_ids,
            handlers,
        })
    }
}

pub fn load_active_handlers(
    runtime_config: &RuntimeConfig,
    registry: &PingoraHandlerRegistry,
) -> Result<ActiveHandlerSet, RuntimeError> {
    let config = load_handler_config(runtime_config)?;
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

fn load_handler_config(runtime_config: &RuntimeConfig) -> Result<HandlerConfig, RuntimeError> {
    for file in [HANDLER_FILE, HANDLER_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<HandlerConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(config),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(HandlerConfig::disabled())
}

fn default_enabled() -> bool {
    true
}

fn default_base_path() -> String {
    "/".to_string()
}

fn validate_handler_config(config: &HandlerConfig) -> Result<(), RuntimeError> {
    if !config.base_path.starts_with('/') {
        return Err(RuntimeError::Unsupported(format!(
            "handler.basePath `{}` must start with `/`",
            config.base_path
        )));
    }

    for path in &config.paths {
        if !path.path.starts_with('/') {
            return Err(RuntimeError::Unsupported(format!(
                "handler path `{}` must start with `/`",
                path.path
            )));
        }
        if !is_http_method(&path.method) {
            return Err(RuntimeError::Unsupported(format!(
                "handler path method `{}` is not a supported HTTP method",
                path.method
            )));
        }
    }

    Ok(())
}

fn is_http_method(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "GET" | "POST" | "PUT" | "DELETE" | "PATCH" | "OPTIONS" | "HEAD" | "TRACE" | "CONNECT"
    )
}

fn declared_handlers(config: &HandlerConfig) -> Result<BTreeSet<String>, RuntimeError> {
    let mut handlers = BTreeSet::new();
    for handler_id in &config.handlers {
        let handler_id = handler_id.trim();
        if handler_id.is_empty() {
            return Err(RuntimeError::Unsupported(
                "handler declaration id must not be empty".to_string(),
            ));
        }
        if handler_id.contains('@') {
            return Err(RuntimeError::Unsupported(format!(
                "handler declaration `{handler_id}` must use a stable Rust handler id without @alias"
            )));
        }
        if !handlers.insert(handler_id.to_string()) {
            return Err(RuntimeError::Unsupported(format!(
                "duplicate handler declaration `{handler_id}` in handler.yml"
            )));
        }
    }
    Ok(handlers)
}

fn referenced_handlers(
    config: &HandlerConfig,
    declared_handlers: &BTreeSet<String>,
) -> Result<BTreeSet<String>, RuntimeError> {
    let mut referenced = BTreeSet::new();
    let mut visiting = Vec::new();

    for path in &config.paths {
        collect_exec_handlers(
            &path.exec,
            config,
            declared_handlers,
            &mut visiting,
            &mut referenced,
        )?;
    }

    collect_exec_handlers(
        &config.default_handlers,
        config,
        declared_handlers,
        &mut visiting,
        &mut referenced,
    )?;

    Ok(referenced)
}

fn collect_chain_handlers(
    chain_name: &str,
    config: &HandlerConfig,
    declared_handlers: &BTreeSet<String>,
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

    let chain = config.chains.get(chain_name).ok_or_else(|| {
        RuntimeError::Unsupported(format!("unknown handler chain `{chain_name}`"))
    })?;
    visiting.push(chain_name.to_string());
    collect_exec_handlers(&chain.exec, config, declared_handlers, visiting, referenced)?;
    visiting.pop();
    Ok(())
}

fn collect_exec_handlers(
    exec: &[String],
    config: &HandlerConfig,
    declared_handlers: &BTreeSet<String>,
    visiting: &mut Vec<String>,
    referenced: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    for item in exec {
        collect_exec_item_handlers(item, config, declared_handlers, visiting, referenced)?;
    }
    Ok(())
}

fn collect_exec_item_handlers(
    item: &str,
    config: &HandlerConfig,
    declared_handlers: &BTreeSet<String>,
    visiting: &mut Vec<String>,
    referenced: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    if config.chains.contains_key(item) {
        collect_chain_handlers(item, config, declared_handlers, visiting, referenced)?;
        return Ok(());
    }

    if !declared_handlers.contains(item) {
        return Err(RuntimeError::Unsupported(format!(
            "unknown handler or chain `{item}` in handler.yml"
        )));
    }
    referenced.insert(item.to_string());
    Ok(())
}

fn resolve_chain_handlers(
    chain_name: &str,
    config: &HandlerConfig,
    visiting: &mut Vec<String>,
    resolved: &mut Vec<String>,
) -> Result<(), RuntimeError> {
    if visiting.iter().any(|item| item == chain_name) {
        let mut path = visiting.clone();
        path.push(chain_name.to_string());
        return Err(RuntimeError::Unsupported(format!(
            "recursive handler chain reference: {}",
            path.join(" -> ")
        )));
    }

    let chain = config.chains.get(chain_name).ok_or_else(|| {
        RuntimeError::Unsupported(format!("unknown handler chain `{chain_name}`"))
    })?;
    visiting.push(chain_name.to_string());
    resolve_exec_handlers(&chain.exec, config, visiting, resolved)?;
    visiting.pop();
    Ok(())
}

fn resolve_exec_handlers(
    exec: &[String],
    config: &HandlerConfig,
    visiting: &mut Vec<String>,
    resolved: &mut Vec<String>,
) -> Result<(), RuntimeError> {
    for item in exec {
        if config.chains.contains_key(item) {
            resolve_chain_handlers(item, config, visiting, resolved)?;
        } else {
            resolved.push(item.to_string());
        }
    }
    Ok(())
}

fn handler_path_match(
    config: &HandlerConfig,
    handler_path: &HandlerPath,
    request_path: &str,
    method: &str,
) -> Option<PathMatch> {
    handler_path_match_with_base(&config.base_path, handler_path, request_path, method)
}

fn handler_path_match_with_base(
    base_path: &str,
    handler_path: &HandlerPath,
    request_path: &str,
    method: &str,
) -> Option<PathMatch> {
    if !handler_path.method.eq_ignore_ascii_case(method) {
        return None;
    }

    path_template_match(&handler_path.path, request_path)
        .or_else(|| {
            strip_base_path(base_path, request_path)
                .and_then(|path| path_template_match(&handler_path.path, path))
        })
        .map(|params| PathMatch {
            path: handler_path.path.clone(),
            method: handler_path.method.to_ascii_uppercase(),
            params,
        })
}

/// HTTP ACL policy identity and compiled protected-route guard spelling only.
/// Never replaces the routed/forwarded URI. Query values are outside this view.
/// Ambiguous separators and double encoding fail on ACL-protected chains.
pub fn canonical_http_path(path: &str) -> Result<String, &'static str> {
    canonical_http_path_impl(path, false)
}

// Security probe only: never becomes a routed/forwarded URI. Repeated decoding,
// separator and dot collapse conservatively recognize aliases of ACL routes;
// the ACL path validator rejects these spellings on protected chains.
fn http_acl_route_alias(path: &str) -> String {
    http_acl_route_alias_impl(path, false)
}

fn http_acl_route_alias_impl(path: &str, pattern: bool) -> String {
    // Stack reduction recognizes nested escapes in linear time: each reduction
    // consumes three bytes and emits one, instead of rescanning the whole path.
    let mut output = Vec::with_capacity(path.len());
    for byte in path.bytes() {
        output.push(byte);
        loop {
            let len = output.len();
            if len < 3 || output[len - 3] != b'%' {
                break;
            }
            let (Some(hi), Some(lo)) = (
                (output[len - 2] as char).to_digit(16),
                (output[len - 1] as char).to_digit(16),
            ) else {
                break;
            };
            output.truncate(len - 3);
            output.push((hi * 16 + lo) as u8);
        }
    }
    let decoded = String::from_utf8_lossy(&output);
    let decoded = decoded.replace('\\', "/");
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        // Java-style path parameters are ignored by some upstream routers.
        // Strip them only in this denial probe, never in the forwarded URI.
        let segment = segment.split(';').next().unwrap_or_default();
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    let alias = format!("/{}", segments.join("/"));
    canonical_http_path_impl(&alias, pattern).unwrap_or(alias)
}

pub(crate) fn canonical_http_pattern(path: &str) -> Result<String, &'static str> {
    canonical_http_path_impl(path, true)
}

/// Reject explicitly configured alias routes whose normalized view can enter a
/// protected chain. Dynamic aliases in ordinary template/wildcard captures are
/// still checked per request. This only validates configuration; it never routes.
fn validate_unprotected_acl_alias_routes(
    config: &HandlerConfig,
    paths: &[(HandlerPath, bool)],
    base_path: &str,
    default_acl: bool,
) -> Result<(), RuntimeError> {
    if !default_acl && !paths.iter().any(|(_, acl)| *acl) {
        return Ok(());
    }
    for (route, acl) in paths {
        if *acl {
            continue;
        }
        for raw in [
            route.path.clone(),
            format!("{}{}", config.base_path.trim_end_matches('/'), route.path),
        ] {
            let alias = http_acl_route_alias_impl(&raw, true);
            if alias == raw {
                continue;
            }
            let mut covered = false;
            let mut protected = default_acl;
            for (candidate, candidate_acl) in paths {
                if !candidate.method.eq_ignore_ascii_case(&route.method) {
                    continue;
                }
                let patterns = [
                    candidate.path.clone(),
                    format!("{}{}", base_path.trim_end_matches('/'), candidate.path),
                ];
                if *candidate_acl
                    && patterns
                        .iter()
                        .any(|pattern| acl_patterns_overlap(pattern, &alias))
                {
                    protected = true;
                    break;
                }
                if !candidate_acl
                    && patterns
                        .iter()
                        .any(|pattern| acl_pattern_covers(pattern, &alias))
                {
                    covered = true;
                    break;
                }
            }
            if protected && !covered {
                return Err(RuntimeError::Config(format!(
                    "unprotected handler route {}@{} has a path alias overlapping an access-control route or default chain; use an unambiguous route spelling or protect this route explicitly (do not broaden ACL grants)",
                    route.path, route.method
                )));
            }
        }
    }
    Ok(())
}

fn acl_template_segment(segment: &str) -> bool {
    segment.starts_with('{') && segment.ends_with('}')
}

// Configuration-only conservative overlap/coverage for the existing segment
// template and terminal wildcard grammar. Partial unprotected coverage cannot
// establish that all aliases are safe, so it does not mask protected overlap.
fn acl_patterns_overlap(left: &str, right: &str) -> bool {
    let left_wildcard = left.ends_with("/*");
    let right_wildcard = right.ends_with("/*");
    let left_segments = path_segments(left);
    let right_segments = path_segments(right);
    let mut left = left_segments.into_iter().peekable();
    let mut right = right_segments.into_iter().peekable();
    loop {
        match (left.next(), right.next()) {
            (Some("*"), _) if left_wildcard && left.peek().is_none() => return true,
            (_, Some("*")) if right_wildcard && right.peek().is_none() => return true,
            (None, None) => return true,
            (Some(a), Some(b)) if a == b || acl_template_segment(a) || acl_template_segment(b) => {}
            _ => return false,
        }
    }
}

fn acl_pattern_covers(pattern: &str, alias: &str) -> bool {
    let pattern_wildcard = pattern.ends_with("/*");
    let alias_wildcard = alias.ends_with("/*");
    let pattern_segments = path_segments(pattern);
    let alias_segments = path_segments(alias);
    let mut pattern = pattern_segments.into_iter().peekable();
    let mut alias = alias_segments.into_iter().peekable();
    loop {
        match (pattern.next(), alias.next()) {
            (Some("*"), _) if pattern_wildcard && pattern.peek().is_none() => return true,
            (None, None) => return true,
            (Some(a), Some(b))
                if a == b
                    || (acl_template_segment(a)
                        && !(b == "*" && alias_wildcard && alias.peek().is_none())) => {}
            _ => return false,
        }
    }
}

fn canonical_http_path_impl(path: &str, pattern: bool) -> Result<String, &'static str> {
    if path == "/" {
        return Ok(path.into());
    }
    if !path.starts_with('/') {
        return Err("path must start with slash");
    }
    let mut result = String::new();
    for segment in path.strip_suffix('/').unwrap_or(path)[1..].split('/') {
        if segment.is_empty() {
            return Err("repeated slash");
        }
        result.push('/');
        if pattern && (segment == "*" || (segment.starts_with('{') && segment.ends_with('}'))) {
            result.push_str(segment);
            continue;
        }
        let bytes = segment.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let byte = if bytes[index] == b'%' {
                if index + 2 >= bytes.len() {
                    return Err("malformed escape");
                }
                let hi = (bytes[index + 1] as char)
                    .to_digit(16)
                    .ok_or("malformed escape")?;
                let lo = (bytes[index + 2] as char)
                    .to_digit(16)
                    .ok_or("malformed escape")?;
                index += 3;
                (hi * 16 + lo) as u8
            } else {
                index += 1;
                bytes[index - 1]
            };
            if byte == b';' {
                return Err("path parameters are unsupported on access-control chains");
            }
            if byte == b'/' || byte == b'\\' || byte == b'%' || byte.is_ascii_control() {
                return Err("separator, percent or control character");
            }
            decoded.push(byte);
        }
        std::str::from_utf8(&decoded).map_err(|_| "invalid UTF-8")?;
        if decoded == b"." || decoded == b".." {
            return Err("dot segment");
        }
        const HEX: &[u8] = b"0123456789ABCDEF";
        for byte in decoded {
            if byte.is_ascii_alphanumeric() || b"-._~!$&'()+,;=:@*".contains(&byte) {
                result.push(byte as char);
            } else {
                result.push('%');
                result.push(HEX[(byte >> 4) as usize] as char);
                result.push(HEX[(byte & 15) as usize] as char);
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod canonical_path_review_tests {
    use super::*;
    #[test]
    fn alias_configuration_overlap_uses_the_legacy_segment_grammar() {
        assert!(acl_pattern_covers("/open/*", "/open/{id}"));
        assert!(acl_pattern_covers("/open//private/", "/open/private"));
        assert!(acl_patterns_overlap("/open/{id}", "/open/private"));
        assert!(acl_patterns_overlap("/open/*", "/open"));
        assert!(!acl_pattern_covers("/open/{id}", "/open/*"));
        assert!(!acl_pattern_covers("/open/*/suffix", "/open/private"));
        assert!(!acl_patterns_overlap("/open/*/suffix", "/open/private"));
        assert!(acl_pattern_covers("/open/{id}/suffix", "/open/*/suffix"));
    }
    #[test]
    fn github_segment_data_and_alias_contract() {
        for (input, output) in [
            (
                "/github/repos/o/r/labels/help%20wanted",
                "/github/repos/o/r/labels/help%20wanted",
            ),
            (
                "/github/repos/o/r/contents/dir%20name/caf%c3%a9.txt",
                "/github/repos/o/r/contents/dir%20name/caf%C3%A9.txt",
            ),
            ("/api/%70rivate/", "/api/private"),
            ("/", "/"),
        ] {
            assert_eq!(canonical_http_path(input).unwrap(), output);
        }
        for input in [
            "/github/repos/o/r/branches/feature%2Ftopic",
            "/a/%5c",
            "/a/%2e",
            "/a/..",
            "/a//b",
            "/a/%252F",
            "/a/%",
            "/a/%xy",
            "/a/%ff",
            "/api/private;x=1",
            "/api/private%3Bx=1",
        ] {
            assert!(canonical_http_path(input).is_err(), "{input}");
        }
    }
    #[test]
    fn wildcard_method_and_configured_literal_aliases_match() {
        let config: HandlerConfig = serde_yaml::from_str("handlers: [router]\npaths:\n  - path: /api/%70rivate/\n    method: '*'\n    exec: [router]\n").unwrap();
        assert!(validate_handler_config(&config).is_err());
        assert!(handler_path_match(&config, &config.paths[0], "/api/private", "GET").is_none());
    }
}

fn strip_base_path<'a>(base_path: &str, request_path: &'a str) -> Option<&'a str> {
    if base_path == "/" {
        return None;
    }
    let base_path = base_path.trim_end_matches('/');
    if request_path == base_path {
        return Some("/");
    }
    request_path
        .strip_prefix(base_path)
        .filter(|path| path.starts_with('/'))
}

fn path_template_match(template: &str, request_path: &str) -> Option<BTreeMap<String, String>> {
    if template == request_path {
        return Some(BTreeMap::new());
    }

    if let Some(prefix) = template.strip_suffix("/*") {
        let prefix = prefix.trim_end_matches('/');
        if prefix.is_empty() {
            return request_path.starts_with('/').then(BTreeMap::new);
        }
        if request_path == prefix
            || request_path
                .strip_prefix(prefix)
                .is_some_and(|suffix| suffix.starts_with('/'))
        {
            return Some(BTreeMap::new());
        }
        return None;
    }

    let template_segments = path_segments(template);
    let request_segments = path_segments(request_path);
    if template_segments.len() != request_segments.len() {
        return None;
    }

    let mut params = BTreeMap::new();
    for (template, request) in template_segments.iter().zip(request_segments.iter()) {
        if template.starts_with('{') && template.ends_with('}') && !request.is_empty() {
            let name = template.trim_start_matches('{').trim_end_matches('}');
            if name.is_empty() {
                return None;
            }
            params.insert(name.to_string(), (*request).to_string());
        } else if template != request {
            return None;
        }
    }
    Some(params)
}

fn path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
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
                id: Box::leak(ctx.handler_id.to_string().into_boxed_str()),
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

    fn simple_descriptor(id: &'static str) -> PingoraHandlerDescriptor {
        PingoraHandlerDescriptor {
            id,
            kind: PingoraHandlerKind::Application,
            factory: |ctx| {
                Ok(Arc::new(TestHandler {
                    id: Box::leak(ctx.handler_id.to_string().into_boxed_str()),
                }))
            },
        }
    }

    #[test]
    fn instantiates_only_handlers_referenced_by_paths_and_defaults() {
        ACTIVE_FACTORY_CALLS.store(0, Ordering::SeqCst);
        UNUSED_FACTORY_CALLS.store(0, Ordering::SeqCst);
        let config_dir = TempDir::new().expect("config dir");
        std::fs::write(
            config_dir.path().join(HANDLER_FILE),
            r#"
enabled: true
reportHandlerDuration: false
handlerMetricsLogLevel: DEBUG
basePath: /
handlers:
  - active
  - unused
chains:
  api:
    exec:
      - active
paths:
  - path: /v1/test
    method: GET
    exec:
      - api
defaultHandlers: []
"#,
        )
        .expect("write handler.yml");
        std::fs::write(config_dir.path().join("active.yml"), "name: active\n")
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
    fn supports_legacy_chain_list_syntax() {
        let config: HandlerConfig = serde_yaml::from_str(
            r#"
handlers:
  - active
chains:
  api:
    - active
paths: []
defaultHandlers:
  - api
"#,
        )
        .expect("parse handler config");

        assert_eq!(config.chains["api"].exec, &["active".to_string()]);
    }

    #[test]
    fn missing_handler_yml_registers_disabled_handler_module() {
        let config_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&config_dir);
        let registry = PingoraHandlerRegistry::new();
        let active = load_active_handlers(&runtime, &registry).expect("missing config is disabled");

        assert!(!active.config().enabled);
        assert!(active.active_handler_ids().is_empty());
        assert_eq!(active.config().base_path, "/");
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == HANDLER_MODULE_ID && !entry.active)
        );
    }

    #[test]
    fn loads_legacy_handler_yaml_file() {
        let config_dir = TempDir::new().expect("config dir");
        std::fs::write(
            config_dir.path().join(HANDLER_LEGACY_FILE),
            r#"
enabled: true
handlers:
  - active
paths: []
defaultHandlers:
  - active
"#,
        )
        .expect("write handler.yaml");
        let runtime = runtime_config(&config_dir);
        let registry = PingoraHandlerRegistry::new().register(simple_descriptor("active"));

        let active = load_active_handlers(&runtime, &registry).expect("load handler.yaml");

        assert_eq!(active.active_handler_ids(), &["active".to_string()]);
    }

    #[test]
    fn applies_additional_handler_config_before_validation() {
        let config_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&config_dir);
        let registry = PingoraHandlerRegistry::new().register(simple_descriptor("active"));
        let config = HandlerConfig {
            additional_handlers: vec!["active".to_string()],
            additional_chains: BTreeMap::from([(
                "api".to_string(),
                HandlerChain {
                    exec: vec!["active".to_string()],
                },
            )]),
            additional_paths: vec![HandlerPath {
                path: "/v1/test".to_string(),
                method: "GET".to_string(),
                exec: vec!["api".to_string()],
            }],
            ..HandlerConfig::default()
        };

        let active = registry
            .build_active_handlers(&runtime, config)
            .expect("additional config should be effective");

        assert_eq!(
            active
                .resolve_handler_ids("/v1/test", "GET")
                .expect("resolve additional path"),
            vec!["active".to_string()]
        );
    }

    #[test]
    fn detects_recursive_chains() {
        let config = HandlerConfig {
            handlers: vec!["active".to_string()],
            chains: BTreeMap::from([
                (
                    "a".to_string(),
                    HandlerChain {
                        exec: vec!["b".to_string()],
                    },
                ),
                (
                    "b".to_string(),
                    HandlerChain {
                        exec: vec!["a".to_string()],
                    },
                ),
            ]),
            default_handlers: vec!["a".to_string()],
            ..HandlerConfig::default()
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
            handlers: vec!["missing".to_string()],
            ..HandlerConfig::default()
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

    #[test]
    fn rejects_java_alias_syntax() {
        let config = HandlerConfig {
            handlers: vec!["com.example.ActiveHandler@active".to_string()],
            ..HandlerConfig::default()
        };
        let runtime_dir = TempDir::new().expect("config dir");
        let runtime = runtime_config(&runtime_dir);
        let registry = PingoraHandlerRegistry::new();

        let error = registry
            .build_active_handlers(&runtime, config)
            .expect_err("alias syntax should fail");

        assert!(error.to_string().contains("without @alias"));
    }

    #[test]
    fn resolves_handlers_for_exact_template_and_default_paths() {
        let config = HandlerConfig {
            handlers: vec!["active".to_string(), "unused".to_string()],
            chains: BTreeMap::from([(
                "api".to_string(),
                HandlerChain {
                    exec: vec!["active".to_string()],
                },
            )]),
            paths: vec![HandlerPath {
                path: "/oauth2/{hostId}/token".to_string(),
                method: "post".to_string(),
                exec: vec!["api".to_string()],
            }],
            default_handlers: vec!["unused".to_string()],
            ..HandlerConfig::default()
        };
        let active = ActiveHandlerSet {
            acl_guard_paths: Vec::new(),
            acl_guard_has_aliases: false,
            acl_guard_active: false,
            acl_guard_default: false,
            acl_guard_base_path: String::new(),
            config,
            active_handler_ids: Vec::new(),
            handlers: Vec::new(),
        };

        assert_eq!(
            active
                .resolve_handler_ids("/oauth2/AZZRJE52eXu3t1hseacnGQ/token", "POST")
                .expect("resolve API handlers"),
            vec!["active".to_string()]
        );
        let resolved = active
            .resolve_handler_chain("/oauth2/AZZRJE52eXu3t1hseacnGQ/token", "POST")
            .expect("resolve API chain");
        assert_eq!(
            resolved.endpoint("/oauth2/AZZRJE52eXu3t1hseacnGQ/token", "POST"),
            "/oauth2/{hostId}/token@post"
        );
        assert_eq!(
            resolved.path.as_ref().expect("matched path").params["hostId"],
            "AZZRJE52eXu3t1hseacnGQ"
        );
        let resolved = active
            .resolve_handler_chain("/spa/account/settings", "GET")
            .expect("resolve default handlers");
        assert_eq!(resolved.handler_ids, vec!["unused".to_string()]);
        assert_eq!(
            resolved.endpoint("/spa/account/settings", "GET"),
            "/spa/account/settings@get"
        );
    }

    #[test]
    fn resolves_handler_paths_after_base_path_is_removed() {
        let active = ActiveHandlerSet {
            acl_guard_paths: Vec::new(),
            acl_guard_has_aliases: false,
            acl_guard_active: false,
            acl_guard_default: false,
            acl_guard_base_path: String::new(),
            config: HandlerConfig {
                base_path: "/api".to_string(),
                paths: vec![HandlerPath {
                    path: "/v1/test".to_string(),
                    method: "GET".to_string(),
                    exec: vec!["active".to_string()],
                }],
                default_handlers: vec!["unused".to_string()],
                ..HandlerConfig::default()
            },
            active_handler_ids: Vec::new(),
            handlers: Vec::new(),
        };

        assert_eq!(
            active
                .resolve_handler_ids("/api/v1/test", "GET")
                .expect("resolve with base path"),
            vec!["active".to_string()]
        );
    }

    #[test]
    fn resolves_trailing_wildcard_paths() {
        let active = ActiveHandlerSet {
            acl_guard_paths: Vec::new(),
            acl_guard_has_aliases: false,
            acl_guard_active: false,
            acl_guard_default: false,
            acl_guard_base_path: String::new(),
            config: HandlerConfig {
                paths: vec![
                    HandlerPath {
                        path: "/customers/*".to_string(),
                        method: "GET".to_string(),
                        exec: vec!["active".to_string()],
                    },
                    HandlerPath {
                        path: "/*".to_string(),
                        method: "POST".to_string(),
                        exec: vec!["unused".to_string()],
                    },
                ],
                ..HandlerConfig::default()
            },
            active_handler_ids: Vec::new(),
            handlers: Vec::new(),
        };

        assert_eq!(
            active
                .resolve_handler_ids("/customers/CUST-1001", "GET")
                .expect("resolve customer wildcard"),
            vec!["active".to_string()]
        );
        assert_eq!(
            active
                .resolve_handler_ids("/customers/CUST-1001/preferences", "GET")
                .expect("resolve nested customer wildcard"),
            vec!["active".to_string()]
        );
        assert_eq!(
            active
                .resolve_handler_ids("/orders", "POST")
                .expect("resolve catch-all wildcard"),
            vec!["unused".to_string()]
        );
    }
}
