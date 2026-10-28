//! Explicit in-process precision profile. No subprocess, IPC, network retrieval,
//! global allocator, panic hook or worker-bundle admission. Requested buffer
//! accounting is not a bound on compiler heap, RSS or synchronous CPU work.
#[path = "in_process_specification.rs"]
mod specification;
use super::{Engine, Failure, Response, Session, Settings, accounting, controller, ownership};
use async_trait::async_trait;
use gateway_policy::openapi_operation::SourceFormat;
pub use specification::SpecificationProvider;
use std::sync::Arc;
use tokio::time::Instant;

#[derive(Clone, Copy)]
pub struct Limits {
    pub buffer_bytes: usize,
    pub body_depth: usize,
    pub metadata: accounting::MetadataLimits,
}
impl Limits {
    fn validate(self) -> Result<(), Failure> {
        if self.buffer_bytes < 8 << 20 || self.body_depth == 0 || self.body_depth > 512 {
            return Err(Failure::AdapterInvariant);
        }
        self.metadata.validate(self.buffer_bytes)
    }
}
pub struct InProcessEngine {
    limits: Limits,
    preparation: Arc<crate::init::PreparationGate>,
    compile_lane: Arc<tokio::sync::Semaphore>,
}
impl InProcessEngine {
    pub fn new(limits: Limits) -> Result<Self, Failure> {
        limits.validate()?;
        Ok(Self {
            limits,
            preparation: crate::init::PreparationGate::new(),
            compile_lane: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
}
struct CompiledResources {
    value: light_lambda_validator::Compiled,
    _input_credit: controller::Reservation,
}
struct InProcessSession {
    compiled: Arc<CompiledResources>,
    drain: Arc<ownership::Drain>,
    budget: Arc<controller::Budget>,
    limits: Limits,
}
fn map(failure: light_lambda_validator::Failure) -> Failure {
    match failure {
        light_lambda_validator::Failure::Resource => Failure::EngineResource,
        light_lambda_validator::Failure::Internal => Failure::EngineInternal,
        light_lambda_validator::Failure::InvalidSource => Failure::Protocol,
        light_lambda_validator::Failure::InvalidEvent => Failure::AdapterInvariant,
    }
}
#[async_trait]
impl Engine for InProcessEngine {
    fn configuration_preparation(&self) -> Option<Arc<crate::init::PreparationGate>> {
        Some(self.preparation.clone())
    }
    async fn initialize_owned(
        &self,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<Settings>,
        deadline: Instant,
        context: Option<Arc<crate::init::PreparationContext>>,
    ) -> Result<Arc<dyn Session>, Failure> {
        if source.len() > 1 << 20 {
            return Err(Failure::Frame);
        }
        let compile_permit = self
            .compile_lane
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::NoReady)?;
        let drain = ownership::Drain::new();
        if let Some(context) = context {
            context
                .register_drain(drain.clone())
                .map_err(|_| Failure::NoReady)?;
        }
        let scope = ownership::Initialization::new(drain.clone());
        let ticket = drain.capture()?;
        let budget = controller::Budget::new(self.limits.buffer_bytes);
        let settings_credit = budget.reservation(accounting::settings_bound(&settings)?)?;
        let settings = serde_json::to_vec(&*settings).map_err(|_| Failure::AdapterInvariant)?;
        let source_credit = budget.reservation(source.len())?;
        let limits = self.limits;
        let result = ownership::capture(ticket, deadline, move || {
            let _compile_permit = compile_permit;
            let _settings_credit = settings_credit;
            let compiled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                light_lambda_validator::Compiled::compile(
                    &source,
                    matches!(format, SourceFormat::Yaml),
                    &settings,
                )
            }))
            .map_err(|_| Failure::EngineInternal)?
            .map_err(map)?;
            // Retained input/metadata estimates only; compiled engine allocations
            // share the gateway allocator and are not represented as RSS bounds.
            let input_credit = budget.reservation(compiled.accounted_input_bytes())?;
            drop(source_credit);
            Ok((
                Arc::new(CompiledResources {
                    value: compiled,
                    _input_credit: input_credit,
                }),
                budget,
            ))
        })
        .await?;
        scope.publish(deadline, || {
            Arc::new(InProcessSession {
                compiled: result.0,
                budget: result.1,
                drain,
                limits,
            }) as Arc<dyn Session>
        })
    }
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
}
#[async_trait]
impl Session for InProcessSession {
    fn initialization_scope(&self) -> Option<ownership::Initialization> {
        Some(ownership::Initialization::new(self.drain.clone()))
    }
    async fn shutdown(&self, deadline: Instant) -> Result<(), Failure> {
        self.drain.shutdown(deadline).await.map(|_| ())
    }
    fn admit_request(&self) -> Result<controller::Admission, Failure> {
        if self.drain.is_closed() || self.drain.snapshot().captures != 0 {
            return Err(Failure::NoReady);
        }
        Ok(controller::Admission::legacy())
    }
    fn metadata_json(&self) -> Option<&str> {
        Some(self.compiled.value.metadata_json())
    }
    fn reserve_request(&self, bytes: usize) -> Result<Option<controller::Reservation>, Failure> {
        self.budget.reservation(bytes).map(Some)
    }
    fn reserve_metadata(
        &self,
        bytes: &[u8],
    ) -> Result<Option<controller::MetadataReservation>, Failure> {
        self.reserve_metadata_limited(bytes, self.limits.metadata)
            .map(Some)
    }
    fn reserve_metadata_limited(
        &self,
        bytes: &[u8],
        limits: accounting::MetadataLimits,
    ) -> Result<controller::MetadataReservation, Failure> {
        accounting::metadata_limited(&self.budget, bytes, limits)
    }
    async fn request(
        &self,
        event: serde_json::Value,
        deadline: Instant,
    ) -> Result<Response, Failure> {
        // A single capture ticket bounds queued + active blocking jobs. It lives
        // in the actual closure, including after timeout or caller cancellation.
        let ticket = self.drain.capture()?;
        let event_credit = controller::budget::retain_event(&self.budget, &event)?;
        let wire_credit = self.budget.reservation(4 << 20)?;
        let compiled = self.compiled.clone();
        let budget = self.budget.clone();
        let depth = self.limits.body_depth;
        ownership::capture(ticket, deadline, move || {
            let _event_credit = event_credit;
            let _wire_credit = wire_credit;
            let wire = serde_json::to_vec(&event).map_err(|_| Failure::Frame)?;
            if wire.len() > 4 << 20 {
                return Err(Failure::Frame);
            }
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                compiled.value.validate(&wire, depth)
            }))
            .map_err(|_| Failure::EngineInternal)?
            .map_err(map)?;
            let credit = budget.reserve(
                result
                    .body
                    .len()
                    .checked_mul(2)
                    .and_then(|n| n.checked_add(4096))
                    .ok_or(Failure::ParentBudget)?,
            )?;
            let reply = controller::Reply {
                status: result.status,
                body: result.body,
                violation: result.violation.map(|v| controller::Violation {
                    code: v.code,
                    message: v.message,
                    description: v.description,
                }),
            };
            Ok(Response::Retained(controller::Retained {
                value: reply,
                _credit: credit,
            }))
        })
        .await
    }
}
impl Drop for InProcessSession {
    fn drop(&mut self) {
        self.drain.close();
    }
}

