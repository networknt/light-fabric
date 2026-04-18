mod consumer;
mod events;
mod executor;

use consumer::EventConsumer;
use executor::TaskExecutor;
use sqlx::postgres::PgPoolOptions;
use std::env;
use std::error::Error;
use std::io;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(
            env::var("RUST_LOG").unwrap_or_else(|_| "light_workflow=debug,info".into()),
        ))
        .with(tracing_subscriber::fmt::layer())
        .init();

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
        }
    )?;

    Ok(())
}
