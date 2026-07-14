/// Rule-context fields owned by the runtime rather than endpoint permission
/// configuration. Permission values remain available under `permission`, but
/// these keys must never be promoted over trusted top-level context values.
pub const RESERVED_RULE_CONTEXT_KEYS: &[&str] = &[
    "auditInfo",
    "headers",
    "endpoint",
    "toolName",
    "toolArguments",
    "correlationId",
    "permission",
    "responseBody",
    "responseBodyJson",
    "statusCode",
    "accessControl",
];

pub fn is_reserved_rule_context_key(key: &str) -> bool {
    RESERVED_RULE_CONTEXT_KEYS.contains(&key)
}
