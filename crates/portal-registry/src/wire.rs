//! Pure mappings between portal-registry application values and the shared
//! runtime wire profile. Transport negotiation and framing remain adapter
//! responsibilities.

use chrono::{DateTime, Utc};
use controller_wire::DecodedMessageV1;
use controller_wire::v1::{
    ClientHelloV1, CommandRequestV1, CommandResponseV1, DiscoveryChangedV1, DiscoveryNodeV1,
    DiscoveryRequestV1, DiscoveryResponseV1, DiscoverySnapshotV1, MetadataUpdateV1,
    RuntimeNotificationV1, ServerDrainingV1, SessionErrorV1, WireErrorV1, WireTagV1,
};
use serde_json::{Value, json};
use thiserror::Error;

use crate::logical::{RuntimeResponse, RuntimeSessionInput, RuntimeSessionOutput};
use crate::protocol::{
    DiscoveryNode, DiscoverySnapshot, DiscoverySubscription, JsonRpcError, ServiceMetadataUpdate,
    ServiceRegistrationParams,
};

#[derive(Debug, Error)]
pub(crate) enum WireMappingError {
    #[error("wire JSON field is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wire timestamp is outside the supported range")]
    InvalidTimestamp,
    #[error("unsupported logical runtime output {0}")]
    UnsupportedOutput(&'static str),
    #[error("unexpected controller wire message {0}")]
    UnexpectedMessage(&'static str),
}

impl From<&ServiceRegistrationParams> for ClientHelloV1 {
    fn from(value: &ServiceRegistrationParams) -> Self {
        Self {
            service_id: value.service_id.clone(),
            env_tag: value.env_tag.clone(),
            service_version: value.version.clone(),
            application_protocol: value.protocol.clone(),
            address: value.address.clone(),
            port: value.port,
            tags: sorted_tags(value.tags.iter()),
        }
    }
}

impl From<&ServiceMetadataUpdate> for MetadataUpdateV1 {
    fn from(value: &ServiceMetadataUpdate) -> Self {
        Self {
            service_version: value.version.clone(),
            application_protocol: value.protocol.clone(),
            port: value.port,
            tags: value.tags.as_ref().map(|tags| sorted_tags(tags.iter())),
        }
    }
}

pub fn discovery_request(
    request_id: impl Into<String>,
    operation: u8,
    value: &DiscoverySubscription,
) -> DiscoveryRequestV1 {
    DiscoveryRequestV1 {
        request_id: request_id.into(),
        operation,
        service_id: value.service_id.clone(),
        env_tag: value.env_tag.clone(),
        application_protocol: value.protocol.clone(),
    }
}

pub(crate) fn output_to_wire(
    output: RuntimeSessionOutput,
) -> Result<DecodedMessageV1, WireMappingError> {
    match output {
        RuntimeSessionOutput::Request {
            request_id,
            method,
            params,
        } => {
            let operation = match method.as_str() {
                "discovery/lookup" => 1,
                "discovery/subscribe" => 2,
                "discovery/unsubscribe" => 3,
                _ => return Err(WireMappingError::UnsupportedOutput("request")),
            };
            let subscription: DiscoverySubscription = serde_json::from_value(params)?;
            Ok(DecodedMessageV1::DiscoveryRequest(discovery_request(
                request_id_to_string(&request_id),
                operation,
                &subscription,
            )))
        }
        RuntimeSessionOutput::Notification { method, params }
            if method == "service/update_metadata" =>
        {
            let update: ServiceMetadataUpdate = serde_json::from_value(params)?;
            Ok(DecodedMessageV1::MetadataUpdate(MetadataUpdateV1::from(
                &update,
            )))
        }
        RuntimeSessionOutput::Notification { method, params } => {
            let method = method
                .strip_prefix("notifications/")
                .unwrap_or(method.as_str())
                .to_string();
            Ok(DecodedMessageV1::RuntimeNotification(
                RuntimeNotificationV1 {
                    method,
                    params_json: serde_json::to_vec(&params)?,
                    sequence: 0,
                },
            ))
        }
        RuntimeSessionOutput::Response {
            request_id,
            response,
        } => Ok(DecodedMessageV1::CommandResponse(command_response_to_wire(
            request_id_to_string(&request_id),
            response,
        )?)),
    }
}

pub(crate) fn input_from_wire(
    message: DecodedMessageV1,
) -> Result<RuntimeSessionInput, WireMappingError> {
    match message {
        DecodedMessageV1::DiscoveryResponse(response) => discovery_response_from_wire(response),
        DecodedMessageV1::DiscoveryChanged(DiscoveryChangedV1 { snapshot }) => {
            Ok(RuntimeSessionInput::Notification {
                method: "discovery/changed".to_string(),
                params: serde_json::to_value(discovery_snapshot_from_wire(snapshot)?)?,
            })
        }
        DecodedMessageV1::CommandRequest(command) => command_request_from_wire(command),
        DecodedMessageV1::SessionError(SessionErrorV1 { error }) => {
            Ok(RuntimeSessionInput::Notification {
                method: "session/error".to_string(),
                params: serde_json::to_value(json_rpc_error_from_wire(error)?)?,
            })
        }
        DecodedMessageV1::ServerDraining(ServerDrainingV1 {
            deadline_ms,
            reason,
        }) => Ok(RuntimeSessionInput::Notification {
            method: "session/draining".to_string(),
            params: json!({"deadlineMs": deadline_ms, "reason": reason}),
        }),
        DecodedMessageV1::Pong(_) => Ok(RuntimeSessionInput::Ignored),
        DecodedMessageV1::ClientHello(_) => {
            Err(WireMappingError::UnexpectedMessage("client_hello"))
        }
        DecodedMessageV1::ServerHello(_) => {
            Err(WireMappingError::UnexpectedMessage("server_hello"))
        }
        DecodedMessageV1::MetadataUpdate(_) => {
            Err(WireMappingError::UnexpectedMessage("metadata_update"))
        }
        DecodedMessageV1::DiscoveryRequest(_) => {
            Err(WireMappingError::UnexpectedMessage("discovery_request"))
        }
        DecodedMessageV1::Ping(_) => Err(WireMappingError::UnexpectedMessage("ping")),
        DecodedMessageV1::CommandResponse(_) => {
            Err(WireMappingError::UnexpectedMessage("command_response"))
        }
        DecodedMessageV1::RuntimeNotification(_) => {
            Err(WireMappingError::UnexpectedMessage("runtime_notification"))
        }
    }
}

fn command_request_from_wire(
    command: CommandRequestV1,
) -> Result<RuntimeSessionInput, WireMappingError> {
    let arguments: Value = serde_json::from_slice(&command.arguments_json)?;
    Ok(RuntimeSessionInput::Request {
        request_id: Value::String(command.request_id),
        method: "tools/call".to_string(),
        params: json!({"name": command.tool_name, "arguments": arguments}),
    })
}

fn command_response_to_wire(
    request_id: String,
    response: RuntimeResponse,
) -> Result<CommandResponseV1, WireMappingError> {
    let (result_json, error) = match (response.result, response.error) {
        (Some(result), None) => (Some(serde_json::to_vec(&result)?), None),
        (None, Some(error)) => (None, Some(json_rpc_error_to_wire(error)?)),
        _ => (
            None,
            Some(WireErrorV1 {
                code: -32_000,
                message: "invalid command response payload".to_string(),
                data_json: None,
            }),
        ),
    };
    Ok(CommandResponseV1 {
        request_id,
        completed_at_ms: Utc::now().timestamp_millis(),
        result_json,
        error,
    })
}

fn discovery_response_from_wire(
    response: DiscoveryResponseV1,
) -> Result<RuntimeSessionInput, WireMappingError> {
    let request_id = response.request_id;
    let response = if let Some(snapshot) = response.snapshot {
        RuntimeResponse {
            result: Some(serde_json::to_value(discovery_snapshot_from_wire(
                snapshot,
            )?)?),
            error: None,
        }
    } else {
        RuntimeResponse {
            result: None,
            error: Some(json_rpc_error_from_wire(
                response
                    .error
                    .expect("controller-wire validates discovery response union"),
            )?),
        }
    };
    Ok(RuntimeSessionInput::Response {
        request_id,
        response,
    })
}

fn discovery_snapshot_from_wire(
    snapshot: DiscoverySnapshotV1,
) -> Result<DiscoverySnapshot, WireMappingError> {
    Ok(DiscoverySnapshot {
        service_id: snapshot.service_id,
        env_tag: snapshot.env_tag,
        protocol: snapshot.application_protocol,
        nodes: snapshot
            .nodes
            .into_iter()
            .map(discovery_node_from_wire)
            .collect::<Result<_, _>>()?,
    })
}

fn discovery_node_from_wire(node: DiscoveryNodeV1) -> Result<DiscoveryNode, WireMappingError> {
    Ok(DiscoveryNode {
        runtime_instance_id: node.runtime_instance_id,
        service_id: node.service_id,
        env_tag: node.env_tag,
        environment: node.environment,
        version: node.service_version,
        protocol: node.application_protocol,
        address: node.address,
        port: node.port,
        tags: node
            .tags
            .into_iter()
            .map(|tag| (tag.key, tag.value))
            .collect(),
        connected_at: DateTime::<Utc>::from_timestamp_millis(node.connected_at_ms)
            .ok_or(WireMappingError::InvalidTimestamp)?,
        last_seen_at: DateTime::<Utc>::from_timestamp_millis(node.last_seen_at_ms)
            .ok_or(WireMappingError::InvalidTimestamp)?,
        connected: node.connected,
    })
}

fn json_rpc_error_to_wire(error: JsonRpcError) -> Result<WireErrorV1, WireMappingError> {
    Ok(WireErrorV1 {
        code: error.code,
        message: error.message,
        data_json: error
            .data
            .map(|data| serde_json::to_vec(&data))
            .transpose()?,
    })
}

fn json_rpc_error_from_wire(error: WireErrorV1) -> Result<JsonRpcError, WireMappingError> {
    Ok(JsonRpcError {
        code: error.code,
        message: error.message,
        data: error
            .data_json
            .map(|data| serde_json::from_slice(&data))
            .transpose()?,
    })
}

fn request_id_to_string(request_id: &Value) -> String {
    request_id
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| request_id.to_string())
}

fn sorted_tags<'a>(tags: impl Iterator<Item = (&'a String, &'a String)>) -> Vec<WireTagV1> {
    let mut tags: Vec<_> = tags
        .map(|(key, value)| WireTagV1 {
            key: key.clone(),
            value: value.clone(),
        })
        .collect();
    tags.sort_unstable_by(|left, right| left.key.cmp(&right.key));
    tags
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use chrono::TimeZone;
    use controller_wire::{DecodedMessageV1, decode_rkyv_frame_v1, encode_rkyv_frame_v1};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn registration_mapping_omits_jwt_and_sorts_tags() {
        let registration = ServiceRegistrationParams {
            service_id: "com.networknt.example-1.0.0".into(),
            version: "1.2.3".into(),
            protocol: "https".into(),
            address: "runtime.example.test".into(),
            port: 8443,
            tags: HashMap::from([("zone".into(), "a".into()), ("region".into(), "ca".into())]),
            env_tag: Some("dev".into()),
            jwt: "must-not-enter-wire-root".into(),
        };
        let hello = ClientHelloV1::from(&registration);
        assert_eq!(hello.tags[0].key, "region");
        assert_eq!(hello.tags[1].key, "zone");

        let message = DecodedMessageV1::ClientHello(hello);
        let frame = encode_rkyv_frame_v1(&message, 1024 * 1024).unwrap();
        assert_eq!(decode_rkyv_frame_v1(&frame, 1024 * 1024).unwrap(), message);
        assert!(
            !frame
                .windows(registration.jwt.len())
                .any(|bytes| bytes == registration.jwt.as_bytes())
        );
    }

