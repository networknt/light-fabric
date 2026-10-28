//! Typed application boundary for the preserved precision engine. No worker is
//! configured or launched by the default application. Experimental limits and
//! infrastructure-error dispositions are not production admission.
#[path = "validator_ipc/accounted_profile.rs"]
pub mod accounted_profile;
#[path = "validator_ipc/accounting.rs"]
pub mod accounting;
#[path = "validator_ipc/bundle_admission.rs"]
pub mod bundle_admission;
#[path = "validator_ipc/controller.rs"]
pub mod controller;
/// Explicit in-process precision profile. No child/IPC/executable admission.
#[path = "validator_ipc/in_process.rs"]
pub mod in_process;
#[path = "validator_ipc/lifecycle.rs"]
pub mod lifecycle;
#[path = "validator_ipc/sealed_executable.rs"]
mod sealed_executable;
pub use sealed_executable::readonly::{
    Filesystem as ReadonlyFilesystem, Profile as ReadonlyProfile,
};

#[path = "validator_ipc/owned_guard.rs"]
mod owned_guard;
#[path = "validator_ipc/ownership.rs"]
pub mod ownership;
use async_trait::async_trait;
pub use controller::{Failure, Reply};
/// The existing worker's settings vocabulary, captured once per Init. None
/// preserves the original controller's absent/default wire representation.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_body_validation: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub handle_nullable_field: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_path_prefixes: Option<Vec<String>>,
}
use gateway_policy::openapi_operation::SourceFormat;
use serde_json::Value;
use std::{ops::Deref, path::PathBuf, sync::Arc};
use tokio::time::Instant;

/// Retain the controller's reply credits until application consumption ends.
pub enum Response {
    Decoded(Reply),
    Retained(controller::Retained<Reply>),
}
impl Deref for Response {
    type Target = Reply;
    fn deref(&self) -> &Reply {
        match self {
            Self::Decoded(r) => r,
            Self::Retained(r) => r,
        }
    }
}
impl Response {
    /// Bytes owns the reply/credit guard; response serialization or clones can
    /// outlive the handler without an uncharged body copy or early refund.
    pub fn into_body(self) -> bytes::Bytes {
        struct Owner(Response);
        impl AsRef<[u8]> for Owner {
            fn as_ref(&self) -> &[u8] {
                self.0.body.as_bytes()
            }
        }
        bytes::Bytes::from_owner(Owner(self))
    }
}
#[async_trait]
pub trait Session: Send + Sync {
    fn initialization_scope(&self) -> Option<ownership::Initialization> {
        None
    }
    #[must_use = "the shutdown future must be awaited to perform the operation"]
    async fn shutdown(&self, _deadline: Instant) -> Result<(), Failure> {
        // Unsupported shutdown cannot be mistaken for observed cleanup.
        Err(Failure::AdapterInvariant)
    }
    /// Acquire before owned projection. Supervised sessions override explicitly.
    fn admit_request(&self) -> Result<controller::Admission, Failure> {
        Ok(controller::Admission::legacy())
    }
    #[must_use = "the request_admitted future must be awaited to perform the operation"]
    async fn request_admitted(
        &self,
        event: Value,
        deadline: Instant,
        admission: controller::Admission,
    ) -> Result<Response, Failure> {
        if !admission.is_legacy() {
            return Err(Failure::AdapterInvariant);
        }
        self.request(event, deadline).await
    }
    fn metadata_json(&self) -> Option<&str> {
        None
    }
    fn reserve_request(&self, _bytes: usize) -> Result<Option<controller::Reservation>, Failure> {
        Ok(None)
    }
    fn reserve_metadata(
        &self,
        _bytes: &[u8],
    ) -> Result<Option<controller::MetadataReservation>, Failure> {
        Ok(None)
    }
    /// Opt-in ceilings must never silently fall back to unrestricted metadata.
    fn reserve_metadata_limited(
        &self,
        _bytes: &[u8],
        _limits: accounting::MetadataLimits,
    ) -> Result<controller::MetadataReservation, Failure> {
        Err(Failure::AdapterInvariant)
    }
    #[must_use = "the request future must be awaited to perform the operation"]
    async fn request(&self, event: Value, deadline: Instant) -> Result<Response, Failure>;
}
#[async_trait]
pub trait Engine: Send + Sync {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        None
    }
    /// Trusted bounded observation only; no default activation or error mapping.
    #[must_use = "the initialize_owned_diagnosed future must be awaited to perform the operation"]
    async fn initialize_owned_diagnosed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        _diagnostic: Arc<crate::validator_init_diagnostic::InitDiagnostic>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_owned(source, format, settings, deadline, context)
            .await
    }
    #[must_use = "the initialize_owned future must be awaited to perform the operation"]
    async fn initialize_owned(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        _context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_shared(source, format, settings, deadline)
            .await
    }
    #[must_use = "the initialize_shared future must be awaited to perform the operation"]
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize(source, format, (*settings).clone(), deadline)
            .await
    }
    /// Eager complete-catalogue preparation; request handlers never compile.
    #[must_use = "the initialize future must be awaited to perform the operation"]
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure>;
}

