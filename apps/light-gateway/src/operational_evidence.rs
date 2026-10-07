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
const GATEWAY_EVIDENCE_CONFIG_NAME: &str = "gateway-evidence";

fn registry_digest(registry: &light_runtime::ModuleRegistry) -> Result<String, serde_json::Error> {
    let mut summaries = serde_json::to_value(registry.module_summaries())?;
    // Pingora registers action-gateway again while preparing its TLS listener.
    // An identical registration changes loadedAt, not effective configuration.
    // Retain lastReload and all effective flags so actual reloads invalidate proof.
    for summary in summaries.as_array_mut().expect("module summaries are an array") {
        summary.as_object_mut().expect("module summary is an object").remove("loadedAt");
    }
    Ok(gateway_operational_store::sha256_digest(&serde_json::to_string(&(
        registry.component_configs(), summaries,
    ))?))
}

#[cfg(test)]
mod registry_identity_tests {
    use super::*;
    #[test]
    fn dispatch_observer_configuration_requires_no_release_image_digest() {
        let config: GatewayEvidenceConfig = serde_json::from_value(serde_json::json!({
            "enabled": true, "dispatchObservationEnabled": true, "contractVersion": 2,
            "databaseUrlFile": "/run/secrets/database-url",
            "bindingId": "11111111-1111-7111-8111-111111111111",
            "bindingDigest": format!("sha256:{}", "b".repeat(64)),
            "hostId": "22222222-2222-7222-8222-222222222222",
            "environment": "test", "serverHost": "postgres", "port": 5432,
            "tlsMode": "DISABLE", "serviceOwner": "light-gateway", "schema": "gateway_ops",
            "expectedDatabase": "operations", "minimumSchemaGeneration": 2,
            "credentialGeneration": 1, "gatewayInstance": "test",
            "maximumPendingRecords": 8192, "maximumPendingBytes": 67108864,
            "sinkEndpoint": "stdout://collector", "publisherBatchRecords": 128,
            "publisherPollMs": 250, "publisherRetryMs": 1000,
            "publisherLeaseSeconds": 30, "deliveredRetentionSeconds": 3600
        })).unwrap();
        validate_config(&config).unwrap();
        assert!(serde_json::to_value(config).unwrap().get("deploymentImageDigest").is_none());
    }
    #[test]
    fn repeated_listener_registration_preserves_identity_but_configuration_changes_do_not() {
        let registry = light_runtime::ModuleRegistry::default();
        let register = |value, enabled| registry.register_config(
            "action-gateway", "action-gateway", ModuleKind::Framework,
            serde_json::json!({"enabled": value}), [], true, Some(enabled), false,
        );
        register(true, true);
        let original = registry_digest(&registry).unwrap();
        register(true, true);
        assert_eq!(original, registry_digest(&registry).unwrap());
        register(false, true);
        assert_ne!(original, registry_digest(&registry).unwrap());
        register(true, false);
        assert_ne!(original, registry_digest(&registry).unwrap());
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GatewayEvidenceConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    dispatch_observation_enabled: bool,
    contract_version: u16,
    database_url_file: String,
    binding_id: Uuid,
    binding_digest: String,
    host_id: Uuid,
    environment: String,
    server_host: String,
    port: u16,
    tls_mode: String,
    service_owner: String,
    schema: String,
    expected_database: String,
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
    pub dispatch_identity: Option<DispatchIdentity>,
    dispatch_validated: OnceCell<()>,
    configuration_registry: Arc<light_runtime::ModuleRegistry>,
}

#[derive(Serialize)]
pub struct DispatchIdentity {
    pub digest: String,
    process_id: Uuid,
    binary_digest: String,
    configuration_digest: String,
    #[serde(skip)]
    registry_digest: String,
}

#[derive(Clone)]
struct ExpectedBindingOwned {
    binding_id: Uuid,
    binding_digest: String,
    host_id: Uuid,
    environment: String,
    server_host: String,
    port: u16,
    tls_mode: String,
    expected_database: String,
    minimum_schema_generation: i64,
}

impl GatewayEvidenceRuntime {
    pub fn configuration_current(&self) -> bool {
        self.dispatch_identity.as_ref().is_some_and(|identity| {
            registry_digest(&self.configuration_registry)
                .is_ok_and(|digest| digest == identity.registry_digest)
        })
    }
    pub async fn record_dispatch(
        &self,
        record: &gateway_operational_store::EvidenceRecord,
    ) -> Result<(), StoreError> {
        self.ensure_validated().await?;
        let identity = self
            .dispatch_identity
            .as_ref()
            .ok_or_else(|| StoreError::Scope("dispatch observation is disabled".into()))?;
        self.dispatch_validated.get_or_try_init(|| async {
            let count: i64=sqlx::query_scalar("SELECT count(*) FROM operational_meta.operational_schema_migration_t WHERE migration_owner='gateway-operational-store' AND schema_name='gateway_ops' AND migration_id=$1")
                .bind(gateway_operational_store::DISPATCH_MIGRATION_ID).fetch_one(self.repository.pool()).await?;
            if count != 1 { return Err(StoreError::Scope("dispatch observation migration is not installed".into())); }
            let count: i64=sqlx::query_scalar("SELECT count(*) FROM operational_meta.operational_schema_migration_t WHERE migration_owner='gateway-operational-store' AND schema_name='gateway_ops' AND migration_id='0003_gateway_dispatch_image_optional'")
                .fetch_one(self.repository.pool()).await?;
            if count != 1 { return Err(StoreError::Scope("dispatch image-optional migration is not installed".into())); }
            sqlx::query("INSERT INTO gateway_ops.gateway_dispatch_identity_t(deployment_config_digest,process_id,binary_digest,configuration_digest,observer_contract_version) VALUES($1,$2,$3,$4,1)")
                .bind(&identity.digest).bind(identity.process_id).bind(&identity.binary_digest).bind(&identity.configuration_digest)
                .execute(self.repository.pool()).await?;
            Ok(())
        }).await?;
        match self.repository.record(record).await? {
            gateway_operational_store::AdmissionOutcome::Persisted => Ok(()),
            gateway_operational_store::AdmissionOutcome::DroppedOptional => Err(StoreError::Scope(
                "required dispatch observation was dropped".into(),
            )),
        }
    }
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
                        server_host: &self.expected_binding.server_host,
                        port: self.expected_binding.port,
                        tls_mode: &self.expected_binding.tls_mode,
                        expected_database: &self.expected_binding.expected_database,
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
    let config = match runtime_config
        .module_registry
        .load_config::<GatewayEvidenceConfig>(runtime_config, GATEWAY_EVIDENCE_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == GATEWAY_EVIDENCE_FILE => return Ok(None),
        Err(error) => return Err(error),
    };
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
        if config.dispatch_observation_enabled {
            return Err(RuntimeError::Config(
                "dispatch observation requires enabled durable Gateway evidence".into(),
            ));
        }
        return Ok(None);
    }
    validate_config(&config)?;
    let database_url = read_database_url(
        Path::new(&config.database_url_file),
        &config.server_host,
        config.port,
        &config.tls_mode,
        &config.expected_database,
    )
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
    let dispatch_identity = if config.dispatch_observation_enabled {
        use sha2::{Digest, Sha256};
        let process_id = Uuid::now_v7();
        let binary = std::fs::read(std::env::current_exe().map_err(|_| {
            RuntimeError::Config("cannot locate dispatch observer executable".into())
        })?)
        .map_err(|_| RuntimeError::Config("cannot hash dispatch observer executable".into()))?;
        let binary_digest = format!("sha256:{:x}", Sha256::digest(binary));
        let ordered: std::collections::BTreeMap<_, _> =
            runtime_config.resolved_values.iter().collect();
        let components = runtime_config.module_registry.component_configs();
        let registry_digest = registry_digest(&runtime_config.module_registry)
            .map_err(|_| RuntimeError::Config("cannot hash observer components".into()))?;
        let configuration_digest = gateway_operational_store::sha256_digest(
            &serde_json::to_string(&(ordered, components))
                .map_err(|_| RuntimeError::Config("cannot hash observer configuration".into()))?,
        );
        let digest = gateway_operational_store::sha256_digest(&format!(
            "{process_id}|{binary_digest}|{configuration_digest}|1"
        ));
        Some(DispatchIdentity {
            digest,
            process_id,
            binary_digest,
            configuration_digest,
            registry_digest,
        })
    } else {
        None
    };
    let runtime = Arc::new(GatewayEvidenceRuntime {
        repository,
        expected_binding: ExpectedBindingOwned {
            binding_id: config.binding_id,
            binding_digest: config.binding_digest.clone(),
            host_id: config.host_id,
            environment: config.environment.clone(),
            server_host: config.server_host.clone(),
            port: config.port,
            tls_mode: config.tls_mode.clone(),
            expected_database: config.expected_database.clone(),
            minimum_schema_generation: config.minimum_schema_generation,
        },
        validated: OnceCell::new(),
        dispatch_identity,
        dispatch_validated: OnceCell::new(),
        configuration_registry: Arc::clone(&runtime_config.module_registry),
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
        || config.contract_version != 2
        || config.host_id.is_nil()
        || config.environment.trim().is_empty()
        || !operational_store::runtime::postgres_identifier(&config.expected_database)
        || config.service_owner != "light-gateway"
        || config.schema != gateway_operational_store::EXPECTED_SCHEMA
        || config.gateway_instance.trim().is_empty()
        || config.minimum_schema_generation < 1
        || config.server_host.trim().is_empty()
        || config.port == 0
        || !matches!(
            config.tls_mode.as_str(),
            "DISABLE" | "PREFER" | "REQUIRE" | "VERIFY_CA" | "VERIFY_FULL"
        )
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
            "invalid enabled gateway-evidence projection".into(),
        ));
    }
    Ok(())
}
