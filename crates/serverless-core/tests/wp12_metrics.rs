pub mod support;
use gateway_policy::observation::{RecordKind, RecordSink};
use serverless_core::{handlers::metrics::MetricsHandler, plan::*, *};
use std::sync::{Arc, Mutex};
use support::*;
#[derive(Default)]
struct Records(Mutex<Vec<(RecordKind, serde_json::Value)>>);
impl RecordSink for Records {
    fn try_emit(&self, kind: RecordKind, record: serde_json::Value) {
        self.0.lock().unwrap().push((kind, record));
    }
}
fn setup(records: &Arc<Records>, counts: &Arc<Counts>, deny_before: bool) -> Pipeline {
    let mut r = registry();
    r.register(
        HandlerId::Metrics,
        Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    let mut ids = vec![
        (HandlerId::Metrics, "metrics"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let mut sequence = vec!["metrics", "proxy", "response"];
    if deny_before {
        ids.insert(0, (HandlerId::RequestHeader, "deny"));
        sequence.insert(0, "deny");
        install(
            &mut r,
            HandlerId::RequestHeader,
            Binding::Request(
                Arc::new(Handler::new(
                    HandlerId::RequestHeader,
                    Action::Reject,
                    counts,
                )),
                RequestCapabilities::default(),
            ),
        );
    }
    Pipeline::new(compile(config(&ids, &sequence), &r), vec![counts.clone()])
}
#[tokio::test]
async fn metrics_real_policy_ordinary_success_and_backend_error_emit_once() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = setup(&records, &counts, false);
    for fail in [false, true] {
        let mut backend = FakeBackend::new(&counts);
        backend.failure = fail;
        let response = p.execute(invocation(), &backend).await;
        assert_eq!(response.status, if fail { 502 } else { 200 });
    }
    let data = records.0.lock().unwrap();
    assert_eq!(data.len(), 2);
    assert!(data.iter().all(|(kind, _)| *kind == RecordKind::Emf));
    assert_eq!(data[0].1["Requests"], 1);
    assert_eq!(data[0].1["Errors"], 0);
    assert_eq!(data[1].1["Errors"], 1);
    assert_eq!(n(&counts.backend), 2);
    assert_eq!(n(&counts.response_entries), 2);
    assert_eq!(n(&counts.finalized), 2);
}
#[tokio::test]
async fn selected_metrics_must_finalize_once_even_before_first_policy_deadline_or_denial() {
    for deadline in [false, true] {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let p = setup(&records, &counts, !deadline);
        let mut inv = invocation();
        if deadline {
            inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
        }
        let response = p.execute(inv, &FakeBackend::new(&counts)).await;
        assert!(response.status.is_client_error() || response.status.is_server_error());
        assert_eq!(n(&counts.backend), 0);
        assert_eq!(n(&counts.response_entries), 1);
        assert_eq!(n(&counts.finalized), 1);
        let records = records.0.lock().unwrap();
        assert_eq!(
            records.len(),
            1,
            "approved selected-metrics completion contract, deadline={deadline}"
        );
        assert_eq!(records[0].1["Requests"], 1);
        assert_eq!(records[0].1["Errors"], 1);
    }
}

#[tokio::test]
async fn selected_metrics_must_finalize_once_at_an_already_exhausted_request_deadline() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let p = setup(&records, &counts, false);
    let mut inv = invocation();
    inv.deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
    let response = p.execute(inv, &FakeBackend::new(&counts)).await;
    assert_eq!(response.status, 504);
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.response_entries), 1);
    assert_eq!(n(&counts.finalized), 1);
    let records = records.0.lock().unwrap();
    assert_eq!(
        records.len(),
        1,
        "approved M-01 exhausted-deadline completion contract"
    );
    assert_eq!(records[0].1["Requests"], 1);
    assert_eq!(records[0].1["Errors"], 1);
}

