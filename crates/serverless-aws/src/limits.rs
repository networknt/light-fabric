use serverless_core::StatusError;

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_request_bytes: usize,
    pub max_invoke_payload_bytes: usize,
    pub max_response_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1_048_576,
            max_invoke_payload_bytes: 6_291_456,
            max_response_bytes: 1_048_576,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.max_response_bytes < 4096 {
            return Err("light-lambda.maxResponseBytes must be at least 4096");
        }
        if self.max_request_bytes == 0 || self.max_invoke_payload_bytes == 0 {
            return Err("payload limits must be positive");
        }
        Ok(())
    }
}

// Trusted constant templates only; backend text is never passed to StatusError.
pub(crate) fn status(code: u16, detail: impl std::fmt::Display) -> StatusError {
    let (http, message, description) = match code {
        14010 => (
            400,
            "UNSUPPORTED_EVENT_FAMILY",
            "The gateway received an event that is not an ALB request.",
        ),
        14011 => (
            500,
            "ALB_MULTI_VALUE_HEADERS_REQUIRED",
            "The target group must enable multi-value headers.",
        ),
        14012 => (
            400,
            "INVALID_REQUEST_ENCODING",
            "Request method, path, query or body encoding is invalid.",
        ),
        14013 => (
            413,
            "PAYLOAD_TOO_LARGE",
            "Request payload exceeds %s bytes.",
        ),
        14014 => (502, "BACKEND_FUNCTION_ERROR", "Backend function %s failed."),
        14015 => (
            502,
            "BACKEND_RESPONSE_INVALID",
            "Backend function %s returned an invalid proxy response.",
        ),
        14016 => (
            502,
            "RESPONSE_TOO_LARGE",
            "Backend response exceeds the ingress limit of %s bytes.",
        ),
        14018 => (429, "BACKEND_THROTTLED", "Backend function is throttled."),
        14019 => (
            502,
            "BACKEND_UNAVAILABLE",
            "Backend function %s is missing or not permitted.",
        ),
        _ => return StatusError::timeout(),
    };
    StatusError::new(
        http,
        &format!("ERR{code}"),
        message,
        &description.replace("%s", &detail.to_string()),
    )
}