    #[test]
    fn portal_registry_reads_every_shared_v1_golden_frame() {
        let fixture = include_str!("../../controller-wire/fixtures/runtime-rkyv-v1.hex");
        let mut count = 0;
        for line in fixture
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (_, bytes) = line.split_once('=').unwrap();
            let bytes = decode_hex(bytes);
            decode_rkyv_frame_v1(&bytes, 1024 * 1024).unwrap();
            count += 1;
        }
        assert_eq!(count, 13);
    }

    #[test]
    fn logical_outputs_map_to_expected_runtime_wire_kinds() {
        let discovery = output_to_wire(RuntimeSessionOutput::Request {
            request_id: json!("lookup-1"),
            method: "discovery/lookup".to_string(),
            params: serde_json::to_value(DiscoverySubscription {
                service_id: "user-service".to_string(),
                env_tag: Some("prod".to_string()),
                protocol: Some("https".to_string()),
            })
            .unwrap(),
        })
        .unwrap();
        assert!(matches!(discovery, DecodedMessageV1::DiscoveryRequest(_)));

        let metadata = output_to_wire(RuntimeSessionOutput::Notification {
            method: "service/update_metadata".to_string(),
            params: serde_json::to_value(ServiceMetadataUpdate {
                version: Some("1.0.1".to_string()),
                ..Default::default()
            })
            .unwrap(),
        })
        .unwrap();
        assert!(matches!(metadata, DecodedMessageV1::MetadataUpdate(_)));

        let notification = output_to_wire(RuntimeSessionOutput::Notification {
            method: "notifications/log".to_string(),
            params: json!({"line": "ready"}),
        })
        .unwrap();
        let DecodedMessageV1::RuntimeNotification(notification) = notification else {
            panic!("runtime notification")
        };
        assert_eq!(notification.method, "log");

        let command = output_to_wire(RuntimeSessionOutput::Response {
            request_id: json!("command-1"),
            response: RuntimeResponse {
                result: Some(json!({"status": "ok"})),
                error: None,
            },
        })
        .unwrap();
        assert!(matches!(command, DecodedMessageV1::CommandResponse(_)));
    }

    #[test]
    fn controller_wire_inputs_preserve_discovery_command_error_and_drain_semantics() {
        let node_id = Uuid::now_v7();
        let snapshot = DiscoverySnapshotV1 {
            service_id: "user-service".to_string(),
            env_tag: Some("prod".to_string()),
            application_protocol: Some("https".to_string()),
            nodes: vec![DiscoveryNodeV1 {
                runtime_instance_id: node_id,
                service_id: "user-service".to_string(),
                env_tag: Some("prod".to_string()),
                environment: "prod".to_string(),
                service_version: "1.0.0".to_string(),
                application_protocol: "https".to_string(),
                address: "127.0.0.1".to_string(),
                port: 8443,
                tags: Vec::new(),
                connected_at_ms: Utc
                    .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                    .unwrap()
                    .timestamp_millis(),
                last_seen_at_ms: Utc
                    .with_ymd_and_hms(2026, 1, 1, 0, 0, 1)
                    .unwrap()
                    .timestamp_millis(),
                connected: true,
            }],
        };
        let input = input_from_wire(DecodedMessageV1::DiscoveryResponse(DiscoveryResponseV1 {
            request_id: "lookup-1".to_string(),
            snapshot: Some(snapshot.clone()),
            error: None,
        }))
        .unwrap();
        let RuntimeSessionInput::Response {
            request_id,
            response,
        } = input
        else {
            panic!("discovery response")
        };
        assert_eq!(request_id, "lookup-1");
        assert_eq!(
            response.result.unwrap()["nodes"][0]["runtimeInstanceId"],
            json!(node_id)
        );

        let changed = input_from_wire(DecodedMessageV1::DiscoveryChanged(DiscoveryChangedV1 {
            snapshot,
        }))
        .unwrap();
        assert!(matches!(
            changed,
            RuntimeSessionInput::Notification { ref method, .. } if method == "discovery/changed"
        ));

        let command = input_from_wire(DecodedMessageV1::CommandRequest(CommandRequestV1 {
            request_id: "command-1".to_string(),
            tool_name: "check".to_string(),
            arguments_json: serde_json::to_vec(&json!({"verbose": true})).unwrap(),
        }))
        .unwrap();
        let RuntimeSessionInput::Request { method, params, .. } = command else {
            panic!("command request")
        };
        assert_eq!(method, "tools/call");
        assert_eq!(params["name"], "check");
        assert_eq!(params["arguments"]["verbose"], true);

        let error = input_from_wire(DecodedMessageV1::SessionError(SessionErrorV1 {
            error: WireErrorV1 {
                code: -32000,
                message: "session failed".to_string(),
                data_json: Some(serde_json::to_vec(&json!({"retry": false})).unwrap()),
            },
        }))
        .unwrap();
        assert!(matches!(
            error,
            RuntimeSessionInput::Notification { ref method, .. } if method == "session/error"
        ));

        let draining = input_from_wire(DecodedMessageV1::ServerDraining(ServerDrainingV1 {
            deadline_ms: 17,
            reason: "reload".to_string(),
        }))
        .unwrap();
        assert!(matches!(
            draining,
            RuntimeSessionInput::Notification { ref method, .. } if method == "session/draining"
        ));
    }

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
