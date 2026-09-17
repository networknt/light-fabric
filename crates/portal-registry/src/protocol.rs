use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl JsonRpcMessage {
    pub fn new_request(id: Value, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn new_notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServiceRegistrationParams {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    pub version: String,
    pub protocol: String,
    pub address: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
    #[serde(rename = "envTag", skip_serializing_if = "Option::is_none")]
    pub env_tag: Option<String>,
    pub jwt: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ServiceMetadataUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySubscription {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    #[serde(rename = "envTag", skip_serializing_if = "Option::is_none")]
    pub env_tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryNode {
    #[serde(rename = "runtimeInstanceId")]
    pub runtime_instance_id: Uuid,
    #[serde(rename = "serviceId")]
    pub service_id: String,
    #[serde(rename = "envTag", default)]
    pub env_tag: Option<String>,
    pub environment: String,
    pub version: String,
    pub protocol: String,
    pub address: String,
    pub port: u16,
    #[serde(default)]
    pub tags: HashMap<String, String>,
    #[serde(rename = "connectedAt")]
    pub connected_at: DateTime<Utc>,
    #[serde(rename = "lastSeenAt")]
    pub last_seen_at: DateTime<Utc>,
    pub connected: bool,
}

/// Reserved registration tag that carries the base path of a service. It is used when the service is deployed
/// behind a path based k8s ingress where the namespace and the service are the path prefix of the url, and the
/// ingress removes that prefix before the request reaches the pod.
pub const BASE_PATH_TAG: &str = "basePath";

/// Reserved tags that describe how a service is reached. They are owned by the runtime rather than by the
/// application, so they survive a metadata update that publishes a complete tag map of its own.
pub const RESERVED_IDENTITY_TAGS: &[&str] = &[BASE_PATH_TAG];

/// Parse a base path into its normalized form, which starts with a slash and has no trailing slash. A blank
/// value yields an empty base path, which means the service is reached without a path prefix.
///
/// Only a path is accepted. A value that carries a query, a fragment, a relative segment, or anything else that
/// is not a path is rejected, because the base path is concatenated with the path of the request and with the
/// uri of an endpoint. For example, `/tenant?x=1` would otherwise turn the rest of the url into a query string.
pub fn parse_base_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    let path = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };
    if let Some(found) = path.chars().find(|character| {
        matches!(character, '?' | '#' | '\\') || character.is_whitespace() || character.is_control()
    }) {
        return Err(format!(
            "`{value}` must be a path, but it contains `{found}`"
        ));
    }
    if path.contains("//") {
        return Err(format!("`{value}` must not contain an empty path segment"));
    }
    if path.split('/').any(is_dot_segment) {
        return Err(format!(
            "`{value}` must not contain a relative path segment"
        ));
    }
    // The consumers build a url from this path, and the url parser normalizes what it considers a dot segment,
    // including its percent encoded spellings. A path that does not survive that parse unchanged would route to
    // a different path than the one validated here, so it is rejected rather than silently rewritten.
    match Url::parse(&format!("https://base.invalid{path}")) {
        Ok(url) if url.path() == path => Ok(path),
        Ok(url) => Err(format!(
            "`{value}` is not a normalized path, a url parser reads it as `{}`",
            url.path()
        )),
        Err(error) => Err(format!("`{value}` is not a usable path: {error}")),
    }
}

/// A path segment that a url parser resolves as the current or the parent directory, including the percent
/// encoded spellings of a dot, which a parser decodes before it resolves the segment.
fn is_dot_segment(segment: &str) -> bool {
    let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
    decoded == "." || decoded == ".."
}

/// Normalize a base path advertised by a node, ignoring a value that is not a usable path. A caller cannot fail
/// a request over a tag published by another service, so an invalid value is dropped and the node is reached
/// without a prefix instead of with a broken url.
pub fn normalize_base_path(value: &str) -> String {
    match parse_base_path(value) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(
                target: "portal_registry::protocol",
                error = %error,
                "ignoring invalid {BASE_PATH_TAG} tag"
            );
            String::new()
        }
    }
}

impl DiscoveryNode {
    /// The base path advertised by the node, normalized to start with a slash and to have no trailing slash. It
    /// is an empty string when the node does not advertise one, which is the case for every node that is reached
    /// by its address and port directly.
    pub fn base_path(&self) -> String {
        self.tags
            .get(BASE_PATH_TAG)
            .map(|value| normalize_base_path(value))
            .unwrap_or_default()
    }

