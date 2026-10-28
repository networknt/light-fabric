//! Proposed opt-in admission of captured source/settings, not a default selector.
//! Trusted code supplies the reviewed identity and already-pinned static engine.
use super::{Engine, Failure, Session, Settings, SupervisedStaticEngine};
use async_trait::async_trait;
use gateway_policy::openapi_operation::{MAX_SOURCE_BYTES, SourceFormat};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::time::Instant;

pub struct CataloguePin {
    pub source_bytes: usize,
    pub source_sha256: [u8; 32],
    pub format: SourceFormat,
    pub settings: Settings,
}
impl CataloguePin {
    async fn admit(
        &self,
        source: &[u8],
        format: SourceFormat,
        settings: &Settings,
        deadline: Instant,
    ) -> Result<(), Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if self.source_bytes == 0
            || self.source_bytes > MAX_SOURCE_BYTES
            || source.len() != self.source_bytes
            || !matches!(
                (self.format, format),
                (SourceFormat::Json, SourceFormat::Json) | (SourceFormat::Yaml, SourceFormat::Yaml)
            )
            || &self.settings != settings
        {
            return Err(Failure::AdapterInvariant);
        }
        // Hash only the captured Arc bytes: no path reopen, projection or parse.
        // Bounded synchronous chunks yield under the original Init deadline;
        // no detached task, new owner or untracked source copy is introduced.
        let mut hash = Sha256::new();
        for chunk in source.chunks(64 * 1024) {
            hash.update(chunk);
            tokio::task::yield_now().await;
            if Instant::now() >= deadline {
                return Err(Failure::Deadline);
            }
        }
        if <[u8; 32]>::from(hash.finalize()) != self.source_sha256 {
            return Err(Failure::AdapterInvariant);
        }
        Ok(())
    }
}

/// Compose only with the existing trusted static supervisor. Keep an Arc to the
/// inner engine for its existing owner-facing shutdown; never replace that API.
pub struct CatalogueEngine {
    pub engine: Arc<SupervisedStaticEngine>,
    pub catalogue: CataloguePin,
}
#[async_trait]
impl Engine for CatalogueEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        self.engine.configuration_preparation()
    }
    async fn initialize_owned_diagnosed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        diagnostic: Arc<crate::validator_init_diagnostic::InitDiagnostic>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_observed(
            source,
            format,
            settings,
            deadline,
            context,
            super::EngineObservation {
                lifecycle: None,
                diagnostic: Some(diagnostic),
            },
        )
        .await
    }
    async fn initialize_owned(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_observed(
            source,
            format,
            settings,
            deadline,
            context,
            super::EngineObservation::default(),
        )
        .await
    }
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_owned(source, format, settings, deadline, None)
            .await
    }
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_shared(source, format, Arc::new(settings), deadline)
            .await
    }
}

// Isolated observer bridge; default Engine calls pass None.
impl CatalogueEngine {
    pub(super) async fn initialize_observed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        observation: super::EngineObservation,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.catalogue
            .admit(&source, format, &settings, deadline)
            .await?;
        self.engine
            .initialize_observed(source, format, settings, deadline, context, observation)
            .await
    }
}

