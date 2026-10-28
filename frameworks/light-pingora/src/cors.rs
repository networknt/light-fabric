use crate::policy_adapter::{PolicyRequestRef, PolicyResponse};
pub use gateway_policy::cors::*;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::{http::ResponseHeader, prelude::Session};
pub fn load_cors_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<CorsConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<CorsConfig>(runtime_config, CORS_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == CORS_FILE => CorsConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        CORS_MODULE_ID,
        CORS_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn evaluate_cors_request(
    session: &Session,
    config: &CorsConfig,
    request_path: &str,
    server_scheme: &str,
    server_port: u16,
) -> CorsRequestOutcome {
    gateway_policy::cors::evaluate_cors_request(
        &PolicyRequestRef(session),
        config,
        request_path,
        server_scheme,
        server_port,
    )
}
pub fn apply_cors_response(
    response: &mut ResponseHeader,
    headers: &CorsResponseHeaders,
) -> pingora::Result<()> {
    gateway_policy::cors::apply_cors_response(&mut PolicyResponse(response), headers)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_config_accepts_string_and_path_prefix_values() {
        let config: CorsConfig = serde_yaml::from_str(
            r#"
allowedOrigins: https://a.example.com, https://b.example.com
allowedMethods: '["GET","POST"]'
pathPrefixAllowed:
  /v1:
    allowedOrigins:
      - https://v1.example.com
    allowedMethods: GET
  /v1/admin:
    allowedOrigins: https://admin.example.com
    allowedMethods:
      - GET
      - DELETE
"#,
        )
        .expect("parse CORS config");

        assert_eq!(config.allowed_origins.len(), 2);
        assert_eq!(config.allowed_methods, ["GET", "POST"]);
        assert_eq!(
            config.policy_for_path("/v1/admin/users").allowed_origins,
            ["https://admin.example.com"]
        );
        assert_eq!(
            config.policy_for_path("/v1/pets").allowed_origins,
            ["https://v1.example.com"]
        );
    }

    #[test]
    fn sanitize_origin_removes_only_default_ports() {
        assert_eq!(
            sanitize_default_port("https://example.com:443"),
            "https://example.com"
        );
        assert_eq!(
            sanitize_default_port("http://example.com:8080"),
            "http://example.com:8080"
        );
    }
}
