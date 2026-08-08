use crate::inference::{
    EmbeddingRequest, EmbeddingResponse, EmbeddingVector, InferenceError, NormalizedUsage,
    ProviderEvidence,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Default)]
pub struct OpenAiEmbeddingsCodec;

impl OpenAiEmbeddingsCodec {
    pub fn encode_request(&self, request: &EmbeddingRequest) -> Value {
        let mut body = Map::new();
        body.insert("model".to_string(), Value::String(request.model.clone()));
        body.insert(
            "input".to_string(),
            Value::Array(request.inputs.iter().cloned().map(Value::String).collect()),
        );
        body.insert(
            "encoding_format".to_string(),
            Value::String("float".to_string()),
        );
        if let Some(dimensions) = request.dimensions {
            body.insert("dimensions".to_string(), Value::from(dimensions));
        }
        Value::Object(body)
    }

    pub fn decode_response(
        &self,
        value: &Value,
        expected_count: usize,
        expected_dimensions: Option<u32>,
    ) -> Result<EmbeddingResponse, InferenceError> {
        let data = value.get("data").and_then(Value::as_array).ok_or_else(|| {
            InferenceError::provider_protocol(Some(502), "OpenAI embeddings response has no data")
        })?;
        if data.len() != expected_count {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "OpenAI embeddings response count mismatch",
            ));
        }
        let mut vectors = Vec::with_capacity(data.len());
        let mut observed_dimensions = None;
        for (position, item) in data.iter().enumerate() {
            let index = item
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| {
                    InferenceError::provider_protocol(
                        Some(502),
                        "OpenAI embedding has an invalid index",
                    )
                })?;
            if usize::try_from(index).ok() != Some(position) {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embedding indices are not contiguous and ordered",
                ));
            }
            let encoded = item.get("embedding").ok_or_else(|| {
                InferenceError::provider_protocol(Some(502), "OpenAI embedding has no vector")
            })?;
            let values = decode_vector(encoded)?;
            let dimensions = u32::try_from(values.len()).map_err(|_| {
                InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embedding dimension exceeds the supported range",
                )
            })?;
            if dimensions == 0
                || expected_dimensions.is_some_and(|expected| expected != dimensions)
                || observed_dimensions.is_some_and(|observed| observed != dimensions)
            {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embedding dimension mismatch",
                ));
            }
            observed_dimensions = Some(dimensions);
            vectors.push(EmbeddingVector { index, values });
        }
        let usage = value.get("usage");
        let input_tokens = usage
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embeddings response has no prompt token usage",
                )
            })?;
        let output_tokens = usage
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64);
        let total_tokens = usage
            .and_then(|usage| usage.get("total_tokens"))
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embeddings response has no total token usage",
                )
            })?;
        if input_tokens.saturating_add(output_tokens.unwrap_or_default()) != total_tokens {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "OpenAI embeddings response usage is inconsistent",
            ));
        }
        Ok(EmbeddingResponse {
            vectors,
            usage: NormalizedUsage {
                input_tokens: Some(input_tokens),
                output_tokens,
                cached_input_tokens: None,
                reasoning_tokens: None,
            },
            evidence: ProviderEvidence {
                request_id: value.get("id").and_then(Value::as_str).map(str::to_string),
                physical_model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                ..ProviderEvidence::default()
            },
        })
    }
}

fn decode_vector(value: &Value) -> Result<Vec<f32>, InferenceError> {
    match value {
        Value::Array(values) => values
            .iter()
            .map(|value| {
                let value = value.as_f64().ok_or_else(|| {
                    InferenceError::provider_protocol(
                        Some(502),
                        "OpenAI embedding vector contains a non-number",
                    )
                })?;
                let narrowed = value as f32;
                if !value.is_finite() || !narrowed.is_finite() {
                    return Err(InferenceError::provider_protocol(
                        Some(502),
                        "OpenAI embedding vector contains a non-finite value",
                    ));
                }
                Ok(narrowed)
            })
            .collect(),
        Value::String(encoded) => {
            let bytes = STANDARD.decode(encoded).map_err(|_| {
                InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embedding vector has malformed base64",
                )
            })?;
            if bytes.is_empty() || bytes.len() % std::mem::size_of::<f32>() != 0 {
                return Err(InferenceError::provider_protocol(
                    Some(502),
                    "OpenAI embedding base64 byte length is invalid",
                ));
            }
            bytes
                .chunks_exact(4)
                .map(|chunk| {
                    let value = f32::from_le_bytes(chunk.try_into().expect("four-byte chunk"));
                    if !value.is_finite() {
                        return Err(InferenceError::provider_protocol(
                            Some(502),
                            "OpenAI embedding vector contains a non-finite value",
                        ));
                    }
                    Ok(value)
                })
                .collect()
        }
        _ => Err(InferenceError::provider_protocol(
            Some(502),
            "OpenAI embedding vector has an unsupported encoding",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn decodes_float_and_little_endian_base64_vectors() {
        let encoded = STANDARD.encode(
            [1.0_f32, -2.5_f32]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        );
        for embedding in [json!([1.0, -2.5]), Value::String(encoded)] {
            let response = OpenAiEmbeddingsCodec
                .decode_response(
                    &json!({
                        "data":[{"index":0,"embedding":embedding}],
                        "usage":{"prompt_tokens":3,"total_tokens":3}
                    }),
                    1,
                    Some(2),
                )
                .unwrap();
            assert_eq!(response.vectors[0].values, vec![1.0, -2.5]);
            assert_eq!(response.usage.input_tokens, Some(3));
        }
    }

    #[test]
    fn rejects_count_index_dimension_and_non_finite_mismatches() {
        assert!(
            OpenAiEmbeddingsCodec
                .decode_response(&serde_json::json!({"data":[]}), 1, None)
                .is_err()
        );
        assert!(
            OpenAiEmbeddingsCodec
                .decode_response(
                    &serde_json::json!({"data":[{"index":1,"embedding":[1.0]}]}),
                    1,
                    None,
                )
                .is_err()
        );
        assert!(
            OpenAiEmbeddingsCodec
                .decode_response(
                    &serde_json::json!({"data":[{"index":0,"embedding":[1.0]}]}),
                    1,
                    Some(2),
                )
                .is_err()
        );
        let nan = STANDARD.encode(f32::NAN.to_le_bytes());
        assert!(
            OpenAiEmbeddingsCodec
                .decode_response(
                    &serde_json::json!({"data":[{"index":0,"embedding":nan}]}),
                    1,
                    Some(1),
                )
                .is_err()
        );
    }
}
