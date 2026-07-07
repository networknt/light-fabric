use crate::access_control::{AccessControlRuntime, AccessDecision};
use crate::config_util::deserialize_optional_u64;
use crate::direct_registry::direct_registry_target;
use crate::proxy::ProxyTarget;
use crate::security::AuthPrincipal;
use async_trait::async_trait;
use light_runtime::{
    DirectRegistryConfig, DiscoveryNode, DiscoverySnapshot, DiscoverySubscription, ModuleKind,
    PortalRegistryClient, RuntimeConfig, RuntimeError,
};
use pingora::http::RequestHeader;
use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use serde_yaml::Value as YamlValue;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::form_urlencoded;

pub const WEBSOCKET_ROUTER_FILE: &str = "websocket-router.yml";
pub const WEBSOCKET_ROUTER_LEGACY_FILE: &str = "websocket-router.yaml";
pub const WEBSOCKET_ROUTER_MODULE_ID: &str = "light-pingora/websocket-router";
pub const WEBSOCKET_ROUTER_CONFIG_NAME: &str = "websocket-router";

const DEFAULT_PROTOCOL: &str = "http";
const DEFAULT_IDLE_TIMEOUT_MS: u64 = 3_600_000;
const SERVICE_ID_HEADERS: [&str; 3] = ["Service-Id", "service_id", "serviceId"];
const SERVICE_ID_QUERY_PARAMS: [&str; 2] = ["service_id", "serviceId"];
const ENV_TAG_QUERY_PARAMS: [&str; 2] = ["env_tag", "envTag"];
const PROTOCOL_QUERY_PARAM: &str = "protocol";
const ROUTER_QUERY_PARAMS: [&str; 5] = ["protocol", "service_id", "serviceId", "env_tag", "envTag"];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketRouterConfig {
    pub default_protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_env_tag: Option<String>,
    pub path_prefix_service: BTreeMap<String, WebSocketServiceTarget>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub preserve_routing_headers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_connection_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_active_connections: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_upgrade_requests_per_second: Option<usize>,
}

impl Default for WebSocketRouterConfig {
    fn default() -> Self {
        Self {
            default_protocol: DEFAULT_PROTOCOL.to_string(),
            default_env_tag: None,
            path_prefix_service: BTreeMap::new(),
            preserve_routing_headers: false,
            idle_timeout_ms: Some(DEFAULT_IDLE_TIMEOUT_MS),
            max_connection_duration_ms: None,
            max_active_connections: None,
            max_upgrade_requests_per_second: None,
        }
    }
}

impl<'de> Deserialize<'de> for WebSocketRouterConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawConfig {
            #[serde(default = "default_protocol")]
            default_protocol: String,
            #[serde(default)]
            default_env_tag: Option<String>,
            #[serde(default, deserialize_with = "deserialize_path_prefix_service")]
            path_prefix_service: BTreeMap<String, RawWebSocketServiceTarget>,
            #[serde(default)]
            preserve_routing_headers: bool,
            #[serde(
                default = "default_idle_timeout_ms",
                deserialize_with = "deserialize_optional_u64"
            )]
            idle_timeout_ms: Option<u64>,
            #[serde(default, deserialize_with = "deserialize_optional_u64")]
            max_connection_duration_ms: Option<u64>,
            #[serde(default, deserialize_with = "deserialize_optional_u64")]
            max_active_connections: Option<u64>,
            #[serde(default, deserialize_with = "deserialize_optional_u64")]
            max_upgrade_requests_per_second: Option<u64>,
        }

        let raw = RawConfig::deserialize(deserializer)?;
        let default_protocol =
            normalize_protocol(raw.default_protocol.as_str()).map_err(D::Error::custom)?;
        let default_env_tag = normalize_optional(raw.default_env_tag);
        let mut path_prefix_service = BTreeMap::new();

        for (prefix, target) in raw.path_prefix_service {
            let prefix = normalize_prefix(prefix.as_str()).map_err(D::Error::custom)?;
            let target = target
                .normalize(default_protocol.as_str(), default_env_tag.as_deref())
                .map_err(D::Error::custom)?;
            if path_prefix_service.insert(prefix.clone(), target).is_some() {
                return Err(D::Error::custom(format!(
                    "duplicate websocket-router pathPrefixService prefix `{prefix}`"
                )));
            }
        }

        Ok(Self {
            default_protocol,
            default_env_tag,
            path_prefix_service,
            preserve_routing_headers: raw.preserve_routing_headers,
            idle_timeout_ms: normalize_optional_millis("idleTimeoutMs", raw.idle_timeout_ms)
                .map_err(D::Error::custom)?,
            max_connection_duration_ms: normalize_optional_millis(
                "maxConnectionDurationMs",
                raw.max_connection_duration_ms,
            )
            .map_err(D::Error::custom)?,
            max_active_connections: normalize_optional_usize(
                "maxActiveConnections",
                raw.max_active_connections,
            )
            .map_err(D::Error::custom)?,
            max_upgrade_requests_per_second: normalize_optional_usize(
                "maxUpgradeRequestsPerSecond",
                raw.max_upgrade_requests_per_second,
            )
            .map_err(D::Error::custom)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketServiceTarget {
    pub service_id: String,
    pub protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env_tag: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebSocketRouteSource {
    Header,
    Query,
    PathPrefix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSocketRouteDecision {
    pub service_id: String,
    pub protocol: String,
    pub env_tag: Option<String>,
    pub upstream_path_and_query: String,
    pub source: WebSocketRouteSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebSocketRouteError {
    MissingTarget,
    InvalidProtocol(String),
    DiscoveryUnavailable(String),
    DiscoveryFailed(String),
    NoUsableEndpoint(String),
    UpgradeRateExceeded(usize),
    TooManyActiveConnections(usize),
}

impl fmt::Display for WebSocketRouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTarget => f.write_str("websocket-router target is not configured"),
            Self::InvalidProtocol(protocol) => {
                write!(f, "unsupported websocket-router protocol `{protocol}`")
            }
            Self::DiscoveryUnavailable(service_id) => write!(
                f,
                "websocket-router discovery is not available for `{service_id}`"
            ),
            Self::DiscoveryFailed(message) => {
                write!(f, "websocket-router discovery lookup failed: {message}")
            }
            Self::NoUsableEndpoint(service_id) => write!(
                f,
                "websocket-router service `{service_id}` has no usable endpoints"
            ),
            Self::UpgradeRateExceeded(limit) => write!(
                f,
                "websocket-router upgrade request rate limit exceeded: {limit}/second"
            ),
            Self::TooManyActiveConnections(limit) => write!(
                f,
                "websocket-router active connection limit exceeded: {limit}"
            ),
        }
    }
}

impl std::error::Error for WebSocketRouteError {}

#[derive(Clone)]
pub struct WebSocketRouterRuntime {
    config: WebSocketRouterConfig,
    discovery: Option<Arc<dyn WebSocketDiscoveryResolver>>,
    direct_registry: DirectRegistryConfig,
    policy: Option<Arc<AccessControlRuntime>>,
    state: Arc<WebSocketRuntimeState>,
}

impl fmt::Debug for WebSocketRouterRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketRouterRuntime")
            .field("path_prefix_count", &self.config.path_prefix_service.len())
            .field("discovery", &self.discovery.is_some())
            .field(
                "direct_registry_entries",
                &self.direct_registry.direct_urls.len(),
            )
            .field("policy", &self.policy.is_some())
            .field(
                "active_connections",
                &self.state.active_connections.load(Ordering::Relaxed),
            )
            .finish()
    }
}

