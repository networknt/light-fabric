use crate::{MessageKindV1, PROFILE_MAJOR_V1, WireError};

pub const FRAME_HEADER_BYTES: usize = 16;
pub const RKYV_MAGIC: [u8; 4] = *b"LCRK";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeaderV1 {
    pub kind: MessageKindV1,
    pub payload_len: u32,
}

impl FrameHeaderV1 {
    pub fn new(kind: MessageKindV1, payload_len: usize) -> Result<Self, WireError> {
        let payload_len =
            u32::try_from(payload_len).map_err(|_| WireError::PayloadLengthOverflow)?;
        Ok(Self { kind, payload_len })
    }

    pub fn encode(self) -> [u8; FRAME_HEADER_BYTES] {
        let mut header = [0_u8; FRAME_HEADER_BYTES];
        header[0..4].copy_from_slice(&RKYV_MAGIC);
        header[4] = PROFILE_MAJOR_V1;
        header[5] = 0;
        header[6..8].copy_from_slice(&(self.kind as u16).to_le_bytes());
        header[8..12].copy_from_slice(&self.payload_len.to_le_bytes());
        header
    }

    pub fn decode(bytes: &[u8], max_payload_bytes: usize) -> Result<Self, WireError> {
        if bytes.len() < FRAME_HEADER_BYTES {
            return Err(WireError::Truncated {
                component: "rkyv frame header",
                expected: FRAME_HEADER_BYTES,
                actual: bytes.len(),
            });
        }
        if bytes[0..4] != RKYV_MAGIC {
            return Err(WireError::InvalidMagic);
        }
        if bytes[4] != PROFILE_MAJOR_V1 {
            return Err(WireError::UnsupportedVersion(bytes[4]));
        }
        if bytes[5] != 0 {
            return Err(WireError::UnsupportedFlags(bytes[5]));
        }
        let kind = MessageKindV1::try_from(u16::from_le_bytes([bytes[6], bytes[7]]))?;
        let payload_len = u32::from_le_bytes(bytes[8..12].try_into().expect("fixed slice"));
        if bytes[12..16] != [0_u8; 4] {
            return Err(WireError::NonZeroReserved);
        }
        if payload_len as usize > max_payload_bytes {
            return Err(WireError::PayloadTooLarge {
                declared: payload_len as usize,
                limit: max_payload_bytes,
            });
        }
        Ok(Self { kind, payload_len })
    }
}
