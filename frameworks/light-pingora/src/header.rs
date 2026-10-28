use crate::policy_adapter::{PolicyRequest, PolicyResponse};
pub use gateway_policy::header::{
    HEADER_CONFIG_NAME, HEADER_FILE, HEADER_MODULE_ID, HeaderConfig, HeaderMutation,
    HeaderPathPrefixConfig,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::{http::ResponseHeader, prelude::Session};
pub fn load_header_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<HeaderConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<HeaderConfig>(runtime_config, HEADER_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == HEADER_FILE => HeaderConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        HEADER_MODULE_ID,
        HEADER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn apply_header_request(
    session: &mut Session,
    config: &HeaderConfig,
    request_path: &str,
) -> pingora::Result<()> {
    gateway_policy::header::apply_header_request(&mut PolicyRequest(session), config, request_path)
}
pub fn apply_header_response(
    response: &mut ResponseHeader,
    config: &HeaderConfig,
    request_path: &str,
) -> pingora::Result<()> {
    gateway_policy::header::apply_header_response(
        &mut PolicyResponse(response),
        config,
        request_path,
    )
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_config_accepts_string_shortcuts() {
        let config: HeaderConfig = serde_yaml::from_str(
            r#"
enabled: true
request:
  remove: a,b
  update: 'x-one=1,x-two=2'
pathPrefixHeader:
  /api:
    response:
      update:
        x-api: true
"#,
        )
        .expect("parse header config");

        assert_eq!(config.request.remove, ["a", "b"]);
        assert_eq!(config.request.update["x-one"], "1");
        assert_eq!(
            config
                .path_prefix_header
                .get("/api")
                .expect("path config")
                .response
                .update["x-api"],
            "true"
        );
    }
}
