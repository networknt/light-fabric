use std::env;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::config::RuntimeConfig;
use crate::module_registry::{
    ModuleKind, ModuleRegistry, ReloadContext, ReloadOutcome, ReloadableModule,
};
use crate::runtime::RuntimeError;
use async_trait::async_trait;
use chrono::Utc;
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::Event;
use tracing::field::{Field, Visit};
use tracing_subscriber::{
    EnvFilter, Layer, Registry, layer::Context, layer::SubscriberExt, reload,
    util::SubscriberInitExt,
};

pub const LOGGING_MODULE_ID: &str = "runtime/logging";
pub const LOGGING_FILTER_KEY: &str = "logging.filter";

type FilterReloadHandle = reload::Handle<EnvFilter, Registry>;

#[derive(Debug, Clone)]
pub struct TracingOptions {
    service_name: &'static str,
    default_filter: &'static str,
    legacy_ansi_env: Option<&'static str>,
}

impl TracingOptions {
    pub const fn new(service_name: &'static str) -> Self {
        Self {
            service_name,
            default_filter: "info",
            legacy_ansi_env: None,
        }
    }

    pub const fn with_default_filter(mut self, default_filter: &'static str) -> Self {
        self.default_filter = default_filter;
        self
    }

    pub const fn with_legacy_ansi_env(mut self, legacy_ansi_env: &'static str) -> Self {
        self.legacy_ansi_env = Some(legacy_ansi_env);
        self
    }
}

#[derive(Debug)]
pub struct TracingGuard {
    logging_control: Arc<LoggingControl>,
    log_stream: Arc<LogStreamBroadcaster>,
    log_file_access: Option<Arc<LogFileAccess>>,
    _json_file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
}

impl TracingGuard {
    pub fn logging_control(&self) -> Arc<LoggingControl> {
        Arc::clone(&self.logging_control)
    }

    pub fn log_stream(&self) -> Arc<LogStreamBroadcaster> {
        Arc::clone(&self.log_stream)
    }

    pub fn log_file_access(&self) -> Option<Arc<LogFileAccess>> {
        self.log_file_access.as_ref().map(Arc::clone)
    }
}

#[derive(Debug, Error)]
pub enum TracingInitError {
    #[error("unsupported LIGHT_LOG_FORMAT `{0}`; expected `text` or `json`")]
    UnsupportedFormat(String),
    #[error(
        "unsupported LIGHT_LOG_JSON_FILE_ROTATION `{0}`; expected `minutely`, `hourly`, `daily`, or `never`"
    )]
    UnsupportedRotation(String),
    #[error("failed to create log directory `{path}`: {source}")]
    CreateLogDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to initialize tracing subscriber: {0}")]
    Init(#[from] tracing_subscriber::util::TryInitError),
    #[error("invalid logging filter `{filter}` from {filter_source}: {message}")]
    InvalidFilter {
        filter: String,
        filter_source: String,
        message: String,
    },
}

