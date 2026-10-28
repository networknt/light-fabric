use crate::{init::HandlerFactoryProvider, init_error::InitError};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{handlers::limit::LimitHandler, plan::HandlerFactory};
use std::sync::Arc;
pub struct LimitPolicyProvider;
fn invalid() -> InitError {
    InitError::InvalidConfiguration("limit.yml: unsupported, unknown or malformed setting")
}
pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    let value: serde_yaml::Value = match config.module_registry.load_config(config, "limit.yml") {
        Ok(v) => v,
        Err(light_runtime::RuntimeError::MissingConfig(_)) => {
            serde_yaml::from_str("enabled: false").map_err(|_| invalid())?
        }
        Err(_) => return Err(invalid()),
    };
    LimitHandler::from_yaml(value.clone()).map_err(|_| invalid())?;
    LimitHandler::warnings(&value).map_err(|_| invalid())
}
impl HandlerFactoryProvider for LimitPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let value = config
            .module_registry
            .load_config(config, "limit.yml")
            .map_err(|_| invalid())?;
        Ok(Arc::new(
            LimitHandler::from_yaml(value).map_err(|_| invalid())?,
        ))
    }
}
