use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InferenceErrorCategory {
    InvalidRequest,
    Authentication,
    PermissionDenied,
    RateLimited,
    TimeoutBeforeAcceptance,
    TimeoutAfterPossibleAcceptance,
    ProviderOverload,
    Network,
    Protocol,
    Cancelled,
    UnsupportedFeature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetryDisposition {
    Never,
    Safe,
    Conditional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceEvidence {
    NotAccepted,
    PossiblyAccepted,
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Error)]
#[error("{category:?}: {detail}")]
#[serde(rename_all = "camelCase")]
pub struct InferenceError {
    pub category: InferenceErrorCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_status: Option<u16>,
    pub retry: RetryDisposition,
    pub acceptance: AcceptanceEvidence,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    pub detail: String,
}

impl InferenceError {
    pub fn invalid_request(detail: impl Into<String>) -> Self {
        Self {
            category: InferenceErrorCategory::InvalidRequest,
            provider_status: None,
            retry: RetryDisposition::Never,
            acceptance: AcceptanceEvidence::NotAccepted,
            retry_after_ms: None,
            detail: detail.into(),
        }
    }

    pub fn unsupported(detail: impl Into<String>) -> Self {
        Self {
            category: InferenceErrorCategory::UnsupportedFeature,
            provider_status: None,
            retry: RetryDisposition::Never,
            acceptance: AcceptanceEvidence::NotAccepted,
            retry_after_ms: None,
            detail: detail.into(),
        }
    }

    pub fn cancelled() -> Self {
        Self {
            category: InferenceErrorCategory::Cancelled,
            provider_status: None,
            retry: RetryDisposition::Never,
            acceptance: AcceptanceEvidence::PossiblyAccepted,
            retry_after_ms: None,
            detail: "provider request cancelled".to_string(),
        }
    }

    pub fn timeout_before_acceptance() -> Self {
        Self {
            category: InferenceErrorCategory::TimeoutBeforeAcceptance,
            provider_status: None,
            retry: RetryDisposition::Safe,
            acceptance: AcceptanceEvidence::NotAccepted,
            retry_after_ms: None,
            detail: "provider timed out before acceptance".to_string(),
        }
    }

    pub fn timeout_after_possible_acceptance() -> Self {
        Self {
            category: InferenceErrorCategory::TimeoutAfterPossibleAcceptance,
            provider_status: None,
            retry: RetryDisposition::Conditional,
            acceptance: AcceptanceEvidence::PossiblyAccepted,
            retry_after_ms: None,
            detail: "provider timed out after possible acceptance".to_string(),
        }
    }

    pub fn network(detail: impl Into<String>) -> Self {
        Self {
            category: InferenceErrorCategory::Network,
            provider_status: None,
            retry: RetryDisposition::Conditional,
            acceptance: AcceptanceEvidence::PossiblyAccepted,
            retry_after_ms: None,
            detail: detail.into(),
        }
    }

    pub fn protocol(detail: impl Into<String>) -> Self {
        Self::provider_protocol(None, detail)
    }

    pub fn provider_protocol(provider_status: Option<u16>, detail: impl Into<String>) -> Self {
        Self {
            category: InferenceErrorCategory::Protocol,
            provider_status,
            retry: RetryDisposition::Never,
            acceptance: AcceptanceEvidence::PossiblyAccepted,
            retry_after_ms: None,
            detail: detail.into(),
        }
    }

    pub fn from_status(status: u16, retry_after: Option<&str>, detail: impl Into<String>) -> Self {
        let (category, retry, acceptance) = match status {
            400 | 404 | 409 | 422 => (
                InferenceErrorCategory::InvalidRequest,
                RetryDisposition::Never,
                AcceptanceEvidence::NotAccepted,
            ),
            401 => (
                InferenceErrorCategory::Authentication,
                RetryDisposition::Never,
                AcceptanceEvidence::NotAccepted,
            ),
            403 => (
                InferenceErrorCategory::PermissionDenied,
                RetryDisposition::Never,
                AcceptanceEvidence::NotAccepted,
            ),
            429 => (
                InferenceErrorCategory::RateLimited,
                RetryDisposition::Safe,
                AcceptanceEvidence::NotAccepted,
            ),
            500..=599 => (
                InferenceErrorCategory::ProviderOverload,
                RetryDisposition::Conditional,
                AcceptanceEvidence::PossiblyAccepted,
            ),
            _ => (
                InferenceErrorCategory::Protocol,
                RetryDisposition::Never,
                AcceptanceEvidence::PossiblyAccepted,
            ),
        };
        Self {
            category,
            provider_status: Some(status),
            retry,
            acceptance,
            retry_after_ms: retry_after.and_then(parse_retry_after),
            detail: detail.into(),
        }
    }
}

fn parse_retry_after(value: &str) -> Option<u64> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return seconds.checked_mul(1_000);
    }
    let parsed = DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let target: SystemTime = parsed.with_timezone(&Utc).into();
    target
        .duration_since(SystemTime::now())
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_delta_retry_after() {
        let error = InferenceError::from_status(429, Some("3"), "limited");
        assert_eq!(error.retry_after_ms, Some(3_000));
        assert_eq!(error.retry, RetryDisposition::Safe);
    }

    #[test]
    fn parses_http_date_retry_after() {
        let future = (Utc::now() + chrono::Duration::minutes(1)).to_rfc2822();
        let error = InferenceError::from_status(429, Some(&future), "limited");
        assert!(error.retry_after_ms.is_some_and(|value| value <= 60_000));
    }

    #[test]
    fn ambiguous_failures_carry_acceptance_evidence() {
        for error in [
            InferenceError::timeout_after_possible_acceptance(),
            InferenceError::network("TLS failure"),
            InferenceError::protocol("drift"),
        ] {
            assert_eq!(error.acceptance, AcceptanceEvidence::PossiblyAccepted);
        }
    }
}
