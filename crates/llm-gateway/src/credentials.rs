use std::collections::BTreeMap;

use crate::error::LlmGatewayError;

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError>;
}

#[derive(Debug, Clone, Default)]
pub struct MapSecretResolver(pub BTreeMap<String, String>);

impl SecretResolver for MapSecretResolver {
    fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError> {
        self.0
            .get(secret_ref)
            .cloned()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LlmGatewayError::Config("provider credential could not be materialized".to_string())
            })
    }
}

#[derive(Debug, Clone, Default)]
pub struct EnvironmentSecretResolver;

impl SecretResolver for EnvironmentSecretResolver {
    fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError> {
        let name = secret_ref.strip_prefix("env:").ok_or_else(|| {
            LlmGatewayError::Config("production secret references must use env:<NAME>".to_string())
        })?;
        std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LlmGatewayError::Config("provider credential could not be materialized".to_string())
            })
    }
}

#[derive(Debug, Clone, Default)]
pub struct EnvironmentReferenceSecretResolver {
    references: BTreeMap<String, String>,
}

impl EnvironmentReferenceSecretResolver {
    pub fn new(references: BTreeMap<String, String>) -> Self {
        Self { references }
    }
}

impl SecretResolver for EnvironmentReferenceSecretResolver {
    fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError> {
        if secret_ref.starts_with("env:") {
            return EnvironmentSecretResolver.resolve(secret_ref);
        }
        let environment_name = self.references.get(secret_ref).ok_or_else(|| {
            LlmGatewayError::Config("provider credential reference is not authorized".to_string())
        })?;
        std::env::var(environment_name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                LlmGatewayError::Config("provider credential could not be materialized".to_string())
            })
    }
}
