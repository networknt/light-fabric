use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::error::LlmGatewayError;

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTrustBundle {
    pub pem: Vec<u8>,
    pub sha256: String,
}

/// Resolves only non-secret CA material for the central provider client. This
/// API deliberately has no sidecar-runtime credential purpose.
pub trait TrustBundleResolver: Send + Sync {
    fn resolve(&self, trust_bundle_ref: &str) -> Result<ResolvedTrustBundle, LlmGatewayError>;
}

#[derive(Debug, Clone)]
pub struct FileTrustBundleResolver {
    references: BTreeMap<String, PathBuf>,
    max_bundle_bytes: usize,
}

impl FileTrustBundleResolver {
    pub fn new(references: BTreeMap<String, String>, max_bundle_bytes: usize) -> Self {
        Self {
            references: references
                .into_iter()
                .map(|(reference, path)| (reference, PathBuf::from(path)))
                .collect(),
            max_bundle_bytes: max_bundle_bytes.max(1),
        }
    }
}

impl TrustBundleResolver for FileTrustBundleResolver {
    fn resolve(&self, trust_bundle_ref: &str) -> Result<ResolvedTrustBundle, LlmGatewayError> {
        let path = self.references.get(trust_bundle_ref).ok_or_else(|| {
            LlmGatewayError::Config("trust bundle reference is not authorized".to_string())
        })?;
        let metadata = fs::metadata(path).map_err(|_| {
            LlmGatewayError::Config("trust bundle could not be materialized".to_string())
        })?;
        if !metadata.is_file() || metadata.len() > self.max_bundle_bytes as u64 {
            return Err(LlmGatewayError::Config(
                "trust bundle is not a bounded regular file".to_string(),
            ));
        }
        let pem = fs::read(path).map_err(|_| {
            LlmGatewayError::Config("trust bundle could not be materialized".to_string())
        })?;
        let sha256 = format!("{:x}", Sha256::digest(&pem));
        Ok(ResolvedTrustBundle { pem, sha256 })
    }
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
