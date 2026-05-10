use crate::config_util::{
    deserialize_string_list, deserialize_typed_map, parse_string_list, request_header,
};
use crate::proxy::ProxyTarget;
use crate::security::HandlerRejection;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::http::RequestHeader;
use pingora::prelude::Session;
use regex::Regex;
use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::BTreeMap;
use url::{Host, Url, form_urlencoded};

pub const ROUTER_FILE: &str = "router.yml";
pub const ROUTER_MODULE_ID: &str = "light-pingora/router";
pub const ROUTER_CONFIG_NAME: &str = "router";

const SERVICE_ID_HEADER: &str = "service_id";
const SERVICE_URL_HEADER: &str = "service_url";
const ENV_TAG_HEADER: &str = "env_tag";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterConfig {
    #[serde(default = "default_true")]
    pub http2_enabled: bool,
    #[serde(default = "default_true")]
    pub https_enabled: bool,
    #[serde(default = "default_max_request_time")]
    pub max_request_time: u64,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub path_prefix_max_request_time: BTreeMap<String, u64>,
    #[serde(default = "default_connections_per_thread")]
    pub connections_per_thread: usize,
    #[serde(default)]
    pub max_queue_size: usize,
    #[serde(default = "default_soft_max_connections_per_thread")]
    pub soft_max_connections_per_thread: usize,
    #[serde(default = "default_true")]
    pub rewrite_host_header: bool,
    #[serde(default)]
    pub reuse_x_forwarded: bool,
    #[serde(default = "default_max_connection_retries")]
    pub max_connection_retries: usize,
    #[serde(default)]
    pub pre_resolve_fqdn2_ip: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub host_whitelist: Vec<String>,
    #[serde(default)]
    pub service_id_query_parameter: bool,
    #[serde(default, deserialize_with = "deserialize_url_rewrite_rules")]
    pub url_rewrite_rules: Vec<UrlRewriteRule>,
    #[serde(default, deserialize_with = "deserialize_method_rewrite_rules")]
    pub method_rewrite_rules: Vec<MethodRewriteRule>,
    #[serde(default, deserialize_with = "deserialize_endpoint_rule_map")]
    pub query_param_rewrite_rules: BTreeMap<String, Vec<QueryHeaderRewriteRule>>,
    #[serde(default, deserialize_with = "deserialize_endpoint_rule_map")]
    pub header_rewrite_rules: BTreeMap<String, Vec<QueryHeaderRewriteRule>>,
    #[serde(default)]
    pub metrics_injection: bool,
    #[serde(default = "default_metrics_name")]
    pub metrics_name: String,
    #[serde(default, deserialize_with = "deserialize_service_targets")]
    pub service_targets: BTreeMap<String, Vec<String>>,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            http2_enabled: true,
            https_enabled: true,
            max_request_time: default_max_request_time(),
            path_prefix_max_request_time: BTreeMap::new(),
            connections_per_thread: default_connections_per_thread(),
            max_queue_size: 0,
            soft_max_connections_per_thread: default_soft_max_connections_per_thread(),
            rewrite_host_header: true,
            reuse_x_forwarded: false,
            max_connection_retries: default_max_connection_retries(),
            pre_resolve_fqdn2_ip: false,
            host_whitelist: Vec::new(),
            service_id_query_parameter: false,
            url_rewrite_rules: Vec::new(),
            method_rewrite_rules: Vec::new(),
            query_param_rewrite_rules: BTreeMap::new(),
            header_rewrite_rules: BTreeMap::new(),
            metrics_injection: false,
            metrics_name: default_metrics_name(),
            service_targets: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlRewriteRule {
    pub pattern: String,
    pub replace: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodRewriteRule {
    pub request_path: String,
    pub source_method: String,
    pub target_method: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QueryHeaderRewriteRule {
    #[serde(default)]
    pub old_k: String,
    #[serde(default)]
    pub old_v: Option<String>,
    #[serde(default)]
    pub new_k: Option<String>,
    #[serde(default)]
    pub new_v: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouterRoute {
    pub config: RouterConfig,
    pub service_targets: BTreeMap<String, Vec<ProxyTarget>>,
}

#[derive(Debug, Clone)]
pub struct RouterDecision {
    pub target: ProxyTarget,
    pub remove_service_id_query: bool,
}

pub fn load_router_route(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<RouterRoute>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<RouterConfig>(runtime_config, ROUTER_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == ROUTER_FILE => RouterConfig::default(),
        Err(error) => return Err(error),
    };
    let service_targets = parse_service_targets(&config.service_targets)?;

    runtime_config.module_registry.register_loaded_config(
        ROUTER_MODULE_ID,
        ROUTER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        true,
        Some(true),
        true,
    )?;

    Ok(Some(RouterRoute {
        config,
        service_targets,
    }))
}

pub fn select_router_target(
    session: &Session,
    route: &RouterRoute,
    index: usize,
) -> Result<RouterDecision, HandlerRejection> {
    let request_uri = session.req_header().uri.clone();
    let query_service_id = route
        .config
        .service_id_query_parameter
        .then(|| query_param(request_uri.query(), SERVICE_ID_HEADER))
        .flatten();
    let remove_service_id_query = query_service_id.is_some();
    let service_id = query_service_id.or_else(|| request_header(session, SERVICE_ID_HEADER));
    let service_url = request_header(session, SERVICE_URL_HEADER);
    let env_tag = request_header(session, ENV_TAG_HEADER);

    if let Some(service_url) = service_url
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let target = parse_router_target(service_url).map_err(|error| {
            HandlerRejection::new(502, "ERR10080", format!("invalid service_url: {error}"))
        })?;
        if !host_is_allowed(&route.config, service_url) {
            return Err(HandlerRejection::forbidden(format!(
                "route to `{service_url}` is not allowed by router.hostWhitelist"
            )));
        }
        return Ok(RouterDecision {
            target,
            remove_service_id_query,
        });
    }

    let service_id = service_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            HandlerRejection::new(502, "ERR10080", "router requires service_url or service_id")
        })?;
    let key = env_tag
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|env_tag| format!("{service_id}|{env_tag}"))
        .unwrap_or_else(|| service_id.to_string());
    let targets = route.service_targets.get(&key).or_else(|| {
        env_tag
            .as_deref()
            .and_then(|_| route.service_targets.get(service_id))
    });
    let targets = targets.ok_or_else(|| {
        HandlerRejection::new(
            502,
            "ERR10080",
            format!("router service target `{key}` is not configured"),
        )
    })?;
    if targets.is_empty() {
        return Err(HandlerRejection::new(
            502,
            "ERR10080",
            format!("router service target `{key}` has no hosts"),
        ));
    }

    Ok(RouterDecision {
        target: targets[index % targets.len()].clone(),
        remove_service_id_query,
    })
}

