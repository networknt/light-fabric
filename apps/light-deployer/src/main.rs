mod api;
mod config;
mod deployer;
mod events;
mod git;
mod kube;
mod model;
mod policy;
mod prune;
mod renderer;

use crate::api::DeployerApp;
use crate::config::DeployerConfig;
use crate::deployer::DeployerService;
use crate::events::EventHub;
use crate::git::LocalTemplateSource;
use crate::kube::{KubeExecutor, KubeRsExecutor, NoopKubeExecutor};
use crate::policy::Policy;
use anyhow::Context;
use light_axum::AxumTransport;
use light_runtime::LightRuntimeBuilder;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config_dir = resolve_config_dir();
    let config = DeployerConfig::load_from_dir(&config_dir)?;
    let template_base_dir = std::env::var("LIGHT_DEPLOYER_TEMPLATE_BASE_DIR")
        .ok()
        .map(PathBuf::from);
    let remote_cache_dir = std::env::var("LIGHT_DEPLOYER_REMOTE_CACHE_DIR")
        .ok()
        .map(PathBuf::from);

    info!(
        deployer_id = %config.deployer_id,
        cluster_id = %config.cluster_id,
        "starting light-deployer"
    );

    let policy = Policy::new(config);
    let template_source = Arc::new(LocalTemplateSource {
        base_dir: template_base_dir,
        remote_cache_dir,
    });
    let kube: Arc<dyn KubeExecutor> = if should_use_real_kube() {
        Arc::new(KubeRsExecutor::try_default().await?)
    } else {
        Arc::new(NoopKubeExecutor)
    };
    let events = EventHub::new(1024);
    let service = DeployerService::new(policy, template_source, kube, events);
    let app = DeployerApp::new(service);
    let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
        .with_config_dir(config_dir)
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start light-deployer runtime")?;
    tokio::signal::ctrl_c()
        .await
        .context("failed to wait for shutdown signal")?;
    running.shutdown().await?;
    Ok(())
}

fn resolve_config_dir() -> PathBuf {
    if let Ok(path) = std::env::var("LIGHT_DEPLOYER_CONFIG_DIR") {
        return PathBuf::from(path);
    }

    let workspace_config = PathBuf::from("apps/light-deployer/config");
    if workspace_config.exists() {
        workspace_config
    } else {
        PathBuf::from("config")
    }
}

fn should_use_real_kube() -> bool {
    match std::env::var("LIGHT_DEPLOYER_KUBE_MODE") {
        Ok(mode) if mode.eq_ignore_ascii_case("real") => true,
        Ok(mode) if mode.eq_ignore_ascii_case("noop") => false,
        _ => std::env::var("KUBERNETES_SERVICE_HOST").is_ok(),
    }
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "light_deployer=debug,info".into()),
    );
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .init();
}
