use crate::policy_adapter::{PolicyRequest, PolicyResponse};
pub use gateway_policy::correlation::{
    CORRELATION_CONFIG_NAME, CORRELATION_FILE, CORRELATION_ID_HEADER, CORRELATION_MODULE_ID,
    CorrelationConfig, CorrelationState, TRACEABILITY_ID_HEADER, correlation_id_for_upstream,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::{http::ResponseHeader, prelude::Session};
pub fn load_correlation_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<CorrelationConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<CorrelationConfig>(runtime_config, CORRELATION_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == CORRELATION_FILE => {
            CorrelationConfig::default()
        }
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        CORRELATION_MODULE_ID,
        CORRELATION_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn apply_correlation_request(
    session: &mut Session,
    config: &CorrelationConfig,
) -> pingora::Result<CorrelationState> {
    gateway_policy::correlation::apply_correlation_request(&mut PolicyRequest(session), config)
}
pub fn apply_correlation_response(
    response: &mut ResponseHeader,
    state: &CorrelationState,
) -> pingora::Result<()> {
    gateway_policy::correlation::apply_correlation_response(&mut PolicyResponse(response), state)
}
#[cfg(test)]
use gateway_policy::correlation::java_compatible_uuid;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_correlation_id_matches_java_url_safe_uuid_shape() {
        let id = java_compatible_uuid();

        assert_eq!(id.len(), 22);
        assert!(
            id.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }
}
