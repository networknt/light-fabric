//! Application factory and async lifecycle. Production engine/profile/error
//! admission stays closed; dependency-injected tests qualify wiring separately.
use crate::validator_init_diagnostic::{InitDiagnostic, Stage};
use crate::{
    init::{
        HandlerFactoryProvider, PreparedPrecision, PreparedSpecificationRuntime,
        PreparedValidatorResources, RuntimeLifecycle, SpecificationResources,
    },
    init_error::InitError,
    validator_ipc::{Engine, Failure, Session, Settings},
};
use async_trait::async_trait;
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory, RequestCapabilities, ValidatorMediaRole},
    *,
};
use std::{sync::Arc, time::Duration};
use tokio::{sync::OnceCell, time::Instant};

/// Explicit trusted disposition is required for infrastructure failures. There
/// is no blanket 500/503 mapping or reserved new status in this provider.
pub trait FailureDisposition: Send + Sync {
    fn reject(&self, failure: Failure) -> StatusError;
}
struct Wiring {
    engine: Arc<dyn Engine>,
    failures: Arc<dyn FailureDisposition>,
    init_timeout: Duration,
}
#[derive(Default)]
pub struct ValidatorPolicyProvider {
    diagnostic: Option<Arc<InitDiagnostic>>,
    wiring: Option<Arc<Wiring>>,
    required_profile: Option<Arc<crate::required_validator_profile::RequiredValidatorProfile>>,
}
impl ValidatorPolicyProvider {
    /// Trusted composition only. This does not admit an application-configured
    /// engine, experimental limits, failure policy or deployment bundle.
    pub fn with_engine(
        engine: Arc<dyn Engine>,
        failures: Arc<dyn FailureDisposition>,
        init_timeout: Duration,
    ) -> Self {
        Self {
            diagnostic: None,
            required_profile: None,
            wiring: Some(Arc::new(Wiring {
                engine,
                failures,
                init_timeout,
            })),
        }
    }
}
impl ValidatorPolicyProvider {
    /// Optional trusted startup observation, absent from ordinary composition.
    pub fn with_init_diagnostic(mut self, diagnostic: Arc<InitDiagnostic>) -> Self {
        self.diagnostic = Some(diagnostic);
        self
    }
    fn diagnosed(&self, stage: Stage, failure: Option<&Failure>) -> InitError {
        if let Some(diagnostic) = &self.diagnostic {
            diagnostic.note(stage, failure);
        }
        invalid()
    }
    /// Explicit trusted profile only; with_engine and Default retain no guard.
    pub fn with_required_profile(
        mut self,
        profile: Arc<crate::required_validator_profile::RequiredValidatorProfile>,
    ) -> Self {
        self.required_profile = Some(profile);
        self
    }
}

