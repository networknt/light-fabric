//! Common offline/startup enforcement of the approved closed EMF key disposition.
use crate::init_error::InitError;
use light_runtime::RuntimeConfig;

pub(crate) fn warnings(config: &RuntimeConfig) -> Result<Vec<String>, InitError> {
    let value: serde_json::Value = match config.module_registry.load_config(config, "metrics.yml") {
        Ok(value) => value,
        Err(light_runtime::RuntimeError::MissingConfig(_)) => serde_json::json!({"enabled":false}),
        Err(_) => {
            return Err(InitError::InvalidConfiguration(
                "metrics.yml: malformed configuration",
            ));
        }
    };
    serverless_core::unsupported::metrics::validate_configuration(&value).map_err(|_| {
        InitError::InvalidConfiguration("metrics.yml: unsupported non-default APM label setting")
    })
}

pub struct MetricsPolicyProvider {
    pub function_name: String,
}
impl crate::init::HandlerFactoryProvider for MetricsPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<std::sync::Arc<light_security::SecurityRuntime>>,
    ) -> Result<std::sync::Arc<dyn serverless_core::plan::HandlerFactory>, InitError> {
        warnings(config)?;
        // Capture only immutable route keys. Destination identity/configuration
        // remains owned by the common compiler; this performs no provider work.
        let proxy: serde_json::Value = config
            .module_registry
            .load_config(config, "lambda-proxy.yml")
            .map_err(|_| {
                InitError::InvalidConfiguration("metrics: invalid proxy route configuration")
            })?;
        let routes = match proxy.get("functions") {
            None | Some(serde_json::Value::Null) => serverless_core::plan::TargetRoutes::default(),
            Some(serde_json::Value::Object(functions)) => {
                serverless_core::plan::TargetRoutes::compile(functions.keys().map(|route| {
                    (
                        route.clone(),
                        serverless_core::Target::Function("observation-only".into()),
                    )
                }))
                .map_err(|_| {
                    InitError::InvalidConfiguration("metrics: invalid proxy route configuration")
                })?
            }
            _ => {
                return Err(InitError::InvalidConfiguration(
                    "metrics: invalid proxy route configuration",
                ));
            }
        };
        let handler = serverless_core::handlers::metrics::MetricsHandler::new(
            self.function_name.clone(),
            std::sync::Arc::new(gateway_policy::observation::StdoutSink),
        )
        .map_err(|_| InitError::InvalidConfiguration("metrics.yml: invalid trusted function name"))?
        .with_routes(std::sync::Arc::new(routes));
        Ok(std::sync::Arc::new(handler))
    }
}
