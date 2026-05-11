use crate::proxy::ProxyTarget;
use crate::router::parse_router_target;
use light_runtime::{DirectRegistryConfig, RuntimeError};
use url::Url;

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectRegistryMatch<'a> {
    pub key: &'a str,
    pub url: &'a str,
}

pub(crate) fn direct_registry_match<'a>(
    config: &'a DirectRegistryConfig,
    service_id: &str,
    env_tag: Option<&str>,
) -> Option<DirectRegistryMatch<'a>> {
    env_tag
        .filter(|value| !value.trim().is_empty())
        .and_then(|env_tag| {
            let key = format!("{service_id}|{env_tag}");
            config
                .direct_urls
                .get_key_value(key.as_str())
                .map(|(key, url)| DirectRegistryMatch { key, url })
        })
        .or_else(|| {
            config
                .direct_urls
                .get_key_value(service_id)
                .map(|(key, url)| DirectRegistryMatch { key, url })
        })
}

pub(crate) fn validate_direct_registry_protocol(
    matched: DirectRegistryMatch<'_>,
    expected_protocol: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(expected_protocol) = expected_protocol.filter(|value| !value.trim().is_empty()) else {
        return Ok(());
    };
    let url_value = matched.url.trim();
    let url = Url::parse(url_value).map_err(|error| {
        RuntimeError::Unsupported(format!(
            "direct-registry.directUrls `{}` value `{}` is invalid: {error}",
            matched.key, url_value
        ))
    })?;
    if url.scheme().eq_ignore_ascii_case(expected_protocol) {
        return Ok(());
    }
    Err(RuntimeError::Unsupported(format!(
        "direct-registry.directUrls `{}` value `{}` uses protocol `{}`, expected `{}`",
        matched.key,
        url_value,
        url.scheme(),
        expected_protocol
    )))
}

pub(crate) fn direct_registry_target(
    config: &DirectRegistryConfig,
    service_id: &str,
    env_tag: Option<&str>,
    expected_protocol: Option<&str>,
) -> Result<Option<ProxyTarget>, RuntimeError> {
    let Some(matched) = direct_registry_match(config, service_id, env_tag) else {
        return Ok(None);
    };
    validate_direct_registry_protocol(matched, expected_protocol)?;
    Ok(Some(parse_router_target(matched.url.trim())?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn env_specific_direct_url_wins_over_plain_service_id() {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([
                (
                    "com.networknt.controller-1.0.0".to_string(),
                    "https://controller-default:8438".to_string(),
                ),
                (
                    "com.networknt.controller-1.0.0|dev".to_string(),
                    "https://controller-dev:8438".to_string(),
                ),
            ]),
        };

        let matched = direct_registry_match(&config, "com.networknt.controller-1.0.0", Some("dev"))
            .expect("direct registry match");

        assert_eq!(matched.key, "com.networknt.controller-1.0.0|dev");
        assert_eq!(matched.url, "https://controller-dev:8438");
    }

    #[test]
    fn protocol_mismatch_is_rejected() {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([(
                "com.networknt.controller-1.0.0".to_string(),
                "http://controller:8438".to_string(),
            )]),
        };
        let matched =
            direct_registry_match(&config, "com.networknt.controller-1.0.0", None).unwrap();

        let error =
            validate_direct_registry_protocol(matched, Some("https")).expect_err("protocol error");

        assert!(
            error
                .to_string()
                .contains("uses protocol `http`, expected `https`")
        );
    }
}
