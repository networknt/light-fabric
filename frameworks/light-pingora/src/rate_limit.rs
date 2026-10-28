use crate::{
    policy_adapter::{PolicyRequestRef, PolicyResponse},
    security::{AuthPrincipal, HandlerRejection},
};
pub use gateway_policy::rate_limit::*;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::{http::ResponseHeader, prelude::Session};
pub fn load_rate_limit_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<RateLimitRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<LimitConfig>(runtime_config, LIMIT_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == LIMIT_FILE => LimitConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        LIMIT_MODULE_ID,
        LIMIT_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(RateLimitRuntime::new(config)))
}

pub fn check_rate_limit(
    session: &Session,
    runtime: &RateLimitRuntime,
    principal: Option<&AuthPrincipal>,
    request_path: &str,
) -> Result<Option<RateLimitHeaders>, HandlerRejection> {
    let peer_address = session.as_downstream().client_addr().map(|address| {
        address
            .as_inet()
            .map(|address| address.ip().to_string())
            .unwrap_or_else(|| address.to_string())
    });
    gateway_policy::rate_limit::check_rate_limit(
        &PolicyRequestRef(session),
        runtime,
        principal,
        request_path,
        peer_address.as_deref(),
    )
}
pub fn apply_rate_limit_headers(
    response: &mut ResponseHeader,
    headers: &RateLimitHeaders,
) -> pingora::Result<()> {
    gateway_policy::rate_limit::apply_rate_limit_headers(&mut PolicyResponse(response), headers)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_java_limit_strings() {
        let config: LimitConfig = serde_yaml::from_str(
            r#"
enabled: true
rateLimit: 10/s, 100/m
key: server
server:
  /api: 5/s
"#,
        )
        .expect("parse limit config");

        assert_eq!(config.rate_limit[0].limit, 10);
        assert_eq!(config.rate_limit[1].window_seconds, 60);
        assert_eq!(config.server["/api"][0].limit, 5);
    }

    #[test]
    fn legacy_disabled_queue_size_maps_to_zero() {
        let config: LimitConfig =
            serde_yaml::from_str("queueSize: -1\n").expect("parse legacy disabled queue size");
        assert_eq!(0, config.queue_size);

        let configured: LimitConfig =
            serde_yaml::from_str("queueSize: 32\n").expect("parse configured queue size");
        assert_eq!(32, configured.queue_size);

        assert!(serde_yaml::from_str::<LimitConfig>("queueSize: -2\n").is_err());
    }
}
