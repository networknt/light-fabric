//! Bounded observation values. Request/business data is never mutated here.
use serde_json::{Map, Value, json};
pub const MAX_SELECTED_STRING_BYTES: usize = 1024;
pub const MAX_SELECTORS: usize = 16;
pub const SAFE_FIELDS: &[&str] = &[
    "client_id",
    "user_id",
    "scope_client_id",
    "endpoint",
    "serviceId",
    "toolName",
    "caller_id",
];
pub fn safe_header(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let name = name.to_ascii_lowercase();
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && ![
            "authorization",
            "cookie",
            "password",
            "secret",
            "token",
            "api-key",
            "apikey",
        ]
        .iter()
        .any(|unsafe_name| name.contains(unsafe_name))
}
pub fn bounded_value(value: &str) -> Value {
    if value.len() > MAX_SELECTED_STRING_BYTES {
        return json!("<truncated>");
    }
    // This is independent of optional Java masking. Sensitive recognizable
    // credentials cannot be opted into by disabling mask.
    let trimmed = value.trim();
    if trimmed.to_ascii_lowercase().starts_with("bearer ")
        || trimmed.to_ascii_lowercase().starts_with("basic ")
        || (trimmed.starts_with("eyJ") && trimmed.split('.').count() == 3)
    {
        json!("<redacted>")
    } else {
        json!(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AuditField {
    ClientId,
    UserId,
    ScopeClientId,
    Endpoint,
    ToolName,
    CallerId,
}
impl AuditField {
    pub fn name(self) -> &'static str {
        match self {
            Self::ClientId => "client_id",
            Self::UserId => "user_id",
            Self::ScopeClientId => "scope_client_id",
            Self::Endpoint => "endpoint",
            Self::ToolName => "toolName",
            Self::CallerId => "caller_id",
        }
    }
}
/// Only six approved metadata fields, each byte-bounded and redacted at insertion.
#[derive(Clone, Debug, Default)]
pub struct AuditFields(std::collections::BTreeMap<AuditField, Value>);
impl AuditFields {
    pub fn set(&mut self, field: AuditField, value: &str) {
        self.0.insert(field, bounded_value(value));
    }
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(field, _)| field.name() == name)
            .map(|(_, value)| value)
    }
}
pub fn insert_selected(map: &mut Map<String, Value>, name: &str, value: &str) {
    map.insert(name.to_owned(), bounded_value(value));
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn approved_byte_boundary_is_exact_for_ascii_and_utf8_and_credentials_are_redacted() {
        for (value, expected) in [
            ("a".repeat(1024), false),
            ("a".repeat(1025), true),
            ("é".repeat(512), false),
            ("é".repeat(513), true),
        ] {
            assert_eq!(
                bounded_value(&value),
                if expected {
                    json!("<truncated>")
                } else {
                    json!(value)
                }
            );
        }
        for value in [
            "Bearer synthetic-secret",
            "Basic synthetic-secret",
            "eyJsynthetic.eyJsynthetic.synthetic",
        ] {
            assert_eq!(bounded_value(value), json!("<redacted>"));
        }
        for name in [
            "Authorization",
            "proxy-authorization",
            "cookie",
            "set-cookie",
            "x-scope-token",
            "x-api-key",
            "x-secret",
        ] {
            assert!(!safe_header(name));
        }
        assert!(safe_header("x-test-header"));
    }
}
