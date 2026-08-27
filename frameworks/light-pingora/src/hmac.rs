use crate::config_util::{deserialize_string_list, deserialize_typed_list};
use crate::hmac_replay::{
    HMAC_REPLAY_CACHE_PREFIX, LocalWebhookReplayStore, RedisWebhookReplayStore, ReplayAdminError,
    ReplayRemovalOutcome, WebhookReplayKey, WebhookReplayStore,
};
use base64::Engine as _;
use hmac::{Hmac, Mac};
use http::HeaderMap;
use http::header::HeaderName;
use light_runtime::{CacheRegistry, ModuleKind, RuntimeCache, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

pub const HMAC_FILE: &str = "hmac.yml";
pub const HMAC_MODULE_ID: &str = "light-pingora/hmac";
pub const HMAC_CONFIG_NAME: &str = "hmac";

const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_BUFFERED_BODY_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_BODY_READ_TIMEOUT_MILLIS: u64 = 10_000;
const DEFAULT_REPLAY_RETENTION_SECONDS: u64 = 7 * 24 * 60 * 60;
const MAX_SECRET_CANDIDATES: usize = 2;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HmacConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_buffered_body_bytes")]
    pub max_buffered_body_bytes: usize,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub path_prefix_auths: Vec<HmacStandaloneRule>,
    #[serde(default)]
    pub profiles: BTreeMap<String, HmacProfileConfig>,
    #[serde(default)]
    pub replay_stores: BTreeMap<String, ReplayStoreConfig>,
}