    /// The base url of the node, including the base path when the node advertises one. A caller appends its own
    /// path to it, so the base path is kept in front of every request sent to the service.
    pub fn base_url(&self) -> String {
        let host = if self.address.contains(':') && !self.address.starts_with('[') {
            format!("[{}]", self.address)
        } else {
            self.address.clone()
        };
        format!(
            "{}://{}:{}{}",
            self.protocol.to_ascii_lowercase(),
            host,
            self.port,
            self.base_path()
        )
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySnapshot {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    #[serde(rename = "envTag", default)]
    pub env_tag: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    pub nodes: Vec<DiscoveryNode>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistrationResponse {
    #[serde(rename = "runtimeInstanceId")]
    pub runtime_instance_id: Uuid,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeregistrationParams {
    pub runtime_instance_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DeregistrationResponse {
    pub runtime_instance_id: Uuid,
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(tags: &[(&str, &str)]) -> DiscoveryNode {
        DiscoveryNode {
            runtime_instance_id: Uuid::nil(),
            service_id: "com.networknt.petstore-1.0.0".to_string(),
            env_tag: None,
            environment: "dev".to_string(),
            version: "1.0.0".to_string(),
            protocol: "https".to_string(),
            address: "api.example.com".to_string(),
            port: 443,
            tags: tags
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
                .collect(),
            connected_at: Utc::now(),
            last_seen_at: Utc::now(),
            connected: true,
        }
    }

    #[test]
    fn node_without_the_tag_has_no_base_path() {
        assert_eq!(node(&[]).base_path(), "");
        assert_eq!(node(&[("region", "ca")]).base_path(), "");
        assert_eq!(node(&[(BASE_PATH_TAG, "  ")]).base_path(), "");
        assert_eq!(node(&[(BASE_PATH_TAG, "/")]).base_path(), "");
        assert_eq!(node(&[]).base_url(), "https://api.example.com:443");
    }

    #[test]
    fn base_path_is_normalized() {
        assert_eq!(
            node(&[(BASE_PATH_TAG, "/namespace1/service1")]).base_path(),
            "/namespace1/service1"
        );
        assert_eq!(
            node(&[(BASE_PATH_TAG, "namespace1/service1/")]).base_path(),
            "/namespace1/service1"
        );
        assert_eq!(
            node(&[(BASE_PATH_TAG, " /namespace1/service1 ")]).base_path(),
            "/namespace1/service1"
        );
    }

    #[test]
    fn base_url_includes_the_base_path() {
        assert_eq!(
            node(&[(BASE_PATH_TAG, "/namespace1/service1")]).base_url(),
            "https://api.example.com:443/namespace1/service1"
        );
    }

    #[test]
    fn a_value_that_is_not_a_path_is_rejected() {
        for value in [
            "/tenant?x=1",
            "/tenant#fragment",
            "/tenant/../admin",
            "/tenant//service",
            "/tenant service",
            "/tenant\\service",
            // a url parser decodes a percent encoded dot before it resolves the segment, so these spellings
            // would route to a path other than the one validated here.
            "/namespace/%2e%2e/admin",
            "/namespace/%2E%2E/admin",
            "/namespace/%2e/admin",
            "/namespace/.%2e/admin",
        ] {
            assert!(parse_base_path(value).is_err(), "{value} must be rejected");
            assert_eq!(normalize_base_path(value), "");
            assert_eq!(node(&[(BASE_PATH_TAG, value)]).base_path(), "");
            assert_eq!(
                node(&[(BASE_PATH_TAG, value)]).base_url(),
                "https://api.example.com:443"
            );
        }
    }

    #[test]
    fn a_blank_value_is_not_an_error() {
        assert_eq!(parse_base_path("   "), Ok(String::new()));
        assert_eq!(parse_base_path("/"), Ok(String::new()));
    }

    #[test]
    fn an_encoded_segment_is_accepted() {
        assert_eq!(
            parse_base_path("/namespace1/service%201"),
            Ok("/namespace1/service%201".to_string())
        );
    }

    #[test]
    fn an_accepted_base_path_survives_url_parsing() {
        for value in [
            "/namespace1/service1",
            "/namespace1/service%201",
            "/tenant-a/api.v1",
            "/namespace1/service1/deep/path",
        ] {
            let parsed = parse_base_path(value).expect("valid base path");
            let url = Url::parse(&format!("https://api.example.com{parsed}")).expect("url");
            assert_eq!(url.path(), parsed, "{value} must not be rewritten");
        }
    }

    #[test]
    fn base_url_brackets_an_ipv6_address() {
        let mut node = node(&[(BASE_PATH_TAG, "/namespace1/service1")]);
        node.address = "2001:db8::1".to_string();
        node.port = 8443;

        assert_eq!(
            node.base_url(),
            "https://[2001:db8::1]:8443/namespace1/service1"
        );
    }
}
