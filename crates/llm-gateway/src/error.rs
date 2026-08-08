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
}

impl LlmGatewayError {
    pub fn public_status(&self) -> u16 {
        match self {
            Self::InvalidRequest(_) => 400,
            Self::MethodNotAllowed => 405,
            Self::UnsupportedMediaType => 415,
            Self::PayloadTooLarge => 413,
            Self::Forbidden => 403,
            Self::RouteNotFound | Self::AliasNotFound => 404,
            Self::UnsupportedCapability(_) => 400,
            Self::NoReadyDeployment => 503,
            Self::Invariant(_) => 500,
            Self::Capacity | Self::Budget => 429,
            Self::Provider(error) => match error.category {
                InferenceErrorCategory::InvalidRequest
                | InferenceErrorCategory::UnsupportedFeature => 400,
                InferenceErrorCategory::Authentication
                | InferenceErrorCategory::PermissionDenied => 502,
                InferenceErrorCategory::RateLimited => 429,
                InferenceErrorCategory::TimeoutBeforeAcceptance
                | InferenceErrorCategory::TimeoutAfterPossibleAcceptance => 504,
                _ => 502,
            },
            Self::Config(_) | Self::AuditUnavailable | Self::ProviderUnavailable => 503,
        }
    }

    pub fn public_code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::MethodNotAllowed => "method_not_allowed",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::PayloadTooLarge => "payload_too_large",
            Self::Forbidden => "permission_denied",
            Self::RouteNotFound => "route_not_found",
            Self::AliasNotFound => "model_not_found",
            Self::UnsupportedCapability(_) => "unsupported_feature",
            Self::NoReadyDeployment => "model_unavailable",
            Self::Invariant(_) => "internal_error",
            Self::Capacity => "capacity_exhausted",
            Self::Budget => "budget_exhausted",
            Self::Provider(error) if error.category == InferenceErrorCategory::RateLimited => {
                "rate_limit_exceeded"
            }
            Self::Provider(_) => "provider_error",
            Self::Config(_) | Self::AuditUnavailable | Self::ProviderUnavailable => {
                "service_unavailable"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LlmGatewayError;

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
}