pub fn init_tracing(options: TracingOptions) -> Result<TracingGuard, TracingInitError> {
    let initial_filter = initial_filter(&options)?;
    let filter = parse_filter(&initial_filter.filter, &initial_filter.source)?;
    let (filter_layer, filter_handle) = reload::Layer::new(filter);
    let logging_control = Arc::new(LoggingControl::new(
        filter_handle,
        options.default_filter,
        initial_filter,
    ));
    let log_stream = Arc::new(LogStreamBroadcaster::new());
    let console_format = LogFormat::from_env()?;
    let console_ansi = configured_ansi(options.legacy_ansi_env);
    let json_file = JsonFileConfig::from_env(options.service_name)?;
    let log_file_access = json_file
        .as_ref()
        .and_then(JsonFileConfig::active_file_path)
        .map(|path| Arc::new(LogFileAccess::new(path)));

    match (console_format, json_file) {
        (LogFormat::Text, None) => {
            let text_layer = text_layer(console_ansi);
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(LogStreamLayer::new(Arc::clone(&log_stream)))
                .with(text_layer)
                .try_init()?;
            Ok(TracingGuard {
                logging_control,
                log_stream,
                log_file_access,
                _json_file_guard: None,
            })
        }
        (LogFormat::Json, None) => {
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(LogStreamLayer::new(Arc::clone(&log_stream)))
                .with(json_console_layer())
                .try_init()?;
            Ok(TracingGuard {
                logging_control,
                log_stream,
                log_file_access,
                _json_file_guard: None,
            })
        }
        (LogFormat::Text, Some(json_file)) => {
            let (json_file_layer, json_file_guard) = json_file_layer(json_file)?;
            let text_layer = text_layer(console_ansi);
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(LogStreamLayer::new(Arc::clone(&log_stream)))
                .with(text_layer)
                .with(json_file_layer)
                .try_init()?;
            Ok(TracingGuard {
                logging_control,
                log_stream,
                log_file_access,
                _json_file_guard: Some(json_file_guard),
            })
        }
        (LogFormat::Json, Some(json_file)) => {
            let (json_file_layer, json_file_guard) = json_file_layer(json_file)?;
            tracing_subscriber::registry()
                .with(filter_layer)
                .with(LogStreamLayer::new(Arc::clone(&log_stream)))
                .with(json_console_layer())
                .with(json_file_layer)
                .try_init()?;
            Ok(TracingGuard {
                logging_control,
                log_stream,
                log_file_access,
                _json_file_guard: Some(json_file_guard),
            })
        }
    }
}

