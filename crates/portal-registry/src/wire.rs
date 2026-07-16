//! Pure mappings between portal-registry application values and the shared
//! runtime wire profile. Transport negotiation remains disabled in Phase 2.

use controller_wire::v1::{ClientHelloV1, DiscoveryRequestV1, MetadataUpdateV1, WireTagV1};

use crate::protocol::{DiscoverySubscription, ServiceMetadataUpdate, ServiceRegistrationParams};

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

    use controller_wire::{DecodedMessageV1, decode_rkyv_frame_v1, encode_rkyv_frame_v1};

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

    fn decode_hex(value: &str) -> Vec<u8> {
        value
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect()
    }
}
