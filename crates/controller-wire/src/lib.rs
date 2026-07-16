#![forbid(unsafe_code)]

//! Shared, versioned controller wire profiles.
//!
//! This crate deliberately contains no transport, async-runtime, application,
//! database, or gateway dependencies. Network adapters must bound an incoming
//! frame before calling these parsers.

mod codec;
mod error;
mod frame;
mod json;
mod profile;
pub mod v1;

pub use codec::{
    DecodedMessageV1, decode_rkyv_frame_v1, decode_rkyv_frame_v1_on_channel, encode_rkyv_frame_v1,
};
pub use error::WireError;
pub use frame::{FRAME_HEADER_BYTES, FrameHeaderV1, RKYV_MAGIC};
pub use json::{JSON_LENGTH_PREFIX_BYTES, decode_json_frame, encode_json_frame, fuzz_json_frame};
pub use profile::{
    LogicalChannel, MCP_JSON_V1, MessageKindV1, PROFILE_MAJOR_V1, RUNTIME_JSON_V1, RUNTIME_RKYV_V1,
};

/// Fuzzable production parser entry point. It never performs unchecked archive
/// access and never panics for arbitrary input.
pub fn fuzz_rkyv_frame(data: &[u8], max_payload_bytes: usize) {
    let _ = decode_rkyv_frame_v1(data, max_payload_bytes);
}
