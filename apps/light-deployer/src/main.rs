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
use light_runtime::{
    LightRuntimeBuilder, ModuleRegistry, ShutdownWatcher, TracingOptions, init_tracing,
};
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::info;

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let watcher = ShutdownWatcher::install().context("failed to install shutdown handlers")?;
    let tracing_guard = init_tracing(
        TracingOptions::new("light-deployer").with_default_filter("light_deployer=debug,info"),
    )?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }

    let config_dir = resolve_config_dir();
    let module_registry = Arc::new(ModuleRegistry::new());
    let config = DeployerConfig::load_from_dir_registered(
        embedded_config::FILES,
        &config_dir,
        &module_registry,
    )?;
    let store = &config.operational_store;
    if store.contract_version != operational_store::runtime::CONTRACT_VERSION as u16
        || store.service_owner != "light-deployer"
        || store.schema != "operational_meta"
        || store.server_host.trim().is_empty()
        || store.port == 0
        || !matches!(store.tls_mode.as_str(), "DISABLE" | "PREFER" | "REQUIRE" | "VERIFY_CA" | "VERIFY_FULL")
        || store.credential_generation < 1
    {
        anyhow::bail!("deployer operational-store projection is invalid");
    }
    let database_url = operational_store::runtime::read_database_url(
        &store.database_url_file,
        &store.server_host,
        store.port,
        &store.tls_mode,
        &store.expected_database,
        "deployer_runtime",
    )?;
    let operational_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .context("connect Deployer operational-store verifier")?;
    operational_store::runtime::validate_binding(
        &operational_pool,
        &operational_store::runtime::ExpectedBinding {
            binding_id: store.binding_id,
            binding_digest: &store.binding_digest,
            host_id: store.host_id,
            environment: &store.environment,
            server_host: &store.server_host,
            port: store.port,
            tls_mode: &store.tls_mode,
            expected_database: &store.expected_database,
            role_suffix: "deployer_runtime",
            minimum_schema_generation: store.minimum_schema_generation,
        },
    )
    .await
    .context("validate Deployer Host-specific operational-store audience")?;
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
        .with_embedded_config(embedded_config::FILES)
        .with_module_registry(module_registry)
        .with_config_dir(config_dir)
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build();

    runtime
        .run_until_shutdown(watcher)
        .await
        .context("light-deployer lifecycle failed")?;
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
