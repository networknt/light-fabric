use execution_security::ProtectedPathPolicy;
use light_axum::{AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext};
use light_runtime::{
    ConfigProvenance, ConfigSource, LifecycleParticipant, LightRuntimeBuilder, ModuleKind,
    ReloadContext, ReloadOutcome, ReloadableModule, RuntimeConfig, RuntimeError, ShutdownContext,
    ShutdownWatcher, TracingOptions, init_tracing,
};
use light_security::load_security_runtime;
use light_workflow::agent_job::AgentJobReconciler;
use light_workflow::artifact_retention::ArtifactRetentionReconciler;
use light_workflow::artifact_store::DurableArtifactStore;
use light_workflow::configuration::{
    RunnerExecutionConfig, WORKFLOW_RUNTIME_MODULE_ID, WorkflowConfigManager,
    WorkflowConfiguration, WorkflowRestartBaseline, restart_required_differences_from_baseline,
};
use light_workflow::consumer::EventConsumer;
use light_workflow::executor::TaskExecutor;
use light_workflow::fixed_action::{FixedActionExecutor, HttpFixedActionProvider};
use light_workflow::lease_reaper::LeaseReaper;
use light_workflow::result_reconciler::ResultReconciler;
use light_workflow::rule_api::{WorkflowHealth, build_rule_api_router};
use light_workflow::runner_scheduler::RunnerScheduler;
use light_workflow::service_runtime::{ManagedWorkflowTask, WorkflowOperationalMetadata};
use light_workflow::session_reconciler::ExecutionSessionReconciler;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::{
    path::PathBuf,
    sync::{Arc, Weak},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

mod config_bootstrap;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

#[cfg(test)]
const CONFIG_DIR: &str = "config";

struct WorkflowDatabase(PgPool);

#[async_trait::async_trait]
impl LifecycleParticipant for WorkflowDatabase {
    fn name(&self) -> &'static str {
        "light-workflow-database"
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        let budget = context.remaining();
        tokio::time::timeout(budget, self.0.close())
            .await
            .map_err(|_| RuntimeError::ShutdownDeadlineExceeded(budget))?;
        Ok(())
    }
}

#[derive(Clone)]
struct WorkflowApp {
    managed_configuration: bool,
    compatibility_environment: String,
    operational_metadata: WorkflowOperationalMetadata,
    provenance: ConfigProvenance,
    cache_dir: PathBuf,
    _materialized_config_dir: Option<Arc<config_bootstrap::EphemeralDirectory>>,
}

struct WorkflowConfigReloader {
    active: tokio::sync::Mutex<(WorkflowRestartBaseline, WorkflowConfiguration)>,
    managed_configuration: bool,
    compatibility_environment: String,
    cache_dir: PathBuf,
    manager: Arc<WorkflowConfigManager>,
    module_registry: Weak<light_runtime::ModuleRegistry>,
    health: WorkflowHealth,
    operational_metadata: WorkflowOperationalMetadata,
}

impl WorkflowConfigReloader {
    async fn reject(
        &self,
        reason_code: &'static str,
        message: String,
        restart_paths: Vec<String>,
        provenance: Option<&ConfigProvenance>,
    ) -> RuntimeError {
        self.health.record_config_rejected(
            message.clone(),
            reason_code,
            restart_paths.clone(),
            provenance,
        );
        self.operational_metadata
            .publish_reload_failure(reason_code, &restart_paths, provenance)
            .await;
        tracing::error!(
            event = "workflow.config.candidate_rejected",
            reasonCode = reason_code,
            snapshotId = provenance.and_then(|value| value.snapshot_id.as_deref()).unwrap_or("unknown"),
            digest = provenance.map(|value| value.content_digest.as_str()).unwrap_or("unknown"),
            propertyPaths = ?restart_paths,
            error = %message,
            "workflow reload candidate rejected; previous generation remains active"
        );
        RuntimeError::Unsupported(message)
    }
}

