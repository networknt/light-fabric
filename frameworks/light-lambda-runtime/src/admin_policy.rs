//! Local-only factory preparation. No authentication substitute or I/O.
use crate::{init::HandlerFactoryProvider, init_error::InitError};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{
    HandlerId, handlers::local_admin::LocalAdminHandler, plan::HandlerFactory,
    unsupported::remaining::HealthSettings,
};
use std::sync::Arc;
pub struct AdminPolicyProvider(pub HandlerId);
impl HandlerFactoryProvider for AdminPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let invalid =
            || InitError::InvalidConfiguration("local admin: invalid or unsupported configuration");
        let file = match self.0 {
            HandlerId::Health => "health.yml",
            HandlerId::Info => "info.yml",
            HandlerId::GetLogger => "logging.yml",
            _ => return Err(invalid()),
        };
        let value = config
            .module_registry
            .load_config(config, file)
            .map_err(|_| invalid())?;
        if self.0 == HandlerId::Health {
            HealthSettings::from_value(&value).map_err(|_| invalid())?;
        } else {
            crate::local_activation::activation(self.0, &value, false)?;
        }
        Ok(Arc::new(match self.0 {
            HandlerId::Health => LocalAdminHandler::health(),
            HandlerId::Info => LocalAdminHandler::info(),
            HandlerId::GetLogger => LocalAdminHandler::logger(crate::telemetry::installed_filter),
            _ => return Err(invalid()),
        }))
    }
}
