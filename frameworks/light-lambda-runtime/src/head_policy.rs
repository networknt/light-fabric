//! Immutable Lambda factories and per-key warning collection. No network work.
use crate::{init::HandlerFactoryProvider, init_error::InitError};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{
    handlers::{
        correlation::CorrelationHandler, header::HeaderHandler, traceability::TraceabilityHandler,
    },
    plan::HandlerFactory,
};
use std::sync::Arc;

pub enum HeadPolicyProvider {
    Correlation,
    Traceability,
    RequestHeader,
    ResponseHeader,
}
fn invalid() -> InitError {
    InitError::InvalidConfiguration(
        "head policy configuration: unsupported, unknown or invalid setting",
    )
}
impl HandlerFactoryProvider for HeadPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        match self {
            Self::Correlation => Ok(Arc::new(
                CorrelationHandler::from_value(
                    &config
                        .module_registry
                        .load_config(config, "correlation.yml")
                        .map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?,
            )),
            Self::Traceability => Ok(Arc::new(
                TraceabilityHandler::from_value(
                    &config
                        .module_registry
                        .load_config(config, "traceability.yml")
                        .map_err(|_| invalid())?,
                )
                .map_err(|_| invalid())?,
            )),
            Self::RequestHeader | Self::ResponseHeader => Ok(Arc::new(
                HeaderHandler::from_yaml(
                    config
                        .module_registry
                        .load_config(config, "header.yml")
                        .map_err(|_| invalid())?,
                    matches!(self, Self::ResponseHeader),
                )
                .map_err(|_| invalid())?,
            )),
        }
    }
}

pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    crate::cors_policy::validate(config)?;
    let correlation = match config
        .module_registry
        .load_config(config, "correlation.yml")
    {
        Ok(value) => value,
        Err(light_runtime::RuntimeError::MissingConfig(_)) => serde_json::json!({"enabled":false}),
        Err(_) => return Err(invalid()),
    };
    let mut warnings = CorrelationHandler::warnings(&correlation).map_err(|_| invalid())?;
    warnings.extend(crate::limit_policy::warnings(config)?);
    // Non-default or malformed settings do not become acceptable by disabling
    // the policy. Validate through the same production parsers in both modes.
    match config
        .module_registry
        .load_config(config, "traceability.yml")
    {
        Ok(value) => {
            TraceabilityHandler::from_value(&value).map_err(|_| invalid())?;
        }
        Err(light_runtime::RuntimeError::MissingConfig(_)) => {}
        Err(_) => return Err(invalid()),
    }
    match config.module_registry.load_config(config, "header.yml") {
        Ok(value) => {
            HeaderHandler::from_yaml(value, false).map_err(|_| invalid())?;
        }
        Err(light_runtime::RuntimeError::MissingConfig(_)) => {}
        Err(_) => return Err(invalid()),
    }
    Ok(warnings)
}
