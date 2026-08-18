use std::sync::Arc;

use light_runtime::ShutdownWatcher;
use light_workflow_runner::{
    configuration::RunnerConfig, health, journal::Journal, staging::InputStager,
    supervisor::Supervisor, transport,
};
use tokio::sync::watch;
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut watcher = ShutdownWatcher::install()?;
    tracing_subscriber::fmt::init();
    let config = Arc::new(RunnerConfig::load().map_err(std::io::Error::other)?);
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments.first().map(String::as_str) == Some("print-admission") {
        if arguments.len() != 3 {
            return Err(std::io::Error::other(
                "usage: light-workflow-runner print-admission <authenticated-subject> <origin-service-id>",
            )
            .into());
        }
        let document = config
            .admission_document(&arguments[1], &arguments[2])
            .map_err(std::io::Error::other)?;
        println!("{}", serde_json::to_string_pretty(&document)?);
        return Ok(());
    }
    if !arguments.is_empty() {
        return Err(std::io::Error::other(format!(
            "unknown light-workflow-runner command `{}`",
            arguments[0]
        ))
        .into());
    }
    std::fs::create_dir_all(&config.data_directory)?;

    let journal = Journal::open(&config.data_directory.join("execution-journal.sqlite"))
        .map_err(std::io::Error::other)?;
    let stager = InputStager::new(
        config.data_directory.join("staging"),
        config.staging_maximum_bytes,
    )
    .map_err(std::io::Error::other)?;
    let backend = config.build_backend().map_err(std::io::Error::other)?;
    let supervisor = Supervisor::new(
        backend,
        journal,
        stager,
        config.allowed_command_template_digests.clone(),
        config.maximum_concurrency,
        config.agent_worker.clone(),
    );
    tokio::time::timeout(
        config.orphan_reconcile_startup_timeout,
        supervisor.reconcile_backend_orphans_once(),
    )
    .await
    .map_err(|_| std::io::Error::other("startup backend orphan reconciliation timed out"))?
    .map_err(std::io::Error::other)?;
    let health_state = health::HealthState::new(Arc::clone(&supervisor));
    let health_address = config.health_address;
    let watchdog = Arc::clone(&supervisor);
    let watchdog = tokio::spawn(async move { watchdog.run_watchdog().await });
    let orphan_reconciler = Arc::clone(&supervisor);
    let orphan_interval = config.orphan_reconcile_interval;
    let orphan_reconciler = tokio::spawn(async move {
        orphan_reconciler
            .run_orphan_reconciler(orphan_interval)
            .await
    });
    let health_for_server = Arc::clone(&health_state);
    let health_server = tokio::spawn(async move {
        if let Err(error) = health::serve(health_address, health_for_server).await {
            error!(%error, "runner health server stopped");
        }
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut transport = tokio::spawn(transport::run(
        Arc::clone(&config),
        Arc::clone(&supervisor),
        health_state,
        shutdown_rx,
    ));
    info!(runner_id = %config.runner_id, "light-workflow-runner started");
    let reason = watcher.recv().await;
    info!(?reason, "light-workflow-runner shutdown requested");
    supervisor.drain();
    let _ = shutdown_tx.send(true);
    let drain_error = match tokio::time::timeout(config.shutdown_grace, &mut transport).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(std::io::Error::other(format!(
            "workflow runner transport task failed: {error}"
        ))),
        Err(_) => {
            error!("workflow runner drain deadline exceeded");
            transport.abort();
            let _ = transport.await;
            Some(std::io::Error::other(
                "workflow runner drain deadline exceeded",
            ))
        }
    };
    health_server.abort();
    orphan_reconciler.abort();
    watchdog.abort();
    let _ = health_server.await;
    let _ = orphan_reconciler.await;
    let _ = watchdog.await;
    info!("light-workflow-runner draining");
    if let Some(error) = drain_error {
        return Err(error.into());
    }
    Ok(())
}
