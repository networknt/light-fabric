use crate::embedded_config;
use light_runtime::{
    AdmissionGate, BoundTransport, ConfigProvenance, ConfigSource, LifecycleRegistrar,
    LightRuntimeBuilder, RuntimeConfig, RuntimeError, ShutdownContext, TransportRuntime,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const CONFIG_MODE_ENV: &str = "LIGHT_WORKFLOW_CONFIG_MODE";
const CACHE_DIR_ENV: &str = "LIGHT_WORKFLOW_CONFIG_CACHE_DIR";
const CONFIG_DIR: &str = "config";
const DEFAULT_CACHE_DIR: &str = "config-cache";
const LKG_FILE: &str = "light-workflow-lkg.json";
const SERVICE_ID: &str = "com.networknt.workflow-1.0.0";

#[derive(Clone, Copy)]
struct ConfigPreparationTransport;

#[async_trait::async_trait]
impl TransportRuntime for ConfigPreparationTransport {
    type Handle = ();

    async fn bind(
        &self,
        _config: &RuntimeConfig,
        _lifecycle: &LifecycleRegistrar,
        _admission: &AdmissionGate,
        _startup_cancel: CancellationToken,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        Err(RuntimeError::Unsupported(
            "configuration preparation does not bind a transport".to_string(),
        ))
    }

    async fn stop(
        &self,
        _handle: &mut Self::Handle,
        _context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct EphemeralDirectory(PathBuf);

impl EphemeralDirectory {
    fn create(parent: &Path, prefix: &str) -> anyhow::Result<Self> {
        let path = parent.join(format!(".{prefix}-{}", Uuid::now_v7()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for EphemeralDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMode {
    Managed,
    Local,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowConfigActivation {
    pub runtime_config: RuntimeConfig,
    pub provenance: ConfigProvenance,
    pub degraded: bool,
    pub cache_age_seconds: Option<u64>,
    pub compatibility_environment: String,
    pub cache_dir: PathBuf,
    /// Keeps the materialized remote/LKG directory alive for as long as the
    /// prepared RuntimeConfig can reload files through external_config_dir.
    pub materialized_config_dir: Option<Arc<EphemeralDirectory>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LastKnownGood {
    config_server_uri: String,
    authority_host: String,
    host_id: String,
    environment: String,
    service_id: String,
    snapshot_id: String,
    portal_config_instance_id: String,
    content_digest: String,
    values_yaml: String,
    validated_at_unix_seconds: i64,
}

pub(crate) async fn prepare_workflow_config() -> anyhow::Result<WorkflowConfigActivation> {
    let invocation_environment = env::var("SERVER_ENVIRONMENT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("SERVER_ENVIRONMENT must be explicitly configured and non-empty")
        })?;
    let mode = parse_mode(env::var(CONFIG_MODE_ENV).ok().as_deref())?;
    let cache_dir = env::var_os(CACHE_DIR_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CACHE_DIR));
    prepare_workflow_config_with(
        mode,
        Path::new(CONFIG_DIR),
        &cache_dir,
        &invocation_environment,
    )
    .await
    .map(|mut activation| {
        activation.compatibility_environment = invocation_environment;
        activation
    })
}

async fn prepare_workflow_config_with(
    mode: ConfigMode,
    config_dir: &Path,
    cache_dir: &Path,
    invocation_environment: &str,
) -> anyhow::Result<WorkflowConfigActivation> {
    if mode == ConfigMode::Local {
        let runtime_config = LightRuntimeBuilder::new(ConfigPreparationTransport)
            .with_embedded_config(embedded_config::FILES)
            .with_config_dir(config_dir)
            .build()
            .prepare_local_config()
            .await?;
        validate_identity(&runtime_config, invocation_environment)?;
        let digest = digest_resolved_values(&runtime_config.resolved_values)?;
        return Ok(WorkflowConfigActivation {
            runtime_config,
            provenance: ConfigProvenance {
                source: ConfigSource::Local,
                host_id: None,
                snapshot_id: None,
                instance_id: None,
                content_digest: digest,
            },
            degraded: false,
            cache_age_seconds: None,
            compatibility_environment: invocation_environment.to_string(),
            cache_dir: cache_dir.to_path_buf(),
            materialized_config_dir: None,
        });
    }

    fs::create_dir_all(cache_dir)?;
    let candidate_dir = EphemeralDirectory::create(cache_dir, "candidate")?;
    let remote_result = LightRuntimeBuilder::new(ConfigPreparationTransport)
        .with_embedded_config(embedded_config::FILES)
        .with_config_dir(config_dir)
        .with_external_config_dir(candidate_dir.path())
        .build()
        .prepare_config_with_provenance()
        .await;

    let activation = match remote_result {
        Ok(prepared) if prepared.provenance.source == ConfigSource::Remote => {
            let validation = (|| {
                validate_identity(&prepared.runtime_config, invocation_environment)?;
                validate_remote_metadata(&prepared.runtime_config, &prepared.provenance)?;
                validate_no_secret_properties(&prepared.runtime_config.resolved_values)
            })();
            if let Err(error) = validation {
                tracing::error!(
                    event = "workflow.config.candidate_rejected",
                    snapshotId = prepared.provenance.snapshot_id.as_deref().unwrap_or("unknown"),
                    digest = %prepared.provenance.content_digest,
                    reasonCode = "CONFIG_IDENTITY_OR_SENSITIVITY_INVALID",
                    propertyPaths = "[]",
                    error = %error,
                    "remote workflow configuration candidate rejected"
                );
                return Err(error);
            }
            let values_yaml = fs::read_to_string(candidate_dir.path().join("values.yml"))?;
            let lkg = LastKnownGood {
                config_server_uri: prepared
                    .runtime_config
                    .bootstrap
                    .config_server_uri
                    .clone()
                    .unwrap_or_default(),
                authority_host: prepared.runtime_config.bootstrap.host.clone(),
                host_id: prepared.provenance.host_id.clone().unwrap(),
                environment: invocation_environment.to_string(),
                service_id: SERVICE_ID.to_string(),
                snapshot_id: prepared.provenance.snapshot_id.clone().unwrap(),
                portal_config_instance_id: prepared.provenance.instance_id.clone().unwrap(),
                content_digest: prepared.provenance.content_digest.clone(),
                values_yaml,
                validated_at_unix_seconds: chrono::Utc::now().timestamp(),
            };
            persist_lkg(cache_dir, &lkg)?;
            WorkflowConfigActivation {
                runtime_config: prepared.runtime_config,
                provenance: prepared.provenance,
                degraded: false,
                cache_age_seconds: None,
                compatibility_environment: invocation_environment.to_string(),
                cache_dir: cache_dir.to_path_buf(),
                materialized_config_dir: Some(Arc::new(candidate_dir)),
            }
        }
        Ok(prepared) => {
            tracing::warn!(
                event = "workflow.config.refresh_failed",
                source = "remote",
                reasonCode = "CONFIG_SERVER_UNAVAILABLE",
                retryable = true,
                "remote workflow configuration unavailable; evaluating last-known-good cache"
            );
            activate_lkg(
                config_dir,
                cache_dir,
                invocation_environment,
                &prepared.runtime_config,
            )
            .await?
        }
        Err(error) => return Err(error.into()),
    };
    Ok(activation)
}

async fn activate_lkg(
    config_dir: &Path,
    cache_dir: &Path,
    invocation_environment: &str,
    bootstrap_config: &RuntimeConfig,
) -> anyhow::Result<WorkflowConfigActivation> {
    let bytes = fs::read(cache_dir.join(LKG_FILE)).map_err(|error| {
        anyhow::anyhow!(
            "managed configuration unavailable and no last-known-good cache exists: {error}"
        )
    })?;
    let lkg: LastKnownGood = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow::anyhow!("last-known-good cache is corrupt: {error}"))?;
    validate_lkg(&lkg, invocation_environment, bootstrap_config)?;
    let materialized = EphemeralDirectory::create(cache_dir, "lkg")?;
    fs::write(materialized.path().join("values.yml"), &lkg.values_yaml)?;
    let runtime_config = LightRuntimeBuilder::new(ConfigPreparationTransport)
        .with_embedded_config(embedded_config::FILES)
        .with_config_dir(config_dir)
        .with_external_config_dir(materialized.path())
        .build()
        .prepare_local_config()
        .await?;
    validate_identity(&runtime_config, invocation_environment)?;
    validate_no_secret_properties(&runtime_config.resolved_values)?;
    Ok(WorkflowConfigActivation {
        runtime_config,
        provenance: ConfigProvenance {
            source: ConfigSource::Cache,
            host_id: Some(lkg.host_id),
            snapshot_id: Some(lkg.snapshot_id),
            instance_id: Some(lkg.portal_config_instance_id),
            content_digest: lkg.content_digest,
        },
        degraded: true,
        cache_age_seconds: Some(
            chrono::Utc::now()
                .timestamp()
                .saturating_sub(lkg.validated_at_unix_seconds)
                .try_into()
                .unwrap_or(u64::MAX),
        ),
        compatibility_environment: invocation_environment.to_string(),
        cache_dir: cache_dir.to_path_buf(),
        materialized_config_dir: Some(Arc::new(materialized)),
    })
}

pub(crate) fn validate_remote_reload(
    runtime_config: &RuntimeConfig,
    provenance: &ConfigProvenance,
    invocation_environment: &str,
) -> anyhow::Result<()> {
    if provenance.source != ConfigSource::Remote {
        anyhow::bail!(
            "explicit workflow refresh requires the current promoted Config Server snapshot"
        );
    }
    validate_identity(runtime_config, invocation_environment)?;
    validate_remote_metadata(runtime_config, provenance)?;
    validate_no_secret_properties(&runtime_config.resolved_values)
}

pub(crate) fn persist_remote_reload(
    runtime_config: &RuntimeConfig,
    provenance: &ConfigProvenance,
    values_yaml: &str,
    invocation_environment: &str,
    cache_dir: &Path,
) -> anyhow::Result<()> {
    let calculated_digest = format!("{:x}", Sha256::digest(values_yaml.as_bytes()));
    anyhow::ensure!(
        calculated_digest == provenance.content_digest,
        "reload candidate values digest mismatch: provenance {}, calculated {}",
        provenance.content_digest,
        calculated_digest
    );
    persist_lkg(
        cache_dir,
        &LastKnownGood {
            config_server_uri: runtime_config
                .bootstrap
                .config_server_uri
                .clone()
                .unwrap_or_default(),
            authority_host: runtime_config.bootstrap.host.clone(),
            host_id: provenance.host_id.clone().unwrap(),
            environment: invocation_environment.to_string(),
            service_id: SERVICE_ID.to_string(),
            snapshot_id: provenance.snapshot_id.clone().unwrap(),
            portal_config_instance_id: provenance.instance_id.clone().unwrap(),
            content_digest: provenance.content_digest.clone(),
            values_yaml: values_yaml.to_string(),
            validated_at_unix_seconds: chrono::Utc::now().timestamp(),
        },
    )
}

fn parse_mode(value: Option<&str>) -> anyhow::Result<ConfigMode> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None | Some("managed") => Ok(ConfigMode::Managed),
        Some("local") => Ok(ConfigMode::Local),
        Some(value) => {
            anyhow::bail!("{CONFIG_MODE_ENV} must be `managed` or `local`, got `{value}`")
        }
    }
}

fn validate_identity(config: &RuntimeConfig, environment: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        config.bootstrap.host.trim().len() > 3,
        "bootstrap host is empty or invalid"
    );
    anyhow::ensure!(
        config.server.service_id == SERVICE_ID,
        "workflow service identity mismatch"
    );
    anyhow::ensure!(
        config.service_identity.service_id == SERVICE_ID,
        "bootstrap service identity mismatch"
    );
    anyhow::ensure!(
        config.server.environment == environment,
        "workflow environment mismatch"
    );
    anyhow::ensure!(
        config.service_identity.env_tag.as_deref() == Some(environment),
        "bootstrap environment tag mismatch"
    );
    Ok(())
}

fn validate_remote_metadata(
    config: &RuntimeConfig,
    provenance: &ConfigProvenance,
) -> anyhow::Result<()> {
    for (name, value) in [
        ("host", provenance.host_id.as_deref()),
        ("snapshot", provenance.snapshot_id.as_deref()),
        ("Portal/config instance", provenance.instance_id.as_deref()),
    ] {
        let value =
            value.ok_or_else(|| anyhow::anyhow!("Config Server omitted {name} identity"))?;
        Uuid::parse_str(value)
            .map_err(|_| anyhow::anyhow!("Config Server returned invalid {name} identity"))?;
    }
    anyhow::ensure!(
        provenance.instance_id.as_deref() == config.bootstrap.instance_id.as_deref(),
        "Config Server returned a snapshot for a different Portal/config instance"
    );
    anyhow::ensure!(
        provenance.content_digest.len() == 64,
        "Config Server returned invalid content digest"
    );
    Ok(())
}

fn validate_lkg(
    lkg: &LastKnownGood,
    environment: &str,
    bootstrap_config: &RuntimeConfig,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        Some(lkg.config_server_uri.as_str())
            == bootstrap_config.bootstrap.config_server_uri.as_deref(),
        "last-known-good Config Server authority mismatch"
    );
    anyhow::ensure!(
        lkg.authority_host == bootstrap_config.bootstrap.host,
        "last-known-good host mismatch"
    );
    anyhow::ensure!(
        lkg.environment == environment,
        "last-known-good environment mismatch"
    );
    anyhow::ensure!(
        lkg.service_id == SERVICE_ID,
        "last-known-good service mismatch"
    );
    anyhow::ensure!(
        Some(lkg.portal_config_instance_id.as_str())
            == bootstrap_config.bootstrap.instance_id.as_deref(),
        "last-known-good Portal/config instance mismatch"
    );
    Uuid::parse_str(&lkg.snapshot_id)?;
    Uuid::parse_str(&lkg.host_id)?;
    Uuid::parse_str(&lkg.portal_config_instance_id)?;
    let digest = format!("{:x}", Sha256::digest(lkg.values_yaml.as_bytes()));
    anyhow::ensure!(
        digest == lkg.content_digest,
        "last-known-good content digest mismatch"
    );
    Ok(())
}

fn validate_no_secret_properties(
    values: &HashMap<String, serde_yaml::Value>,
) -> anyhow::Result<()> {
    for key in values.keys() {
        let normalized = key.to_ascii_lowercase().replace(['-', '_'], "");
        let secret = [
            ".password",
            ".secret",
            ".bearertoken",
            ".apikey",
            ".privatekey",
            ".databaseurl",
            ".authorization",
        ]
        .iter()
        .any(|suffix| normalized.ends_with(suffix));
        anyhow::ensure!(
            !secret,
            "secret-classified property `{key}` is not allowed in managed workflow configuration"
        );
    }
    Ok(())
}

fn persist_lkg(cache_dir: &Path, lkg: &LastKnownGood) -> anyhow::Result<()> {
    let path = cache_dir.join(LKG_FILE);
    let staged = cache_dir.join(format!(".{LKG_FILE}.{}.tmp", Uuid::now_v7()));
    fs::write(&staged, serde_json::to_vec(lkg)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(staged, path)?;
    Ok(())
}

fn digest_resolved_values(values: &HashMap<String, serde_yaml::Value>) -> anyhow::Result<String> {
    let bytes = serde_json::to_vec(&values.iter().collect::<BTreeMap<_, _>>())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn config_server(values: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        config_server_for_instance(values, "01a00000-0000-7000-8000-000000000002").await
    }

    async fn config_server_for_instance(
        values: &'static str,
        instance_id: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let digest = format!("{:x}", Sha256::digest(values.as_bytes()));
        let task = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let count = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                let (status, body, extra) = if request.starts_with("GET /config-server/configs") {
                    (
                        "200 OK",
                        values,
                        format!(
                            "x-light-config-host-id: 01964b05-552a-7c4b-9184-6857e7f3dc5f\r\nx-light-config-snapshot-id: 01a00000-0000-7000-8000-000000000001\r\nx-light-config-instance-id: {instance_id}\r\nx-light-config-content-digest: {digest}\r\n"
                        ),
                    )
                } else {
                    ("404 Not Found", "", String::new())
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/yaml\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        (format!("http://{address}"), task)
    }

    fn write_startup(config_dir: &Path, uri: &str) {
        fs::create_dir_all(config_dir).unwrap();
        fs::write(
            config_dir.join("startup.yml"),
            format!(
                "host: dev.lightapi.net\nserviceId: {SERVICE_ID}\ninstanceId: 01a00000-0000-7000-8000-000000000002\nenvTag: dev\nacceptHeader: application/yaml\ntimeout: 100\nconnectTimeout: 100\nconfigServerUri: {uri}\n"
            ),
        )
        .unwrap();
        fs::write(
            config_dir.join("values.yml"),
            "client.caCertPath: ''\nclient.verifyHostname: false\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn remote_boot_then_lkg_outage_recovery_is_identity_bound() {
        let root = TempDir::new().unwrap();
        let config_dir = root.path().join("config");
        let cache_dir = root.path().join("cache");
        let values = "server.serviceId: com.networknt.workflow-1.0.0\nserver.environment: dev\n";
        let (uri, server) = config_server(values).await;
        write_startup(&config_dir, &uri);

        let remote =
            prepare_workflow_config_with(ConfigMode::Managed, &config_dir, &cache_dir, "dev")
                .await
                .unwrap();
        assert_eq!(remote.provenance.source, ConfigSource::Remote);
        assert!(!remote.degraded);
        let remote_materialized = remote.runtime_config.external_config_dir.clone();
        assert!(remote_materialized.join("values.yml").is_file());
        server.await.unwrap();

        let cached =
            prepare_workflow_config_with(ConfigMode::Managed, &config_dir, &cache_dir, "dev")
                .await
                .unwrap();
        assert_eq!(cached.provenance.source, ConfigSource::Cache);
        assert!(cached.degraded);
        assert_eq!(cached.provenance.snapshot_id, remote.provenance.snapshot_id);
        let cached_materialized = cached.runtime_config.external_config_dir.clone();
        assert!(cached_materialized.join("values.yml").is_file());
        drop(cached);
        assert!(!cached_materialized.exists());
        drop(remote);
        assert!(!remote_materialized.exists());
    }

    #[tokio::test]
    async fn managed_boot_fails_without_remote_or_valid_lkg() {
        let root = TempDir::new().unwrap();
        let config_dir = root.path().join("config");
        let cache_dir = root.path().join("cache");
        write_startup(&config_dir, "http://127.0.0.1:9");
        let error =
            prepare_workflow_config_with(ConfigMode::Managed, &config_dir, &cache_dir, "dev")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("no last-known-good cache"));

        fs::write(cache_dir.join(LKG_FILE), b"{partial").unwrap();
        let error =
            prepare_workflow_config_with(ConfigMode::Managed, &config_dir, &cache_dir, "dev")
                .await
                .unwrap_err();
        assert!(error.to_string().contains("cache is corrupt"));
    }

    #[tokio::test]
    async fn wrong_remote_instance_is_rejected_without_lkg_fallback() {
        let root = TempDir::new().unwrap();
        let config_dir = root.path().join("config");
        let cache_dir = root.path().join("cache");
        let values = "server.serviceId: com.networknt.workflow-1.0.0\nserver.environment: dev\n";
        let (uri, server) =
            config_server_for_instance(values, "01a00000-0000-7000-8000-000000000099").await;
        write_startup(&config_dir, &uri);
        let error =
            prepare_workflow_config_with(ConfigMode::Managed, &config_dir, &cache_dir, "dev")
                .await
                .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different Portal/config instance")
        );
        assert!(!cache_dir.join(LKG_FILE).exists());
        server.await.unwrap();
    }

    #[test]
    fn secret_classified_remote_properties_are_rejected() {
        let values = HashMap::from([
            (
                "server.serviceId".to_string(),
                serde_yaml::Value::String(SERVICE_ID.to_string()),
            ),
            (
                "workflow.databaseUrl".to_string(),
                serde_yaml::Value::String("redacted".to_string()),
            ),
        ]);
        let error = validate_no_secret_properties(&values).unwrap_err();
        assert!(error.to_string().contains("workflow.databaseUrl"));
    }

    #[test]
    fn effective_value_digest_is_independent_of_input_map_order() {
        let first = HashMap::from([
            (
                "workflow.execution.maximumParallelism".to_string(),
                64.into(),
            ),
            (
                "workflow.execution.hostExecutorConcurrency".to_string(),
                8.into(),
            ),
        ]);
        let second = HashMap::from([
            (
                "workflow.execution.hostExecutorConcurrency".to_string(),
                8.into(),
            ),
            (
                "workflow.execution.maximumParallelism".to_string(),
                64.into(),
            ),
        ]);

        assert_eq!(
            digest_resolved_values(&first).unwrap(),
            digest_resolved_values(&second).unwrap()
        );
    }

    #[tokio::test]
    async fn reload_lkg_rejects_source_bytes_that_do_not_match_candidate_provenance() {
        let cache_dir = TempDir::new().unwrap();
        let runtime = LightRuntimeBuilder::new(ConfigPreparationTransport)
            .with_embedded_config(embedded_config::FILES)
            .with_config_dir(cache_dir.path())
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        let mut provenance = ConfigProvenance {
            source: ConfigSource::Remote,
            host_id: Some("host".to_string()),
            snapshot_id: Some("snapshot".to_string()),
            instance_id: Some("instance".to_string()),
            content_digest: format!("{:x}", Sha256::digest(b"candidate-a")),
        };

        let error = persist_remote_reload(
            &runtime,
            &provenance,
            "candidate-b",
            "dev",
            cache_dir.path(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("values digest mismatch"));
        assert!(!cache_dir.path().join(LKG_FILE).exists());

        provenance.content_digest = format!("{:x}", Sha256::digest(b"candidate-b"));
        persist_remote_reload(
            &runtime,
            &provenance,
            "candidate-b",
            "dev",
            cache_dir.path(),
        )
        .unwrap();
        let persisted: LastKnownGood = serde_json::from_slice(
            &fs::read(cache_dir.path().join(LKG_FILE)).expect("read persisted LKG"),
        )
        .unwrap();
        assert_eq!(persisted.values_yaml, "candidate-b");
        assert_eq!(persisted.content_digest, provenance.content_digest);
    }

    #[tokio::test]
    async fn local_mode_is_explicit_and_never_contacts_config_server() {
        let root = TempDir::new().unwrap();
        let activation = prepare_workflow_config_with(
            ConfigMode::Local,
            root.path(),
            &root.path().join("cache"),
            "dev",
        )
        .await
        .unwrap();
        assert_eq!(activation.provenance.source, ConfigSource::Local);
    }
}
