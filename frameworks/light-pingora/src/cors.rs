use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use pingora::http::ResponseHeader;
use pingora::prelude::Session;
use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use url::Url;

pub const CORS_FILE: &str = "cors.yml";
pub const CORS_MODULE_ID: &str = "light-pingora/cors";
pub const CORS_CONFIG_NAME: &str = "cors";

const ORIGIN: &str = "origin";
const ACCESS_CONTROL_REQUEST_HEADERS: &str = "access-control-request-headers";
const ACCESS_CONTROL_REQUEST_METHOD: &str = "access-control-request-method";
const ACCESS_CONTROL_ALLOW_ORIGIN: &str = "access-control-allow-origin";
const ACCESS_CONTROL_ALLOW_METHODS: &str = "access-control-allow-methods";
const ACCESS_CONTROL_ALLOW_HEADERS: &str = "access-control-allow-headers";
const ACCESS_CONTROL_ALLOW_CREDENTIALS: &str = "access-control-allow-credentials";
const ACCESS_CONTROL_MAX_AGE: &str = "access-control-max-age";
const VARY: &str = "vary";
const DEFAULT_ALLOWED_HEADERS: &str = "Content-Type, WWW-Authenticate, Authorization";
const PREFLIGHT_MAX_AGE_SECONDS: u64 = 3600;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorsConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_origins: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_methods: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_path_prefix_allowed")]
    pub path_prefix_allowed: BTreeMap<String, CorsPathPrefix>,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allowed_origins: Vec::new(),
            allowed_methods: Vec::new(),
            path_prefix_allowed: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorsPathPrefix {
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_origins: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub allowed_methods: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CorsResponseHeaders {
    pub allow_origin: Option<String>,
    pub allow_methods: Vec<String>,
    pub allow_headers: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsRequestOutcome {
    Continue(Option<CorsResponseHeaders>),
    Respond {
        status: u16,
        headers: CorsResponseHeaders,
    },
}

pub fn load_cors_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<CorsConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<CorsConfig>(runtime_config, CORS_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == CORS_FILE => CorsConfig::default(),
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        CORS_MODULE_ID,
        CORS_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn evaluate_cors_request(
    session: &Session,
    config: &CorsConfig,
    request_path: &str,
    server_scheme: &str,
    server_port: u16,
) -> CorsRequestOutcome {
    if !config.enabled || !is_cors_request(session) {
        return CorsRequestOutcome::Continue(None);
    }

    let policy = config.policy_for_path(request_path);
    let origin = first_request_header(session, ORIGIN);
    let allow_origin = origin.as_deref().and_then(|origin| {
        match_origin(
            origin,
            &policy.allowed_origins,
            session,
            server_scheme,
            server_port,
        )
    });

    if origin.is_some() && allow_origin.is_none() {
        return CorsRequestOutcome::Respond {
            status: 403,
            headers: CorsResponseHeaders::default(),
        };
    }

    let headers = CorsResponseHeaders {
        allow_origin,
        allow_methods: policy.allowed_methods.clone(),
        allow_headers: first_request_header(session, ACCESS_CONTROL_REQUEST_HEADERS)
            .unwrap_or_else(|| DEFAULT_ALLOWED_HEADERS.to_string()),
    };

    if is_preflight_request(session) {
        CorsRequestOutcome::Respond {
            status: 200,
            headers,
        }
    } else {
        CorsRequestOutcome::Continue(Some(headers))
    }
}

pub fn apply_cors_response(
    response: &mut ResponseHeader,
    headers: &CorsResponseHeaders,
) -> pingora::Result<()> {
    if let Some(origin) = headers.allow_origin.as_deref() {
        response.insert_header(ACCESS_CONTROL_ALLOW_ORIGIN, origin)?;
        response.insert_header(
            VARY,
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        )?;
    }
    if !headers.allow_methods.is_empty() {
        response.insert_header(
            ACCESS_CONTROL_ALLOW_METHODS,
            headers.allow_methods.join(", "),
        )?;
    }
    response.insert_header(ACCESS_CONTROL_ALLOW_HEADERS, headers.allow_headers.as_str())?;
    response.insert_header(ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")?;
    response.insert_header(
        ACCESS_CONTROL_MAX_AGE,
        PREFLIGHT_MAX_AGE_SECONDS.to_string(),
    )?;
    Ok(())
}

impl CorsConfig {
    fn policy_for_path(&self, request_path: &str) -> CorsPathPrefix {
        self.path_prefix_allowed
            .iter()
            .filter(|(prefix, _)| request_path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, policy)| policy.clone())
            .unwrap_or_else(|| CorsPathPrefix {
                allowed_origins: self.allowed_origins.clone(),
                allowed_methods: self.allowed_methods.clone(),
            })
    }
}

fn is_cors_request(session: &Session) -> bool {
    let headers = &session.req_header().headers;
    headers.contains_key(ORIGIN)
        || headers.contains_key(ACCESS_CONTROL_REQUEST_HEADERS)
        || headers.contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

fn is_preflight_request(session: &Session) -> bool {
    session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("OPTIONS")
}

fn match_origin(
    origin: &str,
    allowed_origins: &[String],
    session: &Session,
    server_scheme: &str,
    server_port: u16,
) -> Option<String> {
    let normalized_origin = sanitize_default_port(origin);
    if allowed_origins.iter().any(|allowed| {
        sanitize_default_port(allowed).eq_ignore_ascii_case(normalized_origin.as_str())
    }) {
        return Some(origin.to_string());
    }

    let default_origin = default_origin(session, server_scheme, server_port)?;
    sanitize_default_port(&default_origin)
        .eq_ignore_ascii_case(normalized_origin.as_str())
        .then(|| origin.to_string())
}

fn default_origin(session: &Session, server_scheme: &str, server_port: u16) -> Option<String> {
    let host = first_request_header(session, "host")?;
    let host = host.split(',').next().unwrap_or(host.as_str()).trim();
    if host.is_empty() {
        return None;
    }
    if host_has_port(host) || is_default_port(server_scheme, server_port) {
        Some(format!("{server_scheme}://{host}"))
    } else {
        Some(format!("{server_scheme}://{host}:{server_port}"))
    }
}

fn sanitize_default_port(origin: &str) -> String {
    let Ok(url) = Url::parse(origin) else {
        return origin.to_string();
    };
    let Some(host) = url.host_str() else {
        return origin.to_string();
    };
    let scheme = url.scheme().to_ascii_lowercase();
    let mut sanitized = format!("{scheme}://{host}");
    if let Some(port) = url.port() {
        if !is_default_port(&scheme, port) {
            sanitized.push(':');
            sanitized.push_str(port.to_string().as_str());
        }
    }
    sanitized
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    (scheme.eq_ignore_ascii_case("http") && port == 80)
        || (scheme.eq_ignore_ascii_case("https") && port == 443)
}

fn host_has_port(host: &str) -> bool {
    if host.starts_with('[') {
        return host.contains("]:");
    }
    host.rsplit_once(':')
        .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
}

fn first_request_header(session: &Session, name: &str) -> Option<String> {
    session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringListVisitor;

    impl<'de> Visitor<'de> for StringListVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a string, JSON/YAML string list, or list of strings")
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
            Ok(value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect())
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut values = Vec::new();
            while let Some(value) = seq.next_element::<String>()? {
                values.push(value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(StringListVisitor)
}

fn deserialize_path_prefix_allowed<'de, D>(
    deserializer: D,
) -> Result<BTreeMap<String, CorsPathPrefix>, D::Error>
where
    D: Deserializer<'de>,
{
    struct PathPrefixVisitor;

    impl<'de> Visitor<'de> for PathPrefixVisitor {
        type Value = BTreeMap<String, CorsPathPrefix>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
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
            if value.starts_with('{') {
                return serde_yaml::from_str::<BTreeMap<String, CorsPathPrefix>>(value)
                    .map_err(E::custom);
            }
            Err(E::custom("pathPrefixAllowed must be a map"))
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
            while let Some((key, value)) = map.next_entry::<String, CorsPathPrefix>()? {
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(PathPrefixVisitor)
}

fn default_enabled() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_config_accepts_string_and_path_prefix_values() {
        let config: CorsConfig = serde_yaml::from_str(
            r#"
allowedOrigins: https://a.example.com, https://b.example.com
allowedMethods: '["GET","POST"]'
pathPrefixAllowed:
  /v1:
    allowedOrigins:
      - https://v1.example.com
    allowedMethods: GET
  /v1/admin:
    allowedOrigins: https://admin.example.com
    allowedMethods:
      - GET
      - DELETE
"#,
        )
        .expect("parse CORS config");

        assert_eq!(config.allowed_origins.len(), 2);
        assert_eq!(config.allowed_methods, ["GET", "POST"]);
        assert_eq!(
            config.policy_for_path("/v1/admin/users").allowed_origins,
            ["https://admin.example.com"]
        );
        assert_eq!(
            config.policy_for_path("/v1/pets").allowed_origins,
            ["https://v1.example.com"]
        );
    }

    #[test]
    fn sanitize_origin_removes_only_default_ports() {
        assert_eq!(
            sanitize_default_port("https://example.com:443"),
            "https://example.com"
        );
        assert_eq!(
            sanitize_default_port("http://example.com:8080"),
            "http://example.com:8080"
        );
    }
}
