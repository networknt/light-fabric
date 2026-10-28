//! Closed finite work executed by the real production blocking lease mechanism.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use light_lambda_runtime::init::{HandlerFactoryProvider, InitInput};
use light_runtime::RuntimeConfig;
use serde_json::{Value, json};
use serverless_core::{
    blocking::{BlockingError, BlockingExecutor, BoundedWork, WorkStep},
    plan::{Binding, HandlerFactory},
    *,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
pub(crate) struct Capture {
    pool: Arc<BlockingExecutor>,
    started: Arc<AtomicUsize>,
    completed: Arc<AtomicUsize>,
    late: Arc<AtomicUsize>,
}
impl Capture {
    fn snapshot(&self) -> Value {
        let started = self.started.load(Ordering::SeqCst);
        let completed = self.completed.load(Ordering::SeqCst);
        json!({"started":started,"completed":completed,"active":started-completed,"permitsHeld":1-serverless_core::blocking::conformance::available(&self.pool)})
    }
    pub async fn observation(&self) -> Result<Value> {
        let at_response = self.snapshot();
        let deadline = Instant::now() + Duration::from_secs(1);
        while serverless_core::blocking::conformance::available(&self.pool) != 1
            || self.completed.load(Ordering::SeqCst) != self.started.load(Ordering::SeqCst)
        {
            ensure!(Instant::now() < deadline, "finite work did not settle");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(
            json!({"capacity":1,"atResponse":at_response,"afterReturn":self.snapshot(),"lateResultsDiscarded":self.late.load(Ordering::SeqCst)}),
        )
    }
}
struct Work {
    capture: Arc<Capture>,
    deadline: Instant,
}
impl BoundedWork for Work {
    type Output = ();
    fn input_bytes(&self) -> usize {
        1
    }
    fn step(&mut self) -> Result<WorkStep<()>, BlockingError> {
        self.capture.started.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
        if Instant::now() >= self.deadline {
            self.capture.late.fetch_add(1, Ordering::SeqCst);
        }
        self.capture.completed.fetch_add(1, Ordering::SeqCst);
        Ok(WorkStep::Ready(()))
    }
}
struct Provider(Arc<Capture>);
struct Factory(Arc<Capture>);
struct Policy(Arc<Capture>);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, light_lambda_runtime::init_error::InitError> {
        Ok(Arc::new(Factory(self.0.clone())))
    }
}
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        Ok(Binding::Response(Arc::new(Policy(self.0.clone()))))
    }
}
#[async_trait]
impl ResponseHandler for Policy {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        state: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        let outcome = self
            .0
            .pool
            .execute(
                Work {
                    capture: self.0.clone(),
                    deadline: inv.response_deadline,
                },
                inv.response_deadline,
            )
            .await;
        if outcome.is_err() {
            let error = StatusError::timeout();
            *response = error.response();
            state.audit.status = Some(error);
        }
        Ok(())
    }
}
pub(crate) fn configure(f: &Value, config: &mut Value) -> Result<Option<Arc<Capture>>> {
    let Some(mode) = f.get("blockingLeasePrerequisite") else {
        return Ok(None);
    };
    ensure!(
        mode == "response-300ms-capacity1-v1" || mode == "response-300ms-capacity1-two-callers-v1",
        "closed lease profile"
    );
    ensure!(
        config["files"].get("header").is_none(),
        "lease probe cannot shadow header config"
    );
    let class = serverless_core::compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::ResponseHeader)
        .unwrap()
        .class;
    ensure!(
        !config["files"]["handler"]["handlers"]
            .as_array()
            .context("declarations")?
            .iter()
            .any(|d| d.as_str().is_some_and(|s| s.starts_with(class))),
        "lease probe cannot replace handler"
    );
    config["files"]["handler"]["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@fixtureLease")));
    config["files"]["handler"]["chains"]["response"]
        .as_array_mut()
        .unwrap()
        .push(json!("fixtureLease"));
    config["files"]["header"] = json!({"enabled":true});
    Ok(Some(Arc::new(Capture {
        pool: Arc::new(serverless_core::blocking::conformance::capacity_one()),
        started: Arc::new(AtomicUsize::new(0)),
        completed: Arc::new(AtomicUsize::new(0)),
        late: Arc::new(AtomicUsize::new(0)),
    })))
}
pub(crate) fn install(input: &mut InitInput, capture: &Arc<Capture>) {
    for registration in &mut input.handlers {
        if registration.id == HandlerId::ResponseHeader {
            registration.factory = Some(Arc::new(Provider(capture.clone())));
        }
    }
}
