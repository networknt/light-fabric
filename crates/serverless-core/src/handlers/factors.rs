//! Lambda-specific Java selection/status adapters over shared credential primitives.
//! No provider types, JWT implementation, client caches, or limiter state.
use crate::{
    compat::Compatibility,
    handlers::jwt::header,
    plan::*,
    unsupported::{boolean, object, strings},
    *,
};
use async_trait::async_trait;
use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use gateway_policy::{
    RequestHead, RequestRead,
    apikey::{ApiKeyConfig, ApiKeyRule, verify_api_key},
    basic_auth::{BasicAuthConfig, UserAuth, password_matches, paths_allow},
};
use http::{HeaderMap, header::HeaderName};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, LazyLock},
};
use tokio::sync::Semaphore;

fn structured(value: Option<&Value>) -> Result<Value, InitError> {
    match value {
        None | Some(Value::Null) => Ok(Value::Null),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(Value::Null),
        Some(Value::String(s)) => serde_yaml::from_str(s)
            .map_err(|_| InitError::Malformed("authentication: invalid collection")),
        Some(v) => Ok(v.clone()),
    }
}
fn string(value: Option<&Value>) -> Result<String, InitError> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(InitError::Malformed("authentication: string required")),
    }
}
fn keys(value: &Value, allowed: &[&str]) -> Result<(), InitError> {
    if object(value, "authentication: object required")?
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        return Err(InitError::Malformed("authentication: unknown key"));
    }
    Ok(())
}
fn denied_path(path: &str, user: &str) -> StatusError {
    let path = path.replace(['\r', '\n'], " ");
    StatusError::new(
        401,
        "ERR10071",
        "NOT_AUTHORIZED_REQUEST_PATH",
        &format!("The request path {path} is not authorized for user {user}."),
    )
}
fn invalid_basic(scheme: &str) -> StatusError {
    StatusError::new(
        401,
        "ERR10046",
        "INVALID_BASIC_HEADER",
        &format!("Invalid basic authentication header {scheme}"),
    )
}
pub(crate) fn invalid_scheme(scheme: &str) -> StatusError {
    StatusError::new(
        401,
        "ERR12003",
        "INVALID_AUTHORIZATION_HEADER",
        &format!("Invalid authorization header {scheme}."),
    )
}
fn invalid_credentials() -> StatusError {
    StatusError::new(
        401,
        "ERR10047",
        "INVALID_USERNAME_OR_PASSWORD",
        "Invalid username or password for basic authentication",
    )
}
fn path_allowed(user: &UserAuth, path: &str) -> bool {
    // Shared Gateway treats an empty path list as all paths. Java Lambda does not.
    !user.paths.is_empty() && paths_allow(&user.paths, path)
}

