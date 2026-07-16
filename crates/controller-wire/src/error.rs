use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum WireError {
    #[error("truncated {component}: expected at least {expected} bytes, received {actual}")]
    Truncated {
        component: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("invalid frame magic")]
    InvalidMagic,
    #[error("unsupported wire profile major version {0}")]
    UnsupportedVersion(u8),
    #[error("unsupported frame flags 0x{0:02x}")]
    UnsupportedFlags(u8),
    #[error("unknown message kind {0}")]
    UnknownMessageKind(u16),
    #[error("message kind {kind} is not allowed on logical channel {channel}")]
    InvalidLogicalChannel { kind: u16, channel: &'static str },
    #[error("reserved frame bytes must be zero")]
    NonZeroReserved,
    #[error("declared payload length {declared} exceeds limit {limit}")]
    PayloadTooLarge { declared: usize, limit: usize },
    #[error("frame length mismatch: declared {declared} bytes, received {actual}")]
    LengthMismatch { declared: usize, actual: usize },
    #[error("zero-length JSON payload is invalid")]
    EmptyJson,
    #[error("JSON payload is not valid UTF-8")]
    InvalidUtf8,
    #[error("JSON payload must contain exactly one valid value: {0}")]
    InvalidJson(String),
    #[error("JSON nesting exceeds maximum depth {0}")]
    JsonDepthExceeded(usize),
    #[error("archive validation failed for message kind {kind}: {reason}")]
    InvalidArchive { kind: u16, reason: String },
    #[error("semantic validation failed for {field}: {reason}")]
    Semantic { field: &'static str, reason: String },
    #[error("encoded payload length cannot be represented by the version 1 header")]
    PayloadLengthOverflow,
}

impl WireError {
    pub(crate) fn semantic(field: &'static str, reason: impl Into<String>) -> Self {
        Self::Semantic {
            field,
            reason: reason.into(),
        }
    }
}
