use std::net::IpAddr;

use rkyv::{Archive, Deserialize, Serialize};
use uuid::Uuid;

use crate::WireError;
use crate::json::validate_json;

pub const MAX_SERVICE_ID_BYTES: usize = 256;
pub const MAX_ENV_TAG_BYTES: usize = 128;
pub const MAX_VERSION_BYTES: usize = 128;
pub const MAX_PROTOCOL_BYTES: usize = 32;
pub const MAX_ADDRESS_BYTES: usize = 253;
pub const MAX_TAGS: usize = 64;
pub const MAX_TAG_COMPONENT_BYTES: usize = 256;
pub const MAX_REQUEST_ID_BYTES: usize = 128;
pub const MAX_TOOL_NAME_BYTES: usize = 256;
pub const MAX_METHOD_BYTES: usize = 256;
pub const MAX_ERROR_MESSAGE_BYTES: usize = 1_024;
pub const MAX_DISCOVERY_NODES: usize = 10_000;

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WireTagV1 {
    pub key: String,
    pub value: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClientHelloV1 {
    pub service_id: String,
    pub env_tag: Option<String>,
    pub service_version: String,
    pub application_protocol: String,
    pub address: String,
    pub port: u16,
    pub tags: Vec<WireTagV1>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ServerHelloV1 {
    pub runtime_instance_id: Uuid,
    pub connection_id: Uuid,
    pub heartbeat_interval_ms: u32,
    pub max_control_payload_bytes: u32,
    pub max_command_streams: u32,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MetadataUpdateV1 {
    pub service_version: Option<String>,
    pub application_protocol: Option<String>,
    pub port: Option<u16>,
    pub tags: Option<Vec<WireTagV1>>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryRequestV1 {
    pub request_id: String,
    pub operation: u8,
    pub service_id: String,
    pub env_tag: Option<String>,
    pub application_protocol: Option<String>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryNodeV1 {
    pub runtime_instance_id: Uuid,
    pub service_id: String,
    pub env_tag: Option<String>,
    pub environment: String,
    pub service_version: String,
    pub application_protocol: String,
    pub address: String,
    pub port: u16,
    pub tags: Vec<WireTagV1>,
    pub connected_at_ms: i64,
    pub last_seen_at_ms: i64,
    pub connected: bool,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoverySnapshotV1 {
    pub service_id: String,
    pub env_tag: Option<String>,
    pub application_protocol: Option<String>,
    pub nodes: Vec<DiscoveryNodeV1>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WireErrorV1 {
    pub code: i32,
    pub message: String,
    pub data_json: Option<Vec<u8>>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryResponseV1 {
    pub request_id: String,
    pub snapshot: Option<DiscoverySnapshotV1>,
    pub error: Option<WireErrorV1>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryChangedV1 {
    pub snapshot: DiscoverySnapshotV1,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PingV1 {
    pub nonce: u64,
    pub timestamp_ms: i64,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PongV1 {
    pub nonce: u64,
    pub timestamp_ms: i64,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SessionErrorV1 {
    pub error: WireErrorV1,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ServerDrainingV1 {
    pub deadline_ms: i64,
    pub reason: String,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CommandRequestV1 {
    pub request_id: String,
    pub tool_name: String,
    pub arguments_json: Vec<u8>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CommandResponseV1 {
    pub request_id: String,
    pub completed_at_ms: i64,
    pub result_json: Option<Vec<u8>>,
    pub error: Option<WireErrorV1>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct RuntimeNotificationV1 {
    pub method: String,
    pub params_json: Vec<u8>,
    pub sequence: u64,
}

pub(crate) trait ValidateV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError>;
}

impl ValidateV1 for ClientHelloV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        required_string("service_id", &self.service_id, MAX_SERVICE_ID_BYTES)?;
        optional_string("env_tag", self.env_tag.as_deref(), MAX_ENV_TAG_BYTES)?;
        required_string("service_version", &self.service_version, MAX_VERSION_BYTES)?;
        required_string(
            "application_protocol",
            &self.application_protocol,
            MAX_PROTOCOL_BYTES,
        )?;
        validate_address(&self.address)?;
        nonzero_port("port", self.port)?;
        validate_tags(&self.tags)
    }
}

impl ValidateV1 for ServerHelloV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        nonnil_uuid("runtime_instance_id", self.runtime_instance_id)?;
        nonnil_uuid("connection_id", self.connection_id)?;
        nonzero_u32("heartbeat_interval_ms", self.heartbeat_interval_ms)?;
        nonzero_u32("max_control_payload_bytes", self.max_control_payload_bytes)?;
        nonzero_u32("max_command_streams", self.max_command_streams)
    }
}

impl ValidateV1 for MetadataUpdateV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        optional_nonempty_string(
            "service_version",
            self.service_version.as_deref(),
            MAX_VERSION_BYTES,
        )?;
        optional_nonempty_string(
            "application_protocol",
            self.application_protocol.as_deref(),
            MAX_PROTOCOL_BYTES,
        )?;
        if let Some(port) = self.port {
            nonzero_port("port", port)?;
        }
        if let Some(tags) = &self.tags {
            validate_tags(tags)?;
        }
        Ok(())
    }
}

impl ValidateV1 for DiscoveryRequestV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        required_string("request_id", &self.request_id, MAX_REQUEST_ID_BYTES)?;
        if !matches!(self.operation, 1..=3) {
            return Err(WireError::semantic(
                "operation",
                "must be 1 (lookup), 2 (subscribe), or 3 (unsubscribe)",
            ));
        }
        required_string("service_id", &self.service_id, MAX_SERVICE_ID_BYTES)?;
        optional_string("env_tag", self.env_tag.as_deref(), MAX_ENV_TAG_BYTES)?;
        optional_nonempty_string(
            "application_protocol",
            self.application_protocol.as_deref(),
            MAX_PROTOCOL_BYTES,
        )
    }
}

impl ValidateV1 for DiscoveryResponseV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        required_string("request_id", &self.request_id, MAX_REQUEST_ID_BYTES)?;
        exactly_one(
            "discovery_response",
            self.snapshot.is_some(),
            self.error.is_some(),
        )?;
        if let Some(snapshot) = &self.snapshot {
            snapshot.validate(max_json_bytes)?;
        }
        if let Some(error) = &self.error {
            error.validate(max_json_bytes)?;
        }
        Ok(())
    }
}

impl ValidateV1 for DiscoveryChangedV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        self.snapshot.validate(max_json_bytes)
    }
}

impl ValidateV1 for PingV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        Ok(())
    }
}

impl ValidateV1 for PongV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        Ok(())
    }
}