#[tokio::test]
async fn metrics_initial_cancellation_and_response_preparation_failures_are_final_outcomes() {
    for mode in 0..3 {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let p = setup(&records, &counts, false);
        let inv = invocation();
        if mode == 0 {
            inv.cancel.cancel();
        }
        let response = p
            .execute_prepared(inv, &FakeBackend::new(&counts), |response| match mode {
                1 => Err(StatusError::handler_failure()),
                2 => Ok(StatusError::timeout().response()),
                _ => Ok(response),
            })
            .await;
        assert!(response.status.is_server_error());
        assert_eq!(n(&counts.backend), usize::from(mode != 0));
        assert_eq!(n(&counts.response_entries), 1);
        assert_eq!(n(&counts.finalized), 1);
        assert_eq!(
            counts.finalized_responses.lock().unwrap()[0].status,
            response.status
        );
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1["Errors"], 1);
    }
}

#[tokio::test]
async fn metrics_selected_occurrences_emit_once_and_unselected_emit_nothing() {
    for selected in [false, true] {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let mut r = registry();
        r.register(
            HandlerId::Metrics,
            Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
        );
        let ids = [
            (HandlerId::Metrics, "first"),
            (HandlerId::Metrics, "second"),
            (HandlerId::Proxy, "proxy"),
        ];
        let sequence: &[&str] = if selected {
            &["first", "second", "proxy"]
        } else {
            &["proxy"]
        };
        let p = Pipeline::new(compile(config(&ids, sequence), &r), vec![counts.clone()]);
        for _ in 0..2 {
            assert_eq!(
                p.execute(invocation(), &FakeBackend::new(&counts))
                    .await
                    .status,
                200
            );
        }
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), if selected { 4 } else { 0 });
        assert!(
            records
                .iter()
                .all(|(_, record)| record["Requests"] == 1 && record["Errors"] == 0)
        );
        assert_eq!(n(&counts.backend), 2);
        assert_eq!(n(&counts.finalized), 2);
    }
}

#[tokio::test]
async fn metrics_request_and_response_timeouts_and_outer_drop_emit_once() {
    use std::time::Duration;
    for mode in 0..3 {
        let records = Arc::new(Records::default());
        let counts = Arc::new(Counts::default());
        let mut r = registry();
        r.register(
            HandlerId::Metrics,
            Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
        );
        let mut request = Handler::new(HandlerId::RequestHeader, Action::Continue, &counts);
        request.delay = if mode == 1 {
            Duration::ZERO
        } else {
            Duration::from_secs(5)
        };
        install(
            &mut r,
            HandlerId::RequestHeader,
            Binding::Request(Arc::new(request), RequestCapabilities::default()),
        );
        let mut response = Handler::new(HandlerId::ResponseHeader, Action::Continue, &counts);
        response.delay = if mode == 1 {
            Duration::from_secs(5)
        } else {
            Duration::ZERO
        };
        install(
            &mut r,
            HandlerId::ResponseHeader,
            Binding::Response(Arc::new(response)),
        );
        let ids = [
            (HandlerId::RequestHeader, "request"),
            (HandlerId::Metrics, "metrics"),
            (HandlerId::Proxy, "proxy"),
            (HandlerId::ResponseHeader, "response"),
        ];
        let p = Pipeline::new(
            compile(
                config(&ids, &["request", "metrics", "proxy", "response"]),
                &r,
            ),
            vec![counts.clone()],
        );
        let mut inv = invocation();
        inv.deadline = tokio::time::Instant::now() + Duration::from_millis(30);
        inv.response_deadline = tokio::time::Instant::now()
            + if mode == 1 {
                Duration::from_millis(60)
            } else {
                Duration::from_secs(1)
            };
        let backend = FakeBackend::new(&counts);
        if mode == 2 {
            assert!(
                tokio::time::timeout(Duration::from_millis(5), p.execute(inv, &backend))
                    .await
                    .is_err()
            );
        } else {
            assert_eq!(p.execute(inv, &backend).await.status, 504);
        }
        assert_eq!(n(&counts.backend), usize::from(mode == 1));
        assert_eq!(n(&counts.response_entries), usize::from(mode != 2));
        assert_eq!(n(&counts.finalized), 1);
        let records = records.0.lock().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].1["Errors"], 1);
    }
}

