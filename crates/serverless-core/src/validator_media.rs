//! Trusted, invocation-local provenance for the HTTP case-insensitive Content-Type lookup.
//! No Deserialize or Debug: neither caller fields nor diagnostics carry proof.
use crate::HandlerFailure;
use http::HeaderValue;

#[derive(Clone, Default)]
pub struct ValidatorMediaProof(State);
#[derive(Clone, Default)]
enum State {
    #[default]
    Unknown,
    Known(Option<HeaderValue>),
}
impl ValidatorMediaProof {
    /// Trusted adapter projection of the current case-insensitive media header.
    /// The historical method name is retained for source compatibility.
    /// Preserve absence and its value; never reconstruct from a bool.
    pub fn exact_content_type(&self) -> Result<Option<&HeaderValue>, HandlerFailure> {
        match &self.0 {
            State::Unknown => Err(HandlerFailure),
            State::Known(value) => Ok(value.as_ref()),
        }
    }
    /// Trusted ingress only, after validating the actual header map. Like a
    /// verified principal, this is a trusted-code boundary, not caller data.
    pub fn from_validated_ingress(exact_content_type: Option<HeaderValue>) -> Self {
        Self(State::Known(exact_content_type))
    }
    /// Missing is native default-JSON behavior, not an invariant failure.
    pub fn validates_json(&self) -> Result<bool, HandlerFailure> {
        match &self.0 {
            State::Unknown => Err(HandlerFailure),
            State::Known(value) => Ok(value
                .as_ref()
                .is_none_or(|value| value.as_bytes().starts_with(b"application/json"))),
        }
    }
    /// Header names are case insensitive, including insertion after removal.
    pub fn record_insert(&mut self, name: &str, value: &HeaderValue, _existed: bool) {
        if name.eq_ignore_ascii_case("content-type")
            && let State::Known(current) = &mut self.0
        {
            *current = Some(value.clone());
        }
    }
    pub fn record_remove(&mut self, name: &str) {
        if name.eq_ignore_ascii_case("content-type")
            && let State::Known(current) = &mut self.0
        {
            *current = None;
        }
    }
    /// Sanitizer encodes a value without changing its current key spelling.
    pub fn map_value(
        &mut self,
        transform: impl FnOnce(&HeaderValue) -> Result<HeaderValue, HandlerFailure>,
    ) -> Result<(), HandlerFailure> {
        if let State::Known(Some(value)) = &mut self.0 {
            *value = transform(value)?;
        }
        Ok(())
    }
}
