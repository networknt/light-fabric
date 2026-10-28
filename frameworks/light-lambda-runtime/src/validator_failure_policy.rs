//! Proposed Validator-only public failure disposition. Not engine admission.
use crate::{validator_ipc::Failure, validator_policy::FailureDisposition};
use serverless_core::StatusError;
pub struct ValidatorFailureDisposition;
impl FailureDisposition for ValidatorFailureDisposition {
    fn reject(&self, failure: Failure) -> StatusError {
        match failure {
            Failure::Deadline | Failure::Cancelled => StatusError::timeout(),
            Failure::Protocol | Failure::EngineInternal | Failure::AdapterInvariant => {
                StatusError::handler_failure()
            }
            Failure::Frame
            | Failure::ParentBudget
            | Failure::RequestedQuota
            | Failure::AllocatorRefused
            | Failure::EngineResource
            | Failure::Death
            | Failure::NoReady
            | Failure::Rate
            | Failure::Stale => StatusError::new(
                503,
                "ERR14034",
                "VALIDATOR_WORKER_UNAVAILABLE",
                "Validator worker is unavailable.",
            ),
        }
    }
}