/// Same approved status/code categories as the worker profile, with a truthful
/// in-process unavailable diagnostic. No parser, source or panic text is exposed.
pub struct InProcessFailureDisposition;
impl crate::validator_policy::FailureDisposition for InProcessFailureDisposition {
    fn reject(&self, failure: Failure) -> serverless_core::StatusError {
        match failure {
            Failure::Deadline | Failure::Cancelled => serverless_core::StatusError::timeout(),
            Failure::Protocol | Failure::EngineInternal | Failure::AdapterInvariant => {
                serverless_core::StatusError::handler_failure()
            }
            _ => serverless_core::StatusError::new(
                503,
                "ERR14034",
                "VALIDATOR_UNAVAILABLE",
                "Validator is unavailable.",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;
    const SOURCE: &[u8] = br#"{"openapi":"3.1.0","paths":{"/pets":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object","required":["name"]}}}}}}}}"#;
    fn limits() -> Limits {
        Limits {
            buffer_bytes: 64 << 20,
            body_depth: 128,
            metadata: accounting::MetadataLimits {
                max_wire_bytes: 2 << 20,
                max_peak_bytes: 32 << 20,
                max_retained_bytes: 16 << 20,
            },
        }
    }
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    fn event(body: &str) -> serde_json::Value {
        json!({"path":"/pets","httpMethod":"POST","headers":{"Content-Type":"application/json"},"body":body})
    }
    async fn session() -> Arc<dyn Session> {
        InProcessEngine::new(limits())
            .unwrap()
            .initialize(
                Arc::from(SOURCE),
                SourceFormat::Json,
                Settings::default(),
                later(),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn compiled_session_reuses_immutable_catalogue_and_shutdown_closes_admission() {
        let s = session().await;
        assert!(s.metadata_json().unwrap().contains("/pets"));
        for (body, status) in [
            (r#"{"name":"first"}"#, 200),
            ("{}", 400),
            (r#"{"name":"warm"}"#, 200),
        ] {
            assert_eq!(
                s.request(event(body), later()).await.unwrap().status,
                status
            );
        }
        s.shutdown(later()).await.unwrap();
        assert!(matches!(s.admit_request(), Err(Failure::NoReady)));
        assert!(matches!(
            s.request(event("{}"), later()).await,
            Err(Failure::NoReady)
        ));
    }
    #[tokio::test]
    async fn bounded_buffers_depth_and_closed_failure_codes() {
        let s = session().await;
        assert!(matches!(
            s.reserve_request(65 << 20),
            Err(Failure::ParentBudget)
        ));
        let guards = s.reserve_metadata(b"{}").unwrap().unwrap();
        assert!(guards.retained.bytes() > 0);
        drop(guards);
        let deep = format!("{}0{}", "[".repeat(129), "]".repeat(129));
        assert!(matches!(
            s.request(event(&deep), later()).await,
            Err(Failure::EngineResource)
        ));
        // Infrastructure failure closes the profile; ordinary schema-invalid
        // requests above remain recoverable. No automatic compiler/retry loop.
        assert!(matches!(s.admit_request(), Err(Failure::NoReady)));
    }
    #[tokio::test]
    async fn expired_initialization_never_publishes_and_invalid_limits_reject() {
        let engine = InProcessEngine::new(limits()).unwrap();
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(SOURCE),
                    SourceFormat::Json,
                    Settings::default(),
                    Instant::now()
                )
                .await,
            Err(Failure::Deadline)
        ));
        let mut bad = limits();
        bad.body_depth = 513;
        assert!(matches!(
            InProcessEngine::new(bad),
            Err(Failure::AdapterInvariant)
        ));
    }
    #[tokio::test]
    async fn compile_admission_is_bounded_before_any_second_job_is_queued() {
        let engine = InProcessEngine::new(limits()).unwrap();
        let held = engine.compile_lane.clone().try_acquire_owned().unwrap();
        assert!(matches!(
            engine
                .initialize(
                    Arc::from(SOURCE),
                    SourceFormat::Json,
                    Settings::default(),
                    later()
                )
                .await,
            Err(Failure::NoReady)
        ));
        drop(held);
        engine
            .initialize(
                Arc::from(SOURCE),
                SourceFormat::Json,
                Settings::default(),
                later(),
            )
            .await
            .unwrap();
    }
    #[test]
    fn cancelled_queued_validation_stays_owned_until_actual_job_finishes() {
        // Occupy the sole blocking lane after real compilation. Timeout must not
        // refund the queued validation ticket or claim that shutdown drained it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        rt.block_on(async {
            let s = session().await;
            let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            struct Release(Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>);
            impl Drop for Release {
                fn drop(&mut self) {
                    let (lock, notify) = &*self.0;
                    *lock.lock().unwrap() = true;
                    notify.notify_all();
                }
            }
            let release = Release(gate.clone());
            let entered = Arc::new(tokio::sync::Notify::new());
            let signal = entered.clone();
            let blocker = tokio::task::spawn_blocking(move || {
                signal.notify_one();
                let (lock, notify) = &*gate;
                let mut ready = lock.lock().unwrap();
                while !*ready {
                    ready = notify.wait(ready).unwrap();
                }
            });
            entered.notified().await;
            assert!(matches!(
                s.request(event("{}"), Instant::now() + Duration::from_millis(30))
                    .await,
                Err(Failure::Deadline)
            ));
            assert!(matches!(
                s.shutdown(Instant::now()).await,
                Err(Failure::Deadline)
            ));
            assert!(matches!(s.admit_request(), Err(Failure::NoReady)));
            drop(release);
            blocker.await.unwrap();
            s.shutdown(later()).await.unwrap();
        });
    }
    #[test]
    fn public_failure_policy_never_exports_internal_details_or_worker_claims() {
        use crate::validator_policy::FailureDisposition;
        let disposition = InProcessFailureDisposition;
        assert_eq!(disposition.reject(Failure::EngineResource).code, "ERR14034");
        assert_eq!(disposition.reject(Failure::EngineInternal).code, "ERR14004");
        assert_eq!(disposition.reject(Failure::Deadline).code, "ERR14017");
        assert_eq!(
            disposition.reject(Failure::NoReady).message,
            "VALIDATOR_UNAVAILABLE"
        );
    }
}
