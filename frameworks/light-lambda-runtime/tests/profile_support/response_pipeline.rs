//! Test-only integration of the proposed blocking worker boundary.
use super::transformer;
use async_trait::async_trait;
use serde_json::json;
use serverless_core::{plan::*, *};
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::time::Instant;

struct ResponseWork(Arc<(Mutex<bool>, Condvar)>, Arc<AtomicUsize>);
#[async_trait]
impl ResponseHandler for ResponseWork {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseTransformer
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        _: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        let gate = self.0.clone();
        let finished = self.1.clone();
        let result = transformer::worker(
            json!({}),
            inv.response_deadline,
            inv.cancel.clone(),
            move |_| {
                let (lock, cv) = &*gate;
                let mut ready = lock.lock().unwrap();
                while !*ready {
                    ready = cv.wait(ready).unwrap();
                }
                finished.fetch_add(1, Ordering::SeqCst);
                Ok(json!({"responseBody":"late"}))
            },
        )
        .await
        .map_err(|_| HandlerFailure)?;
        response.body = Some(bytes::Bytes::from(
            result["responseBody"].as_str().unwrap().to_owned(),
        ));
        Ok(())
    }
}
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
struct Counts {
    dispatch: AtomicUsize,
    finalizations: AtomicUsize,
}
#[async_trait]
impl Backend for Counts {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.dispatch.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from_static(b"backend")),
            body_is_binary: false,
        })
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, response: &GatewayResponse) {
        assert!(
            std::str::from_utf8(response.body.as_ref().unwrap())
                .unwrap()
                .contains("ERR14017")
        );
        self.finalizations.fetch_add(1, Ordering::SeqCst);
    }
}
pub async fn timeout_discards_late_response_and_finalizes_once() {
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let finished = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(Counts {
        dispatch: AtomicUsize::new(0),
        finalizations: AtomicUsize::new(0),
    });
    let mut registry = Registry::default();
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-only".into()))])
                .unwrap(),
        ))),
    );
    registry.register(
        HandlerId::ResponseTransformer,
        Arc::new(Factory(Binding::Response(Arc::new(ResponseWork(
            gate.clone(),
            finished.clone(),
        ))))),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    let plan = Arc::new(ExecutionPlan::compile(json!({"handlers":[format!("{}@proxy",class(HandlerId::Proxy)),format!("{}@response",class(HandlerId::ResponseTransformer))],"defaultHandlers":["proxy","response"]}),json!({}),&registry).unwrap());
    let pipeline = Pipeline::new(plan, vec![counts.clone()]);
    let inv = Invocation {
        method: http::Method::GET,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers: http::HeaderMap::new(),
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + Duration::from_millis(50),
        cancel: tokio_util::sync::CancellationToken::new(),
        path_params: Default::default(),
        route: None,
        original: serde_json::Value::Null,
    };
    let response = pipeline.execute(inv, &*counts).await;
    assert!(
        std::str::from_utf8(response.body.as_ref().unwrap())
            .unwrap()
            .contains("ERR14017")
    );
    assert_eq!(counts.dispatch.load(Ordering::SeqCst), 1);
    assert_eq!(counts.finalizations.load(Ordering::SeqCst), 1);
    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while finished.load(Ordering::SeqCst) != 1 || transformer::permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(counts.finalizations.load(Ordering::SeqCst), 1);
    assert_eq!(counts.dispatch.load(Ordering::SeqCst), 1);
    assert!(
        std::str::from_utf8(response.body.as_ref().unwrap())
            .unwrap()
            .contains("ERR14017")
    );
}
