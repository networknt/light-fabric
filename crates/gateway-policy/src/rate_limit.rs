use crate::config_util::request_header;
use crate::security::{AuthPrincipal, HandlerRejection};
use crate::{HeaderWriter, RequestRead};
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
    #[serde(default, deserialize_with = "deserialize_legacy_queue_size")]
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
    /// Explicit opt-in to True-Client-IP as an unverified source-quota label.
    /// Deployments must sanitize this header at their trusted ingress; it is
    /// never an authentication credential or a verified principal identity.
    Akamai,
    /// Uses the verified client ID. An arbitrary request header cannot create
    /// new buckets when the authenticated principal has no client ID.
    #[serde(rename = "header-client-id", alias = "headerclientid")]
    HeaderClientId,
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

pub fn check_rate_limit(
    session: &impl RequestRead,
    runtime: &RateLimitRuntime,
    principal: Option<&AuthPrincipal>,
    request_path: &str,
    peer_address: Option<&str>,
) -> Result<Option<RateLimitHeaders>, HandlerRejection> {
    let config = &runtime.config;
    if !config.enabled {
        return Ok(None);
    }
    let Some((key, quotas)) =
        rate_limit_key_and_quotas(session, config, principal, request_path, peer_address)?
    else {
        return Ok(None);
    };
    if quotas.is_empty() {
        return Ok(None);
    }

    let now = now_epoch_seconds();
    let mut state = runtime
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let entries = state.entry(key).or_default();
    if let Some((quota, count)) = evaluate_counter(entries, &quotas, now, false) {
        let headers = headers_for_quota(*quota, count, entries, now, true);
        return Err(
            HandlerRejection::new(config.error_code, "ERR10077", "rate limit exceeded")
                .with_header(X_RATE_LIMIT_LIMIT, headers.limit.to_string())
                .with_header(X_RATE_LIMIT_REMAINING, headers.remaining.to_string())
                .with_header(X_RATE_LIMIT_RESET, headers.reset.to_string())
                .with_header(
                    RETRY_AFTER,
                    headers.retry_after.unwrap_or(headers.reset).to_string(),
                ),
        );
    }
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

/// A selected Lambda quota uses the same synchronized counters/expiry core.
/// Selection and Java response encoding remain in the Lambda wrapper.
pub struct SelectedQuotaDecision {
    pub allowed: bool,
    pub count: u64,
    pub reset: Option<u64>,
}
pub fn check_selected_rate_limit(
    runtime: &RateLimitRuntime,
    label: &str,
    quota: LimitQuota,
) -> SelectedQuotaDecision {
    let now = now_epoch_seconds();
    let mut state = runtime
        .state
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let entries = state.entry(format!("lambda-selected:{label}")).or_default();
    let quotas = [quota];
    if let Some((_, count)) = evaluate_counter(entries, &quotas, now, true) {
        let reset = if quota.window_seconds == 1 {
            Some(1)
        } else {
            entries.front().map(|first| {
                quota
                    .window_seconds
                    .saturating_sub(now.saturating_sub(*first))
            })
        };
        SelectedQuotaDecision {
            allowed: false,
            count,
            reset,
        }
    } else {
        SelectedQuotaDecision {
            allowed: true,
            count: entries.len() as u64,
            reset: None,
        }
    }
}
fn evaluate_counter<'a>(
    entries: &mut VecDeque<u64>,
    quotas: &'a [LimitQuota],
    now: u64,
    allow_initial_empty: bool,
) -> Option<(&'a LimitQuota, u64)> {
    let initially_empty = entries.is_empty();
    let max_window = quotas
        .iter()
        .map(|quota| quota.window_seconds)
        .max()
        .unwrap_or(1);
    while entries
        .front()
        .is_some_and(|timestamp| now.saturating_sub(*timestamp) >= max_window)
    {
        entries.pop_front();
    }
    if !(allow_initial_empty && initially_empty) {
        for quota in quotas {
            let count = entries
                .iter()
                .filter(|timestamp| now.saturating_sub(**timestamp) < quota.window_seconds)
                .count() as u64;
            if count >= quota.limit {
                return Some((quota, count));
            }
        }
    }
    entries.push_back(now);
    None
}

pub fn apply_rate_limit_headers<W: HeaderWriter>(
    response: &mut W,
    headers: &RateLimitHeaders,
) -> Result<(), W::Error> {
    response.insert_header(X_RATE_LIMIT_LIMIT, headers.limit.to_string())?;
    response.insert_header(X_RATE_LIMIT_REMAINING, headers.remaining.to_string())?;
    response.insert_header(X_RATE_LIMIT_RESET, headers.reset.to_string())?;
    if let Some(retry_after) = headers.retry_after {
        response.insert_header(RETRY_AFTER, retry_after.to_string())?;
    }
    Ok(())
}