struct WebSocketRuntimeState {
    active_connections: AtomicUsize,
    upgrade_window: Mutex<UpgradeRateWindow>,
}

impl Default for WebSocketRuntimeState {
    fn default() -> Self {
        Self {
            active_connections: AtomicUsize::new(0),
            upgrade_window: Mutex::new(UpgradeRateWindow::default()),
        }
    }
}

#[derive(Default)]
struct UpgradeRateWindow {
    second: u64,
    count: usize,
}

pub struct WebSocketConnectionPermit {
    state: Arc<WebSocketRuntimeState>,
}

impl fmt::Debug for WebSocketConnectionPermit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebSocketConnectionPermit").finish()
    }
}

impl Drop for WebSocketConnectionPermit {
    fn drop(&mut self) {
        self.state
            .active_connections
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            })
            .ok();
    }
}

#[async_trait]
pub trait WebSocketDiscoveryResolver: Send + Sync {
    async fn lookup_discovery(
        &self,
        subscription: DiscoverySubscription,
    ) -> Result<DiscoverySnapshot, String>;
}

#[async_trait]
impl WebSocketDiscoveryResolver for PortalRegistryClient {
    async fn lookup_discovery(
        &self,
        subscription: DiscoverySubscription,
    ) -> Result<DiscoverySnapshot, String> {
        PortalRegistryClient::lookup_discovery(self, subscription)
            .await
            .map_err(|error| error.to_string())
    }
}

impl WebSocketRouterRuntime {
    pub fn new(config: WebSocketRouterConfig) -> Result<Self, RuntimeError> {
        Self::new_with_discovery(config, None)
    }

    pub fn new_with_discovery(
        config: WebSocketRouterConfig,
        discovery: Option<Arc<dyn WebSocketDiscoveryResolver>>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_policy_and_direct_registry(
            config,
            discovery,
            None,
            DirectRegistryConfig::default(),
        )
    }

    pub fn new_with_policy(
        config: WebSocketRouterConfig,
        policy: Option<Arc<AccessControlRuntime>>,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_policy_and_direct_registry(
            config,
            None,
            policy,
            DirectRegistryConfig::default(),
        )
    }

    pub fn new_with_discovery_and_direct_registry(
        config: WebSocketRouterConfig,
        discovery: Option<Arc<dyn WebSocketDiscoveryResolver>>,
        direct_registry: DirectRegistryConfig,
    ) -> Result<Self, RuntimeError> {
        Self::new_with_discovery_policy_and_direct_registry(
            config,
            discovery,
            None,
            direct_registry,
        )
    }

    fn new_with_discovery_policy_and_direct_registry(
        config: WebSocketRouterConfig,
        discovery: Option<Arc<dyn WebSocketDiscoveryResolver>>,
        policy: Option<Arc<AccessControlRuntime>>,
        direct_registry: DirectRegistryConfig,
    ) -> Result<Self, RuntimeError> {
        validate_config(&config)?;
        Ok(Self {
            config,
            discovery,
            direct_registry,
            policy,
            state: Arc::new(WebSocketRuntimeState::default()),
        })
    }

    pub fn config(&self) -> &WebSocketRouterConfig {
        &self.config
    }

    pub fn idle_timeout(&self) -> Option<Duration> {
        self.config.idle_timeout_ms.map(Duration::from_millis)
    }

    pub fn max_connection_duration(&self) -> Option<Duration> {
        self.config
            .max_connection_duration_ms
            .map(Duration::from_millis)
    }

    pub fn active_connection_count(&self) -> usize {
        self.state.active_connections.load(Ordering::Acquire)
    }

    pub fn preserve_state_from(&mut self, previous: &Self) {
        self.state = Arc::clone(&previous.state);
    }

