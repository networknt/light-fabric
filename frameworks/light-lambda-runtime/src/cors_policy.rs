//! Offline/startup CORS parsing uses the same immutable Lambda factory.
use crate::{init::HandlerFactoryProvider, init_error::InitError};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{handlers::cors::CorsHandler, plan::HandlerFactory};
use std::sync::Arc;
pub struct CorsPolicyProvider(pub bool);
fn invalid() -> InitError {
    InitError::InvalidConfiguration("cors.yml: unknown or malformed setting")
}
pub(crate) fn validate(config: &RuntimeConfig) -> Result<(), InitError> {
    match config.module_registry.load_config(config, "cors.yml") {
        Ok(value) => {
            CorsHandler::from_yaml(value, false).map_err(|_| invalid())?;
        }
        Err(light_runtime::RuntimeError::MissingConfig(_)) => {}
        Err(_) => return Err(invalid()),
    }
    Ok(())
}
impl HandlerFactoryProvider for CorsPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let value = config
            .module_registry
            .load_config(config, "cors.yml")
            .map_err(|_| invalid())?;
        Ok(Arc::new(
            CorsHandler::from_yaml(value, self.0).map_err(|_| invalid())?,
        ))
    }
}
