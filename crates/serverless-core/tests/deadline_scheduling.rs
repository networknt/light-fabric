//! Deterministic scheduling control; does not replace the wall-clock D-08e fixture.
pub mod support;
use serverless_core::{plan::RequestCapabilities, *};
use std::{sync::Arc, time::Duration};
use support::*;

struct DescheduledBackend(FakeBackend);
#[async_trait::async_trait]
impl Backend for DescheduledBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        let response = self.0.dispatch(inv, state, target).await?;
        // Model resumption after OS descheduling without spending wall time.
        tokio::time::advance(Duration::from_millis(60)).await;
        Ok(response)
    }
}

#[tokio::test(start_paused = true)]
async fn resumption_after_response_deadline_does_not_enter_response_handler() {
    let counts = Arc::new(Counts::default());
    let response = Handler::new(HandlerId::ResponseHeader, Action::Continue, &counts);
    let pipeline = pipeline(
        None,
        Some(response),
        RequestCapabilities::default(),
        &counts,
    );
    let mut inv = invocation();
    let start = tokio::time::Instant::now();
    inv.deadline = start + Duration::from_millis(20);
    inv.response_deadline = start + Duration::from_millis(40);
    let response = pipeline
        .execute(inv, &DescheduledBackend(FakeBackend::new(&counts)))
        .await;
    assert_eq!(response, StatusError::timeout().response());
    assert_eq!(n(&counts.backend), 1);
    assert_eq!(n(&counts.response_entries), 0);
    assert_eq!(n(&counts.finalized), 1);
}