pub fn register_logging_module(
    registry: &Arc<ModuleRegistry>,
    runtime_config: &RuntimeConfig,
    logging_control: Arc<LoggingControl>,
) -> Result<(), RuntimeError> {
    logging_control.apply_runtime_config_startup_filter(runtime_config)?;
    registry.register_config(
        LOGGING_MODULE_ID,
        "logging",
        ModuleKind::Core,
        logging_control.status_json(),
        std::iter::empty(),
        true,
        None,
        true,
    );
    registry.register_reloader(
        LOGGING_MODULE_ID,
        Arc::new(LoggingReloader::new(logging_control)),
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoggingFilterState {
    pub filter: String,
    pub source: String,
}

pub struct LoggingControl {
    handle: FilterReloadHandle,
    default_filter: String,
    state: RwLock<LoggingFilterState>,
    startup_locked_by_env: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogStreamEvent {
    pub timestamp: String,
    pub level: String,
    pub logger: String,
    pub target: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<JsonValue>,
}

#[derive(Debug)]
pub struct LogStreamBroadcaster {
    sender: broadcast::Sender<LogStreamEvent>,
}

#[derive(Debug)]
pub struct LogFileAccess {
    path: PathBuf,
}

impl LogFileAccess {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl LogStreamBroadcaster {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(1024);
        Self { sender }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<LogStreamEvent> {
        self.sender.subscribe()
    }

    fn publish(&self, event: LogStreamEvent) {
        if self.sender.receiver_count() > 0 {
            let _ = self.sender.send(event);
        }
    }
}

impl Default for LogStreamBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

struct LogStreamLayer {
    broadcaster: Arc<LogStreamBroadcaster>,
}

impl LogStreamLayer {
    fn new(broadcaster: Arc<LogStreamBroadcaster>) -> Self {
        Self { broadcaster }
    }
}

impl<S> Layer<S> for LogStreamLayer
where
    S: tracing::Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.broadcaster.publish(LogStreamEvent::from_event(event));
    }
}

impl LogStreamEvent {
    fn from_event(event: &Event<'_>) -> Self {
        let metadata = event.metadata();
        let mut visitor = LogFieldVisitor::default();
        event.record(&mut visitor);
        let fields = if visitor.fields.is_empty() {
            None
        } else {
            Some(JsonValue::Object(visitor.fields))
        };

        Self {
            timestamp: Utc::now().to_rfc3339(),
            level: metadata.level().to_string(),
            logger: metadata.target().to_string(),
            target: metadata.target().to_string(),
            message: visitor.message.unwrap_or_default(),
            thread: std::thread::current().name().map(str::to_string),
            fields,
        }
    }
}

#[derive(Default)]
struct LogFieldVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, JsonValue>,
}

impl Visit for LogFieldVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record_value(field, JsonValue::Bool(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_value(field, json!(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_value(field, json!(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record_value(field, JsonValue::String(value.to_string()));
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record_value(field, JsonValue::String(value.to_string()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record_value(field, JsonValue::String(format!("{value:?}")));
    }
}

impl LogFieldVisitor {
    fn record_value(&mut self, field: &Field, value: JsonValue) {
        if field.name() == "message" {
            self.message = value
                .as_str()
                .map(str::to_string)
                .or_else(|| Some(value.to_string()));
            return;
        }
        self.fields.insert(field.name().to_string(), value);
    }
}

impl std::fmt::Debug for LoggingControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoggingControl")
            .field("default_filter", &self.default_filter)
            .field("state", &self.current_state())
            .field("startup_locked_by_env", &self.startup_locked_by_env)
            .finish_non_exhaustive()
    }
}

impl LoggingControl {
    fn new(
        handle: FilterReloadHandle,
        default_filter: impl Into<String>,
        initial_state: LoggingFilterState,
    ) -> Self {
        let startup_locked_by_env = initial_state.source == "env:RUST_LOG";
        Self {
            handle,
            default_filter: default_filter.into(),
            state: RwLock::new(initial_state),
            startup_locked_by_env,
        }
    }

    pub fn current_state(&self) -> LoggingFilterState {
        self.state
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .clone()
    }

    pub fn status_json(&self) -> JsonValue {
        let state = self.current_state();
        json!({
            "status": "success",
            "filter": state.filter,
            "source": state.source,
            "dynamic": true,
            "moduleId": LOGGING_MODULE_ID
        })
    }

    pub fn set_filter(
        &self,
        filter: impl Into<String>,
        source: impl Into<String>,
    ) -> Result<LoggingFilterState, RuntimeError> {
        let filter = filter.into();
        let source = source.into();
        let parsed = EnvFilter::try_new(&filter)
            .map_err(|error| RuntimeError::Config(format!("invalid logging filter: {error}")))?;
        self.handle.reload(parsed).map_err(|error| {
            RuntimeError::Config(format!("failed to reload logging filter: {error}"))
        })?;
        let state = LoggingFilterState { filter, source };
        *self.state.write().unwrap_or_else(|err| err.into_inner()) = state.clone();
        Ok(state)
    }

    pub fn reload_from_runtime_config(
        &self,
        runtime_config: &RuntimeConfig,
    ) -> Result<LoggingFilterState, RuntimeError> {
        let state = filter_from_runtime_config(runtime_config, &self.default_filter);
        self.set_filter(state.filter, state.source)
    }

    fn apply_runtime_config_startup_filter(
        &self,
        runtime_config: &RuntimeConfig,
    ) -> Result<(), RuntimeError> {
        if self.startup_locked_by_env {
            return Ok(());
        }
        self.reload_from_runtime_config(runtime_config)?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        initial_filter: &str,
    ) -> (Arc<Self>, reload::Layer<EnvFilter, Registry>) {
        let filter = EnvFilter::new(initial_filter);
        let (filter_layer, filter_handle) = reload::Layer::new(filter);
        (
            Arc::new(Self::new(
                filter_handle,
                initial_filter,
                LoggingFilterState {
                    filter: initial_filter.to_string(),
                    source: "test".to_string(),
                },
            )),
            filter_layer,
        )
    }
}

struct LoggingReloader {
    control: Arc<LoggingControl>,
}

impl LoggingReloader {
    fn new(control: Arc<LoggingControl>) -> Self {
        Self { control }
    }
}

#[async_trait]
impl ReloadableModule for LoggingReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let state = self
            .control
            .reload_from_runtime_config(&ctx.runtime_config)?;
        Ok(ReloadOutcome::success(format!(
            "logging filter set to `{}` from {}",
            state.filter, state.source
        )))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LogFormat {
    Text,
    Json,
}

impl LogFormat {
    fn from_env() -> Result<Self, TracingInitError> {
        match env::var("LIGHT_LOG_FORMAT") {
            Ok(value) => Self::parse(&value)
                .ok_or_else(|| TracingInitError::UnsupportedFormat(value.clone())),
            Err(_) => Ok(Self::Text),
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match normalized(value).as_str() {
            "text" => Some(Self::Text),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

#[derive(Debug)]
struct JsonFileConfig {
    dir: PathBuf,
    file_name: String,
    rotation: LogFileRotation,
}

impl JsonFileConfig {
    fn from_env(service_name: &str) -> Result<Option<Self>, TracingInitError> {
        if !parse_bool_env("LIGHT_LOG_JSON_FILE_ENABLED").unwrap_or(false) {
            return Ok(None);
        }

        let dir = env::var_os("LIGHT_LOG_JSON_FILE_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/var/log/light-fabric"));
        let file_name = env::var("LIGHT_LOG_JSON_FILE_NAME")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| format!("{service_name}.jsonl"));
        let rotation = match env::var("LIGHT_LOG_JSON_FILE_ROTATION") {
            Ok(value) => LogFileRotation::parse(&value)
                .ok_or_else(|| TracingInitError::UnsupportedRotation(value.clone()))?,
            Err(_) => LogFileRotation::Daily,
        };

        Ok(Some(Self {
            dir,
            file_name,
            rotation,
        }))
    }

    fn active_file_path(&self) -> Option<PathBuf> {
        (self.rotation == LogFileRotation::Never).then(|| self.dir.join(&self.file_name))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum LogFileRotation {
    Minutely,
    Hourly,
    Daily,
    Never,
}

impl LogFileRotation {
    fn parse(value: &str) -> Option<Self> {
        match normalized(value).as_str() {
            "minutely" => Some(Self::Minutely),
            "hourly" => Some(Self::Hourly),
            "daily" => Some(Self::Daily),
            "never" => Some(Self::Never),
            _ => None,
        }
    }
}

fn text_layer<S>(ansi: Option<bool>) -> tracing_subscriber::fmt::Layer<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    let layer = tracing_subscriber::fmt::layer();
    match ansi {
        Some(ansi) => layer.with_ansi(ansi),
        None => layer,
    }
}

fn json_console_layer<S>() -> tracing_subscriber::fmt::Layer<
    S,
    tracing_subscriber::fmt::format::JsonFields,
    tracing_subscriber::fmt::format::Format<tracing_subscriber::fmt::format::Json>,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    tracing_subscriber::fmt::layer().json().with_ansi(false)
}

fn json_file_layer<S>(
    config: JsonFileConfig,
) -> Result<
    (
        tracing_subscriber::fmt::Layer<
            S,
            tracing_subscriber::fmt::format::JsonFields,
            tracing_subscriber::fmt::format::Format<tracing_subscriber::fmt::format::Json>,
            tracing_appender::non_blocking::NonBlocking,
        >,
        tracing_appender::non_blocking::WorkerGuard,
    ),
    TracingInitError,
>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    std::fs::create_dir_all(&config.dir).map_err(|source| TracingInitError::CreateLogDir {
        path: config.dir.display().to_string(),
        source,
    })?;

    let appender = match config.rotation {
        LogFileRotation::Minutely => {
            tracing_appender::rolling::minutely(&config.dir, &config.file_name)
        }
        LogFileRotation::Hourly => {
            tracing_appender::rolling::hourly(&config.dir, &config.file_name)
        }
        LogFileRotation::Daily => tracing_appender::rolling::daily(&config.dir, &config.file_name),
        LogFileRotation::Never => tracing_appender::rolling::never(&config.dir, &config.file_name),
    };
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let layer = tracing_subscriber::fmt::layer()
        .json()
        .with_ansi(false)
        .with_writer(writer);

    Ok((layer, guard))
}

fn configured_ansi(legacy_ansi_env: Option<&str>) -> Option<bool> {
    parse_bool_env("LIGHT_LOG_ANSI").or_else(|| legacy_ansi_env.and_then(parse_bool_env))
}

fn parse_bool_env(name: &str) -> Option<bool> {
    env::var(name).ok().and_then(|value| parse_bool(&value))
}

fn parse_bool(value: &str) -> Option<bool> {
    match normalized(value).as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn normalized(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn initial_filter(options: &TracingOptions) -> Result<LoggingFilterState, TracingInitError> {
    match env::var("RUST_LOG") {
        Ok(value) if !value.trim().is_empty() => {
            let state = LoggingFilterState {
                filter: value,
                source: "env:RUST_LOG".to_string(),
            };
            parse_filter(&state.filter, &state.source)?;
            Ok(state)
        }
        _ => {
            let state = LoggingFilterState {
                filter: options.default_filter.to_string(),
                source: "default".to_string(),
            };
            parse_filter(&state.filter, &state.source)?;
            Ok(state)
        }
    }
}

fn filter_from_runtime_config(
    runtime_config: &RuntimeConfig,
    default_filter: &str,
) -> LoggingFilterState {
    runtime_config
        .resolved_values
        .get(LOGGING_FILTER_KEY)
        .and_then(serde_yaml_scalar_to_string)
        .map(|filter| LoggingFilterState {
            filter,
            source: format!("values.yml:{LOGGING_FILTER_KEY}"),
        })
        .unwrap_or_else(|| LoggingFilterState {
            filter: default_filter.to_string(),
            source: "default".to_string(),
        })
}

fn serde_yaml_scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(value) => {
            let trimmed = value.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_yaml::Value::Number(value) => Some(value.to_string()),
        serde_yaml::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn parse_filter(filter: &str, source: &str) -> Result<EnvFilter, TracingInitError> {
    EnvFilter::try_new(filter).map_err(|error| TracingInitError::InvalidFilter {
        filter: filter.to_string(),
        filter_source: source.to_string(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_format() {
        assert_eq!(LogFormat::parse("text"), Some(LogFormat::Text));
        assert_eq!(LogFormat::parse(" JSON "), Some(LogFormat::Json));
        assert_eq!(LogFormat::parse("yaml"), None);
    }

    #[test]
    fn parses_bool_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("YES"), Some(true));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));
        assert_eq!(parse_bool(""), None);
    }

    #[test]
    fn parses_file_rotation() {
        assert_eq!(
            LogFileRotation::parse("minutely"),
            Some(LogFileRotation::Minutely)
        );
        assert_eq!(
            LogFileRotation::parse("hourly"),
            Some(LogFileRotation::Hourly)
        );
        assert_eq!(
            LogFileRotation::parse("daily"),
            Some(LogFileRotation::Daily)
        );
        assert_eq!(
            LogFileRotation::parse("never"),
            Some(LogFileRotation::Never)
        );
        assert_eq!(LogFileRotation::parse("weekly"), None);
    }

    #[test]
    fn reads_logging_filter_from_runtime_values() {
        let mut config = RuntimeConfig {
            bootstrap: crate::config::BootstrapConfig::default(),
            server: crate::config::ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: crate::config::DirectRegistryConfig::default(),
            service_identity: crate::config::ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config-cache"),
            resolved_values: std::collections::HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        };
        config.resolved_values.insert(
            LOGGING_FILTER_KEY.to_string(),
            serde_yaml::Value::String("info,light_gateway=debug".to_string()),
        );

        let state = filter_from_runtime_config(&config, "info");

        assert_eq!(state.filter, "info,light_gateway=debug");
        assert_eq!(state.source, "values.yml:logging.filter");
    }
}
