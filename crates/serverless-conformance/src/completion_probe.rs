//! Fixture-only callback registrations; production pipeline owns execution.
use anyhow::{Result, ensure};
use async_trait::async_trait;
use light_lambda_runtime::init::{HandlerFactoryProvider, InitInput};
use light_runtime::RuntimeConfig;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
pub(crate) struct Capture {
    order: Mutex<Vec<String>>,
    observation: Mutex<Option<Value>>,
}
struct Callback(Arc<Capture>, &'static str);
impl Finalizer for Callback {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.0.order.lock().unwrap().push(self.1.into());
    }
}
impl Finalizer for Capture {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, _: &GatewayResponse) {
        let before = self.order.lock().unwrap().len();
        let request = state.completion.observe_repeat().is_err();
        let response = state.completion.observe_repeat().is_err();
        let order = self.order.lock().unwrap().clone();
        *self.observation.lock().unwrap() = Some(
            json!({"order":order,"countBeforeRepeat":before,"countAfterRepeat":order.len(),"requestRepeatRejected":request,"responseRepeatRejected":response}),
        );
    }
}
impl Capture {
    pub fn observation(&self) -> Result<Value> {
        self.observation
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("actual completion probe never finalized"))
    }
}
struct Provider(Arc<Capture>, bool);
struct Factory(Arc<Capture>, bool);
struct Policy(Arc<Capture>);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, light_lambda_runtime::init_error::InitError> {
        Ok(Arc::new(Factory(self.0.clone(), self.1)))
    }
}
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        let policy = Arc::new(Policy(self.0.clone()));
        Ok(if self.1 {
            Binding::Request(policy, RequestCapabilities::default())
        } else {
            Binding::Response(policy)
        })
    }
}
#[async_trait]
impl RequestHandler for Policy {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state
            .completion
            .add(Arc::new(Callback(self.0.clone(), "request-first")))
            .map_err(|_| HandlerFailure)?;
        state
            .completion
            .add(Arc::new(Callback(self.0.clone(), "request-last")))
            .map_err(|_| HandlerFailure)?;
        Ok(HandlerOutcome::Continue)
    }
}
#[async_trait]
impl ResponseHandler for Policy {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        _: &Invocation,
        state: &mut ExchangeState,
        _: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        state
            .completion
            .add(Arc::new(Callback(self.0.clone(), "response-first")))
            .map_err(|_| HandlerFailure)?;
        state
            .completion
            .add(Arc::new(Callback(self.0.clone(), "response-last")))
            .map_err(|_| HandlerFailure)?;
        Ok(())
    }
}
pub(crate) fn configure(f: &Value, config: &mut Value) -> Result<Option<Arc<Capture>>> {
    if f.get("completionProbe").is_none() {
        return Ok(None);
    }
    ensure!(
        f["completionProbe"] == "reverse-order-finalization"
            && f["id"] == "C-03-completion-probe-multi-contract-v1",
        "closed completion recipe"
    );
    ensure!(
        config["files"].get("header").is_none(),
        "probe cannot replace headers"
    );
    for (id, phase) in [
        (HandlerId::RequestHeader, "request"),
        (HandlerId::ResponseHeader, "response"),
    ] {
        let class = serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class;
        let alias = format!("fixtureCompletion{phase}");
        ensure!(
            !config["files"]["handler"]["handlers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d.as_str().is_some_and(|s| s.starts_with(class))),
            "completion probe cannot replace a handler"
        );
        config["files"]["handler"]["handlers"]
            .as_array_mut()
            .unwrap()
            .push(json!(format!("{class}@{alias}")));
        config["files"]["handler"]["chains"][phase]
            .as_array_mut()
            .unwrap()
            .push(json!(alias));
    }
    // Explicit routes bypass defaultHandlers. Install the same fixture policies
    // there too, so callbacks are registered by actual pipeline execution.
    for path in config["files"]["handler"]["paths"].as_array_mut().unwrap() {
        let exec = path["exec"].as_array_mut().unwrap();
        exec.insert(0, json!("fixtureCompletionrequest"));
        exec.push(json!("fixtureCompletionresponse"));
    }
    config["files"]["header"] = json!({"enabled":true});
    Ok(Some(Arc::new(Capture::default())))
}
pub(crate) fn install(input: &mut InitInput, capture: &Arc<Capture>) {
    for registration in &mut input.handlers {
        if registration.id == HandlerId::RequestHeader
            || registration.id == HandlerId::ResponseHeader
        {
            registration.factory = Some(Arc::new(Provider(
                capture.clone(),
                registration.id == HandlerId::RequestHeader,
            )));
        }
    }
    input.finalizers.push(capture.clone());
}
