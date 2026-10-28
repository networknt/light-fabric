//! Observation-only decorator around the registered production response factory.
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use light_lambda_runtime::init::{HandlerFactoryProvider, InitInput};
use light_runtime::RuntimeConfig;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory},
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
#[derive(Default)]
pub(crate) struct Counts {
    entries: AtomicUsize,
    completions: AtomicUsize,
    finalizations: AtomicUsize,
}
impl Counts {
    pub fn entry(&self) {
        self.entries.fetch_add(1, Ordering::SeqCst);
    }
    pub fn complete(&self) {
        self.completions.fetch_add(1, Ordering::SeqCst);
    }
    pub fn observation(&self, name: &str) -> Value {
        json!({"handler":name,"entries":self.entries.load(Ordering::SeqCst),"completions":self.completions.load(Ordering::SeqCst),"finalizations":self.finalizations.load(Ordering::SeqCst)})
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.finalizations.fetch_add(1, Ordering::SeqCst);
    }
}
struct Provider(Arc<dyn HandlerFactoryProvider>, Arc<Counts>);
struct Factory(Arc<dyn HandlerFactory>, Arc<Counts>);
struct Handler(Arc<dyn ResponseHandler>, Arc<Counts>);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, light_lambda_runtime::init_error::InitError> {
        Ok(Arc::new(Factory(
            self.0.create(config, security)?,
            self.1.clone(),
        )))
    }
}
impl HandlerFactory for Factory {
    fn validator_media_role(&self) -> serverless_core::plan::ValidatorMediaRole {
        self.0.validator_media_role()
    }
    fn build(
        &self,
        definition: &serverless_core::compat::Compatibility,
        alias: &str,
    ) -> Result<Binding, InitError> {
        let binding = self.0.build(definition, alias)?;
        if alias != "response0" {
            return Ok(binding);
        }
        match binding {
            Binding::Response(handler) => Ok(Binding::Response(Arc::new(Handler(
                handler,
                self.1.clone(),
            )))),
            _ => Err(InitError::FactoryContract(HandlerId::ResponseHeader)),
        }
    }
}
#[async_trait]
impl ResponseHandler for Handler {
    fn id(&self) -> HandlerId {
        self.0.id()
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        state: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.1.entry();
        let result = self.0.on_response(inv, state, response).await;
        self.1.complete();
        result
    }
}
pub(crate) fn install(
    f: &Value,
    input: &mut InitInput,
    work: Option<&Arc<crate::bounded_work::Work>>,
) -> Result<Option<Arc<Counts>>> {
    let Some(probe) = f.get("responsePolicyProbe") else {
        return Ok(None);
    };
    if probe["handler"] == "fixtureWork" {
        let counts = work
            .context("response work observer needs actual work")?
            .counts
            .clone();
        input.finalizers.push(counts.clone());
        return Ok(Some(counts));
    }
    ensure!(
        probe["handler"] == "response0",
        "unsupported response observer"
    );
    let counts = Arc::new(Counts::default());
    let registration = input
        .handlers
        .iter_mut()
        .find(|r| r.id == HandlerId::ResponseHeader)
        .context("registered response factory")?;
    registration.factory = Some(Arc::new(Provider(
        registration
            .factory
            .clone()
            .context("available response factory")?,
        counts.clone(),
    )));
    input.finalizers.push(counts.clone());
    Ok(Some(counts))
}
