use async_trait::async_trait;
use light_runtime::{
    AdmissionGate, ConfigProvenance, ConfigSource, LifecycleParticipant, PortalRegistryClient,
    RegistrationState, RuntimeConfig, RuntimeError, ServiceMetadataUpdate, ShutdownContext,
};
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::configuration::{RunnerExecutionConfig, WorkflowConfiguration, WorkflowRuntimeConfig};
use crate::rule_api::WorkflowHealth;

const TAG_PREFIX: &str = "light.workflow.";

#[derive(Clone)]
pub struct WorkflowOperationalMetadata {
    tags: Arc<RwLock<HashMap<String, String>>>,
    registry_client: Option<Arc<PortalRegistryClient>>,
    health: Arc<RwLock<Option<WorkflowHealth>>>,
}

impl WorkflowOperationalMetadata {
    pub fn new(
        provenance: &ConfigProvenance,
        degraded: bool,
        cache_age_seconds: Option<u64>,
        registry_client: Option<Arc<PortalRegistryClient>>,
    ) -> Self {
        let source = match provenance.source {
            ConfigSource::Remote => "remote",
            ConfigSource::Cache => "cache",
            ConfigSource::Local => "local",
        };
        let last_refresh = chrono::Utc::now()
            - chrono::Duration::seconds(
                i64::try_from(cache_age_seconds.unwrap_or_default()).unwrap_or(i64::MAX),
            );
        let tags = HashMap::from([
            (
                format!("{TAG_PREFIX}config.digest"),
                provenance.content_digest.clone(),
            ),
            (
                format!("{TAG_PREFIX}config.snapshotId"),
                provenance
                    .snapshot_id
                    .clone()
                    .unwrap_or_else(|| "local".into()),
            ),
            (
                format!("{TAG_PREFIX}config.instanceId"),
                provenance
                    .instance_id
                    .clone()
                    .unwrap_or_else(|| "local".into()),
            ),
            (
                format!("{TAG_PREFIX}config.hostId"),
                provenance.host_id.clone().unwrap_or_else(|| "local".into()),
            ),
            (format!("{TAG_PREFIX}config.source"), source.into()),
            (format!("{TAG_PREFIX}config.generation"), "1".into()),
            (
                format!("{TAG_PREFIX}config.lastRefreshUnixSeconds"),
                last_refresh.timestamp().to_string(),
            ),
            (format!("{TAG_PREFIX}degraded"), degraded.to_string()),
            (
                format!("{TAG_PREFIX}degradedReason"),
                if degraded {
                    "CONFIG_SERVER_UNAVAILABLE"
                } else {
                    "none"
                }
                .into(),
            ),
            (format!("{TAG_PREFIX}readiness.state"), "ready".into()),
            (format!("{TAG_PREFIX}readiness.reason"), "ready".into()),
            (
                format!("{TAG_PREFIX}lifecycle.drainState"),
                "accepting".into(),
            ),
            (format!("{TAG_PREFIX}controller.state"), "connecting".into()),
        ]);
        Self {
            tags: Arc::new(RwLock::new(tags)),
            registry_client,
            health: Arc::new(RwLock::new(None)),
        }
    }

    pub fn attach_health(&self, health: WorkflowHealth) {
        *self.health.write().expect("workflow metadata health lock") = Some(health);
    }

    pub fn configure_capacities(
        &self,
        workflow: &WorkflowConfiguration,
        runner: &RunnerExecutionConfig,
    ) {
        let mut tags = self.tags.write().expect("workflow metadata lock");
        tags.insert(
            format!("{TAG_PREFIX}capacity.httpAvailable"),
            workflow.maximum_parallelism.to_string(),
        );
        tags.insert(
            format!("{TAG_PREFIX}capacity.waitListeners"),
            workflow.wait_listener_connections.to_string(),
        );
        tags.insert(
            format!("{TAG_PREFIX}capacity.taskExecutorAvailable"),
            workflow.host_executor_concurrency.to_string(),
        );
        tags.insert(
            format!("{TAG_PREFIX}capacity.runnerSchedulerAvailable"),
            usize::from(runner.enabled).to_string(),
        );
        tags.insert(
            format!("{TAG_PREFIX}capacity.eventProjectionAvailable"),
            "1".into(),
        );
        tags.insert(
            format!("{TAG_PREFIX}runner.originServiceId"),
            runner.origin_service_id.clone(),
        );
        tags.insert(
            format!("{TAG_PREFIX}runner.originId"),
            runner.origin_instance_id.clone(),
        );
    }

