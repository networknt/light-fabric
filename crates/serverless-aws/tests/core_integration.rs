mod support;
use async_trait::async_trait;
use serde_json::{Value, json};
use serverless_aws::{
    alb::{IngressMode, encode},
    invoke::InvokeOptions,
    limits::Limits,
};
use serverless_core::{
    compat::{COMPATIBILITY, Compatibility},
    plan::*,
    *,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::*;

struct Factory(Binding);
impl HandlerFactory for Factory {
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
struct ResponsePolicy(Arc<AtomicUsize>);
#[async_trait]
impl ResponseHandler for ResponsePolicy {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        _: &Invocation,
        _: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.0.fetch_add(1, Ordering::SeqCst);
        response
            .headers
            .insert("x-large", "a".repeat(4200).parse().unwrap());
        Ok(())
    }
}
struct Observer(Arc<AtomicUsize>);
impl Finalizer for Observer {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
#[tokio::test]
async fn real_core_r03_collision_and_post_policy_limit_run_once() {
    let fake = Fake::start(vec![Reply::ok()]).await;
    let b = fake.backend(InvokeOptions::default(), Limits::default());
    let policies = Arc::new(AtomicUsize::new(0));
    let finalizers = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory(Binding::Dispatch(
            TargetRoutes::compile([("/pets/{id}@get".into(), Target::Function("FnX".into()))])
                .unwrap(),
        ))),
    );
    registry.register(
        HandlerId::ResponseHeader,
        Arc::new(Factory(Binding::Response(Arc::new(ResponsePolicy(
            policies.clone(),
        ))))),
    );
    let class = |id, alias| {
        format!(
            "{}@{}",
            COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class,
            alias
        )
    };
    let plan=ExecutionPlan::compile(json!({"enabled":true,"handlers":[class(HandlerId::Proxy,"proxy"),class(HandlerId::ResponseHeader,"after")],"defaultHandlers":["proxy","after"]}),json!({}),&registry).unwrap();
    let pipeline = Pipeline::new(Arc::new(plan), vec![Arc::new(Observer(finalizers.clone()))]);
    let mut raw = event();
    raw["path"] = json!("/pets/42");
    raw["multiValueQueryStringParameters"] = json!({"id":["original"]});
    let response = pipeline.execute(invocation(raw), &b).await;
    let output: Value = serde_json::from_slice(
        &encode(
            &response,
            IngressMode::Multi,
            false,
            &Limits {
                max_response_bytes: 4096,
                ..Limits::default()
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(output["statusCode"], 502);
    assert!(output["multiValueHeaders"].get("x-large").is_none());
    assert_eq!(policies.load(Ordering::SeqCst), 1);
    assert_eq!(finalizers.load(Ordering::SeqCst), 1);
    assert_eq!(fake.requests(), 1);
    let seen: Value = serde_json::from_slice(&fake.state.received.lock().unwrap()[0].body).unwrap();
    assert_eq!(seen["queryStringParameters"]["id"], "42");
    assert_eq!(seen["pathParameters"]["id"], "42");
    assert_eq!(
        seen["multiValueQueryStringParameters"]["id"],
        json!(["original"])
    );
}