impl ValidateV1 for SessionErrorV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        self.error.validate(max_json_bytes)
    }
}

impl ValidateV1 for ServerDrainingV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        bounded_string("reason", &self.reason, MAX_ERROR_MESSAGE_BYTES)
    }
}

impl ValidateV1 for CommandRequestV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        required_string("request_id", &self.request_id, MAX_REQUEST_ID_BYTES)?;
        required_string("tool_name", &self.tool_name, MAX_TOOL_NAME_BYTES)?;
        validate_json(&self.arguments_json, max_json_bytes).map(|_| ())
    }
}

impl ValidateV1 for CommandResponseV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        required_string("request_id", &self.request_id, MAX_REQUEST_ID_BYTES)?;
        exactly_one(
            "command_response",
            self.result_json.is_some(),
            self.error.is_some(),
        )?;
        if let Some(result) = &self.result_json {
            validate_json(result, max_json_bytes)?;
        }
        if let Some(error) = &self.error {
            error.validate(max_json_bytes)?;
        }
        Ok(())
    }
}

impl ValidateV1 for RuntimeNotificationV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        required_string("method", &self.method, MAX_METHOD_BYTES)?;
        validate_json(&self.params_json, max_json_bytes).map(|_| ())
    }
}

impl ValidateV1 for DiscoverySnapshotV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        required_string("service_id", &self.service_id, MAX_SERVICE_ID_BYTES)?;
        optional_string("env_tag", self.env_tag.as_deref(), MAX_ENV_TAG_BYTES)?;
        optional_nonempty_string(
            "application_protocol",
            self.application_protocol.as_deref(),
            MAX_PROTOCOL_BYTES,
        )?;
        if self.nodes.len() > MAX_DISCOVERY_NODES {
            return Err(WireError::semantic(
                "nodes",
                format!("must contain at most {MAX_DISCOVERY_NODES} entries"),
            ));
        }
        for node in &self.nodes {
            node.validate(max_json_bytes)?;
        }
        Ok(())
    }
}

