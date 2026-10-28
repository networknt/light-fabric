//! Real Lambda Basic/API-key factories, sharing credential primitives with Gateway.
use crate::init::HandlerFactoryProvider;
use crate::init_error::InitError;
use light_runtime::{RuntimeConfig, RuntimeError};
use light_security::SecurityRuntime;
use serde_json::Value;
use serverless_core::{
    handlers::factors::{ApiKeyHandler, BasicHandler},
    plan::HandlerFactory,
};
use std::sync::Arc;
pub enum FactorPolicyProvider {
    Basic,
    ApiKey,
}
fn load(config: &RuntimeConfig, file: &str) -> Result<Value, InitError> {
    config
        .module_registry
        .load_config(config, file)
        .map_err(|_| {
            InitError::InvalidConfiguration("authentication: missing or invalid configuration")
        })
}
impl HandlerFactoryProvider for FactorPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        match self {
            Self::Basic => Ok(Arc::new(
                BasicHandler::from_value(&load(config, "basic-auth.yml")?).map_err(|_| {
                    InitError::InvalidConfiguration(
                        "basic-auth.yml: invalid or deferred configuration",
                    )
                })?,
            )),
            Self::ApiKey => Ok(Arc::new(
                ApiKeyHandler::from_value(&load(config, "apikey.yml")?).map_err(|_| {
                    InitError::InvalidConfiguration(
                        "apikey.yml: invalid or unbounded configuration",
                    )
                })?,
            )),
        }
    }
}
pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    let value: Value = match config.module_registry.load_config(config, "basic-auth.yml") {
        Ok(value) => value,
        Err(RuntimeError::MissingConfig(_)) => serde_json::json!({}),
        Err(_) => {
            return Err(InitError::InvalidConfiguration(
                "basic-auth.yml: invalid configuration",
            ));
        }
    };
    BasicHandler::from_value(&value).map_err(|_| {
        InitError::InvalidConfiguration("basic-auth.yml: invalid or deferred configuration")
    })?;
    let mode = if value.get("enableAD").is_none() {
        "absent"
    } else {
        "default-false"
    };
    Ok(vec![format!(
        "basic-auth.enableAD: deferred {mode} ignored"
    )])
}
