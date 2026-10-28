//! Decoded-body Java sanitizer. No source reload, global state or dispatch.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::{boolean, object, remaining, strings},
    *,
};
use async_trait::async_trait;
use gateway_policy::{sanitizer_body, sanitizer_encoding};
use serde_json::Value;
use std::sync::Arc;
#[derive(Clone)]
pub struct SanitizerHandler {
    body_enabled: bool,
    header_enabled: bool,
    body_encoder: String,
    header_encoder: String,
    body_ignore: Vec<String>,
    body_encode: Vec<String>,
    header_ignore: Vec<String>,
    header_encode: Option<Vec<String>>,
}
impl SanitizerHandler {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        remaining::sanitizer(value)?;
        let m = object(value, "sanitizer.yml: object required")?;
        let encoder = |key| {
            m.get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(InitError::Malformed("sanitizer.yml: encoder required"))
        };
        Ok(Self {
            body_enabled: boolean(m, "bodyEnabled", false)?,
            header_enabled: boolean(m, "headerEnabled", false)?,
            body_encoder: encoder("bodyEncoder")?,
            header_encoder: encoder("headerEncoder")?,
            body_ignore: strings(m.get("bodyAttributesToIgnore"))?,
            body_encode: strings(m.get("bodyAttributesToEncode"))?,
            header_ignore: strings(m.get("headerAttributesToIgnore"))?,
            header_encode: match m.get("headerAttributesToEncode") {
                None | Some(Value::Null) => None,
                Some(v) => Some(strings(Some(v))?),
            },
        })
    }
}
#[async_trait]
impl RequestHandler for SanitizerHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Sanitizer
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let mut media = state.validator_media_snapshot();
        if self.header_enabled {
            let ignore = self
                .header_ignore
                .iter()
                .any(|key| key.eq_ignore_ascii_case("content-type"));
            let select = self.header_encode.as_ref().is_none_or(|keys| {
                keys.iter()
                    .any(|key| key.eq_ignore_ascii_case("content-type"))
            });
            if !ignore && select {
                media.map_value(|value| {
                    let input =
                        std::str::from_utf8(value.as_bytes()).map_err(|_| HandlerFailure)?;
                    let output = sanitizer_encoding::encode(&self.header_encoder, input)
                        .ok_or(HandlerFailure)?;
                    http::HeaderValue::from_bytes(output.as_bytes()).map_err(|_| HandlerFailure)
                })?;
            }
            let mut next = http::HeaderMap::new();
            for (name, value) in &inv.headers {
                let ignore = self
                    .header_ignore
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(name.as_str()));
                let select = self
                    .header_encode
                    .as_ref()
                    .is_none_or(|keys| keys.iter().any(|s| s.eq_ignore_ascii_case(name.as_str())));
                let value = if !ignore && select {
                    let input =
                        std::str::from_utf8(value.as_bytes()).map_err(|_| HandlerFailure)?;
                    let output = sanitizer_encoding::encode(&self.header_encoder, input)
                        .ok_or(HandlerFailure)?;
                    http::HeaderValue::from_bytes(output.as_bytes()).map_err(|_| HandlerFailure)?
                } else {
                    value.clone()
                };
                next.append(name.clone(), value);
            }
            inv.headers = next;
        }
        if self.body_enabled
            && ["POST", "PUT", "PATCH"]
                .iter()
                .any(|m| inv.method.as_str().eq_ignore_ascii_case(m))
            && let Some(body) = &inv.body
        {
            // String.trim in Java removes only U+0000..U+0020. Plain/non-JSON
            // bodies retain their original bytes, including base64 provenance.
            let start = body.iter().position(|b| *b > 32).unwrap_or(body.len());
            let end = body.iter().rposition(|b| *b > 32).map_or(start, |i| i + 1);
            let trimmed = &body[start..end];
            if trimmed.first().is_some_and(|b| matches!(b, b'{' | b'[')) {
                match sanitizer_body::sanitize(
                    trimmed,
                    &self.body_encoder,
                    &self.body_ignore,
                    &self.body_encode,
                ) {
                    Ok(body) => inv.body = Some(bytes::Bytes::from(body)),
                    Err(_) => {
                        let content_type = inv
                            .headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("unknown");
                        // Preserve the accepted Java Status string-field bytes,
                        // including unescaped error parameters; do not introduce
                        // a Sanitizer-only serialization amendment.
                        return Ok(HandlerOutcome::Reject(StatusError::new(
                            400,
                            "ERR10015",
                            "CONTENT_TYPE_MISMATCH",
                            &format!(
                                "Either the Content-Type header {} does not match the body, or the body was serialized incorrectly.",
                                content_type
                            ),
                        )));
                    }
                }
            }
        }
        state.commit_validator_media(media);
        Ok(HandlerOutcome::Continue)
    }
}
impl HandlerFactory for SanitizerHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Maintains
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}
