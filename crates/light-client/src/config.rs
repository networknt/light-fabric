use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientConfig {
    pub tls: ClientTlsConfig,
    pub request: ClientRequestConfig,
    pub oauth: ClientOauthConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub path_prefix_services: BTreeMap<String, String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            tls: ClientTlsConfig::default(),
            request: ClientRequestConfig::default(),
            oauth: ClientOauthConfig::default(),
            path_prefix_services: BTreeMap::new(),
        }
    }
}

impl<'de> Deserialize<'de> for ClientConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Default, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct RawClientConfig {
            #[serde(default)]
            tls: Option<RawClientTlsConfig>,
            #[serde(default)]
            request: ClientRequestConfig,
            #[serde(default)]
            oauth: ClientOauthConfig,
            #[serde(default, deserialize_with = "deserialize_string_map")]
            path_prefix_services: BTreeMap<String, String>,
            #[serde(default)]
            verify_hostname: Option<bool>,
        }

        let raw = Option::<RawClientConfig>::deserialize(deserializer)?.unwrap_or_default();
        let top_level_verify_hostname = raw.verify_hostname;
        let tls = ClientTlsConfig::from_raw(raw.tls, top_level_verify_hostname);
        if top_level_verify_hostname.is_some() {
            tracing::warn!(
                "`verifyHostname` at the root of client.yml is deprecated; use `tls.verifyHostname`"
            );
        }
        Ok(Self {
            tls,
            request: raw.request,
            oauth: raw.oauth,
            path_prefix_services: raw.path_prefix_services,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientTlsConfig {
    pub verify_hostname: bool,
    pub accept_invalid_certs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_version: Option<TlsVersion>,
}

impl Default for ClientTlsConfig {
    fn default() -> Self {
        Self {
            verify_hostname: true,
            accept_invalid_certs: false,
            ca_cert_path: None,
            client_cert_path: None,
            client_key_path: None,
            tls_version: None,
        }
    }
}

impl<'de> Deserialize<'de> for ClientTlsConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        RawClientTlsConfig::deserialize(deserializer).map(|raw| Self::from_raw(Some(raw), None))
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawClientTlsConfig {
    #[serde(default)]
    verify_hostname: Option<bool>,
    #[serde(default)]
    accept_invalid_certs: Option<bool>,
    #[serde(default)]
    ca_cert_path: Option<PathBuf>,
    #[serde(default)]
    client_cert_path: Option<PathBuf>,
    #[serde(default)]
    client_key_path: Option<PathBuf>,
    #[serde(default)]
    tls_version: Option<TlsVersion>,
}

impl ClientTlsConfig {
    fn from_raw(raw: Option<RawClientTlsConfig>, top_level_verify_hostname: Option<bool>) -> Self {
        let Some(raw) = raw else {
            return Self {
                verify_hostname: top_level_verify_hostname.unwrap_or(true),
                ..Self::default()
            };
        };

        Self {
            verify_hostname: raw
                .verify_hostname
                .or(top_level_verify_hostname)
                .unwrap_or(true),
            accept_invalid_certs: raw.accept_invalid_certs.unwrap_or(false),
            ca_cert_path: raw.ca_cert_path,
            client_cert_path: raw.client_cert_path,
            client_key_path: raw.client_key_path,
            tls_version: raw.tls_version,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TlsVersion {
    #[serde(rename = "TLSv1.2", alias = "TLSv1_2")]
    TlsV1_2,
    #[serde(rename = "TLSv1.3", alias = "TLSv1_3")]
    TlsV1_3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientRequestConfig {
    #[serde(default = "default_error_threshold")]
    pub error_threshold: u32,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    #[serde(default = "default_reset_timeout")]
    pub reset_timeout: u64,
    #[serde(default)]
    pub inject_open_tracing: bool,
    #[serde(default)]
    pub inject_caller_id: bool,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default = "default_connection_pool_size")]
    pub connection_pool_size: u32,
    #[serde(default = "default_connection_expire_time")]
    pub connection_expire_time: u64,
    #[serde(default = "default_max_req_per_conn")]
    pub max_req_per_conn: u32,
    #[serde(default = "default_max_connection_num_per_host")]
    pub max_connection_num_per_host: u32,
    #[serde(default = "default_min_connection_num_per_host")]
    pub min_connection_num_per_host: u32,
    #[serde(default = "default_max_request_retry")]
    pub max_request_retry: u32,
    #[serde(default = "default_request_retry_delay")]
    pub request_retry_delay: u64,
    #[serde(default)]
    pub pool_metrics_enabled: bool,
    #[serde(default)]
    pub pool_warm_up_enabled: bool,
    #[serde(default = "default_pool_warm_up_size")]
    pub pool_warm_up_size: u32,
    #[serde(default = "default_true")]
    pub health_check_enabled: bool,
    #[serde(default = "default_health_check_interval_ms")]
    pub health_check_interval_ms: u64,
}

impl Default for ClientRequestConfig {
    fn default() -> Self {
        Self {
            error_threshold: default_error_threshold(),
            connect_timeout: default_connect_timeout(),
            timeout: default_timeout(),
            reset_timeout: default_reset_timeout(),
            inject_open_tracing: false,
            inject_caller_id: false,
            enable_http2: true,
            connection_pool_size: default_connection_pool_size(),
            connection_expire_time: default_connection_expire_time(),
            max_req_per_conn: default_max_req_per_conn(),
            max_connection_num_per_host: default_max_connection_num_per_host(),
            min_connection_num_per_host: default_min_connection_num_per_host(),
            max_request_retry: default_max_request_retry(),
            request_retry_delay: default_request_retry_delay(),
            pool_metrics_enabled: false,
            pool_warm_up_enabled: false,
            pool_warm_up_size: default_pool_warm_up_size(),
            health_check_enabled: true,
            health_check_interval_ms: default_health_check_interval_ms(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientOauthConfig {
    #[serde(default)]
    pub multiple_auth_servers: bool,
    #[serde(default)]
    pub token: OAuthTokenConfig,
    #[serde(default)]
    pub sign: OAuthSignConfig,
    #[serde(default)]
    pub deref: OAuthDerefConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenConfig {
    #[serde(default)]
    pub cache: OAuthTokenCacheConfig,
    #[serde(default = "default_token_renew_before_expired")]
    pub token_renew_before_expired: u64,
    #[serde(default = "default_expired_refresh_retry_delay")]
    pub expired_refresh_retry_delay: u64,
    #[serde(default = "default_early_refresh_retry_delay")]
    pub early_refresh_retry_delay: u64,
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default, rename = "authorization_code", alias = "authorizationCode")]
    pub authorization_code: OAuthTokenAuthorizationCodeConfig,
    #[serde(default, rename = "client_credentials", alias = "clientCredentials")]
    pub client_credentials: OAuthClientCredentialsConfig,
    #[serde(default, rename = "refresh_token", alias = "refreshToken")]
    pub refresh_token: OAuthTokenRefreshTokenConfig,
    #[serde(default, rename = "token_exchange", alias = "tokenExchange")]
    pub token_exchange: OAuthTokenExchangeConfig,
    #[serde(default)]
    pub key: OAuthKeyConfig,
}

impl Default for OAuthTokenConfig {
    fn default() -> Self {
        Self {
            cache: OAuthTokenCacheConfig::default(),
            token_renew_before_expired: default_token_renew_before_expired(),
            expired_refresh_retry_delay: default_expired_refresh_retry_delay(),
            early_refresh_retry_delay: default_early_refresh_retry_delay(),
            server_url: None,
            service_id: None,
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
            authorization_code: OAuthTokenAuthorizationCodeConfig::default(),
            client_credentials: OAuthClientCredentialsConfig::default(),
            refresh_token: OAuthTokenRefreshTokenConfig::default(),
            token_exchange: OAuthTokenExchangeConfig::default(),
            key: OAuthKeyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenCacheConfig {
    #[serde(default = "default_cache_capacity")]
    pub capacity: usize,
}

impl Default for OAuthTokenCacheConfig {
    fn default() -> Self {
        Self {
            capacity: default_cache_capacity(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenAuthorizationCodeConfig {
    #[serde(default = "default_token_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default, rename = "redirect_uri", alias = "redirectUri")]
    pub redirect_uri: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
}

impl Default for OAuthTokenAuthorizationCodeConfig {
    fn default() -> Self {
        Self {
            uri: default_token_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: None,
            scope: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthClientCredentialsConfig {
    #[serde(default = "default_token_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub service_id_auth_servers: BTreeMap<String, AuthServerConfig>,
}

impl Default for OAuthClientCredentialsConfig {
    fn default() -> Self {
        Self {
            uri: default_token_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: Vec::new(),
            service_id_auth_servers: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenRefreshTokenConfig {
    #[serde(default = "default_token_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
}

impl Default for OAuthTokenRefreshTokenConfig {
    fn default() -> Self {
        Self {
            uri: default_token_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokenExchangeConfig {
    #[serde(default = "default_token_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
    #[serde(default, alias = "subject_token")]
    pub subject_token: Option<String>,
    #[serde(default, alias = "subject_token_type")]
    pub subject_token_type: Option<String>,
    #[serde(default, alias = "requested_token_type")]
    pub requested_token_type: Option<String>,
    #[serde(default)]
    pub audience: Option<String>,
}

impl Default for OAuthTokenExchangeConfig {
    fn default() -> Self {
        Self {
            uri: default_token_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            scope: Vec::new(),
            subject_token: None,
            subject_token_type: None,
            requested_token_type: None,
            audience: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthKeyConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default = "default_key_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub service_id_auth_servers: BTreeMap<String, AuthServerConfig>,
    #[serde(default)]
    pub audience: Option<String>,
}

impl Default for OAuthKeyConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            service_id: None,
            uri: default_key_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            enable_http2: true,
            service_id_auth_servers: BTreeMap::new(),
            audience: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSignConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default = "default_sign_uri")]
    pub uri: String,
    #[serde(default = "default_sign_timeout")]
    pub timeout: u64,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default)]
    pub key: OAuthSignKeyConfig,
}

impl Default for OAuthSignConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            service_id: None,
            uri: default_sign_uri(),
            timeout: default_sign_timeout(),
            client_id: String::new(),
            client_secret: String::new(),
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
            key: OAuthSignKeyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSignKeyConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default = "default_key_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
    #[serde(default)]
    pub audience: Option<String>,
}

impl Default for OAuthSignKeyConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            service_id: None,
            uri: default_key_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            enable_http2: true,
            audience: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthDerefConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default = "default_deref_uri")]
    pub uri: String,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: String,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: String,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default = "default_true")]
    pub enable_http2: bool,
}

impl Default for OAuthDerefConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            service_id: None,
            uri: default_deref_uri(),
            client_id: String::new(),
            client_secret: String::new(),
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthServerConfig {
    #[serde(default, rename = "server_url", alias = "serverUrl")]
    pub server_url: Option<String>,
    #[serde(default)]
    pub service_id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    #[serde(default, rename = "client_id", alias = "clientId")]
    pub client_id: Option<String>,
    #[serde(default, rename = "client_secret", alias = "clientSecret")]
    pub client_secret: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub scope: Vec<String>,
    #[serde(default)]
    pub audience: Option<String>,
    #[serde(default)]
    pub proxy_host: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_u16")]
    pub proxy_port: Option<u16>,
    #[serde(default)]
    pub enable_http2: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub token_renew_before_expired: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub expired_refresh_retry_delay: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_optional_u64")]
    pub early_refresh_retry_delay: Option<u64>,
}

fn default_true() -> bool {
    true
}

fn default_error_threshold() -> u32 {
    2
}

fn default_connect_timeout() -> u64 {
    2_000
}

fn default_timeout() -> u64 {
    3_000
}

fn default_reset_timeout() -> u64 {
    7_000
}

fn default_connection_pool_size() -> u32 {
    1_000
}

fn default_connection_expire_time() -> u64 {
    1_800_000
}

fn default_max_req_per_conn() -> u32 {
    1_000_000
}

fn default_max_connection_num_per_host() -> u32 {
    1_000
}

fn default_min_connection_num_per_host() -> u32 {
    250
}

fn default_max_request_retry() -> u32 {
    3
}

fn default_request_retry_delay() -> u64 {
    1_000
}

fn default_pool_warm_up_size() -> u32 {
    1
}

fn default_health_check_interval_ms() -> u64 {
    30_000
}

fn default_cache_capacity() -> usize {
    200
}

fn default_token_renew_before_expired() -> u64 {
    60_000
}

fn default_expired_refresh_retry_delay() -> u64 {
    2_000
}

fn default_early_refresh_retry_delay() -> u64 {
    4_000
}

fn default_token_uri() -> String {
    "/oauth2/token".to_string()
}

fn default_key_uri() -> String {
    "/oauth2/key".to_string()
}

fn default_sign_uri() -> String {
    "/oauth2/sign".to_string()
}

fn default_deref_uri() -> String {
    "/oauth2/deref".to_string()
}

fn default_sign_timeout() -> u64 {
    2_000
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
            Ok(parse_string_list(value))
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(Vec::new())
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

    deserializer.deserialize_any(StringListVisitor)
}

fn deserialize_string_map<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringMapVisitor;

    impl<'de> Visitor<'de> for StringMapVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map, JSON/YAML string map, or key=value list")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            parse_string_map(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(BTreeMap::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(BTreeMap::new())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, serde_yaml::Value>()? {
                values.insert(key, yaml_scalar_to_string(value).map_err(A::Error::custom)?);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(StringMapVisitor)
}

fn deserialize_typed_map<'de, D, T>(deserializer: D) -> Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct TypedMapVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T> Visitor<'de> for TypedMapVisitor<T>
    where
        T: serde::de::DeserializeOwned,
    {
        type Value = BTreeMap<String, T>;

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
            serde_yaml::from_str::<BTreeMap<String, T>>(value).map_err(E::custom)
        }

        fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            self.visit_str(value.as_str())
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(BTreeMap::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(BTreeMap::new())
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, T>()? {
                values.insert(key, value);
            }
            Ok(values)
        }
    }

    deserializer.deserialize_any(TypedMapVisitor(std::marker::PhantomData))
}

fn deserialize_optional_u16<'de, D>(deserializer: D) -> Result<Option<u16>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_number(deserializer, "u16", |value| {
        u16::try_from(value).map_err(|_| format!("value {value} is outside u16 range"))
    })
}

fn deserialize_optional_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_optional_number(deserializer, "u64", Ok)
}

fn deserialize_optional_number<'de, D, T, F>(
    deserializer: D,
    label: &'static str,
    convert: F,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    F: Fn(u64) -> Result<T, String>,
{
    struct OptionalNumberVisitor<T, F> {
        label: &'static str,
        convert: F,
        _marker: std::marker::PhantomData<T>,
    }

    impl<'de, T, F> Visitor<'de> for OptionalNumberVisitor<T, F>
    where
        F: Fn(u64) -> Result<T, String>,
    {
        type Value = Option<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "an optional {}", self.label)
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            Ok(None)
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            let value = value.parse::<u64>().map_err(E::custom)?;
            (self.convert)(value).map(Some).map_err(E::custom)
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
            (self.convert)(value).map(Some).map_err(E::custom)
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            let value = u64::try_from(value).map_err(E::custom)?;
            (self.convert)(value).map(Some).map_err(E::custom)
        }
    }

    deserializer.deserialize_any(OptionalNumberVisitor {
        label,
        convert,
        _marker: std::marker::PhantomData,
    })
}

fn parse_string_list(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with('[') {
        return serde_yaml::from_str::<Vec<String>>(value).unwrap_or_default();
    }
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_string_map(value: &str) -> Result<BTreeMap<String, String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(BTreeMap::new());
    }
    if value.starts_with('{') {
        let value = serde_yaml::from_str::<serde_yaml::Value>(value).map_err(|e| e.to_string())?;
        return parse_yaml_string_map(value);
    }

    let mut map = BTreeMap::new();
    for entry in value.split([',', '&']) {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let (key, value) = entry
            .split_once('=')
            .or_else(|| entry.split_once(':'))
            .ok_or_else(|| format!("invalid key/value entry `{entry}`"))?;
        let key = key.trim();
        if key.is_empty() {
            return Err("map key must not be empty".to_string());
        }
        map.insert(key.to_string(), value.trim().to_string());
    }
    Ok(map)
}

fn parse_yaml_string_map(value: serde_yaml::Value) -> Result<BTreeMap<String, String>, String> {
    match value {
        serde_yaml::Value::Mapping(mapping) => {
            let mut values = BTreeMap::new();
            for (key, value) in mapping {
                let key = key
                    .as_str()
                    .ok_or_else(|| "map key must be a string".to_string())?
                    .to_string();
                values.insert(key, yaml_scalar_to_string(value)?);
            }
            Ok(values)
        }
        serde_yaml::Value::Null => Ok(BTreeMap::new()),
        other => Err(format!("expected map value, got {other:?}")),
    }
}

fn yaml_scalar_to_string(value: serde_yaml::Value) -> Result<String, String> {
    match value {
        serde_yaml::Value::String(value) => Ok(value),
        serde_yaml::Value::Number(value) => Ok(value.to_string()),
        serde_yaml::Value::Bool(value) => Ok(value.to_string()),
        serde_yaml::Value::Null => Ok(String::new()),
        other => Err(format!("expected scalar value, got {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_tls_verify_hostname() {
        let config: ClientConfig = serde_yaml::from_str(
            r#"
tls:
  verifyHostname: false
"#,
        )
        .expect("client config");

        assert!(!config.tls.verify_hostname);
    }

    #[test]
    fn parses_nested_tls_accept_invalid_certs() {
        let config: ClientConfig = serde_yaml::from_str(
            r#"
tls:
  acceptInvalidCerts: true
"#,
        )
        .expect("client config");

        assert!(config.tls.accept_invalid_certs);
    }

    #[test]
    fn keeps_top_level_verify_hostname_as_fallback() {
        let config: ClientConfig =
            serde_yaml::from_str("verifyHostname: false\n").expect("client config");

        assert!(!config.tls.verify_hostname);
        assert!(!config.tls.accept_invalid_certs);
    }

    #[test]
    fn nested_verify_hostname_wins_over_top_level_fallback() {
        let config: ClientConfig = serde_yaml::from_str(
            r#"
verifyHostname: false
tls:
  verifyHostname: true
"#,
        )
        .expect("client config");

        assert!(config.tls.verify_hostname);
    }

    #[test]
    fn blank_config_uses_defaults() {
        let config: ClientConfig = serde_yaml::from_str("").expect("client config");

        assert!(config.tls.verify_hostname);
        assert!(!config.tls.accept_invalid_certs);
        assert!(config.path_prefix_services.is_empty());
    }

    #[test]
    fn parses_java_compatible_oauth_sections() {
        let config: ClientConfig = serde_yaml::from_str(
            r#"
tls:
  verifyHostname: true
  tlsVersion: TLSv1.3
oauth:
  multipleAuthServers: true
  token:
    client_credentials:
      client_id: cc-client
      client_secret: cc-secret
      scope:
        - pet.r
      serviceIdAuthServers:
        com.networknt.petstore-1.0.0:
          server_url: https://oauth.example.com
          client_id: pet-client
          client_secret: pet-secret
          scope: pet.r,pet.w
    key:
      serviceIdAuthServers: '{"com.networknt.petstore-1.0.0":{"server_url":"https://key.example.com","audience":"petstore"}}'
      audience: gateway
    token_exchange:
      subject_token: jwt-subject
      subject_token_type: urn:ietf:params:oauth:token-type:jwt
      requested_token_type: urn:ietf:params:oauth:token-type:access_token
  sign:
    key:
      audience: signing
  deref:
    uri: /oauth2/deref
pathPrefixServices:
  /v1/pets: com.networknt.petstore-1.0.0
"#,
        )
        .expect("client config");

        assert!(config.oauth.multiple_auth_servers);
        assert_eq!(config.tls.tls_version, Some(TlsVersion::TlsV1_3));
        assert_eq!(
            config.path_prefix_services["/v1/pets"],
            "com.networknt.petstore-1.0.0"
        );
        assert_eq!(
            config
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers["com.networknt.petstore-1.0.0"]
                .scope,
            vec!["pet.r".to_string(), "pet.w".to_string()]
        );
        assert_eq!(
            config.oauth.token.key.service_id_auth_servers["com.networknt.petstore-1.0.0"]
                .audience
                .as_deref(),
            Some("petstore")
        );
        assert_eq!(
            config.oauth.token.token_exchange.subject_token.as_deref(),
            Some("jwt-subject")
        );
        assert_eq!(
            config
                .oauth
                .token
                .token_exchange
                .requested_token_type
                .as_deref(),
            Some("urn:ietf:params:oauth:token-type:access_token")
        );
        assert_eq!(config.oauth.sign.key.audience.as_deref(), Some("signing"));
    }

    #[test]
    fn parses_config_server_string_maps_and_empty_defaults() {
        let config: ClientConfig = serde_yaml::from_str(
            r#"
pathPrefixServices: "/v1/pets=com.networknt.petstore-1.0.0"
oauth:
  token:
    client_credentials:
      serviceIdAuthServers: ""
"#,
        )
        .expect("client config");

        assert_eq!(
            config.path_prefix_services["/v1/pets"],
            "com.networknt.petstore-1.0.0"
        );
        assert!(
            config
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers
                .is_empty()
        );
    }
}