pub fn apply_router_upstream_request(
    upstream_request: &mut RequestHeader,
    route: &RouterRoute,
    decision: &RouterDecision,
    endpoint: &str,
) -> pingora::Result<()> {
    let original_path = upstream_request.uri.path().to_string();
    apply_method_rewrite(upstream_request, route, original_path.as_str())?;
    apply_header_rewrite(upstream_request, route, original_path.as_str())?;
    apply_uri_rewrite(upstream_request, route, decision, endpoint)?;
    upstream_request.remove_header(SERVICE_URL_HEADER);
    upstream_request.remove_header(SERVICE_ID_HEADER);
    Ok(())
}

fn parse_service_targets(
    service_targets: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Vec<ProxyTarget>>, RuntimeError> {
    let mut parsed = BTreeMap::new();
    for (key, values) in service_targets {
        let mut targets = Vec::new();
        for value in values {
            targets.push(parse_router_target(value)?);
        }
        if !targets.is_empty() {
            parsed.insert(key.clone(), targets);
        }
    }
    Ok(parsed)
}

fn parse_router_target(raw_host: &str) -> Result<ProxyTarget, RuntimeError> {
    let url = Url::parse(raw_host)
        .map_err(|e| RuntimeError::Unsupported(format!("invalid router host `{raw_host}`: {e}")))?;
    let tls = match url.scheme() {
        "http" => false,
        "https" => true,
        scheme => {
            return Err(RuntimeError::Unsupported(format!(
                "router host `{raw_host}` uses unsupported scheme `{scheme}`"
            )));
        }
    };
    if url.username() != "" || url.password().is_some() {
        return Err(RuntimeError::Unsupported(format!(
            "router host `{raw_host}` must not contain user info"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(RuntimeError::Unsupported(format!(
            "router host `{raw_host}` must not contain query or fragment"
        )));
    }

    let host = url.host().ok_or_else(|| {
        RuntimeError::Unsupported(format!("router host `{raw_host}` is missing a host"))
    })?;
    let host_for_authority = host_for_authority(host);
    let sni = url.host_str().unwrap_or_default().to_string();
    let port = url.port_or_known_default().ok_or_else(|| {
        RuntimeError::Unsupported(format!("router host `{raw_host}` is missing a port"))
    })?;
    let address = format!("{host_for_authority}:{port}");
    let host_header = match url.port() {
        Some(_) => address.clone(),
        None => host_for_authority,
    };
    let path_prefix = normalize_path_prefix(url.path());

    Ok(ProxyTarget {
        address,
        tls,
        sni,
        host_header,
        path_prefix,
    })
}

fn host_is_allowed(config: &RouterConfig, service_url: &str) -> bool {
    let Ok(url) = Url::parse(service_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    if config.host_whitelist.is_empty() {
        return false;
    }
    config
        .host_whitelist
        .iter()
        .any(|pattern| Regex::new(pattern).is_ok_and(|regex| regex_matches_full(&regex, host)))
}

fn regex_matches_full(regex: &Regex, value: &str) -> bool {
    regex
        .find(value)
        .is_some_and(|matched| matched.start() == 0 && matched.end() == value.len())
}

fn apply_method_rewrite(
    upstream_request: &mut RequestHeader,
    route: &RouterRoute,
    request_path: &str,
) -> pingora::Result<()> {
    for rule in &route.config.method_rewrite_rules {
        if path_pattern_matches(rule.request_path.as_str(), request_path)
            && upstream_request
                .method
                .as_str()
                .eq_ignore_ascii_case(rule.source_method.as_str())
        {
            upstream_request.method = rule.target_method.parse().map_err(|error| {
                pingora::Error::because(
                    pingora::ErrorType::InvalidHTTPHeader,
                    format!("invalid router target method `{}`", rule.target_method),
                    error,
                )
            })?;
            break;
        }
    }
    Ok(())
}

fn apply_header_rewrite(
    upstream_request: &mut RequestHeader,
    route: &RouterRoute,
    request_path: &str,
) -> pingora::Result<()> {
    let Some(rules) = match_endpoint_rules(&route.config.header_rewrite_rules, request_path) else {
        return Ok(());
    };
    for rule in rules {
        if rule.old_k.is_empty() {
            continue;
        }
        let Some(current) = upstream_request
            .headers
            .get(rule.old_k.as_str())
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
        else {
            continue;
        };
        let mut value = current;
        if let (Some(old_v), Some(new_v)) = (rule.old_v.as_deref(), rule.new_v.as_deref())
            && value == old_v
        {
            value = new_v.to_string();
        }
        let name = rule.new_k.as_deref().unwrap_or(rule.old_k.as_str());
        if name != rule.old_k {
            upstream_request.remove_header(rule.old_k.as_str());
        }
        upstream_request.insert_header(name.to_string(), value)?;
    }
    Ok(())
}

fn apply_uri_rewrite(
    upstream_request: &mut RequestHeader,
    route: &RouterRoute,
    decision: &RouterDecision,
    endpoint: &str,
) -> pingora::Result<()> {
    let original_path = upstream_request.uri.path().to_string();
    let original_query = upstream_request.uri.query().map(str::to_string);
    let rewritten = rewrite_path(route, original_path.as_str());
    let (rewritten_path, rewritten_query) = split_path_query(rewritten.as_str());
    let target_path = prepend_path_prefix(decision.target.path_prefix.as_str(), rewritten_path);
    let query = rewrite_query(
        route,
        original_path.as_str(),
        endpoint,
        original_query.as_deref(),
        rewritten_query,
        decision.remove_service_id_query,
    );
    let path_and_query = query.map_or(target_path.clone(), |query| {
        if query.is_empty() {
            target_path.clone()
        } else {
            format!("{target_path}?{query}")
        }
    });
    let uri = path_and_query.parse().map_err(|error| {
        pingora::Error::because(
            pingora::ErrorType::InvalidHTTPHeader,
            format!("invalid router URI `{path_and_query}`"),
            error,
        )
    })?;
    upstream_request.set_uri(uri);
    Ok(())
}

fn rewrite_path(route: &RouterRoute, request_path: &str) -> String {
    for rule in &route.config.url_rewrite_rules {
        let Ok(regex) = Regex::new(rule.pattern.as_str()) else {
            continue;
        };
        let Some(matched) = regex.find(request_path) else {
            continue;
        };
        if matched.start() == 0 && matched.end() == request_path.len() {
            return regex
                .replace(request_path, rule.replace.as_str())
                .into_owned();
        }
    }
    request_path.to_string()
}

fn rewrite_query(
    route: &RouterRoute,
    request_path: &str,
    endpoint: &str,
    original_query: Option<&str>,
    rewritten_query: Option<&str>,
    remove_service_id_query: bool,
) -> Option<String> {
    let rules = match_endpoint_rules(&route.config.query_param_rewrite_rules, request_path)
        .or_else(|| match_endpoint_rules(&route.config.query_param_rewrite_rules, endpoint));
    let mut pairs = Vec::<(String, String)>::new();
    if let Some(rewritten_query) = rewritten_query {
        pairs.extend(parse_query_pairs(Some(rewritten_query), false));
    }
    pairs.extend(parse_query_pairs(original_query, remove_service_id_query));

    if let Some(rules) = rules {
        pairs = rewrite_query_pairs(pairs, rules);
    }

    if pairs.is_empty() {
        None
    } else {
        let mut serializer = form_urlencoded::Serializer::new(String::new());
        for (key, value) in pairs {
            serializer.append_pair(key.as_str(), value.as_str());
        }
        Some(serializer.finish())
    }
}

fn rewrite_query_pairs(
    pairs: Vec<(String, String)>,
    rules: &[QueryHeaderRewriteRule],
) -> Vec<(String, String)> {
    pairs
        .into_iter()
        .map(|(key, value)| {
            let mut rewritten_key = key;
            let mut rewritten_value = value;
            for rule in rules {
                if rule.old_k != rewritten_key {
                    continue;
                }
                if let (Some(old_v), Some(new_v)) = (rule.old_v.as_deref(), rule.new_v.as_deref())
                    && rewritten_value == old_v
                {
                    rewritten_value = new_v.to_string();
                }
                if let Some(new_k) = rule.new_k.as_deref() {
                    rewritten_key = new_k.to_string();
                }
            }
            (rewritten_key, rewritten_value)
        })
        .collect()
}

fn parse_query_pairs(query: Option<&str>, remove_service_id: bool) -> Vec<(String, String)> {
    query
        .map(|query| {
            form_urlencoded::parse(query.as_bytes())
                .filter(|(key, _)| !(remove_service_id && key == SERVICE_ID_HEADER))
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn split_path_query(path_and_query: &str) -> (&str, Option<&str>) {
    path_and_query
        .split_once('?')
        .map_or((path_and_query, None), |(path, query)| (path, Some(query)))
}

fn prepend_path_prefix(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        return ensure_path(path);
    }
    let path = ensure_path(path);
    if path == "/" {
        prefix.to_string()
    } else {
        format!("{}{}", prefix.trim_end_matches('/'), path)
    }
}

fn ensure_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    form_urlencoded::parse(query?.as_bytes())
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.into_owned())
}

fn match_endpoint_rules<'a>(
    rules: &'a BTreeMap<String, Vec<QueryHeaderRewriteRule>>,
    request_path: &str,
) -> Option<&'a [QueryHeaderRewriteRule]> {
    rules
        .iter()
        .filter(|(pattern, rules)| {
            !rules.is_empty()
                && (path_pattern_matches(pattern.as_str(), request_path)
                    || request_path.starts_with(pattern.as_str()))
        })
        .max_by_key(|(pattern, _)| pattern.len())
        .map(|(_, rules)| rules.as_slice())
}

fn path_pattern_matches(pattern: &str, path: &str) -> bool {
    if pattern == path {
        return true;
    }
    let mut regex = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            for next in chars.by_ref() {
                if next == '}' {
                    break;
                }
            }
            regex.push_str("[^/]+");
        } else {
            regex.push_str(regex::escape(ch.to_string().as_str()).as_str());
        }
    }
    regex.push('$');
    Regex::new(regex.as_str())
        .map(|regex| regex.is_match(path))
        .unwrap_or(false)
}

fn deserialize_url_rewrite_rules<'de, D>(deserializer: D) -> Result<Vec<UrlRewriteRule>, D::Error>
where
    D: Deserializer<'de>,
{
    parse_rule_strings(deserializer)?
        .into_iter()
        .map(|rule| parse_url_rewrite_rule(rule.as_str()).map_err(D::Error::custom))
        .collect()
}

fn deserialize_method_rewrite_rules<'de, D>(
    deserializer: D,
) -> Result<Vec<MethodRewriteRule>, D::Error>
where
    D: Deserializer<'de>,
{
    parse_rule_strings(deserializer)?
        .into_iter()
        .map(|rule| parse_method_rewrite_rule(rule.as_str()).map_err(D::Error::custom))
        .collect()
}

