//! Trusted Router composition; the application profile admits forwarding only.
mod captured;
mod prepared;
use crate::{http_router::HttpRouterBackend, init::HandlerFactoryProvider, init_error::InitError};
pub use captured::{CaptureLimits, CapturedRouterSources};
use light_client::{ClientConfig, ClientFactory};
use light_runtime::RuntimeConfig;
use light_security::SecurityRuntime;
pub use prepared::PreparedRewriteProgram;
use serverless_core::{handlers::router::RouterHandler, plan::HandlerFactory};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
pub struct RouterPolicyProvider;
/// Application forwarding profile. Java regex workers are deliberately not
/// admitted by this provider; a configured rewrite fails Init explicitly.
pub struct ApplicationRouterPolicyProvider;
impl HandlerFactoryProvider for ApplicationRouterPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(self.prepare_router_resources(config, security)?.factory)
    }
    fn prepare_router_resources(
        &self,
        config: &RuntimeConfig,
        security: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedRouterResources, InitError> {
        let value: serde_json::Value = config
            .module_registry
            .load_config(config, "router.yml")
            .map_err(|_| invalid())?;
        if !serverless_core::unsupported::strings(value.get("urlRewriteRules"))
            .map_err(|_| invalid())?
            .is_empty()
        {
            return Err(InitError::InvalidConfiguration(
                "router: Java rewrite execution is unavailable in the application forwarding profile",
            ));
        }
        RouterPolicyProvider.prepare_router_resources(config, security)
    }
}
fn invalid() -> InitError {
    InitError::InvalidConfiguration("router: unsupported or malformed trusted configuration")
}
fn constructor_properties(
    value: &serde_json::Value,
) -> Result<serde_json::Map<String, serde_json::Value>, InitError> {
    if let Some(properties) = value.as_object() {
        return Ok(properties.clone());
    }
    let args = value.as_array().ok_or_else(invalid)?;
    if ![4, 5].contains(&args.len()) {
        return Err(invalid());
    }
    let mut properties = serde_json::Map::new();
    for (index, arg) in args.iter().enumerate() {
        let arg = arg.as_object().ok_or_else(invalid)?;
        if arg.len() != 1 {
            return Err(invalid());
        }
        let (name, value) = arg.iter().next().ok_or_else(invalid)?;
        let (key, valid) = match index {
            0 => (
                "protocol",
                name == "java.lang.String" && (value.is_string() || value.is_null()),
            ),
            1 => (
                "host",
                name == "java.lang.String" && (value.is_string() || value.is_null()),
            ),
            2 => (
                "port",
                name == "int" && value.as_i64().is_some_and(|n| i32::try_from(n).is_ok()),
            ),
            3 => (
                "path",
                name == "java.lang.String" && (value.is_string() || value.is_null()),
            ),
            _ => (
                "parameters",
                ["java.util.Map", "java.util.HashMap"].contains(&name.as_str())
                    && (value.is_object() || value.is_null()),
            ),
        };
        if !valid {
            return Err(invalid());
        }
        properties.insert(key.into(), value.clone());
    }
    Ok(properties)
}
fn destinations(config: &RuntimeConfig) -> Result<BTreeMap<String, String>, InitError> {
    let value = match config.module_registry.load_config(config, "service.yml") {
        Ok(value) => value,
        Err(light_runtime::RuntimeError::MissingConfig(_)) => serde_json::json!({}),
        Err(_) => return Err(invalid()),
    };
    destinations_value(&value, &config.direct_registry.direct_urls)
}
fn destinations_value(
    value: &serde_json::Value,
    direct: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, InitError> {
    let map = value.as_object().ok_or_else(invalid)?;
    if map.keys().any(|k| k != "singletons") {
        return Err(invalid());
    }
    let mut parameters = None;
    if let Some(bindings) = map.get("singletons") {
        for binding in bindings.as_array().ok_or_else(invalid)? {
            let binding = binding.as_object().ok_or_else(invalid)?;
            if binding.len() != 1 {
                return Err(invalid());
            }
            for (interface, implementations) in binding {
                let expected = match interface.as_str() {
                    "com.networknt.registry.URL" => "com.networknt.registry.URLImpl",
                    "com.networknt.registry.Registry" => {
                        "com.networknt.registry.support.DirectRegistry"
                    }
                    "com.networknt.balance.LoadBalance" => {
                        "com.networknt.balance.RoundRobinLoadBalance"
                    }
                    "com.networknt.cluster.Cluster" => "com.networknt.cluster.LightCluster",
                    _ => return Err(invalid()),
                };
                let implementations = implementations.as_array().ok_or_else(invalid)?;
                if implementations.len() != 1 {
                    return Err(invalid());
                }
                if implementations[0].as_str() == Some(expected) {
                    continue;
                }
                if interface != "com.networknt.registry.URL" {
                    return Err(invalid());
                }
                let declaration = implementations[0].as_object().ok_or_else(invalid)?;
                if declaration.len() != 1 {
                    return Err(invalid());
                }
                let properties =
                    constructor_properties(declaration.get(expected).ok_or_else(invalid)?)?;
                if properties.keys().any(|k| {
                    !["protocol", "host", "port", "path", "parameters"].contains(&k.as_str())
                }) {
                    return Err(invalid());
                }
                for key in ["protocol", "host", "path"] {
                    if properties
                        .get(key)
                        .is_some_and(|v| !v.is_string() && !v.is_null())
                    {
                        return Err(invalid());
                    }
                }
                if properties.get("port").is_some_and(|v| {
                    !v.is_null() && v.as_i64().is_none_or(|n| i32::try_from(n).is_err())
                }) {
                    return Err(invalid());
                }
                if let Some(value) = properties.get("parameters").filter(|v| !v.is_null()) {
                    let values = value.as_object().ok_or_else(invalid)?;
                    let values = values
                        .iter()
                        .map(|(key, value)| {
                            Ok((key.clone(), value.as_str().ok_or_else(invalid)?.to_owned()))
                        })
                        .collect::<Result<BTreeMap<_, _>, InitError>>()?;
                    if !values.is_empty() {
                        parameters = Some(values);
                    }
                }
            }
        }
    }
    if let Some(parameters) = parameters {
        let mut catalogue: BTreeMap<String, String> = BTreeMap::new();
        for (key, values) in parameters {
            for raw in values.split(',') {
                let tag = raw.split_once('?').and_then(|(_, query)| {
                    query
                        .split('&')
                        .filter_map(|part| {
                            let part = part.trim();
                            let (k, v) = part.split_once('=').unwrap_or((part, part));
                            (k == "environment").then_some(v)
                        })
                        .next_back()
                });
                let key = tag.map_or_else(|| key.clone(), |tag| format!("{key}|{tag}"));
                catalogue
                    .entry(key)
                    .and_modify(|urls| {
                        urls.push(',');
                        urls.push_str(raw);
                    })
                    .or_insert_with(|| raw.to_owned());
            }
        }
        Ok(catalogue)
    } else {
        Ok(direct.clone())
    }
}
type Settings = (
    BTreeMap<String, Vec<String>>,
    Duration,
    Vec<(String, String)>,
);
/// Immutable one-Init capture; fields cannot be supplied or mutated by callers.
pub struct RouterSettings(Settings);
/// Explicit trusted runtime handoff; ordinary providers keep the forwarding path.
pub struct PreparedRouterRuntime {
    pub prepared: PreparedRouterResources,
    pub backend: Option<Arc<dyn serverless_core::Backend>>,
    pub lifecycle: Option<Arc<dyn crate::init::RuntimeLifecycle>>,
}
pub struct PreparedRouterResources {
    pub factory: Arc<dyn HandlerFactory>,
    pub resources: Option<Arc<RouterSettings>>,
}
/// Source-level opt-in only. No HandlerFactory is published until resource,
/// recovery and built-app admission gates close.
pub struct PreparedJavaRouter {
    pub resources: Arc<RouterSettings>,
    pub worker: crate::router_java_worker::Session,
}
impl RouterPolicyProvider {
    pub async fn prepare_with_java(
        &self,
        config: &RuntimeConfig,
        preparation: crate::router_java_worker::Preparation<'_>,
        deadline: tokio::time::Instant,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<PreparedJavaRouter, InitError> {
        let value = config
            .module_registry
            .load_config(config, "router.yml")
            .map_err(|_| invalid())?;
        self.prepare_java_values(
            value,
            || destinations(config),
            preparation,
            deadline,
            cancel,
        )
        .await
    }
    pub async fn prepare_captured_with_java(
        &self,
        sources: &CapturedRouterSources,
        preparation: crate::router_java_worker::Preparation<'_>,
        deadline: tokio::time::Instant,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<PreparedJavaRouter, InitError> {
        let (value, destinations) = sources.resolve()?;
        self.prepare_java_values(value, || Ok(destinations), preparation, deadline, cancel)
            .await
    }
    async fn prepare_java_values(
        &self,
        value: serde_json::Value,
        destinations: impl FnOnce() -> Result<BTreeMap<String, String>, InitError>,
        preparation: crate::router_java_worker::Preparation<'_>,
        deadline: tokio::time::Instant,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<PreparedJavaRouter, InitError> {
        serverless_core::unsupported::remaining::router(&value).map_err(|_| invalid())?;
        let mut program = Vec::new();
        for rule in serverless_core::unsupported::strings(value.get("urlRewriteRules"))
            .map_err(|_| invalid())?
        {
            // Java StringUtils.split(s, ' ') removes empty space-delimited
            // tokens; tabs and other Unicode whitespace are not separators.
            let parts = rule
                .split(' ')
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            if parts.len() != 2 {
                return Err(invalid());
            }
            program.push(crate::router_java_worker::Rule {
                pattern: parts[0].to_owned(),
                replacement: parts[1].to_owned(),
            });
        }
        let https = value
            .get("httpsEnabled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let catalogue = destinations()?
            .into_iter()
            .map(|(id, raw)| {
                let urls = raw
                    .split(',')
                    .map(|raw| normalize_destination(raw.trim(), https))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((id, urls))
            })
            .collect::<Result<BTreeMap<_, _>, InitError>>()?;
        let timeout = Duration::from_millis(
            value
                .get("maxRequestTime")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1000),
        );
        let worker = crate::router_java_worker::Session::prepare(
            preparation,
            program.into(),
            deadline,
            cancel,
        )
        .await
        .map_err(|_| invalid())?;
        Ok(PreparedJavaRouter {
            resources: Arc::new(RouterSettings((catalogue, timeout, Vec::new()))),
            worker,
        })
    }
}
// Java DirectRegistry moves the raw configured path to basePath, then
// LightCluster reconstructs requested protocol, host, port and base path.
// Shared registry/token semantics do not change.
fn normalize_destination(raw: &str, https: bool) -> Result<String, InitError> {
    let url = url::Url::parse(raw).map_err(|_| invalid())?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(invalid());
    }
    let userinfo = raw.split_once("://").is_some_and(|(_, tail)| {
        tail.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    if userinfo
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(InitError::InvalidConfiguration(
            "router: unsupported trusted destination",
        ));
    }
    let host = url.host().ok_or_else(invalid)?;
    let port = url
        .port_or_known_default()
        .filter(|port| *port != 0)
        .or_else(|| match url.scheme() {
            "http" => Some(80),
            "https" => Some(443),
            _ => None,
        })
        .ok_or_else(invalid)?;
    let (body, query) = raw.split_once('?').unwrap_or((raw, ""));
    let authority = body.split_once("://").ok_or_else(invalid)?.1;
    let path = authority.split_once('/').map_or("", |(_, p)| p);
    let path = path.trim().trim_matches('/');
    let mut base = query
        .split('&')
        .filter_map(|part| {
            let part = part.trim();
            let (key, value) = part.split_once('=').unwrap_or((part, part));
            (key == "basePath").then_some(value)
        })
        .next_back()
        .unwrap_or("")
        .trim()
        .to_owned();
    if !path.is_empty() {
        base = format!("/{path}");
    } else if !base.is_empty() && !base.starts_with('/') {
        base.insert(0, '/');
    }
    Ok(format!(
        "{}://{host}:{port}{base}",
        if https { "https" } else { "http" }
    ))
}
fn settings(config: &RuntimeConfig) -> Result<Settings, InitError> {
    let value = config
        .module_registry
        .load_config(config, "router.yml")
        .map_err(|_| invalid())?;
    serverless_core::unsupported::remaining::router(&value).map_err(|_| invalid())?;
    let mut rewrites = Vec::new();
    for rule in serverless_core::unsupported::strings(value.get("urlRewriteRules"))
        .map_err(|_| invalid())?
    {
        let parts = rule.split_whitespace().collect::<Vec<_>>();
        if parts.len() != 2
            || parts[0].contains([
                '\\', '.', '^', '$', '?', '*', '+', '[', ']', '(', ')', '{', '}', '|',
            ])
            || parts[1].contains(['\\', '$'])
        {
            return Err(InitError::InvalidConfiguration(
                "router: regex rewrite qualification pending",
            ));
        }
        rewrites.push((parts[0].to_owned(), parts[1].to_owned()));
    }
    let ms = value
        .get("maxRequestTime")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(1000);
    let https = value
        .get("httpsEnabled")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let destinations = destinations(config)?
        .into_iter()
        .map(|(id, raw)| {
            let urls = raw
                .split(',')
                .map(|url| normalize_destination(url.trim(), https))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((id, urls))
        })
        .collect::<Result<_, _>>()?;
    Ok((destinations, Duration::from_millis(ms), rewrites))
}

pub(crate) fn prepare_backend(
    config: &RuntimeConfig,
    limits: &serverless_aws::limits::Limits,
) -> Result<Arc<HttpRouterBackend>, InitError> {
    prepare_backend_from_resources(config, limits, &RouterSettings(settings(config)?))
}
pub(crate) fn prepare_backend_from_resources(
    config: &RuntimeConfig,
    limits: &serverless_aws::limits::Limits,
    resources: &RouterSettings,
) -> Result<Arc<HttpRouterBackend>, InitError> {
    let (destinations, timeout, rewrites) = &resources.0;
    let client: ClientConfig = config.client.clone().unwrap_or_default();
    Ok(Arc::new(
        HttpRouterBackend::from_catalogue(
            destinations.clone(),
            &ClientFactory::from_config(&client),
            *timeout,
            limits,
        )?
        .with_literal_rewrites(rewrites.clone()),
    ))
}
impl HandlerFactoryProvider for RouterPolicyProvider {
    fn create(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, InitError> {
        Ok(Arc::new(RouterHandler::new(
            settings(config)?.0.into_keys(),
        )))
    }
    fn prepare_router_resources(
        &self,
        config: &RuntimeConfig,
        _: Option<Arc<SecurityRuntime>>,
    ) -> Result<PreparedRouterResources, InitError> {
        let resources = Arc::new(RouterSettings(settings(config)?));
        let factory = Arc::new(RouterHandler::new(resources.0.0.keys().cloned()));
        Ok(PreparedRouterResources {
            factory,
            resources: Some(resources),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{constructor_properties, normalize_destination};
    #[test]
    fn only_literal_urlimpl_constructor_arguments_are_decoded() {
        let four = serde_json::json!([{"java.lang.String":"http"},{"java.lang.String":"localhost"},{"int":0},{"java.lang.String":"direct"}]);
        assert!(
            !constructor_properties(&four)
                .unwrap()
                .contains_key("parameters")
        );
        let mut five = four.clone();
        five.as_array_mut()
            .unwrap()
            .push(serde_json::json!({"java.util.Map":{"service":"http://one:80,http://two:80"}}));
        assert_eq!(
            constructor_properties(&five).unwrap()["parameters"]["service"],
            "http://one:80,http://two:80"
        );
        let mut plugin = four.clone();
        plugin[0] = serde_json::json!({"com.example.Plugin":"http"});
        assert!(constructor_properties(&plugin).is_err());
        let mut malformed = four;
        malformed[2] = serde_json::json!({"int":"not-an-int"});
        assert!(constructor_properties(&malformed).is_err());
    }
    #[test]
    fn java_registry_base_query_and_requested_protocol_are_router_local() {
        for (raw, https, expected) in [
            ("http://host:8080", false, "http://host:8080"),
            ("http://host:8080/", false, "http://host:8080"),
            (
                "http://host:8080///base///?fixed=1",
                false,
                "http://host:8080/base",
            ),
            ("http://host:8080?fixed=1", false, "http://host:8080"),
            (
                "http://host:8080?basePath=service/",
                false,
                "http://host:8080/service/",
            ),
            (
                "http://host:8080/base?basePath=ignored",
                false,
                "http://host:8080/base",
            ),
            ("http://host", true, "https://host:80"),
            ("https://host", false, "http://host:443"),
            ("https://host:8443/a%2Fb", true, "https://host:8443/a%2Fb"),
        ] {
            assert_eq!(
                normalize_destination(raw, https).unwrap(),
                expected,
                "{raw}"
            );
        }
        for raw in ["http://user:pass@host:8080", "http://host:8080#fragment"] {
            assert!(normalize_destination(raw, false).is_err());
        }
    }
}
