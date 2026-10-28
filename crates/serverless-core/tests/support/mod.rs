use async_trait::async_trait;
use bytes::Bytes;
use http::{HeaderMap, Method, StatusCode};
use serde_json::{Value, json};
use serverless_core::{
    compat::{COMPATIBILITY, Compatibility},
    plan::*,
    *,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

pub fn invocation() -> Invocation {
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
        request_id: "test-only".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + Duration::from_secs(2),
        cancel: CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: Value::Null,
    }
}
pub fn response(body: Option<Bytes>) -> GatewayResponse {
    GatewayResponse {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        body,
        body_is_binary: false,
    }
}
pub fn entry(id: HandlerId, alias: &str) -> String {
    format!(
        "{}@{alias}",
        COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class
    )
}
pub fn config(ids: &[(HandlerId, &str)], exec: &[&str]) -> Value {
    json!({"enabled":true,"handlers":ids.iter().map(|(id,a)|entry(*id,a)).collect::<Vec<_>>(),"defaultHandlers":exec})
}
pub struct Factory(pub Binding);
impl HandlerFactory for Factory {
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
pub fn install(registry: &mut Registry, id: HandlerId, binding: Binding) {
    registry.register(id, Arc::new(Factory(binding)));
}
pub fn targets() -> TargetRoutes {
    TargetRoutes::compile([("/*@get".into(), Target::Function("FnTest".into()))]).unwrap()
}
pub fn registry() -> Registry {
    let mut r = Registry::default();
    install(&mut r, HandlerId::Proxy, Binding::Dispatch(targets()));
    r
}
pub fn compile(value: Value, r: &Registry) -> Arc<ExecutionPlan> {
    Arc::new(ExecutionPlan::compile(value, json!({}), r).unwrap())
}

#[derive(Default)]
pub struct Counts {
    pub request_entries: AtomicUsize,
    pub request_completions: AtomicUsize,
    pub response_entries: AtomicUsize,
    pub response_completions: AtomicUsize,
    pub backend: AtomicUsize,
    pub finalized: AtomicUsize,
    pub order: Mutex<Vec<String>>,
    pub remaining: Mutex<Vec<Duration>>,
    pub seen: Mutex<Vec<(bool, bool, bool)>>,
    pub finalized_responses: Mutex<Vec<GatewayResponse>>,
}
impl Counts {
    pub fn mark(&self, s: &str) {
        self.order.lock().unwrap().push(s.into());
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, st: &ExchangeState, resp: &GatewayResponse) {
        assert!(st.metrics.finalized);
        self.finalized.fetch_add(1, Ordering::SeqCst);
        self.mark("finalize");
        self.finalized_responses.lock().unwrap().push(resp.clone());
    }
}
#[derive(Clone, Copy)]
pub enum Action {
    Continue,
    Reject,
    Respond,
    Panic,
    Fail,
    Authenticate,
    LeakCheck,
    Partial,
    ObserveResponseCancel,
}
pub struct Handler {
    pub id: HandlerId,
    pub action: Action,
    pub delay: Duration,
    pub counts: Arc<Counts>,
}
impl Handler {
    pub fn new(id: HandlerId, action: Action, counts: &Arc<Counts>) -> Self {
        Self {
            id,
            action,
            delay: Duration::ZERO,
            counts: counts.clone(),
        }
    }
}
#[async_trait]
impl RequestHandler for Handler {
    fn id(&self) -> HandlerId {
        self.id
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        self.counts.request_entries.fetch_add(1, Ordering::SeqCst);
        self.counts.mark(&format!("request:{:?}", self.id));
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        // Delegate panic completion is counted at propagation, cancellation is not.
        self.counts
            .request_completions
            .fetch_add(1, Ordering::SeqCst);
        Ok(match self.action {
            Action::Reject => HandlerOutcome::Reject(StatusError::missing_identity()),
            Action::Respond => {
                HandlerOutcome::Respond(response(Some(Bytes::from_static(b"local"))))
            }
            Action::Panic => panic!("test-only handler failure"),
            Action::Fail => return Err(HandlerFailure),
            Action::Authenticate => {
                st.principal = Some(Principal::from_verified(
                    "test".into(),
                    Some("verified-test-client".into()),
                    None,
                    json!({"sub":"test"}),
                ));
                HandlerOutcome::Continue
            }
            Action::LeakCheck => {
                assert!(st.traceability_id.is_none());
                self.counts.seen.lock().unwrap().push((
                    st.principal.is_some(),
                    st.correlation_id.is_some(),
                    st.response_headers.contains_key("x-test"),
                ));
                st.correlation_id = Some("test".into());
                st.traceability_id = Some("trace-test".into());
                st.principal = Some(Principal::from_verified(
                    "synthetic-verified".into(),
                    None,
                    None,
                    json!({"sub":"test"}),
                ));
                st.response_headers.insert("x-test", "yes".parse().unwrap());
                inv.headers.insert("x-mutated", "test".parse().unwrap());
                HandlerOutcome::Continue
            }
            _ => HandlerOutcome::Continue,
        })
    }
}
#[async_trait]
impl ResponseHandler for Handler {
    fn id(&self) -> HandlerId {
        self.id
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        _: &mut ExchangeState,
        resp: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.counts.response_entries.fetch_add(1, Ordering::SeqCst);
        self.counts.mark(&format!("response:{:?}", self.id));
        resp.headers
            .insert("x-response-policy", "entered".parse().unwrap());
        if matches!(self.action, Action::ObserveResponseCancel) {
            assert!(!inv.cancel.is_cancelled());
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.counts
            .response_completions
            .fetch_add(1, Ordering::SeqCst);
        match self.action {
            Action::Panic => panic!("test-only response failure"),
            Action::Fail => Err(HandlerFailure),
            _ => Ok(()),
        }
    }
}
pub struct FakeBackend {
    pub counts: Arc<Counts>,
    pub delay: Duration,
    pub failure: bool,
    pub panic: bool,
}
impl FakeBackend {
    pub fn new(counts: &Arc<Counts>) -> Self {
        Self {
            counts: counts.clone(),
            delay: Duration::ZERO,
            failure: false,
            panic: false,
        }
    }
}
#[async_trait]
impl Backend for FakeBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.counts.backend.fetch_add(1, Ordering::SeqCst);
        self.counts.mark("backend");
        self.counts
            .remaining
            .lock()
            .unwrap()
            .push(inv.deadline.saturating_duration_since(Instant::now()));
        if self.panic {
            panic!("test-only backend failure");
        }
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        if self.failure {
            Err(DispatchError(StatusError::new(
                502,
                "ERR14019",
                "BACKEND_UNAVAILABLE",
                "Backend function FnTest is missing or not permitted.",
            )))
        } else {
            Ok(response(inv.body.clone()))
        }
    }
}
pub fn pipeline(
    request: Option<Handler>,
    resp: Option<Handler>,
    caps: RequestCapabilities,
    counts: &Arc<Counts>,
) -> Pipeline {
    let mut r = registry();
    let mut ids = vec![];
    let mut exec = vec![];
    if let Some(h) = request {
        ids.push((h.id, "before"));
        exec.push("before");
        install(&mut r, h.id, Binding::Request(Arc::new(h), caps));
    }
    ids.push((HandlerId::Proxy, "proxy"));
    exec.push("proxy");
    if let Some(h) = resp {
        ids.push((h.id, "after"));
        exec.push("after");
        install(&mut r, h.id, Binding::Response(Arc::new(h)));
    }
    Pipeline::new(compile(config(&ids, &exec), &r), vec![counts.clone()])
}
pub fn n(a: &AtomicUsize) -> usize {
    a.load(Ordering::SeqCst)
}
