use async_trait::async_trait;
use futures_util::StreamExt;
use object_store::{ObjectStore, PutPayload, aws::AmazonS3Builder, path::Path};
use sha2::{Digest, Sha256};
use std::{env, sync::Arc};

use crate::{
    artifact_publish::ArtifactPublisherStore,
    artifact_retention::{ArtifactObjectStore, ArtifactStoreError},
};

#[derive(Clone)]
pub struct DurableArtifactStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl DurableArtifactStore {
    pub fn from_environment() -> Result<Option<Self>, ArtifactStoreError> {
        let bucket = match env::var("WORKFLOW_ARTIFACT_S3_BUCKET") {
            Ok(value) if !value.is_empty() => value,
            _ => return Ok(None),
        };
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
        if let Ok(endpoint) = env::var("WORKFLOW_ARTIFACT_S3_ENDPOINT") {
            builder = builder.with_endpoint(endpoint);
        }
        if env::var("WORKFLOW_ARTIFACT_S3_ALLOW_HTTP").as_deref() == Ok("true") {
            builder = builder.with_allow_http(true);
        }
        let store = builder.build().map_err(store_error)?;
        let prefix = env::var("WORKFLOW_ARTIFACT_PREFIX")
            .unwrap_or_else(|_| "light-workflow".into())
            .trim_matches('/')
            .to_string();
        if prefix.is_empty()
            || prefix
                .split('/')
                .any(|part| part.is_empty() || part == "..")
        {
            return Err(error("artifact object-store prefix is invalid"));
        }
        Ok(Some(Self {
            store: Arc::new(store),
            prefix,
        }))
    }

    #[cfg(test)]
    pub fn in_memory(prefix: &str) -> Self {
        Self {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix.into(),
        }
    }

    fn staging_path(&self, key: &str) -> Result<Path, ArtifactStoreError> {
        safe_key(key)?;
        Ok(Path::from(format!("{}/staging/{key}", self.prefix)))
    }

