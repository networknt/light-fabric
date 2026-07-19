use model_provider::inference::{InferenceError, InferenceErrorCategory};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LlmGatewayError {
    #[error("configuration error: {0}")]
    Config(String),
    #[error("model is not available")]
    ModelUnavailable,
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
            Self::ModelUnavailable => 404,
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
            Self::ModelUnavailable => "model_not_found",
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
