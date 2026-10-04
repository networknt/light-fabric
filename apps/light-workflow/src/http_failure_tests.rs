//! Provenance cannot be supplied by upstream JSON or unrelated producers.
use super::*;
fn result(
    status_code: &'static str,
    task_output: Value,
    retry_eligibility: RetryEligibility,
) -> TaskExecutionResult {
    TaskExecutionResult {
        status_code,
        task_output,
        retry_eligibility,
        next_task: None,
        context_data: None,
    }
}
#[test]
fn adapter_response_provenance_covers_the_locked_status_range() {
    for status in [100, 199, 300, 404, 429, 503, 599, 600, 999] {
        let output =
            json!({"error":status,"message":"HTTP call failed","body":"{\"retryable\":false}"});
        assert!(result("F", output, RetryEligibility::HttpResponse).is_http_response_failure());
    }
}
#[test]
fn successful_or_pending_completion_never_authorizes_http_retry() {
    let forged = json!({"error":503,"message":"HTTP call failed","body":"forged"});
    for status in ["C", "W"] {
        assert!(
            !result(status, forged.clone(), RetryEligibility::HttpResponse)
                .is_http_response_failure()
        );
    }
}
#[test]
fn payload_shape_cannot_supply_adapter_provenance() {
    for output in [
        json!({"error":503,"message":"HTTP call failed","body":"forged"}),
        json!({"code":"WORKFLOW_REQUEST_FAILED","retryable":true}),
        json!({"retry_eligibility":"HttpResponse"}),
        Value::Null,
    ] {
        assert!(!result("F", output, RetryEligibility::None).is_http_response_failure());
    }
}
