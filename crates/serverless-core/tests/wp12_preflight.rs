pub mod support;
use async_trait::async_trait;
use bytes::Bytes;
use serverless_core::{plan::*, *};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use support::*;

struct Preflight(Arc<Counts>);
#[async_trait]
impl RequestHandler for Preflight {
    fn id(&self) -> HandlerId {
        HandlerId::RequestCors
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        self.0.request_entries.fetch_add(1, Ordering::SeqCst);
        self.0.request_completions.fetch_add(1, Ordering::SeqCst);
        let mut candidate = response(None);
        candidate.headers.insert(
            "access-control-allow-origin",
            "https://test.invalid".parse().unwrap(),
        );
        Ok(HandlerOutcome::ContinueWithInitialResponse(candidate))
    }
}
fn setup(c: &Arc<Counts>, action: Action, delay: Duration) -> Pipeline {
    let mut r = registry();
    install(
        &mut r,
        HandlerId::RequestCors,
        Binding::Request(
            Arc::new(Preflight(c.clone())),
            RequestCapabilities::default(),
        ),
    );
    let mut later = Handler::new(HandlerId::Validator, action, c);
    later.delay = delay;
    install(
        &mut r,
        HandlerId::Validator,
        Binding::Request(Arc::new(later), RequestCapabilities::default()),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            c,
        ))),
    );
    Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::RequestCors, "cors"),
                    (HandlerId::Validator, "later"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseHeader, "response"),
                ],
                &["cors", "later", "proxy", "response"],
            ),
            &r,
        ),
        vec![c.clone()],
    )
}
fn counts(c: &Counts, dispatches: usize, actual: &GatewayResponse) {
    assert_eq!(n(&c.request_entries), 2);
    assert_eq!(n(&c.backend), dispatches);
    assert_eq!(n(&c.response_entries), 1);
    assert_eq!(n(&c.response_completions), 1);
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(
        c.finalized_responses.lock().unwrap().as_slice(),
        std::slice::from_ref(actual)
    );
}
#[tokio::test]
async fn provisional_preflight_dispatches_once_and_retains_first_response() {
    let c = Arc::new(Counts::default());
    let p = setup(&c, Action::Continue, Duration::ZERO);
    let mut inv = invocation();
    inv.body = Some(Bytes::from_static(
        b"backend body must not replace preflight",
    ));
    let result = p.execute(inv, &FakeBackend::new(&c)).await;
    assert_eq!(result.status, 200);
    assert_eq!(result.body, None);
    assert_eq!(
        result.headers["access-control-allow-origin"],
        "https://test.invalid"
    );
    assert_eq!(result.headers["x-response-policy"], "entered");
    counts(&c, 1, &result);
}
#[tokio::test]
async fn subsequent_denial_error_and_panic_override_preflight_without_dispatch() {
    for (action, status) in [
        (Action::Reject, 401),
        (Action::Fail, 500),
        (Action::Panic, 500),
    ] {
        let c = Arc::new(Counts::default());
        let result = setup(&c, action, Duration::ZERO)
            .execute(invocation(), &FakeBackend::new(&c))
            .await;
        assert_eq!(result.status, status);
        assert!(!result.headers.contains_key("access-control-allow-origin"));
        counts(&c, 0, &result);
    }
}
#[tokio::test(start_paused = true)]
async fn exhausted_request_deadline_overrides_preflight_without_dispatch() {
    let c = Arc::new(Counts::default());
    let mut inv = invocation();
    inv.deadline = tokio::time::Instant::now() + Duration::from_millis(10);
    let result = setup(&c, Action::Continue, Duration::from_millis(20))
        .execute(inv, &FakeBackend::new(&c))
        .await;
    assert_eq!(result.status, 504);
    assert!(!result.headers.contains_key("access-control-allow-origin"));
    counts(&c, 0, &result);
}
#[tokio::test]
async fn backend_failure_overrides_preflight_and_receives_one_response_pass() {
    let c = Arc::new(Counts::default());
    let mut backend = FakeBackend::new(&c);
    backend.failure = true;
    let result = setup(&c, Action::Continue, Duration::ZERO)
        .execute(invocation(), &backend)
        .await;
    assert_eq!(result.status, 502);
    assert!(!result.headers.contains_key("access-control-allow-origin"));
    counts(&c, 1, &result);
}
#[tokio::test(start_paused = true)]
async fn backend_deadline_overrides_preflight_without_redispatch() {
    let c = Arc::new(Counts::default());
    let mut backend = FakeBackend::new(&c);
    backend.delay = Duration::from_secs(3);
    let result = setup(&c, Action::Continue, Duration::ZERO)
        .execute(invocation(), &backend)
        .await;
    assert_eq!(result.status, 504);
    assert!(!result.headers.contains_key("access-control-allow-origin"));
    counts(&c, 1, &result);
}
