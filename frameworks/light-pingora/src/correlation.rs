use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::http::ResponseHeader;
use pingora::prelude::Session;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const CORRELATION_FILE: &str = "correlation.yml";
pub const CORRELATION_MODULE_ID: &str = "light-pingora/correlation";
pub const CORRELATION_CONFIG_NAME: &str = "correlation";
pub const CORRELATION_ID_HEADER: &str = "X-Correlation-Id";
pub const TRACEABILITY_ID_HEADER: &str = "X-Traceability-Id";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorrelationConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_autogen_correlation_id")]
    pub autogen_correlation_id: bool,
    #[serde(default = "default_correlation_mdc_field")]
    pub correlation_mdc_field: String,
    #[serde(default = "default_traceability_mdc_field")]
    pub traceability_mdc_field: String,
}

impl Default for CorrelationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            autogen_correlation_id: true,
            correlation_mdc_field: default_correlation_mdc_field(),
            traceability_mdc_field: default_traceability_mdc_field(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorrelationState {
    pub correlation_id: Option<String>,
    pub traceability_id: Option<String>,
}

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
    if !config.enabled {
        return Ok(CorrelationState::default());
    }

    let traceability_id = first_request_header(session, TRACEABILITY_ID_HEADER);
    let mut correlation_id = first_request_header(session, CORRELATION_ID_HEADER);

    if correlation_id.is_none() && config.autogen_correlation_id {
        let generated = java_compatible_uuid();
        session
            .req_header_mut()
            .insert_header(CORRELATION_ID_HEADER, generated.as_str())?;
        correlation_id = Some(generated);
    }

    Ok(CorrelationState {
        correlation_id,
        traceability_id,
    })
}

pub fn apply_correlation_response(
    response: &mut ResponseHeader,
    state: &CorrelationState,
) -> pingora::Result<()> {
    if let Some(traceability_id) = state.traceability_id.as_deref() {
        response.insert_header(TRACEABILITY_ID_HEADER, traceability_id)?;
    }
    Ok(())
}

pub fn correlation_id_for_upstream(state: &CorrelationState) -> Option<&str> {
    state.correlation_id.as_deref()
}

fn first_request_header(session: &Session, name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn java_compatible_uuid() -> String {
    URL_SAFE_NO_PAD.encode(Uuid::now_v7().as_bytes())
}

fn default_enabled() -> bool {
    true
}

fn default_autogen_correlation_id() -> bool {
    true
}

fn default_correlation_mdc_field() -> String {
    "cId".to_string()
}

fn default_traceability_mdc_field() -> String {
    "tId".to_string()
}

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
