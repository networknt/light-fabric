use crate::events::EventHub;
use crate::git::TemplateSource;
use crate::kube::KubeExecutor;
use crate::model::{
    DeploymentAction, DeploymentError, DeploymentEvent, DeploymentRequest, DeploymentResponse,
    DeploymentStatus, DiffSummary, ResourceAction, ResourceSummary,
};
use crate::policy::{Policy, PolicyError};
use crate::prune::calculate_pruned;
use crate::renderer::{AstRenderer, Renderer, add_management_labels, redacted_unified_diff};
use chrono::Utc;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DeployerError {
    #[error(transparent)]
    Policy(#[from] PolicyError),
    #[error(transparent)]
    Render(#[from] crate::renderer::RenderError),
    #[error(transparent)]
    Template(#[from] crate::git::TemplateSourceError),
    #[error(transparent)]
    Kube(#[from] crate::kube::KubeExecutorError),
}

#[derive(Clone)]
pub struct DeployerService {
    policy: Arc<Policy>,
    template_source: Arc<dyn TemplateSource>,
    kube: Arc<dyn KubeExecutor>,
    renderer: Arc<dyn Renderer>,
    events: EventHub,
}

impl DeployerService {
    pub fn new(
        policy: Policy,
        template_source: Arc<dyn TemplateSource>,
        kube: Arc<dyn KubeExecutor>,
        events: EventHub,
    ) -> Self {
        Self {
            policy: Arc::new(policy),
            template_source,
            kube,
            renderer: Arc::new(AstRenderer),
            events,
        }
    }

    pub fn events(&self) -> EventHub {
        self.events.clone()
    }

    pub async fn execute(&self, request: DeploymentRequest) -> DeploymentResponse {
        let request_id = request
            .request_id
            .clone()
            .unwrap_or_else(|| Uuid::now_v7().to_string());

        match self.execute_inner(request_id.clone(), request).await {
            Ok(response) => response,
            Err(error) => self.error_response(request_id, error),
        }
    }

    async fn execute_inner(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<DeploymentResponse, DeployerError> {
        self.policy.validate_request(&request)?;

        match request.action {
            DeploymentAction::Render => self.render(request_id, request).await,
            DeploymentAction::DryRun => self.dry_run(request_id, request).await,
            DeploymentAction::Diff => self.diff(request_id, request).await,
            DeploymentAction::Deploy => {
                self.accept_and_run(request_id, request, DeploymentStatus::Applying)
                    .await
            }
            DeploymentAction::Undeploy => {
                self.accept_and_run(request_id, request, DeploymentStatus::Deleted)
                    .await
            }
            DeploymentAction::Status => self.status(request_id, request).await,
            DeploymentAction::Rollback => {
                self.accept_and_run(request_id, request, DeploymentStatus::Applying)
                    .await
            }
        }
    }

    async fn render(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<DeploymentResponse, DeployerError> {
        let prepared = self.prepare(&request_id, &request).await?;
        Ok(self.response(
            request_id,
            request,
            DeploymentStatus::Rendered,
            Some(prepared.rendered.manifest_hash),
            prepared.commit_sha,
            prepared.resources,
            None,
            None,
        ))
    }

    async fn dry_run(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<DeploymentResponse, DeployerError> {
        let prepared = self.prepare(&request_id, &request).await?;
        let resources = self.kube.dry_run(&prepared.rendered).await?;
        Ok(self.response(
            request_id,
            request,
            DeploymentStatus::Validated,
            Some(prepared.rendered.manifest_hash),
            prepared.commit_sha,
            resources,
            None,
            None,
        ))
    }

    async fn diff(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<DeploymentResponse, DeployerError> {
        let prepared = self.prepare(&request_id, &request).await?;
        let current = self
            .kube
            .current_managed_resources(&request.namespace, &request.instance_id)
            .await?;
        let pruned = calculate_pruned(&current, &prepared.resources);
        self.policy
            .validate_prune(current.len(), &pruned, request.options.prune_override)?;

        let current_set: BTreeSet<_> = current.iter().map(ResourceSummary::identity).collect();
        let mut resources = prepared
            .resources
            .iter()
            .map(|resource| ResourceSummary {
                action: if current_set.contains(&resource.identity()) {
                    ResourceAction::Modified
                } else {
                    ResourceAction::Added
                },
                ..resource.clone()
            })
            .collect::<Vec<_>>();
        resources.extend(pruned);
        let diff = DiffSummary {
            added: resources
                .iter()
                .filter(|resource| resource.action == ResourceAction::Added)
                .count(),
            modified: resources
                .iter()
                .filter(|resource| resource.action == ResourceAction::Modified)
                .count(),
            deleted: resources
                .iter()
                .filter(|resource| {
                    matches!(
                        resource.action,
                        ResourceAction::Deleted | ResourceAction::Pruned
                    )
                })
                .count(),
            unified_diff: redacted_unified_diff(
                &[],
                &prepared
                    .rendered
                    .documents
                    .iter()
                    .map(|document| document.redacted.clone())
                    .collect::<Vec<_>>(),
            ),
            resources: resources.clone(),
        };

        Ok(self.response(
            request_id,
            request,
            DeploymentStatus::Validated,
            Some(prepared.rendered.manifest_hash),
            prepared.commit_sha,
            resources,
            Some(diff),
            None,
        ))
    }

    async fn accept_and_run(
        &self,
        request_id: String,
        request: DeploymentRequest,
        accepted_status: DeploymentStatus,
    ) -> Result<DeploymentResponse, DeployerError> {
        let service = self.clone();
        let background_request = request.clone();
        let background_request_id = request_id.clone();
        let failure_request_id = request_id.clone();
        tokio::spawn(async move {
            if let Err(error) = service
                .run_background(background_request_id, background_request)
                .await
            {
                let message = error.to_string();
                tracing::error!(
                    request_id = %failure_request_id,
                    error = %message,
                    "deployment background task failed"
                );
                service
                    .events
                    .publish(DeploymentEvent {
                        request_id: failure_request_id,
                        timestamp: Utc::now(),
                        status: DeploymentStatus::Failed,
                        message,
                        resource: None,
                    })
                    .await;
            }
        });

        self.events
            .publish(DeploymentEvent {
                request_id: request_id.clone(),
                timestamp: Utc::now(),
                status: accepted_status.clone(),
                message: "deployment request accepted".to_string(),
                resource: None,
            })
            .await;

        Ok(self.response(
            request_id,
            request,
            DeploymentStatus::Accepted,
            None,
            None,
            Vec::new(),
            None,
            None,
        ))
    }

    async fn run_background(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<(), DeployerError> {
        let prepared = self.prepare(&request_id, &request).await?;
        let current = self
            .kube
            .current_managed_resources(&request.namespace, &request.instance_id)
            .await?;
        let pruned = calculate_pruned(&current, &prepared.resources);
        self.policy
            .validate_prune(current.len(), &pruned, request.options.prune_override)?;

        let resources = match request.action {
            DeploymentAction::Undeploy => self.kube.delete(&current).await?,
            _ => self.kube.apply(&prepared.rendered, &pruned).await?,
        };

        for resource in &resources {
            self.events
                .publish(DeploymentEvent {
                    request_id: request_id.clone(),
                    timestamp: Utc::now(),
                    status: DeploymentStatus::Applying,
                    message: format!("{:?} {}/{}", resource.action, resource.kind, resource.name),
                    resource: Some(resource.identity()),
                })
                .await;
        }

        self.events
            .publish(DeploymentEvent {
                request_id,
                timestamp: Utc::now(),
                status: match request.action {
                    DeploymentAction::Undeploy => DeploymentStatus::Deleted,
                    DeploymentAction::Rollback => DeploymentStatus::RolledBack,
                    _ => DeploymentStatus::Deployed,
                },
                message: format!("processed {} resources", resources.len()),
                resource: None,
            })
            .await;

        Ok(())
    }

    async fn status(
        &self,
        request_id: String,
        request: DeploymentRequest,
    ) -> Result<DeploymentResponse, DeployerError> {
        let events = self.events.history_for(&request_id).await;
        Ok(self.response(
            request_id,
            request,
            DeploymentStatus::Validated,
            None,
            None,
            Vec::new(),
            None,
            Some(events),
        ))
    }

    async fn prepare(
        &self,
        request_id: &str,
        request: &DeploymentRequest,
    ) -> Result<PreparedDeployment, DeployerError> {
        let bundle = self.template_source.fetch(&request.template).await?;
        let values = request.values.clone().unwrap_or_else(|| json!({}));
        let mut rendered = self
            .renderer
            .render(&bundle.documents, &values, &request.namespace)?;

        for document in &mut rendered.documents {
            add_management_labels(
                &mut document.value,
                &request.host_id,
                &request.instance_id,
                request_id,
            );
            document.summary =
                crate::renderer::summarize_resource(&document.value, ResourceAction::Unchanged)?;
            document.redacted = crate::renderer::redact_yaml(&document.value);
            self.policy.validate_resource_kind(&document.summary.kind)?;
        }

        let resources = rendered
            .documents
            .iter()
            .map(|document| ResourceSummary {
                action: ResourceAction::Modified,
                ..document.summary.clone()
            })
            .collect();

        Ok(PreparedDeployment {
            rendered,
            commit_sha: bundle.commit_sha,
            resources,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn response(
        &self,
        request_id: String,
        request: DeploymentRequest,
        status: DeploymentStatus,
        manifest_hash: Option<String>,
        template_commit_sha: Option<String>,
        resources: Vec<ResourceSummary>,
        diff: Option<DiffSummary>,
        events: Option<Vec<DeploymentEvent>>,
    ) -> DeploymentResponse {
        DeploymentResponse {
            request_id,
            action: request.action,
            status,
            deployer_id: self.policy.config().deployer_id.clone(),
            cluster_id: self.policy.config().cluster_id.clone(),
            namespace: request.namespace,
            manifest_hash,
            values_hash: request.values_hash,
            values_snapshot_id: request.values_snapshot_id,
            runtime_values_hash: request.runtime_values_hash,
            runtime_values_snapshot_id: request.runtime_values_snapshot_id,
            template_commit_sha,
            resources,
            diff,
            artifact_ref: None,
            events: events.unwrap_or_default(),
            error: None,
        }
    }

    fn error_response(&self, request_id: String, error: DeployerError) -> DeploymentResponse {
        let (code, message) = match &error {
            DeployerError::Policy(PolicyError::RequiresOverride { message }) => {
                ("requiresOverride".to_string(), message.clone())
            }
            DeployerError::Policy(_) => ("policyDenied".to_string(), error.to_string()),
            DeployerError::Render(_) => ("renderFailed".to_string(), error.to_string()),
            DeployerError::Template(_) => ("templateFetchFailed".to_string(), error.to_string()),
            DeployerError::Kube(_) => ("kubernetesFailed".to_string(), error.to_string()),
        };

        let status = if code == "requiresOverride" {
            DeploymentStatus::RequiresOverride
        } else {
            DeploymentStatus::Failed
        };

        DeploymentResponse {
            request_id,
            action: DeploymentAction::Status,
            status,
            deployer_id: self.policy.config().deployer_id.clone(),
            cluster_id: self.policy.config().cluster_id.clone(),
            namespace: String::new(),
            manifest_hash: None,
            values_hash: None,
            values_snapshot_id: None,
            runtime_values_hash: None,
            runtime_values_snapshot_id: None,
            template_commit_sha: None,
            resources: Vec::new(),
            diff: None,
            artifact_ref: None,
            events: Vec::new(),
            error: Some(DeploymentError {
                code,
                message,
                details: BTreeMap::new(),
            }),
        }
    }
}

struct PreparedDeployment {
    rendered: crate::renderer::RenderedManifestSet,
    commit_sha: Option<String>,
    resources: Vec<ResourceSummary>,
}
