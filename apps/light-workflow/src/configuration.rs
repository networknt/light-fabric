use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use config_loader::{ConfigLoader, load_config_from_sources};
use light_runtime::{ConfigManager, ConfigProvenance, RuntimeConfig};
use light_security::SecurityConfig;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use sqlx::postgres::PgConnectOptions;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};
use workflow_policy::{CommandTemplate, ExecutionProfile};

use crate::command_template::validate_command_template;

pub const DEFAULT_MAXIMUM_PARALLELISM: usize = 64;
const ABSOLUTE_MAXIMUM_PARALLELISM: usize = 64;
const WORKFLOW_FILE: &str = "workflow.yml";
const SECURITY_FILE: &str = "security.yml";
const SERVICE_ID: &str = "com.networknt.workflow-1.0.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowConfiguration {
    pub environment: String,
    pub http_addr: SocketAddr,
    pub database_url: String,
    pub database_max_connections: u32,
    pub invocation_caller_service_ids: Vec<String>,
    pub wait_listener_connections: usize,
    pub ignore_user_jwt_expiry: bool,
    pub maximum_parallelism: usize,
    pub host_executor_concurrency: usize,
    pub interactive_estimated_task_ms: u64,
    pub service_authorization: String,
    pub delegation_secret: Option<String>,
    pub runner: RunnerSettings,
    pub artifact: ArtifactSettings,
    pub fixed_actions: FixedActionSettings,
    pub agent_provider_base_urls: BTreeMap<String, String>,
    /// Complete verifier configuration. The verifier is constructed once at
    /// startup, so any change to this value is restart-required.
    pub security: SecurityConfig,
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerSettings {
    pub enabled: bool,
    pub origin_service_id: String,
    pub origin_id: String,
    pub config_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactSettings {
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    pub allow_http: bool,
    pub prefix: String,
    pub retention_days: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedActionSettings {
    pub root: PathBuf,
    pub artifact_root: PathBuf,
    pub branch_prefix: String,
    pub repository_url: Option<String>,
    pub repository_token: Option<String>,
    pub release_url: Option<String>,
    pub release_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowFile {
    invocation: InvocationFile,
    execution: ExecutionFile,
    database: DatabaseFile,
    runner: RunnerFile,
    artifact: ArtifactFile,
    fixed_actions: FixedActionsFile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InvocationFile {
    allowed_caller_service_ids: Vec<String>,
    wait_listener_connections: usize,
    ignore_user_jwt_expiry: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExecutionFile {
    maximum_parallelism: usize,
    host_executor_concurrency: usize,
    interactive_estimated_task_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DatabaseFile {
    max_connections: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunnerFile {
    enabled: bool,
    origin_service_id: String,
    origin_id: String,
    config_file: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArtifactFile {
    s3_bucket: String,
    s3_endpoint: String,
    allow_http: bool,
    prefix: String,
    retention_days: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FixedActionsFile {
    root: PathBuf,
    artifact_root: PathBuf,
    branch_prefix: String,
    repository_url: String,
    release_url: String,
}

impl WorkflowConfiguration {
    pub fn build(
        runtime: &RuntimeConfig,
        managed: bool,
        compatibility_environment: &str,
    ) -> Result<Self, String> {
        Self::build_with_environment_policy(
            runtime,
            managed,
            compatibility_environment,
            &|name| env::var(name).ok(),
            chrono::Utc::now().timestamp(),
            true,
        )
    }

    /// Builds a reload candidate without re-aging restart-required service
    /// credentials. Their expiry is enforced at process startup; changing the
    /// credential still requires a restart.
    pub fn build_reload_candidate(
        runtime: &RuntimeConfig,
        managed: bool,
        compatibility_environment: &str,
    ) -> Result<Self, String> {
        Self::build_with_environment_policy(
            runtime,
            managed,
            compatibility_environment,
            &|name| env::var(name).ok(),
            chrono::Utc::now().timestamp(),
            false,
        )
    }

    #[cfg(test)]
    fn build_with_environment(
        runtime: &RuntimeConfig,
        managed: bool,
        compatibility_environment: &str,
        environment_value: &dyn Fn(&str) -> Option<String>,
        now_unix_seconds: i64,
    ) -> Result<Self, String> {
        Self::build_with_environment_policy(
            runtime,
            managed,
            compatibility_environment,
            environment_value,
            now_unix_seconds,
            true,
        )
    }

    fn build_with_environment_policy(
        runtime: &RuntimeConfig,
        managed: bool,
        compatibility_environment: &str,
        environment_value: &dyn Fn(&str) -> Option<String>,
        now_unix_seconds: i64,
        enforce_scope_token_expiry: bool,
    ) -> Result<Self, String> {
        let loader = ConfigLoader::from_values(runtime.resolved_values.clone(), None, None)
            .map_err(|error| error.to_string())?;
        let workflow: WorkflowFile = load_typed(runtime, &loader, WORKFLOW_FILE)?;
        let security: SecurityConfig = load_typed(runtime, &loader, SECURITY_FILE)?;
        let environment = runtime.server.environment.trim().to_string();
        let mut violations = Vec::new();
        let ignore_user_jwt_expiry = compatibility_boolean(
            "WORKFLOW_IGNORE_USER_JWT_EXPIRY",
            workflow.invocation.ignore_user_jwt_expiry,
            environment_value,
            &mut violations,
        );

        required("server.environment", &environment, &mut violations);
        if runtime.server.service_id != SERVICE_ID {
            violations.push(format!(
                "server.serviceId: expected `{SERVICE_ID}`, got `{}`",
                runtime.server.service_id
            ));
        }
        if runtime.bootstrap.env_tag.as_deref() != Some(environment.as_str()) {
            violations.push("server.environment: must match startup.envTag".to_string());
        }
        validate_timeout_ordering(
            "startup",
            runtime.bootstrap.connect_timeout,
            runtime.bootstrap.timeout,
            &mut violations,
        );
        if let Some(client) = runtime.client.as_ref() {
            validate_timeout_ordering(
                "client.request",
                client.request.connect_timeout,
                client.request.timeout,
                &mut violations,
            );
        }
        if compatibility_environment != environment {
            violations.push(
                "server.environment: must match compatibility variable SERVER_ENVIRONMENT"
                    .to_string(),
            );
        }
        if let Some(value) = environment_value("LIGHTAPI_ENVIRONMENT") {
            let value = value.trim();
            if value.is_empty() || value != environment {
                violations.push(
                    "server.environment: must match compatibility variable LIGHTAPI_ENVIRONMENT"
                        .to_string(),
                );
            }
        }
        required("security.issuer", &security.issuer, &mut violations);
        validate_non_empty_unique_list("security.audience", &security.audience, &mut violations);
        validate_non_empty_unique_list(
            "workflow.invocation.allowedCallerServiceIds",
            &workflow.invocation.allowed_caller_service_ids,
            &mut violations,
        );
        range(
            "workflow.invocation.waitListenerConnections",
            workflow.invocation.wait_listener_connections,
            1,
            64,
            &mut violations,
        );
        range(
            "workflow.execution.maximumParallelism",
            workflow.execution.maximum_parallelism,
            1,
            ABSOLUTE_MAXIMUM_PARALLELISM,
            &mut violations,
        );
        range(
            "workflow.execution.hostExecutorConcurrency",
            workflow.execution.host_executor_concurrency,
            1,
            128,
            &mut violations,
        );
        if workflow.execution.host_executor_concurrency > workflow.execution.maximum_parallelism {
            violations.push("workflow.execution.hostExecutorConcurrency: must not exceed workflow.execution.maximumParallelism".to_string());
        }
        range(
            "workflow.execution.interactiveEstimatedTaskMs",
            workflow.execution.interactive_estimated_task_ms,
            1,
            30_000,
            &mut violations,
        );
        range(
            "workflow.database.maxConnections",
            workflow.database.max_connections,
            8,
            512,
            &mut violations,
        );
        validate_user_expiry_policy(ignore_user_jwt_expiry, &environment, &mut violations);
        required(
            "workflow.runner.originServiceId",
            &workflow.runner.origin_service_id,
            &mut violations,
        );
        required(
            "workflow.runner.originId",
            &workflow.runner.origin_id,
            &mut violations,
        );
        if managed && workflow.runner.origin_id.trim() == "light-workflow-1" {
            violations.push(
                "workflow.runner.originId: placeholder identity is forbidden in managed mode"
                    .to_string(),
            );
        }
        validate_absolute_path(
            "workflow.fixedActions.root",
            &workflow.fixed_actions.root,
            &mut violations,
        );
        validate_absolute_path(
            "workflow.fixedActions.artifactRoot",
            &workflow.fixed_actions.artifact_root,
            &mut violations,
        );
        required(
            "workflow.fixedActions.branchPrefix",
            &workflow.fixed_actions.branch_prefix,
            &mut violations,
        );
        validate_optional_url(
            "workflow.fixedActions.repositoryUrl",
            &workflow.fixed_actions.repository_url,
            false,
            &mut violations,
        );
        validate_optional_url(
            "workflow.fixedActions.releaseUrl",
            &workflow.fixed_actions.release_url,
            false,
            &mut violations,
        );
        validate_optional_url(
            "workflow.artifact.s3Endpoint",
            &workflow.artifact.s3_endpoint,
            workflow.artifact.allow_http && environment.eq_ignore_ascii_case("dev"),
            &mut violations,
        );
        if workflow.artifact.allow_http && !environment.eq_ignore_ascii_case("dev") {
            violations.push("workflow.artifact.allowHttp: true is permitted only when server.environment is dev".to_string());
        }
        validate_object_prefix(
            "workflow.artifact.prefix",
            &workflow.artifact.prefix,
            &mut violations,
        );
        range(
            "workflow.artifact.retentionDays",
            workflow.artifact.retention_days,
            1,
            3650,
            &mut violations,
        );
        let http_addr = format!("{}:{}", runtime.server.ip, runtime.server.http_port)
            .parse::<SocketAddr>()
            .map_err(|error| format!("server.ip/server.httpPort: invalid socket address: {error}"));
        if let Err(error) = &http_addr {
            violations.push(error.clone());
        }

        let database_url = required_secret("DATABASE_URL", environment_value, &mut violations);
        if let Some(value) = database_url.as_deref() {
            if let Err(error) = PgConnectOptions::from_str(value) {
                violations.push(format!("DATABASE_URL: invalid PostgreSQL URL: {error}"));
            }
        }
        let service_authorization = required_secret(
            "LIGHT_PORTAL_AUTHORIZATION",
            environment_value,
            &mut violations,
        );
        if let Some(value) = service_authorization.as_deref() {
            validate_scope_token(
                value,
                &environment,
                now_unix_seconds,
                enforce_scope_token_expiry,
                &mut violations,
            );
        }
        let repository_token = provider_token(
            "workflow.fixedActions.repositoryUrl",
            &workflow.fixed_actions.repository_url,
            "WORKFLOW_REPOSITORY_FIXED_ACTION_TOKEN",
            environment_value,
            &mut violations,
        );
        let release_token = provider_token(
            "workflow.fixedActions.releaseUrl",
            &workflow.fixed_actions.release_url,
            "WORKFLOW_RELEASE_FIXED_ACTION_TOKEN",
            environment_value,
            &mut violations,
        );
        let delegation_secret = optional_secret("WORKFLOW_DELEGATION_SECRET", environment_value);
        let agent_provider_base_urls = agent_provider_base_urls(runtime, &mut violations);

        if !violations.is_empty() {
            return Err(format!(
                "workflow configuration candidate rejected:\n- {}",
                violations.join("\n- ")
            ));
        }

        Ok(Self {
            environment,
            http_addr: http_addr.expect("validated socket address"),
            database_url: database_url.expect("validated database secret"),
            database_max_connections: workflow.database.max_connections,
            invocation_caller_service_ids: workflow.invocation.allowed_caller_service_ids,
            wait_listener_connections: workflow.invocation.wait_listener_connections,
            ignore_user_jwt_expiry,
            maximum_parallelism: workflow.execution.maximum_parallelism,
            host_executor_concurrency: workflow.execution.host_executor_concurrency,
            interactive_estimated_task_ms: workflow.execution.interactive_estimated_task_ms,
            service_authorization: service_authorization.expect("validated service bearer"),
            delegation_secret,
            runner: RunnerSettings {
                enabled: workflow.runner.enabled,
                origin_service_id: workflow.runner.origin_service_id,
                origin_id: workflow.runner.origin_id,
                config_file: non_empty(&workflow.runner.config_file).map(PathBuf::from),
            },
            artifact: ArtifactSettings {
                bucket: non_empty(&workflow.artifact.s3_bucket),
                endpoint: non_empty(&workflow.artifact.s3_endpoint),
                allow_http: workflow.artifact.allow_http,
                prefix: workflow.artifact.prefix.trim_matches('/').to_string(),
                retention_days: workflow.artifact.retention_days,
            },
            fixed_actions: FixedActionSettings {
                root: workflow.fixed_actions.root,
                artifact_root: workflow.fixed_actions.artifact_root,
                branch_prefix: workflow.fixed_actions.branch_prefix,
                repository_url: non_empty(&workflow.fixed_actions.repository_url),
                repository_token,
                release_url: non_empty(&workflow.fixed_actions.release_url),
                release_token,
            },
            agent_provider_base_urls,
            security,
            managed,
        })
    }
}

pub const WORKFLOW_RUNTIME_MODULE_ID: &str = "light-workflow/runtime-config";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRuntimeConfig {
    pub generation: u64,
    pub content_digest: String,
    pub snapshot_id: Option<String>,
    pub invocation_caller_service_ids: Vec<String>,
    pub wait_listener_connections: usize,
    pub ignore_user_jwt_expiry: bool,
    pub maximum_parallelism: usize,
    pub host_executor_concurrency: usize,
    pub interactive_estimated_task_ms: u64,
}

impl WorkflowRuntimeConfig {
    fn from_configuration(
        configuration: &WorkflowConfiguration,
        provenance: &ConfigProvenance,
        generation: u64,
    ) -> Self {
        Self {
            generation,
            content_digest: provenance.content_digest.clone(),
            snapshot_id: provenance.snapshot_id.clone(),
            invocation_caller_service_ids: configuration.invocation_caller_service_ids.clone(),
            wait_listener_connections: configuration.wait_listener_connections,
            ignore_user_jwt_expiry: configuration.ignore_user_jwt_expiry,
            maximum_parallelism: configuration.maximum_parallelism,
            host_executor_concurrency: configuration.host_executor_concurrency,
            interactive_estimated_task_ms: configuration.interactive_estimated_task_ms,
        }
    }

    fn same_policy(&self, other: &Self) -> bool {
        self.invocation_caller_service_ids == other.invocation_caller_service_ids
            && self.wait_listener_connections == other.wait_listener_connections
            && self.ignore_user_jwt_expiry == other.ignore_user_jwt_expiry
            && self.maximum_parallelism == other.maximum_parallelism
            && self.host_executor_concurrency == other.host_executor_concurrency
            && self.interactive_estimated_task_ms == other.interactive_estimated_task_ms
    }
}

#[derive(Debug)]
pub struct WorkflowConfigGeneration {
    pub config: WorkflowRuntimeConfig,
    pub wait_listener_permits: Arc<tokio::sync::Semaphore>,
}

#[derive(Debug)]
pub struct WorkflowConfigManager {
    current: ConfigManager<WorkflowConfigGeneration>,
    updates: tokio::sync::watch::Sender<u64>,
}

impl WorkflowConfigManager {
    pub fn new(configuration: &WorkflowConfiguration, provenance: &ConfigProvenance) -> Self {
        let config = WorkflowRuntimeConfig::from_configuration(configuration, provenance, 1);
        let wait_listener_permits = Arc::new(tokio::sync::Semaphore::new(
            config.wait_listener_connections,
        ));
        let (updates, _) = tokio::sync::watch::channel(config.generation);
        Self {
            current: ConfigManager::new(WorkflowConfigGeneration {
                config,
                wait_listener_permits,
            }),
            updates,
        }
    }

    pub fn load(&self) -> Arc<WorkflowConfigGeneration> {
        self.current.load()
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.updates.subscribe()
    }

    pub fn activate(
        &self,
        configuration: &WorkflowConfiguration,
        provenance: &ConfigProvenance,
    ) -> Arc<WorkflowConfigGeneration> {
        let current = self.load();
        let mut config = WorkflowRuntimeConfig::from_configuration(
            configuration,
            provenance,
            current.config.generation,
        );
        let same_policy = current.config.same_policy(&config);
        if !same_policy {
            config.generation = current.config.generation.saturating_add(1);
        }
        let generation = self.current.store(WorkflowConfigGeneration {
            wait_listener_permits: if same_policy {
                Arc::clone(&current.wait_listener_permits)
            } else {
                Arc::new(tokio::sync::Semaphore::new(
                    config.wait_listener_connections,
                ))
            },
            config,
        });
        let _ = self.updates.send(generation.config.generation);
        generation
    }
}

/// Restart-required runtime state retained by the workflow reloader. Keeping
/// this value-only baseline avoids retaining RuntimeConfig's ModuleRegistry Arc
/// through the registry's own reload handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRestartBaseline {
    bootstrap: JsonValue,
    server: JsonValue,
    client: JsonValue,
    portal_registry: JsonValue,
    direct_registry: JsonValue,
}

impl WorkflowRestartBaseline {
    pub fn from_runtime(runtime: &RuntimeConfig) -> Self {
        Self {
            bootstrap: serde_json::to_value(&runtime.bootstrap)
                .expect("bootstrap configuration serializes"),
            server: serde_json::to_value(&runtime.server).expect("server configuration serializes"),
            client: serde_json::to_value(&runtime.client).expect("client configuration serializes"),
            portal_registry: serde_json::to_value(&runtime.portal_registry)
                .expect("portal registry configuration serializes"),
            direct_registry: serde_json::to_value(&runtime.direct_registry)
                .expect("direct registry configuration serializes"),
        }
    }
}

pub fn restart_required_differences(
    active_runtime: &RuntimeConfig,
    active: &WorkflowConfiguration,
    candidate_runtime: &RuntimeConfig,
    candidate: &WorkflowConfiguration,
) -> Vec<String> {
    restart_required_differences_from_baseline(
        &WorkflowRestartBaseline::from_runtime(active_runtime),
        active,
        candidate_runtime,
        candidate,
    )
}

pub fn restart_required_differences_from_baseline(
    active_runtime: &WorkflowRestartBaseline,
    active: &WorkflowConfiguration,
    candidate_runtime: &RuntimeConfig,
    candidate: &WorkflowConfiguration,
) -> Vec<String> {
    let mut differences = BTreeSet::new();
    if active_runtime.bootstrap
        != serde_json::to_value(&candidate_runtime.bootstrap)
            .expect("bootstrap configuration serializes")
    {
        differences.insert("startup.yml".to_string());
    }
    if active_runtime.server
        != serde_json::to_value(&candidate_runtime.server).expect("server configuration serializes")
    {
        differences.insert("server.yml".to_string());
    }
    if active_runtime.client
        != serde_json::to_value(&candidate_runtime.client).expect("client configuration serializes")
    {
        differences.insert("client.yml".to_string());
    }
    if active_runtime.portal_registry
        != serde_json::to_value(&candidate_runtime.portal_registry)
            .expect("portal registry configuration serializes")
    {
        differences.insert("portal-registry.yml".to_string());
    }
    if active_runtime.direct_registry
        != serde_json::to_value(&candidate_runtime.direct_registry)
            .expect("direct registry configuration serializes")
    {
        differences.insert("direct-registry.yml".to_string());
    }
    if active.environment != candidate.environment {
        differences.insert("server.environment".to_string());
    }
    if active.http_addr != candidate.http_addr {
        differences.insert("server.httpEndpoint".to_string());
    }
    if active.database_url != candidate.database_url {
        differences.insert("DATABASE_URL".to_string());
    }
    if active.database_max_connections != candidate.database_max_connections {
        differences.insert("workflow.database.maxConnections".to_string());
    }
    if active.service_authorization != candidate.service_authorization {
        differences.insert("LIGHT_PORTAL_AUTHORIZATION".to_string());
    }
    if active.delegation_secret != candidate.delegation_secret {
        differences.insert("WORKFLOW_DELEGATION_SECRET".to_string());
    }
    if active.runner != candidate.runner {
        differences.insert("workflow.runner".to_string());
    }
    if active.artifact != candidate.artifact {
        differences.insert("workflow.artifact".to_string());
    }
    if active.fixed_actions != candidate.fixed_actions {
        differences.insert("workflow.fixedActions".to_string());
    }
    if active.agent_provider_base_urls != candidate.agent_provider_base_urls {
        differences.insert("workflow.agentProviders".to_string());
    }
    if active.security != candidate.security {
        differences.insert("security.yml".to_string());
    }
    if active.managed != candidate.managed {
        differences.insert("configuration.mode".to_string());
    }
    differences.into_iter().collect()
}

fn load_typed<T: for<'de> Deserialize<'de>>(
    runtime: &RuntimeConfig,
    loader: &ConfigLoader,
    file_name: &str,
) -> Result<T, String> {
    let value = load_config_from_sources(
        loader,
        runtime.embedded_config,
        runtime.default_config_dir.as_deref(),
        &runtime.config_dir,
        Some(&runtime.external_config_dir),
        file_name,
    )
    .map_err(|error| format!("{file_name}: {error}"))?
    .ok_or_else(|| format!("{file_name}: configuration file is missing"))?;
    serde_yaml::from_value(value).map_err(|error| format!("{file_name}: {error}"))
}

fn required(path: &str, value: &str, violations: &mut Vec<String>) {
    if value.trim().is_empty() {
        violations.push(format!("{path}: must not be empty"));
    }
}

fn range<T>(path: &str, value: T, min: T, max: T, violations: &mut Vec<String>)
where
    T: PartialOrd + std::fmt::Display,
{
    if value < min || value > max {
        violations.push(format!("{path}: must be between {min} and {max}"));
    }
}

fn validate_non_empty_unique_list(path: &str, values: &[String], violations: &mut Vec<String>) {
    let normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .collect::<BTreeSet<_>>();
    if normalized.len() != values.len() || normalized.is_empty() {
        violations.push(format!("{path}: must contain unique non-empty values"));
    }
}

fn validate_absolute_path(path: &str, value: &Path, violations: &mut Vec<String>) {
    if !value.is_absolute() {
        violations.push(format!("{path}: must be an absolute path"));
    }
}

fn validate_optional_url(path: &str, value: &str, allow_http: bool, violations: &mut Vec<String>) {
    let Some(value) = non_empty(value) else {
        return;
    };
    match reqwest::Url::parse(&value) {
        Ok(url) if url.scheme() == "https" || (allow_http && url.scheme() == "http") => {}
        Ok(_) => violations.push(format!(
            "{path}: must use https unless an explicit dev HTTP policy applies"
        )),
        Err(error) => violations.push(format!("{path}: invalid URL: {error}")),
    }
}

fn validate_object_prefix(path: &str, value: &str, violations: &mut Vec<String>) {
    let value = value.trim_matches('/');
    if value.is_empty() || value.split('/').any(|part| part.is_empty() || part == "..") {
        violations.push(format!("{path}: invalid object prefix"));
    }
}

fn validate_user_expiry_policy(enabled: bool, environment: &str, violations: &mut Vec<String>) {
    if enabled && !environment.eq_ignore_ascii_case("dev") {
        violations.push(
            "workflow.invocation.ignoreUserJwtExpiry: true is permitted only when server.environment is dev"
                .to_string(),
        );
    }
}

fn validate_timeout_ordering(
    path: &str,
    connect_timeout_ms: u64,
    request_timeout_ms: u64,
    violations: &mut Vec<String>,
) {
    if connect_timeout_ms == 0 || request_timeout_ms == 0 {
        violations.push(format!("{path}.timeout: timeout values must be positive"));
    } else if connect_timeout_ms > request_timeout_ms {
        violations.push(format!(
            "{path}.connectTimeout: must not exceed {path}.timeout"
        ));
    }
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn required_secret(
    name: &str,
    environment_value: &dyn Fn(&str) -> Option<String>,
    violations: &mut Vec<String>,
) -> Option<String> {
    let value = environment_value(name).and_then(|value| non_empty(&value));
    if value.is_none() {
        violations.push(format!(
            "{name}: required secret environment variable is missing"
        ));
    }
    value
}

fn compatibility_boolean(
    name: &str,
    configured: bool,
    environment_value: &dyn Fn(&str) -> Option<String>,
    violations: &mut Vec<String>,
) -> bool {
    let Some(value) = environment_value(name) else {
        return configured;
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "" => configured,
        "true" => true,
        "false" => false,
        _ => {
            violations.push(format!("workflow.invocation.ignoreUserJwtExpiry: {name} compatibility value must be true or false"));
            configured
        }
    }
}

fn optional_secret(
    name: &str,
    environment_value: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    environment_value(name).and_then(|value| non_empty(&value))
}

fn provider_token(
    url_path: &str,
    url: &str,
    token_name: &str,
    environment_value: &dyn Fn(&str) -> Option<String>,
    violations: &mut Vec<String>,
) -> Option<String> {
    if non_empty(url).is_none() {
        return None;
    }
    let token = optional_secret(token_name, environment_value);
    if token.is_none() {
        violations.push(format!(
            "{token_name}: required when {url_path} is configured"
        ));
    }
    token
}

fn validate_scope_token(
    value: &str,
    environment: &str,
    now_unix_seconds: i64,
    enforce_expiry: bool,
    violations: &mut Vec<String>,
) {
    let token = value
        .split_once(char::is_whitespace)
        .and_then(|(scheme, token)| {
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim())
        })
        .unwrap_or(value.trim());
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<JsonValue>(&payload).ok());
    let Some(claims) = claims else {
        violations.push("LIGHT_PORTAL_AUTHORIZATION: must be a valid JWT".to_string());
        return;
    };
    if claims.get("env").and_then(JsonValue::as_str) != Some(environment) {
        violations.push(
            "LIGHT_PORTAL_AUTHORIZATION: env claim must match server.environment".to_string(),
        );
    }
    match claims.get("exp").and_then(JsonValue::as_i64) {
        Some(exp) if !enforce_expiry || exp > now_unix_seconds => {}
        Some(_) => violations.push(
            "LIGHT_PORTAL_AUTHORIZATION: scope-token expiry is enforced at startup".to_string(),
        ),
        None => violations.push("LIGHT_PORTAL_AUTHORIZATION: exp claim is required".to_string()),
    }
}

fn agent_provider_base_urls(
    runtime: &RuntimeConfig,
    violations: &mut Vec<String>,
) -> BTreeMap<String, String> {
    let mut output = BTreeMap::new();
    for (key, value) in &runtime.resolved_values {
        let Some(provider) = key
            .strip_prefix("workflow.agentProviders.")
            .and_then(|value| value.strip_suffix(".baseUrl"))
        else {
            continue;
        };
        let Some(value) = value.as_str().and_then(non_empty) else {
            violations.push(format!("{key}: must be a non-empty URL"));
            continue;
        };
        validate_optional_url(key, &value, false, violations);
        output.insert(provider.to_ascii_lowercase(), value);
    }
    output
}

#[derive(Debug, Clone)]
pub struct RunnerExecutionConfig {
    pub enabled: bool,
    pub origin_service_id: String,
    pub origin_instance_id: String,
    pub profiles: BTreeMap<String, ExecutionProfile>,
    pub command_templates: BTreeMap<String, CommandTemplate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RunnerExecutionConfigFile {
    version: u16,
    #[serde(default)]
    profiles: Vec<ExecutionProfile>,
    #[serde(default)]
    command_templates: Vec<CommandTemplate>,
}

impl RunnerExecutionConfig {
    pub fn load(settings: &RunnerSettings) -> Result<Self, String> {
        let config = match settings.config_file.as_deref() {
            Some(path) => load_file(path)?,
            None => RunnerExecutionConfigFile {
                version: 1,
                profiles: Vec::new(),
                command_templates: Vec::new(),
            },
        };
        if config.version != 1 {
            return Err(format!(
                "unsupported runner execution config version {}",
                config.version
            ));
        }
        let profiles = unique_by_id(config.profiles, |profile| profile.id.as_str(), "profile")?;
        let command_templates = unique_by_id(
            config.command_templates,
            |template| template.id.as_str(),
            "command template",
        )?;
        for template in command_templates.values() {
            validate_command_template(template)?;
        }
        if settings.enabled && (profiles.is_empty() || command_templates.is_empty()) {
            return Err(
                "enabled runner execution requires at least one profile and command template"
                    .to_string(),
            );
        }

        Ok(Self {
            enabled: settings.enabled,
            origin_service_id: settings.origin_service_id.clone(),
            origin_instance_id: settings.origin_id.clone(),
            profiles,
            command_templates,
        })
    }
}

fn load_file(path: &Path) -> Result<RunnerExecutionConfigFile, String> {
    let content = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_yaml::from_str(&content)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn unique_by_id<T, F>(values: Vec<T>, id: F, kind: &str) -> Result<BTreeMap<String, T>, String>
where
    F: Fn(&T) -> &str,
{
    let mut output = BTreeMap::new();
    for value in values {
        let value_id = id(&value).trim().to_string();
        if value_id.is_empty() {
            return Err(format!("{kind} ID must not be empty"));
        }
        if output.insert(value_id.clone(), value).is_some() {
            return Err(format!("duplicate {kind} ID `{value_id}`"));
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::{
        ArtifactSettings, FixedActionSettings, RunnerExecutionConfigFile, RunnerSettings,
        WorkflowConfigManager, WorkflowConfiguration, compatibility_boolean, range,
        restart_required_differences, validate_scope_token, validate_timeout_ordering,
        validate_user_expiry_policy,
    };
    use async_trait::async_trait;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use light_runtime::{
        AdmissionGate, BoundTransport, ConfigProvenance, ConfigSource, LifecycleRegistrar,
        LightRuntimeBuilder, RuntimeConfig, RuntimeError, ShutdownContext, TransportRuntime,
    };
    use light_security::SecurityConfig;
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio_util::sync::CancellationToken;

    fn workflow_configuration() -> WorkflowConfiguration {
        WorkflowConfiguration {
            environment: "dev".to_string(),
            http_addr: "0.0.0.0:8436".parse().unwrap(),
            database_url: "postgres://workflow".to_string(),
            database_max_connections: 32,
            invocation_caller_service_ids: vec!["caller-a".to_string()],
            wait_listener_connections: 2,
            ignore_user_jwt_expiry: false,
            maximum_parallelism: 16,
            host_executor_concurrency: 4,
            interactive_estimated_task_ms: 100,
            service_authorization: "scope-token".to_string(),
            delegation_secret: None,
            runner: RunnerSettings {
                enabled: false,
                origin_service_id: "workflow".to_string(),
                origin_id: "workflow-dev".to_string(),
                config_file: None,
            },
            artifact: ArtifactSettings {
                bucket: None,
                endpoint: None,
                allow_http: false,
                prefix: "workflow".to_string(),
                retention_days: 30,
            },
            fixed_actions: FixedActionSettings {
                root: "/tmp/workflow".into(),
                artifact_root: "/tmp/workflow-artifacts".into(),
                branch_prefix: "workflow/".to_string(),
                repository_url: None,
                repository_token: None,
                release_url: None,
                release_token: None,
            },
            agent_provider_base_urls: BTreeMap::new(),
            security: SecurityConfig {
                issuer: "https://issuer".to_string(),
                audience: vec!["workflow".to_string()],
                ..SecurityConfig::default()
            },
            managed: true,
        }
    }

    fn provenance(digest: &str, snapshot: &str) -> ConfigProvenance {
        ConfigProvenance {
            source: ConfigSource::Remote,
            host_id: Some("host".to_string()),
            snapshot_id: Some(snapshot.to_string()),
            instance_id: Some("instance".to_string()),
            content_digest: digest.to_string(),
        }
    }

    mod embedded_config {
        include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
    }

    #[derive(Clone, Copy)]
    struct TestTransport;

    #[async_trait]
    impl TransportRuntime for TestTransport {
        type Handle = ();

        async fn bind(
            &self,
            _config: &RuntimeConfig,
            _lifecycle: &LifecycleRegistrar,
            _admission: &AdmissionGate,
            _startup_cancel: CancellationToken,
        ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
            Err(RuntimeError::Unsupported("test transport".to_string()))
        }

        async fn stop(
            &self,
            _handle: &mut Self::Handle,
            _context: &ShutdownContext,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    #[test]
    fn config_rejects_unknown_authority_fields() {
        let config = r#"
version: 1
allowHostDockerSocket: true
profiles: []
commandTemplates: []
"#;
        assert!(serde_yaml::from_str::<RunnerExecutionConfigFile>(config).is_err());
    }

    #[test]
    fn user_expiry_exception_is_typed_and_dev_only() {
        let mut violations = Vec::new();
        let override_value = compatibility_boolean(
            "WORKFLOW_IGNORE_USER_JWT_EXPIRY",
            false,
            &|_| Some("true".to_string()),
            &mut violations,
        );
        assert!(override_value);
        validate_user_expiry_policy(override_value, "dev", &mut violations);
        assert!(violations.is_empty());
        validate_user_expiry_policy(true, "prod", &mut violations);
        assert!(
            violations
                .iter()
                .any(|value| value.starts_with("workflow.invocation.ignoreUserJwtExpiry:"))
        );
        violations.clear();
        compatibility_boolean(
            "WORKFLOW_IGNORE_USER_JWT_EXPIRY",
            false,
            &|_| Some("yes".to_string()),
            &mut violations,
        );
        assert!(violations[0].contains("must be true or false"));
        violations.clear();
        assert!(!compatibility_boolean(
            "WORKFLOW_IGNORE_USER_JWT_EXPIRY",
            false,
            &|_| Some("   ".to_string()),
            &mut violations,
        ));
        assert!(violations.is_empty());
    }

    #[test]
    fn invocation_scope_token_expiry_is_enforced_at_startup_but_not_reaged_on_reload() {
        let token = |environment: &str, exp: i64| {
            let payload = URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&serde_json::json!({"env": environment, "exp": exp})).unwrap(),
            );
            format!("header.{payload}.signature")
        };
        let mut violations = Vec::new();
        validate_scope_token(&token("dev", 101), "dev", 100, true, &mut violations);
        assert!(violations.is_empty());

        validate_scope_token(&token("dev", 100), "dev", 100, true, &mut violations);
        assert!(
            violations
                .iter()
                .any(|value| value.contains("scope-token expiry is enforced at startup"))
        );
        violations.clear();
        validate_scope_token(&token("dev", 100), "dev", 100, false, &mut violations);
        assert!(violations.is_empty());
        validate_scope_token(&token("loc", 101), "dev", 100, false, &mut violations);
        assert!(
            violations
                .iter()
                .any(|value| value.contains("env claim must match"))
        );
    }

    #[test]
    fn candidate_validation_reports_all_actionable_paths() {
        let mut violations = Vec::new();
        range(
            "workflow.execution.maximumParallelism",
            0,
            1,
            64,
            &mut violations,
        );
        range(
            "workflow.database.maxConnections",
            4,
            8,
            512,
            &mut violations,
        );
        validate_timeout_ordering("client.request", 2000, 1000, &mut violations);
        assert_eq!(violations.len(), 3);
        assert!(violations[0].starts_with("workflow.execution.maximumParallelism:"));
        assert!(violations[1].starts_with("workflow.database.maxConnections:"));
        assert!(violations[2].starts_with("client.request.connectTimeout:"));
    }

    #[test]
    fn workflow_runtime_generation_swaps_one_complete_policy() {
        let first = workflow_configuration();
        let manager = std::sync::Arc::new(WorkflowConfigManager::new(
            &first,
            &provenance("digest-a", "snapshot-a"),
        ));
        let reader = {
            let manager = std::sync::Arc::clone(&manager);
            std::thread::spawn(move || {
                for _ in 0..10_000 {
                    let generation = manager.load();
                    let tuple = (
                        generation.config.maximum_parallelism,
                        generation.config.host_executor_concurrency,
                        generation.config.interactive_estimated_task_ms,
                    );
                    assert!(tuple == (16, 4, 100) || tuple == (32, 8, 200));
                }
            })
        };
        for index in 0..100 {
            let mut candidate = first.clone();
            if index % 2 == 0 {
                candidate.maximum_parallelism = 32;
                candidate.host_executor_concurrency = 8;
                candidate.interactive_estimated_task_ms = 200;
            }
            manager.activate(
                &candidate,
                &provenance(&format!("digest-{index}"), &format!("snapshot-{index}")),
            );
        }
        reader.join().unwrap();
    }

    #[test]
    fn unchanged_policy_refreshes_provenance_without_advancing_generation() {
        let configuration = workflow_configuration();
        let manager =
            WorkflowConfigManager::new(&configuration, &provenance("digest-a", "snapshot-a"));
        let original = manager.load();
        let refreshed = manager.activate(&configuration, &provenance("digest-a", "snapshot-b"));
        assert_eq!(refreshed.config.generation, 1);
        assert_eq!(refreshed.config.snapshot_id.as_deref(), Some("snapshot-b"));
        assert!(Arc::ptr_eq(
            &original.wait_listener_permits,
            &refreshed.wait_listener_permits
        ));
    }

    #[tokio::test]
    async fn capacity_consumers_are_notified_of_the_committed_generation() {
        let configuration = workflow_configuration();
        let manager =
            WorkflowConfigManager::new(&configuration, &provenance("digest-a", "snapshot-a"));
        let mut updates = manager.subscribe();
        let mut candidate = configuration.clone();
        candidate.host_executor_concurrency = 8;

        let activated = manager.activate(&candidate, &provenance("digest-b", "snapshot-b"));
        updates.changed().await.unwrap();

        assert_eq!(*updates.borrow(), activated.config.generation);
        assert_eq!(manager.load().config.host_executor_concurrency, 8);
    }

    #[tokio::test]
    async fn embedded_and_remote_values_build_the_same_typed_candidate() {
        let config_dir = TempDir::new().unwrap();
        let external_dir = TempDir::new().unwrap();
        std::fs::write(
            external_dir.path().join("values.yml"),
            r#"
workflow.invocation.allowedCallerServiceIds: [com.networknt.portal.gateway-1.0.0]
workflow.invocation.waitListenerConnections: 8
workflow.invocation.ignoreUserJwtExpiry: false
workflow.execution.maximumParallelism: 64
workflow.execution.hostExecutorConcurrency: 8
workflow.execution.interactiveEstimatedTaskMs: 500
workflow.database.maxConnections: 32
workflow.runner.enabled: false
workflow.runner.originServiceId: com.networknt.light-workflow-1.0.0
workflow.runner.originId: workflow-dev
"#,
        )
        .unwrap();
        let build = |external: Option<&std::path::Path>| {
            let mut builder = LightRuntimeBuilder::new(TestTransport)
                .with_embedded_config(embedded_config::FILES)
                .with_config_dir(config_dir.path());
            if let Some(external) = external {
                builder = builder.with_external_config_dir(external);
            }
            builder.build()
        };
        let local_runtime = build(None).prepare_local_config().await.unwrap();
        let remote_runtime = build(Some(external_dir.path()))
            .prepare_local_config()
            .await
            .unwrap();
        let payload = URL_SAFE_NO_PAD.encode(br#"{"env":"dev","exp":101}"#);
        let values = BTreeMap::from([
            (
                "DATABASE_URL".to_string(),
                "postgres://workflow:workflow@localhost/workflow".to_string(),
            ),
            (
                "LIGHT_PORTAL_AUTHORIZATION".to_string(),
                format!("header.{payload}.signature"),
            ),
        ]);
        let resolver = |name: &str| values.get(name).cloned();
        let local = WorkflowConfiguration::build_with_environment(
            &local_runtime,
            false,
            "dev",
            &resolver,
            100,
        )
        .unwrap();
        let remote = WorkflowConfiguration::build_with_environment(
            &remote_runtime,
            true,
            "dev",
            &resolver,
            100,
        )
        .unwrap();
        assert_eq!(local.environment, remote.environment);
        assert_eq!(local.http_addr, remote.http_addr);
        assert_eq!(local.maximum_parallelism, remote.maximum_parallelism);
        assert_eq!(
            local.host_executor_concurrency,
            remote.host_executor_concurrency
        );
        assert_eq!(
            local.wait_listener_connections,
            remote.wait_listener_connections
        );
        assert_eq!(local.runner.origin_id, remote.runner.origin_id);

        let mut reloadable_candidate = remote.clone();
        reloadable_candidate.maximum_parallelism = 32;
        reloadable_candidate.host_executor_concurrency = 4;
        assert!(
            restart_required_differences(
                &remote_runtime,
                &remote,
                &remote_runtime,
                &reloadable_candidate,
            )
            .is_empty()
        );
        let mut restart_candidate = remote.clone();
        restart_candidate.database_max_connections += 1;
        assert_eq!(
            restart_required_differences(
                &remote_runtime,
                &remote,
                &remote_runtime,
                &restart_candidate,
            ),
            vec!["workflow.database.maxConnections"]
        );
        let mut security_candidates = Vec::new();
        let mut candidate = remote.clone();
        candidate.security.enable_verify_jwt = !candidate.security.enable_verify_jwt;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.enable_verify_scope = !candidate.security.enable_verify_scope;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.ignore_jwt_expiry = !candidate.security.ignore_jwt_expiry;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.enable_relaxed_key_validation =
            !candidate.security.enable_relaxed_key_validation;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.jwt.clock_skew_in_seconds += 1;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.bootstrap_from_key_service =
            !candidate.security.bootstrap_from_key_service;
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.issuer.push_str("/changed");
        security_candidates.push(candidate);
        let mut candidate = remote.clone();
        candidate.security.audience.push("changed".to_string());
        security_candidates.push(candidate);

        for security_candidate in security_candidates {
            assert_eq!(
                restart_required_differences(
                    &remote_runtime,
                    &remote,
                    &remote_runtime,
                    &security_candidate,
                ),
                vec!["security.yml"]
            );
        }
    }
}