#[derive(Clone)]
pub struct BasicHandler {
    config: Arc<BasicAuthConfig>,
}
impl BasicHandler {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        keys(
            value,
            &[
                "enabled",
                "enableAD",
                "allowAnonymous",
                "allowBearerToken",
                "users",
            ],
        )?;
        let map = object(value, "basic-auth.yml: object required")?;
        if boolean(map, "enableAD", false)? {
            return Err(InitError::Malformed("basic-auth.yml: LDAP deferred"));
        }
        let mut users = BTreeMap::new();
        let raw = structured(map.get("users"))?;
        let mut add = |key: Option<&str>, raw: &Value| -> Result<(), InitError> {
            keys(raw, &["username", "password", "paths"])?;
            let username = match key {
                Some(key) => key.to_owned(),
                None => string(raw.get("username"))?,
            };
            if username.is_empty() {
                return Err(InitError::Malformed("basic-auth.yml: username required"));
            }
            let paths = strings(raw.get("paths"))?;
            if paths.iter().any(|p| !p.starts_with('/')) {
                return Err(InitError::Malformed("basic-auth.yml: invalid user path"));
            }
            users.insert(
                username.clone(),
                UserAuth {
                    username,
                    password: string(raw.get("password"))?,
                    paths,
                },
            );
            Ok(())
        };
        match raw {
            Value::Null => {}
            Value::Array(entries) => {
                for entry in entries {
                    add(None, &entry)?;
                }
            }
            Value::Object(entries) => {
                for (username, entry) in entries {
                    add(Some(&username), &entry)?;
                }
            }
            _ => {
                return Err(InitError::Malformed(
                    "basic-auth.yml: users collection required",
                ));
            }
        }
        Ok(Self {
            config: Arc::new(BasicAuthConfig {
                enabled: boolean(map, "enabled", false)?,
                enable_ad: false,
                allow_anonymous: boolean(map, "allowAnonymous", false)?,
                allow_bearer_token: boolean(map, "allowBearerToken", false)?,
                users: users.into_values().collect(),
            }),
        })
    }
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }
    pub(crate) fn authenticate_basic(
        &self,
        path: &str,
        authorization: &str,
    ) -> Result<(), StatusError> {
        if !self.enabled() {
            return Err(StatusError::handler_failure());
        }
        let Some((scheme, credentials)) = authorization.split_once(' ') else {
            return Err(invalid_scheme("BASIC"));
        };
        if !scheme.eq_ignore_ascii_case("basic") || credentials.trim().is_empty() {
            return Err(invalid_scheme("BASIC"));
        }
        let decoded = if credentials.contains(':') {
            credentials.to_owned()
        } else {
            // Apache Commons Base64 accepts missing padding, URL alphabet and
            // nonalphabet bytes. Adapt decoding locally; Gateway is unchanged.
            let bytes: Vec<u8> = credentials
                .bytes()
                .filter_map(|b| match b {
                    b'-' => Some(b'+'),
                    b'_' => Some(b'/'),
                    b if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' => {
                        Some(b)
                    }
                    _ => None,
                })
                .collect();
            let engine = GeneralPurpose::new(
                &alphabet::STANDARD,
                GeneralPurposeConfig::new()
                    .with_decode_padding_mode(DecodePaddingMode::Indifferent)
                    .with_decode_allow_trailing_bits(true),
            );
            let bytes = engine.decode(bytes).map_err(|_| invalid_basic("BASIC"))?;
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let Some((username, password)) = decoded.split_once(':') else {
            return Err(invalid_basic("BASIC"));
        };
        if ["anonymous", "bearer"].contains(&username) {
            return Err(invalid_credentials());
        }
        let user = self
            .config
            .users
            .iter()
            .find(|u| u.username == username)
            .ok_or_else(invalid_credentials)?;
        if !password_matches(password, &user.password) {
            return Err(invalid_credentials());
        }
        if !path_allowed(user, path) {
            return Err(denied_path(path, &user.username));
        }
        Ok(())
    }
    fn anonymous(&self, path: &str, state: &mut ExchangeState) -> Result<(), StatusError> {
        if self.config.allow_anonymous
            && let Some(user) = self.config.users.iter().find(|u| u.username == "anonymous")
        {
            if path_allowed(user, path) {
                return Ok(());
            }
            state.response_headers.insert(
                "www-authenticate",
                "Basic realm=\"Default Realm\""
                    .parse()
                    .expect("static header"),
            );
            return Err(denied_path(path, "anonymous"));
        }
        state.response_headers.insert(
            "www-authenticate",
            "Basic realm=\"Basic Auth\"".parse().expect("static header"),
        );
        Err(StatusError::missing_identity())
    }
    fn bearer_permission(&self, path: &str) -> Result<(), StatusError> {
        if !self.config.allow_bearer_token {
            return Err(invalid_basic("BEARER"));
        }
        let user = self
            .config
            .users
            .iter()
            .find(|u| u.username == "bearer")
            .ok_or_else(|| {
                StatusError::new(
                    400,
                    "ERR10072",
                    "BEARER_USER_NOT_FOUND",
                    "Bearer token is allowed but bearer user is not found.",
                )
            })?;
        if !path_allowed(user, path) {
            return Err(denied_path(path, "bearer"));
        }
        Ok(())
    }
}
#[async_trait]
impl RequestHandler for BasicHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Basic
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        if !self.enabled() {
            return Ok(HandlerOutcome::Reject(StatusError::handler_failure()));
        }
        let result = match header(inv, "authorization") {
            Err(_) => Err(invalid_scheme("unknown")),
            Ok(None) => self.anonymous(&inv.path, state),
            Ok(Some(value)) if value.trim().is_empty() => self.anonymous(&inv.path, state),
            Ok(Some(value)) => match value.split_once(' ') {
                Some((scheme, credentials))
                    if scheme.eq_ignore_ascii_case("basic") && !credentials.trim().is_empty() =>
                {
                    self.authenticate_basic(&inv.path, value)
                }
                Some((scheme, credentials))
                    if scheme.eq_ignore_ascii_case("bearer") && !credentials.trim().is_empty() =>
                {
                    self.bearer_permission(&inv.path)
                }
                _ => Err(invalid_scheme(
                    if value
                        .split(' ')
                        .next()
                        .unwrap_or(value)
                        .eq_ignore_ascii_case("basic")
                    {
                        "BASIC"
                    } else if value
                        .split(' ')
                        .next()
                        .unwrap_or(value)
                        .eq_ignore_ascii_case("bearer")
                    {
                        "BEARER"
                    } else {
                        "unknown"
                    },
                )),
            },
        };
        Ok(match result {
            Ok(()) => HandlerOutcome::Continue,
            Err(e) => HandlerOutcome::Reject(e),
        })
    }
}
impl HandlerFactory for BasicHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        if !self.enabled() {
            return Err(InitError::Malformed("basic-auth.yml: disabled binding"));
        }
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}

