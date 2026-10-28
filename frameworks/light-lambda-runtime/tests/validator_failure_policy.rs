use light_lambda_runtime::{
    validator_failure_policy::ValidatorFailureDisposition, validator_ipc::Failure,
    validator_policy::FailureDisposition,
};
#[test]
fn validator_failure_classes_have_exact_static_dispositions() {
    let policy = ValidatorFailureDisposition;
    for failure in [
        Failure::Protocol,
        Failure::EngineInternal,
        Failure::AdapterInvariant,
    ] {
        let status = policy.reject(failure);
        assert_eq!(status.status_code, 500);
        assert_eq!(status.code, "ERR14004");
        assert_eq!(
            status.description,
            "Middleware thread unhandled runtime exception."
        );
    }
    for failure in [
        Failure::Frame,
        Failure::ParentBudget,
        Failure::RequestedQuota,
        Failure::AllocatorRefused,
        Failure::EngineResource,
        Failure::Death,
        Failure::NoReady,
        Failure::Rate,
        Failure::Stale,
    ] {
        let status = policy.reject(failure);
        assert_eq!(
            status.to_bytes(),
            r#"{"statusCode":503,"code":"ERR14034","message":"VALIDATOR_WORKER_UNAVAILABLE","description":"Validator worker is unavailable.","severity":"ERROR"}"#
        );
    }
    for failure in [Failure::Deadline, Failure::Cancelled] {
        let status = policy.reject(failure);
        assert_eq!(status.status_code, 504);
        assert_eq!(status.code, "ERR14017");
    }
}
