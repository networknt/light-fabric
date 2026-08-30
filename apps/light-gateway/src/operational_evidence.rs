use gateway_operational_store::{
    ExpectedBinding, HttpPublisher, Repository, SpoolLimits, StoreError, read_database_url,
    read_secret,
};
use light_runtime::{AdmissionGate, MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use tracing::{error, warn};
use uuid::Uuid;

pub const GATEWAY_EVIDENCE_FILE: &str = "gateway-evidence.yml";
pub const GATEWAY_EVIDENCE_MODULE_ID: &str = "light-gateway/gateway-evidence";
const GATEWAY_EVIDENCE_CONFIG_NAME: &str = "gatewayEvidence";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayEvidenceConfig {
    #[serde(default)]
    enabled: bool,
    database_url_file: String,
    binding_id: Uuid,
    binding_digest: String,
    host_id: Uuid,
    environment: String,
    #[serde(default = "default_generation")]
    minimum_schema_generation: i64,
    credential_generation: u64,
    gateway_instance: String,
    maximum_pending_records: i64,
    maximum_pending_bytes: i64,
    sink_endpoint: String,
    #[serde(default)]
    sink_bearer_token_file: String,
    publisher_batch_records: i64,
    publisher_poll_ms: u64,
    publisher_retry_ms: u64,
    publisher_lease_seconds: u64,
    delivered_retention_seconds: i64,
}

fn default_generation() -> i64 {
    1
}

pub struct GatewayEvidenceRuntime {
    repository: Repository,
    expected_binding: ExpectedBindingOwned,
    validated: OnceCell<()>,
}

#[derive(Clone)]
struct ExpectedBindingOwned {
    binding_id: Uuid,
    binding_digest: String,
    host_id: Uuid,
    environment: String,
    minimum_schema_generation: i64,
}

impl GatewayEvidenceRuntime {
    pub async fn record(
        &self,
        record: &gateway_operational_store::EvidenceRecord,
    ) -> Result<gateway_operational_store::AdmissionOutcome, StoreError> {
        self.ensure_validated().await?;
        self.repository.record(record).await
    }

    async fn ensure_validated(&self) -> Result<(), StoreError> {
        self.validated
            .get_or_try_init(|| async {
                gateway_operational_store::validate(
                    self.repository.pool(),
                    &ExpectedBinding {
                        binding_id: self.expected_binding.binding_id,
                        binding_digest: &self.expected_binding.binding_digest,
                        host_id: self.expected_binding.host_id,
                        environment: &self.expected_binding.environment,
                        minimum_schema_generation: self.expected_binding.minimum_schema_generation,
                    },
                )
                .await
            })
            .await
            .map(|_| ())
    }
}

pub fn load_gateway_evidence_runtime(
    runtime_config: &RuntimeConfig,
    admission: AdmissionGate,
) -> Result<Option<Arc<GatewayEvidenceRuntime>>, RuntimeError> {
    let config = runtime_config
        .module_registry
        .load_config::<GatewayEvidenceConfig>(runtime_config, GATEWAY_EVIDENCE_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        GATEWAY_EVIDENCE_MODULE_ID,
        GATEWAY_EVIDENCE_CONFIG_NAME,
        ModuleKind::Application,
        &config,
        [MaskSpec::key("sinkBearerTokenFile")],
        config.enabled,
        Some(config.enabled),
        false,
    )?;
    if !config.enabled {
        return Ok(None);
    }
    validate_config(&config)?;
    let database_url = read_database_url(Path::new(&config.database_url_file))
        .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_lazy(&database_url)
        .map_err(|error| RuntimeError::Config(format!("invalid Gateway database URL: {error}")))?;
    let repository = Repository::new(
        pool,
        config.host_id,
        config.gateway_instance.clone(),
        SpoolLimits {
            maximum_pending_records: config.maximum_pending_records,
            maximum_pending_bytes: config.maximum_pending_bytes,
        },
    )
    .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let bearer_token = if config.sink_bearer_token_file.trim().is_empty() {
        None
    } else {
        Some(
            read_secret(
                Path::new(&config.sink_bearer_token_file),
                "gateway evidence sink bearer token",
                8192,
            )
            .map_err(|error| RuntimeError::Config(error.to_string()))?,
        )
    };
    let publisher = HttpPublisher::new(config.sink_endpoint.clone(), bearer_token)
        .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let runtime = Arc::new(GatewayEvidenceRuntime {
        repository,
        expected_binding: ExpectedBindingOwned {
            binding_id: config.binding_id,
            binding_digest: config.binding_digest.clone(),
            host_id: config.host_id,
            environment: config.environment.clone(),
            minimum_schema_generation: config.minimum_schema_generation,
        },
        validated: OnceCell::new(),
    });
    start_publisher(Arc::clone(&runtime), publisher, config, admission);
    Ok(Some(runtime))
}

fn start_publisher(
    runtime: Arc<GatewayEvidenceRuntime>,
    publisher: HttpPublisher,
    config: GatewayEvidenceConfig,
    admission: AdmissionGate,
) {
    tokio::spawn(async move {
        let poll = Duration::from_millis(config.publisher_poll_ms);
        let retry = Duration::from_millis(config.publisher_retry_ms);
        let lease = Duration::from_secs(config.publisher_lease_seconds);
        loop {
            if let Err(error) = runtime.ensure_validated().await {
                if matches!(error, StoreError::Scope(_)) {
                    admission.fail();
                    error!(error = %error, "Gateway evidence binding is invalid; application admission failed closed");
                    return;
                }
                warn!(error = %error, "Gateway evidence binding validation failed; publisher will retry");
                tokio::time::sleep(retry).await;
                continue;
            }
            let records = match runtime
                .repository
                .claim(
                    &format!("{}:publisher", config.gateway_instance),
                    config.publisher_batch_records,
                    lease,
                )
                .await
            {
                Ok(records) => records,
                Err(error) => {
                    warn!(error = %error, "Gateway evidence claim failed; publisher will retry");
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };
            if records.is_empty() {
                let cutoff = chrono::Utc::now()
                    - chrono::Duration::seconds(config.delivered_retention_seconds);
                if let Err(error) = runtime.repository.purge_delivered_before(cutoff).await {
                    warn!(error = %error, "Gateway delivered-evidence purge failed");
                }
                tokio::time::sleep(poll).await;
                continue;
            }
            match publisher.publish(&records).await {
                Ok(()) => {
                    if let Err(error) = runtime.repository.delivered(&records).await {
                        error!(error = %error, "Gateway evidence delivery acknowledgement failed");
                    }
                }
                Err(error) => {
                    warn!(error = %error, "Gateway evidence sink is unavailable; bounded spool retained the batch");
                    if let Err(retry_error) = runtime
                        .repository
                        .retry(&records, "sink_unavailable", retry)
                        .await
                    {
                        error!(error = %retry_error, "Gateway evidence retry scheduling failed");
                    }
                }
            }
        }
    });
}

fn validate_config(config: &GatewayEvidenceConfig) -> Result<(), RuntimeError> {
    if config.binding_id.is_nil()
        || config.host_id.is_nil()
        || config.environment.trim().is_empty()
        || config.gateway_instance.trim().is_empty()
        || config.minimum_schema_generation < 1
        || config.credential_generation < 1
        || config.maximum_pending_records < 1
        || config.maximum_pending_bytes < 1
        || config.publisher_batch_records < 1
        || config.publisher_poll_ms < 10
        || config.publisher_retry_ms < 10
        || config.publisher_lease_seconds < 1
        || config.delivered_retention_seconds < 0
        || config.binding_digest.len() != 71
        || !config.binding_digest.starts_with("sha256:")
    {
        return Err(RuntimeError::Config(
            "invalid enabled gatewayEvidence projection".into(),
        ));
    }
    Ok(())
}