/// Closed typed qualification composition. Reuses the same catalogue,
/// supervisor and observation path; never chooses a filesystem from runtime I/O.
pub struct ReadonlyCatalogueEngine {
    pub(super) catalogue: Arc<CatalogueEngine>,
    profile: super::ReadonlyProfile,
    lifecycle: Option<Arc<super::lifecycle::Lifecycle>>,
}
impl ReadonlyCatalogueEngine {
    pub fn new_observed(
        catalogue: Arc<CatalogueEngine>,
        profile: super::ReadonlyProfile,
        lifecycle: Arc<super::lifecycle::Lifecycle>,
    ) -> Result<Self, Failure> {
        profile.validate_pin(&catalogue.engine.executable)?;
        Ok(Self {
            catalogue,
            profile,
            lifecycle: Some(lifecycle),
        })
    }
    #[allow(clippy::too_many_arguments)]
    async fn initialize_inner(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        diagnostic: Option<Arc<crate::validator_init_diagnostic::InitDiagnostic>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.catalogue
            .catalogue
            .admit(&source, format, &settings, deadline)
            .await?;
        self.catalogue
            .engine
            .initialize_selected(
                source,
                format,
                settings,
                deadline,
                context,
                super::EngineObservation {
                    lifecycle: self.lifecycle.clone(),
                    diagnostic,
                },
                Some(self.profile),
            )
            .await
    }
}
#[async_trait]
impl Engine for ReadonlyCatalogueEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        self.catalogue.configuration_preparation()
    }
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_inner(source, format, Arc::new(settings), deadline, None, None)
            .await
    }
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_inner(source, format, settings, deadline, None, None)
            .await
    }
    async fn initialize_owned(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_inner(source, format, settings, deadline, context, None)
            .await
    }
    async fn initialize_owned_diagnosed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        diagnostic: Arc<crate::validator_init_diagnostic::InitDiagnostic>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_inner(
            source,
            format,
            settings,
            deadline,
            context,
            Some(diagnostic),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn settings() -> Settings {
        Settings {
            skip_body_validation: Some(false),
            handle_nullable_field: Some(true),
            skip_path_prefixes: Some(vec![]),
        }
    }
    fn pin(source: &[u8]) -> CataloguePin {
        CataloguePin {
            source_bytes: source.len(),
            source_sha256: Sha256::digest(source).into(),
            format: SourceFormat::Yaml,
            settings: settings(),
        }
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    #[tokio::test]
    async fn exact_source_and_settings_admit_without_io_or_child() {
        let source = b"openapi: 3.0.0\n";
        assert!(
            pin(source)
                .admit(source, SourceFormat::Yaml, &settings(), deadline())
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn same_length_changed_bytes_reject() {
        assert!(matches!(
            pin(b"old")
                .admit(b"new", SourceFormat::Yaml, &settings(), deadline())
                .await,
            Err(Failure::AdapterInvariant)
        ));
    }
    #[tokio::test]
    async fn wrong_size_and_empty_and_oversized_profile_reject() {
        let mut p = pin(b"x");
        for bytes in [0, 2, MAX_SOURCE_BYTES + 1] {
            p.source_bytes = bytes;
            assert!(matches!(
                p.admit(b"x", SourceFormat::Yaml, &settings(), deadline())
                    .await,
                Err(Failure::AdapterInvariant)
            ));
        }
    }
    #[tokio::test]
    async fn source_format_cannot_be_reinterpreted() {
        assert!(matches!(
            pin(b"{}")
                .admit(b"{}", SourceFormat::Json, &settings(), deadline())
                .await,
            Err(Failure::AdapterInvariant)
        ));
    }
    #[tokio::test]
    async fn settings_each_field_and_presence_are_pinned() {
        let mut variants = vec![];
        for flag in [None, Some(true)] {
            let mut value = settings();
            value.skip_body_validation = flag;
            variants.push(value);
        }
        for flag in [None, Some(false)] {
            let mut value = settings();
            value.handle_nullable_field = flag;
            variants.push(value);
        }
        for prefixes in [None, Some(vec!["/skip".into()])] {
            let mut value = settings();
            value.skip_path_prefixes = prefixes;
            variants.push(value);
        }
        for value in variants {
            assert!(matches!(
                pin(b"x")
                    .admit(b"x", SourceFormat::Yaml, &value, deadline())
                    .await,
                Err(Failure::AdapterInvariant)
            ));
        }
    }
    #[tokio::test]
    async fn original_capture_survives_path_replacement_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schema");
        std::fs::write(&path, b"old").unwrap();
        let source: Arc<[u8]> = std::fs::read(&path).unwrap().into();
        let p = pin(&source);
        std::fs::write(&path, b"new").unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(
            p.admit(&source, SourceFormat::Yaml, &settings(), deadline())
                .await
                .is_ok()
        );
    }
    #[tokio::test]
    async fn expired_deadline_is_preserved() {
        assert!(matches!(
            pin(b"x")
                .admit(b"x", SourceFormat::Yaml, &settings(), Instant::now())
                .await,
            Err(Failure::Deadline)
        ));
    }
    #[tokio::test(start_paused = true)]
    async fn deadline_during_hash_is_not_restarted() {
        let source = vec![1; MAX_SOURCE_BYTES];
        let p = pin(&source);
        let until = Instant::now() + Duration::from_secs(1);
        let handle = tokio::spawn(async move {
            p.admit(&source, SourceFormat::Yaml, &settings(), until)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(matches!(handle.await.unwrap(), Err(Failure::Deadline)));
    }
    #[tokio::test]
    async fn mismatches_stop_before_static_supervisor_admission() {
        let engine = CatalogueEngine {
            engine: Arc::new(SupervisedStaticEngine {
                executable: crate::bundle_snapshot::Pin {
                    path: "/never-opened-catalogue-test".into(),
                    bytes: 64,
                    sha256: [0; 32],
                },
                policy: super::super::supervision::Policy {
                    platform_bytes: 3,
                    parent_requested_bytes: 1,
                    worker_requested_bytes: 1,
                    headroom_bytes: 1,
                    worker_address_space_bytes: 1,
                    lifetime_cpu_seconds: 1,
                },
            }),
            catalogue: pin(b"old"),
        };
        let before = super::super::supervision::Authority::process().phase();
        let result = engine
            .initialize_shared(
                Arc::from(b"new".as_slice()),
                SourceFormat::Yaml,
                Arc::new(settings()),
                deadline(),
            )
            .await;
        assert!(matches!(result, Err(Failure::AdapterInvariant)));
        assert_eq!(
            super::super::supervision::Authority::process().phase(),
            before
        );
    }
}
