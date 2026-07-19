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
                LlmGatewayError::Config(format!("unresolved secret reference `{secret_ref}`"))
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
                LlmGatewayError::Config(format!("unresolved secret reference `{secret_ref}`"))
            })
    }
}