fn parse_rule_strings<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct RuleStringsVisitor;

    impl<'de> Visitor<'de> for RuleStringsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a rule string, JSON/YAML string list, or list of strings")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(Vec::new());
            }
            if value.starts_with('[') {
                return serde_yaml::from_str::<Vec<String>>(value).map_err(E::custom);
            }
            if value.contains('\n') {
                return Ok(value
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_string)
                    .collect());
            }
            Ok(vec![value.to_string()])
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(RuleStringsVisitor)
}

fn parse_url_rewrite_rule(value: &str) -> Result<UrlRewriteRule, String> {
    let (pattern, replace) = split_rule(value, 2)?;
    Ok(UrlRewriteRule { pattern, replace })
}

fn parse_method_rewrite_rule(value: &str) -> Result<MethodRewriteRule, String> {
    let parts = value.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!("invalid method rewrite rule `{value}`"));
    }
    Ok(MethodRewriteRule {
        request_path: parts[0].to_string(),
        source_method: parts[1].to_ascii_uppercase(),
        target_method: parts[2].to_ascii_uppercase(),
    })
}

fn split_rule(value: &str, parts: usize) -> Result<(String, String), String> {
    let mut iter = value.splitn(parts, char::is_whitespace);
    let first = iter
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("invalid rewrite rule `{value}`"))?;
    let second = iter
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("invalid rewrite rule `{value}`"))?;
    Ok((first.to_string(), second.to_string()))
}