fn unavailable() -> InitError {
    // Preserve existing fixture admission until the engine/profile is admitted.
    InitError::InvalidConfiguration("configuration: enabled handler not supported until WP12")
}
fn invalid() -> InitError {
    InitError::InvalidConfiguration(
        "validator: captured resources or supported engine settings required",
    )
}
struct Prepared {
    context: Option<Arc<crate::init::PreparationContext>>,
    settings: Arc<Settings>,
    resources: Arc<SpecificationResources>,
    wiring: Arc<Wiring>,
    session: Arc<OnceCell<Arc<dyn Session>>>,
}
impl HandlerFactoryProvider for ValidatorPolicyProvider {
    fn required_execution_profile(
        &self,
    ) -> Option<&crate::required_validator_profile::RequiredValidatorProfile> {
        self.required_profile.as_deref()
    }
    fn prepare_specification_resources(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<crate::init::PreparedSpecificationResources, InitError> {
        Err(InitError::InvalidConfiguration(
            "validator: precision metadata requires runtime initialization; offline validation does not start an engine",
        ))
    }
    fn runtime_preparation_profile(&self) -> Option<crate::init::PreparationProfile> {
        let wiring = self.wiring.as_ref()?;
        Some(crate::init::PreparationProfile {
            gate: wiring.engine.configuration_preparation()?,
            timeout: wiring.init_timeout,
        })
    }
    fn prepare_specification_runtime<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        _security: Option<Arc<SecurityRuntime>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PreparedSpecificationRuntime, InitError>>
                + Send
                + 'a,
        >,
    > {
        self.prepare_precision(config, None)
    }
    fn prepare_specification_runtime_owned<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        _security: Option<Arc<SecurityRuntime>>,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PreparedSpecificationRuntime, InitError>>
                + Send
                + 'a,
        >,
    > {
        self.prepare_precision(config, context)
    }
    fn prepare_validator_runtime_owned(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        precision: Option<Arc<crate::init::PreparedPrecision>>,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        // Prepare through the existing contract, then attach the shared context
        // to an otherwise identical factory/lifecycle.
        self.prepare_validator_context(config, security, resources, precision, context)
    }
    fn prepare_validator_runtime(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        precision: Option<Arc<crate::init::PreparedPrecision>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        self.prepare_validator_context(config, security, resources, precision, None)
    }
    fn application_available(&self) -> bool {
        false
    }
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Err(self.diagnosed(Stage::FactoryCreate, None))
    }
    fn prepare_validator_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        self.prepare_validator_resources_context(config, security, resources, None)
    }
}
impl ValidatorPolicyProvider {
    fn prepare_precision<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<PreparedSpecificationRuntime, InitError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let wiring = self.wiring.clone().ok_or_else(unavailable)?;
            let deadline = context.as_ref().map_or_else(
                || {
                    Instant::now()
                        .checked_add(wiring.init_timeout)
                        .ok_or_else(|| self.diagnosed(Stage::DeadlineCreation, None))
                },
                |context| Ok(context.deadline()),
            )?;
            if let Some(context) = &context {
                context.check()?;
            }
            let value: Value = config
                .module_registry
                .load_config(config, "openapi-validator.yml")
                .map_err(|_| self.diagnosed(Stage::PrecisionSettingsLoad, None))?;
            let settings = settings(&value).inspect_err(|_| {
                let _ = self.diagnosed(Stage::PrecisionSettingsParse, None);
            })?;
            let capture = crate::specification_policy::SpecificationPolicyProvider
                .capture_projection(config)?;
            if let Some(context) = &context {
                context.check()?;
            }
            let session = tokio::time::timeout_at(deadline, async {
                match &self.diagnostic {
                    Some(diagnostic) => {
                        wiring
                            .engine
                            .initialize_owned_diagnosed(
                                capture.source.clone(),
                                capture.format,
                                settings.clone(),
                                deadline,
                                context.clone(),
                                diagnostic.clone(),
                            )
                            .await
                    }
                    None => {
                        wiring
                            .engine
                            .initialize_owned(
                                capture.source.clone(),
                                capture.format,
                                settings.clone(),
                                deadline,
                                context.clone(),
                            )
                            .await
                    }
                }
            })
            .await
            .map_err(|_| self.diagnosed(Stage::EngineTimeout, None))?
            .map_err(|error| self.diagnosed(Stage::EngineInitialize, Some(&error)))?;
            struct Owner {
                _session: Arc<dyn Session>,
                _credit: Option<crate::validator_ipc::controller::Reservation>,
            }
            if let Some(context) = &context
                && let Some(scope) = session.initialization_scope()
            {
                context.register_scope(scope)?;
            }
            let mut scope = session.initialization_scope();
            let result = async {
                let metadata_work = scope
                    .as_ref()
                    .map(|s| s.metadata())
                    .transpose()
                    .map_err(|error| self.diagnosed(Stage::MetadataScope, Some(&error)))?;
                let owned = session.clone();
                let diagnostic = self.diagnostic.clone();
                let job = move || {
                    let diagnosed = |stage, failure: Option<&Failure>| {
                        if let Some(diagnostic) = &diagnostic {
                            diagnostic.note(stage, failure);
                        }
                        invalid()
                    };
                    let metadata = owned
                        .metadata_json()
                        .ok_or_else(|| diagnosed(Stage::MetadataMissing, None))?;
                    let reservations = owned
                        .reserve_metadata(metadata.as_bytes())
                        .map_err(|error| diagnosed(Stage::MetadataReservation, Some(&error)))?;
                    let specification = capture.finish(metadata.as_bytes())?;
                    let retained = reservations.map(|r| {
                        drop(r.scratch);
                        r.retained
                    });
                    Ok::<_, InitError>((
                        specification,
                        Arc::new(Owner {
                            _session: owned,
                            _credit: retained,
                        }),
                    ))
                };
                let task_credit = session
                    .reserve_request(std::mem::size_of_val(&job) + 4096)
                    .map_err(|error| {
                        self.diagnosed(Stage::MetadataTaskReservation, Some(&error))
                    })?;
                let task = tokio::task::spawn_blocking(move || {
                    let _credit = task_credit;
                    let _work = metadata_work;
                    job()
                });
                let (specification, owner) = tokio::time::timeout_at(deadline, task)
                    .await
                    .map_err(|_| self.diagnosed(Stage::MetadataTaskTimeout, None))?
                    .map_err(|_| self.diagnosed(Stage::MetadataTaskJoin, None))??;
                if Instant::now() >= deadline {
                    return Err(self.diagnosed(Stage::MetadataDeadline, None));
                }
                Ok(PreparedSpecificationRuntime {
                    specification,
                    precision: Some(Arc::new(PreparedPrecision { session, settings })),
                    owners: vec![owner],
                })
            }
            .await;
            match result {
                Ok(prepared) => match scope.take() {
                    Some(scope) => scope
                        .publish(deadline, || prepared)
                        .map_err(|error| self.diagnosed(Stage::MetadataPublish, Some(&error))),
                    None => Ok(prepared),
                },
                Err(error) => {
                    if let Some(scope) = scope.take() {
                        let _cleanup = scope.shutdown(deadline).await;
                    }
                    Err(error)
                }
            }
        })
    }

    fn prepare_validator_context(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        precision: Option<Arc<crate::init::PreparedPrecision>>,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        let Some(precision) = precision else {
            return self.prepare_validator_resources_context(config, security, resources, context);
        };
        let wiring = self.wiring.clone().ok_or_else(unavailable)?;
        let resources = resources.ok_or_else(|| self.diagnosed(Stage::ResourcesMissing, None))?;
        let slot = OnceCell::new();
        slot.set(precision.session.clone())
            .map_err(|_| self.diagnosed(Stage::SessionSlot, None))?;
        let prepared = Arc::new(Prepared {
            context,
            settings: precision.settings.clone(),
            resources,
            wiring,
            session: Arc::new(slot),
        });
        Ok(PreparedValidatorResources {
            factory: prepared,
            lifecycle: None,
        })
    }

    fn prepare_validator_resources_context(
        &self,
        config: &RuntimeConfig,
        _security: Option<Arc<SecurityRuntime>>,
        resources: Option<Arc<SpecificationResources>>,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<PreparedValidatorResources, InitError> {
        let wiring = self.wiring.clone().ok_or_else(unavailable)?;
        let value: Value = config
            .module_registry
            .load_config(config, "openapi-validator.yml")
            .map_err(|_| self.diagnosed(Stage::ResourceSettingsLoad, None))?;
        let settings = settings(&value).inspect_err(|_| {
            let _ = self.diagnosed(Stage::ResourceSettingsParse, None);
        })?;
        let resources = resources.ok_or_else(|| self.diagnosed(Stage::ResourcesMissing, None))?;
        resources
            .catalogue
            .source_resource()
            .ok_or_else(|| self.diagnosed(Stage::ResourceCaptureMissing, None))?;
        let prepared = Arc::new(Prepared {
            context,
            settings,
            resources,
            wiring,
            session: Arc::new(OnceCell::new()),
        });
        Ok(PreparedValidatorResources {
            factory: prepared.clone(),
            lifecycle: Some(prepared),
        })
    }
}

