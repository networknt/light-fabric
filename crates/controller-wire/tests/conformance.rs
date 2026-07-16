use controller_wire::v1::*;
use controller_wire::{
    DecodedMessageV1, FRAME_HEADER_BYTES, FrameHeaderV1, LogicalChannel, MessageKindV1, WireError,
    decode_json_frame, decode_rkyv_frame_v1, decode_rkyv_frame_v1_on_channel, encode_json_frame,
    encode_rkyv_frame_v1,
};
use uuid::Uuid;

const LIMIT: usize = 1024 * 1024;

#[test]
fn json_length_prefix_round_trip_and_rejections() {
    let frame = encode_json_frame(br#"{"jsonrpc":"2.0","id":1}"#, LIMIT).unwrap();
    assert_eq!(
        decode_json_frame(&frame, LIMIT).unwrap(),
        r#"{"jsonrpc":"2.0","id":1}"#
    );
    assert_eq!(&frame[..4], &24_u32.to_be_bytes());

    assert!(matches!(
        decode_json_frame(&frame[..3], LIMIT),
        Err(WireError::Truncated { .. })
    ));
    assert!(matches!(
        decode_json_frame(&frame, 23),
        Err(WireError::PayloadTooLarge { .. })
    ));

    let mut truncated = frame.clone();
    truncated.pop();
    assert!(matches!(
        decode_json_frame(&truncated, LIMIT),
        Err(WireError::LengthMismatch { .. })
    ));

    let mut trailing = frame.clone();
    trailing.push(b' ');
    assert!(matches!(
        decode_json_frame(&trailing, LIMIT),
        Err(WireError::LengthMismatch { .. })
    ));
    assert!(matches!(
        encode_json_frame(b"", LIMIT),
        Err(WireError::EmptyJson)
    ));
    assert!(matches!(
        encode_json_frame(&[0xff], LIMIT),
        Err(WireError::InvalidUtf8)
    ));
    assert!(matches!(
        encode_json_frame(b"{} {}", LIMIT),
        Err(WireError::InvalidJson(_))
    ));

    let nested = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert!(matches!(
        encode_json_frame(nested.as_bytes(), LIMIT),
        Err(WireError::JsonDepthExceeded(64))
    ));
}

#[test]
fn frame_header_rejects_unknown_contract_fields_before_archive_access() {
    let valid = FrameHeaderV1::new(MessageKindV1::Ping, 8).unwrap().encode();
    assert_eq!(FrameHeaderV1::decode(&valid, 8).unwrap().payload_len, 8);

    let mut invalid = valid;
    invalid[0] = b'X';
    assert_eq!(
        FrameHeaderV1::decode(&invalid, LIMIT),
        Err(WireError::InvalidMagic)
    );

    let mut invalid = valid;
    invalid[4] = 2;
    assert_eq!(
        FrameHeaderV1::decode(&invalid, LIMIT),
        Err(WireError::UnsupportedVersion(2))
    );

    let mut invalid = valid;
    invalid[5] = 1;
    assert_eq!(
        FrameHeaderV1::decode(&invalid, LIMIT),
        Err(WireError::UnsupportedFlags(1))
    );

    let mut invalid = valid;
    invalid[6..8].copy_from_slice(&999_u16.to_le_bytes());
    assert_eq!(
        FrameHeaderV1::decode(&invalid, LIMIT),
        Err(WireError::UnknownMessageKind(999))
    );

    let mut invalid = valid;
    invalid[12] = 1;
    assert_eq!(
        FrameHeaderV1::decode(&invalid, LIMIT),
        Err(WireError::NonZeroReserved)
    );
}

#[test]
fn every_v1_root_round_trips_through_validated_access() {
    for message in samples() {
        let frame = encode_rkyv_frame_v1(&message, LIMIT).unwrap();
        assert!(frame.len() > FRAME_HEADER_BYTES);
        assert_eq!(decode_rkyv_frame_v1(&frame, LIMIT).unwrap(), message);
    }
}

#[test]
fn rkyv_parser_rejects_trailing_truncated_and_corrupt_payloads() {
    let frame = encode_rkyv_frame_v1(&samples()[0], LIMIT).unwrap();

    let mut trailing = frame.clone();
    trailing.push(0);
    assert!(matches!(
        decode_rkyv_frame_v1(&trailing, LIMIT),
        Err(WireError::LengthMismatch { .. })
    ));

    let truncated = &frame[..frame.len() - 1];
    assert!(matches!(
        decode_rkyv_frame_v1(truncated, LIMIT),
        Err(WireError::LengthMismatch { .. })
    ));

    let mut corrupt = frame;
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xff;
    assert!(matches!(
        decode_rkyv_frame_v1(&corrupt, LIMIT),
        Err(WireError::InvalidArchive { .. }) | Err(WireError::Semantic { .. })
    ));
}

#[test]
fn semantic_limits_are_enforced_after_archive_validation() {
    let unsorted = DecodedMessageV1::ClientHello(ClientHelloV1 {
        tags: vec![tag("z", "1"), tag("a", "2")],
        ..client_hello()
    });
    assert!(matches!(
        encode_rkyv_frame_v1(&unsorted, LIMIT),
        Err(WireError::Semantic { field: "tags", .. })
    ));

    let bad_json = DecodedMessageV1::CommandRequest(CommandRequestV1 {
        request_id: "request-1".into(),
        tool_name: "server/info".into(),
        arguments_json: b"{} {}".to_vec(),
    });
    assert!(matches!(
        encode_rkyv_frame_v1(&bad_json, LIMIT),
        Err(WireError::InvalidJson(_))
    ));

    let both = DecodedMessageV1::CommandResponse(CommandResponseV1 {
        request_id: "request-1".into(),
        completed_at_ms: 1_720_000_000_000,
        result_json: Some(b"{}".to_vec()),
        error: Some(wire_error()),
    });
    assert!(matches!(
        encode_rkyv_frame_v1(&both, LIMIT),
        Err(WireError::Semantic {
            field: "command_response",
            ..
        })
    ));
}

#[test]
fn message_kind_registry_enforces_logical_channels() {
    assert!(
        MessageKindV1::ClientHello
            .require_channel(LogicalChannel::SessionControl)
            .is_ok()
    );
    assert!(
        MessageKindV1::CommandRequest
            .require_channel(LogicalChannel::Command)
            .is_ok()
    );
    assert!(
        MessageKindV1::RuntimeNotification
            .require_channel(LogicalChannel::RuntimeEvents)
            .is_ok()
    );
    assert!(matches!(
        MessageKindV1::CommandResponse.require_channel(LogicalChannel::SessionControl),
        Err(WireError::InvalidLogicalChannel { .. })
    ));

    let command = encode_rkyv_frame_v1(&samples()[10], LIMIT).unwrap();
    assert!(matches!(
        decode_rkyv_frame_v1_on_channel(&command, LIMIT, LogicalChannel::SessionControl),
        Err(WireError::InvalidLogicalChannel { .. })
    ));
}

#[test]
fn committed_golden_bytes_are_stable_and_readable() {
    let expected = parse_golden(include_str!("../fixtures/runtime-rkyv-v1.hex"));
    let messages = samples();
    if std::env::var_os("PRINT_CONTROLLER_WIRE_GOLDEN").is_some() {
        for message in &messages {
            println!(
                "{}={}",
                message.kind() as u16,
                hex(&encode_rkyv_frame_v1(message, LIMIT).unwrap())
            );
        }
    }
    assert_eq!(expected.len(), messages.len());
    for message in messages {
        let bytes = expected
            .iter()
            .find_map(|(kind, bytes)| (*kind == message.kind() as u16).then_some(bytes))
            .unwrap_or_else(|| panic!("missing golden kind {}", message.kind() as u16));
        assert_eq!(encode_rkyv_frame_v1(&message, LIMIT).unwrap(), *bytes);
        assert_eq!(decode_rkyv_frame_v1(bytes, LIMIT).unwrap(), message);
    }
}

fn samples() -> Vec<DecodedMessageV1> {
    let snapshot = snapshot();
    vec![
        DecodedMessageV1::ClientHello(client_hello()),
        DecodedMessageV1::ServerHello(ServerHelloV1 {
            runtime_instance_id: uuid("018f47f7-5a8e-7bd1-9a18-24ea7f63f001"),
            connection_id: uuid("018f47f7-5a8e-7bd1-9a18-24ea7f63f002"),
            heartbeat_interval_ms: 30_000,
            max_control_payload_bytes: 1_048_576,
            max_command_streams: 32,
        }),
        DecodedMessageV1::MetadataUpdate(MetadataUpdateV1 {
            service_version: Some("1.2.4".into()),
            application_protocol: Some("https".into()),
            port: Some(8444),
            tags: Some(vec![tag("region", "ca-central-1")]),
        }),
        DecodedMessageV1::DiscoveryRequest(DiscoveryRequestV1 {
            request_id: "discovery-1".into(),
            operation: 1,
            service_id: "com.networknt.example-1.0.0".into(),
            env_tag: Some("dev".into()),
            application_protocol: Some("https".into()),
        }),
        DecodedMessageV1::DiscoveryResponse(DiscoveryResponseV1 {
            request_id: "discovery-1".into(),
            snapshot: Some(snapshot.clone()),
            error: None,
        }),
        DecodedMessageV1::DiscoveryChanged(DiscoveryChangedV1 {
            snapshot: snapshot.clone(),
        }),
        DecodedMessageV1::Ping(PingV1 {
            nonce: 42,
            timestamp_ms: 1_720_000_000_000,
        }),
        DecodedMessageV1::Pong(PongV1 {
            nonce: 42,
            timestamp_ms: 1_720_000_000_001,
        }),
        DecodedMessageV1::SessionError(SessionErrorV1 {
            error: wire_error(),
        }),
        DecodedMessageV1::ServerDraining(ServerDrainingV1 {
            deadline_ms: 1_720_000_030_000,
            reason: "planned replacement".into(),
        }),
        DecodedMessageV1::CommandRequest(CommandRequestV1 {
            request_id: "command-1".into(),
            tool_name: "server/info".into(),
            arguments_json: br#"{"verbose":true}"#.to_vec(),
        }),
        DecodedMessageV1::CommandResponse(CommandResponseV1 {
            request_id: "command-1".into(),
            completed_at_ms: 1_720_000_000_100,
            result_json: Some(br#"{"status":"ok"}"#.to_vec()),
            error: None,
        }),
        DecodedMessageV1::RuntimeNotification(RuntimeNotificationV1 {
            method: "notifications/log".into(),
            params_json: br#"{"level":"INFO","message":"ready"}"#.to_vec(),
            sequence: 7,
        }),
    ]
}

fn client_hello() -> ClientHelloV1 {
    ClientHelloV1 {
        service_id: "com.networknt.example-1.0.0".into(),
        env_tag: Some("dev".into()),
        service_version: "1.2.3".into(),
        application_protocol: "https".into(),
        address: "runtime.example.test".into(),
        port: 8443,
        tags: vec![tag("region", "ca-central-1"), tag("zone", "a")],
    }
}

fn snapshot() -> DiscoverySnapshotV1 {
    DiscoverySnapshotV1 {
        service_id: "com.networknt.example-1.0.0".into(),
        env_tag: Some("dev".into()),
        application_protocol: Some("https".into()),
        nodes: vec![DiscoveryNodeV1 {
            runtime_instance_id: uuid("018f47f7-5a8e-7bd1-9a18-24ea7f63f001"),
            service_id: "com.networknt.example-1.0.0".into(),
            env_tag: Some("dev".into()),
            environment: "dev".into(),
            service_version: "1.2.3".into(),
            application_protocol: "https".into(),
            address: "runtime.example.test".into(),
            port: 8443,
            tags: vec![tag("region", "ca-central-1")],
            connected_at_ms: 1_720_000_000_000,
            last_seen_at_ms: 1_720_000_000_500,
            connected: true,
        }],
    }
}

fn wire_error() -> WireErrorV1 {
    WireErrorV1 {
        code: -32_000,
        message: "runtime rejected command".into(),
        data_json: Some(br#"{"retryable":false}"#.to_vec()),
    }
}

fn tag(key: &str, value: &str) -> WireTagV1 {
    WireTagV1 {
        key: key.into(),
        value: value.into(),
    }
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_golden(input: &str) -> Vec<(u16, Vec<u8>)> {
    input
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (kind, bytes) = line.split_once('=').expect("kind=hex fixture line");
            let bytes = bytes.as_bytes();
            assert_eq!(bytes.len() % 2, 0, "even hex length");
            let decoded = bytes
                .chunks_exact(2)
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect();
            (kind.parse().unwrap(), decoded)
        })
        .collect()
}
