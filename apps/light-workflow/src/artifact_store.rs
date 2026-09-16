use async_trait::async_trait;
use futures_util::StreamExt;
use object_store::{
    ObjectStore, PutPayload, aws::AmazonS3Builder, local::LocalFileSystem, path::Path,
};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::{
    artifact_publish::ArtifactPublisherStore,
    artifact_retention::{ArtifactObjectStore, ArtifactStoreError},
    configuration::ArtifactSettings,
};

#[derive(Clone)]
pub struct DurableArtifactStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
    filesystem_root: Option<std::path::PathBuf>,
}

#[derive(Clone)]
pub struct DevelopmentArtifactAccess(pub Option<DurableArtifactStore>);

impl DurableArtifactStore {
    /// Admission probes actual backend IO, including a full/read-only volume.
    pub async fn probe_writable(&self) -> Result<(), ArtifactStoreError> {
        let path = self.staging_path(&format!("readiness/{}", uuid::Uuid::now_v7()))?;
        self.store
            .put(&path, PutPayload::from_static(b"ready"))
            .await
            .map_err(store_error)?;
        self.sync_path(&path)?;
        self.store.delete(&path).await.map_err(store_error)?;
        Ok(())
    }
    pub fn from_configuration(
        configuration: &ArtifactSettings,
    ) -> Result<Option<Self>, ArtifactStoreError> {
        let (store, filesystem_root): (Arc<dyn ObjectStore>, _) =
            match configuration.backend.as_str() {
                "disabled" => return Ok(None),
                "filesystem" => {
                    let root = configuration
                        .filesystem_root
                        .as_ref()
                        .ok_or_else(|| error("filesystem artifact root required"))?;
                    if !root.is_absolute() || root == std::path::Path::new("/") {
                        return Err(error(
                            "artifact root must be a dedicated absolute directory",
                        ));
                    }
                    std::fs::create_dir_all(root).map_err(store_error)?;
                    if root.canonicalize().map_err(store_error)? != *root {
                        return Err(error(
                            "artifact root cannot contain symlinks or dot segments",
                        ));
                    }
                    // Probe actual writes/fsync, not permission bits (root/container mappings differ).
                    let probe = root.join(format!(".probe-{}", uuid::Uuid::now_v7()));
                    let file = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&probe)
                        .map_err(store_error)?;
                    use std::io::Write;
                    (&file)
                        .write_all(b"ready")
                        .and_then(|_| file.sync_all())
                        .map_err(store_error)?;
                    std::fs::remove_file(&probe).map_err(store_error)?;
                    std::fs::File::open(root)
                        .and_then(|f| f.sync_all())
                        .map_err(store_error)?;
                    (
                        Arc::new(LocalFileSystem::new_with_prefix(root).map_err(store_error)?),
                        Some(root.clone()),
                    )
                }
                "s3" => {
                    let Some(bucket) = configuration.bucket.as_deref() else {
                        return Ok(None);
                    };
                    let mut builder = AmazonS3Builder::from_env().with_bucket_name(bucket);
                    if let Some(endpoint) = configuration.endpoint.as_deref() {
                        builder = builder.with_endpoint(endpoint);
                    }
                    if configuration.allow_http {
                        builder = builder.with_allow_http(true);
                    }
                    (Arc::new(builder.build().map_err(store_error)?), None)
                }
                _ => return Err(error("unknown artifact backend")),
            };
        let prefix = configuration.prefix.clone();
        if prefix.is_empty()
            || prefix
                .split('/')
                .any(|part| part.is_empty() || part == "..")
        {
            return Err(error("artifact object-store prefix is invalid"));
        }
        Ok(Some(Self {
            store,
            prefix,
            filesystem_root,
        }))
    }

    #[cfg(test)]
    pub fn in_memory(prefix: &str) -> Self {
        Self {
            store: Arc::new(object_store::memory::InMemory::new()),
            prefix: prefix.into(),
            filesystem_root: None,
        }
    }

    fn sync_path(&self, path: &Path) -> Result<(), ArtifactStoreError> {
        if let Some(root) = &self.filesystem_root {
            let file = root.join(path.as_ref());
            std::fs::File::open(&file)
                .and_then(|f| f.sync_all())
                .map_err(store_error)?;
            let mut parent = file.parent();
            while let Some(directory) = parent {
                std::fs::File::open(directory)
                    .and_then(|f| f.sync_all())
                    .map_err(store_error)?;
                if directory == root {
                    break;
                }
                parent = directory.parent();
            }
        }
        Ok(())
    }

    /// Bounded verified recovery read; callers additionally verify tenant metadata.
    pub async fn read_verified(
        &self,
        namespace: &str,
        digest: &str,
        maximum_bytes: usize,
    ) -> Result<Vec<u8>, ArtifactStoreError> {
        let path = self.durable_path(namespace, digest)?;
        let object = self.store.get(&path).await.map_err(store_error)?;
        if object.meta.size > maximum_bytes as u64 {
            return Err(error("artifact exceeds read bound"));
        }
        let mut stream = object.into_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(store_error)?;
            if chunk.len() > maximum_bytes.saturating_sub(bytes.len()) {
                return Err(error("artifact exceeds read bound"));
            }
            bytes.extend_from_slice(&chunk);
        }
        if format!("sha256:{}", hex::encode(Sha256::digest(&bytes))) != digest {
            return Err(error("artifact recovery digest mismatch"));
        }
        Ok(bytes)
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
        self.sync_path(&path)?;
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
            self.sync_path(&destination)?;
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
        self.sync_path(&destination)?;
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
    async fn filesystem_reopens_recovers_promotions_and_rejects_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let config = ArtifactSettings {
            backend: "filesystem".into(),
            filesystem_root: Some(directory.path().to_owned()),
            bucket: None,
            endpoint: None,
            allow_http: false,
            prefix: "evidence".into(),
            retention_days: 30,
        };
        let store = DurableArtifactStore::from_configuration(&config)
            .unwrap()
            .unwrap();
        let bytes = b"immutable snapshot";
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
        let staged = store
            .stage(&format!("{HOST_A}/attempt/snapshot"), bytes)
            .await
            .unwrap();
        drop(store);
        let reopened = DurableArtifactStore::from_configuration(&config)
            .unwrap()
            .unwrap();
        let reference = reopened.promote(HOST_A, &staged, &digest).await.unwrap();
        assert_eq!(
            reopened.promote(HOST_A, &staged, &digest).await.unwrap(),
            reference
        );
        assert_eq!(
            reopened.read_verified(HOST_A, &digest, 1024).await.unwrap(),
            bytes
        );
        assert!(reopened.read_verified(HOST_A, &digest, 2).await.is_err());
        assert!(reopened.read_verified(HOST_B, &digest, 1024).await.is_err());
        let path = reopened.durable_path(HOST_A, &digest).unwrap();
        std::fs::write(directory.path().join(path.as_ref()), b"corrupt").unwrap();
        assert!(reopened.read_verified(HOST_A, &digest, 1024).await.is_err());
        assert!(reopened.promote(HOST_A, &staged, &digest).await.is_err());
    }

    #[test]
    fn disabled_or_invalid_filesystem_store_does_not_admit_storage() {
        let mut config = ArtifactSettings {
            backend: "disabled".into(),
            filesystem_root: None,
            bucket: None,
            endpoint: None,
            allow_http: false,
            prefix: "evidence".into(),
            retention_days: 30,
        };
        assert!(
            DurableArtifactStore::from_configuration(&config)
                .unwrap()
                .is_none()
        );
        config.backend = "filesystem".into();
        assert!(DurableArtifactStore::from_configuration(&config).is_err());
        config.filesystem_root = Some("/proc/light-workflow-evidence-test".into());
        assert!(DurableArtifactStore::from_configuration(&config).is_err());
    }

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
