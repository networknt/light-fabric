use execution_security::ProtectedPathPolicy;
use light_runtime::{TracingOptions, init_tracing};
use light_workflow::configuration::RunnerExecutionConfig;
use light_workflow::consumer::EventConsumer;
use light_workflow::executor::TaskExecutor;
use light_workflow::fixed_action::FixedActionExecutor;
use light_workflow::lease_reaper::LeaseReaper;
use light_workflow::result_reconciler::ResultReconciler;
use light_workflow::rule_api::run_rule_api;
use light_workflow::runner_scheduler::RunnerScheduler;
use light_workflow::session_reconciler::ExecutionSessionReconciler;
use sqlx::postgres::PgPoolOptions;
use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
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
        let session_reconciler = ExecutionSessionReconciler::new(
            pool.clone(),
            runner_config.origin_service_id.clone(),
            runner_config.origin_instance_id.clone(),
        );
        let fixed_actions = FixedActionExecutor::new(
            pool.clone(),
            PathBuf::from(
                env::var("WORKFLOW_FIXED_ACTION_ROOT")
                    .unwrap_or_else(|_| "/var/lib/light-workflow/fixed-actions".into()),
            ),
            PathBuf::from(
                env::var("WORKFLOW_FIXED_ACTION_ARTIFACT_ROOT")
                    .unwrap_or_else(|_| "/var/lib/light-workflow/artifacts".into()),
            ),
            env::var("WORKFLOW_FIXED_ACTION_BRANCH_PREFIX").unwrap_or_else(|_| "agent/".into()),
            ProtectedPathPolicy::default_deny(),
        );
        Some(tokio::spawn(async move {
            tokio::try_join!(
                scheduler.run(),
                reconciler.run(),
                lease_reaper.run(),
                session_reconciler.run(),
                fixed_actions.run()
            )
            .map(|_| ())
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