    pub fn check_upgrade_rate(&self) -> Result<(), WebSocketRouteError> {
        let Some(limit) = self.config.max_upgrade_requests_per_second else {
            return Ok(());
        };
        let now = current_epoch_second();
        let mut window = self
            .state
            .upgrade_window
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if window.second != now {
            window.second = now;
            window.count = 0;
        }
        if window.count >= limit {
            return Err(WebSocketRouteError::UpgradeRateExceeded(limit));
        }
        window.count += 1;
        Ok(())
    }

    pub fn acquire_connection(&self) -> Result<WebSocketConnectionPermit, WebSocketRouteError> {
        if let Some(limit) = self.config.max_active_connections {
            loop {
                let current = self.state.active_connections.load(Ordering::Acquire);
                if current >= limit {
                    return Err(WebSocketRouteError::TooManyActiveConnections(limit));
                }
                if self
                    .state
                    .active_connections
                    .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    break;
                }
            }
        } else {
            self.state.active_connections.fetch_add(1, Ordering::AcqRel);
        }
        Ok(WebSocketConnectionPermit {
            state: Arc::clone(&self.state),
        })
    }

    pub fn try_accept_upgrade(&self) -> Result<WebSocketConnectionPermit, WebSocketRouteError> {
        self.check_upgrade_rate()?;
        self.acquire_connection()
    }

    pub async fn authorize(
        &self,
        decision: &WebSocketRouteDecision,
        endpoint: &str,
        headers: &[(String, String)],
        auth: Option<&AuthPrincipal>,
        correlation_id: Option<&str>,
    ) -> AccessDecision {
        let Some(policy) = self.policy.as_ref() else {
            return AccessDecision::Allowed;
        };
        let arguments = json!({
            "serviceId": decision.service_id.as_str(),
            "protocol": decision.protocol.as_str(),
            "envTag": decision.env_tag.as_deref(),
            "upstreamPathAndQuery": decision.upstream_path_and_query.as_str(),
            "source": websocket_route_source(decision.source),
        });
        policy
            .authorize_tool(
                "websocket",
                endpoint,
                headers,
                auth,
                &arguments,
                correlation_id,
            )
            .await
    }

    pub fn resolve<I, K, V>(
        &self,
        path: &str,
        query: Option<&str>,
        headers: I,
    ) -> Result<WebSocketRouteDecision, WebSocketRouteError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<str>,
        V: AsRef<str>,
    {
        let headers = headers
            .into_iter()
            .map(|(name, value)| (name.as_ref().to_string(), value.as_ref().to_string()))
            .collect::<Vec<_>>();
        let (request_path, path_query) = split_path_query(path);
        let query = query.or(path_query);
        let mut protocol = self.config.default_protocol.clone();
        let mut env_tag = self.config.default_env_tag.clone();

        let (service_id, source) = if let Some(service_id) =
            first_non_blank_header(&headers, &SERVICE_ID_HEADERS)
        {
            (service_id, WebSocketRouteSource::Header)
        } else if let Some(service_id) = first_non_blank_query(query, &SERVICE_ID_QUERY_PARAMS) {
            (service_id, WebSocketRouteSource::Query)
        } else if let Some(target) =
            best_path_prefix(&self.config.path_prefix_service, request_path)
        {
            protocol = target.protocol.clone();
            env_tag = target.env_tag.clone();
            (target.service_id.clone(), WebSocketRouteSource::PathPrefix)
        } else {
            return Err(WebSocketRouteError::MissingTarget);
        };

        if let Some(requested_protocol) = first_non_blank_query(query, &[PROTOCOL_QUERY_PARAM]) {
            protocol = normalize_protocol(requested_protocol.as_str())
                .map_err(|_| WebSocketRouteError::InvalidProtocol(requested_protocol))?;
        }
        if let Some(requested_env_tag) = first_non_blank_query(query, &ENV_TAG_QUERY_PARAMS) {
            env_tag = Some(requested_env_tag);
        }

        Ok(WebSocketRouteDecision {
            service_id,
            protocol,
            env_tag,
            upstream_path_and_query: clean_path_and_query(request_path, query),
            source,
        })
    }

    pub async fn select_target(
        &self,
        decision: &WebSocketRouteDecision,
        index: usize,
    ) -> Result<ProxyTarget, WebSocketRouteError> {
        let mut discovery_unavailable = false;
        let mut discovery_error = None;
        if let Some(discovery) = self.discovery.as_ref() {
            match discovery
                .lookup_discovery(DiscoverySubscription {
                    service_id: decision.service_id.clone(),
                    env_tag: decision.env_tag.clone(),
                    protocol: Some(decision.protocol.clone()),
                })
                .await
            {
                Ok(snapshot) => {
                    let targets = snapshot
                        .nodes
                        .iter()
                        .filter(|node| {
                            node.protocol
                                .eq_ignore_ascii_case(decision.protocol.as_str())
                        })
                        .filter_map(discovery_node_to_target)
                        .collect::<Vec<_>>();
                    if !targets.is_empty() {
                        return Ok(targets[index % targets.len()].clone());
                    }
                }
                Err(error) => {
                    discovery_error = Some(error);
                }
            }
        } else {
            discovery_unavailable = true;
        }

        match direct_registry_target(
            &self.direct_registry,
            decision.service_id.as_str(),
            decision.env_tag.as_deref(),
            Some(decision.protocol.as_str()),
        ) {
            Ok(Some(target)) => return Ok(target),
            Ok(None) => {}
            Err(error) => return Err(WebSocketRouteError::DiscoveryFailed(error.to_string())),
        }

        if discovery_unavailable {
            return Err(WebSocketRouteError::DiscoveryUnavailable(
                decision.service_id.clone(),
            ));
        }
        if let Some(error) = discovery_error {
            return Err(WebSocketRouteError::DiscoveryFailed(error));
        }
        Err(WebSocketRouteError::NoUsableEndpoint(
            decision.service_id.clone(),
        ))
    }
}

