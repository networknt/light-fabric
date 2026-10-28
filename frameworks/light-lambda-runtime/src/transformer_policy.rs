//! Per-Init fixed-program factory. App activation remains qualification-gated.
use crate::{
    init::HandlerFactoryProvider, init_error::InitError, transformer_adapter::TransformerAdapter,
    transformer_program::Candidate,
};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
use serverless_core::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
};
use std::sync::Arc;
pub struct RequestTransformerPolicyProvider;
pub struct ResponseTransformerPolicyProvider;
fn invalid() -> InitError {
    InitError::InvalidConfiguration("transformer: missing prepared immutable program")
}
struct Factory(Arc<TransformerAdapter>);
struct ResponseFactory(Arc<TransformerAdapter>);
impl HandlerFactory for ResponseFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Response(self.0.clone()))
    }
}
impl HandlerFactory for Factory {
    fn validator_media_role(&self) -> serverless_core::plan::ValidatorMediaRole {
        serverless_core::plan::ValidatorMediaRole::Maintains
    }
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Request(
            self.0.clone(),
            RequestCapabilities::default(),
        ))
    }
}
impl HandlerFactoryProvider for RequestTransformerPolicyProvider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Err(invalid())
    }
    fn prepare_transformer(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        program: Option<Arc<Candidate>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let value: serde_json::Value = config
            .module_registry
            .load_config(config, "request-transformer.yml")
            .map_err(|_| invalid())?;
        let prefixes = serverless_core::unsupported::strings(value.get("appliedPathPrefixes"))
            .map_err(|_| invalid())?;
        Ok(Arc::new(Factory(Arc::new(TransformerAdapter::new(
            program.ok_or_else(invalid)?,
            prefixes,
        )))))
    }
}
impl HandlerFactoryProvider for ResponseTransformerPolicyProvider {
    fn create(
        &self,
        _: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Err(invalid())
    }
    fn prepare_transformer(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
        program: Option<Arc<Candidate>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        let value: serde_json::Value = config
            .module_registry
            .load_config(config, "response-transformer.yml")
            .map_err(|_| invalid())?;
        let prefixes = serverless_core::unsupported::strings(value.get("appliedPathPrefixes"))
            .map_err(|_| invalid())?;
        Ok(Arc::new(ResponseFactory(Arc::new(
            TransformerAdapter::new(program.ok_or_else(invalid)?, prefixes),
        ))))
    }
}
