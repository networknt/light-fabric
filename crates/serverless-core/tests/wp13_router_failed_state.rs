#[allow(dead_code)]
mod support;
use async_trait::async_trait;
use serverless_core::{handlers::router::RouterHandler, plan::*, *};
use std::sync::{Arc, Mutex};
struct FailurePrerequisite;
#[async_trait]
impl RequestHandler for FailurePrerequisite {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if inv.headers.contains_key("x-test-failure") {
            state.record_request_status(StatusError::new(
                401,
                "ERR10000",
                "INVALID_AUTH_TOKEN",
                "test-only-cause",
            ));
        }
        Ok(HandlerOutcome::Continue)
    }
}
#[derive(Default)]
struct Calls(Mutex<Vec<Target>>);
#[async_trait]
impl Backend for Calls {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.lock().unwrap().push(target.clone());
        Ok(support::response(None))
    }
}
#[derive(Default)]
struct Observation(Mutex<Vec<(u16, bool, String)>>);
impl Finalizer for Observation {
    fn finalize(&self, inv: &Invocation, state: &ExchangeState, _: &GatewayResponse) {
        if let Some(cause) = state.request_failure() {
            self.0.lock().unwrap().push((
                state.native_request_state(),
                inv.headers.contains_key("service_id"),
                cause.code.clone(),
            ));
        }
    }
}
#[tokio::test]
async fn factory_router_rejects_before_header_target_and_transport_and_is_warm_isolated() {
    let mut registry = support::registry();
    support::install(
        &mut registry,
        HandlerId::RequestHeader,
        Binding::Request(
            Arc::new(FailurePrerequisite),
            RequestCapabilities::default(),
        ),
    );
    registry.register(
        HandlerId::Router,
        Arc::new(RouterHandler::new(["test-service".into()])),
    );
    let plan = support::compile(
        support::config(
            &[
                (HandlerId::RequestHeader, "prerequisite"),
                (HandlerId::Router, "router"),
                (HandlerId::Proxy, "proxy"),
            ],
            &["prerequisite", "router", "proxy"],
        ),
        &registry,
    );
    let observed = Arc::new(Observation::default());
    let pipeline = Pipeline::new(plan, vec![observed.clone()]);
    let calls = Calls::default();
    for failed in [true, false, true, false] {
        let mut inv = support::invocation();
        inv.headers
            .insert("service_id", "test-service".parse().unwrap());
        if failed {
            inv.headers.insert("x-test-failure", "1".parse().unwrap());
        }
        let count = calls.0.lock().unwrap().len();
        let response = pipeline.execute(inv, &calls).await;
        if failed {
            assert_eq!(response.status, 400);
            let body = String::from_utf8(response.body.unwrap().to_vec()).unwrap();
            assert!(body.contains("ERR10087"));
            assert!(!body.contains("test-only-cause"));
            assert_eq!(calls.0.lock().unwrap().len(), count);
        } else {
            assert_eq!(response.status, 200);
            assert_eq!(
                calls.0.lock().unwrap().last(),
                Some(&Target::HttpService("test-service".into()))
            );
        }
    }
    assert_eq!(
        *observed.0.lock().unwrap(),
        vec![((1 << 1) | (1 << 3), true, "ERR10000".into()); 2]
    );
    assert_eq!(
        *calls.0.lock().unwrap(),
        vec![Target::HttpService("test-service".into()); 2]
    );
}