    pub fn registration_tags(&self) -> HashMap<String, String> {
        self.tags.read().expect("workflow metadata lock").clone()
    }

    async fn update_and_publish(&self, values: &[(&str, &str)]) -> Result<(), String> {
        {
            let mut tags = self.tags.write().expect("workflow metadata lock");
            for (key, value) in values {
                tags.insert(format!("{TAG_PREFIX}{key}"), (*value).to_string());
            }
        }
        self.publish_current().await
    }

    async fn publish_current(&self) -> Result<(), String> {
        let Some(client) = self.registry_client.as_ref() else {
            return Ok(());
        };
        client
            .send_metadata_update(ServiceMetadataUpdate {
                tags: Some(self.registration_tags()),
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn publish_reload_success(&self, config: &WorkflowRuntimeConfig) {
        {
            let mut tags = self.tags.write().expect("workflow metadata lock");
            tags.insert(
                format!("{TAG_PREFIX}config.digest"),
                config.content_digest.clone(),
            );
            tags.insert(
                format!("{TAG_PREFIX}config.snapshotId"),
                config.snapshot_id.clone().unwrap_or_else(|| "local".into()),
            );
            tags.insert(
                format!("{TAG_PREFIX}config.lastRefreshUnixSeconds"),
                chrono::Utc::now().timestamp().to_string(),
            );
            tags.insert(
                format!("{TAG_PREFIX}config.generation"),
                config.generation.to_string(),
            );
            tags.insert(format!("{TAG_PREFIX}config.source"), "remote".into());
            tags.insert(format!("{TAG_PREFIX}degraded"), "false".into());
            tags.insert(format!("{TAG_PREFIX}degradedReason"), "none".into());
            tags.remove(&format!("{TAG_PREFIX}restartRequiredPaths"));
            tags.remove(&format!("{TAG_PREFIX}config.rejectedSnapshotId"));
            tags.remove(&format!("{TAG_PREFIX}config.rejectedDigest"));
            tags.insert(
                format!("{TAG_PREFIX}capacity.httpAvailable"),
                config.maximum_parallelism.to_string(),
            );
            tags.insert(
                format!("{TAG_PREFIX}capacity.waitListeners"),
                config.wait_listener_connections.to_string(),
            );
            tags.insert(
                format!("{TAG_PREFIX}capacity.taskExecutorAvailable"),
                config.host_executor_concurrency.to_string(),
            );
        }
        if let Err(error) = self.publish_current().await {
            tracing::warn!(%error, "workflow reload metadata will be replayed on registry reconnect");
        }
    }

    pub async fn publish_reload_failure(
        &self,
        reason: &str,
        restart_paths: &[String],
        provenance: Option<&ConfigProvenance>,
    ) {
        {
            let mut tags = self.tags.write().expect("workflow metadata lock");
            tags.insert(format!("{TAG_PREFIX}degraded"), "true".into());
            tags.insert(format!("{TAG_PREFIX}degradedReason"), reason.to_string());
            if restart_paths.is_empty() {
                tags.remove(&format!("{TAG_PREFIX}restartRequiredPaths"));
            } else {
                tags.insert(
                    format!("{TAG_PREFIX}restartRequiredPaths"),
                    restart_paths.join(","),
                );
            }
            if let Some(snapshot_id) = provenance.and_then(|value| value.snapshot_id.as_deref()) {
                tags.insert(
                    format!("{TAG_PREFIX}config.rejectedSnapshotId"),
                    snapshot_id.to_string(),
                );
            } else {
                tags.remove(&format!("{TAG_PREFIX}config.rejectedSnapshotId"));
            }
            if let Some(provenance) = provenance {
                tags.insert(
                    format!("{TAG_PREFIX}config.rejectedDigest"),
                    provenance.content_digest.clone(),
                );
            } else {
                tags.remove(&format!("{TAG_PREFIX}config.rejectedDigest"));
            }
        }
        if let Err(error) = self.publish_current().await {
            tracing::warn!(%error, "workflow rejected-candidate metadata will be replayed on registry reconnect");
        }
    }

    pub async fn mark_unready(&self, reason: &str) {
        if let Err(error) = self
            .update_and_publish(&[
                ("readiness.state", "not-ready"),
                ("readiness.reason", reason),
            ])
            .await
        {
            tracing::warn!(%error, "workflow readiness metadata will be replayed on registry reconnect");
        }
    }

    pub async fn observe_registry(
        &self,
        health: WorkflowHealth,
        cancellation: CancellationToken,
    ) -> Result<(), String> {
        let Some(client) = self.registry_client.as_ref() else {
            health.set_controller_state("disabled");
            self.update_and_publish(&[("controller.state", "disabled")])
                .await?;
            cancellation.cancelled().await;
            return Ok(());
        };
        let mut registration = client.subscribe_registration();
        let mut connection_generation = 0_u64;
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => return Ok(()),
                changed = registration.changed() => {
                    changed.map_err(|_| "controller registration state channel closed".to_string())?;
                    let (state, runtime_instance_id, reason_code) = match &*registration.borrow() {
                        RegistrationState::Registered { runtime_instance_id } => {
                            connection_generation = connection_generation.saturating_add(1);
                            ("connected", runtime_instance_id.to_string(), "REGISTERED")
                        }
                        RegistrationState::Disconnected => {
                            ("disconnected", "unavailable".to_string(), "DISCONNECTED")
                        }
                    };
                    health.set_controller_state(state);
                    tracing::info!(
                        event = "workflow.registry.state_changed",
                        state,
                        controllerRuntimeInstanceId = %runtime_instance_id,
                        connectionGeneration = connection_generation,
                        reasonCode = reason_code,
                        "workflow controller registration state changed"
                    );
                    if let Err(error) = self.update_and_publish(&[("controller.state", state)]).await {
                        tracing::warn!(%error, controller_state = state, "workflow controller metadata will be replayed on reconnect");
                    }
                }
            }
        }
    }
}

#[async_trait]
impl LifecycleParticipant for WorkflowOperationalMetadata {
    fn name(&self) -> &'static str {
        "light-workflow-controller-metadata"
    }