fn deserialize_endpoint_rule_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Vec<QueryHeaderRewriteRule>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct EndpointRuleMapVisitor;

    impl<'de> Visitor<'de> for EndpointRuleMapVisitor {
        type Value = BTreeMap<String, Vec<QueryHeaderRewriteRule>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a map or JSON/YAML string map")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(BTreeMap::new());
            }
            let yaml = serde_yaml::from_str::<YamlValue>(value).map_err(E::custom)?;
            parse_endpoint_rule_map(yaml).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) =
                map.next_entry::<String, Vec<QueryHeaderRewriteRule>>()?
            {
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(EndpointRuleMapVisitor)
}

fn parse_endpoint_rule_map(
    value: YamlValue,
) -> Result<BTreeMap<String, Vec<QueryHeaderRewriteRule>>, String> {
    match value {
        YamlValue::Mapping(map) => {
            let mut values = BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| "router rewrite-rule map key must be a string".to_string())?
                    .to_string();
                let rules = serde_yaml::from_value::<Vec<QueryHeaderRewriteRule>>(value)
                    .map_err(|e| e.to_string())?;
                values.insert(key, rules);
            }
            Ok(values)
        }
        YamlValue::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected endpoint rewrite-rule map, got {other:?}")),
    }
}

fn deserialize_service_targets<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ServiceTargetsVisitor;

    impl<'de> Visitor<'de> for ServiceTargetsVisitor {
        type Value = BTreeMap<String, Vec<String>>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a service target map")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(BTreeMap::new());
            }
            let yaml = serde_yaml::from_str::<YamlValue>(value).map_err(E::custom)?;
            parse_service_target_map(yaml).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, YamlValue>()? {
                values.insert(
                    key,
                    parse_service_target_value(value).map_err(A::Error::custom)?,
                );
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(ServiceTargetsVisitor)
}