pub fn load_websocket_router_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<WebSocketRouterRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match load_websocket_router_config(runtime_config)? {
        Some(config) => config,
        None => WebSocketRouterConfig::default(),
    };
    runtime_config.module_registry.register_loaded_config(
        WEBSOCKET_ROUTER_MODULE_ID,
        WEBSOCKET_ROUTER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        true,
        None,
        true,
    )?;
    Ok(Some(
        WebSocketRouterRuntime::new_with_discovery_and_direct_registry(
            config,
            discovery_resolver(runtime_config.registry_client.clone()),
            runtime_config.direct_registry.clone(),
        )?,
    ))
}

pub fn apply_websocket_upstream_request(
    upstream_request: &mut RequestHeader,
    decision: &WebSocketRouteDecision,
    preserve_routing_headers: bool,
) -> pingora::Result<()> {
    let uri = decision.upstream_path_and_query.parse().map_err(|error| {
        pingora::Error::because(
            pingora::ErrorType::InvalidHTTPHeader,
            format!(
                "invalid websocket upstream URI `{}`",
                decision.upstream_path_and_query
            ),
            error,
        )
    })?;
    upstream_request.set_uri(uri);
    if !preserve_routing_headers {
        for header in SERVICE_ID_HEADERS {
            upstream_request.remove_header(header);
        }
    }
    Ok(())
}

fn load_websocket_router_config(
    runtime_config: &RuntimeConfig,
) -> Result<Option<WebSocketRouterConfig>, RuntimeError> {
    for file in [WEBSOCKET_ROUTER_FILE, WEBSOCKET_ROUTER_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<WebSocketRouterConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(config)),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn discovery_resolver(
    client: Option<Arc<PortalRegistryClient>>,
) -> Option<Arc<dyn WebSocketDiscoveryResolver>> {
    client.map(|client| client as Arc<dyn WebSocketDiscoveryResolver>)
}

fn discovery_node_to_target(node: &DiscoveryNode) -> Option<ProxyTarget> {
    if node.port == 0 || !node.connected {
        return None;
    }
    let protocol = node.protocol.to_ascii_lowercase();
    let tls = match protocol.as_str() {
        "http" => false,
        "https" => true,
        _ => return None,
    };
    let host = host_for_discovery(&node.address);
    let address = format!("{host}:{}", node.port);
    let sni = if tls {
        node.address.clone()
    } else {
        String::new()
    };
    Some(ProxyTarget {
        address: address.clone(),
        tls,
        sni,
        host_header: address,
        path_prefix: String::new(),
    })
}

fn host_for_discovery(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RawWebSocketServiceTarget {
    ServiceId(String),
    Target {
        service_id: String,
        protocol: Option<String>,
        env_tag: Option<String>,
    },
}

impl RawWebSocketServiceTarget {
    fn normalize(
        self,
        default_protocol: &str,
        default_env_tag: Option<&str>,
    ) -> Result<WebSocketServiceTarget, String> {
        match self {
            Self::ServiceId(service_id) => {
                let service_id = normalize_required("serviceId", service_id.as_str())?;
                Ok(WebSocketServiceTarget {
                    service_id,
                    protocol: default_protocol.to_string(),
                    env_tag: default_env_tag.map(str::to_string),
                })
            }
            Self::Target {
                service_id,
                protocol,
                env_tag,
            } => {
                let service_id = normalize_required("serviceId", service_id.as_str())?;
                let protocol = protocol
                    .as_deref()
                    .map(normalize_protocol)
                    .transpose()?
                    .unwrap_or_else(|| default_protocol.to_string());
                let env_tag =
                    normalize_optional(env_tag).or_else(|| default_env_tag.map(str::to_string));
                Ok(WebSocketServiceTarget {
                    service_id,
                    protocol,
                    env_tag,
                })
            }
        }
    }
}

fn deserialize_path_prefix_service<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, RawWebSocketServiceTarget>, D::Error>
where
    D: Deserializer<'de>,
{
    struct PathPrefixServiceVisitor;

    impl<'de> Visitor<'de> for PathPrefixServiceVisitor {
        type Value = BTreeMap<String, RawWebSocketServiceTarget>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a pathPrefixService map, JSON/YAML map string, or key=value list")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            parse_path_prefix_service_str(value).map_err(E::custom)
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
                    parse_path_prefix_service_value(value).map_err(A::Error::custom)?,
                );
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(PathPrefixServiceVisitor)
}

fn parse_path_prefix_service_str(
    value: &str,
) -> Result<BTreeMap<String, RawWebSocketServiceTarget>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(BTreeMap::new());
    }
    if value.starts_with('{') {
        let value = serde_yaml::from_str::<YamlValue>(value).map_err(|error| error.to_string())?;
        return parse_path_prefix_service_map(value);
    }

    let mut values = BTreeMap::new();
    for entry in value.split('&') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (prefix, service_id) = entry
            .split_once('=')
            .ok_or_else(|| format!("invalid pathPrefixService entry `{entry}`"))?;
        let prefix = prefix.trim();
        if prefix.is_empty() {
            return Err("pathPrefixService prefix must not be empty".to_string());
        }
        values.insert(
            prefix.to_string(),
            RawWebSocketServiceTarget::ServiceId(service_id.trim().to_string()),
        );
    }
    Ok(values)
}

fn parse_path_prefix_service_map(
    value: YamlValue,
) -> Result<BTreeMap<String, RawWebSocketServiceTarget>, String> {
    match value {
        YamlValue::Mapping(map) => {
            let mut values = BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| "pathPrefixService key must be a string".to_string())?
                    .to_string();
                values.insert(key, parse_path_prefix_service_value(value)?);
            }
            Ok(values)
        }
        YamlValue::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected pathPrefixService map, got {other:?}")),
    }
}

