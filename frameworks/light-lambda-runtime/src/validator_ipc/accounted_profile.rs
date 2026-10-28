//! Proposed explicit ceilings around existing source/metadata requested-capacity
//! accounting. Not a production adoption capability, RSS bound or default selector.
use super::{
    Engine, Failure, Response, Session, Settings, accounting::MetadataLimits,
    bundle_admission::CatalogueEngine, controller, ownership, supervision,
};
use async_trait::async_trait;
use gateway_policy::openapi_operation::{MAX_SOURCE_BYTES, SourceFormat};
use std::{mem::size_of, sync::Arc};
use tokio::time::Instant;

/// Trusted code supplies all ceilings; no Default/Deserialize or layer override.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_source_bytes: usize,
    pub metadata: MetadataLimits,
}
impl Limits {
    fn validate(&self, policy: supervision::Policy) -> Result<(), Failure> {
        policy.validate().map_err(|_| Failure::AdapterInvariant)?;
        let parent = usize::try_from(policy.parent_requested_bytes)
            .map_err(|_| Failure::AdapterInvariant)?;
        if self.max_source_bytes == 0 || self.max_source_bytes > MAX_SOURCE_BYTES {
            return Err(Failure::AdapterInvariant);
        }
        self.metadata.validate(parent)
    }
}

/// Additive opt-in wrapper around the already trusted catalogue/static engine.
/// Existing qualification composition is unchanged; no ordinary app uses this.
pub struct AccountedCatalogueEngine {
    engine: Arc<dyn Engine>,
    limits: Limits,
}
impl AccountedCatalogueEngine {
    /// Only the reviewed typed catalogue wrapper, never arbitrary Engine input.
    pub fn new_readonly_observed(
        engine: Arc<super::bundle_admission::ReadonlyCatalogueEngine>,
        limits: Option<Limits>,
    ) -> Result<Self, Failure> {
        let policy = engine.catalogue.engine.policy;
        Self::with_engine(engine, policy, limits)
    }
    pub fn new(engine: Arc<CatalogueEngine>, limits: Option<Limits>) -> Result<Self, Failure> {
        let policy = engine.engine.policy;
        Self::with_engine(engine, policy, limits)
    }
    /// Same trusted catalogue/static gate with the existing qualification observer.
    /// No arbitrary Engine injection, observer replacement or default selection.
    pub fn new_observed(
        engine: Arc<super::lifecycle::CatalogueObservedProfileEngine>,
        limits: Option<Limits>,
    ) -> Result<Self, Failure> {
        let policy = engine.engine.engine.policy;
        Self::with_engine(engine, policy, limits)
    }
    fn with_engine(
        engine: Arc<dyn Engine>,
        policy: supervision::Policy,
        limits: Option<Limits>,
    ) -> Result<Self, Failure> {
        let limits = limits.ok_or(Failure::AdapterInvariant)?;
        limits.validate(policy)?;
        Ok(Self { engine, limits })
    }
}
#[async_trait]
impl Engine for AccountedCatalogueEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        self.engine.configuration_preparation()
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
impl AccountedCatalogueEngine {
    async fn initialize_inner(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        diagnostic: Option<Arc<crate::validator_init_diagnostic::InitDiagnostic>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if source.len() > self.limits.max_source_bytes {
            return Err(Failure::ParentBudget);
        }
        let session = match diagnostic {
            Some(diagnostic) => {
                self.engine
                    .initialize_owned_diagnosed(
                        source, format, settings, deadline, context, diagnostic,
                    )
                    .await?
            }
            None => {
                self.engine
                    .initialize_owned(source, format, settings, deadline, context)
                    .await?
            }
        };
        // Ready already exists here. Failure is post-start Init rejection, never
        // described as zero starts. The existing context tracks the actual drain.
        let result = (|| {
            let bytes = session.metadata_json().ok_or(Failure::AdapterInvariant)?;
            if bytes.len() > self.limits.metadata.max_wire_bytes {
                return Err(Failure::ParentBudget);
            }
            let size = size_of::<AccountedSession>()
                .checked_add(2 * size_of::<usize>())
                .ok_or(Failure::ParentBudget)?;
            let credit = session
                .reserve_request(size)?
                .ok_or(Failure::AdapterInvariant)?;
            if Instant::now() >= deadline {
                return Err(Failure::Deadline);
            }
            Ok(credit)
        })();
        match result {
            Ok(credit) => Ok(Arc::new(AccountedSession {
                session,
                limits: self.limits.metadata,
                _credit: credit,
            })),
            Err(error) => {
                // Keep primary typed failure; a failed drain is not called clean.
                let _cleanup = session.shutdown(deadline).await;
                Err(error)
            }
        }
    }
}

