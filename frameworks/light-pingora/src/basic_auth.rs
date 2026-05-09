use crate::config_util::request_header;
use crate::security::HandlerRejection;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use light_runtime::{MaskSpec, ModuleKind, RuntimeConfig, RuntimeError};
use pingora::prelude::Session;
use serde::de::{Error as DeError, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_yaml::Value as YamlValue;
use std::fmt;
use subtle::ConstantTimeEq;

pub const BASIC_AUTH_FILE: &str = "basic-auth.yml";
pub const BASIC_AUTH_MODULE_ID: &str = "light-pingora/basic-auth";
pub const BASIC_AUTH_CONFIG_NAME: &str = "basic-auth";

const ANONYMOUS_USER: &str = "anonymous";
const BEARER_USER: &str = "bearer";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BasicAuthConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub enable_ad: bool,
    #[serde(default)]
    pub allow_anonymous: bool,
    #[serde(default)]
    pub allow_bearer_token: bool,
    #[serde(default, deserialize_with = "deserialize_users")]
    pub users: Vec<UserAuth>,
}

impl Default for BasicAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            enable_ad: true,
            allow_anonymous: false,
            allow_bearer_token: false,
            users: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserAuth {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub paths: Vec<String>,
}

pub fn load_basic_auth_config(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<BasicAuthConfig>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let config = match runtime_config
        .module_registry
        .load_config::<BasicAuthConfig>(runtime_config, BASIC_AUTH_FILE)
    {
        Ok(config) => config,
        Err(RuntimeError::MissingConfig(file)) if file == BASIC_AUTH_FILE => {
            BasicAuthConfig::default()
        }
        Err(error) => return Err(error),
    };

    runtime_config.module_registry.register_loaded_config(
        BASIC_AUTH_MODULE_ID,
        BASIC_AUTH_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [MaskSpec::key("password")],
        config.enabled,
        Some(config.enabled),
        true,
    )?;

    Ok(config.enabled.then_some(config))
}

pub fn verify_basic_auth(
    session: &Session,
    config: &BasicAuthConfig,
    request_path: &str,
) -> Result<(), HandlerRejection> {
    if !config.enabled {
        return Ok(());
    }

    let Some(authorization) = request_header(session, "authorization") else {
        return verify_anonymous(config, request_path);
    };
    let (scheme, credentials) = authorization
        .split_once(' ')
        .ok_or_else(|| basic_rejection("invalid Authorization header"))?;

    if scheme.eq_ignore_ascii_case("bearer") {
        if config.allow_bearer_token && user_can_access(config, BEARER_USER, request_path) {
            return Ok(());
        }
        return Err(basic_rejection("bearer token is not allowed for this path"));
    }

    if !scheme.eq_ignore_ascii_case("basic") {
        return Err(basic_rejection("unsupported Authorization scheme"));
    }

    let decoded = STANDARD
        .decode(credentials.trim())
        .map_err(|_| basic_rejection("invalid Basic credentials"))?;
    let decoded =
        String::from_utf8(decoded).map_err(|_| basic_rejection("invalid Basic credentials"))?;
    let (username, password) = decoded
        .split_once(':')
        .ok_or_else(|| basic_rejection("invalid Basic credentials"))?;
    let Some(user) = config.users.iter().find(|user| user.username == username) else {
        return Err(basic_rejection("unknown Basic user"));
    };
    let password_matches: bool = password.as_bytes().ct_eq(user.password.as_bytes()).into();
    if !password_matches {
        return Err(basic_rejection("invalid Basic password"));
    }
    if !paths_allow(user.paths.as_slice(), request_path) {
        return Err(HandlerRejection::forbidden(
            "Basic user is not allowed for this path",
        ));
    }
    Ok(())
}

fn verify_anonymous(config: &BasicAuthConfig, request_path: &str) -> Result<(), HandlerRejection> {
    if config.allow_anonymous && user_can_access(config, ANONYMOUS_USER, request_path) {
        Ok(())
    } else {
        Err(basic_rejection("Authorization header is required"))
    }
}