#[async_trait::async_trait]
impl ReloadableModule for WorkflowConfigReloader {
    fn requires_exclusive_reload(&self) -> bool {
        true
    }

    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        // RuntimeMcpHandler serializes fetch through activation. This lock also
        // protects direct/in-process callers and the full active candidate used
        // for restart-required comparisons.
        let mut active = self.active.lock().await;
        let Some(provenance) = ctx.provenance.as_ref() else {
            return Err(self
                .reject(
                    "CONFIG_PROVENANCE_MISSING",
                    "workflow reload candidate has no Config Server provenance".to_string(),
                    Vec::new(),
                    None,
                )
                .await);
        };
        let Some(values_yaml) = ctx.source_values_yaml.as_deref() else {
            return Err(self
                .reject(
                    "CONFIG_SOURCE_MISSING",
                    "workflow reload candidate has no source values document".to_string(),
                    Vec::new(),
                    Some(provenance),
                )
                .await);
        };
        if let Err(error) = config_bootstrap::validate_remote_reload(
            &ctx.runtime_config,
            provenance,
            &self.compatibility_environment,
        ) {
            return Err(self
                .reject(
                    "CONFIG_IDENTITY_OR_SENSITIVITY_INVALID",
                    error.to_string(),
                    Vec::new(),
                    Some(provenance),
                )
                .await);
        }
        let candidate = match WorkflowConfiguration::build_reload_candidate(
            &ctx.runtime_config,
            self.managed_configuration,
            &self.compatibility_environment,
        ) {
            Ok(candidate) => candidate,
            Err(error) => {
                return Err(self
                    .reject(
                        "CONFIG_VALIDATION_FAILED",
                        error,
                        Vec::new(),
                        Some(provenance),
                    )
                    .await);
            }
        };
        let restart_paths = restart_required_differences_from_baseline(
            &active.0,
            &active.1,
            &ctx.runtime_config,
            &candidate,
        );
        if !restart_paths.is_empty() {
            let message = format!(
                "workflow reload requires restart for: {}",
                restart_paths.join(", ")
            );
            return Err(self
                .reject("RESTART_REQUIRED", message, restart_paths, Some(provenance))
                .await);
        }
        let Some(module_registry) = self.module_registry.upgrade() else {
            return Err(self
                .reject(
                    "MODULE_REGISTRY_UNAVAILABLE",
                    "workflow module registry is unavailable".to_string(),
                    Vec::new(),
                    Some(provenance),
                )
                .await);
        };
        if let Err(error) = config_bootstrap::persist_remote_reload(
            &ctx.runtime_config,
            provenance,
            values_yaml,
            &self.compatibility_environment,
            &self.cache_dir,
        ) {
            return Err(self
                .reject(
                    "LKG_PERSIST_FAILED",
                    error.to_string(),
                    Vec::new(),
                    Some(provenance),
                )
                .await);
        }

        let generation = self.manager.activate(&candidate, provenance);
        *active = (
            WorkflowRestartBaseline::from_runtime(&ctx.runtime_config),
            candidate,
        );
        // Module display state is not a second configuration authority. The
        // immutable manager swap above is the activation point, so a display
        // failure cannot turn an already-active generation into a rejection.
        if let Err(error) =
            module_registry.update_registered_config(WORKFLOW_RUNTIME_MODULE_ID, &generation.config)
        {
            tracing::error!(
                event = "workflow.config.module_display_update_failed",
                generation = generation.config.generation,
                error = %error,
                "workflow generation is active but its module display could not be refreshed"
            );
        }
        self.health.record_config_active(
            generation.config.generation,
            generation.config.content_digest.clone(),
            provenance,
            true,
        );
        self.operational_metadata
            .publish_reload_success(&generation.config)
            .await;
        tracing::info!(
            event = "workflow.config.activated",
            source = "remote",
            snapshotId = generation.config.snapshot_id.as_deref().unwrap_or("unknown"),
            digest = %generation.config.content_digest,
            generation = generation.config.generation,
            hostId = provenance.host_id.as_deref().unwrap_or("unknown"),
            portalConfigInstanceId = provenance.instance_id.as_deref().unwrap_or("unknown"),
            environment = %active.1.environment,
            serviceId = %ctx.runtime_config.service_identity.service_id,
            "workflow runtime configuration generation activated"
        );
        Ok(ReloadOutcome::success(format!(
            "workflow runtime generation {} active",
            generation.config.generation
        )))
    }
}

impl WorkflowApp {
    fn runtime_error(context: &str, error: impl std::fmt::Display) -> RuntimeError {
        RuntimeError::Unsupported(format!("{context}: {error}"))
    }