fn parse_service_target_map(value: YamlValue) -> Result<BTreeMap<String, Vec<String>>, String> {
    match value {
        YamlValue::Mapping(map) => {
            let mut values = BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| "serviceTargets key must be a string".to_string())?
                    .to_string();
                values.insert(key, parse_service_target_value(value)?);
            }
            Ok(values)
        }
        YamlValue::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected serviceTargets map, got {other:?}")),
    }
}

fn parse_service_target_value(value: YamlValue) -> Result<Vec<String>, String> {
    match value {
        YamlValue::String(value) => Ok(parse_string_list(value.as_str())),
        YamlValue::Sequence(values) => values
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "serviceTargets values must be strings".to_string())
            })
            .collect(),
        YamlValue::Null => Ok(Vec::new()),
        other => Err(format!("unsupported serviceTargets value: {other:?}")),
    }
}

fn host_for_authority(host: Host<&str>) -> String {
    match host {
        Host::Domain(domain) => domain.to_string(),
        Host::Ipv4(address) => address.to_string(),
        Host::Ipv6(address) => format!("[{address}]"),
    }
}

fn normalize_path_prefix(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() {
        String::new()
    } else {
        path.to_string()
    }
}

fn default_true() -> bool {
    true
}

fn default_max_request_time() -> u64 {
    1000
}

