//! Actual dispatch observation around the initialized production SDK backend.
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use serverless_core::*;
use std::sync::{Arc, Mutex};
#[derive(Default)]
pub(crate) struct Capture(Mutex<Vec<Option<String>>>);
struct BackendObserver(Arc<dyn Backend>, Arc<Capture>);
#[async_trait]
impl Backend for BackendObserver {
    async fn dispatch(
        &self,
        invocation: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.1.0.lock().unwrap().push(state.traceability_id.clone());
        // No trace value is supplied here: only the actual inherited log context is observed.
        tracing::info!(target:"light_lambda::trace_probe", message="dispatch context observation");
        self.0.dispatch(invocation, state, target).await
    }
}
impl Capture {
    pub fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
    pub fn observation(&self, before: usize) -> Result<Value> {
        let values = self.0.lock().unwrap();
        Ok(
            json!({"captures":values.len()-before,"exchangeTraceabilityId":values.last().cloned().flatten(),"logContextTraceabilityId":crate::audit_capture::trace_context()?}),
        )
    }
}
pub(crate) fn install(
    snapshot: &Arc<light_lambda_runtime::init::Snapshot>,
) -> (Arc<light_lambda_runtime::init::Snapshot>, Arc<Capture>) {
    let capture = Arc::new(Capture::default());
    let observer = capture.clone();
    let snapshot = light_lambda_runtime::init::with_backend_observer(snapshot, move |backend| {
        Arc::new(BackendObserver(backend, observer))
    });
    (snapshot, capture)
}
