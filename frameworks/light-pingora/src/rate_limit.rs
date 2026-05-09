use crate::config_util::request_header;
use crate::security::{AuthPrincipal, HandlerRejection};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::http::ResponseHeader;
use pingora::prelude::Session;
use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LIMIT_FILE: &str = "limit.yml";
pub const LIMIT_MODULE_ID: &str = "light-pingora/limit";
pub const LIMIT_CONFIG_NAME: &str = "limit";

const X_RATE_LIMIT_LIMIT: &str = "X-RateLimit-Limit";
const X_RATE_LIMIT_REMAINING: &str = "X-RateLimit-Remaining";
const X_RATE_LIMIT_RESET: &str = "X-RateLimit-Reset";
const RETRY_AFTER: &str = "Retry-After";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub concurrent_request: u64,
    #[serde(default)]
    pub queue_size: u64,
    #[serde(default = "default_error_code")]
    pub error_code: u16,
    #[serde(default, deserialize_with = "deserialize_quota_list")]
    pub rate_limit: Vec<LimitQuota>,
    #[serde(default)]
    pub headers_always_set: bool,
    #[serde(default)]
    pub key: LimitKey,
    #[serde(default, deserialize_with = "deserialize_quota_map")]
    pub server: BTreeMap<String, Vec<LimitQuota>>,
    #[serde(default, deserialize_with = "deserialize_quota_map")]
    pub address: BTreeMap<String, Vec<LimitQuota>>,
    #[serde(default, deserialize_with = "deserialize_quota_map")]
    pub client: BTreeMap<String, Vec<LimitQuota>>,
    #[serde(default, deserialize_with = "deserialize_quota_map")]
    pub user: BTreeMap<String, Vec<LimitQuota>>,
}