fn user_can_access(config: &BasicAuthConfig, username: &str, request_path: &str) -> bool {
    config
        .users
        .iter()
        .find(|user| user.username == username)
        .map(|user| paths_allow(user.paths.as_slice(), request_path))
        .unwrap_or(false)
}

fn paths_allow(paths: &[String], request_path: &str) -> bool {
    paths.is_empty()
        || paths
            .iter()
            .any(|path| request_path.starts_with(path.as_str()))
}

fn basic_rejection(message: impl Into<String>) -> HandlerRejection {
    HandlerRejection::new(401, "ERR10005", message)
        .with_header("www-authenticate", "Basic realm=\"light-gateway\"")
}

fn deserialize_users<'de, D>(deserializer: D) -> Result<Vec<UserAuth>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UsersVisitor;

    impl<'de> Visitor<'de> for UsersVisitor {
        type Value = Vec<UserAuth>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a user list, map, or JSON/YAML string")
        }

        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: DeError,
        {
            parse_users_value(serde_yaml::from_str::<YamlValue>(value).map_err(E::custom)?)
                .map_err(E::custom)
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
            let mut users = Vec::new();
            while let Some(user) = seq.next_element::<UserAuth>()? {
                users.push(user);
            }
            Ok(users)
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut values = serde_yaml::Mapping::new();
            while let Some((key, value)) = map.next_entry::<String, YamlValue>()? {
                values.insert(YamlValue::String(key), value);
            }
            parse_users_value(YamlValue::Mapping(values)).map_err(A::Error::custom)
        }
    }

    deserializer.deserialize_any(UsersVisitor)
}

fn parse_users_value(value: YamlValue) -> Result<Vec<UserAuth>, String> {
    match value {
        YamlValue::Null => Ok(Vec::new()),
        YamlValue::Sequence(values) => values
            .into_iter()
            .map(|value| serde_yaml::from_value::<UserAuth>(value).map_err(|e| e.to_string()))
            .collect(),
        YamlValue::Mapping(map) => {
            let mut users = Vec::new();
            for (key, value) in map {
                let username = key
                    .as_str()
                    .ok_or_else(|| "basic-auth user key must be a string".to_string())?
                    .to_string();
                let mut user = match value {
                    YamlValue::String(password) => UserAuth {
                        username: username.clone(),
                        password,
                        paths: Vec::new(),
                    },
                    value => {
                        serde_yaml::from_value::<UserAuth>(value).map_err(|e| e.to_string())?
                    }
                };
                if user.username.is_empty() {
                    user.username = username;
                }
                users.push(user);
            }
            Ok(users)
        }
        YamlValue::String(value) if value.trim().is_empty() => Ok(Vec::new()),
        other => Err(format!("unsupported basic-auth users value: {other:?}")),
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_users_accept_map_and_list_forms() {
        let map_config: BasicAuthConfig = serde_yaml::from_str(
            r#"
enabled: true
users:
  alice:
    password: secret
    paths:
      - /api
  anonymous:
    paths:
      - /public
"#,
        )
        .expect("parse map config");
        assert_eq!(map_config.users[0].username, "alice");
        assert_eq!(map_config.users[0].paths, ["/api"]);

        let list_config: BasicAuthConfig = serde_yaml::from_str(
            r#"
enabled: true
users: '[{"username":"bob","password":"secret","paths":["/v1"]}]'
"#,
        )
        .expect("parse list config");
        assert_eq!(list_config.users[0].username, "bob");
    }

    #[test]
    fn paths_default_to_all() {
        assert!(paths_allow(&[], "/anything"));
        assert!(paths_allow(&["/api".to_string()], "/api/pets"));
        assert!(!paths_allow(&["/api".to_string()], "/admin"));
    }
}