    fn register_task<F, Fut, E>(
        &self,
        context: &ServerContext,
        name: &'static str,
        cancellation: &CancellationToken,
        health: &WorkflowHealth,
        run: F,
    ) -> Result<(), RuntimeError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        context.lifecycle.register(ManagedWorkflowTask::spawn(
            name,
            cancellation.clone(),
            context.admission.clone(),
            health.clone(),
            Some(self.operational_metadata.clone()),
            run,
        ))
    }
}

#[async_trait::async_trait]
impl AxumApp for WorkflowApp {
    async fn router(&self, context: ServerContext) -> Result<axum::Router, RuntimeError> {
        let workflow_config = WorkflowConfiguration::build(
            &context.runtime_config,
            self.managed_configuration,
            &self.compatibility_environment,
        )
        .map_err(|error| Self::runtime_error("workflow configuration", error))?;
        if workflow_config.ignore_user_jwt_expiry {
            warn!(
                environment = %workflow_config.environment,
                "expired workflow user JWTs are accepted; this development-only override must not be enabled in production"
            );
        }

        let invocation_security = Arc::new(
            load_security_runtime(&context.runtime_config, true)
                .map_err(|error| Self::runtime_error("workflow JWT verifier", error))?
                .ok_or_else(|| {
                    RuntimeError::Unsupported(
                        "workflow JWT verification must be enabled".to_string(),
                    )
                })?,
        );
        invocation_security.bootstrap().await.map_err(|error| {
            Self::runtime_error(
                "workflow JWKS bootstrap",
                format!("{}: {}", error.code, error.message),
            )
        })?;

        let pool = PgPoolOptions::new()
            .max_connections(workflow_config.database_max_connections)
            .connect(&workflow_config.database_url)
            .await
            .map_err(|error| Self::runtime_error("workflow database", error))?;
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&pool)
            .await
            .map_err(|error| Self::runtime_error("workflow database readiness", error))?;
        context
            .lifecycle
            .register(Arc::new(WorkflowDatabase(pool.clone())))?;
        info!("Connected to Postgres");

        let runner_config = RunnerExecutionConfig::load(&workflow_config.runner)
            .map_err(|error| Self::runtime_error("workflow runner configuration", error))?;
        self.operational_metadata
            .configure_capacities(&workflow_config, &runner_config);
        let health = WorkflowHealth::default();
        self.operational_metadata.attach_health(health.clone());
        let runtime_config = Arc::new(WorkflowConfigManager::new(
            &workflow_config,
            &self.provenance,
        ));
        let initial_generation = runtime_config.load();
        health.record_config_active(
            initial_generation.config.generation,
            initial_generation.config.content_digest.clone(),
            &self.provenance,
            false,
        );
        context
            .runtime_config
            .module_registry
            .register_loaded_config(
                WORKFLOW_RUNTIME_MODULE_ID,
                "workflow-runtime",
                ModuleKind::Application,
                &initial_generation.config,
                [],
                true,
                Some(true),
                true,
            )?;
        context.runtime_config.module_registry.register_reloader(
            WORKFLOW_RUNTIME_MODULE_ID,
            Arc::new(WorkflowConfigReloader {
                active: tokio::sync::Mutex::new((
                    WorkflowRestartBaseline::from_runtime(&context.runtime_config),
                    workflow_config.clone(),
                )),
                managed_configuration: self.managed_configuration,
                compatibility_environment: self.compatibility_environment.clone(),
                cache_dir: self.cache_dir.clone(),
                manager: Arc::clone(&runtime_config),
                module_registry: Arc::downgrade(&context.runtime_config.module_registry),
                health: health.clone(),
                operational_metadata: self.operational_metadata.clone(),
            }),
        );
        let artifact_store = DurableArtifactStore::from_configuration(&workflow_config.artifact)
            .map_err(|error| Self::runtime_error("workflow artifact store", error))?;
        let executor = Arc::new(
            TaskExecutor::new(pool.clone())
                .with_runtime_configuration(
                    workflow_config.database_url.clone(),
                    workflow_config.host_executor_concurrency,
                    workflow_config.environment.clone(),
                    workflow_config.service_authorization.clone(),
                    workflow_config.delegation_secret.clone(),
                    workflow_config.agent_provider_base_urls.clone(),
                    workflow_config.managed,
                )
                .map_err(|error| Self::runtime_error("workflow executor", error))?
                .with_execution_profiles(runner_config.profiles.clone()),
        );
        let cancellation = CancellationToken::new();