impl Default for LimitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            concurrent_request: 0,
            queue_size: 0,
            error_code: default_error_code(),
            rate_limit: Vec::new(),
            headers_always_set: false,
            key: LimitKey::Server,
            server: BTreeMap::new(),
            address: BTreeMap::new(),
            client: BTreeMap::new(),
            user: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitKey {
    #[default]
    Server,
    Address,
    Client,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LimitQuota {
    pub limit: u64,
    pub window_seconds: u64,
}

impl<'de> Deserialize<'de> for LimitQuota {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct LimitQuotaVisitor;

        impl<'de> Visitor<'de> for LimitQuotaVisitor {
            type Value = LimitQuota;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a quota string like 100/s or an object")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                parse_quota(value).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                self.visit_str(value.as_str())
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: DeError,
            {
                Ok(LimitQuota {
                    limit: value,
                    window_seconds: 1,
                })
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut limit = None;
                let mut window_seconds = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "limit" => limit = Some(map.next_value()?),
                        "windowSeconds" | "window_seconds" => {
                            window_seconds = Some(map.next_value()?)
                        }
                        "unit" => {
                            let unit = map.next_value::<String>()?;
                            window_seconds = Some(
                                window_seconds_for_unit(unit.as_str()).map_err(A::Error::custom)?,
                            )
                        }
                        _ => {
                            let _ = map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(LimitQuota {
                    limit: limit.ok_or_else(|| A::Error::custom("missing quota limit"))?,
                    window_seconds: window_seconds.unwrap_or(1),
                })
            }
        }

        deserializer.deserialize_any(LimitQuotaVisitor)
    }
}

#[derive(Clone)]
pub struct RateLimitRuntime {
    pub config: LimitConfig,
    state: Arc<Mutex<BTreeMap<String, VecDeque<u64>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitHeaders {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
    pub retry_after: Option<u64>,
}

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

    Ok(config.enabled.then_some(RateLimitRuntime {
        config,
        state: Arc::new(Mutex::new(BTreeMap::new())),
    }))
}

pub fn check_rate_limit(
    session: &Session,
    runtime: &RateLimitRuntime,
    principal: Option<&AuthPrincipal>,
    request_path: &str,
) -> Result<Option<RateLimitHeaders>, HandlerRejection> {
    let config = &runtime.config;
    if !config.enabled {
        return Ok(None);
    }
    let Some((key, quotas)) = rate_limit_key_and_quotas(session, config, principal, request_path)?
    else {
        return Ok(None);
    };
    if quotas.is_empty() {
        return Ok(None);
    }

    let now = now_epoch_seconds();
    let max_window = quotas
        .iter()
        .map(|quota| quota.window_seconds)
        .max()
        .unwrap_or(1);
    let mut state = runtime
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let entries = state.entry(key).or_default();
    while entries
        .front()
        .is_some_and(|timestamp| now.saturating_sub(*timestamp) >= max_window)
    {
        entries.pop_front();
    }

    for quota in &quotas {
        let count = entries
            .iter()
            .filter(|timestamp| now.saturating_sub(**timestamp) < quota.window_seconds)
            .count() as u64;
        if count >= quota.limit {
            let headers = headers_for_quota(*quota, count, entries, now, true);
            return Err(HandlerRejection::new(
                config.error_code,
                "ERR10077",
                "rate limit exceeded",
            )
            .with_header(X_RATE_LIMIT_LIMIT, headers.limit.to_string())
            .with_header(X_RATE_LIMIT_REMAINING, headers.remaining.to_string())
            .with_header(X_RATE_LIMIT_RESET, headers.reset.to_string())
            .with_header(
                RETRY_AFTER,
                headers.retry_after.unwrap_or(headers.reset).to_string(),
            ));
        }
    }

    entries.push_back(now);
    if config.headers_always_set {
        let quota = quotas[0];
        let count = entries
            .iter()
            .filter(|timestamp| now.saturating_sub(**timestamp) < quota.window_seconds)
            .count() as u64;
        return Ok(Some(headers_for_quota(quota, count, entries, now, false)));
    }
    Ok(None)
}

pub fn apply_rate_limit_headers(
    response: &mut ResponseHeader,
    headers: &RateLimitHeaders,
) -> pingora::Result<()> {
    response.insert_header(X_RATE_LIMIT_LIMIT, headers.limit.to_string())?;
    response.insert_header(X_RATE_LIMIT_REMAINING, headers.remaining.to_string())?;
    response.insert_header(X_RATE_LIMIT_RESET, headers.reset.to_string())?;
    if let Some(retry_after) = headers.retry_after {
        response.insert_header(RETRY_AFTER, retry_after.to_string())?;
    }
    Ok(())
}

fn rate_limit_key_and_quotas(
    session: &Session,
    config: &LimitConfig,
    principal: Option<&AuthPrincipal>,
    request_path: &str,
) -> Result<Option<(String, Vec<LimitQuota>)>, HandlerRejection> {
    match config.key {
        LimitKey::Server => {
            let quotas = best_path_quotas(&config.server, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| ("server".to_string(), quotas.to_vec())))
        }
        LimitKey::Address => {
            let address = request_header(session, "x-forwarded-for")
                .and_then(|value| value.split(',').next().map(str::trim).map(str::to_string))
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    session.as_downstream().client_addr().map(|address| {
                        address
                            .as_inet()
                            .map(|address| address.ip().to_string())
                            .unwrap_or_else(|| address.to_string())
                    })
                })
                .unwrap_or_else(|| "unknown".to_string());
            let quotas = direct_quotas(&config.address, &address, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| (format!("address:{address}"), quotas.to_vec())))
        }
        LimitKey::Client => {
            let client_id = principal
                .and_then(|principal| principal.client_id.as_deref())
                .ok_or_else(|| {
                    HandlerRejection::unauthorized("client id is required for client rate limiting")
                })?;
            let quotas = direct_quotas(&config.client, client_id, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| (format!("client:{client_id}"), quotas.to_vec())))
        }
        LimitKey::User => {
            let user_id = principal
                .and_then(|principal| principal.user_id.as_deref())
                .ok_or_else(|| {
                    HandlerRejection::unauthorized("user id is required for user rate limiting")
                })?;
            let quotas = direct_quotas(&config.user, user_id, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| (format!("user:{user_id}"), quotas.to_vec())))
        }
    }
}

fn best_path_quotas<'a>(
    map: &'a BTreeMap<String, Vec<LimitQuota>>,
    request_path: &str,
) -> Option<&'a [LimitQuota]> {
    map.iter()
        .filter(|(prefix, quotas)| request_path.starts_with(prefix.as_str()) && !quotas.is_empty())
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, quotas)| quotas.as_slice())
}

fn direct_quotas<'a>(
    map: &'a BTreeMap<String, Vec<LimitQuota>>,
    direct_key: &str,
    request_path: &str,
) -> Option<&'a [LimitQuota]> {
    map.get(format!("{direct_key}#{request_path}").as_str())
        .or_else(|| map.get(direct_key))
        .map(Vec::as_slice)
        .filter(|quotas| !quotas.is_empty())
}

fn non_empty(values: &[LimitQuota]) -> Option<&[LimitQuota]> {
    (!values.is_empty()).then_some(values)
}

