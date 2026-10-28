use crate::{init::HandlerFactoryProvider, init_error::InitError};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{handlers::sanitizer::SanitizerHandler, plan::HandlerFactory};
use std::sync::Arc;
pub struct SanitizerPolicyProvider;
impl HandlerFactoryProvider for SanitizerPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let invalid = || InitError::InvalidConfiguration("sanitizer.yml: invalid configuration");
        let value = config
            .module_registry
            .load_config(config, "sanitizer.yml")
            .map_err(|_| invalid())?;
        Ok(Arc::new(
            SanitizerHandler::from_value(&value).map_err(|_| invalid())?,
        ))
    }
}