struct AccountedSession {
    session: Arc<dyn Session>,
    limits: MetadataLimits,
    _credit: controller::Reservation,
}
#[async_trait]
impl Session for AccountedSession {
    fn initialization_scope(&self) -> Option<ownership::Initialization> {
        self.session.initialization_scope()
    }
    async fn shutdown(&self, deadline: Instant) -> Result<(), Failure> {
        self.session.shutdown(deadline).await
    }
    fn admit_request(&self) -> Result<controller::Admission, Failure> {
        self.session.admit_request()
    }
    async fn request_admitted(
        &self,
        event: serde_json::Value,
        deadline: Instant,
        admission: controller::Admission,
    ) -> Result<Response, Failure> {
        self.session
            .request_admitted(event, deadline, admission)
            .await
    }
    fn metadata_json(&self) -> Option<&str> {
        self.session.metadata_json()
    }
    fn reserve_request(&self, bytes: usize) -> Result<Option<controller::Reservation>, Failure> {
        self.session.reserve_request(bytes)
    }
    fn reserve_metadata(
        &self,
        bytes: &[u8],
    ) -> Result<Option<controller::MetadataReservation>, Failure> {
        self.session
            .reserve_metadata_limited(bytes, self.limits)
            .map(Some)
    }
    // A second policy wrapper cannot silently replace this wrapper's ceilings.
    // Public construction admits only CatalogueEngine, not arbitrary sessions.
    async fn request(
        &self,
        event: serde_json::Value,
        deadline: Instant,
    ) -> Result<Response, Failure> {
        self.session.request(event, deadline).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };
    struct FakeSession {
        budget: Arc<controller::Budget>,
        metadata: Option<&'static str>,
        support: bool,
        fail_shutdown: bool,
        requests: AtomicUsize,
        shutdowns: Mutex<Vec<Instant>>,
        limited: AtomicUsize,
    }
    #[async_trait]
    impl Session for FakeSession {
        fn metadata_json(&self) -> Option<&str> {
            self.metadata
        }
        fn reserve_request(&self, n: usize) -> Result<Option<controller::Reservation>, Failure> {
            if self.support {
                self.budget.reservation(n).map(Some)
            } else {
                Ok(None)
            }
        }
        fn reserve_metadata(
            &self,
            _: &[u8],
        ) -> Result<Option<controller::MetadataReservation>, Failure> {
            panic!("must not fall back to unrestricted metadata")
        }
        fn reserve_metadata_limited(
            &self,
            bytes: &[u8],
            limits: MetadataLimits,
        ) -> Result<controller::MetadataReservation, Failure> {
            self.limited.fetch_add(1, Ordering::SeqCst);
            super::super::accounting::metadata_limited(&self.budget, bytes, limits)
        }
        async fn shutdown(&self, deadline: Instant) -> Result<(), Failure> {
            self.shutdowns.lock().unwrap().push(deadline);
            if self.fail_shutdown {
                Err(Failure::Cancelled)
            } else {
                Ok(())
            }
        }
        async fn request(&self, event: serde_json::Value, _: Instant) -> Result<Response, Failure> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            Ok(Response::Decoded(super::super::Reply {
                status: 200,
                body: event.to_string(),
                violation: None,
            }))
        }
    }
    struct FakeEngine {
        session: Arc<FakeSession>,
        starts: AtomicUsize,
        source: Mutex<Option<Arc<[u8]>>>,
        settings: Mutex<Option<Arc<Settings>>>,
    }
    #[async_trait]
    impl Engine for FakeEngine {
        async fn initialize(
            &self,
            source: Arc<[u8]>,
            format: SourceFormat,
            settings: Settings,
            deadline: Instant,
        ) -> Result<Arc<dyn Session>, Failure> {
            self.initialize_owned(source, format, Arc::new(settings), deadline, None)
                .await
        }
        async fn initialize_owned(
            &self,
            source: Arc<[u8]>,
            _: SourceFormat,
            settings: Arc<Settings>,
            _: Instant,
            _: Option<Arc<crate::init::PreparationContext>>,
        ) -> Result<Arc<dyn Session>, Failure> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            *self.source.lock().unwrap() = Some(source);
            *self.settings.lock().unwrap() = Some(settings);
            Ok(self.session.clone())
        }
    }
    fn policy() -> supervision::Policy {
        supervision::Policy {
            platform_bytes: 1 << 22,
            parent_requested_bytes: 1 << 20,
            worker_requested_bytes: 1 << 20,
            headroom_bytes: 1 << 20,
            worker_address_space_bytes: 1 << 21,
            lifetime_cpu_seconds: 1,
        }
    }
    fn limits() -> Limits {
        Limits {
            max_source_bytes: 100,
            metadata: MetadataLimits {
                max_wire_bytes: 1000,
                max_peak_bytes: 1 << 19,
                max_retained_bytes: 1 << 18,
            },
        }
    }
    fn fake(metadata: Option<&'static str>, support: bool, fail_shutdown: bool) -> Arc<FakeEngine> {
        Arc::new(FakeEngine {
            session: Arc::new(FakeSession {
                budget: controller::Budget::new(1 << 20),
                metadata,
                support,
                fail_shutdown,
                requests: AtomicUsize::new(0),
                shutdowns: Mutex::new(vec![]),
                limited: AtomicUsize::new(0),
            }),
            starts: AtomicUsize::new(0),
            source: Mutex::new(None),
            settings: Mutex::new(None),
        })
    }
    fn until() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    #[test]
    fn missing_or_invalid_explicit_policy_never_starts_engine() {
        let fake = fake(Some("{}"), true, false);
        assert!(matches!(
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), None),
            Err(Failure::AdapterInvariant)
        ));
        let mut invalid = limits();
        invalid.max_source_bytes = 0;
        assert!(matches!(
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(invalid)),
            Err(Failure::AdapterInvariant)
        ));
        invalid = limits();
        invalid.metadata.max_peak_bytes = (1 << 20) + 1;
        assert!(matches!(
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(invalid)),
            Err(Failure::AdapterInvariant)
        ));
        let mut p = policy();
        p.parent_requested_bytes = u64::MAX;
        assert!(matches!(
            AccountedCatalogueEngine::with_engine(fake.clone(), p, Some(limits())),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn source_policy_and_expired_deadline_reject_before_start() {
        let fake = fake(Some("{}"), true, false);
        let mut l = limits();
        l.max_source_bytes = 1;
        let engine =
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(l)).unwrap();
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(b"{}".as_slice()),
                    SourceFormat::Json,
                    Settings::default(),
                    until()
                )
                .await,
            Err(Failure::ParentBudget)
        ));
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(b"x".as_slice()),
                    SourceFormat::Json,
                    Settings::default(),
                    Instant::now()
                )
                .await,
            Err(Failure::Deadline)
        ));
        assert_eq!(fake.starts.load(Ordering::SeqCst), 0);
        assert_eq!(fake.session.requests.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn admitted_wrapper_preserves_arcs_responses_and_shared_credit_lifetime() {
        let fake = fake(Some("{}"), true, false);
        let engine =
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(limits())).unwrap();
        let source: Arc<[u8]> = Arc::from(b"{}".as_slice());
        let settings = Arc::new(Settings::default());
        let session = engine
            .initialize_shared(
                source.clone(),
                SourceFormat::Json,
                settings.clone(),
                until(),
            )
            .await
            .unwrap();
        assert!(Arc::ptr_eq(
            &source,
            fake.source.lock().unwrap().as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            &settings,
            fake.settings.lock().unwrap().as_ref().unwrap()
        ));
        let held = fake.session.budget.used();
        assert!(held > 0);
        let same = session.clone();
        drop(session);
        assert_eq!(fake.session.budget.used(), held);
        let guards = same.reserve_metadata(b"{}").unwrap().unwrap();
        assert_eq!(fake.session.limited.load(Ordering::SeqCst), 1);
        drop(guards.scratch);
        let retained = guards.retained.bytes();
        assert_eq!(fake.session.budget.used(), held + retained);
        let event = serde_json::json!({"value":"synthetic"});
        let response = same.request(event.clone(), until()).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, event.to_string());
        drop(same);
        assert_eq!(fake.session.budget.used(), retained);
        drop(guards.retained);
        assert_eq!(fake.session.budget.used(), 0);
    }
    #[tokio::test]
    async fn post_ready_oversize_preserves_primary_error_and_original_cleanup_deadline() {
        let fake = fake(Some("{}"), true, true);
        let mut l = limits();
        l.metadata.max_wire_bytes = 1;
        let until = until();
        let engine =
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(l)).unwrap();
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(b"{}".as_slice()),
                    SourceFormat::Json,
                    Settings::default(),
                    until
                )
                .await,
            Err(Failure::ParentBudget)
        ));
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        assert_eq!(*fake.session.shutdowns.lock().unwrap(), vec![until]);
        assert_eq!(fake.session.requests.load(Ordering::SeqCst), 0);
        assert_eq!(fake.session.budget.used(), 0);
    }
    #[tokio::test]
    async fn absent_metadata_or_unaccounted_session_cannot_fall_back() {
        for (metadata, support) in [(None, true), (Some("{}"), false)] {
            let fake = fake(metadata, support, false);
            let engine =
                AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(limits()))
                    .unwrap();
            assert!(matches!(
                engine
                    .initialize(
                        Arc::from(b"{}".as_slice()),
                        SourceFormat::Json,
                        Settings::default(),
                        until()
                    )
                    .await,
                Err(Failure::AdapterInvariant)
            ));
            assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
            assert_eq!(fake.session.shutdowns.lock().unwrap().len(), 1);
        }
    }
    #[tokio::test]
    async fn insufficient_metadata_policy_refunds_without_repeated_start_or_fallback() {
        let fake = fake(Some("{}"), true, false);
        let mut l = limits();
        l.metadata.max_peak_bytes = 2;
        l.metadata.max_retained_bytes = 1;
        let engine =
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(l)).unwrap();
        let session = engine
            .initialize(
                Arc::from(b"{}".as_slice()),
                SourceFormat::Json,
                Settings::default(),
                until(),
            )
            .await
            .unwrap();
        let held = fake.session.budget.used();
        assert!(matches!(
            session.reserve_metadata(b"{}"),
            Err(Failure::ParentBudget)
        ));
        assert_eq!(fake.session.budget.used(), held);
        assert_eq!(fake.starts.load(Ordering::SeqCst), 1);
        assert_eq!(fake.session.requests.load(Ordering::SeqCst), 0);
        drop(session);
        assert_eq!(fake.session.budget.used(), 0);
    }

    #[tokio::test]
    async fn caller_cancellation_cannot_refund_the_metadata_work_reference() {
        let fake = fake(Some("{}"), true, false);
        let engine =
            AccountedCatalogueEngine::with_engine(fake.clone(), policy(), Some(limits())).unwrap();
        let caller = engine
            .initialize(
                Arc::from(b"{}".as_slice()),
                SourceFormat::Json,
                Settings::default(),
                until(),
            )
            .await
            .unwrap();
        let work = caller.clone();
        let held = fake.session.budget.used();
        let reservations = work.reserve_metadata(b"{}").unwrap().unwrap();
        let peak_held = fake.session.budget.used();
        drop(caller);
        assert_eq!(fake.session.budget.used(), peak_held);
        drop(reservations.scratch);
        let retained = reservations.retained.bytes();
        assert_eq!(fake.session.budget.used(), held + retained);
        // Synthetic unfinished metadata owner, not a child/quarantine experiment.
        let owner = Arc::new((work, reservations.retained));
        let uncertain = owner.clone();
        drop(owner);
        assert_eq!(fake.session.budget.used(), held + retained);
        drop(uncertain);
        assert_eq!(fake.session.budget.used(), 0);
    }
    #[test]
    fn legacy_session_default_refuses_limited_metadata_without_fallback() {
        struct Legacy;
        #[async_trait]
        impl Session for Legacy {
            fn reserve_metadata(
                &self,
                _: &[u8],
            ) -> Result<Option<controller::MetadataReservation>, Failure> {
                panic!("unrestricted fallback is forbidden")
            }
            async fn request(&self, _: serde_json::Value, _: Instant) -> Result<Response, Failure> {
                Err(Failure::NoReady)
            }
        }
        let budget = controller::Budget::new(100_000);
        let wrapper = AccountedSession {
            session: Arc::new(Legacy),
            limits: limits().metadata,
            _credit: budget.reservation(1).unwrap(),
        };
        assert!(matches!(
            wrapper.reserve_metadata(b"{}"),
            Err(Failure::AdapterInvariant)
        ));
        drop(wrapper);
        assert_eq!(budget.used(), 0);
    }
}
