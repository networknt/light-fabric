use std::sync::Arc;

use light_workflow_runner::{
    configuration::RunnerConfig, health, journal::Journal, staging::InputStager,
    supervisor::Supervisor, transport,
};
use tokio::{signal, sync::watch};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    let health_state = health::HealthState::new(Arc::clone(&supervisor));
    let health_address = config.health_address;
    let watchdog = Arc::clone(&supervisor);
    tokio::spawn(async move { watchdog.run_watchdog().await });
    let orphan_reconciler = Arc::clone(&supervisor);
    tokio::spawn(async move { orphan_reconciler.run_orphan_reconciler().await });
    let health_for_server = Arc::clone(&health_state);
    tokio::spawn(async move {
        if let Err(error) = health::serve(health_address, health_for_server).await {
            error!(%error, "runner health server stopped");
        }
    });

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let transport = tokio::spawn(transport::run(
        Arc::clone(&config),
        Arc::clone(&supervisor),
        health_state,
        shutdown_rx,
    ));
    info!(runner_id = %config.runner_id, "light-workflow-runner started");
    signal::ctrl_c().await?;
    supervisor.drain();
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(config.shutdown_grace, transport).await;
    info!("light-workflow-runner draining");
    Ok(())
}
