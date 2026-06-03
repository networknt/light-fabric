mod consumer;
mod events;
mod executor;
mod rule_api;

use consumer::EventConsumer;
use executor::TaskExecutor;
use light_runtime::{TracingOptions, init_tracing};
use rule_api::run_rule_api;
use sqlx::postgres::PgPoolOptions;
use std::env;
use std::error::Error;
use std::io;
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

    // Initialize Consumer
    let consumer = EventConsumer::new(
        pool.clone(),
        "workflow-engine-group".to_string(),
        0,  // partition_id
        1,  // total_partitions
        10, // batch_size
    );

    // Initialize Executor
    let executor = TaskExecutor::new(pool);

    // Run them concurrently
    let consumer_handle = tokio::spawn(async move { consumer.run().await });

    let executor_handle = tokio::spawn(async move { executor.run().await });
    let rule_api_handle = tokio::spawn(async move { run_rule_api().await });

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
        }
    )?;

    Ok(())
}
