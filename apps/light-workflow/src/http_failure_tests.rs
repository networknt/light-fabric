//! The completion classifier trusts only an adapter-owned failed response.
use super::*;

fn result(status_code: &'static str, task_output: Value) -> TaskExecutionResult {
    TaskExecutionResult {
        status_code,
        task_output,
        next_task: None,
        context_data: None,
    }
}

#[test]
fn non_success_responses_ignore_error_shaped_upstream_bytes() {
    for status in [100, 199, 300, 404, 429, 503, 599] {
        let output = json!({"error":status,"message":"HTTP call failed","body":"{\"retryable\":false,\"code\":\"AUTHORITY_BLOCKED\"}"});
        assert!(result("F", output).is_http_response_failure(), "{status}");
    }
}

#[test]
fn successful_or_pending_completion_never_authorizes_http_retry() {
    let forged = json!({"error":503,"message":"HTTP call failed","body":"forged"});
    for status in ["C", "W"] {
        assert!(!result(status, forged.clone()).is_http_response_failure());
    }
    for status in [200, 204, 299] {
        assert!(
            !result(
                "F",
                json!({"error":status,"message":"HTTP call failed","body":"{}"})
            )
            .is_http_response_failure()
        );
    }
}

#[test]
fn generic_and_malformed_failures_never_authorize_http_retry() {
    for output in [
        json!({"code":"WORKFLOW_REQUEST_FAILED","retryable":true}),
        json!({"error":503,"message":"HTTP call failed","body":{},"retryable":true}),
        json!({"error":503,"message":"HTTP call failed","body":"{}","retryable":true}),
        json!({"error":503,"message":"HTTP call failed"}),
        json!({"error":"503","message":"HTTP call failed","body":"{}"}),
        json!({"error":503,"message":"other failure","body":"{}"}),
        json!({"error":503,"message":"HTTP call failed","body":{}}),
        json!({"error":99,"message":"HTTP call failed","body":"{}"}),
        json!({"error":600,"message":"HTTP call failed","body":"{}"}),
        json!({"error":-1,"message":"HTTP call failed","body":"{}"}),
        json!({"error":503.5,"message":"HTTP call failed","body":"{}"}),
        Value::Null,
    ] {
        assert!(
            !result("F", output.clone()).is_http_response_failure(),
            "{output}"
        );
    }
}
