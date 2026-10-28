pub mod support;
use serverless_core::{plan::*, *};
use std::{sync::Arc, time::Duration};
use support::*;
use tokio::time::Instant;

fn budgets(request: u64, response: u64) -> Invocation {
    let mut i = invocation();
    i.deadline = Instant::now() + Duration::from_millis(request);
    i.response_deadline = Instant::now() + Duration::from_millis(response);
    i
}
#[tokio::test(start_paused = true)]
async fn eighty_percent_policy_leaves_only_twenty_percent_for_backend() {
    let c = Arc::new(Counts::default());
    let mut h = Handler::new(HandlerId::Correlation, Action::Continue, &c);
    h.delay = Duration::from_millis(80);
    let p = pipeline(Some(h), None, RequestCapabilities::default(), &c);
    let mut b = FakeBackend::new(&c);
    b.delay = Duration::from_millis(30);
    let start = Instant::now();
    let resp = p.execute(budgets(100, 150), &b).await;
    assert_eq!(resp.status, 504);
    assert!(start.elapsed() <= Duration::from_millis(101));
    assert!(c.remaining.lock().unwrap()[0] <= Duration::from_millis(20));
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test(start_paused = true)]
async fn exhausted_and_initially_expired_request_budgets_make_zero_calls() {
    for delay in [0, 100, 120] {
        let c = Arc::new(Counts::default());
        let mut h = Handler::new(HandlerId::Correlation, Action::Continue, &c);
        h.delay = Duration::from_millis(delay);
        let p = pipeline(Some(h), None, RequestCapabilities::default(), &c);
        let inv = budgets(if delay == 0 { 0 } else { 100 }, 150);
        let token = inv.cancel.clone();
        let resp = p.execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(resp.status, 504);
        assert!(token.is_cancelled());
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.finalized), 1);
    }
}
#[tokio::test(start_paused = true)]
async fn slow_response_discards_partial_changes_no_rerun_or_redispatch() {
    let c = Arc::new(Counts::default());
    let mut h = Handler::new(HandlerId::ResponseHeader, Action::Partial, &c);
    h.delay = Duration::from_millis(200);
    let p = pipeline(None, Some(h), RequestCapabilities::default(), &c);
    let start = Instant::now();
    let resp = p.execute(budgets(100, 150), &FakeBackend::new(&c)).await;
    assert_eq!(resp, StatusError::timeout().response());
    assert!(start.elapsed() <= Duration::from_millis(151));
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.response_entries), 1);
    assert_eq!(n(&c.response_completions), 0);
    assert_eq!(n(&c.finalized), 1);
}
#[tokio::test(start_paused = true)]
async fn canceled_request_retains_independent_bounded_response_budget() {
    for response_delay in [10, 100] {
        let c = Arc::new(Counts::default());
        let mut request = Handler::new(HandlerId::Correlation, Action::Continue, &c);
        request.delay = Duration::from_millis(200);
        let mut response =
            Handler::new(HandlerId::ResponseHeader, Action::ObserveResponseCancel, &c);
        response.delay = Duration::from_millis(response_delay);
        let p = pipeline(
            Some(request),
            Some(response),
            RequestCapabilities::default(),
            &c,
        );
        let start = Instant::now();
        let resp = p.execute(budgets(100, 150), &FakeBackend::new(&c)).await;
        assert_eq!(resp.status, 504);
        assert!(start.elapsed() <= Duration::from_millis(151));
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.finalized), 1);
        assert_eq!(n(&c.response_entries), 1);
        assert_eq!(
            n(&c.response_completions),
            usize::from(response_delay == 10)
        );
        assert_eq!(
            resp.headers.contains_key("x-response-policy"),
            response_delay == 10
        );
    }
}
#[tokio::test]
async fn aborting_outer_task_actually_finalizes_once() {
    let c = Arc::new(Counts::default());
    let mut h = Handler::new(HandlerId::Correlation, Action::Continue, &c);
    h.delay = Duration::from_secs(5);
    let p = pipeline(Some(h), None, RequestCapabilities::default(), &c);
    let b = FakeBackend::new(&c);
    let inv = invocation();
    let token = inv.cancel.clone();
    let task = tokio::spawn(async move { p.execute(inv, &b).await });
    tokio::time::timeout(Duration::from_secs(1), async {
        while n(&c.request_entries) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(token.is_cancelled());
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.backend), 0);
    assert_eq!(n(&c.request_completions), 0);
}
#[tokio::test(start_paused = true)]
async fn explicit_request_cancellation_still_gets_response_treatment() {
    let c = Arc::new(Counts::default());
    let p = pipeline(
        None,
        Some(Handler::new(
            HandlerId::ResponseHeader,
            Action::ObserveResponseCancel,
            &c,
        )),
        RequestCapabilities::default(),
        &c,
    );
    let inv = budgets(100, 150);
    inv.cancel.cancel();
    let resp = p.execute(inv, &FakeBackend::new(&c)).await;
    assert_eq!(resp.status, 504);
    assert_eq!(n(&c.backend), 0);
    assert_eq!(n(&c.response_completions), 1);
    assert_eq!(n(&c.finalized), 1);
}
