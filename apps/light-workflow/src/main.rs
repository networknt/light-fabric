use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use execution_security::ProtectedPathPolicy;
use light_runtime::{
    BoundTransport, LightRuntimeBuilder, RuntimeConfig, RuntimeError, TracingOptions,
    TransportRuntime, init_tracing,
};
use light_security::load_security_runtime;
use light_workflow::agent_job::AgentJobReconciler;
use light_workflow::artifact_retention::ArtifactRetentionReconciler;
use light_workflow::artifact_store::DurableArtifactStore;
use light_workflow::configuration::{RunnerExecutionConfig, maximum_parallelism_from_environment};
use light_workflow::consumer::EventConsumer;
use light_workflow::executor::TaskExecutor;
use light_workflow::fixed_action::{FixedActionExecutor, HttpFixedActionProvider};
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

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";

#[derive(Debug, Clone, Copy)]
struct HeadlessTransport;

#[async_trait]
impl TransportRuntime for HeadlessTransport {
    type Handle = ();

    async fn bind(
        &self,
        _config: &RuntimeConfig,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        Err(RuntimeError::Unsupported(
            "light-workflow builds its Axum listener separately".into(),
        ))
    }

    async fn stop(&self, _handle: &mut Self::Handle) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let _tracing_guard = init_tracing(
        TracingOptions::new("light-workflow")
            .with_default_filter("light_workflow=debug,info")
            .with_legacy_ansi_env("WORKFLOW_LOG_ANSI"),
    )?;

    info!("Light Workflow Engine starting...");
    let invocation_environment = required_environment("SERVER_ENVIRONMENT")?;
    validate_service_authorization(&invocation_environment)?;
    let invocation_caller_service_ids =
        required_list_environment("WORKFLOW_INVOCATION_CALLER_SERVICE_IDS")?;

    let runtime_config = LightRuntimeBuilder::new(HeadlessTransport)
        .with_embedded_config(embedded_config::FILES)
        .with_config_dir(CONFIG_DIR)
        .build()
        .prepare_local_config()
        .await?;
    let invocation_security = Arc::new(
        load_security_runtime(&runtime_config, true)?
            .ok_or_else(|| io::Error::other("workflow JWT verification must be enabled"))?,
    );
    invocation_security.bootstrap().await.map_err(|error| {
        io::Error::other(format!(
            "workflow JWKS bootstrap failed ({}): {}",
            error.code, error.message
        ))
    })?;

