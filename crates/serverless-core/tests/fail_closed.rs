pub mod support;
use serverless_core::{plan::*, *};
use std::sync::Arc;
use support::*;

#[tokio::test]
async fn request_panic_and_failure_fail_closed_and_receive_response_policy() {
    for action in [Action::Panic, Action::Fail] {
        let c = Arc::new(Counts::default());
        let p = pipeline(
            Some(Handler::new(HandlerId::Correlation, action, &c)),
            Some(Handler::new(
                HandlerId::ResponseHeader,
                Action::Continue,
                &c,
            )),
            RequestCapabilities::default(),
            &c,
        );
        let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
        assert_eq!(resp.status, 500);
        assert_eq!(resp.body, Some(StatusError::handler_failure().to_bytes()));
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.response_entries), 1);
        assert_eq!(n(&c.finalized), 1);
    }
}
#[tokio::test]
async fn response_panic_and_failure_stop_chain_without_redispatch_or_partial_output() {
    for action in [Action::Panic, Action::Fail] {
        let c = Arc::new(Counts::default());
        let mut r = registry();
        for id in [HandlerId::ResponseHeader, HandlerId::ResponseCors] {
            install(
                &mut r,
                id,
                Binding::Response(Arc::new(Handler::new(id, action, &c))),
            );
        }
        let p = Pipeline::new(
            compile(
                config(
                    &[
                        (HandlerId::Proxy, "p"),
                        (HandlerId::ResponseHeader, "a"),
                        (HandlerId::ResponseCors, "b"),
                    ],
                    &["p", "a", "b"],
                ),
                &r,
            ),
            vec![c.clone()],
        );
        let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
        assert_eq!(resp, StatusError::handler_failure().response());
        assert_eq!(n(&c.backend), 1);
        assert_eq!(n(&c.response_entries), 1);
        assert_eq!(n(&c.response_completions), 1);
        assert_eq!(n(&c.finalized), 1);
    }
}
#[tokio::test]
async fn backend_panic_is_an_ordinary_gateway_error() {
    let c = Arc::new(Counts::default());
    let p = pipeline(
        None,
        Some(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &c,
        )),
        RequestCapabilities::default(),
        &c,
    );
    let mut b = FakeBackend::new(&c);
    b.panic = true;
    let resp = p.execute(invocation(), &b).await;
    assert_eq!(resp.status, 500);
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.response_completions), 1);
    assert_eq!(n(&c.finalized), 1);
}
struct PanicFinalizer;
impl Finalizer for PanicFinalizer {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        panic!("test-only finalizer");
    }
}
#[tokio::test]
async fn finalizer_panic_does_not_skip_remaining_finalization() {
    let c = Arc::new(Counts::default());
    let p = Pipeline::new(
        compile(config(&[(HandlerId::Proxy, "p")], &["p"]), &registry()),
        vec![c.clone(), Arc::new(PanicFinalizer)],
    );
    let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
    assert_eq!(resp.status, 200);
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.backend), 1);
}
