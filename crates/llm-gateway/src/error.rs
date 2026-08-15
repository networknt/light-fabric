use crate::reasoning_seal::ReasoningSealError;
use model_provider::inference::{InferenceError, InferenceErrorCategory};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LlmGatewayError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("route is not found")]
    RouteNotFound,
    #[error("model alias is not found")]
    AliasNotFound,
    #[error("requested capability is not supported: {0}")]
    UnsupportedCapability(String),
    #[error("no ready deployment can serve the request")]
    NoReadyDeployment,
    #[error("gateway invariant failed: {0}")]
    Invariant(String),
    #[error("request is not authorized")]
    Forbidden,
    #[error("request is invalid: {0}")]
    InvalidRequest(String),
    #[error("method is not allowed")]
    MethodNotAllowed,
    #[error("request media type is not supported")]
    UnsupportedMediaType,
    #[error("request body is too large")]
    PayloadTooLarge,
    #[error("request capacity is exhausted")]
    Capacity,
    #[error("request budget is exhausted")]
    Budget,
    #[error("audit admission failed")]
    AuditUnavailable,
    #[error("provider is unavailable")]
    ProviderUnavailable,
    #[error("provider request failed: {0}")]
    Provider(InferenceError),
    #[error("reasoning continuation failed: {0}")]
    ReasoningState(ReasoningSealError),
}

impl LlmGatewayError {
    fn public_mapping(&self) -> PublicErrorMapping {
        match self {
            Self::InvalidRequest(_) => PublicErrorMapping::new(400, "invalid_request"),
            Self::MethodNotAllowed => PublicErrorMapping::new(405, "method_not_allowed"),
            Self::UnsupportedMediaType => PublicErrorMapping::new(415, "unsupported_media_type"),
            Self::PayloadTooLarge => PublicErrorMapping::new(413, "payload_too_large"),
            Self::Forbidden => PublicErrorMapping::new(403, "permission_denied"),
            Self::RouteNotFound => PublicErrorMapping::new(404, "route_not_found"),
            Self::AliasNotFound => PublicErrorMapping::new(404, "model_not_found"),
            Self::UnsupportedCapability(_) => PublicErrorMapping::new(400, "unsupported_feature"),
            Self::NoReadyDeployment => PublicErrorMapping::new(503, "model_unavailable"),
            Self::Invariant(_) => PublicErrorMapping::new(500, "internal_error"),
            Self::Capacity => PublicErrorMapping::new(429, "capacity_exhausted"),
            Self::Budget => PublicErrorMapping::new(429, "budget_exhausted"),
            Self::Provider(error) => provider_public_mapping(error.category),
            Self::ReasoningState(error) => match error {
                ReasoningSealError::TooLarge => PublicErrorMapping::new(413, error.code()),
                ReasoningSealError::RouteUnavailable => PublicErrorMapping::new(409, error.code()),
                ReasoningSealError::KeyUnavailable
                | ReasoningSealError::KeyConfigInvalid
                | ReasoningSealError::Disabled => PublicErrorMapping::new(503, error.code()),
                ReasoningSealError::TooManyItems
                | ReasoningSealError::Invalid
                | ReasoningSealError::Required
                | ReasoningSealError::Tampered
                | ReasoningSealError::Stale => PublicErrorMapping::new(400, error.code()),
            },
            Self::Config(_) | Self::AuditUnavailable | Self::ProviderUnavailable => {
                PublicErrorMapping::new(503, "service_unavailable")
            }
        }
    }

    pub fn public_status(&self) -> u16 {
        self.public_mapping().status
    }

    pub fn public_code(&self) -> &'static str {
        self.public_mapping().code
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublicErrorMapping {
    status: u16,
    code: &'static str,
}

impl PublicErrorMapping {
    const fn new(status: u16, code: &'static str) -> Self {
        Self { status, code }
    }
}

fn provider_public_mapping(category: InferenceErrorCategory) -> PublicErrorMapping {
    match category {
        InferenceErrorCategory::InvalidRequest | InferenceErrorCategory::UnsupportedFeature => {
            PublicErrorMapping::new(400, "provider_error")
        }
        InferenceErrorCategory::Authentication | InferenceErrorCategory::PermissionDenied => {
            PublicErrorMapping::new(502, "provider_error")
        }
        InferenceErrorCategory::RateLimited => PublicErrorMapping::new(429, "rate_limit_exceeded"),
        InferenceErrorCategory::TimeoutBeforeAcceptance
        | InferenceErrorCategory::TimeoutAfterPossibleAcceptance => {
            PublicErrorMapping::new(504, "provider_error")
        }
        InferenceErrorCategory::ProviderOverload
        | InferenceErrorCategory::Network
        | InferenceErrorCategory::SecurityInvariant
        | InferenceErrorCategory::Protocol
        | InferenceErrorCategory::Cancelled => PublicErrorMapping::new(502, "provider_error"),
    }
}

#[cfg(test)]
mod tests {
    use super::LlmGatewayError;
    use model_provider::inference::{
        AcceptanceEvidence, InferenceError, InferenceErrorCategory, RetryDisposition,
    };

    #[test]
    fn route_alias_capability_and_readiness_remain_distinct() {
        let cases = [
            (LlmGatewayError::RouteNotFound, 404, "route_not_found"),
            (LlmGatewayError::AliasNotFound, 404, "model_not_found"),
            (
                LlmGatewayError::UnsupportedCapability("embed".to_string()),
                400,
                "unsupported_feature",
            ),
            (LlmGatewayError::NoReadyDeployment, 503, "model_unavailable"),
        ];
        for (error, status, code) in cases {
            assert_eq!(error.public_status(), status);
            assert_eq!(error.public_code(), code);
        }
    }

    #[test]
    fn every_provider_category_has_a_complete_public_mapping() {
        use InferenceErrorCategory as Category;

        let cases = [
            (Category::InvalidRequest, 400, "provider_error"),
            (Category::Authentication, 502, "provider_error"),
            (Category::PermissionDenied, 502, "provider_error"),
            (Category::RateLimited, 429, "rate_limit_exceeded"),
            (Category::TimeoutBeforeAcceptance, 504, "provider_error"),
            (
                Category::TimeoutAfterPossibleAcceptance,
                504,
                "provider_error",
            ),
            (Category::ProviderOverload, 502, "provider_error"),
            (Category::Network, 502, "provider_error"),
            (Category::SecurityInvariant, 502, "provider_error"),
            (Category::Protocol, 502, "provider_error"),
            (Category::Cancelled, 502, "provider_error"),
            (Category::UnsupportedFeature, 400, "provider_error"),
        ];

        for (category, status, code) in cases {
            let error = LlmGatewayError::Provider(InferenceError {
                category,
                provider_status: None,
                retry: RetryDisposition::Never,
                acceptance: AcceptanceEvidence::NotAccepted,
                retry_after_ms: None,
                detail: "test provider error".to_string(),
            });
            assert_eq!(error.public_status(), status, "{category:?}");
            assert_eq!(error.public_code(), code, "{category:?}");
        }
    }
}
