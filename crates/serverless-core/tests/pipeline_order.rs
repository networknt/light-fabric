pub mod support;
use serverless_core::{plan::*, *};
use std::sync::Arc;
use support::*;

#[tokio::test]
async fn compiled_order_and_exactly_once_success_finalization() {
    let c = Arc::new(Counts::default());
    let p = pipeline(
        Some(Handler::new(HandlerId::Correlation, Action::Continue, &c)),
        Some(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &c,
        )),
        RequestCapabilities::default(),
        &c,
    );
    let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
    assert_eq!(resp.status, 200);
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.backend), 1);
    assert_eq!(
        *c.order.lock().unwrap(),
        [
            "request:Correlation",
            "backend",
            "response:ResponseHeader",
            "finalize"
        ]
    );
    assert_eq!(n(&c.request_entries), 1);
    assert_eq!(n(&c.request_completions), 1);
    assert_eq!(n(&c.response_entries), 1);
    assert_eq!(n(&c.response_completions), 1);
}
#[tokio::test]
async fn reject_and_respond_skip_remaining_request_handlers_and_dispatch() {
    for action in [Action::Reject, Action::Respond] {
        let c = Arc::new(Counts::default());
        let mut r = registry();
        install(
            &mut r,
            HandlerId::Correlation,
            Binding::Request(
                Arc::new(Handler::new(HandlerId::Correlation, action, &c)),
                RequestCapabilities::default(),
            ),
        );
        install(
            &mut r,
            HandlerId::Validator,
            Binding::Request(
                Arc::new(Handler::new(HandlerId::Validator, Action::Panic, &c)),
                RequestCapabilities::default(),
            ),
        );
        install(
            &mut r,
            HandlerId::ResponseHeader,
            Binding::Response(Arc::new(Handler::new(
                HandlerId::ResponseHeader,
                Action::Continue,
                &c,
            ))),
        );
        let p = Pipeline::new(
            compile(
                config(
                    &[
                        (HandlerId::Correlation, "first"),
                        (HandlerId::Validator, "later"),
                        (HandlerId::Proxy, "p"),
                        (HandlerId::ResponseHeader, "r"),
                    ],
                    &["first", "later", "p", "r"],
                ),
                &r,
            ),
            vec![c.clone()],
        );
        let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
        assert_eq!(
            resp.status,
            if matches!(action, Action::Reject) {
                401
            } else {
                200
            }
        );
        assert_eq!(n(&c.request_entries), 1);
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.finalized), 1);
        assert_eq!(n(&c.response_entries), 1);
        assert_eq!(n(&c.response_completions), 1);
        assert!(resp.headers.contains_key("x-response-policy"));
    }
}
#[tokio::test]
async fn backend_errors_receive_response_policies() {
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
    b.failure = true;
    let resp = p.execute(invocation(), &b).await;
    assert_eq!(resp.status, 502);
    assert!(resp.headers.contains_key("x-response-policy"));
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.response_completions), 1);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test]
async fn local_terminal_never_dispatches_and_continue_fails_closed() {
    for action in [Action::Respond, Action::Continue] {
        let c = Arc::new(Counts::default());
        let mut r = Registry::default();
        install(
            &mut r,
            HandlerId::Health,
            Binding::Local(Arc::new(Handler::new(HandlerId::Health, action, &c))),
        );
        let p = Pipeline::new(
            compile(config(&[(HandlerId::Health, "h")], &["h"]), &r),
            vec![c.clone()],
        );
        let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
        assert_eq!(
            resp.status,
            if matches!(action, Action::Respond) {
                200
            } else {
                500
            }
        );
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.finalized), 1);
    }
}
#[tokio::test]
async fn invocation_principal_headers_and_state_are_not_shared() {
    let c = Arc::new(Counts::default());
    let p = pipeline(
        Some(Handler::new(HandlerId::Correlation, Action::LeakCheck, &c)),
        None,
        RequestCapabilities::default(),
        &c,
    );
    let original = invocation();
    let b = FakeBackend::new(&c);
    let (a, b) = tokio::join!(
        p.execute(original.clone(), &b),
        p.execute(original.clone(), &b)
    );
    assert_eq!(a.status, 200);
    assert_eq!(b.status, 200);
    assert!(!original.headers.contains_key("x-mutated"));
    assert_eq!(
        *c.seen.lock().unwrap(),
        [(false, false, false), (false, false, false)]
    );
    assert_eq!(n(&c.finalized), 2);
}
#[tokio::test]
async fn declared_authentication_cannot_allow_identity_limit_without_a_principal() {
    let c = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Security,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Security, Action::Continue, &c)),
            RequestCapabilities {
                establishes_verified_identity: true,
                requires_verified_identity: false,
            },
        ),
    );
    install(
        &mut r,
        HandlerId::Limit,
        Binding::Request(
            Arc::new(Handler::new(HandlerId::Limit, Action::Continue, &c)),
            RequestCapabilities {
                establishes_verified_identity: false,
                requires_verified_identity: true,
            },
        ),
    );
    let p = Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Security, "a"),
                    (HandlerId::Limit, "l"),
                    (HandlerId::Proxy, "p"),
                ],
                &["a", "l", "p"],
            ),
            &r,
        ),
        vec![c.clone()],
    );
    let resp = p.execute(invocation(), &FakeBackend::new(&c)).await;
    assert_eq!(resp.status, 401);
    assert_eq!(n(&c.request_entries), 1);
    assert_eq!(n(&c.backend), 0);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test]