/// Staged connector to the existing v6 controller/precision IPC protocol.
/// Constructed by trusted code only; never from an event or environment escape.
/// Production adoption of its limits, bundle and failure policy remains gated.
pub struct ControllerEngine {
    pub executable: PathBuf,
}
struct ControllerSession {
    controller: controller::Controller,
    ready: Arc<controller::Retained<controller::Ready>>,
}
#[async_trait]
impl Engine for ControllerEngine {
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        initialize_controller(
            &self.executable,
            source,
            format,
            settings,
            controller::ResourceProfile::default(),
            deadline,
        )
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
impl Drop for ControllerSession {
    fn drop(&mut self) {
        self.controller.close_supervised();
    }
}
#[async_trait]
impl Session for ControllerSession {
    fn initialization_scope(&self) -> Option<ownership::Initialization> {
        self.controller.initialization_scope()
    }
    async fn shutdown(&self, deadline: Instant) -> Result<(), Failure> {
        self.controller.shutdown(deadline).await.map(|_| ())
    }
    fn admit_request(&self) -> Result<controller::Admission, Failure> {
        self.controller.admit_request()
    }
    async fn request_admitted(
        &self,
        event: Value,
        deadline: Instant,
        admission: controller::Admission,
    ) -> Result<Response, Failure> {
        self.controller
            .request_admitted(event, deadline, admission)
            .await
            .map(Response::Retained)
    }
    fn metadata_json(&self) -> Option<&str> {
        Some(self.ready.metadata_json())
    }
    fn reserve_request(&self, bytes: usize) -> Result<Option<controller::Reservation>, Failure> {
        self.controller.budget().reservation(bytes).map(Some)
    }
    fn reserve_metadata(
        &self,
        bytes: &[u8],
    ) -> Result<Option<controller::MetadataReservation>, Failure> {
        accounting::metadata(&self.controller.budget(), bytes).map(Some)
    }
    fn reserve_metadata_limited(
        &self,
        bytes: &[u8],
        limits: accounting::MetadataLimits,
    ) -> Result<controller::MetadataReservation, Failure> {
        accounting::metadata_limited(&self.controller.budget(), bytes, limits)
    }
    async fn request(&self, event: Value, deadline: Instant) -> Result<Response, Failure> {
        self.controller
            .request(event, deadline)
            .await
            .map(Response::Retained)
    }
}
pub struct ProfileEngine {
    pub executable: PathBuf,
    pub profile: controller::ResourceProfile,
}
#[async_trait]
impl Engine for ProfileEngine {
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
        initialize_controller(
            &self.executable,
            source,
            format,
            settings,
            self.profile,
            deadline,
        )
        .await
    }
}
async fn initialize_controller(
    path: &std::path::Path,
    source: Arc<[u8]>,
    format: SourceFormat,
    settings: Arc<Settings>,
    profile: controller::ResourceProfile,
    deadline: Instant,
) -> Result<Arc<dyn Session>, Failure> {
    let format = match format {
        SourceFormat::Json => controller::SourceFormat::Json,
        SourceFormat::Yaml => controller::SourceFormat::Yaml,
    };
    let controller = controller::Controller::prepare(path, &source, format, settings, profile)?;
    controller.recover(deadline).await?;
    let ready = controller.actor().await?.ready;
    Ok(Arc::new(ControllerSession { controller, ready }))
}

