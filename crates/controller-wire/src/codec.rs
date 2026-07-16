use rkyv::rancor::Error as RkyvError;
use rkyv::util::AlignedVec;

use crate::frame::FRAME_HEADER_BYTES;
use crate::v1::{
    ClientHelloV1, CommandRequestV1, CommandResponseV1, DiscoveryChangedV1, DiscoveryRequestV1,
    DiscoveryResponseV1, MetadataUpdateV1, PingV1, PongV1, RuntimeNotificationV1, ServerDrainingV1,
    ServerHelloV1, SessionErrorV1, ValidateV1,
};
use crate::{FrameHeaderV1, LogicalChannel, MessageKindV1, WireError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedMessageV1 {
    ClientHello(ClientHelloV1),
    ServerHello(ServerHelloV1),
    MetadataUpdate(MetadataUpdateV1),
    DiscoveryRequest(DiscoveryRequestV1),
    DiscoveryResponse(DiscoveryResponseV1),
    DiscoveryChanged(DiscoveryChangedV1),
    Ping(PingV1),
    Pong(PongV1),
    SessionError(SessionErrorV1),
    ServerDraining(ServerDrainingV1),
    CommandRequest(CommandRequestV1),
    CommandResponse(CommandResponseV1),
    RuntimeNotification(RuntimeNotificationV1),
}

impl DecodedMessageV1 {
    pub const fn kind(&self) -> MessageKindV1 {
        match self {
            Self::ClientHello(_) => MessageKindV1::ClientHello,
            Self::ServerHello(_) => MessageKindV1::ServerHello,
            Self::MetadataUpdate(_) => MessageKindV1::MetadataUpdate,
            Self::DiscoveryRequest(_) => MessageKindV1::DiscoveryRequest,
            Self::DiscoveryResponse(_) => MessageKindV1::DiscoveryResponse,
            Self::DiscoveryChanged(_) => MessageKindV1::DiscoveryChanged,
            Self::Ping(_) => MessageKindV1::Ping,
            Self::Pong(_) => MessageKindV1::Pong,
            Self::SessionError(_) => MessageKindV1::SessionError,
            Self::ServerDraining(_) => MessageKindV1::ServerDraining,
            Self::CommandRequest(_) => MessageKindV1::CommandRequest,
            Self::CommandResponse(_) => MessageKindV1::CommandResponse,
            Self::RuntimeNotification(_) => MessageKindV1::RuntimeNotification,
        }
    }

    fn validate(&self, max_json_bytes: usize) -> Result<(), WireError> {
        match self {
            Self::ClientHello(value) => value.validate(max_json_bytes),
            Self::ServerHello(value) => value.validate(max_json_bytes),
            Self::MetadataUpdate(value) => value.validate(max_json_bytes),
            Self::DiscoveryRequest(value) => value.validate(max_json_bytes),
            Self::DiscoveryResponse(value) => value.validate(max_json_bytes),
            Self::DiscoveryChanged(value) => value.validate(max_json_bytes),
            Self::Ping(value) => value.validate(max_json_bytes),
            Self::Pong(value) => value.validate(max_json_bytes),
            Self::SessionError(value) => value.validate(max_json_bytes),
            Self::ServerDraining(value) => value.validate(max_json_bytes),
            Self::CommandRequest(value) => value.validate(max_json_bytes),
            Self::CommandResponse(value) => value.validate(max_json_bytes),
            Self::RuntimeNotification(value) => value.validate(max_json_bytes),
        }
    }
}

pub fn encode_rkyv_frame_v1(
    message: &DecodedMessageV1,
    max_payload_bytes: usize,
) -> Result<Vec<u8>, WireError> {
    message.validate(max_payload_bytes)?;
    let payload = match message {
        DecodedMessageV1::ClientHello(value) => encode(value)?,
        DecodedMessageV1::ServerHello(value) => encode(value)?,
        DecodedMessageV1::MetadataUpdate(value) => encode(value)?,
        DecodedMessageV1::DiscoveryRequest(value) => encode(value)?,
        DecodedMessageV1::DiscoveryResponse(value) => encode(value)?,
        DecodedMessageV1::DiscoveryChanged(value) => encode(value)?,
        DecodedMessageV1::Ping(value) => encode(value)?,
        DecodedMessageV1::Pong(value) => encode(value)?,
        DecodedMessageV1::SessionError(value) => encode(value)?,
        DecodedMessageV1::ServerDraining(value) => encode(value)?,
        DecodedMessageV1::CommandRequest(value) => encode(value)?,
        DecodedMessageV1::CommandResponse(value) => encode(value)?,
        DecodedMessageV1::RuntimeNotification(value) => encode(value)?,
    };
    if payload.len() > max_payload_bytes {
        return Err(WireError::PayloadTooLarge {
            declared: payload.len(),
            limit: max_payload_bytes,
        });
    }
    let header = FrameHeaderV1::new(message.kind(), payload.len())?.encode();
    let mut frame = Vec::with_capacity(FRAME_HEADER_BYTES + payload.len());
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_rkyv_frame_v1(
    frame: &[u8],
    max_payload_bytes: usize,
) -> Result<DecodedMessageV1, WireError> {
    decode_rkyv_frame_v1_inner(frame, max_payload_bytes, None)
}

pub fn decode_rkyv_frame_v1_on_channel(
    frame: &[u8],
    max_payload_bytes: usize,
    channel: LogicalChannel,
) -> Result<DecodedMessageV1, WireError> {
    decode_rkyv_frame_v1_inner(frame, max_payload_bytes, Some(channel))
}

fn decode_rkyv_frame_v1_inner(
    frame: &[u8],
    max_payload_bytes: usize,
    channel: Option<LogicalChannel>,
) -> Result<DecodedMessageV1, WireError> {
    let header = FrameHeaderV1::decode(frame, max_payload_bytes)?;
    if let Some(channel) = channel {
        header.kind.require_channel(channel)?;
    }
    let actual = frame.len().saturating_sub(FRAME_HEADER_BYTES);
    if actual != header.payload_len as usize {
        return Err(WireError::LengthMismatch {
            declared: header.payload_len as usize,
            actual,
        });
    }
    let mut aligned = AlignedVec::<16>::with_capacity(actual);
    aligned.extend_from_slice(&frame[FRAME_HEADER_BYTES..]);
    let decoded = match header.kind {
        MessageKindV1::ClientHello => DecodedMessageV1::ClientHello(decode(&aligned, header.kind)?),
        MessageKindV1::ServerHello => DecodedMessageV1::ServerHello(decode(&aligned, header.kind)?),
        MessageKindV1::MetadataUpdate => {
            DecodedMessageV1::MetadataUpdate(decode(&aligned, header.kind)?)
        }
        MessageKindV1::DiscoveryRequest => {
            DecodedMessageV1::DiscoveryRequest(decode(&aligned, header.kind)?)
        }
        MessageKindV1::DiscoveryResponse => {
            DecodedMessageV1::DiscoveryResponse(decode(&aligned, header.kind)?)
        }
        MessageKindV1::DiscoveryChanged => {
            DecodedMessageV1::DiscoveryChanged(decode(&aligned, header.kind)?)
        }
        MessageKindV1::Ping => DecodedMessageV1::Ping(decode(&aligned, header.kind)?),
        MessageKindV1::Pong => DecodedMessageV1::Pong(decode(&aligned, header.kind)?),
        MessageKindV1::SessionError => {
            DecodedMessageV1::SessionError(decode(&aligned, header.kind)?)
        }
        MessageKindV1::ServerDraining => {
            DecodedMessageV1::ServerDraining(decode(&aligned, header.kind)?)
        }
        MessageKindV1::CommandRequest => {
            DecodedMessageV1::CommandRequest(decode(&aligned, header.kind)?)
        }
        MessageKindV1::CommandResponse => {
            DecodedMessageV1::CommandResponse(decode(&aligned, header.kind)?)
        }
        MessageKindV1::RuntimeNotification => {
            DecodedMessageV1::RuntimeNotification(decode(&aligned, header.kind)?)
        }
    };
    decoded.validate(max_payload_bytes)?;
    Ok(decoded)
}

fn encode<T>(value: &T) -> Result<AlignedVec, WireError>
where
    T: for<'a> rkyv::Serialize<
            rkyv::api::high::HighSerializer<
                AlignedVec,
                rkyv::ser::allocator::ArenaHandle<'a>,
                RkyvError,
            >,
        >,
{
    rkyv::to_bytes::<RkyvError>(value).map_err(|error| WireError::InvalidArchive {
        kind: 0,
        reason: error.to_string(),
    })
}

fn decode<T>(bytes: &[u8], kind: MessageKindV1) -> Result<T, WireError>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: for<'a> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'a, RkyvError>>
        + rkyv::Deserialize<T, rkyv::api::high::HighDeserializer<RkyvError>>,
{
    rkyv::from_bytes::<T, RkyvError>(bytes).map_err(|error| WireError::InvalidArchive {
        kind: kind as u16,
        reason: error.to_string(),
    })
}
