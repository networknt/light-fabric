use crate::model::{ResourceAction, ResourceSummary};
use crate::renderer::RenderedManifestSet;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use kube::{Api, Client, ResourceExt};
use serde_json::Value as JsonValue;
use std::fmt;

const FIELD_MANAGER: &str = "light-deployer";

#[derive(Debug, Clone)]
pub struct KubeExecutorError {
    message: String,
}

impl KubeExecutorError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for KubeExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for KubeExecutorError {}

impl From<kube::Error> for KubeExecutorError {
    fn from(error: kube::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_json::Error> for KubeExecutorError {
    fn from(error: serde_json::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl From<serde_yaml::Error> for KubeExecutorError {
    fn from(error: serde_yaml::Error) -> Self {
        Self::new(error.to_string())
    }
}

#[async_trait::async_trait]
pub trait KubeExecutor: Send + Sync {
    async fn current_managed_resources(
        &self,
        namespace: &str,
        instance_id: &str,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError>;

    async fn dry_run(
        &self,
        rendered: &RenderedManifestSet,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError>;

    async fn apply(
        &self,
        rendered: &RenderedManifestSet,
        pruned: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError>;

    async fn delete(
        &self,
        resources: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError>;
}

#[derive(Debug, Clone, Default)]
pub struct NoopKubeExecutor;

#[async_trait::async_trait]
impl KubeExecutor for NoopKubeExecutor {
    async fn current_managed_resources(
        &self,
        _namespace: &str,
        _instance_id: &str,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        Ok(Vec::new())
    }

    async fn dry_run(
        &self,
        rendered: &RenderedManifestSet,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        Ok(rendered
            .documents
            .iter()
            .map(|document| ResourceSummary {
                action: ResourceAction::Unchanged,
                ..document.summary.clone()
            })
            .collect())
    }

    async fn apply(
        &self,
        rendered: &RenderedManifestSet,
        pruned: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        let mut resources: Vec<ResourceSummary> = rendered
            .documents
            .iter()
            .map(|document| ResourceSummary {
                action: ResourceAction::Modified,
                ..document.summary.clone()
            })
            .collect();
        resources.extend_from_slice(pruned);
        Ok(resources)
    }

    async fn delete(
        &self,
        resources: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        Ok(resources
            .iter()
            .map(|resource| ResourceSummary {
                action: ResourceAction::Deleted,
                ..resource.clone()
            })
            .collect())
    }
}

#[derive(Clone)]
pub struct KubeRsExecutor {
    client: Client,
}

impl KubeRsExecutor {
    pub async fn try_default() -> Result<Self, KubeExecutorError> {
        Ok(Self {
            client: Client::try_default().await?,
        })
    }

    fn api(&self, namespace: &str, resource: &ApiResource) -> Api<DynamicObject> {
        Api::namespaced_with(self.client.clone(), namespace, resource)
    }
}

#[async_trait::async_trait]
impl KubeExecutor for KubeRsExecutor {
    async fn current_managed_resources(
        &self,
        namespace: &str,
        instance_id: &str,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        let selector = format!(
            "app.kubernetes.io/managed-by=light-deployer,lightapi.net/instance-id={instance_id}"
        );
        let params = ListParams::default().labels(&selector);
        let mut resources = Vec::new();

        for resource in supported_api_resources() {
            let api = self.api(namespace, &resource);
            let list = api.list(&params).await?;
            for item in list {
                resources.push(summary_from_dynamic(
                    &resource,
                    &item,
                    ResourceAction::Unchanged,
                ));
            }
        }

        Ok(resources)
    }

    async fn dry_run(
        &self,
        rendered: &RenderedManifestSet,
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        let mut resources = Vec::new();
        for document in &rendered.documents {
            let patch = json_from_yaml(&document.value)?;
            let resource = api_resource_for(&document.summary.api_version, &document.summary.kind)?;
            let api = self.api(&document.summary.namespace, &resource);
            let params = PatchParams::apply(FIELD_MANAGER).force().dry_run();
            api.patch(
                &document.summary.name,
                &params,
                &Patch::Apply(patch.clone()),
            )
            .await?;
            resources.push(ResourceSummary {
                action: ResourceAction::Unchanged,
                ..document.summary.clone()
            });
        }
        Ok(resources)
    }

    async fn apply(
        &self,
        rendered: &RenderedManifestSet,
        pruned: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        let mut resources = Vec::new();
        for document in &rendered.documents {
            let patch = json_from_yaml(&document.value)?;
            let resource = api_resource_for(&document.summary.api_version, &document.summary.kind)?;
            let api = self.api(&document.summary.namespace, &resource);
            let params = PatchParams::apply(FIELD_MANAGER).force();
            api.patch(
                &document.summary.name,
                &params,
                &Patch::Apply(patch.clone()),
            )
            .await?;
            resources.push(ResourceSummary {
                action: ResourceAction::Modified,
                ..document.summary.clone()
            });
        }

        let deleted = self.delete(pruned).await?;
        resources.extend(deleted.into_iter().map(|resource| ResourceSummary {
            action: ResourceAction::Pruned,
            ..resource
        }));
        Ok(resources)
    }

    async fn delete(
        &self,
        resources: &[ResourceSummary],
    ) -> Result<Vec<ResourceSummary>, KubeExecutorError> {
        let mut deleted = Vec::new();
        for resource_summary in resources {
            let resource = api_resource_for(&resource_summary.api_version, &resource_summary.kind)?;
            let api = self.api(&resource_summary.namespace, &resource);
            match api
                .delete(&resource_summary.name, &DeleteParams::default())
                .await
            {
                Ok(_) => deleted.push(ResourceSummary {
                    action: ResourceAction::Deleted,
                    ..resource_summary.clone()
                }),
                Err(kube::Error::Api(error)) if error.code == 404 => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(deleted)
    }
}

fn json_from_yaml(value: &serde_yaml::Value) -> Result<JsonValue, KubeExecutorError> {
    Ok(serde_json::to_value(value)?)
}

fn summary_from_dynamic(
    resource: &ApiResource,
    item: &DynamicObject,
    action: ResourceAction,
) -> ResourceSummary {
    ResourceSummary {
        api_version: resource.api_version.clone(),
        kind: resource.kind.clone(),
        namespace: item.namespace().unwrap_or_default(),
        name: item.name_any(),
        action,
    }
}

fn supported_api_resources() -> Vec<ApiResource> {
    vec![
        api_resource("apps", "v1", "Deployment", "deployments"),
        api_resource("", "v1", "Service", "services"),
        api_resource("networking.k8s.io", "v1", "Ingress", "ingresses"),
        api_resource("", "v1", "ConfigMap", "configmaps"),
        api_resource("", "v1", "Secret", "secrets"),
    ]
}

fn api_resource_for(api_version: &str, kind: &str) -> Result<ApiResource, KubeExecutorError> {
    let (group, version) = split_api_version(api_version);
    let plural = match (group, version, kind) {
        ("apps", "v1", "Deployment") => "deployments",
        ("", "v1", "Service") => "services",
        ("networking.k8s.io", "v1", "Ingress") => "ingresses",
        ("", "v1", "ConfigMap") => "configmaps",
        ("", "v1", "Secret") => "secrets",
        _ => {
            return Err(KubeExecutorError::new(format!(
                "unsupported Kubernetes resource {api_version}/{kind}"
            )));
        }
    };
    Ok(api_resource(group, version, kind, plural))
}

fn api_resource(group: &str, version: &str, kind: &str, plural: &str) -> ApiResource {
    let gvk = GroupVersionKind::gvk(group, version, kind);
    ApiResource::from_gvk_with_plural(&gvk, plural)
}

fn split_api_version(api_version: &str) -> (&str, &str) {
    api_version.split_once('/').unwrap_or(("", api_version))
}