#[async_trait]
impl RuntimeLifecycle for Prepared {
    async fn activate(&self) -> Result<(), InitError> {
        let (format, source) = self
            .resources
            .catalogue
            .source_resource()
            .ok_or_else(invalid)?;
        let deadline = self.context.as_ref().map_or_else(
            || {
                Instant::now()
                    .checked_add(self.wiring.init_timeout)
                    .ok_or_else(invalid)
            },
            |context| Ok(context.deadline()),
        )?;
        if let Some(context) = &self.context {
            context.check()?;
        }
        self.session
            .get_or_try_init(|| async {
                tokio::time::timeout_at(
                    deadline,
                    self.wiring.engine.initialize_owned(
                        source,
                        format,
                        self.settings.clone(),
                        deadline,
                        self.context.clone(),
                    ),
                )
                .await
                .map_err(|_| {
                    InitError::InvalidConfiguration("validator: precision engine activation failed")
                })?
                .map_err(|_| {
                    InitError::InvalidConfiguration("validator: precision engine activation failed")
                })
            })
            .await?;
        if let (Some(context), Some(session)) = (&self.context, self.session.get())
            && let Some(scope) = session.initialization_scope()
        {
            context.register_scope(scope)?;
        }
        Ok(())
    }
}
impl HandlerFactory for Prepared {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        ValidatorMediaRole::Requires
    }
    fn build(&self, _: &compat::Compatibility, _: &str) -> Result<Binding, plan::InitError> {
        // Each occurrence shares the same Init-owned prepared engine/session.
        Ok(Binding::Request(
            Arc::new(Handler {
                resources: self.resources.clone(),
                session: self.session.clone(),
                failures: self.wiring.failures.clone(),
            }),
            RequestCapabilities::default(),
        ))
    }
}
// The plan is compiled before activation. Bind the shared prepared slot, not a
// snapshot of its pre-activation value.
struct Handler {
    resources: Arc<SpecificationResources>,
    session: Arc<OnceCell<Arc<dyn Session>>>,
    failures: Arc<dyn FailureDisposition>,
}
#[async_trait]
impl RequestHandler for Handler {
    fn id(&self) -> HandlerId {
        HandlerId::Validator
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        OperationAuthorizationRole::MetadataConsumer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let operation = state.operation.as_ref().ok_or(HandlerFailure)?;
        let expected = self
            .resources
            .catalogue
            .operation(operation.path_template(), inv.method.as_str())
            .ok_or(HandlerFailure)?;
        if !Arc::ptr_eq(operation, &expected) {
            return Err(HandlerFailure);
        }
        let exact_media = state.validator_content_type()?;
        let session = self.session.get().ok_or(HandlerFailure)?;
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Ok(HandlerOutcome::Reject(StatusError::timeout()));
        }
        if inv.path.len() > gateway_policy::openapi_operation::MAX_NORMALIZATION_PATH_BYTES {
            return Ok(HandlerOutcome::Reject(
                StatusError::specification_path_too_large(),
            ));
        }
        let admission = match session.admit_request() {
            Ok(admission) => admission,
            Err(failure) => return Ok(HandlerOutcome::Reject(self.failures.reject(failure))),
        };
        let _admission_lifetime = admission.hold();
        let mut bytes = inv
            .body
            .as_ref()
            .map_or(0, |b| b.len())
            .checked_add(inv.path.len())
            // The captured profile can add one leading slash. Reserve its
            // separate owned output before normalization; never mutate ingress.
            .and_then(|n| n.checked_add(inv.path.len().checked_add(1)?))
            .ok_or(HandlerFailure)?;
        let entries = inv
            .headers
            .len()
            .checked_add(inv.query.0.len())
            .and_then(|n| n.checked_add(inv.path_params.len()))
            .ok_or(HandlerFailure)?;
        for (k, v) in &inv.query.0 {
            bytes = bytes
                .checked_add(k.len())
                .and_then(|n| n.checked_add(v.len()))
                .ok_or(HandlerFailure)?;
        }
        for (k, v) in &inv.path_params {
            bytes = bytes
                .checked_add(k.len())
                .and_then(|n| n.checked_add(v.len()))
                .ok_or(HandlerFailure)?;
        }
        for (k, v) in &inv.headers {
            bytes = bytes
                .checked_add(k.as_str().len())
                .and_then(|n| n.checked_add(v.len()))
                .ok_or(HandlerFailure)?;
        }
        let bound = crate::validator_ipc::accounting::request_bound(bytes, entries)
            .map_err(|_| HandlerFailure)?;
        let _reservation = match session.reserve_request(bound) {
            Ok(r) => r,
            Err(f) => return Ok(HandlerOutcome::Reject(self.failures.reject(f))),
        };
        // Operation keys live in the captured Specification profile's path
        // space. Only the IPC projection uses that path: forwarding retains
        // Invocation.path, including actual parameter values and base prefix.
        let path = self
            .resources
            .profile
            .normalize_bounded_path(&inv.path)
            .map_err(|_| HandlerFailure)?;
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Ok(HandlerOutcome::Reject(StatusError::timeout()));
        }
        let body = inv
            .body
            .as_ref()
            .map(|b| std::str::from_utf8(b).map(str::to_owned))
            .transpose()
            .map_err(|_| HandlerFailure)?;
        let mut headers = serde_json::Map::new();
        for name in inv.headers.keys() {
            let value = inv
                .headers
                .get(name)
                .ok_or(HandlerFailure)?
                .to_str()
                .map_err(|_| HandlerFailure)?;
            headers.insert(name.to_string(), Value::String(value.into()));
        }
        // The validator IPC profile uses canonical Content-Type spelling.
        // The trusted proof follows case-insensitive HTTP ingress and mutations.
        if let Some(value) = exact_media {
            headers.remove("content-type");
            headers.insert(
                "Content-Type".into(),
                json!(value.to_str().map_err(|_| HandlerFailure)?),
            );
        }
        let mut query = serde_json::Map::new();
        for (key, value) in &inv.query.0 {
            query.entry(key.clone()).or_insert_with(|| json!(value));
        }
        let event = json!({"path":path,"httpMethod":inv.method.as_str(),"headers":headers,"queryStringParameters":query,"pathParameters":inv.path_params,"body":body});
        let result = tokio::select! {biased;
            _=inv.cancel.cancelled()=>return Ok(HandlerOutcome::Reject(StatusError::timeout())),
            r=tokio::time::timeout_at(inv.deadline,session.request_admitted(event, inv.deadline, admission))=>r,
        };
        if inv.cancel.is_cancelled() || Instant::now() >= inv.deadline {
            return Ok(HandlerOutcome::Reject(StatusError::timeout()));
        }
        let reply = match result {
            Err(_) | Ok(Err(Failure::Deadline | Failure::Cancelled)) => {
                return Ok(HandlerOutcome::Reject(StatusError::timeout()));
            }
            Ok(Err(failure)) => return Ok(HandlerOutcome::Reject(self.failures.reject(failure))),
            Ok(Ok(reply)) => reply,
        };
        match reply.status {
            200 => Ok(HandlerOutcome::Continue),
            400 => {
                let mut headers = http::HeaderMap::new();
                headers.insert(
                    http::header::CONTENT_TYPE,
                    http::HeaderValue::from_static("application/json"),
                );
                Ok(HandlerOutcome::Respond(GatewayResponse {
                    status: http::StatusCode::BAD_REQUEST,
                    headers,
                    body: Some(reply.into_body()),
                    body_is_binary: false,
                }))
            }
            _ => Err(HandlerFailure),
        }
    }
}
fn settings(value: &Value) -> Result<Arc<Settings>, InitError> {
    let (s, _) =
        unsupported::remaining::ValidatorSettings::from_value(value).map_err(|_| invalid())?;
    if !s.enabled {
        return Err(invalid());
    }
    Ok(Arc::new(Settings {
        skip_body_validation: Some(s.skip_body_validation),
        handle_nullable_field: Some(s.handle_nullable_field),
        skip_path_prefixes: Some(s.skip_path_prefixes),
    }))
}
