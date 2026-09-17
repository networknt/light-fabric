use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
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

/// Normalize a base path so that it starts with a slash and has no trailing slash. An empty string is returned
/// when the value is blank or is only a slash, which means the service is reached without a path prefix.
pub fn normalize_base_path(value: &str) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return String::new();
    }
    if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
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
