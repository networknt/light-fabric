use anyhow::{Context, Result};
use provider_qualification_runner::{QualificationRunner, RunnerConfig};
use std::fs;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .json()
        .init();
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config/provider-qualification-runner.json".to_string());
    let config = serde_json::from_slice::<RunnerConfig>(
        &fs::read(&path).with_context(|| format!("read {path}"))?,
    )?;
    QualificationRunner::new(config)?.run_forever().await
}
