//! Request-phase state only; this does not emulate Java response/finalization flags.
use crate::StatusError;
#[derive(Clone, Debug, Default)]
pub(crate) enum RequestState {
    // ExchangeState is invocation-local and used after a typed request exists.
    #[default]
    Ready,
    Failed(StatusError),
}
impl RequestState {
    pub(crate) fn record(&mut self, status: StatusError) {
        // Native updateExchangeStatus: only ERROR severity marks failure,
        // and the first request failure cannot be replaced by later updates.
        if matches!(self, Self::Ready) && status.severity.starts_with("ERR") {
            *self = Self::Failed(status);
        }
    }
    pub(crate) fn failure(&self) -> Option<&StatusError> {
        match self {
            Self::Ready => None,
            Self::Failed(status) => Some(status),
        }
    }
    pub(crate) fn native_flags(&self) -> u16 {
        // LightLambdaExchange FLAG_REQUEST_SET and FLAG_REQUEST_HAS_FAILURE.
        // Derive the diagnostic from typed state, never accept caller bitmasks.
        (1 << 1) | if self.failure().is_some() { 1 << 3 } else { 0 }
    }
    pub(crate) fn routing_rejection(&self) -> Option<StatusError> {
        self.failure().map(|_| {
            StatusError::new(
                400,
                "ERR10087",
                "EXCHANGE_HAS_FAILED_STATE",
                &format!("Exchange has failed state {}.", self.native_flags()),
            )
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn request_failure_is_typed_sticky_and_redacted_in_router_status() {
        let mut state = RequestState::default();
        assert_eq!(state.native_flags(), 1 << 1);
        assert!(state.routing_rejection().is_none());
        state.record(StatusError::new(
            401,
            "ERR10000",
            "INVALID_AUTH_TOKEN",
            "test-only-private-cause",
        ));
        assert_eq!(state.native_flags(), (1 << 1) | (1 << 3));
        state.record(StatusError::handler_failure());
        assert_eq!(state.failure().unwrap().code, "ERR10000");
        let status = state.routing_rejection().unwrap();
        assert_eq!(status.status_code, 400);
        assert_eq!(status.code, "ERR10087");
        assert_eq!(
            status.to_bytes(),
            r#"{"statusCode":400,"code":"ERR10087","message":"EXCHANGE_HAS_FAILED_STATE","description":"Exchange has failed state 10.","severity":"ERROR"}"#
        );
        assert!(!String::from_utf8_lossy(&status.to_bytes()).contains("test-only-private-cause"));
    }
    #[test]
    fn nonerror_updates_and_warm_requests_do_not_inherit_failure() {
        let mut state = RequestState::default();
        let mut warning = StatusError::handler_failure();
        warning.severity = "WARN".into();
        state.record(warning);
        assert!(state.failure().is_none());
        state.record(StatusError::handler_failure());
        assert!(state.failure().is_some());
        assert!(RequestState::default().failure().is_none());
    }
}