fn default_connections_per_thread() -> usize {
    10
}

fn default_soft_max_connections_per_thread() -> usize {
    5
}

fn default_max_connection_retries() -> usize {
    3
}

fn default_metrics_name() -> String {
    "router-response".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig, ServiceIdentity,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn runtime_config(config_dir: &TempDir) -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: config_dir.path().join("external"),
            resolved_values: HashMap::new(),
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
        }
    }

    #[test]
    fn router_config_accepts_java_rule_shapes_and_service_targets() {
        let config: RouterConfig = serde_yaml::from_str(
            r#"
hostWhitelist: '["api\\.example\\.com"]'
serviceIdQueryParameter: true
urlRewriteRules:
  - /v1/listings/(.*)$ /listing.html?listing=$1
methodRewriteRules: '["/v1/pets/{petId} POST PATCH"]'
queryParamRewriteRules: '{"/v1/pets/{petId}":[{"oldK":"old","newK":"new"}]}'
headerRewriteRules:
  /v1/pets/{petId}:
    - oldK: x-old
      newK: x-new
serviceTargets:
  com.networknt.petstore-1.0.0: https://api.example.com/base
"#,
        )
        .expect("parse router config");

        assert!(config.service_id_query_parameter);
        assert_eq!(
            config.url_rewrite_rules[0].replace,
            "/listing.html?listing=$1"
        );
        assert_eq!(config.method_rewrite_rules[0].target_method, "PATCH");
        assert_eq!(
            config.query_param_rewrite_rules["/v1/pets/{petId}"][0]
                .new_k
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            config.service_targets["com.networknt.petstore-1.0.0"][0],
            "https://api.example.com/base"
        );
    }

    #[test]
    fn router_loads_static_service_targets() {
        let config_dir = TempDir::new().expect("config temp dir");
        std::fs::write(
            config_dir.path().join(ROUTER_FILE),
            r#"
serviceTargets:
  com.networknt.petstore-1.0.0:
    - https://api.example.com/base
"#,
        )
        .expect("write router config");
        let runtime = runtime_config(&config_dir);

        let route = load_router_route(&runtime, true)
            .expect("load router")
            .expect("router route");

        let target = &route.service_targets["com.networknt.petstore-1.0.0"][0];
        assert_eq!(target.address, "api.example.com:443");
        assert_eq!(target.path_prefix, "/base");
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == ROUTER_MODULE_ID && entry.active)
        );
    }

    #[test]
    fn router_rewrites_path_query_and_method() {
        let route = RouterRoute {
            config: serde_yaml::from_str(
                r#"
urlRewriteRules:
  - /v1/listings/(.*)$ /listing.html?listing=$1
methodRewriteRules:
  - /v1/listings/{id} POST PUT
queryParamRewriteRules:
  /v1/listings/{id}:
    - oldK: old
      newK: new
"#,
            )
            .expect("parse router config"),
            service_targets: BTreeMap::new(),
        };
        let decision = RouterDecision {
            target: parse_router_target("https://api.example.com/base").expect("target"),
            remove_service_id_query: true,
        };
        let mut request = RequestHeader::build(
            "POST",
            b"/v1/listings/123?service_id=svc&old=value",
            Some(8),
        )
        .expect("request");

        apply_router_upstream_request(&mut request, &route, &decision, "/v1/listings/{id}")
            .expect("rewrite request");

        assert_eq!(request.method.as_str(), "PUT");
        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/base/listing.html?listing=123&new=value"
        );
    }

    #[test]
    fn host_whitelist_uses_regex_against_host() {
        let config = RouterConfig {
            host_whitelist: vec![r"api\d+\.example\.com".to_string()],
            ..RouterConfig::default()
        };

        assert!(host_is_allowed(&config, "https://api1.example.com/v1"));
        assert!(!host_is_allowed(&config, "https://evil.example.com/v1"));
        assert!(!host_is_allowed(
            &config,
            "https://xapi1.example.com.evil/v1"
        ));
    }
}
