use crate::init_error::InitError;
use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use futures_util::FutureExt;
pub use light_runtime::RuntimeConfig;
use light_runtime::{
    AdmissionGate, BoundTransport, LifecycleRegistrar, LightRuntimeBuilder, RuntimeError,
    ShutdownContext, TransportRuntime,
};
use light_security::SecurityRuntime;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use serverless_aws::{
    invoke::{InvokeOptions, LambdaInvokeBackend, SystemClock},
    limits::Limits,
    target::{EndpointOverride, GatewayIdentity, Targets},
    v1::EventConfig,
};
use serverless_core::{
    Backend, ExecutionPlan, Finalizers, HandlerId, Target,
    compat::{COMPATIBILITY, Compatibility},
    plan::{Binding, HandlerFactory, Registry, TargetRoutes},
};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    panic::AssertUnwindSafe,
    path::{Component, Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
mod lifecycle;
mod preparation;
pub use preparation::{
    Cleanup as PreparationCleanup, PreparationContext, PreparationGate, PreparationProfile,
    PreparationSnapshot,
};

#[derive(Clone, Copy)]
pub struct ConfigPreparationTransport;
#[async_trait]
impl TransportRuntime for ConfigPreparationTransport {
    type Handle = ();
    async fn bind(
        &self,
        _: &RuntimeConfig,
        _: &LifecycleRegistrar,
        _: &AdmissionGate,
        _: CancellationToken,
    ) -> Result<BoundTransport<()>, RuntimeError> {
        Err(RuntimeError::Unsupported(
            "configuration preparation does not bind a transport".into(),
        ))
    }
    async fn stop(&self, _: &mut (), _: &ShutdownContext) -> Result<(), RuntimeError> {
        Ok(())
    }
}
#[derive(Clone)]
pub struct Environment {
    pub region: Option<String>,
    pub function_name: String,
    pub function_version: String,
    pub memory_mb: u32,
    pub allow_insecure_test_endpoints: bool,
}
impl Environment {
    pub fn from_env() -> Result<Self, InitError> {
        let required = |name| {
            std::env::var(name).map_err(|_| {
                InitError::InvalidConfiguration("environment: missing Lambda identity")
            })
        };
        Ok(Self {
            region: std::env::var("AWS_REGION").ok(),
            function_name: required("AWS_LAMBDA_FUNCTION_NAME")?,
            function_version: required("AWS_LAMBDA_FUNCTION_VERSION")?,
            memory_mb: required("AWS_LAMBDA_FUNCTION_MEMORY_SIZE")?
                .parse()
                .map_err(|_| {
                    InitError::InvalidConfiguration("environment: invalid Lambda memory")
                })?,
            allow_insecure_test_endpoints: std::env::var(
                "LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS",
            )
            .as_deref()
                == Ok("1"),
        })
    }
}
#[derive(Clone, Copy, Debug)]
pub struct Budget {
    pub ingress: Duration,
    pub response_reserve: Duration,
    pub post_reserve: Duration,
}
impl Budget {
    pub fn validate(&self) -> Result<(), InitError> {
        if self.ingress.is_zero()
            || self.post_reserve.is_zero()
            || self.post_reserve >= self.response_reserve
        {
            return Err(InitError::InvalidConfiguration(
                "light-lambda.yml: require positive ingress and 0 < postReserveMs < responseReserveMs",
            ));
        }
        Ok(())
    }
}
/// Immutable published wiring. Only cold-start accounting and dependency-owned
/// key/credential caches mutate; configuration and plan are never reloaded.
pub struct Snapshot {
    pub(crate) resource_owners: Vec<Arc<dyn RuntimeResource>>,
    pub(crate) plan: Arc<ExecutionPlan>,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) finalizers: Finalizers,
    pub(crate) budget: Budget,
    pub(crate) limits: Limits,
    pub(crate) force_base64: bool,
    pub(crate) cold: AtomicBool,
    pub(crate) digest: String,
    pub(crate) layer: Option<String>,
    pub security: Option<Arc<SecurityRuntime>>,
}
impl Snapshot {
    pub fn config_digest(&self) -> &str {
        &self.digest
    }
    pub fn layer_identity(&self) -> Option<&str> {
        self.layer.as_deref()
    }
}
/// Explicit phase boundary: WP9a/WP12 supply real factories. Disabled entries are
/// derived from effective config, never replaced by success/no-op handlers.
pub struct HandlerRegistration {
    pub id: HandlerId,
    pub config_file: &'static str,
    pub factory: Option<Arc<dyn HandlerFactoryProvider>>,
}
pub type PreparedSpecificationFactory = (Arc<dyn HandlerFactory>, Option<[u8; 32]>);
/// Immutable resources from one Specification source capture in this Init.
pub struct SpecificationResources {
    pub catalogue: gateway_policy::openapi_operation::OperationCatalogue,
    pub profile: Arc<gateway_policy::openapi_operation::BasePathProfile>,
}
pub struct PreparedSpecificationResources {
    pub prepared: PreparedSpecificationFactory,
    pub resources: Option<Arc<SpecificationResources>>,
}
/// Pure factory preparation is shared by offline validation and runtime Init.
/// Only runtime Init activates resources, before publishing any Snapshot.
#[async_trait::async_trait]
pub trait RuntimeLifecycle: Send + Sync {
    #[must_use = "the activate future must be awaited to perform the operation"]
    async fn activate(&self) -> Result<(), InitError>;
    /// Idempotent release of owned resources on failed Init or final Snapshot drop.
    /// Existing lifecycles retain their prior drop-based behavior by default.
    #[must_use = "the deactivate future must be awaited to perform the operation"]
    async fn deactivate(&self) {}
}
pub struct PreparedValidatorResources {
    pub factory: Arc<dyn HandlerFactory>,
    pub lifecycle: Option<Arc<dyn RuntimeLifecycle>>,
}
pub trait RuntimeResource: Send + Sync {}
impl<T: Send + Sync> RuntimeResource for T {}
pub struct PreparedPrecision {
    pub session: Arc<dyn crate::validator_ipc::Session>,
    pub settings: Arc<crate::validator_ipc::Settings>,
}
pub struct PreparedSpecificationRuntime {
    pub specification: PreparedSpecificationResources,
    pub precision: Option<Arc<PreparedPrecision>>,
    pub owners: Vec<Arc<dyn RuntimeResource>>,
}
pub trait HandlerFactoryProvider: Send + Sync {
    /// Trusted opt-in requirement, checked even when this registration is disabled.
    /// A layer cannot select or relax this profile; ordinary providers have none.
    fn required_execution_profile(
        &self,
    ) -> Option<&crate::required_validator_profile::RequiredValidatorProfile> {
        None
    }
    /// Opt-in trusted preparation lane; defaults preserve ordinary profiles.
    fn runtime_preparation_profile(&self) -> Option<PreparationProfile> {
        None
    }
    fn prepare_specification_runtime_owned<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        context: Option<Arc<PreparationContext>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PreparedSpecificationRuntime, InitError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if let Some(context) = &context {
                context.check()?;
            }
            let prepared = self.prepare_specification_runtime(config, security).await?;
            if let (Some(context), Some(precision)) = (&context, &prepared.precision)
                && let Some(scope) = precision.session.initialization_scope()
            {
                context.register_scope(scope)?;
            }
            Ok(prepared)
        })
    }
    fn prepare_validator_runtime_owned(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        precision: Option<Arc<PreparedPrecision>>,
        _context: Option<Arc<PreparationContext>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        self.prepare_validator_runtime(config, security, resources, precision)
    }
    /// A wired factory may still reject all enabled application configurations
    /// pending explicit engine/profile admission. Inventory must keep that gap.
    fn application_available(&self) -> bool {
        true
    }
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError>;

    /// Only Specification declarations use this cold-start entry. The default
    /// preserves existing providers and performs exactly one factory creation.
    fn prepare_specification(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationFactory, InitError> {
        Ok((self.create(config, security)?, None))
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        Ok(PreparedSpecificationResources {
            prepared: self.prepare_specification(config, security)?,
            resources: None,
        })
    }
    fn prepare_specification_runtime<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PreparedSpecificationRuntime, InitError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok(PreparedSpecificationRuntime {
                specification: self.prepare_specification_resources(config, security)?,
                precision: None,
                owners: vec![],
            })
        })
    }
    fn prepare_validator_runtime(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        _precision: Option<Arc<PreparedPrecision>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        self.prepare_validator_resources(config, security, resources)
    }
    fn prepare_validator(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        _resources: Option<Arc<SpecificationResources>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.create(config, security)
    }
    fn prepare_validator_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        Ok(PreparedValidatorResources {
            factory: self.prepare_validator(config, security, resources)?,
            lifecycle: None,
        })
    }
    /// Same immutable per-Init program for both phases; legacy providers retain
    /// their existing create behavior, including deliberate test factories.
    fn prepare_transformer(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        _program: Option<Arc<crate::transformer_program::Candidate>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.create(config, security)
    }
    fn prepare_router_runtime(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        _running: bool,
        _limits: &Limits,
    ) -> Result<crate::router_policy::PreparedRouterRuntime, InitError> {
        Ok(crate::router_policy::PreparedRouterRuntime {
            prepared: self.prepare_router_resources(config, security)?,
            backend: None,
            lifecycle: None,
        })
    }
    fn prepare_router_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<crate::router_policy::PreparedRouterResources, InitError> {
        Ok(crate::router_policy::PreparedRouterResources {
            factory: self.create(config, security)?,
            resources: None,
        })
    }
}
pub struct InitInput {
    pub embedded: &'static [EmbeddedConfigFile],
    pub local_dir: PathBuf,
    pub default_dir: Option<PathBuf>,
    pub external_dir: PathBuf,
    pub environment: Environment,
    /// Trusted deployment input using the existing WP8 identity model. WP10 owns
    /// its source; optional business-event account metadata is never used here.
    pub gateway_identity: GatewayIdentity,
    pub handlers: Vec<HandlerRegistration>,
    pub finalizers: Finalizers,
    pub backend: BackendConfiguration,
    #[cfg(test)]
    pub(crate) panic_during_preparation: bool,
}
impl InitInput {
    pub fn from_env(
        embedded: &'static [EmbeddedConfigFile],
        gateway_identity: GatewayIdentity,
    ) -> Result<Self, InitError> {
        Ok(Self {
            embedded,
            local_dir: "config".into(),
            default_dir: None,
            external_dir: std::env::var_os("LIGHT_LAMBDA_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "/opt".into()),
            environment: Environment::from_env()?,
            gateway_identity,
            handlers: Vec::new(),
            finalizers: Vec::new(),
            backend: BackendConfiguration::Production,
            #[cfg(test)]
            panic_during_preparation: false,
        })
    }
}
/// Supplying an SDK config bypasses all default credential/region providers.
/// Component tests use synthetic static credentials or an explicit Backend double.
pub enum BackendConfiguration {
    Production,
    /// Local validation only; never constructs an AWS client or provider chain.
    Offline,
    Sdk(Box<aws_sdk_lambda::Config>),
    Injected(Arc<dyn Backend>),
}

fn invalid(message: &'static str) -> InitError {
    InitError::InvalidConfiguration(message)
}
fn load<T: DeserializeOwned>(config: &RuntimeConfig, file: &'static str) -> Result<T, InitError> {
    config
        .module_registry
        .load_config(config, file)
        .map_err(|_| {
            invalid(match file {
                "light-lambda.yml" => "light-lambda.yml: invalid configuration",
                "handler.yml" => "handler.yml: invalid configuration",
                "lambda-proxy.yml" => "lambda-proxy.yml: invalid configuration",
                "lambda-app.yml" => "lambda-app.yml: invalid configuration",
                _ => "configuration: invalid handler file",
            })
        })
}
fn optional<T: DeserializeOwned + Default>(
    config: &RuntimeConfig,
    file: &'static str,
) -> Result<T, InitError> {
    match config.module_registry.load_config(config, file) {
        Ok(v) => Ok(v),
        Err(RuntimeError::MissingConfig(_)) => Ok(T::default()),
        Err(_) => Err(invalid("configuration: invalid optional file")),
    }
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct LambdaConfig {
    stage: String,
    stage_variables: Option<BTreeMap<String, String>>,
    account_id: Option<String>,
    api_id: Option<String>,
    domain_name: Option<String>,
    resource_id: Option<String>,
    ingress_timeout_ms: u64,
    response_reserve_ms: u64,
    post_reserve_ms: u64,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_invoke_payload_bytes: usize,
    invoke_max_retries: u32,
}
impl Default for LambdaConfig {
    fn default() -> Self {
        Self {
            stage: String::new(),
            stage_variables: None,
            account_id: None,
            api_id: None,
            domain_name: None,
            resource_id: None,
            ingress_timeout_ms: 60000,
            response_reserve_ms: 250,
            post_reserve_ms: 100,
            max_request_bytes: 1048576,
            max_response_bytes: 1048576,
            max_invoke_payload_bytes: 6291456,
            invoke_max_retries: 0,
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct AppConfig {
    // Java service metadata; audit policy implementation remains deferred to WP12.
    #[serde(rename = "lambdaAppId")]
    _lambda_app_id: Option<String>,
    encode_base64_request: bool,
    encode_base64_response: bool,
}
#[derive(Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
struct ProxyConfig {
    enabled: bool,
    functions: BTreeMap<String, String>,
    region: Option<String>,
    endpoint_override: Option<String>,
    api_call_timeout: u64,
    api_call_attempt_timeout: u64,
    connection_timeout: u64,
    read_timeout: u64,
    write_timeout: u64,
    log_type: Option<String>,
    metrics_injection: bool,
    metrics_name: Option<String>,
}
impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            functions: BTreeMap::new(),
            region: None,
            endpoint_override: None,
            api_call_timeout: 60000,
            api_call_attempt_timeout: 20000,
            connection_timeout: 10000,
            read_timeout: 60000,
            write_timeout: 60000,
            log_type: None,
            metrics_injection: true,
            metrics_name: None,
        }
    }
}
struct ProxyFactory(TargetRoutes);
impl HandlerFactory for ProxyFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Dispatch(self.0.clone()))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Manifest {
    schema: String,
    files: Vec<ManifestFile>,
    config_digest: String,
    created_at: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    path: String,
    sha256: String,
}
fn directory(path: &Path) -> Result<(), InitError> {
    if !path.is_dir() {
        return Err(InitError::MissingDirectory(path.to_owned()));
    }
    for entry in std::fs::read_dir(path)
        .map_err(|_| invalid("configuration: required deployment directory unreadable"))?
    {
        let path = entry
            .map_err(|_| invalid("configuration: deployment directory entry unreadable"))?
            .path();
        if path
            .extension()
            .is_some_and(|s| s == "yml" || s == "yaml" || s == "json")
        {
            std::fs::File::open(path)
                .map_err(|_| invalid("configuration: deployment configuration file unreadable"))?;
        }
    }
    Ok(())
}
fn manifest(path: &Path) -> Result<Option<String>, InitError> {
    let p = path.join("light-lambda-layer.json");
    if !p
        .try_exists()
        .map_err(|_| invalid("light-lambda-layer.json: unreadable"))?
    {
        return Ok(None);
    }
    let m: Manifest = serde_json::from_slice(
        &std::fs::read(p).map_err(|_| invalid("light-lambda-layer.json: unreadable"))?,
    )
    .map_err(|_| invalid("light-lambda-layer.json: invalid manifest"))?;
    let hex = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    if m.schema != "light-lambda-layer/v1" || !hex(&m.config_digest) || m.created_at.is_empty() {
        return Err(invalid("light-lambda-layer.json: invalid metadata"));
    }
    let root = path
        .canonicalize()
        .map_err(|_| invalid("light-lambda-layer.json: invalid root"))?;
    let mut seen = std::collections::BTreeSet::new();
    for file in m.files {
        if file.path.is_empty()
            || !Path::new(&file.path)
                .components()
                .all(|c| matches!(c, Component::Normal(_)))
            || !seen.insert(file.path.clone())
            || !hex(&file.sha256)
        {
            return Err(invalid("light-lambda-layer.json: invalid file entry"));
        }
        let p = root
            .join(&file.path)
            .canonicalize()
            .map_err(|_| invalid("light-lambda-layer.json: listed file missing"))?;
        if !p.starts_with(&root) {
            return Err(invalid(
                "light-lambda-layer.json: file outside deployment directory",
            ));
        }
        let bytes = std::fs::read(p)
            .map_err(|_| invalid("light-lambda-layer.json: listed file unreadable"))?;
        if format!("{:x}", Sha256::digest(bytes)) != file.sha256.to_ascii_lowercase() {
            return Err(invalid("light-lambda-layer.json: file digest mismatch"));
        }
    }
    Ok(Some(m.config_digest.to_ascii_lowercase()))
}

/// Loads configuration locally without registration, bind, reload, or Config Server I/O.
pub async fn prepare_local(input: &InitInput) -> Result<RuntimeConfig, InitError> {
    prepare_local_with_sources(input)
        .await
        .map(|(runtime, _)| runtime)
}

async fn prepare_local_with_sources(
    input: &InitInput,
) -> Result<(RuntimeConfig, config_loader::BoundedLocalSources), InitError> {
    directory(&input.external_dir)?;
    #[cfg(test)]
    if input.panic_during_preparation {
        panic!("test-only-init-secret-marker");
    }
    // Prevent registry-client construction before shared preparation; same loader precedence.
    let sources = config_loader::BoundedLocalSources::capture(
        input.embedded,
        input.default_dir.as_deref(),
        &input.local_dir,
        &input.external_dir,
    )
    .map_err(bounded_source_error)?;
    let values = sources.values(false).map_err(bounded_source_error)?;
    let password = std::env::var("light_4j_config_password").ok();
    let loader = config_loader::ConfigLoader::from_values(values, password.as_deref(), None)
        .map_err(|_| InitError::Preparation)?;
    if let Some(server) = config_loader::load_config_from_sources(
        &loader,
        input.embedded,
        input.default_dir.as_deref(),
        &input.local_dir,
        Some(&input.external_dir),
        "server.yml",
    )
    .map_err(|_| InitError::Preparation)?
        && server
            .get("enableRegistry")
            .and_then(serde_yaml::Value::as_bool)
            == Some(true)
    {
        return Err(invalid("server.yml: registry unsupported in Lambda"));
    }
    let mut builder = LightRuntimeBuilder::new(ConfigPreparationTransport)
        .with_embedded_config(input.embedded)
        .with_config_dir(&input.local_dir)
        .with_external_config_dir(&input.external_dir);
    if let Some(path) = &input.default_dir {
        builder = builder.with_default_config_dir(path);
    }
    let runtime = builder.build();
    let router = input
        .handlers
        .iter()
        .any(|handler| handler.id == HandlerId::Router && handler.factory.is_some());
    let config = runtime
        .prepare_local_config_from_sources(&sources, router)
        .await
        .map_err(|_| InitError::Preparation)?;
    Ok((config, sources))
}

fn bounded_source_error(error: config_loader::ConfigError) -> InitError {
    match error {
        config_loader::ConfigError::Convert(message)
            if message == "transformer: unsupported rule profile" =>
        {
            invalid("transformer: unsupported rule profile")
        }
        _ => InitError::Preparation,
    }
}
pub async fn initialize(input: InitInput) -> Result<Arc<Snapshot>, InitError> {
    // The process entry installs telemetry/panic redaction once. Pure preparation
    // does not change global subscribers or hooks in an embedding test harness.
    AssertUnwindSafe(initialize_inner(input))
        .catch_unwind()
        .await
        .unwrap_or(Err(InitError::Panic))
}
struct Prepared {
    resource_owners: Vec<Arc<dyn RuntimeResource>>,
    lifecycles: Vec<Arc<dyn RuntimeLifecycle>>,
    http_router: Option<Arc<dyn Backend>>,
    plan: Arc<ExecutionPlan>,
    security: Option<Arc<SecurityRuntime>>,
    targets: Targets,
    event: EventConfig,
    limits: Limits,
    budget: Budget,
    force_base64: bool,
    options: InvokeOptions,
    endpoint: Option<EndpointOverride>,
    region: String,
    digest: String,
    layer: Option<String>,
    routes: Vec<String>,
    warnings: Vec<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidationReport {
    pub valid: bool,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub config_digest: Option<String>,
    pub handlers: Vec<String>,
    pub routes: Vec<String>,
}
/// Startup steps 1-6 only: no AWS SDK/client/provider construction or key bootstrap.
pub async fn validate_configuration(input: &InitInput) -> ValidationReport {
    let result = AssertUnwindSafe(prepare_configuration(input))
        .catch_unwind()
        .await
        .unwrap_or(Err(InitError::Panic));
    match result {
        Ok(prepared) => {
            let (handlers, mut routes) = prepared.plan.description();
            routes.extend(prepared.routes);
            routes.sort();
            routes.dedup();
            let mut warnings = prepared.warnings;
            warnings.push("Offline validity does not establish remote keys, credentials, targets or readiness.".into());
            ValidationReport {
                valid: true,
                errors: vec![],
                warnings,
                config_digest: Some(prepared.digest),
                handlers,
                routes,
            }
        }
        Err(error) => ValidationReport {
            valid: false,
            errors: vec![error.message().to_string()],
            warnings: vec![],
            config_digest: None,
            handlers: vec![],
            routes: vec![],
        },
    }
}
async fn prepare_configuration(input: &InitInput) -> Result<Prepared, InitError> {
    prepare_configuration_for(input, false).await
}
async fn prepare_configuration_for(
    input: &InitInput,
    runtime: bool,
) -> Result<Prepared, InitError> {
    prepare_configuration_core(input, runtime, None).await
}
async fn prepare_configuration_core(
    input: &InitInput,
    runtime: bool,
    context: Option<Arc<PreparationContext>>,
) -> Result<Prepared, InitError> {
    if let Some(context) = &context {
        context.check()?;
    }

    let (config, sources) = prepare_local_with_sources(input).await?;
    if let Some(context) = &context {
        context.check()?;
    }
    crate::service_admission::reject(&optional::<Value>(&config, "service.yml")?)?;
    let layer = manifest(&input.external_dir)?;
    if config.server.enable_registry {
        return Err(invalid("server.yml: registry unsupported in Lambda"));
    }
    let cfg: LambdaConfig = load(&config, "light-lambda.yml")?;
    let mut event = EventConfig::new(cfg.stage).map_err(invalid)?;
    let budget = Budget {
        ingress: Duration::from_millis(cfg.ingress_timeout_ms),
        response_reserve: Duration::from_millis(cfg.response_reserve_ms),
        post_reserve: Duration::from_millis(cfg.post_reserve_ms),
    };
    budget.validate()?;
    let limits = Limits {
        max_request_bytes: cfg.max_request_bytes,
        max_response_bytes: cfg.max_response_bytes,
        max_invoke_payload_bytes: cfg.max_invoke_payload_bytes,
    };
    limits.validate().map_err(invalid)?;
    let app: AppConfig = optional(&config, "lambda-app.yml")?;
    event.stage_variables = cfg.stage_variables;
    event.account_id = cfg.account_id.clone();
    event.api_id = cfg.api_id;
    event.domain_name = cfg.domain_name;
    event.resource_id = cfg.resource_id;
    event.encode_base64_request = app.encode_base64_request;
    let proxy: ProxyConfig = load(&config, "lambda-proxy.yml")?;
    let region = proxy
        .region
        .as_ref()
        .filter(|s| !s.is_empty())
        .or(input.environment.region.as_ref())
        .cloned()
        .ok_or_else(|| invalid("lambda-proxy.yml: missing region"))?;
    let offline_identity;
    let identity = if matches!(input.backend, BackendConfiguration::Offline) {
        offline_identity = GatewayIdentity {
            function_name: input.gateway_identity.function_name.clone(),
            region: region.clone(),
            account_id: cfg
                .account_id
                .clone()
                .unwrap_or_else(|| input.gateway_identity.account_id.clone()),
        };
        &offline_identity
    } else {
        &input.gateway_identity
    };
    if !matches!(input.backend, BackendConfiguration::Offline)
        && (identity.function_name != input.environment.function_name || identity.region != region)
    {
        return Err(invalid(
            "lambda-proxy.yml: gateway identity conflicts with runtime region or function",
        ));
    }
    let mut functions = BTreeMap::new();
    let mut routes = Vec::new();
    let mut described_routes = Vec::new();
    for (i, (route, function)) in proxy.functions.into_iter().enumerate() {
        described_routes.push(route.clone());
        let alias = format!("function{i}");
        functions.insert(alias.clone(), function);
        routes.push((route, Target::Function(alias)));
    }
    let targets = Targets::new(functions, identity)
        .map_err(|_| invalid("lambda-proxy.yml: invalid target identity"))?;
    let mut registry = Registry::default();
    if proxy.enabled {
        registry.register(
            HandlerId::Proxy,
            Arc::new(ProxyFactory(
                TargetRoutes::compile(routes)
                    .map_err(|_| invalid("lambda-proxy.yml: invalid routes"))?,
            )),
        );
    } else {
        registry.disable(HandlerId::Proxy);
    }
    let mut settings = Vec::new();
    let mut local_warnings = Vec::new();
    for declaration in &input.handlers {
        if declaration.id == HandlerId::Specification
            && declaration.config_file != "openapi-handler.yml"
        {
            return Err(invalid("specification: incorrect configuration file"));
        }
        let value: Value = load(&config, declaration.config_file)?;
        let enabled = if matches!(
            declaration.id,
            HandlerId::Info | HandlerId::GetLogger | HandlerId::SetLogger
        ) {
            if declaration.config_file
                != match declaration.id {
                    HandlerId::Info => "info.yml",
                    _ => "logging.yml",
                }
            {
                return Err(invalid("local admin: incorrect configuration file"));
            }
            let (enabled, warnings) = crate::local_activation::activation(
                declaration.id,
                &value,
                declaration.factory.is_some(),
            )?;
            local_warnings.extend(warnings);
            enabled
        } else {
            value
                .get("enabled")
                .and_then(Value::as_bool)
                .ok_or_else(|| invalid("configuration: handler enabled flag required"))?
        };
        settings.push((declaration, enabled));
    }
    let security_active = settings.iter().any(|(h, enabled)| {
        *enabled && matches!(h.id, HandlerId::Jwt | HandlerId::Security) && h.factory.is_some()
    });
    let mut handler: Value = load(&config, "handler.yml")?;
    // Use captured values, before ignored-setting removal, handler pruning,
    // source capture, factory preparation or runtime engine activation. Consult
    // every trusted provider, including disabled or unselected registrations.
    for declaration in &input.handlers {
        if let Some(profile) = declaration
            .factory
            .as_ref()
            .and_then(|factory| factory.required_execution_profile())
        {
            profile.admit(&handler, proxy.enabled, &settings)?;
        }
    }
    let setter_enabled = settings
        .iter()
        .any(|(h, enabled)| h.id == HandlerId::SetLogger && *enabled && h.factory.is_some());
    crate::local_activation::reject_logger_set_selection(&handler, setter_enabled)?;
    // Closed CFG-04 contract: ignored default is diagnosed, never implemented.
    // Remove only this reviewed default before the unchanged strict plan compiler.
    let mode = match handler.get("reportHandlerDuration") {
        None => "absent",
        Some(Value::Bool(false)) => "default-false",
        _ => {
            return Err(invalid(
                "handler.yml: reportHandlerDuration unsupported nondefault",
            ));
        }
    };
    tracing::warn!(target:"light_lambda::init", configuration_key="handler.reportHandlerDuration", mode, ignored=true, message="unsupported default handler setting ignored");
    if let Some(object) = handler.as_object_mut() {
        object.remove("reportHandlerDuration");
    }
    let mut warnings = vec![format!(
        "handler.reportHandlerDuration: unsupported {mode} ignored"
    )];
    warnings.extend(crate::head_policy::warnings(&config)?);
    warnings.extend(crate::factor_policy::warnings(&config)?);
    crate::audit_policy::reject_credential_headers(&config)?;
    warnings.extend(crate::token_policy::warnings(&config)?);
    warnings.extend(crate::metrics_policy::warnings(&config)?);
    warnings.extend(crate::audit_policy::warnings(&config)?);
    warnings.extend(crate::specification_policy::warnings(&config)?);
    let (remaining_warnings, transformer_program) =
        crate::remaining_policy::prepare_with_sources(&config, Some(&sources))?;
    warnings.extend(remaining_warnings);
    warnings.extend(local_warnings);
    warnings.sort();
    warnings.dedup();
    let security = crate::security::register(&config, &mut registry, &handler, security_active)?;
    if security.is_some()
        && let Some(client) = &config.client
    {
        // Construction validates configured PEM/trust artifacts locally. No request
        // is sent, no network key service is bootstrapped and no transport is bound.
        light_client::build_reqwest_client(
            &client.request,
            &client.tls,
            light_client::EndpointOptions::default(),
        )
        .map_err(|_| invalid("client.yml: invalid local TLS configuration or artifact"))?;
    }
    let mut registered = std::collections::BTreeSet::new();
    // Validate the complete catalogue before preparing any source or consumer.
    for (declaration, enabled) in &settings {
        if declaration.id == HandlerId::Proxy {
            return Err(invalid("handler.yml: duplicate proxy factory"));
        }
        if !COMPATIBILITY.iter().any(|c| c.id == declaration.id) {
            return Err(invalid("handler.yml: invalid factory identity"));
        }
        if !registered.insert(declaration.id) {
            return Err(invalid("handler.yml: duplicate factory"));
        }
        if *enabled
            && declaration.factory.is_none()
            && !matches!(
                declaration.id,
                HandlerId::Jwt | HandlerId::Security | HandlerId::SetLogger
            )
        {
            return Err(invalid(
                "configuration: enabled handler not supported until WP12",
            ));
        }
    }
    // Producer preparation is independent of declaration order, with no cache
    // surviving this Init. Consumers receive the very same immutable resources.
    let mut specification = None;
    let mut precision = None;
    let mut resource_owners = Vec::new();
    for (declaration, enabled) in &settings {
        if let Some(context) = &context {
            context.check()?;
        }
        if *enabled && declaration.id == HandlerId::Specification {
            let factory = declaration.factory.as_ref().expect("validated factory");
            if runtime {
                let prepared = factory
                    .prepare_specification_runtime_owned(&config, security.clone(), context.clone())
                    .await?;
                precision = prepared.precision;
                resource_owners.extend(prepared.owners);
                specification = Some(prepared.specification);
            } else {
                specification =
                    Some(factory.prepare_specification_resources(&config, security.clone())?);
            }
        }
    }
    let specification_resources = specification.as_ref().and_then(|s| s.resources.clone());
    let mut specification_identity = None;
    let mut router_enabled = false;
    let mut router_resources = None;
    let mut router_runtime_backend = None;
    let mut lifecycles = Vec::new();
    for (declaration, enabled) in settings {
        if let Some(context) = &context {
            context.check()?;
        }
        match Some(enabled) {
            Some(false) => registry.disable(declaration.id),
            Some(true) => {
                if let Some(factory) = &declaration.factory {
                    let factory = if declaration.id == HandlerId::Specification {
                        let (factory, identity) =
                            specification.take().expect("prepared producer").prepared;
                        specification_identity = identity;
                        factory
                    } else if declaration.id == HandlerId::Validator {
                        let prepared = factory.prepare_validator_runtime_owned(
                            &config,
                            security.clone(),
                            specification_resources.clone(),
                            precision.clone(),
                            context.clone(),
                        )?;
                        if let Some(lifecycle) = prepared.lifecycle {
                            lifecycles.push(lifecycle);
                        }
                        prepared.factory
                    } else if declaration.id == HandlerId::Router {
                        let runtime = factory.prepare_router_runtime(
                            &config,
                            security.clone(),
                            runtime,
                            &limits,
                        )?;
                        if let Some(lifecycle) = runtime.lifecycle {
                            lifecycles.push(lifecycle);
                        }
                        router_runtime_backend = runtime.backend;
                        router_resources = runtime.prepared.resources;
                        runtime.prepared.factory
                    } else if matches!(
                        declaration.id,
                        HandlerId::RequestTransformer | HandlerId::ResponseTransformer
                    ) {
                        factory.prepare_transformer(
                            &config,
                            security.clone(),
                            transformer_program.clone(),
                        )?
                    } else {
                        factory.create(&config, security.clone())?
                    };
                    registry.register(declaration.id, factory);
                    if declaration.id == HandlerId::Router {
                        router_enabled = true;
                    }
                } else if !matches!(declaration.id, HandlerId::Jwt | HandlerId::Security) {
                    return Err(invalid(
                        "configuration: enabled handler not supported until WP12",
                    ));
                }
            }
            None => return Err(invalid("configuration: handler enabled flag required")),
        }
    }
    let audit: Value = optional::<BTreeMap<String, Value>>(&config, "audit.yml").and_then(|v| {
        serde_json::to_value(v).map_err(|_| invalid("audit.yml: invalid configuration"))
    })?;
    let plan = Arc::new(
        ExecutionPlan::compile(handler.clone(), audit.clone(), &registry).map_err(|error| {
            match error {
                serverless_core::plan::InitError::UnknownClass(class) => {
                    InitError::UnknownHandler(class)
                }
                _ => invalid("handler.yml: invalid execution plan or unavailable handler"),
            }
        })?,
    );
    if let Some(program) = &transformer_program {
        for occurrences in plan.transformer_occurrences() {
            let uses = occurrences
                .iter()
                .map(|id| {
                    (
                        program.as_ref(),
                        if *id == HandlerId::RequestTransformer {
                            "req-tra"
                        } else {
                            "res-tra"
                        },
                    )
                })
                .collect::<Vec<_>>();
            crate::transformer_program::aggregate(&uses)
                .map_err(|_| invalid("transformer: aggregate profile exceeded"))?;
        }
    }
    let endpoint = proxy
        .endpoint_override
        .filter(|s| !s.is_empty())
        .map(|s| EndpointOverride::new(s, input.environment.allow_insecure_test_endpoints))
        .transpose()
        .map_err(|_| invalid("lambda-proxy.yml: invalid endpointOverride"))?;
    let timeout = |ms| {
        if ms == 0 {
            None
        } else {
            Some(Duration::from_millis(ms))
        }
    };
    if proxy.connection_timeout == 0 {
        return Err(invalid(
            "lambda-proxy.yml: connectionTimeout must be positive",
        ));
    }
    let options = InvokeOptions {
        max_retries: cfg.invoke_max_retries,
        api_call_timeout: timeout(proxy.api_call_timeout),
        api_call_attempt_timeout: timeout(proxy.api_call_attempt_timeout),
        connection_timeout: timeout(proxy.connection_timeout),
        read_timeout: timeout(proxy.read_timeout),
        write_timeout: timeout(proxy.write_timeout),
    };
    if cfg.invoke_max_retries == u32::MAX {
        return Err(invalid("light-lambda.yml: invalid invokeMaxRetries"));
    }
    let digest = match specification_identity {
        Some(identity) => crate::specification_policy::configuration_digest(&config, identity)?,
        None => configuration_digest(&config)?,
    };
    let http_router = if router_enabled {
        Some(match router_runtime_backend {
            Some(backend) => backend,
            None => match &router_resources {
                Some(resources) => crate::router_policy::prepare_backend_from_resources(
                    &config, &limits, resources,
                )? as Arc<dyn Backend>,
                None => {
                    crate::router_policy::prepare_backend(&config, &limits)? as Arc<dyn Backend>
                }
            },
        })
    } else {
        None
    };
    Ok(Prepared {
        resource_owners,
        lifecycles,
        http_router,
        plan,
        security,
        targets,
        event,
        limits,
        budget,
        force_base64: app.encode_base64_response,
        options,
        endpoint,
        region,
        digest,
        layer,
        routes: described_routes,
        warnings,
    })
}
async fn initialize_inner(input: InitInput) -> Result<Arc<Snapshot>, InitError> {
    if let Some(profile) = preparation::profile(&input) {
        preparation::initialize(input, profile).await
    } else {
        initialize_owned(input, None).await
    }
}
async fn initialize_owned(
    input: InitInput,
    context: Option<Arc<PreparationContext>>,
) -> Result<Arc<Snapshot>, InitError> {
    if let Some(context) = &context {
        context.check()?;
    }

    let Prepared {
        resource_owners,
        lifecycles,
        http_router,
        plan,
        security,
        targets,
        event,
        limits,
        budget,
        force_base64,
        options,
        endpoint,
        region,
        digest,
        layer,
        routes: _,
        warnings: _,
    } = prepare_configuration_core(&input, true, context.clone()).await?;
    if let Some(context) = &context {
        context.check()?;
    }
    lifecycle::publish(lifecycles, |owner| async move {
    let mut resource_owners = resource_owners;
    resource_owners.push(owner);
    if let Some(security) = &security
        && !security.config.enable_mock_jwt
    {
        security
            .bootstrap()
            .await
            .map_err(|_| InitError::SecurityBootstrap)?;
    }
    if let Some(context)=&context{context.check()?;}
    let backend: Arc<dyn Backend> = match input.backend {
        BackendConfiguration::Injected(backend) => backend,
        kind => {
            let sdk = match kind {
                BackendConfiguration::Sdk(sdk) => *sdk,
                BackendConfiguration::Production => {
                    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .region(aws_config::Region::new(region))
                        .load()
                        .await;
                    aws_sdk_lambda::config::Builder::from(&config).build()
                }
                BackendConfiguration::Injected(_) => unreachable!(),
                BackendConfiguration::Offline => {
                    return Err(invalid("offline configuration cannot start a runtime"));
                }
            };
            Arc::new(
                LambdaInvokeBackend::new(
                    sdk,
                    targets,
                    event,
                    limits.clone(),
                    options,
                    Arc::new(SystemClock),
                    endpoint,
                )
                .map_err(|_| InitError::Backend)?,
            )
        }
    };
    let backend: Arc<dyn Backend> = match http_router {
        Some(http) => Arc::new(crate::http_router::ComposedBackend {
            lambda: backend,
            http,
        }),
        None => backend,
    };
    if let Some(context)=&context{resource_owners.push(context.snapshot_owner());}
    let snapshot=Arc::new(Snapshot {
        resource_owners,
        plan,
        backend,
        finalizers: input.finalizers,
        budget,
        limits,
        force_base64,
        cold: AtomicBool::new(true),
        digest,
        layer,
        security,
    });
    let snapshot=match &context{Some(context)=>context.publish(||snapshot)?,None=>snapshot};
    tracing::info!(target:"light_lambda::init",config_digest=%snapshot.digest,layer_identity=snapshot.layer.as_deref(),adapter_version=env!("CARGO_PKG_VERSION"),function_version=crate::telemetry::bounded(&input.environment.function_version),memory_mb=input.environment.memory_mb,message="immutable snapshot ready");
    Ok(snapshot)
    }).await
}

fn configuration_digest(config: &RuntimeConfig) -> Result<String, InitError> {
    let mut names = std::collections::BTreeSet::new();
    for file in config.embedded_config {
        names.insert(file.name.to_owned());
    }
    for dir in [
        config.default_config_dir.as_deref(),
        Some(config.config_dir.as_path()),
        Some(config.external_config_dir.as_path()),
    ]
    .into_iter()
    .flatten()
    {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir)
            .map_err(|_| invalid("configuration: digest directory unreadable"))?
        {
            let path = entry
                .map_err(|_| invalid("configuration: digest file unreadable"))?
                .path();
            if path.extension().is_some_and(|s| s == "yml" || s == "yaml") {
                names.insert(
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .ok_or_else(|| invalid("configuration: invalid file name"))?
                        .to_owned(),
                );
            }
        }
    }
    let mut documents = BTreeMap::new();
    for name in names {
        if name.ends_with(".yml") || name.ends_with(".yaml") {
            let value: Value = config
                .module_registry
                .load_config(config, &name)
                .map_err(|_| invalid("configuration: effective document unavailable for digest"))?;
            documents.insert(name, value);
        }
    }
    let values: BTreeMap<_, _> = config.resolved_values.iter().collect();
    Ok(format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(values, documents))
                .map_err(|_| invalid("configuration: digest failed"))?
        )
    ))
}

/// Derive an isolated qualification snapshot around the already initialized
/// backend. The original snapshot and all immutable policy resources are kept.
pub fn with_backend_observer(
    snapshot: &Arc<Snapshot>,
    wrap: impl FnOnce(Arc<dyn Backend>) -> Arc<dyn Backend>,
) -> Arc<Snapshot> {
    Arc::new(Snapshot {
        resource_owners: snapshot.resource_owners.clone(),
        plan: snapshot.plan.clone(),
        backend: wrap(snapshot.backend.clone()),
        finalizers: snapshot.finalizers.clone(),
        budget: snapshot.budget,
        limits: snapshot.limits.clone(),
        force_base64: snapshot.force_base64,
        cold: AtomicBool::new(snapshot.cold.load(std::sync::atomic::Ordering::Relaxed)),
        digest: snapshot.digest.clone(),
        layer: snapshot.layer.clone(),
        security: snapshot.security.clone(),
    })
}