        let metadata_observer = self.operational_metadata.clone();
        let observer_health = health.clone();
        self.register_task(
            &context,
            "light-workflow-controller-observer",
            &cancellation,
            &health,
            move |shutdown| async move {
                metadata_observer
                    .observe_registry(observer_health, shutdown)
                    .await
            },
        )?;

        let consumer =
            EventConsumer::new(pool.clone(), "workflow-engine-group".to_string(), 0, 1, 10)
                .with_database_url(workflow_config.database_url.clone())
                .with_runtime_config(Arc::clone(&runtime_config))
                .with_execution_profiles(runner_config.profiles.clone());
        consumer
            .initialize()
            .await
            .map_err(|error| Self::runtime_error("workflow event recovery", error))?;
        self.register_task(
            &context,
            "light-workflow-event-consumer",
            &cancellation,
            &health,
            move |shutdown| async move { consumer.run(shutdown).await },
        )?;

        let host_executor = Arc::clone(&executor);
        let executor_runtime_config = Arc::clone(&runtime_config);
        self.register_task(
            &context,
            "light-workflow-task-executor",
            &cancellation,
            &health,
            move |shutdown| async move {
                host_executor
                    .run_dynamic(executor_runtime_config, shutdown)
                    .await
            },
        )?;

        let agent_job_reconciler = AgentJobReconciler::new(pool.clone(), Arc::clone(&executor));
        self.register_task(
            &context,
            "light-workflow-agent-job-reconciler",
            &cancellation,
            &health,
            move |shutdown| async move { agent_job_reconciler.run(shutdown).await },
        )?;

        if runner_config.enabled {
            let scheduler = RunnerScheduler::new(pool.clone(), runner_config.clone());
            self.register_task(
                &context,
                "light-workflow-runner-scheduler",
                &cancellation,
                &health,
                move |shutdown| async move { scheduler.run(shutdown).await },
            )?;

            let reconciler = ResultReconciler::new(
                pool.clone(),
                Arc::clone(&executor),
                runner_config.origin_service_id.clone(),
                runner_config.origin_instance_id.clone(),
                artifact_store.clone(),
                workflow_config.artifact.retention_days,
            );
            reconciler
                .run_once()
                .await
                .map_err(|error| Self::runtime_error("workflow result recovery", error))?;
            self.register_task(
                &context,
                "light-workflow-result-reconciler",
                &cancellation,
                &health,
                move |shutdown| async move { reconciler.run(shutdown).await },
            )?;

            let lease_reaper = LeaseReaper::new(pool.clone());
            lease_reaper
                .run_once()
                .await
                .map_err(|error| Self::runtime_error("workflow lease recovery", error))?;
            self.register_task(
                &context,
                "light-workflow-lease-reaper",
                &cancellation,
                &health,
                move |shutdown| async move { lease_reaper.run(shutdown).await },
            )?;

            let session_reconciler = ExecutionSessionReconciler::new(
                pool.clone(),
                runner_config.origin_service_id.clone(),
                runner_config.origin_instance_id.clone(),
            );
            session_reconciler
                .reconcile_once()
                .await
                .map_err(|error| Self::runtime_error("workflow session recovery", error))?;
            self.register_task(
                &context,
                "light-workflow-session-reconciler",
                &cancellation,
                &health,
                move |shutdown| async move { session_reconciler.run(shutdown).await },
            )?;

            let provider = |url: Option<&str>,
                            token: Option<&str>|
             -> Result<Option<HttpFixedActionProvider>, RuntimeError> {
                let Some(url) = url else {
                    return Ok(None);
                };
                let token = token.ok_or_else(|| {
                    RuntimeError::Unsupported(
                        "fixed-action provider token is required when its URL is configured"
                            .to_string(),
                    )
                })?;
                HttpFixedActionProvider::new(url, token.to_string())
                    .map(Some)
                    .map_err(|error| Self::runtime_error("fixed-action provider", error))
            };
            let repository_provider = provider(
                workflow_config.fixed_actions.repository_url.as_deref(),
                workflow_config.fixed_actions.repository_token.as_deref(),
            )?;
            let release_provider = provider(
                workflow_config.fixed_actions.release_url.as_deref(),
                workflow_config.fixed_actions.release_token.as_deref(),
            )?;
            let fixed_actions = FixedActionExecutor::new(
                pool.clone(),
                workflow_config.fixed_actions.root.clone(),
                workflow_config.fixed_actions.artifact_root.clone(),
                workflow_config.fixed_actions.branch_prefix.clone(),
                ProtectedPathPolicy::default_deny(),
            )
            .with_providers(repository_provider, release_provider);
            self.register_task(
                &context,
                "light-workflow-fixed-actions",
                &cancellation,
                &health,
                move |shutdown| async move { fixed_actions.run(shutdown).await },
            )?;

            if let Some(store) = artifact_store {
                let retention = ArtifactRetentionReconciler::new(pool.clone(), store, 100);
                self.register_task(
                    &context,
                    "light-workflow-artifact-retention",
                    &cancellation,
                    &health,
                    move |shutdown| async move { retention.run(shutdown).await },
                )?;
            }
        } else {
            info!("Runner execution is disabled");
        }

