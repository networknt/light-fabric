mod command_template;
mod configuration;
mod consumer;
mod events;
mod executor;
mod lease_reaper;
mod repositories;
mod result_reconciler;
mod rule_api;
mod runner_scheduler;

use configuration::RunnerExecutionConfig;
use consumer::EventConsumer;
use executor::TaskExecutor;
use lease_reaper::LeaseReaper;
use light_runtime::{TracingOptions, init_tracing};
use result_reconciler::ResultReconciler;
use rule_api::run_rule_api;
use runner_scheduler::RunnerScheduler;
use sqlx::postgres::PgPoolOptions;
use std::env;
use std::error::Error;
use std::io;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _tracing_guard = init_tracing(
        TracingOptions::new("light-workflow")
            .with_default_filter("light_workflow=debug,info")
            .with_legacy_ansi_env("WORKFLOW_LOG_ANSI"),
    )?;

    info!("Light Workflow Engine starting...");

    // Database connection
    let db_url = env::var("DATABASE_URL").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "DATABASE_URL environment variable must be set",
        )
    })?;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;

    info!("Connected to Postgres");
    let runner_config = RunnerExecutionConfig::load().map_err(io::Error::other)?;

    // Initialize Consumer
    let consumer = EventConsumer::new(
        pool.clone(),
        "workflow-engine-group".to_string(),
        0,  // partition_id
        1,  // total_partitions
        10, // batch_size
    )
    .with_execution_profiles(runner_config.profiles.clone());

    // Initialize Executor
    let executor = Arc::new(
        TaskExecutor::new(pool.clone()).with_execution_profiles(runner_config.profiles.clone()),
    );

    // Run them concurrently
    let consumer_handle = tokio::spawn(async move { consumer.run().await });

    let host_executor = Arc::clone(&executor);
    let executor_handle = tokio::spawn(async move { host_executor.run().await });
    let rule_api_handle = tokio::spawn(async move { run_rule_api().await });
    let runner_runtime = if runner_config.enabled {
        let scheduler = RunnerScheduler::new(pool.clone(), runner_config.clone());
        let reconciler = ResultReconciler::new(
            pool.clone(),
            Arc::clone(&executor),
            runner_config.origin_service_id.clone(),
            runner_config.origin_instance_id.clone(),
        );
        let lease_reaper = LeaseReaper::new(pool.clone());
        Some(tokio::spawn(async move {
            tokio::try_join!(scheduler.run(), reconciler.run(), lease_reaper.run()).map(|_| ())
        }))
    } else {
        info!("Runner execution is disabled");
        None
    };

    tokio::try_join!(
        async {
            consumer_handle
                .await
                .map_err(|err| -> Box<dyn Error + Send + Sync> {
                    Box::new(io::Error::other(format!(
                        "consumer task failed to join: {err}"
                    )))
                })?
                .map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>)
        },
        async {
            executor_handle
                .await
                .map_err(|err| -> Box<dyn Error + Send + Sync> {
                    Box::new(io::Error::other(format!(
                        "executor task failed to join: {err}"
                    )))
                })?
                .map_err(|err| err)
        },
        async {
            rule_api_handle
                .await
                .map_err(|err| -> Box<dyn Error + Send + Sync> {
                    Box::new(io::Error::other(format!(
                        "rule API task failed to join: {err}"
                    )))
                })?
        },
        async {
            match runner_runtime {
                Some(handle) => handle
                    .await
                    .map_err(|err| -> Box<dyn Error + Send + Sync> {
                        Box::new(io::Error::other(format!(
                            "runner runtime failed to join: {err}"
                        )))
                    })?
                    .map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>),
                None => std::future::pending::<Result<(), Box<dyn Error + Send + Sync>>>().await,
            }
        }
    )?;

    Ok(())
}
