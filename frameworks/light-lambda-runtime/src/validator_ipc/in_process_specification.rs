//! Select precision preparation only for enabled Validator configurations in the
//! explicit in-process application. Existing worker providers remain unchanged.
use crate::{
    init::{
        HandlerFactoryProvider, PreparationContext, PreparationProfile,
        PreparedSpecificationResources, PreparedSpecificationRuntime,
    },
    init_error::InitError,
    specification_policy::SpecificationPolicyProvider,
};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{plan::HandlerFactory, unsupported::remaining::ValidatorSettings};
use std::{future::Future, pin::Pin, sync::Arc};

pub struct SpecificationProvider {
    precision: Arc<dyn HandlerFactoryProvider>,
}
impl SpecificationProvider {
    pub fn new(precision: Arc<dyn HandlerFactoryProvider>) -> Self {
        Self { precision }
    }
    fn precision_enabled(config: &RuntimeConfig) -> Result<bool, InitError> {
        // Use the existing typed/defaulted settings contract. Neither a malformed
        // flag nor malformed enabled settings can select the disabled branch.
        let value: serde_json::Value = config
            .module_registry
            .load_config(config, "openapi-validator.yml")
            .map_err(|_| invalid())?;
        ValidatorSettings::from_value(&value)
            .map(|(settings, _)| settings.enabled)
            .map_err(|_| invalid())
    }
}
fn invalid() -> InitError {
    InitError::InvalidConfiguration(
        "validator: captured resources or supported engine settings required",
    )
}
impl HandlerFactoryProvider for SpecificationProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        if Self::precision_enabled(config)? {
            self.precision.create(config, security)
        } else {
            SpecificationPolicyProvider.create(config, security)
        }
    }
    fn runtime_preparation_profile(&self) -> Option<PreparationProfile> {
        self.precision.runtime_preparation_profile()
    }
    fn prepare_specification_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedSpecificationResources, InitError> {
        if Self::precision_enabled(config)? {
            // Preserve the enabled profile's offline fail-closed behavior.
            self.precision
                .prepare_specification_resources(config, security)
        } else {
            SpecificationPolicyProvider.prepare_specification_resources(config, security)
        }
    }
    fn prepare_specification_runtime<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedSpecificationRuntime, InitError>> + Send + 'a>>
    {
        self.prepare_specification_runtime_owned(config, security, None)
    }
    fn prepare_specification_runtime_owned<'a>(
        &'a self,
        config: &'a RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
        context: Option<Arc<PreparationContext>>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedSpecificationRuntime, InitError>> + Send + 'a>>
    {
        Box::pin(async move {
            if Self::precision_enabled(config)? {
                self.precision
                    .prepare_specification_runtime_owned(config, security, context)
                    .await
            } else {
                SpecificationPolicyProvider
                    .prepare_specification_runtime_owned(config, security, context)
                    .await
            }
        })
    }
}