    async fn quiesce(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        if let Some(health) = self
            .health
            .read()
            .expect("workflow metadata health lock")
            .as_ref()
        {
            health.set_drain_state("draining");
        }
        tracing::info!(
            event = "workflow.lifecycle.drain_changed",
            state = "draining",
            reasonCode = "SHUTDOWN",
            deadlineUnixSeconds = chrono::Utc::now().timestamp()
                + i64::try_from(context.remaining().as_secs()).unwrap_or(i64::MAX),
            "workflow lifecycle entered drain"
        );
        if let Err(error) = self
            .update_and_publish(&[
                ("readiness.state", "not-ready"),
                ("readiness.reason", "draining"),
                ("lifecycle.drainState", "draining"),
            ])
            .await
        {
            tracing::warn!(%error, "workflow drain metadata could not be delivered before deregistration");
        }
        Ok(())
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        _context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}

pub struct ManagedWorkflowTask {
    name: &'static str,
    cancellation: CancellationToken,
    handle: Mutex<Option<JoinHandle<Result<(), String>>>>,
}

impl ManagedWorkflowTask {
    pub fn spawn<F, Fut, E>(
        name: &'static str,
        cancellation: CancellationToken,
        admission: AdmissionGate,
        health: WorkflowHealth,
        metadata: Option<WorkflowOperationalMetadata>,
        run: F,
    ) -> Arc<Self>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), E>> + Send + 'static,
        E: std::fmt::Display + Send + 'static,
    {
        let task_cancellation = cancellation.child_token();
        let observed_cancellation = task_cancellation.clone();
        let root_cancellation = cancellation.clone();
        let handle = tokio::spawn(async move {
            let result = run(task_cancellation)
                .await
                .map_err(|error| error.to_string());
            if !observed_cancellation.is_cancelled() {
                let reason = match &result {
                    Ok(()) => format!("{name} exited unexpectedly"),
                    Err(error) => format!("{name} failed: {error}"),
                };
                tracing::error!(task = name, reason = %reason, "workflow lifecycle task stopped");
                let public_reason = format!("critical background task `{name}` is unavailable");
                health.mark_failed(public_reason.clone());
                admission.fail();
                root_cancellation.cancel();
                if let Some(metadata) = metadata {
                    metadata.mark_unready(&public_reason).await;
                }
            }
            result
        });
        Arc::new(Self {
            name,
            cancellation,
            handle: Mutex::new(Some(handle)),
        })
    }
}

