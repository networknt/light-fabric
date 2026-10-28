use light_runtime::DirectRegistryConfig;

#[derive(Debug, Clone, Copy)]
pub struct DirectRegistryMatch<'a> {
    pub key: &'a str,
    pub url: &'a str,
}

pub fn direct_registry_match<'a>(
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
        // Apply only after env-tag selection: a selected multi-destination
        // entry must not fall back to a different untagged single URL.
        .filter(|matched| !matched.url.contains(','))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    #[test]
    fn csv_fails_closed_after_existing_tag_precedence() {
        let config = DirectRegistryConfig {
            direct_urls: BTreeMap::from([
                ("service".into(), " http://base:80 ".into()),
                ("service|csv".into(), "http://one:80,http://two:80".into()),
                ("service|single".into(), "http://tag:80".into()),
                ("multi".into(), "http://one:80,http://two:80".into()),
            ]),
        };
        assert!(direct_registry_match(&config, "service", Some("csv")).is_none());
        assert!(direct_registry_match(&config, "multi", None).is_none());
        for tag in [None, Some("missing"), Some("")] {
            let m = direct_registry_match(&config, "service", tag).unwrap();
            assert_eq!(m.key, "service");
            assert_eq!(m.url, " http://base:80 ");
        }
        assert_eq!(
            direct_registry_match(&config, "service", Some("single"))
                .unwrap()
                .url,
            "http://tag:80"
        );
    }
}
