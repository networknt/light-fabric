use serde_json::Value;

use crate::WireError;

pub const JSON_LENGTH_PREFIX_BYTES: usize = 4;
pub const MAX_JSON_DEPTH: usize = 64;

pub fn encode_json_frame(json: &[u8], max_payload_bytes: usize) -> Result<Vec<u8>, WireError> {
    validate_json(json, max_payload_bytes)?;
    let length = u32::try_from(json.len()).map_err(|_| WireError::PayloadLengthOverflow)?;
    let mut frame = Vec::with_capacity(JSON_LENGTH_PREFIX_BYTES + json.len());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(json);
    Ok(frame)
}

pub fn decode_json_frame(frame: &[u8], max_payload_bytes: usize) -> Result<&str, WireError> {
    if frame.len() < JSON_LENGTH_PREFIX_BYTES {
        return Err(WireError::Truncated {
            component: "JSON length prefix",
            expected: JSON_LENGTH_PREFIX_BYTES,
            actual: frame.len(),
        });
    }
    let declared = u32::from_be_bytes(frame[..4].try_into().expect("fixed slice")) as usize;
    if declared > max_payload_bytes {
        return Err(WireError::PayloadTooLarge {
            declared,
            limit: max_payload_bytes,
        });
    }
    let actual = frame.len() - JSON_LENGTH_PREFIX_BYTES;
    if actual != declared {
        return Err(WireError::LengthMismatch { declared, actual });
    }
    validate_json(&frame[4..], max_payload_bytes)
}

pub fn fuzz_json_frame(data: &[u8], max_payload_bytes: usize) {
    let _ = decode_json_frame(data, max_payload_bytes);
}

pub(crate) fn validate_json(json: &[u8], max_payload_bytes: usize) -> Result<&str, WireError> {
    if json.is_empty() {
        return Err(WireError::EmptyJson);
    }
    if json.len() > max_payload_bytes {
        return Err(WireError::PayloadTooLarge {
            declared: json.len(),
            limit: max_payload_bytes,
        });
    }
    let text = std::str::from_utf8(json).map_err(|_| WireError::InvalidUtf8)?;
    let value: Value =
        serde_json::from_str(text).map_err(|error| WireError::InvalidJson(error.to_string()))?;
    ensure_depth(&value, 0)?;
    Ok(text)
}

fn ensure_depth(value: &Value, depth: usize) -> Result<(), WireError> {
    if depth > MAX_JSON_DEPTH {
        return Err(WireError::JsonDepthExceeded(MAX_JSON_DEPTH));
    }
    match value {
        Value::Array(values) => {
            for value in values {
                ensure_depth(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                ensure_depth(value, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
