//! Local-only Specification preparation. Source order and bytes belong to one
//! Init; neither the provider nor the factory retains a mutable source cache.
use crate::{
    init::{
        HandlerFactoryProvider, PreparedSpecificationFactory, PreparedSpecificationResources,
        SpecificationResources,
    },
    init_error::InitError,
};
use gateway_policy::openapi_operation::{
    BasePathProfile, MAX_SOURCE_BYTES, OperationCatalogue, SourceFormat,
};
use light_runtime::{RuntimeConfig, RuntimeError};
use light_security::SecurityRuntime;
use serde_json::Value;
use serverless_core::{handlers::specification::SpecificationHandler, plan::HandlerFactory};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, sync::Arc};

pub const SOURCE_NAMES: [&str; 4] = ["openapi.yml", "openapi.yaml", "openapi.json", "spec.yaml"];
pub struct SpecificationPolicyProvider;

fn invalid() -> InitError {
    InitError::InvalidConfiguration("specification: invalid or unavailable local source")
}
fn settings_error() -> InitError {
    InitError::InvalidConfiguration("openapi-handler.yml: unsupported, unknown or invalid setting")
}

pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    let value: Value = match config
        .module_registry
        .load_config(config, "openapi-handler.yml")
    {
        Ok(value) => value,
        Err(RuntimeError::MissingConfig(_)) => serde_json::json!({}),
        Err(_) => return Err(settings_error()),
    };
    let object = value.as_object().ok_or_else(settings_error)?;
    for (key, value) in object {
        match key.as_str() {
            "enabled" if value.is_boolean() => {}
            "multipleSpec" | "ignoreInvalidPath" if value == &Value::Bool(false) => {}
            "pathSpecMapping" if value.as_object().is_some_and(|v| v.is_empty()) => {}
            _ => return Err(settings_error()),
        }
    }
    Ok(["multipleSpec", "ignoreInvalidPath", "pathSpecMapping"]
        .into_iter()
        .map(|key| {
            format!(
                "openapi-handler.{key}: unsupported {} ignored",
                if object.contains_key(key) {
                    "default"
                } else {
                    "absent"
                }
            )
        })
        .collect())
}

/// Filename precedence applies before per-filename source precedence: external,
/// application, default, embedded. A present failing candidate is never skipped.
fn capture(config: &RuntimeConfig) -> Result<(&'static str, Vec<u8>), InitError> {
    for name in SOURCE_NAMES {
        for dir in [
            Some(config.external_config_dir.as_path()),
            Some(config.config_dir.as_path()),
            config.default_config_dir.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            let path = dir.join(name);
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    if !std::fs::metadata(&path).map_err(|_| invalid())?.is_file() {
                        return Err(invalid());
                    }
                    let file = std::fs::File::open(path).map_err(|_| invalid())?;
                    if !file.metadata().map_err(|_| invalid())?.is_file() {
                        return Err(invalid());
                    }
                    let mut bytes = Vec::new();
                    file.take((MAX_SOURCE_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)
                        .map_err(|_| invalid())?;
                    if bytes.len() > MAX_SOURCE_BYTES {
                        return Err(invalid());
                    }
                    return Ok((name, bytes));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(invalid()),
            }
        }
        if let Some(file) = config_loader::embedded_config_file(config.embedded_config, name) {
            if file.content.len() > MAX_SOURCE_BYTES {
                return Err(invalid());
            }
            return Ok((name, file.content.as_bytes().to_vec()));
        }
    }
    Err(invalid())
}

impl SpecificationPolicyProvider {
    /// Opt-in isolated-parser integration. The callback receives the single
    /// selected capture before any ordinary numeric deserialization. It must
    /// prepare the complete precision catalogue before returning its metadata
    /// projection. Existing provider methods retain their ordinary parser.
    pub fn prepare_with_metadata_projection(
        &self,
        config: &RuntimeConfig,
        project: impl FnOnce(Arc<[u8]>, SourceFormat) -> Result<Vec<u8>, InitError>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        let capture = self.capture_projection(config)?;
        let metadata = project(capture.source.clone(), capture.format)?;
        capture.finish(&metadata)
    }

    /// Capture once before any numeric deserialization. Runtime precision providers
    /// can await eager compilation before finishing this immutable producer.
    pub fn capture_projection(
        &self,
        config: &RuntimeConfig,
    ) -> Result<CapturedSpecification, InitError> {
        warnings(config)?;
        let handler: Value = config
            .module_registry
            .load_config(config, "handler.yml")
            .map_err(|_| invalid())?;
        let (name, bytes) = capture(config)?;
        Ok(CapturedSpecification {
            name,
            source: bytes.into(),
            format: if name.ends_with(".json") {
                SourceFormat::Json
            } else {
                SourceFormat::Yaml
            },
            fallback: handler.get("basePath").cloned(),
        })
    }

