use anyhow::Context;
use axum::{Json, Router, routing::get};
use light_runtime::{LightRuntimeBuilder, RuntimeConfig};
use light_axum::{AxumApp, AxumTransport, ServerContext};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[derive(Clone, Default)]
struct AgentApp;

impl AxumApp for AgentApp {
    fn router(&self, context: ServerContext) -> Router {
        Router::new().route("/health", get(health)).route(
            "/info",
            get(move || info(context.runtime_config.clone())),
        )
    }
}

async fn health() -> &'static str {
    "ok"
}

async fn info(config: Arc<RuntimeConfig>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ready",
        "config": config.as_ref()
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(AgentApp))
        .with_config_dir("config")
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start agent runtime")?;
    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;
    running
        .shutdown()
        .await
        .context("failed to shut down agent")?;
    Ok(())
}
