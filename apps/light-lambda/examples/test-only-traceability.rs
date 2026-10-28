//! Test-only observation executable. Never the production bootstrap target.
//! Reuses the conformance observer hook around the real initialized SDK backend.
//! Only two fixed synthetic witnesses may appear in stdout; all other present
//! values are redacted. No state, headers, target, response or error is changed.
use async_trait::async_trait;
use light_lambda_runtime::{init, init_error::InitError, runtime, telemetry};
use serde_json::{Value, json};
use serverless_core::{Backend, DispatchError, ExchangeState, GatewayResponse, Invocation, Target};
use std::sync::Arc;

fn observation(state: &ExchangeState) -> Value {
    let value = match state.traceability_id.as_deref() {
        None => None,
        Some(value @ ("trace-probe-A" | "trace-probe-B")) => Some(value),
        Some(_) => Some("<redacted>"),
    };
    json!({"testOnlyObservation":"traceability-state-v1",
           "statePresent":state.traceability_id.is_some(),"traceabilityId":value})
}

struct ObserveBackend(Arc<dyn Backend>);
#[async_trait]
impl Backend for ObserveBackend {
    async fn dispatch(
        &self,
        invocation: &Invocation,
        state: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        println!("{}", observation(state));
        self.0.dispatch(invocation, state, target).await
    }
}

#[tokio::main]
async fn main() {
    telemetry::install_panic_redaction();
    if let Err(error) = telemetry::initialize() {
        runtime::fail(error).await;
    }
    let input = match std::panic::catch_unwind(|| light_lambda::input(None, true))
        .unwrap_or(Err(InitError::Panic))
    {
        Ok(input) => input,
        Err(error) => runtime::fail(error).await,
    };
    let snapshot = match init::initialize(input).await {
        Ok(snapshot) => snapshot,
        Err(error) => runtime::fail(error).await,
    };
    let observed =
        init::with_backend_observer(&snapshot, |backend| Arc::new(ObserveBackend(backend)));
    if runtime::run_with_guard(observed, Some(light_lambda::phase1_guard()))
        .await
        .is_err()
    {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn observation_preserves_absence_and_only_two_synthetic_witnesses() {
        let mut state = ExchangeState::default();
        assert_eq!(observation(&state)["traceabilityId"], Value::Null);
        assert_eq!(observation(&state)["statePresent"], false);
        for value in ["trace-probe-A", "trace-probe-B"] {
            state.traceability_id = Some(value.into());
            assert_eq!(observation(&state)["traceabilityId"], value);
            assert_eq!(observation(&state)["statePresent"], true);
        }
    }
    #[test]
    fn observation_redacts_empty_arbitrary_long_and_credential_values() {
        for value in [
            "".to_owned(),
            "Bearer PRIVATE-TOKEN".into(),
            "x".repeat(100_000),
        ] {
            let mut state = ExchangeState::default();
            state.traceability_id = Some(value.clone());
            let observed = observation(&state);
            assert_eq!(observed["statePresent"], true);
            assert_eq!(observed["traceabilityId"], "<redacted>");
            assert!(observed.to_string().len() < 160);
            assert_eq!(state.traceability_id.as_deref(), Some(value.as_str()));
        }
    }
}
