use std::sync::Arc;
use std::{env, path::Path, path::PathBuf};

use anyhow::Context;
use light_axum::{AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext};
use light_knowledge::{KnowledgeConfig, KnowledgeState, knowledge_router};
use light_knowledge_worker::EmbeddedKnowledgeRuntime;
use light_runtime::{
    LifecycleParticipant, LightRuntimeBuilder, RuntimeConfig, RuntimeError, ShutdownContext,
    ShutdownWatcher, TracingOptions, init_tracing,
};
use portal_registry::RegistryHandler;
use sqlx::PgPool;
use tokio::sync::RwLock;
use uuid::Uuid;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";

#[derive(Clone)]
struct KnowledgeApp {
    registry_handler: Arc<KnowledgeRegistryHandler>,
    knowledge_config_file: String,
    worker_config_file: String,
}

#[async_trait::async_trait]
impl AxumApp for KnowledgeApp {
    async fn router(&self, context: ServerContext) -> Result<axum::Router, RuntimeError> {
        let config = KnowledgeConfig::load_from_runtime_file(
            &context.runtime_config,
            &self.knowledge_config_file,
        )
        .map_err(RuntimeError::Config)?;
        let state = KnowledgeState::build(&context.runtime_config, config).await?;
        context
            .lifecycle
            .register(Arc::new(KnowledgeDatabase(state.database_pool())))?;
        let background = Arc::new(
            EmbeddedKnowledgeRuntime::start(&context.runtime_config, &self.worker_config_file)
                .await
                .map_err(|error| {
                    RuntimeError::Config(format!("embedded Knowledge components: {error:#}"))
                })?,
        );
        self.registry_handler.install(Arc::clone(&background)).await;
        context
            .lifecycle
            .register(Arc::new(KnowledgeBackground(background)))?;
        Ok(knowledge_router(Arc::new(state)))
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
}

fn configured_path(environment_name: &str) -> anyhow::Result<Option<(PathBuf, String)>> {
    let Ok(value) = env::var(environment_name) else {
        return Ok(None);
    };
    parse_configured_path(environment_name, &value).map(Some)
}

fn parse_configured_path(environment_name: &str, value: &str) -> anyhow::Result<(PathBuf, String)> {
    let path = Path::new(&value);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context(format!(
            "{environment_name} must name a UTF-8 configuration file"
        ))?;
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if file_name.is_empty() {
        anyhow::bail!("{environment_name} must name a configuration file");
    }
    Ok((directory.to_path_buf(), file_name.to_string()))
}

fn configured_files() -> anyhow::Result<(PathBuf, String, String)> {
    let knowledge = configured_path("LIGHT_KNOWLEDGE_CONFIG_FILE")?;
    let worker = configured_path("LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE")?;
    let config_dir = knowledge
        .as_ref()
        .map(|(directory, _)| directory.clone())
        .or_else(|| worker.as_ref().map(|(directory, _)| directory.clone()))
        .unwrap_or_else(|| PathBuf::from(CONFIG_DIR));
    if worker
        .as_ref()
        .is_some_and(|(directory, _)| directory != &config_dir)
    {
        anyhow::bail!(
            "LIGHT_KNOWLEDGE_CONFIG_FILE and LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE must share a directory"
        );
    }
    if knowledge
        .as_ref()
        .is_some_and(|(directory, _)| directory != &config_dir)
    {
        anyhow::bail!(
            "LIGHT_KNOWLEDGE_CONFIG_FILE and LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE must share a directory"
        );
    }
    Ok((
        config_dir,
        knowledge
            .map(|(_, file)| file)
            .unwrap_or_else(|| "knowledge.yml".into()),
        worker
            .map(|(_, file)| file)
            .unwrap_or_else(|| "worker.yml".into()),
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_configured_path;
    use std::path::Path;

    #[test]
    fn absolute_configuration_path_preserves_its_directory() {
        let (directory, file_name) = parse_configured_path(
            "LIGHT_KNOWLEDGE_CONFIG_FILE",
            "/tmp/runtime-config/knowledge.yml",
        )
        .unwrap();
        assert_eq!(directory, Path::new("/tmp/runtime-config"));
        assert_eq!(file_name, "knowledge.yml");
    }
}

struct KnowledgeDatabase(PgPool);

struct KnowledgeBackground(Arc<EmbeddedKnowledgeRuntime>);

#[derive(Default)]
struct KnowledgeRegistryHandler {
    runtime: RwLock<Option<Arc<EmbeddedKnowledgeRuntime>>>,
}

impl KnowledgeRegistryHandler {
    async fn install(&self, runtime: Arc<EmbeddedKnowledgeRuntime>) {
        *self.runtime.write().await = Some(runtime);
    }

    async fn runtime(&self) -> Option<Arc<EmbeddedKnowledgeRuntime>> {
        self.runtime.read().await.clone()
    }
}

#[async_trait::async_trait]
impl RegistryHandler for KnowledgeRegistryHandler {
    async fn handle_notification(&self, method: &str, _params: serde_json::Value) {
        let Some(runtime) = self.runtime().await else {
            return;
        };
        match method {
            "knowledge.wake_projection" => runtime.wake_projection(),
            "knowledge.wake_jobs" => runtime.wake_jobs(),
            _ => {}
        }
    }

    async fn handle_request(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let Some(runtime) = self.runtime().await else {
            return serde_json::json!({
                "supported": true,
                "status": "unavailable",
                "error": {"code": "knowledge_runtime_not_ready"}
            });
        };
        match method {
            "knowledge.get_runtime_status" => runtime.status().await,
            "knowledge.wake_projection" => {
                runtime.wake_projection();
                serde_json::json!({"status": "accepted"})
            }
            "knowledge.wake_jobs" => {
                runtime.wake_jobs();
                serde_json::json!({"status": "accepted"})
            }
            "knowledge.retry_job" => {
                let job_id = params
                    .get("jobId")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| Uuid::parse_str(value).ok());
                match job_id {
                    Some(job_id) => match runtime.retry_job(job_id).await {
                        Ok(true) => serde_json::json!({"status": "accepted", "jobId": job_id}),
                        Ok(false) => serde_json::json!({
                            "status": "rejected",
                            "error": {"code": "knowledge_job_not_retryable"}
                        }),
                        Err(error) => {
                            tracing::warn!(%error, %job_id, "Knowledge job retry command failed");
                            serde_json::json!({
                                "status": "failed",
                                "error": {"code": "knowledge_job_retry_failed"}
                            })
                        }
                    },
                    None => serde_json::json!({
                        "status": "rejected",
                        "error": {"code": "knowledge_job_id_invalid"}
                    }),
                }
            }
            "knowledge.retry_projection_event" => {
                let event_id = params
                    .get("eventId")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|value| Uuid::parse_str(value).ok());
                match event_id {
                    Some(event_id) => match runtime.retry_projection_event(event_id).await {
                        Ok(true) => serde_json::json!({"status": "accepted", "eventId": event_id}),
                        Ok(false) => serde_json::json!({
                            "status": "rejected",
                            "error": {"code": "knowledge_projection_event_not_retryable"}
                        }),
                        Err(error) => {
                            tracing::warn!(%error, %event_id,
                                "Knowledge projection event retry command failed");
                            serde_json::json!({
                                "status": "failed",
                                "error": {"code": "knowledge_projection_event_retry_failed"}
                            })
                        }
                    },
                    None => serde_json::json!({
                        "status": "rejected",
                        "error": {"code": "knowledge_projection_event_id_invalid"}
                    }),
                }
            }
            "knowledge.reload" => serde_json::json!({
                "status": "restart-required",
                "components": ["knowledge-api", "knowledge-projection", "knowledge-jobs"]
            }),
            _ => portal_registry::unsupported_method_response(method),
        }
    }
}

#[async_trait::async_trait]
impl LifecycleParticipant for KnowledgeBackground {
    fn name(&self) -> &'static str {
        "light-knowledge-background"
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        let budget = context.remaining();
        self.0
            .shutdown(budget)
            .await
            .map_err(|error| RuntimeError::CleanupFailed(vec![format!("{}: {error}", self.name())]))
    }
}

#[async_trait::async_trait]
impl LifecycleParticipant for KnowledgeDatabase {
    fn name(&self) -> &'static str {
        "light-knowledge-database"
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let watcher = ShutdownWatcher::install().context("failed to install shutdown handlers")?;
    let tracing_guard = init_tracing(
        TracingOptions::new("light-knowledge").with_legacy_ansi_env("LIGHT_KNOWLEDGE_LOG_ANSI"),
    )?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }

    let registry_handler = Arc::new(KnowledgeRegistryHandler::default());
    let (config_dir, knowledge_config_file, worker_config_file) = configured_files()?;
    let app = KnowledgeApp {
        registry_handler: Arc::clone(&registry_handler),
        knowledge_config_file,
        worker_config_file,
    };
    let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir(DEFAULT_CONFIG_DIR)
        .with_config_dir(config_dir)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
        .with_registry_handler(registry_handler)
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build();
    runtime
        .run_until_shutdown(watcher)
        .await
        .context("light-knowledge lifecycle failed")?;
    Ok(())
}