    fn prepare(&self, config: &RuntimeConfig) -> Result<PreparedSpecificationResources, InitError> {
        warnings(config)?;
        let handler: Value = config
            .module_registry
            .load_config(config, "handler.yml")
            .map_err(|_| invalid())?;
        let (name, bytes) = capture(config)?;
        let format = if name.ends_with(".json") {
            SourceFormat::Json
        } else {
            SourceFormat::Yaml
        };
        let profile = Arc::new(
            BasePathProfile::from_source(&bytes, format, handler.get("basePath"))
                .map_err(InitError::InvalidConfiguration)?,
        );
        let catalogue = OperationCatalogue::from_source(&bytes, format)
            .map_err(InitError::InvalidConfiguration)?;
        let factory = SpecificationHandler::from_prepared(catalogue.clone(), profile.clone())
            .map_err(|error| match error {
                serverless_core::plan::InitError::Malformed(message) => {
                    InitError::InvalidConfiguration(message)
                }
                _ => invalid(),
            })?;
        let mut hash = Sha256::new();
        hash.update(b"light-lambda/specification-source/v1\0");
        hash.update((name.len() as u64).to_be_bytes());
        hash.update(name.as_bytes());
        hash.update((bytes.len() as u64).to_be_bytes());
        hash.update(&bytes);
        Ok(PreparedSpecificationResources {
            prepared: (Arc::new(factory), Some(hash.finalize().into())),
            resources: Some(Arc::new(SpecificationResources { catalogue, profile })),
        })
    }
}
impl HandlerFactoryProvider for SpecificationPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.prepare(config).map(|result| result.prepared.0)
    }
    fn prepare_specification(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationFactory, InitError> {
        self.prepare(config).map(|result| result.prepared)
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        self.prepare(config)
    }
}

/// Active captured sources replace canonical source documents in the digest.
/// In particular, do not reopen the selected bytes or parse ignored competitors.
/// The legacy digest path remains unchanged when the provider supplies no identity.
pub(crate) fn configuration_digest(
    config: &RuntimeConfig,
    identity: [u8; 32],
) -> Result<String, InitError> {
    let mut names = std::collections::BTreeSet::new();
    for file in config.embedded_config {
        names.insert(file.name.to_owned());
    }
    for dir in [
        config.default_config_dir.as_deref(),
        Some(config.config_dir.as_path()),
        Some(config.external_config_dir.as_path()),
    ]
    .into_iter()
    .flatten()
    {
        if !dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(dir).map_err(|_| invalid())? {
            let path = entry.map_err(|_| invalid())?.path();
            if path.extension().is_some_and(|s| s == "yml" || s == "yaml") {
                names.insert(
                    path.file_name()
                        .and_then(|s| s.to_str())
                        .ok_or_else(invalid)?
                        .to_owned(),
                );
            }
        }
    }
    let mut documents = BTreeMap::new();
    for name in names {
        if !SOURCE_NAMES.contains(&name.as_str())
            && (name.ends_with(".yml") || name.ends_with(".yaml"))
        {
            let value: Value = config
                .module_registry
                .load_config(config, &name)
                .map_err(|_| invalid())?;
            documents.insert(name, value);
        }
    }
    let values: BTreeMap<_, _> = config.resolved_values.iter().collect();
    let base = Sha256::digest(serde_json::to_vec(&(values, documents)).map_err(|_| invalid())?);
    let mut hash = Sha256::new();
    hash.update(b"light-lambda/configuration-with-specification/v1\0");
    hash.update(base);
    hash.update(identity);
    Ok(format!("{:x}", hash.finalize()))
}

