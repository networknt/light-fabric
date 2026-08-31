use crate::proxy::ProxyTarget;
use crate::router::{RouterRoute, select_registered_service_target};
use crate::security::HandlerRejection;
use a2a_core::{AuthorizedInvocation, Direction, request_digest, sign_authorized_invocation};
use chrono::{Duration, Utc};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

pub const A2A_ROUTER_FILE: &str = "a2a-router.yml";
pub const A2A_ROUTER_MODULE_ID: &str = "light-pingora/a2a-router";
pub const A2A_ROUTER_CONFIG_NAME: &str = "a2a-router";

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aRouterConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_max_request_body_bytes")]
    pub maximum_request_body_bytes: usize,
    #[serde(default = "default_max_buffered_request_bytes")]
    pub maximum_buffered_request_bytes: usize,
    pub authorization_context_key_file: PathBuf,
    #[serde(default)]
    pub routes: Vec<A2aRouteConfig>,
}

const fn default_max_request_body_bytes() -> usize {
    1_048_576
}

const fn default_max_buffered_request_bytes() -> usize {
    16_777_216
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aRouteConfig {
    pub public_path_prefix: String,
    pub allowed_hosts: Vec<String>,
    pub agent_ref: String,
    pub binding_id: String,
    pub publication_id: String,
    pub policy_digest: String,
    pub tenant_host_id: Uuid,
    pub instance_api_id: String,
    pub api_version_id: String,
    pub agent_def_id: String,
    pub implementation_kind: A2aImplementationKind,
    pub target_service_id: String,
    pub target_env_tag: String,
    pub target_path_prefix: String,
    #[serde(default)]
    pub outbound_path_prefix: Option<String>,
    #[serde(default)]
    pub outbound_allowed_hosts: Vec<String>,
    #[serde(default)]
    pub target_outbound_path_prefix: Option<String>,
    pub policy_endpoints: A2aPolicyEndpoints,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum A2aImplementationKind {
    LightAgent,
    ExternalSidecar,
    RemoteA2a,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aPolicyEndpoints {
    pub card: String,
    pub invoke: String,
}

#[derive(Debug, Clone)]
pub struct A2aRouterRuntime {
    config: A2aRouterConfig,
    routes: BTreeMap<String, A2aRouteConfig>,
    authorization_key: Arc<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct A2aRouteDecision {
    pub route: A2aRouteConfig,
    pub policy_endpoint: String,
    pub upstream_path: String,
    pub card_request: bool,
    pub outbound_request: bool,
}

#[derive(Debug, Clone)]
pub struct A2aTargetDecision {
    pub target: ProxyTarget,
    pub route: A2aRouteDecision,
}

impl A2aRouterRuntime {
    pub fn new(config: A2aRouterConfig, authorization_key: Vec<u8>) -> Result<Self, RuntimeError> {
        if config.enabled && config.routes.is_empty() {
            return Err(RuntimeError::Unsupported(
                "a2a-router requires at least one route when enabled".into(),
            ));
        }
        if config.maximum_request_body_bytes == 0
            || config.maximum_buffered_request_bytes < config.maximum_request_body_bytes
        {
            return Err(RuntimeError::Unsupported(
                "a2a-router body limits are invalid".into(),
            ));
        }
        if authorization_key.len() < 32 {
            return Err(RuntimeError::Unsupported(
                "a2a-router authorized-context key must contain at least 32 bytes".into(),
            ));
        }
        let mut routes = BTreeMap::new();
        let mut policy_endpoints = BTreeSet::new();
        for mut route in config.routes.iter().cloned() {
            route.public_path_prefix = normalize_path(&route.public_path_prefix)?;
            route.target_path_prefix = normalize_path(&route.target_path_prefix)?;
            route.outbound_path_prefix = route
                .outbound_path_prefix
                .as_deref()
                .map(normalize_path)
                .transpose()?;
            route.target_outbound_path_prefix = route
                .target_outbound_path_prefix
                .as_deref()
                .map(normalize_path)
                .transpose()?;
            route.outbound_allowed_hosts = route
                .outbound_allowed_hosts
                .iter()
                .map(|host| normalize_host(host))
                .collect::<Result<Vec<_>, _>>()?;
            if route.outbound_path_prefix.is_some() != route.target_outbound_path_prefix.is_some()
                || route.outbound_path_prefix.is_some()
                    != (!route.outbound_allowed_hosts.is_empty())
                || (route.implementation_kind == A2aImplementationKind::RemoteA2a
                    && route.outbound_path_prefix.is_none())
            {
                return Err(RuntimeError::Unsupported(
                    "REMOTE_A2A routes require paired outbound path prefixes".into(),
                ));
            }
            route.allowed_hosts = route
                .allowed_hosts
                .iter()
                .map(|host| normalize_host(host))
                .collect::<Result<Vec<_>, _>>()?;
            if route.allowed_hosts.is_empty()
                || route.instance_api_id.trim().is_empty()
                || route.agent_ref.trim().is_empty()
                || route.binding_id.trim().is_empty()
                || route.publication_id.trim().is_empty()
                || !route.policy_digest.starts_with("sha256:")
                || route.api_version_id.trim().is_empty()
                || route.agent_def_id.trim().is_empty()
                || route.target_service_id.trim().is_empty()
                || route.target_env_tag.trim().is_empty()
            {
                return Err(RuntimeError::Unsupported(
                    "a2a-router route contains an empty authority field".into(),
                ));
            }
            if Uuid::parse_str(&route.binding_id).is_err()
                || Uuid::parse_str(&route.publication_id).is_err()
            {
                return Err(RuntimeError::Unsupported(
                    "a2a-router bindingId and publicationId must be UUIDs".into(),
                ));
            }
            let expected_prefix = format!("a2a:instance-api:{}:", route.instance_api_id);
            if route.policy_endpoints.card != format!("{expected_prefix}card")
                || route.policy_endpoints.invoke != format!("{expected_prefix}invoke")
                || !policy_endpoints.insert(route.policy_endpoints.card.clone())
                || !policy_endpoints.insert(route.policy_endpoints.invoke.clone())
            {
                return Err(RuntimeError::Unsupported(
                    "a2a-router policy endpoints must be unique and derived from instanceApiId"
                        .into(),
                ));
            }
            if route.implementation_kind == A2aImplementationKind::LightAgent
                && route.target_service_id == "com.networknt.light-agent-1.0.0"
            {
                return Err(RuntimeError::Unsupported(
                    "native A2A routes must use the real registered agent serviceId".into(),
                ));
            }
            let key = route.public_path_prefix.clone();
            if routes.insert(key, route).is_some() {
                return Err(RuntimeError::Unsupported(
                    "a2a-router contains a duplicate normalized public route".into(),
                ));
            }
        }
        Ok(Self {
            config,
            routes,
            authorization_key: Arc::new(authorization_key),
        })
    }

    pub fn maximum_request_body_bytes(&self) -> usize {
        self.config.maximum_request_body_bytes
    }

    pub fn maximum_buffered_request_bytes(&self) -> usize {
        self.config.maximum_buffered_request_bytes
    }

    pub fn resolve(
        &self,
        host: &str,
        method: &str,
        path: &str,
    ) -> Result<A2aRouteDecision, HandlerRejection> {
        let host = normalize_request_host(host);
        if let Some(route) = self.routes.values().find(|route| {
            route.outbound_path_prefix.as_deref() == Some(path)
                && route
                    .outbound_allowed_hosts
                    .iter()
                    .any(|allowed| allowed == &host)
        }) {
            if method != "POST" {
                return Err(HandlerRejection::new(
                    405,
                    "ERR10201",
                    "method not allowed for A2A route",
                ));
            }
            return Ok(A2aRouteDecision {
                route: route.clone(),
                policy_endpoint: route.policy_endpoints.invoke.clone(),
                upstream_path: route
                    .target_outbound_path_prefix
                    .clone()
                    .expect("validated outbound target path"),
                card_request: false,
                outbound_request: true,
            });
        }
        let (public_prefix, card_request) = card_or_invoke_prefix(path);
        let route = self
            .routes
            .get(public_prefix)
            .ok_or_else(|| HandlerRejection::new(404, "ERR10200", "A2A route is not published"))?;
        if !route.allowed_hosts.iter().any(|allowed| allowed == &host) {
            return Err(HandlerRejection::new(
                404,
                "ERR10200",
                "A2A route is not published",
            ));
        }
        if (card_request && method != "GET") || (!card_request && method != "POST") {
            return Err(HandlerRejection::new(
                405,
                "ERR10201",
                "method not allowed for A2A route",
            ));
        }
        let suffix = path.strip_prefix(public_prefix).unwrap_or("");
        let upstream_path = format!("{}{}", route.target_path_prefix, suffix);
        Ok(A2aRouteDecision {
            route: route.clone(),
            policy_endpoint: if card_request {
                route.policy_endpoints.card.clone()
            } else {
                route.policy_endpoints.invoke.clone()
            },
            upstream_path,
            card_request,
            outbound_request: false,
        })
    }

    pub async fn select_target(
        &self,
        router: &RouterRoute,
        decision: A2aRouteDecision,
        index: usize,
    ) -> Result<A2aTargetDecision, HandlerRejection> {
        let target = select_registered_service_target(
            router,
            &decision.route.target_service_id,
            Some(&decision.route.target_env_tag),
            index,
        )
        .await?;
        Ok(A2aTargetDecision {
            target,
            route: decision,
        })
    }

    pub fn authorize_invocation(
        &self,
        decision: &A2aRouteDecision,
        principal_subject: &str,
        caller_agent_ref: &str,
        correlation_id: &str,
        body: &[u8],
    ) -> Result<(String, String), HandlerRejection> {
        if decision.card_request {
            return Err(HandlerRejection::new(
                500,
                "ERR10202",
                "Agent Card requests do not carry an invocation context",
            ));
        }
        let binding_id = Uuid::parse_str(&decision.route.binding_id).map_err(|_| {
            HandlerRejection::new(500, "ERR10202", "A2A route bindingId is invalid")
        })?;
        let publication_id = Uuid::parse_str(&decision.route.publication_id).map_err(|_| {
            HandlerRejection::new(500, "ERR10202", "A2A route publicationId is invalid")
        })?;
        let audience = match decision.route.implementation_kind {
            A2aImplementationKind::LightAgent => "light-agent",
            A2aImplementationKind::ExternalSidecar | A2aImplementationKind::RemoteA2a => {
                "light-a2a"
            }
        };
        let now = Utc::now();
        let invocation = AuthorizedInvocation {
            host_id: decision.route.tenant_host_id,
            audience: audience.to_string(),
            principal_subject: principal_subject.to_string(),
            caller_agent_ref: caller_agent_ref.to_string(),
            target_agent_ref: decision.route.agent_ref.clone(),
            binding_id,
            policy_digest: decision.route.policy_digest.clone(),
            publication_id,
            direction: Direction::Inbound,
            idempotency_key: correlation_id.to_string(),
            request_digest: request_digest(body),
            outbound: None,
            issued_at: now,
            expires_at: now + Duration::seconds(30),
        };
        invocation.validate(audience, now).map_err(|_| {
            HandlerRejection::new(500, "ERR10202", "A2A authorized context is invalid")
        })?;
        sign_authorized_invocation(&invocation, body, &self.authorization_key).map_err(|_| {
            HandlerRejection::new(500, "ERR10202", "A2A authorized context signing failed")
        })
    }

    pub fn authorize_forwarded_outbound(
        &self,
        decision: &A2aRouteDecision,
        encoded: &str,
        signature: &str,
        body: &[u8],
    ) -> Result<(String, String), HandlerRejection> {
        if !decision.outbound_request {
            return Err(HandlerRejection::new(
                500,
                "ERR10202",
                "not an outbound A2A route",
            ));
        }
        let invocation = a2a_core::verify_authorized_invocation(
            encoded,
            signature,
            body,
            &self.authorization_key,
            "light-a2a",
            Utc::now(),
        )
        .map_err(|_| HandlerRejection::new(403, "ERR10203", "outbound A2A context rejected"))?;
        if invocation.direction != Direction::Outbound
            || invocation.target_agent_ref != decision.route.agent_ref
            || invocation.binding_id.to_string() != decision.route.binding_id
            || invocation.publication_id.to_string() != decision.route.publication_id
            || invocation.policy_digest != decision.route.policy_digest
        {
            return Err(HandlerRejection::new(
                403,
                "ERR10203",
                "outbound A2A binding mismatch",
            ));
        }
        Ok((encoded.to_string(), signature.to_string()))
    }
}

fn card_or_invoke_prefix(path: &str) -> (&str, bool) {
    for suffix in ["/.well-known/agent-card.json", "/.well-known/agent.json"] {
        if let Some(prefix) = path.strip_suffix(suffix) {
            return (prefix, true);
        }
    }
    (path, false)
}

fn normalize_path(value: &str) -> Result<String, RuntimeError> {
    let value = value.trim().trim_end_matches('/');
    if !value.starts_with('/') || value.len() < 2 || value.contains('?') || value.contains('#') {
        return Err(RuntimeError::Unsupported(
            "A2A route paths must be absolute path prefixes".into(),
        ));
    }
    Ok(value.to_string())
}

fn normalize_host(value: &str) -> Result<String, RuntimeError> {
    let host = normalize_request_host(value);
    if host.is_empty() || host.contains('/') || host.contains("://") {
        return Err(RuntimeError::Unsupported(
            "A2A allowed host is invalid".into(),
        ));
    }
    Ok(host)
}

fn normalize_request_host(value: &str) -> String {
    let value = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if value.starts_with('[') {
        return value
            .split_once(']')
            .map_or(value.clone(), |(host, _)| format!("{host}]"));
    }
    value.split(':').next().unwrap_or("").to_string()
}

pub fn load_a2a_router_runtime(
    runtime: &RuntimeConfig,
    active: bool,
) -> Result<Option<A2aRouterRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }
    let config = runtime
        .module_registry
        .load_config::<A2aRouterConfig>(runtime, A2A_ROUTER_FILE)?;
    let enabled = config.enabled;
    if !enabled {
        runtime.module_registry.register_loaded_config(
            A2A_ROUTER_MODULE_ID,
            A2A_ROUTER_CONFIG_NAME,
            ModuleKind::Framework,
            &config,
            [],
            true,
            Some(false),
            true,
        )?;
        return Ok(None);
    }
    let authorization_key =
        std::fs::read(&config.authorization_context_key_file).map_err(|error| {
            RuntimeError::Config(format!("read A2A authorized-context key: {error}"))
        })?;
    let router = A2aRouterRuntime::new(config.clone(), authorization_key)?;
    runtime.module_registry.register_loaded_config(
        A2A_ROUTER_MODULE_ID,
        A2A_ROUTER_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        true,
        Some(enabled),
        true,
    )?;
    Ok(enabled.then_some(router))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::RouterConfig;
    use a2a_core::verify_authorized_invocation;
    use light_runtime::DirectRegistryConfig;

    fn runtime() -> A2aRouterRuntime {
        A2aRouterRuntime::new(
            A2aRouterConfig {
                enabled: true,
                maximum_request_body_bytes: 1024,
                maximum_buffered_request_bytes: 8192,
                authorization_context_key_file: "/test/a2a-key".into(),
                routes: vec![A2aRouteConfig {
                    public_path_prefix: "/a2a/account".into(),
                    allowed_hosts: vec!["Agents.Example.com".into()],
                    agent_ref: "account.agent".into(),
                    binding_id: "50000000-0000-0000-0000-000000000008".into(),
                    publication_id: "50000000-0000-0000-0000-000000000009".into(),
                    policy_digest: format!("sha256:{}", "a".repeat(64)),
                    tenant_host_id: Uuid::nil(),
                    instance_api_id: "i1".into(),
                    api_version_id: "a1".into(),
                    agent_def_id: "d1".into(),
                    implementation_kind: A2aImplementationKind::LightAgent,
                    target_service_id: "com.networknt.agent.account-1.0.0".into(),
                    target_env_tag: "prod".into(),
                    target_path_prefix: "/a2a/account.agent".into(),
                    outbound_path_prefix: None,
                    outbound_allowed_hosts: Vec::new(),
                    target_outbound_path_prefix: None,
                    policy_endpoints: A2aPolicyEndpoints {
                        card: "a2a:instance-api:i1:card".into(),
                        invoke: "a2a:instance-api:i1:invoke".into(),
                    },
                }],
            },
            vec![b'k'; 32],
        )
        .unwrap()
    }

    #[test]
    fn identical_raw_protocol_paths_resolve_to_instance_scoped_policy() {
        let invoke = runtime()
            .resolve("agents.example.com:443", "POST", "/a2a/account")
            .unwrap();
        assert_eq!(invoke.policy_endpoint, "a2a:instance-api:i1:invoke");
        assert_eq!(invoke.upstream_path, "/a2a/account.agent");
        let card = runtime()
            .resolve(
                "agents.example.com",
                "GET",
                "/a2a/account/.well-known/agent-card.json",
            )
            .unwrap();
        assert_eq!(card.policy_endpoint, "a2a:instance-api:i1:card");
        assert_eq!(
            card.upstream_path,
            "/a2a/account.agent/.well-known/agent-card.json"
        );
    }

    #[test]
    fn outbound_route_preserves_only_a_verified_binding_scoped_context() {
        let mut config = runtime().config.clone();
        config.routes[0].implementation_kind = A2aImplementationKind::RemoteA2a;
        config.routes[0].target_service_id = "com.networknt.light-a2a-1.0.0".into();
        config.routes[0].outbound_path_prefix = Some("/internal/a2a/outbound/account.agent".into());
        config.routes[0].outbound_allowed_hosts = vec!["gateway.example".into()];
        config.routes[0].target_outbound_path_prefix =
            Some("/internal/a2a/outbound/account.agent".into());
        let router = A2aRouterRuntime::new(config, vec![b'k'; 32]).unwrap();
        let decision = router
            .resolve(
                "gateway.example",
                "POST",
                "/internal/a2a/outbound/account.agent",
            )
            .unwrap();
        assert!(decision.outbound_request);
        let body = br#"{"jsonrpc":"2.0","id":"m1","method":"message/send","params":{}}"#;
        let now = Utc::now();
        let invocation = AuthorizedInvocation {
            host_id: Uuid::nil(),
            audience: "light-a2a".into(),
            principal_subject: "user:alice".into(),
            caller_agent_ref: "caller.agent".into(),
            target_agent_ref: "account.agent".into(),
            binding_id: Uuid::parse_str("50000000-0000-0000-0000-000000000008").unwrap(),
            policy_digest: format!("sha256:{}", "a".repeat(64)),
            publication_id: Uuid::parse_str("50000000-0000-0000-0000-000000000009").unwrap(),
            direction: Direction::Outbound,
            idempotency_key: "m1".into(),
            request_digest: request_digest(body),
            outbound: Some(a2a_core::OutboundInvocationConstraints {
                delegation_id: Uuid::now_v7(),
                environment: "prod".into(),
                data_boundary_digest: format!("sha256:{}", "b".repeat(64)),
                delegation_depth: 1,
                maximum_delegation_depth: 4,
                remaining_budget_units: 1024,
                deadline: now + Duration::seconds(20),
                call_chain: vec!["caller.agent".into()],
                skill_id: None,
            }),
            issued_at: now,
            expires_at: now + Duration::seconds(30),
        };
        let (encoded, signature) =
            sign_authorized_invocation(&invocation, body, &[b'k'; 32]).unwrap();
        assert!(
            router
                .authorize_forwarded_outbound(&decision, &encoded, &signature, body)
                .is_ok()
        );
        assert!(
            router
                .authorize_forwarded_outbound(&decision, &encoded, &signature, b"tampered")
                .is_err()
        );
    }

    #[test]
    fn route_does_not_prefix_match_unpublished_children() {
        assert!(
            runtime()
                .resolve("agents.example.com", "POST", "/a2a/account/admin")
                .is_err()
        );
    }

    #[test]
    fn gateway_context_is_short_lived_body_bound_and_instance_scoped() {
        let runtime = runtime();
        let decision = runtime
            .resolve("agents.example.com", "POST", "/a2a/account")
            .unwrap();
        let body = br#"{"jsonrpc":"2.0","id":"1","method":"message/send","params":{}}"#;
        let (context, signature) = runtime
            .authorize_invocation(
                &decision,
                "user:alice",
                "client:calling-agent",
                "correlation-1",
                body,
            )
            .unwrap();
        let invocation = verify_authorized_invocation(
            &context,
            &signature,
            body,
            &[b'k'; 32],
            "light-agent",
            Utc::now(),
        )
        .unwrap();
        assert_eq!(invocation.host_id, Uuid::nil());
        assert_eq!(invocation.target_agent_ref, "account.agent");
        assert_eq!(invocation.principal_subject, "user:alice");
        assert_eq!(invocation.caller_agent_ref, "client:calling-agent");
        assert_eq!(invocation.idempotency_key, "correlation-1");
        assert!(
            verify_authorized_invocation(
                &context,
                &signature,
                b"different",
                &[b'k'; 32],
                "light-agent",
                Utc::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn agents_sharing_raw_endpoint_keep_distinct_public_authority() {
        let first = runtime().config.routes[0].clone();
        let mut second = first.clone();
        second.public_path_prefix = "/a2a/billing".into();
        second.agent_ref = "billing.agent".into();
        second.binding_id = "50000000-0000-0000-0000-000000000018".into();
        second.publication_id = "50000000-0000-0000-0000-000000000019".into();
        second.instance_api_id = "i2".into();
        second.policy_endpoints.card = "a2a:instance-api:i2:card".into();
        second.policy_endpoints.invoke = "a2a:instance-api:i2:invoke".into();
        let runtime = A2aRouterRuntime::new(
            A2aRouterConfig {
                enabled: true,
                maximum_request_body_bytes: 1024,
                maximum_buffered_request_bytes: 8192,
                authorization_context_key_file: "/test/a2a-key".into(),
                routes: vec![first, second],
            },
            vec![b'k'; 32],
        )
        .unwrap();
        let account = runtime
            .resolve("agents.example.com", "POST", "/a2a/account")
            .unwrap();
        let billing = runtime
            .resolve("agents.example.com", "POST", "/a2a/billing")
            .unwrap();
        assert_eq!(account.upstream_path, billing.upstream_path);
        assert_ne!(account.policy_endpoint, billing.policy_endpoint);
        assert_ne!(account.route.binding_id, billing.route.binding_id);
    }

    #[tokio::test]
    async fn registered_service_routing_ignores_caller_selected_destinations() {
        let runtime = runtime();
        let decision = runtime
            .resolve("agents.example.com", "POST", "/a2a/account")
            .unwrap();
        let route = RouterRoute {
            config: RouterConfig::default(),
            direct_registry: DirectRegistryConfig {
                direct_urls: BTreeMap::from([(
                    "com.networknt.agent.account-1.0.0|prod".into(),
                    "https://agent-one.internal:8448".into(),
                )]),
            },
            registry_client: None,
        };
        let selected = runtime.select_target(&route, decision, 999).await.unwrap();
        assert_eq!(selected.target.address, "agent-one.internal:8448");
        assert_eq!(selected.target.path_prefix, "");
    }
}