/// Staged supervision primitives; no controller or application activation.
#[path = "validator_ipc/supervision.rs"]
pub mod supervision;

/// Trusted qualification composition only. Default/layer selectors are unchanged.
pub struct SupervisedStaticEngine {
    pub executable: crate::bundle_snapshot::Pin,
    pub policy: supervision::Policy,
}
#[async_trait]
impl Engine for SupervisedStaticEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        Some(crate::init::PreparationGate::process())
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
            EngineObservation {
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
            EngineObservation::default(),
        )
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
    async fn initialize_shared(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_observed(
            source,
            format,
            settings,
            deadline,
            None,
            EngineObservation::default(),
        )
        .await
    }
}

#[derive(Default)]
pub(super) struct EngineObservation {
    pub lifecycle: Option<Arc<lifecycle::Lifecycle>>,
    pub diagnostic: Option<Arc<crate::validator_init_diagnostic::InitDiagnostic>>,
}
impl SupervisedStaticEngine {
    pub(super) async fn initialize_observed(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        observation: EngineObservation,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.initialize_selected(
            source,
            format,
            settings,
            deadline,
            context,
            observation,
            None,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn initialize_selected(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
        observation: EngineObservation,
        readonly: Option<ReadonlyProfile>,
    ) -> Result<Arc<dyn Session>, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        let format = match format {
            SourceFormat::Json => controller::SourceFormat::Json,
            SourceFormat::Yaml => controller::SourceFormat::Yaml,
        };
        let controls = (
            supervision::Authority::process(),
            ownership::Drain::process(),
            context,
            observation.diagnostic.clone(),
        );
        let (controller, scope) = if let Some(profile) = readonly {
            controller::Controller::prepare_supervised_async_selected(
                &self.executable,
                source,
                format,
                settings,
                self.policy,
                deadline,
                controls,
                Some(profile),
            )
            .await?
        } else {
            controller::Controller::prepare_supervised_async_observed(
                &self.executable,
                source,
                format,
                settings,
                self.policy,
                deadline,
                controls,
            )
            .await?
        };
        // Isolated observation only, before the actual child can start.
        if let Some(lifecycle) = observation.lifecycle {
            *controller
                .stats
                .lifecycle
                .lock()
                .map_err(|_| Failure::AdapterInvariant)? = Some(lifecycle);
        }
        let result = async {
            controller.recover(deadline).await?;
            let ready = controller.actor().await?.ready;
            Ok::<Arc<dyn Session>, Failure>(Arc::new(ControllerSession { controller, ready }))
        }
        .await;
        match result {
            Ok(session) => scope.publish(deadline, || session).map_err(|failure| {
                if matches!(failure, Failure::NoReady)
                    && let Some(d) = &observation.diagnostic
                {
                    d.note(
                        crate::validator_init_diagnostic::Stage::ControllerPublish,
                        Some(&failure),
                    );
                }
                failure
            }),
            Err(error) => {
                // Keep the primary typed error. Drain outcome remains queryable
                // via shutdown/snapshot; never describe an incomplete drain green.
                let _cleanup = scope.shutdown(deadline).await;
                Err(error)
            }
        }
    }
    /// Explicit owner-facing drain; no cleanup is claimed on Err.
    pub async fn shutdown(&self, deadline: Instant) -> Result<ownership::Snapshot, Failure> {
        ownership::Drain::process().shutdown(deadline).await
    }
}