#[tokio::test]
async fn real_basic_authentication_denial_before_metrics_emits_once_without_dispatch() {
    use serverless_core::handlers::factors::BasicHandler;
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    let basic = BasicHandler::from_value(&serde_json::json!({"enabled":true,"enableAD":false,"users":[{"username":"test-user","password":"test-only-password","paths":["/x"]}]})).unwrap();
    install(
        &mut r,
        HandlerId::Basic,
        Binding::Request(Arc::new(basic), RequestCapabilities::default()),
    );
    r.register(
        HandlerId::Metrics,
        Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    let ids = [
        (HandlerId::Basic, "basic"),
        (HandlerId::Metrics, "metrics"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let p = Pipeline::new(
        compile(config(&ids, &["basic", "metrics", "proxy", "response"]), &r),
        vec![counts.clone()],
    );
    for auth in [None, Some("Basic test-user:wrong-test-password")] {
        let mut inv = invocation();
        if let Some(auth) = auth {
            inv.headers.insert("authorization", auth.parse().unwrap());
        }
        assert_eq!(p.execute(inv, &FakeBackend::new(&counts)).await.status, 401);
    }
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|(_, record)| record["Errors"] == 1));
    assert_eq!(n(&counts.backend), 0);
    assert_eq!(n(&counts.response_entries), 2);
    assert_eq!(n(&counts.finalized), 2);
}

struct PanickingCompletion;
impl Finalizer for PanickingCompletion {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        panic!("synthetic completion failure");
    }
}

