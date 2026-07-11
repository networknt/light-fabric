use std::collections::BTreeMap;

use chrono::Utc;
use execution_backend::{BackendError, BackendOutput};
use execution_runner_protocol::{
    AttemptState, CleanupState, CommandExecutionSpec, ExecuteLease, NormalizedExecutionResult,
    NormalizedOutput, RetrySafety,
};

pub fn from_backend_output(
    lease: &ExecuteLease,
    command: &CommandExecutionSpec,
    backend_operation_id: String,
    output: BackendOutput,
) -> NormalizedExecutionResult {
    let succeeded = output.exit_code == Some(0) && output.failure_class.is_none();
    NormalizedExecutionResult {
        execution_id: lease.lease.execution_id,
        origin: lease.lease.origin.clone(),
        subject: lease.lease.subject.clone(),
        attempt: lease.lease.attempt,
        state: if succeeded {
            AttemptState::Succeeded
        } else {
            AttemptState::Failed
        },
        failure_class: output.failure_class,
        exit_code: output.exit_code,
        signal: output.signal,
        started_at: output.started_at,
        finished_at: output.finished_at,
        duration_ms: duration_ms(output.started_at, output.finished_at),
        stdout: normalize_output(output.stdout, command.stdout_limit_bytes),
        stderr: normalize_output(output.stderr, command.stderr_limit_bytes),
        structured_output: output.structured_output,
        artifacts: Vec::new(),
        backend_operation_id,
        cleanup_state: CleanupState::Required,
        policy_digest: lease.lease.policy_digest.clone(),
        compatibility_digest: lease.lease.compatibility_digest.clone(),
        definition_digest: lease.definition_digest.clone(),
        command_template_digest: lease.command_template_digest.clone(),
        retry_safety: if succeeded {
            RetrySafety::Safe
        } else {
            RetrySafety::Unsafe
        },
        evidence: output.evidence,
    }
}

pub fn from_backend_error(
    lease: &ExecuteLease,
    backend_operation_id: String,
    started_at: chrono::DateTime<Utc>,
    error: &BackendError,
) -> NormalizedExecutionResult {
    let finished_at = Utc::now();
    let (state, failure_class, retry_safety) = match error {
        BackendError::Cancelled(_) => (
            AttemptState::Cancelled,
            "cancelled",
            RetrySafety::InspectRequired,
        ),
        BackendError::TimedOut(_) => (
            AttemptState::TimedOut,
            "deadline_exceeded",
            RetrySafety::InspectRequired,
        ),
        BackendError::Unknown(_) | BackendError::Transport(_) => (
            AttemptState::Unknown,
            "backend_outcome_unknown",
            RetrySafety::InspectRequired,
        ),
        BackendError::InvalidRequest(_) | BackendError::Unsupported(_) => (
            AttemptState::Failed,
            "execution_policy_rejected",
            RetrySafety::Safe,
        ),
        BackendError::NotFound(_) => (
            AttemptState::Unknown,
            "backend_operation_missing",
            RetrySafety::InspectRequired,
        ),
        BackendError::Cleanup(_) => (
            AttemptState::Unknown,
            "cleanup_failed",
            RetrySafety::InspectRequired,
        ),
    };
    NormalizedExecutionResult {
        execution_id: lease.lease.execution_id,
        origin: lease.lease.origin.clone(),
        subject: lease.lease.subject.clone(),
        attempt: lease.lease.attempt,
        state,
        failure_class: Some(failure_class.to_string()),
        exit_code: None,
        signal: None,
        started_at,
        finished_at,
        duration_ms: duration_ms(started_at, finished_at),
        stdout: empty_output(),
        stderr: empty_output(),
        structured_output: None,
        artifacts: Vec::new(),
        backend_operation_id,
        cleanup_state: CleanupState::Required,
        policy_digest: lease.lease.policy_digest.clone(),
        compatibility_digest: lease.lease.compatibility_digest.clone(),
        definition_digest: lease.definition_digest.clone(),
        command_template_digest: lease.command_template_digest.clone(),
        retry_safety,
        evidence: BTreeMap::from([("errorClass".to_string(), failure_class.to_string())]),
    }
}

fn normalize_output(bytes: Vec<u8>, maximum: u64) -> NormalizedOutput {
    let original_bytes = bytes.len() as u64;
    let redacted = redact(&String::from_utf8_lossy(&bytes));
    let bytes = redacted.as_bytes();
    let maximum = usize::try_from(maximum).unwrap_or(usize::MAX);
    let truncated = bytes.len() > maximum;
    let retained = if truncated { &bytes[..maximum] } else { bytes };
    let mut inline = String::from_utf8_lossy(retained).to_string();
    if truncated {
        inline.push_str("\n[TRUNCATED BY TRUSTED RUNNER]");
    }
    NormalizedOutput {
        inline: Some(inline),
        reference: None,
        truncated,
        original_bytes,
    }
}

fn redact(value: &str) -> String {
    value
        .lines()
        .map(|line| {
            let lower = line.to_ascii_lowercase();
            if [
                "authorization:",
                "bearer ",
                "access_token",
                "refresh_token",
                "api_key",
                "client_secret",
                "password=",
                "private key",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                "[REDACTED BY TRUSTED RUNNER]".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn empty_output() -> NormalizedOutput {
    NormalizedOutput {
        inline: Some(String::new()),
        reference: None,
        truncated: false,
        original_bytes: 0,
    }
}

fn duration_ms(started: chrono::DateTime<Utc>, finished: chrono::DateTime<Utc>) -> u64 {
    finished
        .signed_duration_since(started)
        .num_milliseconds()
        .max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_is_bounded_with_original_size_evidence() {
        let output = normalize_output(b"abcdef".to_vec(), 3);
        assert_eq!(
            output.inline.as_deref(),
            Some("abc\n[TRUNCATED BY TRUSTED RUNNER]")
        );
        assert!(output.truncated);
        assert_eq!(output.original_bytes, 6);
    }

    #[test]
    fn output_is_redacted_before_normalization() {
        let output = normalize_output(b"Authorization: Bearer secret".to_vec(), 1024);
        assert_eq!(
            output.inline.as_deref(),
            Some("[REDACTED BY TRUSTED RUNNER]")
        );
    }
}