impl ValidateV1 for DiscoveryNodeV1 {
    fn validate(&self, _: usize) -> Result<(), WireError> {
        nonnil_uuid("node.runtime_instance_id", self.runtime_instance_id)?;
        required_string("node.service_id", &self.service_id, MAX_SERVICE_ID_BYTES)?;
        optional_string("node.env_tag", self.env_tag.as_deref(), MAX_ENV_TAG_BYTES)?;
        bounded_string("node.environment", &self.environment, MAX_ENV_TAG_BYTES)?;
        required_string(
            "node.service_version",
            &self.service_version,
            MAX_VERSION_BYTES,
        )?;
        required_string(
            "node.application_protocol",
            &self.application_protocol,
            MAX_PROTOCOL_BYTES,
        )?;
        validate_address(&self.address)?;
        nonzero_port("node.port", self.port)?;
        validate_tags(&self.tags)
    }
}

impl ValidateV1 for WireErrorV1 {
    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        bounded_string("error.message", &self.message, MAX_ERROR_MESSAGE_BYTES)?;
        if let Some(data) = &self.data_json {
            validate_json(data, max_json_bytes)?;
        }
        Ok(())
    }
}

fn validate_tags(tags: &[WireTagV1]) -> Result<(), WireError> {
    if tags.len() > MAX_TAGS {
        return Err(WireError::semantic(
            "tags",
            format!("must contain at most {MAX_TAGS} entries"),
        ));
    }
    let mut previous: Option<&str> = None;
    for tag in tags {
        required_string("tag.key", &tag.key, MAX_TAG_COMPONENT_BYTES)?;
        bounded_string("tag.value", &tag.value, MAX_TAG_COMPONENT_BYTES)?;
        if previous.is_some_and(|key| key >= tag.key.as_str()) {
            return Err(WireError::semantic(
                "tags",
                "keys must be sorted and unique",
            ));
        }
        previous = Some(tag.key.as_str());
    }
    Ok(())
}

fn validate_address(address: &str) -> Result<(), WireError> {
    required_string("address", address, MAX_ADDRESS_BYTES)?;
    if address.parse::<IpAddr>().is_ok() || valid_dns_name(address) {
        Ok(())
    } else {
        Err(WireError::semantic(
            "address",
            "must be an IP literal or DNS hostname",
        ))
    }
}

fn valid_dns_name(value: &str) -> bool {
    let value = value.strip_suffix('.').unwrap_or(value);
    !value.is_empty()
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn exactly_one(field: &'static str, first: bool, second: bool) -> Result<(), WireError> {
    if first ^ second {
        Ok(())
    } else {
        Err(WireError::semantic(
            field,
            "exactly one success or error value must be present",
        ))
    }
}

fn nonnil_uuid(field: &'static str, value: Uuid) -> Result<(), WireError> {
    if value.is_nil() {
        Err(WireError::semantic(field, "must not be nil"))
    } else {
        Ok(())
    }
}

fn required_string(field: &'static str, value: &str, max: usize) -> Result<(), WireError> {
    if value.is_empty() {
        return Err(WireError::semantic(field, "must not be empty"));
    }
    bounded_string(field, value, max)
}

fn optional_string(field: &'static str, value: Option<&str>, max: usize) -> Result<(), WireError> {
    if let Some(value) = value {
        bounded_string(field, value, max)?;
    }
    Ok(())
}

fn optional_nonempty_string(
    field: &'static str,
    value: Option<&str>,
    max: usize,
) -> Result<(), WireError> {
    if let Some(value) = value {
        required_string(field, value, max)?;
    }
    Ok(())
}

fn bounded_string(field: &'static str, value: &str, max: usize) -> Result<(), WireError> {
    if value.len() > max {
        Err(WireError::semantic(
            field,
            format!("must contain at most {max} UTF-8 bytes"),
        ))
    } else {
        Ok(())
    }
}

fn nonzero_port(field: &'static str, value: u16) -> Result<(), WireError> {
    if value == 0 {
        Err(WireError::semantic(field, "must be greater than zero"))
    } else {
        Ok(())
    }
}

fn nonzero_u32(field: &'static str, value: u32) -> Result<(), WireError> {
    if value == 0 {
        Err(WireError::semantic(field, "must be greater than zero"))
    } else {
        Ok(())
    }
}