impl Default for HmacConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_buffered_body_bytes: default_max_buffered_body_bytes(),
            path_prefix_auths: Vec::new(),
            profiles: BTreeMap::new(),
            replay_stores: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HmacStandaloneRule {
    #[serde(default)]
    pub prefix: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub methods: Vec<String>,
    #[serde(default)]
    pub profile: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HmacProfileConfig {
    #[serde(default)]
    pub signed_input: HmacSignedInput,
    #[serde(default)]
    pub algorithm: HmacAlgorithm,
    #[serde(default = "default_signature_header")]
    pub signature_header: String,
    #[serde(default = "default_signature_prefix")]
    pub signature_prefix: String,
    #[serde(default)]
    pub signature_encoding: HmacSignatureEncoding,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_body_read_timeout_millis")]
    pub body_read_timeout_millis: u64,
    #[serde(default)]
    pub secrets: HmacSecretsConfig,
    #[serde(default)]
    pub replay: HmacReplayConfig,
}

impl Default for HmacProfileConfig {
    fn default() -> Self {
        Self {
            signed_input: HmacSignedInput::RawBody,
            algorithm: HmacAlgorithm::HmacSha256,
            signature_header: default_signature_header(),
            signature_prefix: default_signature_prefix(),
            signature_encoding: HmacSignatureEncoding::Hex,
            max_body_bytes: default_max_body_bytes(),
            body_read_timeout_millis: default_body_read_timeout_millis(),
            secrets: HmacSecretsConfig::default(),
            replay: HmacReplayConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HmacSignedInput {
    #[default]
    RawBody,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HmacAlgorithm {
    #[default]
    HmacSha256,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum HmacSignatureEncoding {
    #[default]
    Hex,
    Base64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HmacSecretsConfig {
    #[serde(default)]
    pub selector_header: String,
    #[serde(default, skip_serializing)]
    pub by_selector: BTreeMap<String, Vec<String>>,
    #[serde(
        default,
        deserialize_with = "deserialize_string_list",
        skip_serializing
    )]
    pub default_env_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HmacReplayConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub id_header: String,
    #[serde(default)]
    pub store: String,
    #[serde(default = "default_replay_retention_seconds")]
    pub retention_seconds: u64,
}

impl Default for HmacReplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            id_header: String::new(),
            store: String::new(),
            retention_seconds: default_replay_retention_seconds(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ReplayStoreConfig {
    Local {
        #[serde(default = "default_local_max_entries")]
        max_entries: usize,
    },
    Redis {
        #[serde(skip_serializing)]
        url_env: String,
        #[serde(default = "default_replay_key_prefix")]
        key_prefix: String,
        #[serde(default = "default_redis_timeout_millis")]
        connect_timeout_millis: u64,
        #[serde(default = "default_redis_timeout_millis")]
        operation_timeout_millis: u64,
    },
}

pub trait HmacSecretResolver: Send + Sync {
    fn resolve(&self, environment_name: &str) -> Result<Vec<u8>, RuntimeError>;

    fn resolve_text(&self, environment_name: &str) -> Result<String, RuntimeError> {
        String::from_utf8(self.resolve(environment_name)?).map_err(|_| {
            RuntimeError::Config(format!(
                "environment variable `{environment_name}` is not valid UTF-8"
            ))
        })
    }
}

#[derive(Debug, Default)]
pub struct EnvironmentHmacSecretResolver;

impl HmacSecretResolver for EnvironmentHmacSecretResolver {
    fn resolve(&self, environment_name: &str) -> Result<Vec<u8>, RuntimeError> {
        let value = std::env::var(environment_name).map_err(|_| {
            RuntimeError::Config(format!(
                "HMAC secret environment variable `{environment_name}` is missing"
            ))
        })?;
        if value.is_empty() {
            return Err(RuntimeError::Config(format!(
                "HMAC secret environment variable `{environment_name}` is empty"
            )));
        }
        Ok(value.into_bytes())
    }
}

#[derive(Clone)]
pub struct HmacRuntime {
    max_buffered_body_bytes: usize,
    standalone_rules: Vec<CompiledStandaloneRule>,
    profiles: BTreeMap<String, Arc<CompiledProfile>>,
    replay_stores: BTreeMap<String, CompiledReplayStore>,
}

#[derive(Clone)]
struct CompiledReplayStore {
    identity: String,
    store: Arc<dyn WebhookReplayStore>,
    local_cache: Option<Arc<LocalWebhookReplayStore>>,
}

#[derive(Debug, Clone)]
struct CompiledStandaloneRule {
    prefix: String,
    methods: BTreeSet<String>,
    profile: String,
}

#[derive(Clone)]
struct CompiledProfile {
    signature_header: HeaderName,
    signature_prefix: String,
    signature_encoding: HmacSignatureEncoding,
    max_body_bytes: usize,
    body_read_timeout_millis: u64,
    selector_header: Option<HeaderName>,
    by_selector: BTreeMap<String, Vec<Vec<u8>>>,
    default_secrets: Vec<Vec<u8>>,
    replay: HmacReplayConfig,
}

#[derive(Clone, PartialEq, Eq)]
pub struct HmacEvidence {
    pub profile: String,
    pub selector: Option<String>,
}

#[derive(Clone)]
pub struct HmacReplayAttempt {
    pub key: WebhookReplayKey,
    pub retention: Duration,
    pub store: Arc<dyn WebhookReplayStore>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StandaloneHmacRoute {
    pub prefix: String,
    pub methods: Vec<String>,
    pub profile: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HmacVerificationError {
    #[error("invalid webhook authentication")]
    Invalid,
    #[error("webhook body exceeds the configured maximum")]
    BodyTooLarge,
}

impl HmacRuntime {
    pub fn compile(
        config: &HmacConfig,
        resolver: &dyn HmacSecretResolver,
    ) -> Result<Self, RuntimeError> {
        Self::compile_preserving(config, resolver, None)
    }

    pub fn compile_preserving(
        config: &HmacConfig,
        resolver: &dyn HmacSecretResolver,
        previous: Option<&HmacRuntime>,
    ) -> Result<Self, RuntimeError> {
        validate_config(config)?;
        let replay_stores = compile_replay_stores(config, resolver, previous)?;
        let profiles = config
            .profiles
            .iter()
            .map(|(name, profile)| {
                compile_profile(name, profile, resolver)
                    .map(|profile| (name.clone(), Arc::new(profile)))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        let standalone_rules = config
            .path_prefix_auths
            .iter()
            .map(|rule| {
                Ok(CompiledStandaloneRule {
                    prefix: rule.prefix.clone(),
                    methods: normalize_methods(&rule.methods)?,
                    profile: rule.profile.clone(),
                })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok(Self {
            max_buffered_body_bytes: config.max_buffered_body_bytes,
            standalone_rules,
            profiles,
            replay_stores,
        })
    }

    pub fn max_buffered_body_bytes(&self) -> usize {
        self.max_buffered_body_bytes
    }

    pub fn contains_profile(&self, profile: &str) -> bool {
        self.profiles.contains_key(profile)
    }

    pub fn standalone_profile(&self, path: &str, method: &str) -> Option<&str> {
        let method = method.to_ascii_uppercase();
        self.standalone_rules
            .iter()
            .filter(|rule| {
                path.starts_with(rule.prefix.as_str())
                    && (rule.methods.is_empty() || rule.methods.contains(&method))
            })
            .max_by_key(|rule| rule.prefix.len())
            .map(|rule| rule.profile.as_str())
    }

    pub fn standalone_routes(&self) -> Vec<StandaloneHmacRoute> {
        self.standalone_rules
            .iter()
            .map(|rule| StandaloneHmacRoute {
                prefix: rule.prefix.clone(),
                methods: rule.methods.iter().cloned().collect(),
                profile: rule.profile.clone(),
            })
            .collect()
    }

    pub fn standalone_policy_overlaps(
        &self,
        prefix: &str,
        methods: &[String],
    ) -> Result<bool, RuntimeError> {
        let methods = normalize_methods(methods)?;
        Ok(self.standalone_rules.iter().any(|rule| {
            prefixes_overlap(rule.prefix.as_str(), prefix)
                && (rule.methods.is_empty()
                    || methods.is_empty()
                    || rule.methods.iter().any(|method| methods.contains(method)))
        }))
    }

    pub fn profile_limits(&self, profile: &str) -> Option<(usize, u64)> {
        self.profiles
            .get(profile)
            .map(|profile| (profile.max_body_bytes, profile.body_read_timeout_millis))
    }

    pub fn replay_config(&self, profile: &str) -> Option<&HmacReplayConfig> {
        self.profiles.get(profile).map(|profile| &profile.replay)
    }

    pub fn replay_store(&self, profile: &str) -> Option<Arc<dyn WebhookReplayStore>> {
        let replay = self.replay_config(profile)?;
        replay
            .enabled
            .then(|| self.replay_stores.get(&replay.store))?
            .map(|compiled| Arc::clone(&compiled.store))
    }

    pub fn replay_attempt(
        &self,
        evidence: &HmacEvidence,
        headers: &HeaderMap,
    ) -> Result<Option<HmacReplayAttempt>, HmacVerificationError> {
        let profile = self
            .profiles
            .get(evidence.profile.as_str())
            .ok_or(HmacVerificationError::Invalid)?;
        if !profile.replay.enabled {
            return Ok(None);
        }
        let id_header = HeaderName::from_bytes(profile.replay.id_header.as_bytes())
            .map_err(|_| HmacVerificationError::Invalid)?;
        let delivery_id = exactly_one_header(headers, &id_header)?.trim();
        if delivery_id.is_empty() {
            return Err(HmacVerificationError::Invalid);
        }
        let selector = evidence.selector.as_deref().unwrap_or("shared");
        let key = WebhookReplayKey::new(evidence.profile.as_str(), selector, delivery_id)
            .map_err(|_| HmacVerificationError::Invalid)?;
        let store = self
            .replay_stores
            .get(profile.replay.store.as_str())
            .map(|compiled| Arc::clone(&compiled.store))
            .ok_or(HmacVerificationError::Invalid)?;
        Ok(Some(HmacReplayAttempt {
            key,
            retention: Duration::from_secs(profile.replay.retention_seconds),
            store,
        }))
    }

    pub async fn force_remove_replay(
        &self,
        profile_name: &str,
        selector: &str,
        delivery_id: &str,
    ) -> Result<ReplayRemovalOutcome, ReplayAdminError> {
        let profile = self
            .profiles
            .get(profile_name)
            .ok_or(ReplayAdminError::UnknownProfile)?;
        if !profile.replay.enabled {
            return Err(ReplayAdminError::ReplayDisabled);
        }
        let replay_store = self
            .replay_stores
            .get(&profile.replay.store)
            .ok_or(ReplayAdminError::ReplayDisabled)?;
        let selector = if profile.by_selector.contains_key(selector) {
            selector
        } else if selector == "shared" && !profile.default_secrets.is_empty() {
            "shared"
        } else {
            return Err(ReplayAdminError::Invalid(
                "selector is not valid for the HMAC profile".to_string(),
            ));
        };
        let key = WebhookReplayKey::new(profile_name, selector, delivery_id)?;
        let removed = replay_store.store.force_remove(&key).await?;
        Ok(ReplayRemovalOutcome {
            removed,
            scope: replay_store.store.scope(),
        })
    }

    pub fn register_local_replay_caches(&self, registry: &CacheRegistry) {
        for (name, compiled) in &self.replay_stores {
            if compiled.store.scope().as_str() == "local" {
                tracing::warn!(
                    target: "light_pingora::hmac",
                    replay_store = name,
                    scope = "local",
                    "HMAC replay protection is process-local and is lost on restart"
                );
            } else {
                tracing::info!(
                    target: "light_pingora::hmac",
                    replay_store = name,
                    scope = "distributed",
                    "distributed HMAC replay protection activated"
                );
            }
            if let Some(cache) = compiled.local_cache.as_ref() {
                let cache: Arc<dyn RuntimeCache> = cache.clone();
                registry.register_arc(format!("{HMAC_REPLAY_CACHE_PREFIX}{name}"), cache);
            }
        }
    }

    pub fn unregister_local_replay_caches(&self, registry: &CacheRegistry) {
        for (name, compiled) in &self.replay_stores {
            if compiled.local_cache.is_some() {
                registry.unregister(&format!("{HMAC_REPLAY_CACHE_PREFIX}{name}"));
            }
        }
    }

    pub fn verify(
        &self,
        profile_name: &str,
        headers: &HeaderMap,
        body: &[u8],
    ) -> Result<HmacEvidence, HmacVerificationError> {
        let profile = self
            .profiles
            .get(profile_name)
            .ok_or(HmacVerificationError::Invalid)?;
        if body.len() > profile.max_body_bytes {
            return Err(HmacVerificationError::BodyTooLarge);
        }
        let signature = exactly_one_header(headers, &profile.signature_header)?
            .trim_matches(|character| character == ' ' || character == '\t');
        let signature = signature
            .strip_prefix(profile.signature_prefix.as_str())
            .ok_or(HmacVerificationError::Invalid)?;
        let signature = match profile.signature_encoding {
            HmacSignatureEncoding::Hex => {
                hex::decode(signature).map_err(|_| HmacVerificationError::Invalid)?
            }
            HmacSignatureEncoding::Base64 => base64::engine::general_purpose::STANDARD
                .decode(signature)
                .map_err(|_| HmacVerificationError::Invalid)?,
        };
        if signature.len() != 32 {
            return Err(HmacVerificationError::Invalid);
        }

        let selector = profile
            .selector_header
            .as_ref()
            .map(|name| {
                optional_single_header(headers, name)
                    .map(|value| value.map(str::trim).map(str::to_string))
            })
            .transpose()?
            .flatten();
        let (selector, candidates) = match selector
            .as_ref()
            .filter(|selector| !selector.is_empty())
            .and_then(|selector| {
                profile
                    .by_selector
                    .get(selector)
                    .map(|candidates| (Some(selector.clone()), candidates))
            }) {
            Some(selected) => selected,
            None => (None, &profile.default_secrets),
        };
        if candidates.is_empty() {
            return Err(HmacVerificationError::Invalid);
        }

        let mut matched = false;
        for secret in candidates {
            let mut mac = Hmac::<Sha256>::new_from_slice(secret)
                .map_err(|_| HmacVerificationError::Invalid)?;
            mac.update(body);
            matched |= mac.verify_slice(&signature).is_ok();
        }
        if !matched {
            return Err(HmacVerificationError::Invalid);
        }
        Ok(HmacEvidence {
            profile: profile_name.to_string(),
            selector,
        })
    }
}

impl std::fmt::Debug for HmacRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HmacRuntime")
            .field("max_buffered_body_bytes", &self.max_buffered_body_bytes)
            .field("standalone_rule_count", &self.standalone_rules.len())
            .field("profile_names", &self.profiles.keys().collect::<Vec<_>>())
            .finish()
    }
}

pub fn load_hmac_runtime(
    runtime_config: &RuntimeConfig,
    required: bool,
) -> Result<Option<HmacRuntime>, RuntimeError> {
    load_hmac_runtime_preserving(runtime_config, required, None)
}

pub fn load_hmac_runtime_preserving(
    runtime_config: &RuntimeConfig,
    required: bool,
    previous: Option<&HmacRuntime>,
) -> Result<Option<HmacRuntime>, RuntimeError> {
    if !required {
        return Ok(None);
    }
    let config = runtime_config
        .module_registry
        .load_config::<HmacConfig>(runtime_config, HMAC_FILE)?;
    if !config.enabled {
        return Err(RuntimeError::Config(
            "hmac.yml is required by the active handler policy but is disabled".to_string(),
        ));
    }
    let runtime =
        HmacRuntime::compile_preserving(&config, &EnvironmentHmacSecretResolver, previous)?;
    runtime_config.module_registry.register_loaded_config(
        HMAC_MODULE_ID,
        HMAC_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [],
        true,
        Some(true),
        true,
    )?;
    Ok(Some(runtime))
}

fn validate_config(config: &HmacConfig) -> Result<(), RuntimeError> {
    if config.max_buffered_body_bytes == 0 {
        return config_error("maxBufferedBodyBytes must be positive");
    }
    let mut max_profile_body = 0;
    for (name, profile) in &config.profiles {
        if name.trim().is_empty() {
            return config_error("HMAC profile names must not be empty");
        }
        validate_profile(name, profile, &config.replay_stores)?;
        max_profile_body = max_profile_body.max(profile.max_body_bytes);
    }
    if config.max_buffered_body_bytes < max_profile_body {
        return config_error(format!(
            "maxBufferedBodyBytes ({}) must be at least the largest profile maxBodyBytes ({max_profile_body})",
            config.max_buffered_body_bytes
        ));
    }

    for rule in &config.path_prefix_auths {
        if rule.prefix.is_empty() {
            return config_error("standalone HMAC rule prefix must not be empty");
        }
        if !config.profiles.contains_key(rule.profile.as_str()) {
            return config_error(format!(
                "standalone HMAC rule `{}` references unknown profile `{}`",
                rule.prefix, rule.profile
            ));
        }
        normalize_methods(&rule.methods)?;
    }
    for (name, store) in &config.replay_stores {
        if name.trim().is_empty() {
            return config_error("HMAC replay-store names must not be empty");
        }
        match store {
            ReplayStoreConfig::Local { max_entries } if *max_entries == 0 => {
                return config_error(format!(
                    "local HMAC replay store `{name}` maxEntries must be positive"
                ));
            }
            ReplayStoreConfig::Redis {
                url_env,
                key_prefix,
                connect_timeout_millis,
                operation_timeout_millis,
            } => {
                if url_env.trim().is_empty() || key_prefix.is_empty() {
                    return config_error(format!(
                        "Redis HMAC replay store `{name}` requires urlEnv and keyPrefix"
                    ));
                }
                if *connect_timeout_millis == 0 || *operation_timeout_millis == 0 {
                    return config_error(format!(
                        "Redis HMAC replay store `{name}` timeouts must be positive"
                    ));
                }
            }
            ReplayStoreConfig::Local { .. } => {}
        }
    }
    for (index, left) in config.path_prefix_auths.iter().enumerate() {
        for right in config.path_prefix_auths.iter().skip(index + 1) {
            if left.prefix == right.prefix && methods_overlap(&left.methods, &right.methods)? {
                return config_error(format!(
                    "duplicate standalone HMAC rules for prefix `{}` have overlapping methods",
                    left.prefix
                ));
            }
        }
    }
    Ok(())
}

fn compile_replay_stores(
    config: &HmacConfig,
    resolver: &dyn HmacSecretResolver,
    previous: Option<&HmacRuntime>,
) -> Result<BTreeMap<String, CompiledReplayStore>, RuntimeError> {
    let referenced = config
        .profiles
        .values()
        .filter(|profile| profile.replay.enabled)
        .map(|profile| profile.replay.store.as_str())
        .collect::<BTreeSet<_>>();
    referenced
        .into_iter()
        .map(|name| {
            let store_config = config.replay_stores.get(name).ok_or_else(|| {
                RuntimeError::Config(format!("unknown HMAC replay store `{name}`"))
            })?;
            let (identity, candidate) = match store_config {
                ReplayStoreConfig::Local { max_entries } => {
                    let identity = format!("local:{max_entries}");
                    let local = Arc::new(LocalWebhookReplayStore::new(*max_entries));
                    let store: Arc<dyn WebhookReplayStore> = local.clone();
                    (
                        identity,
                        CompiledReplayStore {
                            identity: String::new(),
                            store,
                            local_cache: Some(local),
                        },
                    )
                }
                ReplayStoreConfig::Redis {
                    url_env,
                    key_prefix,
                    connect_timeout_millis,
                    operation_timeout_millis,
                } => {
                    let url = resolver.resolve_text(url_env)?;
                    let url_digest = hex::encode(Sha256::digest(url.as_bytes()));
                    let identity = format!(
                        "redis:{url_digest}:{key_prefix}:{connect_timeout_millis}:{operation_timeout_millis}"
                    );
                    let redis = RedisWebhookReplayStore::new(
                        &url,
                        key_prefix.clone(),
                        std::time::Duration::from_millis(*connect_timeout_millis),
                        std::time::Duration::from_millis(*operation_timeout_millis),
                    )?;
                    (
                        identity,
                        CompiledReplayStore {
                            identity: String::new(),
                            store: Arc::new(redis),
                            local_cache: None,
                        },
                    )
                }
            };
            let compiled = previous
                .and_then(|runtime| runtime.replay_stores.get(name))
                .filter(|compiled| compiled.identity == identity)
                .cloned()
                .unwrap_or_else(|| CompiledReplayStore {
                    identity: identity.clone(),
                    ..candidate
                });
            Ok((name.to_string(), compiled))
        })
        .collect()
}

fn validate_profile(
    name: &str,
    profile: &HmacProfileConfig,
    replay_stores: &BTreeMap<String, ReplayStoreConfig>,
) -> Result<(), RuntimeError> {
    if profile.max_body_bytes == 0 {
        return config_error(format!(
            "HMAC profile `{name}` maxBodyBytes must be positive"
        ));
    }
    if profile.body_read_timeout_millis == 0 {
        return config_error(format!(
            "HMAC profile `{name}` bodyReadTimeoutMillis must be positive"
        ));
    }
    parse_header_name(name, "signatureHeader", &profile.signature_header)?;
    if !profile.secrets.selector_header.is_empty() {
        parse_header_name(name, "selectorHeader", &profile.secrets.selector_header)?;
    }
    if !profile.secrets.by_selector.is_empty() && profile.secrets.selector_header.is_empty() {
        return config_error(format!(
            "HMAC profile `{name}` configures bySelector without selectorHeader"
        ));
    }
    validate_secret_names(name, "defaultEnvNames", &profile.secrets.default_env_names)?;
    for (selector, names) in &profile.secrets.by_selector {
        if selector.trim().is_empty() {
            return config_error(format!(
                "HMAC profile `{name}` contains an empty secret selector"
            ));
        }
        if names.is_empty() {
            return config_error(format!(
                "HMAC profile `{name}` selector `{selector}` has no secret candidates"
            ));
        }
        validate_secret_names(name, selector, names)?;
    }
    if profile.secrets.by_selector.is_empty() && profile.secrets.default_env_names.is_empty() {
        return config_error(format!(
            "HMAC profile `{name}` must configure selector secrets or defaultEnvNames"
        ));
    }
    if profile.replay.enabled {
        parse_header_name(name, "replay.idHeader", &profile.replay.id_header)?;
        if profile.replay.retention_seconds == 0 {
            return config_error(format!(
                "HMAC profile `{name}` replay retentionSeconds must be positive"
            ));
        }
        if !replay_stores.contains_key(profile.replay.store.as_str()) {
            return config_error(format!(
                "HMAC profile `{name}` references unknown replay store `{}`",
                profile.replay.store
            ));
        }
    }
    Ok(())
}

fn validate_secret_names(name: &str, key: &str, names: &[String]) -> Result<(), RuntimeError> {
    if names.len() > MAX_SECRET_CANDIDATES {
        return config_error(format!(
            "HMAC profile `{name}` secret list `{key}` exceeds the maximum of {MAX_SECRET_CANDIDATES}"
        ));
    }
    if names.iter().any(|name| name.trim().is_empty()) {
        return config_error(format!(
            "HMAC profile `{name}` secret list `{key}` contains an empty environment name"
        ));
    }
    let unique = names.iter().collect::<BTreeSet<_>>();
    if unique.len() != names.len() {
        return config_error(format!(
            "HMAC profile `{name}` secret list `{key}` contains a duplicate environment name"
        ));
    }
    Ok(())
}

fn compile_profile(
    name: &str,
    profile: &HmacProfileConfig,
    resolver: &dyn HmacSecretResolver,
) -> Result<CompiledProfile, RuntimeError> {
    let by_selector = profile
        .secrets
        .by_selector
        .iter()
        .map(|(selector, names)| {
            resolve_secret_list(names, resolver).map(|secrets| (selector.clone(), secrets))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(CompiledProfile {
        signature_header: parse_header_name(name, "signatureHeader", &profile.signature_header)?,
        signature_prefix: profile.signature_prefix.clone(),
        signature_encoding: profile.signature_encoding,
        max_body_bytes: profile.max_body_bytes,
        body_read_timeout_millis: profile.body_read_timeout_millis,
        selector_header: (!profile.secrets.selector_header.is_empty())
            .then(|| parse_header_name(name, "selectorHeader", &profile.secrets.selector_header))
            .transpose()?,
        by_selector,
        default_secrets: resolve_secret_list(&profile.secrets.default_env_names, resolver)?,
        replay: profile.replay.clone(),
    })
}

fn resolve_secret_list(
    names: &[String],
    resolver: &dyn HmacSecretResolver,
) -> Result<Vec<Vec<u8>>, RuntimeError> {
    names.iter().map(|name| resolver.resolve(name)).collect()
}

fn normalize_methods(methods: &[String]) -> Result<BTreeSet<String>, RuntimeError> {
    let mut normalized = BTreeSet::new();
    for method in methods {
        let method = method.trim().to_ascii_uppercase();
        if method.is_empty() || http::Method::from_bytes(method.as_bytes()).is_err() {
            return config_error(format!("invalid HTTP method `{method}` in HMAC policy"));
        }
        if !normalized.insert(method.clone()) {
            return config_error(format!("duplicate HTTP method `{method}` in HMAC policy"));
        }
    }
    Ok(normalized)
}

fn methods_overlap(left: &[String], right: &[String]) -> Result<bool, RuntimeError> {
    let left = normalize_methods(left)?;
    let right = normalize_methods(right)?;
    Ok(left.is_empty() || right.is_empty() || left.iter().any(|method| right.contains(method)))
}

fn prefixes_overlap(left: &str, right: &str) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn parse_header_name(profile: &str, field: &str, value: &str) -> Result<HeaderName, RuntimeError> {
    if value.trim().is_empty() {
        return config_error(format!(
            "HMAC profile `{profile}` {field} must not be empty"
        ));
    }
    HeaderName::from_bytes(value.as_bytes()).map_err(|_| {
        RuntimeError::Config(format!(
            "HMAC profile `{profile}` {field} `{value}` is not a valid HTTP header name"
        ))
    })
}

fn exactly_one_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<&'a str, HmacVerificationError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(HmacVerificationError::Invalid)?;
    if values.next().is_some() {
        return Err(HmacVerificationError::Invalid);
    }
    value.to_str().map_err(|_| HmacVerificationError::Invalid)
}

fn optional_single_header<'a>(
    headers: &'a HeaderMap,
    name: &HeaderName,
) -> Result<Option<&'a str>, HmacVerificationError> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(HmacVerificationError::Invalid);
    }
    value
        .to_str()
        .map(Some)
        .map_err(|_| HmacVerificationError::Invalid)
}

fn config_error<T>(message: impl Into<String>) -> Result<T, RuntimeError> {
    Err(RuntimeError::Config(message.into()))
}

fn default_enabled() -> bool {
    true
}

fn default_max_buffered_body_bytes() -> usize {
    DEFAULT_MAX_BUFFERED_BODY_BYTES
}

fn default_max_body_bytes() -> usize {
    DEFAULT_MAX_BODY_BYTES
}

fn default_body_read_timeout_millis() -> u64 {
    DEFAULT_BODY_READ_TIMEOUT_MILLIS
}

fn default_signature_header() -> String {
    "X-Hub-Signature-256".to_string()
}

fn default_signature_prefix() -> String {
    "sha256=".to_string()
}

fn default_replay_retention_seconds() -> u64 {
    DEFAULT_REPLAY_RETENTION_SECONDS
}

fn default_local_max_entries() -> usize {
    100_000
}

fn default_replay_key_prefix() -> String {
    "light:hmac-replay:".to_string()
}

fn default_redis_timeout_millis() -> u64 {
    1_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    struct MapResolver(BTreeMap<String, Vec<u8>>);

    impl HmacSecretResolver for MapResolver {
        fn resolve(&self, environment_name: &str) -> Result<Vec<u8>, RuntimeError> {
            self.0.get(environment_name).cloned().ok_or_else(|| {
                RuntimeError::Config(format!("missing test secret `{environment_name}`"))
            })
        }
    }

    fn github_config() -> HmacConfig {
        serde_yaml::from_str(
            r#"
enabled: true
maxBufferedBodyBytes: 268435456
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    profile: github
  - prefix: /webhook/specific
    methods: [POST]
    profile: selected
profiles:
  github:
    signatureHeader: X-Hub-Signature-256
    signaturePrefix: sha256=
    signatureEncoding: hex
    secrets:
      defaultEnvNames: [GITHUB_SECRET]
  selected:
    signatureHeader: X-Hub-Signature-256
    signaturePrefix: sha256=
    signatureEncoding: hex
    secrets:
      selectorHeader: X-GitHub-Hook-ID
      bySelector:
        hook-1: [CURRENT_SECRET, PREVIOUS_SECRET]
      defaultEnvNames: [FALLBACK_SECRET]
"#,
        )
        .expect("parse HMAC fixture")
    }

    fn resolver() -> MapResolver {
        MapResolver(BTreeMap::from([
            (
                "GITHUB_SECRET".to_string(),
                b"It's a Secret to Everybody".to_vec(),
            ),
            ("CURRENT_SECRET".to_string(), b"current".to_vec()),
            ("PREVIOUS_SECRET".to_string(), b"previous".to_vec()),
            ("FALLBACK_SECRET".to_string(), b"fallback".to_vec()),
        ]))
    }

    #[test]
    fn verifies_github_published_raw_body_vector() {
        let runtime = HmacRuntime::compile(&github_config(), &resolver()).expect("compile runtime");
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_static(
                "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
            ),
        );

        let evidence = runtime
            .verify("github", &headers, b"Hello, World!")
            .expect("valid GitHub signature");
        assert_eq!(evidence.profile, "github");
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_static(
                " sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17\t",
            ),
        );
        runtime
            .verify("github", &headers, b"Hello, World!")
            .expect("HTTP optional whitespace must match Java verification");
        assert!(matches!(
            runtime.verify("github", &headers, b"Hello, World!\n"),
            Err(HmacVerificationError::Invalid)
        ));
    }

    #[test]
    fn shared_java_rust_raw_request_conformance_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/hmac-webhook-conformance-v1.json"
        ))
        .expect("parse shared HMAC fixture");
        assert_eq!(fixture["schemaVersion"], 1);
        for vector in fixture["signatureVectors"]
            .as_array()
            .expect("signature vectors")
        {
            let secret = vector["secretUtf8"].as_str().expect("fixture secret");
            let body = base64::engine::general_purpose::STANDARD
                .decode(vector["bodyBase64"].as_str().expect("fixture body"))
                .expect("decode fixture body");
            let signature = vector["signatureHeader"]
                .as_str()
                .expect("fixture signature");
            let mut config = HmacConfig::default();
            let mut profile = HmacProfileConfig::default();
            profile.secrets.default_env_names = vec!["FIXTURE_SECRET".to_string()];
            config.profiles.insert("fixture".to_string(), profile);
            let runtime = HmacRuntime::compile(
                &config,
                &MapResolver(BTreeMap::from([(
                    "FIXTURE_SECRET".to_string(),
                    secret.as_bytes().to_vec(),
                )])),
            )
            .expect("compile fixture runtime");
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-hub-signature-256",
                HeaderValue::from_str(signature).expect("fixture signature header"),
            );
            runtime
                .verify("fixture", &headers, body.as_slice())
                .unwrap_or_else(|_| panic!("fixture `{}` must verify", vector["name"]));
        }
    }

    #[test]
    fn selects_longest_method_aware_standalone_rule() {
        let runtime = HmacRuntime::compile(&github_config(), &resolver()).expect("compile runtime");
        assert_eq!(
            runtime.standalone_profile("/webhook/specific/event", "post"),
            Some("selected")
        );
        assert_eq!(
            runtime.standalone_profile("/webhook/event", "POST"),
            Some("github")
        );
        assert_eq!(runtime.standalone_profile("/webhook/event", "PUT"), None);
    }

    #[test]
    fn selector_accepts_previous_secret_and_unknown_uses_explicit_fallback() {
        let mut config = github_config();
        config.profiles.get_mut("selected").unwrap().replay = HmacReplayConfig {
            enabled: true,
            id_header: "X-GitHub-Delivery".to_string(),
            store: "selected-local".to_string(),
            retention_seconds: 60,
        };
        config.replay_stores.insert(
            "selected-local".to_string(),
            ReplayStoreConfig::Local { max_entries: 4 },
        );
        let runtime = HmacRuntime::compile(&config, &resolver()).expect("compile runtime");
        let body = b"payload";
        let mut previous = Hmac::<Sha256>::new_from_slice(b"previous").unwrap();
        previous.update(body);
        let mut headers = HeaderMap::new();
        headers.insert("x-github-hook-id", HeaderValue::from_static("hook-1"));
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(
                format!("sha256={}", hex::encode(previous.finalize().into_bytes())).as_str(),
            )
            .unwrap(),
        );
        let evidence = runtime.verify("selected", &headers, body).unwrap();
        assert_eq!(evidence.selector.as_deref(), Some("hook-1"));

        let mut fallback = Hmac::<Sha256>::new_from_slice(b"fallback").unwrap();
        fallback.update(body);
        headers.insert("x-github-hook-id", HeaderValue::from_static("unknown"));
        headers.insert(
            "x-github-delivery",
            HeaderValue::from_static("delivery-fallback"),
        );
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(
                format!("sha256={}", hex::encode(fallback.finalize().into_bytes())).as_str(),
            )
            .unwrap(),
        );
        let evidence = runtime.verify("selected", &headers, body).unwrap();
        assert_eq!(
            evidence.selector, None,
            "fallback must use the shared replay namespace"
        );
        let unknown_key = runtime
            .replay_attempt(&evidence, &headers)
            .unwrap()
            .expect("replay attempt")
            .key;

        headers.remove("x-github-hook-id");
        let evidence = runtime.verify("selected", &headers, body).unwrap();
        assert_eq!(evidence.selector, None);
        let missing_key = runtime
            .replay_attempt(&evidence, &headers)
            .unwrap()
            .expect("replay attempt")
            .key;
        assert_eq!(unknown_key.digest(), missing_key.digest());

        headers.insert("x-github-hook-id", HeaderValue::from_static("   "));
        let evidence = runtime.verify("selected", &headers, body).unwrap();
        assert_eq!(evidence.selector, None);
        let blank_key = runtime
            .replay_attempt(&evidence, &headers)
            .unwrap()
            .expect("replay attempt")
            .key;
        assert_eq!(unknown_key.digest(), blank_key.digest());
    }

    #[test]
    fn duplicate_signature_header_is_invalid() {
        let runtime = HmacRuntime::compile(&github_config(), &resolver()).expect("compile runtime");
        let mut headers = HeaderMap::new();
        headers.append("x-hub-signature-256", HeaderValue::from_static("sha256=00"));
        headers.append("x-hub-signature-256", HeaderValue::from_static("sha256=11"));
        assert!(matches!(
            runtime.verify("github", &headers, b"payload"),
            Err(HmacVerificationError::Invalid)
        ));
    }

    #[test]
    fn verifies_base64_binary_body_and_enforces_exact_limit() {
        let mut config = github_config();
        let profile = config.profiles.get_mut("selected").unwrap();
        profile.signature_encoding = HmacSignatureEncoding::Base64;
        profile.signature_prefix.clear();
        profile.max_body_bytes = 5;
        profile.secrets.selector_header.clear();
        profile.secrets.by_selector.clear();
        let runtime = HmacRuntime::compile(&config, &resolver()).expect("compile runtime");
        let body = [0_u8, 0xff, b' ', b'\n', b'}'];
        let mut mac = Hmac::<Sha256>::new_from_slice(b"fallback").unwrap();
        mac.update(&body);
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            HeaderValue::from_str(&signature).unwrap(),
        );
        assert!(runtime.verify("selected", &headers, &body).is_ok());
        assert!(matches!(
            runtime.verify("selected", &headers, &[0_u8; 6]),
            Err(HmacVerificationError::BodyTooLarge)
        ));
    }

    #[test]
    fn rejects_duplicate_methods_and_unknown_profiles() {
        let mut config = github_config();
        config.path_prefix_auths[0].methods = vec!["POST".to_string(), "post".to_string()];
        assert!(HmacRuntime::compile(&config, &resolver()).is_err());

        let mut config = github_config();
        config.path_prefix_auths[0].profile = "missing".to_string();
        assert!(HmacRuntime::compile(&config, &resolver()).is_err());
    }

    #[test]
    fn serialized_public_config_omits_secret_environment_names() {
        let config = github_config();
        let value = serde_json::to_value(&config).expect("serialize public config");
        let text = value.to_string();
        assert!(!text.contains("GITHUB_SECRET"));
        assert!(!text.contains("CURRENT_SECRET"));
        let runtime = HmacRuntime::compile(&config, &resolver()).expect("compile runtime");
        let debug = format!("{runtime:?}");
        assert!(!debug.contains("Secret to Everybody"));
        assert!(!debug.contains("current"));
    }

    #[tokio::test]
    async fn local_replay_store_is_selected_reused_and_administrable() {
        let mut config = github_config();
        let profile = config.profiles.get_mut("github").unwrap();
        profile.replay = HmacReplayConfig {
            enabled: true,
            id_header: "X-GitHub-Delivery".to_string(),
            store: "github-local".to_string(),
            retention_seconds: 60,
        };
        config.replay_stores.insert(
            "github-local".to_string(),
            ReplayStoreConfig::Local { max_entries: 4 },
        );
        let runtime = HmacRuntime::compile(&config, &resolver()).expect("compile replay runtime");
        let store = runtime
            .replay_store("github")
            .expect("profile replay store");
        let key = WebhookReplayKey::new("github", "shared", "delivery-1").unwrap();
        assert!(matches!(
            store
                .reserve(&key, std::time::Duration::from_secs(60))
                .await,
            Ok(crate::ReserveOutcome::Reserved(_))
        ));
        let preserved = HmacRuntime::compile_preserving(&config, &resolver(), Some(&runtime))
            .expect("preserve replay store");
        assert!(matches!(
            preserved
                .replay_store("github")
                .unwrap()
                .reserve(&key, std::time::Duration::from_secs(60))
                .await,
            Ok(crate::ReserveOutcome::Duplicate)
        ));
        let removed = preserved
            .force_remove_replay("github", "shared", "delivery-1")
            .await
            .expect("administrative removal");
        assert!(removed.removed);
        assert_eq!(removed.scope, crate::ReplayStoreScope::Local);
        assert!(matches!(
            preserved
                .force_remove_replay("github", "attacker-controlled", "delivery-1")
                .await,
            Err(ReplayAdminError::Invalid(_))
        ));

        let registry = CacheRegistry::new();
        preserved.register_local_replay_caches(&registry);
        let summary = registry
            .entries_summary("hmac-replay:github-local")
            .await
            .expect("safe local summary");
        assert_eq!(summary["scope"], "local");
        assert_eq!(
            registry.clear_supported("hmac-replay:github-local"),
            Some(false)
        );
    }

    #[test]
    fn replay_store_configuration_fails_closed_and_redacts_redis_url_env() {
        let mut config = github_config();
        config.profiles.get_mut("github").unwrap().replay = HmacReplayConfig {
            enabled: true,
            id_header: "X-GitHub-Delivery".to_string(),
            store: "redis".to_string(),
            retention_seconds: 60,
        };
        assert!(HmacRuntime::compile(&config, &resolver()).is_err());

        config.replay_stores.insert(
            "redis".to_string(),
            ReplayStoreConfig::Redis {
                url_env: "REDIS_URL".to_string(),
                key_prefix: "light:hmac-replay:".to_string(),
                connect_timeout_millis: 100,
                operation_timeout_millis: 100,
            },
        );
        let mut values = resolver().0;
        values.insert("REDIS_URL".to_string(), b"redis://127.0.0.1:6379".to_vec());
        let runtime = HmacRuntime::compile(&config, &MapResolver(values));
        assert!(runtime.is_ok());
        let public = serde_json::to_string(&config).unwrap();
        assert!(!public.contains("REDIS_URL"));
    }
}
