use std::sync::Arc;

use anyhow::Context;
use light_axum::{AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext};
use light_knowledge::{KnowledgeConfig, KnowledgeState, knowledge_router};
use light_runtime::{
    LifecycleParticipant, LightRuntimeBuilder, RuntimeConfig, RuntimeError, ShutdownContext,
    ShutdownWatcher, TracingOptions, init_tracing,
};
use sqlx::PgPool;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";

#[derive(Clone)]
struct KnowledgeApp;

#[async_trait::async_trait]
impl AxumApp for KnowledgeApp {
    async fn router(&self, context: ServerContext) -> Result<axum::Router, RuntimeError> {
        let config = KnowledgeConfig::load().map_err(RuntimeError::Config)?;
        let state = KnowledgeState::build(&context.runtime_config, config).await?;
        context
            .lifecycle
            .register(Arc::new(KnowledgeDatabase(state.database_pool())))?;
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

struct KnowledgeDatabase(PgPool);

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

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(KnowledgeApp))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir(DEFAULT_CONFIG_DIR)
        .with_config_dir(CONFIG_DIR)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
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