struct OrderedPanic(Arc<Counts>);
impl Finalizer for OrderedPanic {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.0.mark("panic-callback");
        panic!("synthetic bounded completion failure");
    }
}
struct PreparedPanic(Arc<Counts>);
#[async_trait::async_trait]
impl RequestHandler for PreparedPanic {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    fn prepare_completion(&self, _: &Invocation) -> Option<Arc<dyn Finalizer>> {
        Some(Arc::new(OrderedPanic(self.0.clone())))
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state
            .completion
            .capture_prepared(Arc::new(OrderedPanic(self.0.clone())))
            .map_err(|_| HandlerFailure)?;
        Ok(HandlerOutcome::Continue)
    }
}
struct OrderedSink {
    records: Arc<Records>,
    counts: Arc<Counts>,
    panic: bool,
}
impl RecordSink for OrderedSink {
    fn try_emit(&self, kind: RecordKind, record: serde_json::Value) {
        self.counts.mark("metrics-callback");
        if self.panic {
            panic!("synthetic telemetry delivery failure");
        }
        self.records.try_emit(kind, record);
    }
}
#[tokio::test]
async fn completion_panics_in_both_orders_preserve_prepared_success_error_and_cancellation() {
    for metrics_first in [false, true] {
        for mode in 0..4 {
            for telemetry_panics in [false, true] {
                let records = Arc::new(Records::default());
                let counts = Arc::new(Counts::default());
                let mut r = registry();
                install(
                    &mut r,
                    HandlerId::RequestHeader,
                    Binding::Request(
                        Arc::new(PreparedPanic(counts.clone())),
                        RequestCapabilities::default(),
                    ),
                );
                r.register(
                    HandlerId::Metrics,
                    Arc::new(
                        MetricsHandler::new(
                            "test-only-gateway".into(),
                            Arc::new(OrderedSink {
                                records: records.clone(),
                                counts: counts.clone(),
                                panic: telemetry_panics,
                            }),
                        )
                        .unwrap(),
                    ),
                );
                install(
                    &mut r,
                    HandlerId::ResponseHeader,
                    Binding::Response(Arc::new(Handler::new(
                        HandlerId::ResponseHeader,
                        Action::Continue,
                        &counts,
                    ))),
                );
                let ids = [
                    (HandlerId::RequestHeader, "panic"),
                    (HandlerId::Metrics, "metrics"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseHeader, "response"),
                ];
                let sequence = if metrics_first {
                    &["panic", "metrics", "proxy", "response"]
                } else {
                    &["metrics", "panic", "proxy", "response"]
                };
                let p = Pipeline::new(compile(config(&ids, sequence), &r), vec![counts.clone()]);
                let inv = invocation();
                if mode == 2 {
                    inv.cancel.cancel();
                }
                let mut backend = FakeBackend::new(&counts);
                if mode == 3 {
                    backend.delay = std::time::Duration::from_secs(5);
                }
                let pending = p.execute_prepared(inv, &backend, |response| {
                    if mode == 1 {
                        Ok(StatusError::handler_failure().response())
                    } else {
                        Ok(response)
                    }
                });
                let response = if mode == 3 {
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(5), pending)
                            .await
                            .is_err()
                    );
                    counts.finalized_responses.lock().unwrap()[0].clone()
                } else {
                    pending.await
                };
                assert_eq!(
                    response.status,
                    match mode {
                        0 => 200,
                        1 => 500,
                        _ => 504,
                    }
                );
                assert_eq!(n(&counts.backend), usize::from(mode != 2));
                assert_eq!(n(&counts.response_entries), usize::from(mode != 3));
                assert_eq!(n(&counts.finalized), 1);
                assert_eq!(counts.finalized_responses.lock().unwrap()[0], response);
                let order = counts.order.lock().unwrap();
                let callbacks: Vec<_> = order
                    .iter()
                    .filter(|s| s.ends_with("callback"))
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    callbacks,
                    if metrics_first {
                        vec!["metrics-callback", "panic-callback"]
                    } else {
                        vec!["panic-callback", "metrics-callback"]
                    }
                );
                let data = records.0.lock().unwrap();
                assert_eq!(
                    data.len(),
                    usize::from(!telemetry_panics),
                    "panicking sink does not establish delivery"
                );
                if !telemetry_panics {
                    assert_eq!(data[0].1["Errors"], u8::from(mode != 0));
                }
            }
        }
    }
}
struct RegisterPanickingCompletion;
#[async_trait::async_trait]
impl RequestHandler for RegisterPanickingCompletion {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state
            .completion
            .add(Arc::new(PanickingCompletion))
            .map_err(|_| HandlerFailure)?;
        Ok(HandlerOutcome::Continue)
    }
}
#[tokio::test]
async fn completion_panic_must_not_change_outcome_after_metrics_observes_prepared_response() {
    let records = Arc::new(Records::default());
    let counts = Arc::new(Counts::default());
    let mut r = registry();
    install(
        &mut r,
        HandlerId::RequestHeader,
        Binding::Request(
            Arc::new(RegisterPanickingCompletion),
            RequestCapabilities::default(),
        ),
    );
    r.register(
        HandlerId::Metrics,
        Arc::new(MetricsHandler::new("test-only-gateway".into(), records.clone()).unwrap()),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            &counts,
        ))),
    );
    let ids = [
        (HandlerId::RequestHeader, "callback"),
        (HandlerId::Metrics, "metrics"),
        (HandlerId::Proxy, "proxy"),
        (HandlerId::ResponseHeader, "response"),
    ];
    let p = Pipeline::new(
        compile(
            config(&ids, &["callback", "metrics", "proxy", "response"]),
            &r,
        ),
        vec![counts.clone()],
    );
    let response = p.execute(invocation(), &FakeBackend::new(&counts)).await;
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.response_entries), 1);
    assert_eq!(n(&counts.finalized), 1);
    let records = records.0.lock().unwrap();
    assert_eq!(records.len(), 1);
    let returned_error = response.status.is_client_error() || response.status.is_server_error();
    assert_eq!(
        records[0].1["Errors"],
        u8::from(returned_error),
        "completion records must describe the returned prepared outcome, even when a later callback panics"
    );
}
