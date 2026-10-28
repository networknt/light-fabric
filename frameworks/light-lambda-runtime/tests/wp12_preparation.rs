//! Actual common pipeline and ALB adapter preparation; no differential runner.
use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use serde_json::{Value, json};
use serverless_aws::{
    alb::{self, IngressMode},
    limits::Limits,
};
use serverless_core::{compat::COMPATIBILITY, plan::*, *};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct Counts {
    dispatch: AtomicUsize,
    entries: AtomicUsize,
    returns: AtomicUsize,
    callbacks: Mutex<Vec<(u16, bool)>>,
    started: Notify,
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        assert!(state.metrics.finalized);
        self.callbacks
            .lock()
            .unwrap()
            .push((response.status.as_u16(), state.audit.status.is_some()));
    }
}
struct BackendProbe(Arc<Counts>);
#[async_trait]
impl Backend for BackendProbe {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.dispatch.fetch_add(1, Ordering::SeqCst);
        let mut headers = HeaderMap::new();
        headers.append("x-test", "one".parse().unwrap());
        headers.append("x-test", "two".parse().unwrap());
        headers.insert("x-correlation-id", "test-only-correlation".parse().unwrap());
        Ok(GatewayResponse {
            status: StatusCode::OK,
            headers,
            body: Some(Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
struct ResponseProbe {
    counts: Arc<Counts>,
    large: bool,
    slow: bool,
}
#[async_trait]
impl ResponseHandler for ResponseProbe {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseHeader
    }
    async fn on_response(
        &self,
        _: &Invocation,
        _: &mut ExchangeState,
        response: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.counts.entries.fetch_add(1, Ordering::SeqCst);
        self.counts.started.notify_one();
        if self.slow {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if self.large {
            response.body = Some(Bytes::from("x".repeat(5000)));
        }
        self.counts.returns.fetch_add(1, Ordering::SeqCst);
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
fn pipeline(counts: &Arc<Counts>, large: bool, slow: bool) -> Pipeline {
    let mut registry = Registry::default();
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory(Binding::Dispatch(
            TargetRoutes::compile([("/x@get".into(), Target::Function("test-only".into()))])
                .unwrap(),
        ))),
    );
    registry.register(
        HandlerId::ResponseHeader,
        Arc::new(Factory(Binding::Response(Arc::new(ResponseProbe {
            counts: counts.clone(),
            large,
            slow,
        })))),
    );
    let entry = |id, alias| {
        format!(
            "{}@{alias}",
            COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class
        )
    };
    let plan=ExecutionPlan::compile(json!({"enabled":true,"handlers":[entry(HandlerId::Proxy,"proxy"),entry(HandlerId::ResponseHeader,"response")],"defaultHandlers":["proxy","response"]}),json!({}),&registry).unwrap();
    Pipeline::new(Arc::new(plan), vec![counts.clone()])
}
fn invocation(response_budget: Duration) -> Invocation {
    Invocation {
        method: Method::GET,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers: HeaderMap::new(),
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only-preparation".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + response_budget,
        cancel: CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: Value::Null,
    }
}
fn limit() -> Limits {
    Limits {
        max_response_bytes: 4096,
        ..Default::default()
    }
}
fn assert_counts(c: &Counts, status: u16, returns: usize) {
    assert_eq!(c.dispatch.load(Ordering::SeqCst), 1);
    assert_eq!(c.entries.load(Ordering::SeqCst), 1);
    assert_eq!(c.returns.load(Ordering::SeqCst), returns);
    let callbacks = c.callbacks.lock().unwrap();
    assert_eq!(callbacks.len(), 1);
    assert_eq!(callbacks[0].0, status);
}

#[tokio::test]
async fn preparation_preserves_exact_bytes_and_single_policy_pass() {
    for large in [false, true] {
        for force_base64 in [false, true] {
            let c = Arc::new(Counts::default());
            let frozen = Mutex::new(Vec::new());
            let response = pipeline(&c, large, false)
                .execute_prepared(
                    invocation(Duration::from_secs(2)),
                    &BackendProbe(c.clone()),
                    |candidate| {
                        let legacy =
                            alb::encode(&candidate, IngressMode::Multi, force_base64, &limit())
                                .unwrap();
                        let prepared = alb::prepare_response(
                            candidate,
                            IngressMode::Multi,
                            force_base64,
                            &limit(),
                        )
                        .unwrap();
                        assert_eq!(
                            prepared.bytes, legacy,
                            "retain exact approved provider bytes"
                        );
                        *frozen.lock().unwrap() = prepared.bytes;
                        Ok(prepared.response)
                    },
                )
                .await;
            let status = if large { 502 } else { 200 };
            assert_eq!(response.status.as_u16(), status);
            assert_counts(&c, status, 1);
            let envelope: Value = serde_json::from_slice(&frozen.into_inner().unwrap()).unwrap();
            assert_eq!(envelope["statusCode"], status);
            if large {
                assert_eq!(envelope["isBase64Encoded"], false);
                assert_eq!(
                    envelope["multiValueHeaders"]["x-correlation-id"],
                    json!(["test-only-correlation"])
                );
                assert!(envelope["multiValueHeaders"].get("x-test").is_none());
            }
        }
    }
}

#[tokio::test]
async fn timeout_prepares_minimal_response_without_second_pass() {
    let c = Arc::new(Counts::default());
    let prepares = AtomicUsize::new(0);
    let response = pipeline(&c, false, true)
        .execute_prepared(
            invocation(Duration::from_millis(20)),
            &BackendProbe(c.clone()),
            |candidate| {
                prepares.fetch_add(1, Ordering::SeqCst);
                let prepared =
                    alb::prepare_response(candidate, IngressMode::Multi, false, &limit()).unwrap();
                let envelope: Value = serde_json::from_slice(&prepared.bytes).unwrap();
                assert_eq!(envelope["statusCode"], 504);
                Ok(prepared.response)
            },
        )
        .await;
    assert_eq!(response.status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(prepares.load(Ordering::SeqCst), 1);
    assert_counts(&c, 504, 0);
}

#[tokio::test]
async fn failed_and_panicking_preparation_finalize_once() {
    for panic in [false, true] {
        let c = Arc::new(Counts::default());
        let prepares = AtomicUsize::new(0);
        let response = pipeline(&c, false, false)
            .execute_prepared(
                invocation(Duration::from_secs(2)),
                &BackendProbe(c.clone()),
                |_| {
                    prepares.fetch_add(1, Ordering::SeqCst);
                    if panic {
                        panic!("test-only bounded preparation failure");
                    }
                    Err(StatusError::handler_failure())
                },
            )
            .await;
        assert_eq!(response.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(prepares.load(Ordering::SeqCst), 1);
        assert_counts(&c, 500, 1);
        assert!(c.callbacks.lock().unwrap()[0].1);
        let bytes = alb::encode_final(&response, IngressMode::Multi, false, &limit()).unwrap();
        let envelope: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(envelope["statusCode"], 500);
    }
}

#[tokio::test]
async fn cancellation_keeps_drop_finalization_and_never_redispatches() {
    let c = Arc::new(Counts::default());
    let counts = c.clone();
    let task = tokio::spawn(async move {
        pipeline(&counts, false, true)
            .execute_prepared(
                invocation(Duration::from_secs(2)),
                &BackendProbe(counts.clone()),
                Ok,
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), c.started.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_counts(&c, 504, 0);
}

#[test]
fn preparation_failure_and_final_encoding_do_not_substitute_an_outcome() {
    let mut response = StatusError::handler_failure().response();
    response
        .headers
        .insert("x-invalid", HeaderValue::from_bytes(&[0xff]).unwrap());
    assert!(alb::prepare_response(response, IngressMode::Multi, false, &limit()).is_err());
    let response = GatewayResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body: Some(Bytes::from("x".repeat(5000))),
        body_is_binary: false,
    };
    assert_eq!(
        alb::encode_final(&response, IngressMode::Multi, false, &limit()).unwrap_err(),
        "prepared response exceeds response limit"
    );
}
