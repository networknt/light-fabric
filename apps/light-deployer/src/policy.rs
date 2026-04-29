use crate::config::DeployerConfig;
use crate::model::{DeploymentRequest, ResourceSummary};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("action is not allowed: {0:?}")]
    ActionDenied(crate::model::DeploymentAction),
    #[error("namespace is not allowed: {0}")]
    NamespaceDenied(String),
    #[error("repository is not allowed: {0}")]
    RepositoryDenied(String),
    #[error("resource kind is blocked: {0}")]
    KindBlocked(String),
    #[error("resource kind is not allowed: {0}")]
    KindDenied(String),
    #[error("prune requires override: {message}")]
    RequiresOverride { message: String },
}

#[derive(Clone)]
pub struct Policy {
    config: DeployerConfig,
}

impl Policy {
    pub fn new(config: DeployerConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &DeployerConfig {
        &self.config
    }

    pub fn validate_request(&self, request: &DeploymentRequest) -> Result<(), PolicyError> {
        if !self.config.allowed_actions.contains(&request.action) {
            return Err(PolicyError::ActionDenied(request.action.clone()));
        }

        if !self.config.allowed_namespaces.is_empty()
            && !self.config.allowed_namespaces.contains(&request.namespace)
        {
            return Err(PolicyError::NamespaceDenied(request.namespace.clone()));
        }

        if !self.config.allowed_repo_prefixes.is_empty()
            && !self
                .config
                .allowed_repo_prefixes
                .iter()
                .any(|prefix| request.template.repo_url.starts_with(prefix))
        {
            return Err(PolicyError::RepositoryDenied(
                request.template.repo_url.clone(),
            ));
        }

        Ok(())
    }

    pub fn validate_resource_kind(&self, kind: &str) -> Result<(), PolicyError> {
        if self.config.blocked_kinds.contains(kind) {
            return Err(PolicyError::KindBlocked(kind.to_string()));
        }
        if !self.config.allowed_kinds.contains(kind) {
            return Err(PolicyError::KindDenied(kind.to_string()));
        }
        Ok(())
    }

    pub fn validate_prune(
        &self,
        current_count: usize,
        pruned: &[ResourceSummary],
        override_requested: bool,
    ) -> Result<(), PolicyError> {
        if pruned.is_empty() || !self.config.prune.enabled {
            return Ok(());
        }

        let has_sensitive_kind = pruned
            .iter()
            .any(|resource| self.config.prune.sensitive_kinds.contains(&resource.kind));
        let delete_percent = if current_count == 0 {
            0
        } else {
            ((pruned.len() * 100) / current_count) as u8
        };

        let over_limit = delete_percent > self.config.prune.max_delete_percent;
        if self.config.prune.override_required
            && !override_requested
            && (has_sensitive_kind || over_limit)
        {
            return Err(PolicyError::RequiresOverride {
                message: format!(
                    "prune would delete {} of {} tracked resources ({}%)",
                    pruned.len(),
                    current_count,
                    delete_percent
                ),
            });
        }

        Ok(())
    }
}