fn headers_for_quota(
    quota: LimitQuota,
    count: u64,
    entries: &VecDeque<u64>,
    now: u64,
    rejected: bool,
) -> RateLimitHeaders {
    let oldest = entries
        .iter()
        .copied()
        .find(|timestamp| now.saturating_sub(*timestamp) < quota.window_seconds)
        .unwrap_or(now);
    let reset = quota
        .window_seconds
        .saturating_sub(now.saturating_sub(oldest))
        .max(1);
    let used = if rejected {
        count
    } else {
        count.min(quota.limit)
    };
    RateLimitHeaders {
        limit: quota.limit,
        remaining: quota.limit.saturating_sub(used),
        reset,
        retry_after: rejected.then_some(reset),
    }
}

fn deserialize_quota_list<'de, D>(deserializer: D) -> Result<Vec<LimitQuota>, D::Error>
where
    D: Deserializer<'de>,
{
    struct QuotaListVisitor;

    impl<'de> Visitor<'de> for QuotaListVisitor {
        type Value = Vec<LimitQuota>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a quota string or list")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            parse_quota_list(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(vec![LimitQuota {
                limit: value,
                window_seconds: 1,
            }])
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<LimitQuota>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(QuotaListVisitor)
}

fn deserialize_quota_map<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, Vec<LimitQuota>>, D::Error>
where
    D: Deserializer<'de>,
{
    struct QuotaMapVisitor;

    impl<'de> Visitor<'de> for QuotaMapVisitor {
        type Value = BTreeMap<String, Vec<LimitQuota>>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a quota map or JSON/YAML string map")
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
            parse_quota_map_value(yaml).map_err(E::custom)
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
                values.insert(key, parse_quota_value(value).map_err(A::Error::custom)?);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(QuotaMapVisitor)
}

fn parse_quota_map_value(value: YamlValue) -> Result<BTreeMap<String, Vec<LimitQuota>>, String> {
    match value {
        YamlValue::Mapping(map) => {
            let mut values = BTreeMap::new();
            for (key, value) in map {
                let key = key
                    .as_str()
                    .ok_or_else(|| "limit map key must be a string".to_string())?
                    .to_string();
                values.insert(key, parse_quota_value(value)?);
            }
            Ok(values)
        }
        YamlValue::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected limit quota map, got {other:?}")),
    }
}

fn parse_quota_value(value: YamlValue) -> Result<Vec<LimitQuota>, String> {
    match value {
        YamlValue::Null => Ok(Vec::new()),
        YamlValue::String(value) => parse_quota_list(value.as_str()),
        YamlValue::Number(number) => number
            .as_u64()
            .map(|limit| {
                vec![LimitQuota {
                    limit,
                    window_seconds: 1,
                }]
            })
            .ok_or_else(|| "limit quota number must be unsigned".to_string()),
        YamlValue::Sequence(values) => values
            .into_iter()
            .map(|value| match value {
                YamlValue::String(value) => parse_quota(value.as_str()),
                value => serde_yaml::from_value::<LimitQuota>(value).map_err(|e| e.to_string()),
            })
            .collect(),
        value => serde_yaml::from_value::<LimitQuota>(value)
            .map(|quota| vec![quota])
            .map_err(|e| e.to_string()),
    }
}

fn parse_quota_list(value: &str) -> Result<Vec<LimitQuota>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    if value.starts_with('[') {
        return serde_yaml::from_str::<Vec<LimitQuota>>(value).map_err(|e| e.to_string());
    }
    value
        .split([',', ' '])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(parse_quota)
        .collect()
}

fn parse_quota(value: &str) -> Result<LimitQuota, String> {
    let value = value.trim();
    let (limit, unit) = value.split_once('/').unwrap_or((value, "s"));
    let limit = limit
        .trim()
        .parse::<u64>()
        .map_err(|_| format!("invalid limit quota `{value}`"))?;
    Ok(LimitQuota {
        limit,
        window_seconds: window_seconds_for_unit(unit.trim())?,
    })
}

fn window_seconds_for_unit(unit: &str) -> Result<u64, String> {
    match unit.to_ascii_lowercase().as_str() {
        "s" | "sec" | "second" | "seconds" => Ok(1),
        "m" | "min" | "minute" | "minutes" => Ok(60),
        "h" | "hour" | "hours" => Ok(60 * 60),
        "d" | "day" | "days" => Ok(60 * 60 * 24),
        _ => Err(format!("unsupported limit quota unit `{unit}`")),
    }
}

fn now_epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn default_error_code() -> u16 {
    429
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
}