fn rate_limit_key_and_quotas(
    session: &impl RequestRead,
    config: &LimitConfig,
    principal: Option<&AuthPrincipal>,
    request_path: &str,
    peer_address: Option<&str>,
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
                .or_else(|| peer_address.map(str::to_string))
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
        LimitKey::Akamai => {
            let address = request_header(session, "True-Client-IP")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_string());
            let quotas = direct_quotas(&config.address, &address, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| (format!("akamai:{address}"), quotas.to_vec())))
        }
        LimitKey::HeaderClientId => {
            let principal = principal.ok_or_else(|| {
                HandlerRejection::unauthorized(
                    "verified authentication is required for header client rate limiting",
                )
            })?;
            let client_id = principal.client_id.as_deref().ok_or_else(|| {
                HandlerRejection::unauthorized(
                    "verified client id is required for client rate limiting",
                )
            })?;
            let namespace = "client";
            let quotas = direct_quotas(&config.client, client_id, request_path)
                .or_else(|| non_empty(config.rate_limit.as_slice()));
            Ok(quotas.map(|quotas| (format!("{namespace}:{client_id}"), quotas.to_vec())))
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

fn deserialize_legacy_queue_size<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    match i64::deserialize(deserializer)? {
        -1 => Ok(0),
        value if value >= 0 => Ok(value as u64),
        value => Err(D::Error::custom(format!(
            "limit queueSize must be -1 or nonnegative, got {value}"
        ))),
    }
}

impl RateLimitRuntime {
    pub fn new(config: LimitConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    struct IdentityHeaders(http::HeaderMap);
    impl RequestRead for IdentityHeaders {
        fn req_header(&self) -> crate::RequestHead<'_> {
            crate::RequestHead {
                headers: &self.0,
                method: &http::Method::GET,
            }
        }
    }

    #[test]
    fn header_client_resolver_requires_verified_auth_and_prefers_verified_identity() {
        let mut headers = http::HeaderMap::new();
        headers.insert("Client-Id", "caller-label".parse().unwrap());
        let request = IdentityHeaders(headers);
        let config = LimitConfig {
            enabled: true,
            key: LimitKey::HeaderClientId,
            rate_limit: vec![LimitQuota {
                limit: 1,
                window_seconds: 3600,
            }],
            ..Default::default()
        };
        assert_eq!(
            rate_limit_key_and_quotas(&request, &config, None, "/", None)
                .unwrap_err()
                .status,
            401
        );
        let verified = AuthPrincipal {
            client_id: Some("verified-client".into()),
            ..Default::default()
        };
        let key = rate_limit_key_and_quotas(&request, &config, Some(&verified), "/", None)
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(key, "client:verified-client");
        let verified = AuthPrincipal {
            user_id: Some("verified-user".into()),
            ..Default::default()
        };
        assert_eq!(
            rate_limit_key_and_quotas(&request, &config, Some(&verified), "/", None)
                .unwrap_err()
                .status,
            401
        );
        let missing = IdentityHeaders(http::HeaderMap::new());
        assert_eq!(
            rate_limit_key_and_quotas(&missing, &config, Some(&verified), "/", None)
                .unwrap_err()
                .status,
            401
        );
    }

    #[test]
    fn akamai_source_label_handles_missing_and_repeated_case_insensitive_values() {
        let config = LimitConfig {
            key: LimitKey::Akamai,
            rate_limit: vec![LimitQuota {
                limit: 1,
                window_seconds: 60,
            }],
            ..Default::default()
        };
        let missing = IdentityHeaders(http::HeaderMap::new());
        assert_eq!(
            rate_limit_key_and_quotas(&missing, &config, None, "/", None)
                .unwrap()
                .unwrap()
                .0,
            "akamai:unknown"
        );
        let mut headers = http::HeaderMap::new();
        headers.append("tRuE-cLiEnT-iP", "192.0.2.10".parse().unwrap());
        headers.append("True-Client-IP", "192.0.2.11".parse().unwrap());
        assert_eq!(
            rate_limit_key_and_quotas(&IdentityHeaders(headers), &config, None, "/", None)
                .unwrap()
                .unwrap()
                .0,
            "akamai:192.0.2.10"
        );
        for name in ["server", "address", "client", "user"] {
            let key: LimitKey = serde_yaml::from_str(name).unwrap();
            assert_eq!(serde_json::to_value(key).unwrap(), name);
        }
    }

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
