//! Pure MCP frontend-profile classification.
//!
//! This module deliberately has no access to the router runtime, session store,
//! HTTP client, or application tool core. Keeping classification here makes it
//! impossible for a rejected or disabled profile to mutate protocol state.

use serde_json::{Map as JsonMap, Value as JsonValue};

pub(crate) const STATELESS_RC_VERSION: &str = "DRAFT-2026-v1";
pub(crate) const STATELESS_PROTOCOL_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrontendProfile {
    Legacy,
    Stateless,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Classification {
    Legacy,
    Stateless { version: String, enabled: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClassificationRejection {
    MultipleProtocolVersionHeaders,
    ProtocolVersionMismatch,
    UnsupportedProtocolVersion(String),
    InvalidJsonRpcRequest,
    MissingLegacySession,
    MethodNotAllowed,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ClassifierConfig<'a> {
    pub legacy_enabled: bool,
    pub legacy_versions: &'a [String],
    pub stateless_enabled: bool,
    pub stateless_versions: &'a [String],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestHead<'a> {
    pub method: &'a str,
    pub protocol_versions: &'a [&'a str],
    pub session_id_present: bool,
}

pub(crate) fn classify_request_head(
    config: ClassifierConfig<'_>,
    head: RequestHead<'_>,
) -> Result<Option<Classification>, ClassificationRejection> {
    if head.protocol_versions.len() > 1 {
        return Err(ClassificationRejection::MultipleProtocolVersionHeaders);
    }
    let version = head.protocol_versions.first().copied();
    let method = head.method.to_ascii_uppercase();
    if method == "POST" {
        return Ok(None);
    }
    if version.is_some_and(is_known_stateless_version) {
        return Err(ClassificationRejection::MethodNotAllowed);
    }
    if let Some(version) = version {
        if config.legacy_enabled && contains(config.legacy_versions, version) {
            return Ok(Some(Classification::Legacy));
        }
        if contains(config.stateless_versions, version) {
            return Err(ClassificationRejection::MethodNotAllowed);
        }
        return Err(ClassificationRejection::UnsupportedProtocolVersion(
            version.to_string(),
        ));
    }
    let _ = head.session_id_present;
    Ok(Some(Classification::Legacy))
}

/// Classifies one already-parsed JSON-RPC POST without accessing runtime state.
pub(crate) fn classify_post(
    config: ClassifierConfig<'_>,
    protocol_versions: &[&str],
    session_id_present: bool,
    message: &JsonMap<String, JsonValue>,
) -> Result<Classification, ClassificationRejection> {
    if protocol_versions.len() > 1 {
        return Err(ClassificationRejection::MultipleProtocolVersionHeaders);
    }
    let header_version = protocol_versions.first().copied();
    let body_version = stateless_body_version(message);
    let claims_stateless = body_version.is_some()
        || header_version.is_some_and(is_known_stateless_version)
        || header_version.is_some_and(|version| contains(config.stateless_versions, version))
        || body_version.is_some_and(|version| contains(config.stateless_versions, version));

    if claims_stateless {
        let (Some(header_version), Some(body_version)) = (header_version, body_version) else {
            return Err(ClassificationRejection::ProtocolVersionMismatch);
        };
        if header_version != body_version {
            return Err(ClassificationRejection::ProtocolVersionMismatch);
        }
        if !is_known_stateless_version(header_version)
            && !contains(config.stateless_versions, header_version)
        {
            return Err(ClassificationRejection::UnsupportedProtocolVersion(
                header_version.to_string(),
            ));
        }
        return Ok(Classification::Stateless {
            version: header_version.to_string(),
            enabled: config.stateless_enabled
                && contains(config.stateless_versions, header_version),
        });
    }

    if let Some(version) = header_version
        && !(config.legacy_enabled && contains(config.legacy_versions, version))
    {
        return Err(ClassificationRejection::UnsupportedProtocolVersion(
            version.to_string(),
        ));
    }

    let method = message.get("method").and_then(JsonValue::as_str);
    if method.is_none() {
        return Err(ClassificationRejection::InvalidJsonRpcRequest);
    }
    if method == Some("initialize") {
        let requested = message
            .get("params")
            .and_then(JsonValue::as_object)
            .and_then(|params| params.get("protocolVersion"))
            .and_then(JsonValue::as_str);
        if let Some(version) = requested
            && !(config.legacy_enabled && contains(config.legacy_versions, version))
        {
            return Err(ClassificationRejection::UnsupportedProtocolVersion(
                version.to_string(),
            ));
        }
        return Ok(Classification::Legacy);
    }
    if !session_id_present {
        return Err(ClassificationRejection::MissingLegacySession);
    }
    Ok(Classification::Legacy)
}

fn stateless_body_version(message: &JsonMap<String, JsonValue>) -> Option<&str> {
    message
        .get("params")
        .and_then(JsonValue::as_object)
        .and_then(|params| params.get("_meta"))
        .and_then(JsonValue::as_object)
        .and_then(|meta| meta.get(STATELESS_PROTOCOL_META_KEY))
        .and_then(JsonValue::as_str)
}

fn contains(versions: &[String], version: &str) -> bool {
    versions.iter().any(|candidate| candidate == version)
}

fn is_known_stateless_version(version: &str) -> bool {
    version == STATELESS_RC_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> ClassifierConfig<'static> {
        static LEGACY: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(|| vec!["2025-11-25".to_string()]);
        static STATELESS: std::sync::LazyLock<Vec<String>> =
            std::sync::LazyLock::new(|| vec![STATELESS_RC_VERSION.to_string()]);
        ClassifierConfig {
            legacy_enabled: true,
            legacy_versions: &LEGACY,
            stateless_enabled: false,
            stateless_versions: &STATELESS,
        }
    }

    fn message(value: JsonValue) -> JsonMap<String, JsonValue> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn decision_table_freezes_legacy_and_stateless_post_boundaries() {
        let legacy_init = message(json!({
            "jsonrpc":"2.0", "id":1, "method":"initialize",
            "params":{"protocolVersion":"2025-11-25"}
        }));
        assert_eq!(
            classify_post(config(), &[], false, &legacy_init),
            Ok(Classification::Legacy)
        );

        let legacy_call = message(json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"}));
        assert_eq!(
            classify_post(config(), &["2025-11-25"], true, &legacy_call),
            Ok(Classification::Legacy)
        );
        assert_eq!(
            classify_post(config(), &[], false, &legacy_call),
            Err(ClassificationRejection::MissingLegacySession)
        );

        let stateless = message(json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/list",
            "params":{"_meta":{STATELESS_PROTOCOL_META_KEY:STATELESS_RC_VERSION}}
        }));
        assert_eq!(
            classify_post(config(), &[STATELESS_RC_VERSION], true, &stateless),
            Ok(Classification::Stateless {
                version: STATELESS_RC_VERSION.to_string(),
                enabled: false
            })
        );
        assert_eq!(
            classify_post(config(), &[], false, &stateless),
            Err(ClassificationRejection::ProtocolVersionMismatch)
        );
        assert_eq!(
            classify_post(config(), &[STATELESS_RC_VERSION], false, &legacy_call),
            Err(ClassificationRejection::ProtocolVersionMismatch)
        );
    }

    #[test]
    fn modern_delete_is_rejected_without_consulting_stale_session_state() {
        assert_eq!(
            classify_request_head(
                config(),
                RequestHead {
                    method: "DELETE",
                    protocol_versions: &[STATELESS_RC_VERSION],
                    session_id_present: true
                }
            ),
            Err(ClassificationRejection::MethodNotAllowed)
        );
    }

    #[test]
    fn duplicate_and_unsupported_versions_are_typed_rejections() {
        let call = message(json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"}));
        assert_eq!(
            classify_post(config(), &["2025-11-25", "2025-11-25"], true, &call),
            Err(ClassificationRejection::MultipleProtocolVersionHeaders)
        );
        assert_eq!(
            classify_post(config(), &["2099-01-01"], true, &call),
            Err(ClassificationRejection::UnsupportedProtocolVersion(
                "2099-01-01".to_string()
            ))
        );
        let unknown_stateless = message(json!({
            "jsonrpc":"2.0", "id":1, "method":"tools/list",
            "params":{"_meta":{STATELESS_PROTOCOL_META_KEY:"2099-01-01"}}
        }));
        assert_eq!(
            classify_post(config(), &["2099-01-01"], false, &unknown_stateless),
            Err(ClassificationRejection::UnsupportedProtocolVersion(
                "2099-01-01".to_string()
            ))
        );
    }
}
