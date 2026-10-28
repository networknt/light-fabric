//! Closed component: capture the application provider's real factory-built Router.
//! Only prerequisites and observations live here; state/status logic is production code.
use anyhow::{Context, Result, ensure};
use light_lambda_runtime::{
    init::{HandlerFactoryProvider, InitInput},
    init_error::InitError,
    router_policy::PreparedRouterResources,
};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serde_json::{Value, json};
use serverless_core::{
    plan::{Binding, HandlerFactory, ValidatorMediaRole},
    *,
};
use std::sync::{Arc, Mutex};

pub(crate) fn supported(f: &Value) -> bool {
    f["component"] == "routerFailedExchangeH04"
        && matches!(
            f["id"].as_str(),
            Some(
                "H-04-already-failed-router-component-v1"
                    | "H-04-already-failed-router-forwarding-profile-v1"
            )
        )
}
pub(crate) fn validate(f: &Value) -> Result<()> {
    ensure!(
        supported(f)
            && f["javaOnly"] == false
            && f["rustOnly"] == false
            && f["ledger"] == json!(["H-04"]),
        "closed paired Router component"
    );
    ensure!(
        f["rawEvent"]["requestContext"]["elb"].is_object()
            && f["rawEvent"]["multiValueHeaders"]["service_id"] == json!(["test-service"]),
        "real ALB Router request required"
    );
    Ok(())
}
#[derive(Default)]
pub(crate) struct Capture(Mutex<Option<Arc<dyn RequestHandler>>>);
struct Provider {
    inner: Arc<dyn HandlerFactoryProvider>,
    capture: Arc<Capture>,
}
struct Factory {
    inner: Arc<dyn HandlerFactory>,
    capture: Arc<Capture>,
}
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        c: &RuntimeConfig,
        s: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(Arc::new(Factory {
            inner: self.inner.create(c, s)?,
            capture: self.capture.clone(),
        }))
    }
    fn prepare_router_resources(
        &self,
        c: &RuntimeConfig,
        s: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedRouterResources, InitError> {
        let mut prepared = self.inner.prepare_router_resources(c, s)?;
        prepared.factory = Arc::new(Factory {
            inner: prepared.factory,
            capture: self.capture.clone(),
        });
        Ok(prepared)
    }
}
impl HandlerFactory for Factory {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        self.inner.validator_media_role()
    }
    fn build(
        &self,
        c: &serverless_core::compat::Compatibility,
        alias: &str,
    ) -> Result<Binding, serverless_core::InitError> {
        let binding = self.inner.build(c, alias)?;
        if alias == "h0"
            && let Binding::Request(handler, _) = &binding
        {
            *self.capture.0.lock().unwrap() = Some(handler.clone());
        }
        Ok(binding)
    }
}
pub(crate) fn install(f: &Value, input: &mut InitInput) -> Result<Option<Arc<Capture>>> {
    if !supported(f) {
        return Ok(None);
    }
    validate(f)?;
    let capture = Arc::new(Capture::default());
    let registration = input
        .handlers
        .iter_mut()
        .find(|r| r.id == HandlerId::Router)
        .context("application Router registration")?;
    let inner = registration
        .factory
        .clone()
        .context("actual application Router provider")?;
    registration.factory = Some(Arc::new(Provider {
        inner,
        capture: capture.clone(),
    }));
    Ok(Some(capture))
}
pub(crate) async fn run(
    f: &Value,
    revision: &str,
    capture: &Capture,
    tokens: &crate::tokens::Tokens,
    invoke: &crate::application::Dependency,
    http: Option<&crate::application::Dependency>,
) -> Result<Value> {
    let started = std::time::Instant::now();
    let handler = capture
        .0
        .lock()
        .unwrap()
        .clone()
        .context("actual Init did not build h0 Router")?;
    ensure!(
        handler.id() == HandlerId::Router,
        "captured handler is not Router"
    );
    let now = tokio::time::Instant::now();
    let mut inv = serverless_aws::alb::decode(
        f["rawEvent"].clone(),
        serverless_aws::alb::DecodeContext {
            request_id: "router-failed-component".into(),
            deadline: now + std::time::Duration::from_secs(2),
            response_deadline: now + std::time::Duration::from_secs(3),
            cancel: Default::default(),
        },
        &serverless_aws::limits::Limits::default(),
    )
    .map_err(|_| anyhow::anyhow!("invalid ALB component input"))?;
    let headers = inv.headers.clone();
    let path = inv.path.clone();
    let query = inv.query.clone();
    let route = inv.route.clone();
    let params = inv.path_params.clone();
    let body = inv.body.clone();
    let mut state = ExchangeState::default();
    state.record_request_status(StatusError::new(
        401,
        "ERR10000",
        "INVALID_AUTH_TOKEN",
        "Incorrect signature or malformed token in authorization header",
    ));
    ensure!(
        state.request_failure().is_some(),
        "typed request failure prerequisite absent"
    );
    let before = state.native_request_state();
    let outcome = handler
        .on_request(&mut inv, &mut state)
        .await
        .map_err(|_| anyhow::anyhow!("Router handler failed"))?;
    let HandlerOutcome::Reject(status) = outcome else {
        anyhow::bail!("factory-built Router did not reject failed request")
    };
    ensure!(
        inv.headers == headers
            && inv.path == path
            && inv.query == query
            && inv.route == route
            && inv.path_params == params
            && inv.body == body,
        "Router mutated failed request"
    );
    ensure!(
        state.native_request_state() == before && state.request_failure().is_some(),
        "Router replaced request failure"
    );
    let observed = json!({"outcome":"handlerStatus","status":status.status_code,"code":status.code,"body":String::from_utf8(status.to_bytes().to_vec())?,"failedState":state.request_failure().is_some(),"exchangeState":state.native_request_state(),"backendCalls":invoke.calls(),"httpCalls":http.map(|h|h.calls()).unwrap_or_default(),"jwksFetches":tokens.calls().len()});
    assert_observation(&f["expect"], &observed)?;
    Ok(
        json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":"passed","durationMs":started.elapsed().as_millis() as u64,"observations":[observed],"assertionErrors":[]}),
    )
}
pub(crate) fn assert_observation(expected: &Value, observed: &Value) -> Result<()> {
    // Component schema closes every field; no projected state or response bytes.
    ensure!(
        expected == observed,
        "actual factory-built Router observation differs"
    );
    Ok(())
}