/// One source identity retained through asynchronous precision preparation.
pub struct CapturedSpecification {
    name: &'static str,
    pub source: Arc<[u8]>,
    pub format: SourceFormat,
    fallback: Option<Value>,
}
impl CapturedSpecification {
    pub fn finish(self, metadata: &[u8]) -> Result<PreparedSpecificationResources, InitError> {
        let profile = Arc::new(
            BasePathProfile::from_metadata_projection(metadata, self.fallback.as_ref())
                .map_err(InitError::InvalidConfiguration)?,
        );
        let catalogue = OperationCatalogue::from_captured_source_metadata(
            self.source.clone(),
            self.format,
            metadata,
        )
        .map_err(InitError::InvalidConfiguration)?;
        let factory = SpecificationHandler::from_prepared(catalogue.clone(), profile.clone())
            .map_err(|error| match error {
                serverless_core::plan::InitError::Malformed(message) => {
                    InitError::InvalidConfiguration(message)
                }
                _ => invalid(),
            })?;
        let mut hash = Sha256::new();
        hash.update(b"light-lambda/specification-source/v1\0");
        hash.update((self.name.len() as u64).to_be_bytes());
        hash.update(self.name.as_bytes());
        hash.update((self.source.len() as u64).to_be_bytes());
        hash.update(&self.source);
        Ok(PreparedSpecificationResources {
            prepared: (Arc::new(factory), Some(hash.finalize().into())),
            resources: Some(Arc::new(SpecificationResources { catalogue, profile })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Provider(AtomicUsize);
    impl HandlerFactoryProvider for Provider {
        fn create(
            &self,
            _: &RuntimeConfig,
            _: Option<Arc<SecurityRuntime>>,
        ) -> Result<Arc<dyn HandlerFactory>, InitError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(
                SpecificationHandler::from_source(
                    br#"{"openapi":"3.1.0","paths":{}}"#,
                    SourceFormat::Json,
                )
                .unwrap(),
            ))
        }
    }
    #[tokio::test]
    async fn default_specification_preparation_delegates_once_without_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.yml"), "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n").unwrap();
        let config =
            light_runtime::LightRuntimeBuilder::new(crate::init::ConfigPreparationTransport)
                .with_config_dir(dir.path())
                .with_external_config_dir(dir.path())
                .build()
                .prepare_local_config()
                .await
                .unwrap();
        let provider: Arc<dyn HandlerFactoryProvider> = Arc::new(Provider(AtomicUsize::new(0)));
        let (_, identity) = provider.prepare_specification(&config, None).unwrap();
        assert!(identity.is_none());
        // Trait-object compatibility and no cross-preparation identity memo.
        let concrete = Provider(AtomicUsize::new(0));
        concrete.prepare_specification(&config, None).unwrap();
        assert_eq!(concrete.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn digest_uses_captured_bytes_not_a_reopened_source() {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in [
            (
                "server.yml",
                "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
            ),
            ("handler.yml", "handlers: []\ndefaultHandlers: []\n"),
            ("openapi-handler.yml", "enabled: true\n"),
            ("openapi.yaml", "openapi: 3.0.3\npaths: {'/x': {get: {}}}\n"),
        ] {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        let config =
            light_runtime::LightRuntimeBuilder::new(crate::init::ConfigPreparationTransport)
                .with_config_dir(dir.path())
                .with_external_config_dir(dir.path())
                .build()
                .prepare_local_config()
                .await
                .unwrap();
        let (_, identity) = SpecificationPolicyProvider
            .prepare_specification(&config, None)
            .unwrap();
        let identity = identity.unwrap();
        let digest = configuration_digest(&config, identity).unwrap();
        std::fs::write(
            dir.path().join("openapi.yaml"),
            "changed-sensitive-marker: [",
        )
        .unwrap();
        std::fs::write(dir.path().join("spec.yaml"), "ignored-sensitive-marker: [").unwrap();
        assert_eq!(configuration_digest(&config, identity).unwrap(), digest);
        assert!(
            SpecificationPolicyProvider
                .prepare_specification(&config, None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn resource_defaults_delegate_once_and_preserve_legacy_tuple() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("server.yml"), "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n").unwrap();
        let config =
            light_runtime::LightRuntimeBuilder::new(crate::init::ConfigPreparationTransport)
                .with_config_dir(dir.path())
                .with_external_config_dir(dir.path())
                .build()
                .prepare_local_config()
                .await
                .unwrap();
        let provider = Provider(AtomicUsize::new(0));
        let object: &dyn HandlerFactoryProvider = &provider;
        let result = object
            .prepare_specification_resources(&config, None)
            .unwrap();
        assert!(result.resources.is_none());
        assert!(result.prepared.1.is_none());
        assert_eq!(provider.0.load(Ordering::SeqCst), 1);
        object.prepare_validator(&config, None, None).unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 2);
        object.prepare_specification(&config, None).unwrap();
        assert_eq!(provider.0.load(Ordering::SeqCst), 3);
        struct TupleProvider(Provider);
        impl HandlerFactoryProvider for TupleProvider {
            fn create(
                &self,
                _: &RuntimeConfig,
                _: Option<Arc<SecurityRuntime>>,
            ) -> Result<Arc<dyn HandlerFactory>, InitError> {
                panic!("default resource entry must preserve the tuple override")
            }
            fn prepare_specification(
                &self,
                config: &RuntimeConfig,
                security: Option<Arc<SecurityRuntime>>,
            ) -> Result<PreparedSpecificationFactory, InitError> {
                Ok((self.0.create(config, security)?, Some([7; 32])))
            }
        }
        let tuple = TupleProvider(Provider(AtomicUsize::new(0)));
        let object: &dyn HandlerFactoryProvider = &tuple;
        let result = object
            .prepare_specification_resources(&config, None)
            .unwrap();
        assert_eq!(result.prepared.1, Some([7; 32]));
        assert!(result.resources.is_none());
        assert_eq!(tuple.0.0.load(Ordering::SeqCst), 1);
    }
}