#[async_trait]
impl LifecycleParticipant for ManagedWorkflowTask {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn quiesce(
        &self,
        _config: &RuntimeConfig,
        _context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        self.cancellation.cancel();
        Ok(())
    }

    async fn shutdown(
        &self,
        _config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        self.cancellation.cancel();
        let handle = self.handle.lock().expect("workflow task lock").take();
        let Some(mut handle) = handle else {
            return Ok(());
        };
        match tokio::time::timeout(context.remaining(), &mut handle).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(RuntimeError::CleanupFailed(vec![format!(
                "{}: {error}",
                self.name
            )])),
            Ok(Err(error)) => Err(RuntimeError::CleanupFailed(vec![format!(
                "{} join failed: {error}",
                self.name
            )])),
            Err(_) => {
                handle.abort();
                let _ = handle.await;
                Err(RuntimeError::ShutdownDeadlineExceeded(context.remaining()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, ServerConfig, ServiceIdentity,
        ShutdownMode, ShutdownReason,
    };
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::time::Instant;

    fn config() -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: PathBuf::from("config"),
            external_config_dir: PathBuf::from("config"),
            resolved_values: Default::default(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    fn context() -> ShutdownContext {
        ShutdownContext {
            reason: ShutdownReason::Programmatic,
            mode: ShutdownMode::Graceful,
            deadline: Instant::now() + Duration::from_secs(1),
            cancellation: CancellationToken::new(),
        }
    }

    fn metadata() -> WorkflowOperationalMetadata {
        WorkflowOperationalMetadata::new(
            &ConfigProvenance {
                source: ConfigSource::Remote,
                host_id: Some("host-1".into()),
                snapshot_id: Some("snapshot-1".into()),
                instance_id: Some("instance-1".into()),
                content_digest: "digest-1".into(),
            },
            false,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn quiesce_stops_a_managed_task_before_shutdown_joins_it() {
        let root = CancellationToken::new();
        let admission = AdmissionGate::default();
        let health = WorkflowHealth::default();
        let task = ManagedWorkflowTask::spawn(
            "test-task",
            root,
            admission,
            health.clone(),
            None,
            |cancel| async move {
                cancel.cancelled().await;
                Ok::<(), std::convert::Infallible>(())
            },
        );

        task.quiesce(&config(), &context()).await.unwrap();
        task.shutdown(&config(), &context()).await.unwrap();

        assert!(health.is_ready());
    }

    #[tokio::test]
    async fn unexpected_task_failure_marks_readiness_and_cancels_siblings() {
        let root = CancellationToken::new();
        let sibling = root.child_token();
        let admission = AdmissionGate::default();
        admission.open();
        let health = WorkflowHealth::default();
        let metadata = metadata();
        let task = ManagedWorkflowTask::spawn(
            "failed-task",
            root,
            admission.clone(),
            health.clone(),
            Some(metadata.clone()),
            |_| async { Err::<(), _>("boom") },
        );

        tokio::time::timeout(Duration::from_secs(1), sibling.cancelled())
            .await
            .unwrap();

        assert!(!health.is_ready());
        assert!(!admission.is_open());
        assert!(admission.has_failed());
        assert!(!admission.try_open());
        assert!(task.shutdown(&config(), &context()).await.is_err());
        let tags = metadata.registration_tags();
        assert_eq!(tags["light.workflow.readiness.state"], "not-ready");
        assert_eq!(
            tags["light.workflow.readiness.reason"],
            "critical background task `failed-task` is unavailable"
        );
    }

    #[tokio::test]
    async fn metadata_quiesce_publishes_complete_draining_state() {
        let metadata = metadata();

        metadata.quiesce(&config(), &context()).await.unwrap();

        let tags = metadata.registration_tags();
        assert_eq!(tags["light.workflow.config.digest"], "digest-1");
        assert_eq!(tags["light.workflow.readiness.state"], "not-ready");
        assert_eq!(tags["light.workflow.readiness.reason"], "draining");
        assert_eq!(tags["light.workflow.lifecycle.drainState"], "draining");
    }
}