    // Database connection
    let db_url = env::var("DATABASE_URL").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "DATABASE_URL environment variable must be set",
        )
    })?;
    let database_max_connections = env::var("WORKFLOW_DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(32)
        .clamp(8, 512);
    let pool = PgPoolOptions::new()
        .max_connections(database_max_connections)
        .connect(&db_url)
        .await?;

    info!("Connected to Postgres");
    let runner_config = RunnerExecutionConfig::load().map_err(io::Error::other)?;
    let maximum_parallelism = maximum_parallelism_from_environment().map_err(io::Error::other)?;
    let artifact_store = DurableArtifactStore::from_environment().map_err(io::Error::other)?;
    let artifact_retention_days = env::var("WORKFLOW_ARTIFACT_RETENTION_DAYS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(30_i64)
        .clamp(1, 3650);

    // Initialize Consumer
    let consumer = EventConsumer::new(
        pool.clone(),
        "workflow-engine-group".to_string(),
        0,  // partition_id
        1,  // total_partitions
        10, // batch_size
    )
    .with_maximum_parallelism(maximum_parallelism)
    .with_execution_profiles(runner_config.profiles.clone());

    // Initialize Executor
    let executor = Arc::new(
        TaskExecutor::new(pool.clone()).with_execution_profiles(runner_config.profiles.clone()),
    );

    // Run them concurrently
    let consumer_handle = tokio::spawn(async move { consumer.run().await });

    let host_executor = Arc::clone(&executor);
    let executor_handle = tokio::spawn(async move { host_executor.run().await });
    let agent_job_reconciler = AgentJobReconciler::new(pool.clone(), Arc::clone(&executor));
    let agent_job_handle = tokio::spawn(async move { agent_job_reconciler.run().await });
    let invocation_pool = pool.clone();
    let rule_api_handle = tokio::spawn(async move {
        run_rule_api(
            invocation_pool,
            maximum_parallelism,
            invocation_security,
            invocation_environment,
            invocation_caller_service_ids,
        )
        .await
    });
    let runner_runtime = if runner_config.enabled {
        let scheduler = RunnerScheduler::new(pool.clone(), runner_config.clone());
        let reconciler = ResultReconciler::new(
            pool.clone(),
            Arc::clone(&executor),
            runner_config.origin_service_id.clone(),
            runner_config.origin_instance_id.clone(),
            artifact_store.clone(),
            artifact_retention_days,
        );
        let lease_reaper = LeaseReaper::new(pool.clone());
        let session_reconciler = ExecutionSessionReconciler::new(
            pool.clone(),
            runner_config.origin_service_id.clone(),
            runner_config.origin_instance_id.clone(),
        );
        let provider = |url_name: &str,
                        token_name: &str|
         -> Result<Option<HttpFixedActionProvider>, io::Error> {
            let Some(url) = env::var(url_name)
                .ok()
                .filter(|value| !value.trim().is_empty())
            else {
                return Ok(None);
            };
            let token = env::var(token_name).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{token_name} is required when {url_name} is configured"),
                )
            })?;
            HttpFixedActionProvider::new(&url, token)
                .map(Some)
                .map_err(io::Error::other)
        };
        let repository_provider = provider(
            "WORKFLOW_REPOSITORY_FIXED_ACTION_URL",
            "WORKFLOW_REPOSITORY_FIXED_ACTION_TOKEN",
        )?;
        let release_provider = provider(
            "WORKFLOW_RELEASE_FIXED_ACTION_URL",
            "WORKFLOW_RELEASE_FIXED_ACTION_TOKEN",
        )?;
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
        )
        .with_providers(repository_provider, release_provider);
        let retention_store = artifact_store.clone();
        Some(tokio::spawn(async move {
            let retention = async move {
                match retention_store {
                    Some(store) => {
                        ArtifactRetentionReconciler::new(pool.clone(), store, 100)
                            .run()
                            .await
                    }
                    None => std::future::pending::<Result<(), sqlx::Error>>().await,
                }
            };
            tokio::try_join!(
                scheduler.run(),
                reconciler.run(),
                lease_reaper.run(),
                session_reconciler.run(),
                fixed_actions.run(),
                retention
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
            agent_job_handle
                .await
                .map_err(|err| -> Box<dyn Error + Send + Sync> {
                    Box::new(io::Error::other(format!(
                        "agent job reconciler failed to join: {err}"
                    )))
                })?
                .map_err(|err| Box::new(err) as Box<dyn Error + Send + Sync>)
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

fn validate_service_authorization(expected_environment: &str) -> Result<(), io::Error> {
    let value = env::var("LIGHT_PORTAL_AUTHORIZATION").map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "LIGHT_PORTAL_AUTHORIZATION is required for authenticated outbound service calls",
        )
    })?;
    validate_service_authorization_value(&value, expected_environment)
}

fn validate_service_authorization_value(
    value: &str,
    expected_environment: &str,
) -> Result<(), io::Error> {
    let value = value.trim();
    let token = value
        .split_once(char::is_whitespace)
        .and_then(|(scheme, token)| {
            scheme
                .eq_ignore_ascii_case("bearer")
                .then_some(token.trim())
        })
        .unwrap_or(value);
    if token.len() < 32 || token.chars().any(char::is_whitespace) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LIGHT_PORTAL_AUTHORIZATION must contain a non-empty service bearer token",
        ));
    }
    let payload = token.split('.').nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "LIGHT_PORTAL_AUTHORIZATION must be a JWT",
        )
    })?;
    let claims: serde_json::Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "LIGHT_PORTAL_AUTHORIZATION has an invalid JWT payload",
            )
        })?)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "LIGHT_PORTAL_AUTHORIZATION has an invalid JWT claims object",
            )
        })?;
    if claims.get("env").and_then(serde_json::Value::as_str) != Some(expected_environment) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "LIGHT_PORTAL_AUTHORIZATION env claim must match SERVER_ENVIRONMENT ({expected_environment})"
            ),
        ));
    }
    Ok(())
}

fn required_environment(name: &str) -> Result<String, io::Error> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{name} must be explicitly configured and non-empty"),
            )
        })
}

fn required_list_environment(name: &str) -> Result<Vec<String>, io::Error> {
    parse_required_list(name, &required_environment(name)?)
}

fn parse_required_list(name: &str, value: &str) -> Result<Vec<String>, io::Error> {
    let values = value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name} must contain at least one service ID"),
        ));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_security_config_builds_the_workflow_jwt_verifier() {
        let runtime_config = LightRuntimeBuilder::new(HeadlessTransport)
            .with_embedded_config(embedded_config::FILES)
            .with_config_dir(CONFIG_DIR)
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        let security = load_security_runtime(&runtime_config, true)
            .unwrap()
            .expect("JWT verification enabled");

        assert_eq!(security.config.issuer, "urn:com:networknt:oauth2:v1");
        assert_eq!(security.config.audience, ["urn:com.networknt"]);
    }

    #[test]
    fn service_authorization_is_validated_before_startup() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"env":"dev"}"#);
        let token = format!("header.{payload}.signature-padding-long-enough");
        assert!(validate_service_authorization_value(&format!("bEaReR {token}"), "dev").is_ok());
        assert!(validate_service_authorization_value(&token, "loc").is_err());
        assert!(validate_service_authorization_value("short", "dev").is_err());
    }

    #[test]
    fn required_service_id_list_rejects_empty_entries_only() {
        assert_eq!(
            parse_required_list("CALLERS", " gateway-a, gateway-b ").unwrap(),
            ["gateway-a", "gateway-b"]
        );
        assert!(parse_required_list("CALLERS", " , ").is_err());
    }
}