struct ReadHeaders<'a>(&'a HeaderMap, &'a http::Method);
impl RequestRead for ReadHeaders<'_> {
    fn req_header(&self) -> RequestHead<'_> {
        RequestHead {
            headers: self.0,
            method: self.1,
        }
    }
}
// Intrinsically bounded credential work and process-wide admission. A blocking
// task owns its permit until actual completion, even if its caller is dropped.
static HASH_WORK: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));
#[derive(Clone)]
pub struct ApiKeyHandler {
    config: Arc<ApiKeyConfig>,
}
impl ApiKeyHandler {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        keys(value, &["enabled", "hashEnabled", "pathPrefixAuths"])?;
        let map = object(value, "apikey.yml: object required")?;
        let hash_enabled = boolean(map, "hashEnabled", false)?;
        let mut rules = Vec::new();
        let raw = structured(map.get("pathPrefixAuths"))?;
        let entries = match raw {
            Value::Null => Vec::new(),
            Value::Array(entries) => entries,
            _ => return Err(InitError::Malformed("apikey.yml: rule list required")),
        };
        for entry in entries {
            keys(&entry, &["pathPrefix", "headerName", "apiKey"])?;
            let path_prefix = string(entry.get("pathPrefix"))?;
            let header_name = string(entry.get("headerName"))?;
            let api_key = entry
                .get("apiKey")
                .and_then(Value::as_str)
                .ok_or(InitError::Malformed("apikey.yml: key string required"))?
                .to_owned();
            if !path_prefix.starts_with('/')
                || HeaderName::from_bytes(header_name.as_bytes()).is_err()
            {
                return Err(InitError::Malformed("apikey.yml: invalid rule"));
            }
            if hash_enabled {
                let parts: Vec<_> = api_key.split(':').collect();
                let bounded = parts.len() == 3
                    && parts[0]
                        .parse::<u32>()
                        .is_ok_and(|n| (1..=1_000_000).contains(&n))
                    && parts[1].len() <= 128
                    && parts[2].len() <= 128
                    && !parts[1].is_empty()
                    && !parts[2].is_empty()
                    && parts[1].len() % 2 == 0
                    && parts[2].len() % 2 == 0
                    && parts[1].bytes().all(|b| b.is_ascii_hexdigit())
                    && parts[2].bytes().all(|b| b.is_ascii_hexdigit());
                if !bounded {
                    return Err(InitError::Malformed(
                        "apikey.yml: invalid or unbounded hash",
                    ));
                }
            }
            rules.push(ApiKeyRule {
                path_prefix,
                header_name,
                api_key,
            });
        }
        Ok(Self {
            config: Arc::new(ApiKeyConfig {
                enabled: boolean(map, "enabled", false)?,
                hash_enabled,
                path_prefix_auths: rules,
            }),
        })
    }
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }
    pub(crate) async fn authenticate(
        &self,
        inv: &Invocation,
        required: bool,
    ) -> Result<(), StatusError> {
        if !self.enabled() {
            return Err(StatusError::handler_failure());
        }
        let mut found = false;
        for rule in &self.config.path_prefix_auths {
            if !inv.path.starts_with(&rule.path_prefix) {
                continue;
            }
            found = true;
            // Reject conflicting multimap credentials before shared first-value I/O.
            if header(inv, &rule.header_name).is_err() {
                continue;
            }
            // Preserve exact Java comparison for empty/whitespace-only keys;
            // shared Gateway rejects those configurations before comparison.
            if !self.config.hash_enabled && rule.api_key.trim().is_empty() {
                if header(inv, &rule.header_name)
                    .ok()
                    .flatten()
                    .is_some_and(|provided| {
                        (rule.api_key.is_empty() && provided.is_empty())
                            || password_matches(provided, &rule.api_key)
                    })
                {
                    return Ok(());
                }
                continue;
            }
            let config = ApiKeyConfig {
                enabled: true,
                hash_enabled: self.config.hash_enabled,
                path_prefix_auths: vec![rule.clone()],
            };
            let matched = if config.hash_enabled {
                let permit =
                    tokio::time::timeout_at(inv.deadline, HASH_WORK.clone().acquire_owned())
                        .await
                        .map_err(|_| StatusError::timeout())?
                        .map_err(|_| StatusError::handler_failure())?;
                let headers = inv.headers.clone();
                let method = inv.method.clone();
                let path = inv.path.clone();
                let task = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    verify_api_key(&ReadHeaders(&headers, &method), &config, &path).is_ok()
                });
                tokio::time::timeout_at(inv.deadline, task)
                    .await
                    .map_err(|_| StatusError::timeout())?
                    .map_err(|_| StatusError::handler_failure())?
            } else {
                verify_api_key(&ReadHeaders(&inv.headers, &inv.method), &config, &inv.path).is_ok()
            };
            if matched {
                return Ok(());
            }
        }
        if !found && !required {
            return Ok(());
        }
        Err(StatusError::new(
            401,
            "ERR10075",
            "API_KEY_MISMATCH",
            &format!(
                "Could not find matched APIKEY for request path {}.",
                inv.path
            ),
        ))
    }
}
#[async_trait]
impl RequestHandler for ApiKeyHandler {
    fn id(&self) -> HandlerId {
        HandlerId::ApiKey
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        Ok(match self.authenticate(inv, false).await {
            Ok(()) => HandlerOutcome::Continue,
            Err(e) => HandlerOutcome::Reject(e),
        })
    }
}
impl HandlerFactory for ApiKeyHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        if !self.enabled() {
            return Err(InitError::Malformed("apikey.yml: disabled binding"));
        }
        Ok(Binding::Request(
            Arc::new(self.clone()),
            RequestCapabilities::default(),
        ))
    }
}