        context
            .lifecycle
            .register(Arc::new(self.operational_metadata.clone()))?;

        info!(
            address = %workflow_config.http_addr,
            maximumParallelism = workflow_config.maximum_parallelism,
            "Light Workflow API lifecycle initialized"
        );
        Ok(build_rule_api_router(
            pool,
            workflow_config.database_url,
            runtime_config,
            invocation_security,
            workflow_config.environment,
            health,
        ))
    }

    fn control_routes(&self) -> &'static [ControlRoute] {
        &[
            ControlRoute {
                method: "GET",
                path: "/health",
                kind: ControlRouteKind::Liveness,
            },
            ControlRoute {
                method: "GET",
                path: "/ready",
                kind: ControlRouteKind::Readiness,
            },
            ControlRoute {
                method: "GET",
                path: "/metrics",
                kind: ControlRouteKind::Metrics,
            },
        ]
    }

    fn registration_tags(&self) -> std::collections::HashMap<String, String> {
        self.operational_metadata.registration_tags()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let watcher = ShutdownWatcher::install()?;
    let tracing_guard = init_tracing(
        TracingOptions::new("light-workflow")
            .with_default_filter("light_workflow=debug,info")
            .with_legacy_ansi_env("WORKFLOW_LOG_ANSI"),
    )?;

    info!("Light Workflow Engine starting...");
    let mut config_activation = config_bootstrap::prepare_workflow_config().await?;
    config_activation.runtime_config.service_identity.version = env!("CARGO_PKG_VERSION").into();
    let managed_configuration = config_activation.provenance.source != ConfigSource::Local;
    info!(
        event = "workflow.config.activated",
        source = ?config_activation.provenance.source,
        snapshotId = config_activation.provenance.snapshot_id.as_deref().unwrap_or("local"),
        portalConfigInstanceId = config_activation.provenance.instance_id.as_deref().unwrap_or("local"),
        digest = %config_activation.provenance.content_digest,
        hostId = config_activation.provenance.host_id.as_deref().unwrap_or("local"),
        environment = %config_activation.runtime_config.server.environment,
        serviceId = %config_activation.runtime_config.server.service_id,
        degraded = config_activation.degraded,
        "workflow configuration activated"
    );
    if config_activation.degraded {
        warn!(
            event = "workflow.config.lkg_activated",
            snapshotId = config_activation.provenance.snapshot_id.as_deref().unwrap_or("unknown"),
            digest = %config_activation.provenance.content_digest,
            reasonCode = "CONFIG_SERVER_UNAVAILABLE",
            cacheAgeSeconds = config_activation.cache_age_seconds.unwrap_or_default(),
            "last-known-good workflow configuration activated"
        );
    }

    let operational_metadata = WorkflowOperationalMetadata::new(
        &config_activation.provenance,
        config_activation.degraded,
        config_activation.cache_age_seconds,
        config_activation.runtime_config.registry_client.clone(),
    );
    let app = WorkflowApp {
        managed_configuration,
        compatibility_environment: config_activation.compatibility_environment,
        operational_metadata,
        provenance: config_activation.provenance,
        cache_dir: config_activation.cache_dir,
        _materialized_config_dir: config_activation.materialized_config_dir,
    };
    LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_embedded_config(embedded_config::FILES)
        .with_prepared_config(config_activation.runtime_config)
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build()
        .run_until_shutdown(watcher)
        .await
        .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_security::SecurityConfig;
    use light_workflow::configuration::{ArtifactSettings, FixedActionSettings, RunnerSettings};
    use std::collections::BTreeMap;

    fn workflow_configuration() -> WorkflowConfiguration {
        WorkflowConfiguration {
            environment: "dev".to_string(),
            http_addr: "127.0.0.1:8436".parse().unwrap(),
            database_url: "postgres://workflow".to_string(),
            database_max_connections: 8,
            invocation_caller_service_ids: vec!["gateway".to_string()],
            wait_listener_connections: 4,
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
                issuer: "issuer".to_string(),
                audience: vec!["workflow".to_string()],
                ..SecurityConfig::default()
            },
            managed: true,
        }
    }

    fn provenance() -> ConfigProvenance {
        ConfigProvenance {
            source: ConfigSource::Remote,
            host_id: Some("host".to_string()),
            snapshot_id: Some("snapshot-rejected".to_string()),
            instance_id: Some("instance".to_string()),
            content_digest: "digest-rejected".to_string(),
        }
    }

    #[tokio::test]
    async fn embedded_security_config_builds_the_workflow_jwt_verifier() {
        let runtime_config = LightRuntimeBuilder::new(TestHeadlessTransport)
            .with_embedded_config(embedded_config::FILES)
            .with_config_dir(CONFIG_DIR)
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        let security = load_security_runtime(&runtime_config, true)
            .unwrap()
            .expect("JWT verification enabled");

        assert_eq!(security.config.issuer, "urn:com:networknt:oauth2:v1");
        assert_eq!(security.config.audience, ["urn:com.networknt"]);
    }

    #[tokio::test]
    async fn workflow_reloader_is_exclusive_and_rejects_without_mutating_active_generation() {
        let runtime = LightRuntimeBuilder::new(TestHeadlessTransport)
            .with_embedded_config(embedded_config::FILES)
            .with_config_dir(CONFIG_DIR)
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        let workflow = workflow_configuration();
        let provenance = provenance();
        let manager = Arc::new(WorkflowConfigManager::new(&workflow, &provenance));
        let metadata = WorkflowOperationalMetadata::new(&provenance, false, None, None);
        let health = WorkflowHealth::default();
        let reloader = WorkflowConfigReloader {
            active: tokio::sync::Mutex::new((
                WorkflowRestartBaseline::from_runtime(&runtime),
                workflow,
            )),
            managed_configuration: true,
            compatibility_environment: "dev".to_string(),
            cache_dir: tempfile::tempdir().unwrap().path().to_path_buf(),
            manager: Arc::clone(&manager),
            module_registry: Weak::new(),
            health: health.clone(),
            operational_metadata: metadata.clone(),
        };

        assert!(reloader.requires_exclusive_reload());
        let error = reloader
            .reload(ReloadContext::new(runtime))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no Config Server provenance"));
        assert_eq!(manager.load().config.generation, 1);

        let _ = reloader
            .reject(
                "RESTART_REQUIRED",
                "restart required".to_string(),
                vec!["security.yml".to_string()],
                Some(&provenance),
            )
            .await;
        let tags = metadata.registration_tags();
        assert_eq!(
            tags["light.workflow.config.rejectedSnapshotId"],
            "snapshot-rejected"
        );
        assert_eq!(
            tags["light.workflow.config.rejectedDigest"],
            "digest-rejected"
        );
        let metrics = health.metrics(&manager);
        for name in [
            "light_workflow_config_active_info",
            "light_workflow_config_refresh_total",
            "light_workflow_config_candidate_rejections_total",
            "light_workflow_config_lkg_uses_total",
            "light_workflow_config_last_success_unixtime_seconds",
            "light_workflow_registry_connected",
            "light_workflow_lifecycle_drain_state",
            "light_workflow_capacity_configured",
        ] {
            assert!(metrics.contains(name), "missing metric {name}");
        }
    }

    #[derive(Clone, Copy)]
    struct TestHeadlessTransport;

    #[async_trait::async_trait]
    impl light_runtime::TransportRuntime for TestHeadlessTransport {
        type Handle = ();

        async fn bind(
            &self,
            _config: &RuntimeConfig,
            _lifecycle: &light_runtime::LifecycleRegistrar,
            _admission: &light_runtime::AdmissionGate,
            _startup_cancel: CancellationToken,
        ) -> Result<light_runtime::BoundTransport<Self::Handle>, RuntimeError> {
            Err(RuntimeError::Unsupported(
                "test transport does not bind".into(),
            ))
        }

        async fn stop(
            &self,
            _handle: &mut Self::Handle,
            _context: &ShutdownContext,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }
}
