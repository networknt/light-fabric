//! Lambda selection/status/header compatibility around the shared counter core.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    *,
};
use async_trait::async_trait;
use gateway_policy::rate_limit::{
    LimitConfig, LimitKey, LimitQuota, RateLimitRuntime, check_selected_rate_limit,
};
use serde_yaml::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy)]
enum Key {
    Server,
    Address,
    Akamai,
    Client,
    HeaderClient,
    User,
}
#[derive(Clone)]
pub struct LimitHandler {
    enabled: bool,
    key: Key,
    status: u16,
    runtime: RateLimitRuntime,
    server_prefixes: Vec<String>,
    server_buckets: Arc<Mutex<super::limit_order::CounterOrder>>,
    configured: BTreeMap<String, LimitQuota>,
    fallback: Vec<LimitQuota>,
}
fn invalid() -> InitError {
    InitError::Malformed("limit.yml: unsupported, unknown or malformed setting")
}
fn internal_label(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    value
        .bytes()
        .flat_map(|b| {
            [
                char::from(HEX[(b >> 4) as usize]),
                char::from(HEX[(b & 15) as usize]),
            ]
        })
        .collect()
}
fn java_map_order(keys: &[String]) -> Vec<String> {
    super::limit_order::quota_order(keys)
}
impl LimitHandler {
    pub fn warnings(value: &Value) -> Result<Vec<String>, InitError> {
        let map = value.as_mapping().ok_or_else(invalid)?;
        let mut warnings = Vec::new();
        for (key, default) in [
            ("concurrentRequest", Value::from(2)),
            ("queueSize", Value::from(-1)),
            ("headersAlwaysSet", Value::from(false)),
        ] {
            if map.get(key).is_some_and(|v| *v != default) {
                return Err(invalid());
            }
            warnings.push(format!(
                "limit.{key}: default/absent unsupported setting; no Lambda behavior"
            ));
        }
        Ok(warnings)
    }
    pub fn from_yaml(mut value: Value) -> Result<Self, InitError> {
        Self::warnings(&value)?;
        let map = value.as_mapping_mut().ok_or_else(invalid)?;
        let keys = [
            "enabled",
            "concurrentRequest",
            "queueSize",
            "errorCode",
            "rateLimit",
            "headersAlwaysSet",
            "key",
            "server",
            "address",
            "client",
            "user",
            "clientIdKeyResolver",
            "addressKeyResolver",
            "userIdKeyResolver",
        ];
        if map
            .keys()
            .any(|k| !k.as_str().is_some_and(|k| keys.contains(&k)))
        {
            return Err(invalid());
        }
        let key_value = match map.get("key") {
            None | Some(Value::Null) => "server",
            Some(v) => v.as_str().ok_or_else(invalid)?,
        };
        let mut key = match key_value {
            "server" => Key::Server,
            "address" => Key::Address,
            "client" => Key::Client,
            "user" => Key::User,
            _ => return Err(invalid()),
        };
        for (name, normal, extra) in [
            (
                "addressKeyResolver",
                "RemoteAddressKeyResolver",
                Some("AkamaiAddressKeyResolver"),
            ),
            (
                "clientIdKeyResolver",
                "JwtClientIdKeyResolver",
                Some("JwtHeaderClientIdKeyResolver"),
            ),
            ("userIdKeyResolver", "JwtUserIdKeyResolver", None),
        ] {
            if let Some(v) = map.remove(Value::from(name))
                && !v.is_null()
            {
                let name_value = v.as_str().ok_or_else(invalid)?;
                let prefix = [
                    "com.networknt.limit.key.",
                    "com.networknt.aws.lambda.handler.middleware.limit.key.",
                ]
                .iter()
                .find_map(|p| name_value.strip_prefix(p))
                .ok_or_else(invalid)?;
                if prefix != normal && Some(prefix) != extra {
                    return Err(invalid());
                }
                if prefix == "AkamaiAddressKeyResolver" && matches!(key, Key::Address) {
                    key = Key::Akamai
                }
                if prefix == "JwtHeaderClientIdKeyResolver" && matches!(key, Key::Client) {
                    key = Key::HeaderClient
                }
            }
        }
        let raw = map.get("server").cloned().unwrap_or(Value::Null);
        let raw: Value = if let Some(s) = raw.as_str() {
            serde_yaml::from_str(s).map_err(|_| invalid())?
        } else {
            raw
        };
        let server_prefixes = match raw {
            Value::Null => vec![],
            Value::Mapping(m) => m
                .keys()
                .map(|v| {
                    v.as_str()
                        .filter(|p| p.starts_with('/'))
                        .map(str::to_owned)
                        .ok_or_else(invalid)
                })
                .collect::<Result<Vec<_>, _>>()?,
            _ => return Err(invalid()),
        };
        map.insert(Value::from("concurrentRequest"), Value::from(0));
        map.insert(Value::from("queueSize"), Value::from(0));
        let config: LimitConfig = serde_yaml::from_value(value).map_err(|_| invalid())?;
        if config.error_code != 0 && !(400..600).contains(&config.error_code) {
            return Err(invalid());
        }
        let configured = match key {
            Key::Server => &config.server,
            Key::Address | Key::Akamai => &config.address,
            Key::Client | Key::HeaderClient => &config.client,
            Key::User => &config.user,
        };
        let configured = configured
            .iter()
            .map(|(k, v)| {
                v.first()
                    .copied()
                    .map(|quota| (k.clone(), quota))
                    .ok_or_else(invalid)
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let fallback = config
            .rate_limit
            .first()
            .copied()
            .into_iter()
            .collect::<Vec<_>>();
        if config.enabled && fallback.is_empty() {
            return Err(invalid());
        }
        if configured
            .values()
            .chain(fallback.iter())
            .any(|q| ![1, 60, 3600, 86400].contains(&q.window_seconds))
        {
            return Err(invalid());
        }
        // Internal labels are isolated and never forwarded or treated as principals.
        // Counter storage, expiry and synchronized updates remain in gateway-policy.
        let runtime = RateLimitRuntime::new(LimitConfig {
            enabled: true,
            key: LimitKey::Address,
            rate_limit: fallback.clone(),
            address: configured
                .iter()
                .map(|(k, v)| (internal_label(k), vec![*v]))
                .collect(),
            ..LimitConfig::default()
        });
        let server_prefixes = java_map_order(&server_prefixes);
        let server_buckets = Arc::new(Mutex::new(super::limit_order::CounterOrder::new(
            &server_prefixes,
        )));
        Ok(Self {
            enabled: config.enabled,
            key,
            status: if config.error_code == 0 {
                429
            } else {
                config.error_code
            },
            runtime,
            server_prefixes,
            server_buckets,
            configured,
            fallback,
        })
    }
    fn label(&self, inv: &Invocation, state: &ExchangeState) -> Result<String, StatusError> {
        match self.key {
            Key::Server => {
                let mut buckets = self
                    .server_buckets
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                Ok(buckets.label(&inv.path))
            }
            Key::Address => inv
                .source
                .source_ip
                .map(|ip| ip.to_string())
                .ok_or_else(StatusError::handler_failure),
            Key::Akamai => inv
                .headers
                .get("true-client-ip")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(StatusError::handler_failure),
            Key::Client => state
                .principal
                .as_ref()
                .and_then(Principal::client_id)
                .map(str::to_owned)
                .ok_or_else(StatusError::missing_identity),
            Key::HeaderClient => {
                let principal = state
                    .principal
                    .as_ref()
                    .ok_or_else(StatusError::missing_identity)?;
                principal
                    .client_id()
                    .or_else(|| inv.headers.get("client-id").and_then(|v| v.to_str().ok()))
                    .map(str::to_owned)
                    .ok_or_else(StatusError::missing_identity)
            }
            Key::User => state
                .principal
                .as_ref()
                .and_then(Principal::user_id)
                .map(str::to_owned)
                .ok_or_else(StatusError::missing_identity),
        }
    }
}
#[async_trait]
impl RequestHandler for LimitHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Limit
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if !self.enabled {
            return Ok(HandlerOutcome::Continue);
        }
        let label = match self.label(inv, state) {
            Ok(label) => label,
            Err(error) => return Ok(HandlerOutcome::Reject(error)),
        };
        let path_label = format!("{label}#{}", inv.path);
        let (selected_label, configured) = if matches!(self.key, Key::Server) {
            let quota = self
                .server_prefixes
                .iter()
                .find(|prefix| inv.path.starts_with(prefix.as_str()))
                .and_then(|prefix| self.configured.get(prefix));
            (label.clone(), quota)
        } else if let Some(quota) = self.configured.get(&path_label) {
            (path_label, Some(quota))
        } else {
            (label.clone(), self.configured.get(&label))
        };
        let quota = configured
            .copied()
            .or_else(|| self.fallback.first().copied())
            .ok_or(HandlerFailure)?;
        // Java recreates the direct fallback bucket on every request. Configured
        // direct buckets and server buckets retain the shared synchronized state.
        let fallback;
        let runtime = if !matches!(self.key, Key::Server) && configured.is_none() {
            fallback = RateLimitRuntime::new(LimitConfig {
                enabled: true,
                key: LimitKey::Address,
                rate_limit: self.fallback.clone(),
                ..LimitConfig::default()
            });
            &fallback
        } else {
            &self.runtime
        };
        let decision = check_selected_rate_limit(runtime, &internal_label(&selected_label), quota);
        if decision.allowed {
            Ok(HandlerOutcome::Continue)
        } else {
            let status = StatusError::new(
                429,
                "ERR10088",
                "RATE_LIMIT_EXCEEDED",
                "Rate limit exceeded.",
            );
            state.audit.status = Some(status.clone());
            let mut response = status.response();
            response.status =
                http::StatusCode::from_u16(self.status).map_err(|_| HandlerFailure)?;
            let unit = match quota.window_seconds {
                1 => "SECONDS",
                60 => "MINUTES",
                3600 => "HOURS",
                _ => "DAYS",
            };
            response.headers.insert(
                "ratelimit-limit",
                format!("{}/{unit}", quota.limit)
                    .parse()
                    .map_err(|_| HandlerFailure)?,
            );
            let remaining = i128::from(quota.limit) - i128::from(decision.count);
            let reset = decision.reset.ok_or(HandlerFailure)?;
            response.headers.insert(
                "ratelimit-remaining",
                remaining.to_string().parse().map_err(|_| HandlerFailure)?,
            );
            response.headers.insert(
                "ratelimit-reset",
                format!("{reset}s").parse().map_err(|_| HandlerFailure)?,
            );
            Ok(HandlerOutcome::Respond(response))
        }
    }
}
impl HandlerFactory for LimitHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities {
                establishes_verified_identity: false,
                requires_verified_identity: self.enabled
                    && matches!(self.key, Key::Client | Key::HeaderClient | Key::User),
            },
        ))
    }
}
