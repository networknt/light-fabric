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

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("authorization")
        || key.contains("password")
        || key.contains("secret")
        || key.contains("token")
        || key.contains("api_key")
        || key.ends_with("key")
}

fn sanitize_json_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *value = serde_json::Value::String("[REDACTED]".to_string());
                } else {
                    sanitize_json_value(value);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                sanitize_json_value(value);
            }
        }
        _ => {}
    }
}

async fn info(config: Arc<RuntimeConfig>) -> Json<serde_json::Value> {
    let mut sanitized_config = serde_json::to_value(config.as_ref())
        .unwrap_or_else(|_| serde_json::json!({ "error": "config unavailable" }));
    sanitize_json_value(&mut sanitized_config);

    Json(serde_json::json!({
        "status": "ready",
        "config": sanitized_config
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