fn parse_path_prefix_service_value(value: YamlValue) -> Result<RawWebSocketServiceTarget, String> {
    match value {
        YamlValue::String(value) => Ok(RawWebSocketServiceTarget::ServiceId(value)),
        YamlValue::Mapping(map) => {
            let mut service_id = None;
            let mut protocol = None;
            let mut env_tag = None;
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| "pathPrefixService entry key must be a string".to_string())?;
                let value = match value {
                    YamlValue::Null => None,
                    YamlValue::String(value) => Some(value),
                    other => {
                        return Err(format!(
                            "pathPrefixService entry `{key}` must be a string, got {other:?}"
                        ));
                    }
                };
                match key {
                    "serviceId" | "service_id" => service_id = value,
                    "protocol" => protocol = value,
                    "envTag" | "env_tag" => env_tag = value,
                    _ => {}
                }
            }
            Ok(RawWebSocketServiceTarget::Target {
                service_id: service_id.unwrap_or_default(),
                protocol,
                env_tag,
            })
        }
        YamlValue::Null => Err("pathPrefixService entry must not be null".to_string()),
        other => Err(format!("unsupported pathPrefixService entry: {other:?}")),
    }
}

fn validate_config(config: &WebSocketRouterConfig) -> Result<(), RuntimeError> {
    normalize_protocol(config.default_protocol.as_str()).map_err(RuntimeError::Unsupported)?;
    for (prefix, target) in &config.path_prefix_service {
        normalize_prefix(prefix.as_str()).map_err(RuntimeError::Unsupported)?;
        normalize_required("serviceId", target.service_id.as_str())
            .map_err(RuntimeError::Unsupported)?;
        normalize_protocol(target.protocol.as_str()).map_err(RuntimeError::Unsupported)?;
    }
    Ok(())
}

fn normalize_optional_millis(_field: &str, value: Option<u64>) -> Result<Option<u64>, String> {
    Ok(value.filter(|value| *value > 0))
}

fn normalize_optional_usize(field: &str, value: Option<u64>) -> Result<Option<usize>, String> {
    let Some(value) = value.filter(|value| *value > 0) else {
        return Ok(None);
    };
    usize::try_from(value)
        .map(Some)
        .map_err(|_| format!("websocket-router {field} is outside usize range"))
}

fn default_protocol() -> String {
    DEFAULT_PROTOCOL.to_string()
}

fn default_idle_timeout_ms() -> Option<u64> {
    Some(DEFAULT_IDLE_TIMEOUT_MS)
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn normalize_protocol(protocol: &str) -> Result<String, String> {
    let protocol = protocol.trim().to_ascii_lowercase();
    match protocol.as_str() {
        "http" | "https" => Ok(protocol),
        _ => Err(format!(
            "websocket-router protocol must be `http` or `https`, got `{protocol}`"
        )),
    }
}

fn normalize_required(field: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("websocket-router {field} must not be empty"));
    }
    Ok(value.to_string())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_prefix(prefix: &str) -> Result<String, String> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Err("websocket-router pathPrefixService prefix must not be empty".to_string());
    }
    if prefix == "/" {
        return Ok("/".to_string());
    }
    let prefix = prefix.trim_end_matches('/');
    let prefix = if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        format!("/{prefix}")
    };
    Ok(prefix)
}

fn split_path_query(path: &str) -> (&str, Option<&str>) {
    path.split_once('?')
        .map_or((path, None), |(path, query)| (path, Some(query)))
}

fn clean_path_and_query(path: &str, query: Option<&str>) -> String {
    let Some(query) = strip_router_query(query) else {
        return path.to_string();
    };
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

fn strip_router_query(query: Option<&str>) -> Option<String> {
    let query = query?;
    let kept = query
        .split('&')
        .filter(|segment| {
            let name = segment.split_once('=').map_or(*segment, |(name, _)| name);
            !ROUTER_QUERY_PARAMS.contains(&name)
        })
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    Some(kept.join("&"))
}

fn first_non_blank_header(headers: &[(String, String)], names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
        {
            return Some(value.to_string());
        }
    }
    None
}

fn first_non_blank_query(query: Option<&str>, names: &[&str]) -> Option<String> {
    let query = query?;
    for name in names {
        if let Some(value) = form_urlencoded::parse(query.as_bytes())
            .find(|(candidate, value)| candidate == *name && !value.trim().is_empty())
            .map(|(_, value)| value.trim().to_string())
        {
            return Some(value);
        }
    }
    None
}

fn best_path_prefix<'a>(
    mapping: &'a BTreeMap<String, WebSocketServiceTarget>,
    request_path: &str,
) -> Option<&'a WebSocketServiceTarget> {
    let request_path = normalize_request_path(request_path);
    mapping
        .iter()
        .filter_map(|(prefix, target)| {
            path_matches_prefix(request_path.as_str(), prefix.as_str())
                .then_some((prefix.len(), target))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, target)| target)
}

