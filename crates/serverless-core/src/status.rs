use crate::GatewayResponse;
use bytes::Bytes;
use http::{HeaderMap, HeaderValue, StatusCode, header::CONTENT_TYPE};
use serde_json::Value;

/// Java Status.toString byte contract, including its unescaped string fields.
/// This is intentionally NOT serde JSON serialization. Only trusted status
/// templates belong here; never pass backend diagnostics or untrusted messages.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusError {
    pub status_code: u16,
    pub code: String,
    pub message: String,
    pub description: String,
    pub severity: String,
    /// Preserve metadata insertion order explicitly rather than relying on JSON map features.
    pub metadata: Option<Box<Vec<(String, Value)>>>,
}
impl StatusError {
    pub fn specification_path_too_large() -> Self {
        Self::new(
            414,
            "ERR14029",
            "SPECIFICATION_PATH_TOO_LARGE",
            "Request path exceeds the supported Specification size.",
        )
    }
    pub fn new(status_code: u16, code: &str, message: &str, description: &str) -> Self {
        Self {
            status_code,
            code: code.into(),
            message: message.into(),
            description: description.into(),
            severity: "ERROR".into(),
            metadata: None,
        }
    }
    pub fn to_bytes(&self) -> Bytes {
        let mut s = format!(
            "{{\"statusCode\":{},\"code\":\"{}\",\"message\":\"{}\",\"description\":\"{}\"",
            self.status_code, self.code, self.message, self.description
        );
        if let Some(metadata) = &self.metadata {
            s.push_str(",\"metadata\":{");
            for (i, (key, value)) in metadata.iter().enumerate() {
                if i != 0 {
                    s.push(',');
                }
                s.push_str(
                    &serde_json::to_string(key).expect("string serialization is infallible"),
                );
                s.push(':');
                s.push_str(&value.to_string());
            }
            s.push('}');
        }
        s.push_str(&format!(",\"severity\":\"{}\"}}", self.severity));
        Bytes::from(s)
    }
    pub fn response(&self) -> GatewayResponse {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        GatewayResponse {
            status: StatusCode::from_u16(self.status_code)
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            headers,
            body: Some(self.to_bytes()),
            body_is_binary: false,
        }
    }
    pub fn handler_failure() -> Self {
        Self::new(
            500,
            "ERR14004",
            "MIDDLEWARE_UNHANDLED_EXCEPTION",
            "Middleware thread unhandled runtime exception.",
        )
    }
    pub fn timeout() -> Self {
        Self::new(
            504,
            "ERR14017",
            "BACKEND_TIMEOUT",
            "Backend did not respond within the request deadline.",
        )
    }
    pub fn missing_identity() -> Self {
        Self::new(
            401,
            "ERR10002",
            "MISSING_AUTH_TOKEN",
            "No Authorization header or the token is not bearer type",
        )
    }
    pub fn missing_target() -> Self {
        Self::new(
            400,
            "ERR10086",
            "FAILED_TO_INVOKE_LAMBDA",
            "Failed to invoke lambda function null.",
        )
    }
}