#[cfg(test)]
mod blocking_tests {
    use super::*;
    use serde_json::json;
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_hash_task_keeps_its_permit_after_caller_cancellation() {
        let handler=ApiKeyHandler::from_value(&json!({"enabled":true,"hashEnabled":true,"pathPrefixAuths":[{"pathPrefix":"/hash","headerName":"x-test-key","apiKey":format!("100000:00:{}","00".repeat(64))}]})).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("x-test-key", "test-only-value".parse().unwrap());
        let inv = Invocation {
            method: http::Method::GET,
            raw_path: "/hash".into(),
            path: "/hash".into(),
            raw_query: None,
            query: MultiMap::default(),
            headers,
            body: None,
            body_was_base64: false,
            source: SourceMetadata::default(),
            request_id: "test-only".into(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(10),
            response_deadline: tokio::time::Instant::now() + Duration::from_secs(11),
            cancel: tokio_util::sync::CancellationToken::new(),
            path_params: BTreeMap::new(),
            route: None,
            original: json!({}),
        };
        let caller = tokio::spawn(async move { handler.authenticate(&inv, false).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while HASH_WORK.available_permits() == 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(
            HASH_WORK.available_permits() < 4,
            "caller cancellation must not release the actual worker's permit"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while HASH_WORK.available_permits() != 4 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
}
