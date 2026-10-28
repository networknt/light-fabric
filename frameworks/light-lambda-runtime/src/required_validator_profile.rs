//! Proposed trusted required-profile admission; never selected by layer values.
//! Check the same captured handler Value and enabled registrations used by Init.
use crate::{init::HandlerRegistration, init_error::InitError};
use serde_json::Value;
use serverless_core::HandlerId;
use sha2::{Digest, Sha256};
use std::io::Write;

pub const PROFILE_ERROR: &str = "validator: required execution profile mismatch";

/// Startup code supplies an independently reviewed identity and complete active
/// registration set. Never derive the expected pin from mutable runtime input.
pub struct RequiredValidatorProfile {
    pub handler_sha256: [u8; 32],
    pub active_handlers: &'static [HandlerId],
}
struct HashWriter(Sha256);
impl Write for HashWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// Offline review utility, not an authorization constructor. JSON serialization
/// is structural/configuration identity, never response/observation rewriting.
pub fn handler_identity_for_review(handler: &Value) -> Result<[u8; 32], InitError> {
    let mut writer = HashWriter(Sha256::new());
    writer
        .0
        .update(b"light-lambda/required-validator-profile/v1\0");
    serde_json::to_writer(&mut writer, handler)
        .map_err(|_| InitError::InvalidConfiguration(PROFILE_ERROR))?;
    Ok(writer.0.finalize().into())
}
impl RequiredValidatorProfile {
    pub(crate) fn admit(
        &self,
        handler: &Value,
        proxy_enabled: bool,
        registrations: &[(&HandlerRegistration, bool)],
    ) -> Result<(), InitError> {
        let refuse = || InitError::InvalidConfiguration(PROFILE_ERROR);
        if !proxy_enabled
            || !self.active_handlers.contains(&HandlerId::Specification)
            || !self.active_handlers.contains(&HandlerId::Validator)
            || self.active_handlers.contains(&HandlerId::Proxy)
            || self
                .active_handlers
                .iter()
                .enumerate()
                .any(|(index, id)| self.active_handlers[..index].contains(id))
            || registrations.iter().filter(|(_, enabled)| *enabled).count()
                != self.active_handlers.len()
        {
            return Err(refuse());
        }
        for required in self.active_handlers {
            let mut matches = registrations.iter().filter(|(registration, enabled)| {
                registration.id == *required && *enabled && registration.factory.is_some()
            });
            if matches.next().is_none() || matches.next().is_some() {
                return Err(refuse());
            }
        }
        if handler_identity_for_review(handler)? != self.handler_sha256 {
            return Err(refuse());
        }
        Ok(())
    }
}
