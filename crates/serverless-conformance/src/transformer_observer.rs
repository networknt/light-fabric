//! Delegation and read-only production context observation; no CEL implementation.
use anyhow::{Context, Result};
use async_trait::async_trait;
use light_lambda_runtime::{
    init::{HandlerFactoryProvider, InitInput},
    init_error::InitError,
    transformer_program::Candidate,
};
use light_runtime::RuntimeConfig;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory, ValidatorMediaRole},
    *,
};
use std::sync::{Arc, Mutex};
#[derive(Default)]
pub(crate) struct Capture(Mutex<Vec<Value>>);
impl Capture {
    pub fn observation(&self) -> Value {
        let inputs = self.0.lock().unwrap();
        json!({"executions":inputs.len(),"input":inputs.first()})
    }
}
struct Provider(Arc<dyn HandlerFactoryProvider>, Arc<Capture>);
struct Factory(Arc<dyn HandlerFactory>, Arc<Capture>);
struct Response(Arc<dyn ResponseHandler>, Arc<Capture>);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        self.0.create(config, security)
    }
    fn prepare_transformer(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<light_security::SecurityRuntime>>,
        program: Option<Arc<Candidate>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(Arc::new(Factory(
            self.0.prepare_transformer(config, security, program)?,
            self.1.clone(),
        )))
    }
}
impl HandlerFactory for Factory {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        self.0.validator_media_role()
    }
    fn build(
        &self,
        definition: &serverless_core::compat::Compatibility,
        alias: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        match self.0.build(definition, alias)? {
            Binding::Response(response) => Ok(Binding::Response(Arc::new(Response(
                response,
                self.1.clone(),
            )))),
            binding => Ok(binding),
        }
    }
}
#[async_trait]
impl ResponseHandler for Response {
    fn id(&self) -> HandlerId {
        self.0.id()
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        state: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        let input = light_lambda_runtime::transformer_adapter::observe_response_context(
            inv, state, response,
        )
        .map_err(|_| HandlerFailure)?;
        self.1.0.lock().unwrap().push(input);
        self.0.on_response(inv, state, response).await
    }
}
pub(crate) fn install(f: &Value, input: &mut InitInput) -> Result<Option<Arc<Capture>>> {
    if f.get("ruleInputProbe").is_none() {
        return Ok(None);
    }
    anyhow::ensure!(
        f["ruleInputProbe"] == "native-response-map-v1",
        "unknown input observation"
    );
    let capture = Arc::new(Capture::default());
    let registration = input
        .handlers
        .iter_mut()
        .find(|r| r.id == HandlerId::ResponseTransformer)
        .context("registered Transformer")?;
    registration.factory = Some(Arc::new(Provider(
        registration
            .factory
            .clone()
            .context("production Transformer factory")?,
        capture.clone(),
    )));
    Ok(Some(capture))
}