fn normalize_request_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn current_epoch_second() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn websocket_route_source(source: WebSocketRouteSource) -> &'static str {
    match source {
        WebSocketRouteSource::Header => "header",
        WebSocketRouteSource::Query => "query",
        WebSocketRouteSource::PathPrefix => "pathPrefix",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, DiscoverySubscription, ModuleRegistry,
        PortalRegistryConfig, ServerConfig, ServiceIdentity,
    };
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn runtime_config(config_dir: &TempDir) -> RuntimeConfig {
        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: config_dir.path().join("external"),
            resolved_values: HashMap::new(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn config_accepts_yaml_object_path_prefix_service() {
        let config: WebSocketRouterConfig = serde_yaml::from_str(
            r#"
defaultProtocol: https
defaultEnvTag: dev
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
    envTag: sit
"#,
        )
        .expect("parse config");

        let target = &config.path_prefix_service["/chat"];
        assert_eq!(config.default_protocol, "https");
        assert_eq!(config.default_env_tag.as_deref(), Some("dev"));
        assert_eq!(target.service_id, "com.networknt.llmchat-1.0.0");
        assert_eq!(target.protocol, "http");
        assert_eq!(target.env_tag.as_deref(), Some("sit"));
    }

    #[test]
    fn config_accepts_string_service_id_entries() {
        let config: WebSocketRouterConfig = serde_yaml::from_str(
            r#"
defaultProtocol: https
defaultEnvTag: dev
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
"#,
        )
        .expect("parse config");

        let target = &config.path_prefix_service["/chat"];
        assert_eq!(target.service_id, "com.networknt.llmchat-1.0.0");
        assert_eq!(target.protocol, "https");
        assert_eq!(target.env_tag.as_deref(), Some("dev"));
    }

    #[test]
    fn config_accepts_json_string_path_prefix_service() {
        let config: WebSocketRouterConfig = serde_yaml::from_str(
            r#"
pathPrefixService: '{"/chat":{"serviceId":"com.networknt.llmchat-1.0.0","protocol":"http","envTag":"dev"}}'
"#,
        )
        .expect("parse config");

        let target = &config.path_prefix_service["/chat"];
        assert_eq!(target.service_id, "com.networknt.llmchat-1.0.0");
        assert_eq!(target.protocol, "http");
        assert_eq!(target.env_tag.as_deref(), Some("dev"));
    }

    #[test]
    fn config_defaults_idle_timeout_to_one_hour() {
        let config: WebSocketRouterConfig = serde_yaml::from_str("{}").expect("parse config");

        assert_eq!(config.idle_timeout_ms, Some(DEFAULT_IDLE_TIMEOUT_MS));
    }

    #[test]
    fn config_accepts_legacy_key_value_string_path_prefix_service() {
        let config: WebSocketRouterConfig = serde_yaml::from_str(
            r#"
defaultEnvTag: dev
pathPrefixService: /chat = com.networknt.llmchat-1.0.0 & /events=com.networknt.events-1.0.0
"#,
        )
        .expect("parse config");

        assert_eq!(
            config.path_prefix_service["/chat"].service_id,
            "com.networknt.llmchat-1.0.0"
        );
        assert_eq!(
            config.path_prefix_service["/events"].env_tag.as_deref(),
            Some("dev")
        );
    }

    #[test]
    fn config_accepts_production_controls_as_numbers_or_strings() {
        let config: WebSocketRouterConfig = serde_yaml::from_str(
            r#"
preserveRoutingHeaders: true
idleTimeoutMs: "30000"
maxConnectionDurationMs: 3600000
maxActiveConnections: "10"
maxUpgradeRequestsPerSecond: 5
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
"#,
        )
        .expect("parse config");

        assert!(config.preserve_routing_headers);
        assert_eq!(config.idle_timeout_ms, Some(30_000));
        assert_eq!(config.max_connection_duration_ms, Some(3_600_000));
        assert_eq!(config.max_active_connections, Some(10));
        assert_eq!(config.max_upgrade_requests_per_second, Some(5));
    }

    #[test]
    fn config_rejects_invalid_entries() {
        let error = serde_yaml::from_str::<WebSocketRouterConfig>(
            r#"
pathPrefixService:
  /chat:
    protocol: http
"#,
        )
        .expect_err("missing serviceId should fail");

        assert!(error.to_string().contains("serviceId"));

        let error = serde_yaml::from_str::<WebSocketRouterConfig>(
            r#"
defaultProtocol: ftp
"#,
        )
        .expect_err("invalid protocol should fail");

        assert!(error.to_string().contains("protocol"));
    }

    #[test]
    fn route_resolution_priority_is_header_query_then_path_prefix() {
        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
defaultProtocol: http
pathPrefixService:
  /chat: path-service
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");

        let decision = runtime
            .resolve(
                "/chat/room",
                Some("service_id=query-service"),
                [("service_id", "header-service")],
            )
            .expect("resolve header");
        assert_eq!(decision.service_id, "header-service");
        assert_eq!(decision.source, WebSocketRouteSource::Header);

        let decision = runtime
            .resolve(
                "/chat/room",
                Some("serviceId=query-service"),
                std::iter::empty::<(&str, &str)>(),
            )
            .expect("resolve query");
        assert_eq!(decision.service_id, "query-service");
        assert_eq!(decision.source, WebSocketRouteSource::Query);

        let decision = runtime
            .resolve("/chat/room", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve prefix");
        assert_eq!(decision.service_id, "path-service");
        assert_eq!(decision.source, WebSocketRouteSource::PathPrefix);
    }

    #[test]
    fn route_resolution_uses_longest_boundary_prefix() {
        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /: root-service
  /chat: chat-service
  /chat/private: private-service
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");

        let decision = runtime
            .resolve(
                "/chat/private/room",
                None,
                std::iter::empty::<(&str, &str)>(),
            )
            .expect("resolve private");
        assert_eq!(decision.service_id, "private-service");

        let decision = runtime
            .resolve("/chatty", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve root");
        assert_eq!(decision.service_id, "root-service");
    }

    #[test]
    fn query_overrides_protocol_and_env_tag_and_router_params_are_stripped() {
        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
defaultProtocol: http
pathPrefixService:
  /chat:
    serviceId: path-service
    envTag: dev
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");

        let decision = runtime
            .resolve(
                "/chat/room",
                Some("service_id=query-service&protocol=https&env_tag=prod&room=one"),
                std::iter::empty::<(&str, &str)>(),
            )
            .expect("resolve query");

        assert_eq!(decision.service_id, "query-service");
        assert_eq!(decision.protocol, "https");
        assert_eq!(decision.env_tag.as_deref(), Some("prod"));
        assert_eq!(decision.upstream_path_and_query, "/chat/room?room=one");
    }

    #[test]
    fn missing_target_and_bad_protocol_are_errors() {
        let runtime =
            WebSocketRouterRuntime::new(WebSocketRouterConfig::default()).expect("runtime");
        assert_eq!(
            runtime
                .resolve("/chat", None, std::iter::empty::<(&str, &str)>())
                .expect_err("missing target"),
            WebSocketRouteError::MissingTarget
        );

        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /chat: path-service
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");
        assert_eq!(
            runtime
                .resolve(
                    "/chat",
                    Some("protocol=ftp"),
                    std::iter::empty::<(&str, &str)>(),
                )
                .expect_err("bad protocol"),
            WebSocketRouteError::InvalidProtocol("ftp".to_string())
        );
    }

    #[test]
    fn production_controls_limit_upgrade_rate_and_active_connections() {
        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
maxActiveConnections: 1
maxUpgradeRequestsPerSecond: 1
pathPrefixService:
  /chat: chat-service
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");

        runtime.check_upgrade_rate().expect("first upgrade");
        assert_eq!(
            runtime.check_upgrade_rate().expect_err("second upgrade"),
            WebSocketRouteError::UpgradeRateExceeded(1)
        );

        let permit = runtime.acquire_connection().expect("first connection");
        assert_eq!(runtime.active_connection_count(), 1);
        assert_eq!(
            runtime.acquire_connection().expect_err("second connection"),
            WebSocketRouteError::TooManyActiveConnections(1)
        );
        drop(permit);
        assert_eq!(runtime.active_connection_count(), 0);
    }

    #[test]
    fn runtime_state_can_be_preserved_across_reload() {
        let runtime = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
maxActiveConnections: 5
pathPrefixService:
  /chat: chat-service
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");
        let permit = runtime.acquire_connection().expect("connection");
        let mut reloaded = WebSocketRouterRuntime::new(
            serde_yaml::from_str(
                r#"
maxActiveConnections: 10
pathPrefixService:
  /chat: chat-service-v2
"#,
            )
            .expect("parse config"),
        )
        .expect("runtime");

        reloaded.preserve_state_from(&runtime);

        assert_eq!(reloaded.active_connection_count(), 1);
        drop(permit);
        assert_eq!(reloaded.active_connection_count(), 0);
    }

    #[tokio::test]
    async fn authorize_delegates_to_access_control_policy() {
        let policy = Arc::new(crate::access_control::AccessControlRuntime::new(
            Some(crate::access_control::AccessControlConfig {
                enabled: true,
                access_rule_logic: "any".to_string(),
                default_deny: true,
                default_include: false,
                skip_path_prefixes: Vec::new(),
                ..crate::access_control::AccessControlConfig::default()
            }),
            crate::access_control::RuleFileConfig::default(),
        ));
        let runtime = WebSocketRouterRuntime::new_with_policy(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /chat: chat-service
"#,
            )
            .expect("parse config"),
            Some(policy),
        )
        .expect("runtime");
        let decision = runtime
            .resolve("/chat/room", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve");

        let decision = runtime.authorize(&decision, "/chat", &[], None, None).await;

        assert_eq!(
            decision,
            AccessDecision::Denied(
                "Access denied: no access control rule defined for /chat".into()
            )
        );
    }

    #[tokio::test]
    async fn select_target_uses_discovery_protocol_env_and_round_robin_index() {
        let discovery = Arc::new(FakeDiscovery::new(discovery_snapshot(
            "com.networknt.llmchat-1.0.0",
            Some("dev"),
        )));
        let runtime = WebSocketRouterRuntime::new_with_discovery(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: https
    envTag: dev
"#,
            )
            .expect("parse config"),
            Some(discovery.clone()),
        )
        .expect("runtime");
        let decision = runtime
            .resolve("/chat/room", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve");

        let target = runtime
            .select_target(&decision, 1)
            .await
            .expect("select target");

        assert_eq!(target.address, "api2.example.com:9443");
        assert!(target.tls);
        assert_eq!(target.sni, "api2.example.com");
        assert_eq!(
            discovery.lookups.lock().expect("lookup lock")[0],
            DiscoverySubscription {
                service_id: "com.networknt.llmchat-1.0.0".to_string(),
                env_tag: Some("dev".to_string()),
                protocol: Some("https".to_string()),
            }
        );
    }

    #[tokio::test]
    async fn select_target_uses_direct_registry_when_discovery_is_unavailable() {
        let runtime = WebSocketRouterRuntime::new_with_discovery_and_direct_registry(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /ctrl/mcp:
    serviceId: com.networknt.controller-1.0.0
    protocol: https
    envTag: dev
"#,
            )
            .expect("parse config"),
            None,
            DirectRegistryConfig {
                direct_urls: BTreeMap::from([(
                    "com.networknt.controller-1.0.0|dev".to_string(),
                    "https://controller:8438".to_string(),
                )]),
            },
        )
        .expect("runtime");
        let decision = runtime
            .resolve("/ctrl/mcp", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve");

        let target = runtime
            .select_target(&decision, 0)
            .await
            .expect("select target");

        assert_eq!(target.address, "controller:8438");
        assert!(target.tls);
    }

    #[tokio::test]
    async fn select_target_uses_discovery_before_direct_registry() {
        let resolver = Arc::new(FakeDiscovery::new(discovery_snapshot(
            "com.networknt.controller-1.0.0",
            Some("dev"),
        )));
        let discovery: Arc<dyn WebSocketDiscoveryResolver> = resolver.clone();
        let runtime = WebSocketRouterRuntime::new_with_discovery_and_direct_registry(
            serde_yaml::from_str(
                r#"
pathPrefixService:
  /ctrl/mcp:
    serviceId: com.networknt.controller-1.0.0
    protocol: https
    envTag: dev
"#,
            )
            .expect("parse config"),
            Some(discovery),
            DirectRegistryConfig {
                direct_urls: BTreeMap::from([(
                    "com.networknt.controller-1.0.0|dev".to_string(),
                    "https://controller:8438".to_string(),
                )]),
            },
        )
        .expect("runtime");
        let decision = runtime
            .resolve("/ctrl/mcp", None, std::iter::empty::<(&str, &str)>())
            .expect("resolve");

        let target = runtime
            .select_target(&decision, 0)
            .await
            .expect("select target");

        assert_eq!(target.address, "api1.example.com:9443");
        assert!(target.tls);
        assert_eq!(resolver.lookups.lock().expect("lookup lock").len(), 1);
    }

    #[test]
    fn apply_upstream_request_strips_routing_headers_and_preserves_handshake_headers() {
        let decision = WebSocketRouteDecision {
            service_id: "com.networknt.llmchat-1.0.0".to_string(),
            protocol: "http".to_string(),
            env_tag: None,
            upstream_path_and_query: "/chat/room?room=one".to_string(),
            source: WebSocketRouteSource::Query,
        };
        let mut request =
            RequestHeader::build("GET", b"/chat/room?service_id=svc&room=one", Some(8))
                .expect("request");
        request
            .insert_header("service_id", "com.networknt.llmchat-1.0.0")
            .expect("service header");
        request
            .insert_header("Sec-WebSocket-Protocol", "chat")
            .expect("subprotocol header");

        apply_websocket_upstream_request(&mut request, &decision, false).expect("apply request");

        assert_eq!(
            request.uri.path_and_query().unwrap().as_str(),
            "/chat/room?room=one"
        );
        assert!(!request.headers.contains_key("service_id"));
        assert_eq!(
            request.headers["sec-websocket-protocol"].to_str().unwrap(),
            "chat"
        );
    }

    #[test]
    fn apply_upstream_request_can_preserve_routing_headers() {
        let decision = WebSocketRouteDecision {
            service_id: "com.networknt.llmchat-1.0.0".to_string(),
            protocol: "http".to_string(),
            env_tag: None,
            upstream_path_and_query: "/chat/room".to_string(),
            source: WebSocketRouteSource::Header,
        };
        let mut request = RequestHeader::build("GET", b"/chat/room", Some(8)).expect("request");
        request
            .insert_header("service_id", "com.networknt.llmchat-1.0.0")
            .expect("service header");

        apply_websocket_upstream_request(&mut request, &decision, true).expect("apply request");

        assert_eq!(
            request.headers["service_id"].to_str().unwrap(),
            "com.networknt.llmchat-1.0.0"
        );
    }

    #[test]
    fn loader_accepts_legacy_yaml_file_and_registers_module() {
        let config_dir = TempDir::new().expect("config temp dir");
        std::fs::write(
            config_dir.path().join(WEBSOCKET_ROUTER_LEGACY_FILE),
            r#"
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
"#,
        )
        .expect("write config");
        let runtime = runtime_config(&config_dir);

        let router = load_websocket_router_runtime(&runtime, true)
            .expect("load runtime")
            .expect("router runtime");

        assert_eq!(
            router.config().path_prefix_service["/chat"].service_id,
            "com.networknt.llmchat-1.0.0"
        );
        assert!(
            runtime
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == WEBSOCKET_ROUTER_MODULE_ID && entry.active)
        );
    }

    fn discovery_snapshot(service_id: &str, env_tag: Option<&str>) -> DiscoverySnapshot {
        serde_json::from_value(serde_json::json!({
            "serviceId": service_id,
            "envTag": env_tag,
            "protocol": "https",
            "nodes": [
                {
                    "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40",
                    "serviceId": service_id,
                    "envTag": env_tag,
                    "environment": "dev",
                    "version": "1.0.0",
                    "protocol": "https",
                    "address": "api1.example.com",
                    "port": 9443,
                    "tags": {},
                    "connectedAt": "2026-01-01T00:00:00Z",
                    "lastSeenAt": "2026-01-01T00:00:01Z",
                    "connected": true
                },
                {
                    "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f41",
                    "serviceId": service_id,
                    "envTag": env_tag,
                    "environment": "dev",
                    "version": "1.0.0",
                    "protocol": "https",
                    "address": "api2.example.com",
                    "port": 9443,
                    "tags": {},
                    "connectedAt": "2026-01-01T00:00:00Z",
                    "lastSeenAt": "2026-01-01T00:00:01Z",
                    "connected": true
                }
            ]
        }))
        .expect("discovery snapshot")
    }

    struct FakeDiscovery {
        snapshot: DiscoverySnapshot,
        lookups: Mutex<Vec<DiscoverySubscription>>,
    }

    impl FakeDiscovery {
        fn new(snapshot: DiscoverySnapshot) -> Self {
            Self {
                snapshot,
                lookups: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl WebSocketDiscoveryResolver for FakeDiscovery {
        async fn lookup_discovery(
            &self,
            subscription: DiscoverySubscription,
        ) -> Result<DiscoverySnapshot, String> {
            self.lookups.lock().expect("lookup lock").push(subscription);
            Ok(self.snapshot.clone())
        }
    }
}