    fn durable_path(&self, namespace: &str, digest: &str) -> Result<Path, ArtifactStoreError> {
        safe_namespace(namespace)?;
        let hex = digest
            .strip_prefix("sha256:")
            .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| error("artifact digest is not canonical SHA-256"))?;
        Ok(Path::from(format!(
            "{}/tenants/{namespace}/objects/sha256/{}/{hex}",
            self.prefix,
            &hex[..2]
        )))
    }

    fn reference(path: &Path) -> String {
        format!("object://{path}")
    }

    fn parse_reference(&self, reference: &str) -> Result<Path, ArtifactStoreError> {
        let value = reference
            .strip_prefix("object://")
            .ok_or_else(|| error("artifact reference scheme is not object"))?;
        let path = Path::parse(value).map_err(store_error)?;
        if !path.as_ref().starts_with(&format!("{}/", self.prefix)) {
            return Err(error("artifact reference is outside the configured prefix"));
        }
        Ok(path)
    }

    async fn verify_digest(&self, path: &Path, expected: &str) -> Result<(), ArtifactStoreError> {
        let mut stream = self
            .store
            .get(path)
            .await
            .map_err(store_error)?
            .into_stream();
        let mut hash = Sha256::new();
        while let Some(chunk) = stream.next().await {
            hash.update(chunk.map_err(store_error)?);
        }
        let actual = format!("sha256:{}", hex::encode(hash.finalize()));
        if actual != expected {
            return Err(error(
                "artifact object digest does not match terminal evidence",
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl ArtifactPublisherStore for DurableArtifactStore {
    async fn stage(&self, key: &str, bytes: &[u8]) -> Result<String, ArtifactStoreError> {
        let path = self.staging_path(key)?;
        self.store
            .put(&path, PutPayload::from(bytes.to_vec()))
            .await
            .map_err(store_error)?;
        Ok(Self::reference(&path))
    }

    async fn promote(
        &self,
        namespace: &str,
        staging: &str,
        digest: &str,
    ) -> Result<String, ArtifactStoreError> {
        safe_namespace(namespace)?;
        let source = self.parse_reference(staging)?;
        if !source
            .as_ref()
            .starts_with(&format!("{}/staging/{namespace}/", self.prefix))
        {
            return Err(error(
                "artifact promotion source is outside the tenant staging namespace",
            ));
        }
        let destination = self.durable_path(namespace, digest)?;
        if self.store.head(&destination).await.is_ok() {
            self.verify_digest(&destination, digest).await?;
            return Ok(Self::reference(&destination));
        }
        self.verify_digest(&source, digest).await?;
        self.store
            .copy(&source, &destination)
            .await
            .map_err(store_error)?;
        // The source may have changed between verification and provider copy.
        // Only the destination verification authorizes the metadata binding.
        self.verify_digest(&destination, digest).await?;
        self.store.delete(&source).await.map_err(store_error)?;
        Ok(Self::reference(&destination))
    }
}

#[async_trait]
impl ArtifactObjectStore for DurableArtifactStore {
    async fn delete(&self, reference: &str) -> Result<(), ArtifactStoreError> {
        let path = self.parse_reference(reference)?;
        match self.store.delete(&path).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(error) => Err(store_error(error)),
        }
    }

    async fn exists(&self, reference: &str) -> Result<bool, ArtifactStoreError> {
        let path = self.parse_reference(reference)?;
        match self.store.head(&path).await {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(error) => Err(store_error(error)),
        }
    }
}

fn safe_key(key: &str) -> Result<(), ArtifactStoreError> {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains('\\')
        || key.contains('\0')
        || key.split('/').any(|part| part.is_empty() || part == "..")
    {
        return Err(error("artifact object key is invalid"));
    }
    Ok(())
}

fn safe_namespace(namespace: &str) -> Result<(), ArtifactStoreError> {
    if namespace.is_empty()
        || !namespace
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(error("artifact tenant namespace is invalid"));
    }
    Ok(())
}

fn store_error(error: impl std::fmt::Display) -> ArtifactStoreError {
    ArtifactStoreError {
        message: error.to_string(),
        retryable: true,
    }
}

fn error(message: &str) -> ArtifactStoreError {
    ArtifactStoreError {
        message: message.into(),
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST_A: &str = "00000000-0000-0000-0000-000000000001";
    const HOST_B: &str = "00000000-0000-0000-0000-000000000002";

    #[tokio::test]
    async fn promotion_is_verified_idempotent_and_deletable() {
        let store = DurableArtifactStore::in_memory("tenant-a");
        let bytes = b"trusted artifact";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
        let staged = store
            .stage(&format!("{HOST_A}/execution/artifact"), bytes)
            .await
            .unwrap();

        let durable = store.promote(HOST_A, &staged, &digest).await.unwrap();
        assert!(store.exists(&durable).await.unwrap());
        assert_eq!(
            store.promote(HOST_A, &staged, &digest).await.unwrap(),
            durable
        );

        store.delete(&durable).await.unwrap();
        assert!(!store.exists(&durable).await.unwrap());
        store.delete(&durable).await.unwrap();
    }

    #[tokio::test]
    async fn digest_mismatch_never_creates_a_durable_object() {
        let store = DurableArtifactStore::in_memory("tenant-a");
        let staged = store
            .stage(&format!("{HOST_A}/execution/artifact"), b"different bytes")
            .await
            .unwrap();
        let digest = format!("sha256:{}", "0".repeat(64));

        let error = store.promote(HOST_A, &staged, &digest).await.unwrap_err();

        assert!(error.message.contains("digest does not match"));
        let destination = store.durable_path(HOST_A, &digest).unwrap();
        assert!(matches!(
            store.store.head(&destination).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn identical_content_is_isolated_by_tenant_namespace() {
        let store = DurableArtifactStore::in_memory("root");
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(b"same")));
        let first = store.durable_path(HOST_A, &digest).unwrap();
        let second = store.durable_path(HOST_B, &digest).unwrap();

        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn existing_destination_is_reverified_before_reuse() {
        let store = DurableArtifactStore::in_memory("root");
        let bytes = b"expected";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
        let staged = store
            .stage(&format!("{HOST_A}/execution/artifact"), bytes)
            .await
            .unwrap();
        let destination = store.durable_path(HOST_A, &digest).unwrap();
        store
            .store
            .put(&destination, PutPayload::from(b"corrupt".to_vec()))
            .await
            .unwrap();

        let error = store.promote(HOST_A, &staged, &digest).await.unwrap_err();

        assert!(!error.retryable);
        assert!(error.message.contains("digest does not match"));
    }
}
