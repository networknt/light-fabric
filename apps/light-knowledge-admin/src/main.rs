use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use light_axum::{AxumApp, AxumTransport, ControlRoute, ControlRouteKind, ServerContext};
use light_knowledge_admin::{AdminConfig, AdminState, admin_router};
use light_runtime::{
    LifecycleParticipant, LightRuntimeBuilder, RuntimeConfig, RuntimeError, ShutdownContext,
    ShutdownWatcher, TracingOptions, init_tracing,
};
use sqlx::PgPool;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

#[derive(Clone)]
struct AdminApp;

#[async_trait::async_trait]
impl AxumApp for AdminApp {
    async fn router(&self, context: ServerContext) -> Result<axum::Router, RuntimeError> {
        let config = AdminConfig::load(&context.runtime_config)?;
        let state = AdminState::build(&context.runtime_config, config).await?;
        context
            .lifecycle
            .register(Arc::new(AdminDatabase(state.pool())))?;
        Ok(admin_router(Arc::new(state)))
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
        ]
    }
}

struct AdminDatabase(PgPool);

#[async_trait::async_trait]
impl LifecycleParticipant for AdminDatabase {
    fn name(&self) -> &'static str {
        "light-knowledge-admin-database"
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        tokio::time::timeout(context.remaining(), self.0.close())
            .await
            .map_err(|_| RuntimeError::ShutdownDeadlineExceeded(context.remaining()))?;
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let watcher = ShutdownWatcher::install().context("failed to install shutdown handlers")?;
    let tracing_guard = init_tracing(TracingOptions::new("light-knowledge-admin"))?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }
    let config_dir = std::env::var("LIGHT_KNOWLEDGE_ADMIN_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config"));
    let runtime = LightRuntimeBuilder::new(AxumTransport::new(AdminApp))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir("config-defaults")
        .with_config_dir(config_dir)
        .with_external_config_dir("config-cache")
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build();
    runtime
        .run_until_shutdown(watcher)
        .await
        .context("light-knowledge-admin lifecycle failed")?;
    Ok(())
}
