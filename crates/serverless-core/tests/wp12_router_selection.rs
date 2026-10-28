use async_trait::async_trait;
use serde_json::json;
use serverless_core::{handlers::router::RouterHandler, plan::*, *};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;
struct Factory(Binding);
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
struct LaterPolicy {
    reject: bool,
}
#[async_trait]
impl RequestHandler for LaterPolicy {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if self.reject {
            return Ok(HandlerOutcome::Reject(StatusError::missing_identity()));
        }
        inv.headers.insert("service_id", "second".parse().unwrap());
        Ok(HandlerOutcome::Continue)
    }
}
#[derive(Default)]
struct BackendProbe(Mutex<Vec<Target>>);
#[async_trait]
impl Backend for BackendProbe {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        assert!(!inv.headers.contains_key("service_id"));
        self.0.lock().unwrap().push(target.clone());
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: None,
            body_is_binary: false,
        })
    }
}
fn invocation(service: Option<&str>) -> Invocation {
    let mut headers = http::HeaderMap::new();
    if let Some(service) = service {
        headers.insert("service_id", service.parse().unwrap());
    }
    Invocation {
        method: http::Method::GET,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers,
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + Duration::from_secs(2),
        cancel: tokio_util::sync::CancellationToken::new(),
        path_params: Default::default(),
        route: None,
        original: serde_json::Value::Null,
    }
}
fn pipeline(exec: Vec<&str>, reject: bool) -> Pipeline {
    let mut registry = Registry::default();
    registry.register(
        HandlerId::Router,
        Arc::new(RouterHandler::new(["first".into(), "second".into()])),
    );
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("lambda".into()))]).unwrap(),
        ))),
    );
    registry.register(
        HandlerId::Correlation,
        Arc::new(Factory(Binding::Request(
            Arc::new(LaterPolicy { reject }),
            RequestCapabilities::default(),
        ))),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let plan=ExecutionPlan::compile(json!({"handlers":[format!("{}@router",class(HandlerId::Router)),format!("{}@later",class(HandlerId::Correlation)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":exec}),json!({}),&registry).unwrap();
    Pipeline::new(Arc::new(plan), vec![])
}
#[tokio::test]
async fn selection_is_first_wins_stripped_and_warm_local_with_lambda_fallback() {
    let p = pipeline(vec!["router", "proxy"], false);
    let backend = BackendProbe::default();
    assert_eq!(
        p.execute(invocation(Some("first")), &backend).await.status,
        200
    );
    assert_eq!(p.execute(invocation(None), &backend).await.status, 200);
    assert_eq!(
        *backend.0.lock().unwrap(),
        vec![
            Target::HttpService("first".into()),
            Target::Function("lambda".into())
        ]
    );
    let p = pipeline(vec!["router", "later", "router", "proxy"], false);
    let backend = BackendProbe::default();
    assert_eq!(
        p.execute(invocation(Some("first")), &backend).await.status,
        200
    );
    assert_eq!(
        *backend.0.lock().unwrap(),
        vec![Target::HttpService("first".into())]
    );
}
#[tokio::test]
async fn rejection_after_selection_and_unknown_service_never_dispatch() {
    let backend = BackendProbe::default();
    let p = pipeline(vec!["router", "later", "proxy"], true);
    assert_eq!(
        p.execute(invocation(Some("first")), &backend).await.status,
        401
    );
    let p = pipeline(vec!["router", "proxy"], false);
    let response = p.execute(invocation(Some("unknown-other")), &backend).await;
    assert_eq!(response.status, 400);
    assert_eq!(response.body.unwrap().as_ref(),br#"{"statusCode":400,"code":"ERR10089","message":"FAILED_TO_INVOKE_SERVICE","description":"Failed to invoke HTTP service with serviceId unknown-other.","severity":"ERROR"}"#);
    assert!(backend.0.lock().unwrap().is_empty());
    let mut unsupported = invocation(Some("first"));
    unsupported.method = http::Method::HEAD;
    assert_eq!(p.execute(unsupported, &backend).await.status, 400);
    assert!(backend.0.lock().unwrap().is_empty());
}