async fn compiled_path_and_default_plans_are_executed() {
    let c = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Health,
        Binding::Local(Arc::new(Handler::new(
            HandlerId::Health,
            Action::Respond,
            &c,
        ))),
    );
    let mut cfg = config(&[(HandlerId::Health, "h"), (HandlerId::Proxy, "p")], &["p"]);
    cfg["paths"] = serde_json::json!([{"path":"/health","method":"get","exec":["h"]}]);
    let p = Pipeline::new(compile(cfg, &r), vec![c.clone()]);
    let b = FakeBackend::new(&c);
    let mut inv = invocation();
    inv.path = "/health".into();
    assert_eq!(p.execute(inv, &b).await.body.unwrap(), "local");
    assert_eq!(n(&c.backend), 0);
    p.execute(invocation(), &b).await;
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.finalized), 2);
}

struct NamedFinalizer {
    name: &'static str,
    counts: Arc<Counts>,
}
impl Finalizer for NamedFinalizer {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.counts.mark(self.name);
    }
}
struct RegisterCompletion(Arc<Counts>);
#[async_trait::async_trait]
impl RequestHandler for RegisterCompletion {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        for name in ["first", "removed", "last"] {
            let id = st
                .completion
                .add(Arc::new(NamedFinalizer {
                    name,
                    counts: self.0.clone(),
                }))
                .unwrap();
            if name == "removed" {
                assert!(st.completion.remove(id).unwrap());
            }
        }
        Ok(HandlerOutcome::Reject(StatusError::missing_identity()))
    }
}
#[tokio::test]
async fn actual_pipeline_consumes_request_completion_callbacks_in_reverse_order() {
    let c = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::Correlation,
        Binding::Request(
            Arc::new(RegisterCompletion(c.clone())),
            RequestCapabilities::default(),
        ),
    );
    let p = Pipeline::new(
        compile(
            config(
                &[(HandlerId::Correlation, "c"), (HandlerId::Proxy, "p")],
                &["c", "p"],
            ),
            &r,
        ),
        vec![c.clone()],
    );
    p.execute(invocation(), &FakeBackend::new(&c)).await;
    assert_eq!(*c.order.lock().unwrap(), ["last", "first", "finalize"]);
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.backend), 0);
}
