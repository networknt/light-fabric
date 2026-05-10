use crate::config_util::{deserialize_string_list, deserialize_typed_list};
use crate::security::{AuthPrincipal, HandlerRejection};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use light_runtime::{MaskSpec, ModuleKind, RuntimeCache, RuntimeConfig, RuntimeError};
use rand::RngCore;
use rand::rngs::OsRng;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value as JsonValue, json};
use sha2::Sha256;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::future::Future;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

pub const PII_TOKENIZATION_FILE: &str = "pii-tokenization.yml";
pub const PII_TOKENIZATION_LEGACY_FILE: &str = "pii-tokenization.yaml";
pub const PII_TOKENIZATION_MODULE_ID: &str = "light-pingora/pii-tokenization";
pub const PII_TOKENIZATION_CONFIG_NAME: &str = "pii-tokenization";
pub const PII_TOKENIZATION_CACHE_NAME: &str = "light-pingora/pii-tokenization-cache";

const DEFAULT_VALUE_ENCRYPTION_KEY_ENV: &str = "PII_TOKENIZATION_VALUE_ENCRYPTION_KEY";
const DEFAULT_VALUE_HASH_KEY_ENV: &str = "PII_TOKENIZATION_VALUE_HASH_KEY";
const DEFAULT_DATABASE_URL_ENV: &str = "PII_TOKENIZATION_DATABASE_URL";
const DEFAULT_MAX_BODY_SIZE: usize = 1_048_576;
const TOKEN_INSERT_RETRIES: usize = 8;
const ALPHA_NUMERIC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiTokenizationConfig {
    #[serde(default)]
    pub database: PiiDatabaseConfig,
    #[serde(default = "default_host_id_claim")]
    pub host_id_claim: String,
    #[serde(default = "default_max_body_size")]
    pub max_body_size: usize,
    #[serde(default)]
    pub crypto: PiiTokenCryptoConfig,
    #[serde(default)]
    pub cache: PiiTokenCacheConfig,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub rules: Vec<PiiTokenizationRule>,
}

impl Default for PiiTokenizationConfig {
    fn default() -> Self {
        Self {
            database: PiiDatabaseConfig::default(),
            host_id_claim: default_host_id_claim(),
            max_body_size: default_max_body_size(),
            crypto: PiiTokenCryptoConfig::default(),
            cache: PiiTokenCacheConfig::default(),
            rules: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiDatabaseConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,
    #[serde(default = "default_min_connections")]
    pub min_connections: u32,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
}

impl Default for PiiDatabaseConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            max_connections: default_max_connections(),
            min_connections: default_min_connections(),
            connect_timeout_ms: default_connect_timeout_ms(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiTokenCryptoConfig {
    #[serde(default = "default_crypto_algorithm")]
    pub algorithm: String,
    #[serde(default = "default_key_id")]
    pub key_id: String,
    #[serde(default = "default_value_encryption_key_env")]
    pub value_encryption_key_env: String,
    #[serde(default = "default_value_hash_key_env")]
    pub value_hash_key_env: String,
    #[serde(default)]
    pub value_encryption_key: String,
    #[serde(default)]
    pub value_hash_key: String,
}

impl Default for PiiTokenCryptoConfig {
    fn default() -> Self {
        Self {
            algorithm: default_crypto_algorithm(),
            key_id: default_key_id(),
            value_encryption_key_env: default_value_encryption_key_env(),
            value_hash_key_env: default_value_hash_key_env(),
            value_encryption_key: String::new(),
            value_hash_key: String::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiTokenCacheConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_cache_max_entries")]
    pub max_entries: usize,
    #[serde(default = "default_cache_ttl_seconds")]
    pub ttl_seconds: u64,
    #[serde(default = "default_true")]
    pub cache_cleartext: bool,
}

impl Default for PiiTokenCacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: default_cache_max_entries(),
            ttl_seconds: default_cache_ttl_seconds(),
            cache_cleartext: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiTokenizationRule {
    #[serde(default, alias = "pathPrefix")]
    pub path_prefix: String,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub methods: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub request: Vec<PiiFieldRule>,
    #[serde(default, deserialize_with = "deserialize_typed_list")]
    pub response: Vec<PiiFieldRule>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PiiFieldRule {
    pub path: String,
    pub scheme: String,
    #[serde(default)]
    pub required: bool,
}

pub struct PiiTokenizationRuntime {
    config: Arc<PiiTokenizationConfig>,
    pool: PgPool,
    keyring: Arc<PiiKeyring>,
    cache: Arc<PiiTokenCache>,
    rules: Arc<Vec<CompiledPiiRule>>,
}

impl fmt::Debug for PiiTokenizationRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiiTokenizationRuntime")
            .field("rule_count", &self.rules.len())
            .field("cache_enabled", &self.config.cache.enabled)
            .field("max_body_size", &self.config.max_body_size)
            .finish()
    }
}

impl PiiTokenizationRuntime {
    pub fn config(&self) -> &PiiTokenizationConfig {
        &self.config
    }

    pub fn max_body_size(&self) -> usize {
        self.config.max_body_size
    }

    pub fn has_request_rules(&self, path: &str, method: &str) -> bool {
        self.matching_rules(path, method)
            .iter()
            .any(|rule| !rule.request.is_empty())
    }

    pub fn has_response_rules(&self, path: &str, method: &str) -> bool {
        self.matching_rules(path, method)
            .iter()
            .any(|rule| !rule.response.is_empty())
    }

    pub fn validate_auth(&self, auth: Option<&AuthPrincipal>) -> Result<(), HandlerRejection> {
        self.host_id(auth).map(|_| ())
    }

    pub async fn tokenize_request_body(
        &self,
        auth: Option<&AuthPrincipal>,
        path: &str,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, HandlerRejection> {
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let host_id = self.host_id(auth)?;
        let mut payload = parse_json_body(body, "tokenize request")?;
        for rule in self.matching_rules(path, method) {
            self.apply_tokenize_fields(host_id, &mut payload, &rule.request)
                .await?;
        }
        serde_json::to_vec(&payload).map_err(|_| {
            HandlerRejection::new(500, "ERR13003", "failed to serialize tokenized JSON")
        })
    }

    pub async fn detokenize_response_body(
        &self,
        auth: Option<&AuthPrincipal>,
        path: &str,
        method: &str,
        body: &[u8],
    ) -> Result<Vec<u8>, HandlerRejection> {
        if body.is_empty() {
            return Ok(Vec::new());
        }
        let host_id = self.host_id(auth)?;
        let mut payload = parse_json_body(body, "detokenize response")?;
        for rule in self.matching_rules(path, method) {
            self.apply_detokenize_fields(host_id, &mut payload, &rule.response)
                .await?;
        }
        serde_json::to_vec(&payload).map_err(|_| {
            HandlerRejection::new(500, "ERR13003", "failed to serialize detokenized JSON")
        })
    }

    pub async fn tokenize_value(
        &self,
        host_id: Uuid,
        scheme: TokenScheme,
        value: &str,
    ) -> Result<String, HandlerRejection> {
        let canonical = canonical_value(value);
        let value_hash = self.keyring.value_hash(host_id, scheme.id(), canonical);
        let value_key = ValueCacheKey {
            host_id,
            scheme_id: scheme.id(),
            value_hash: value_hash.clone(),
        };
        if let Some(token) = self.cache.get_token(&value_key).await {
            return Ok(token);
        }
        if let Some(token) = self
            .select_token_by_value_hash(host_id, scheme.id(), &value_hash)
            .await?
        {
            self.cache.insert_token(value_key, token.clone()).await;
            return Ok(token);
        }

        for _ in 0..TOKEN_INSERT_RETRIES {
            let token = scheme.generate(canonical)?;
            let (ciphertext, nonce) = self.keyring.encrypt(canonical)?;
            let inserted = self
                .insert_token_row(
                    host_id,
                    token.as_str(),
                    scheme.id(),
                    &value_hash,
                    &ciphertext,
                    &nonce,
                )
                .await?;
            if inserted {
                self.cache
                    .insert_token(value_key.clone(), token.clone())
                    .await;
                self.cache
                    .insert_cleartext(
                        TokenCacheKey {
                            host_id,
                            token: token.clone(),
                        },
                        canonical.to_string(),
                    )
                    .await;
                return Ok(token);
            }
            if let Some(existing) = self
                .select_token_by_value_hash(host_id, scheme.id(), &value_hash)
                .await?
            {
                self.cache
                    .insert_token(value_key.clone(), existing.clone())
                    .await;
                return Ok(existing);
            }
        }

        Err(HandlerRejection::new(
            500,
            "ERR13004",
            "failed to generate unique PII token",
        ))
    }

    pub async fn detokenize_value(
        &self,
        host_id: Uuid,
        token: &str,
    ) -> Result<String, HandlerRejection> {
        let token_key = TokenCacheKey {
            host_id,
            token: token.to_string(),
        };
        if let Some(cleartext) = self.cache.get_cleartext(&token_key).await {
            return Ok(cleartext);
        }
        let Some(row) = self.select_token_row(host_id, token).await? else {
            return Err(HandlerRejection::new(
                500,
                "ERR13005",
                "PII token cannot be resolved",
            ));
        };
        let cleartext = self
            .keyring
            .decrypt(&row.value_ciphertext, &row.value_nonce)?;
        self.cache
            .insert_cleartext(token_key, cleartext.clone())
            .await;
        Ok(cleartext)
    }

    fn matching_rules(&self, path: &str, method: &str) -> Vec<&CompiledPiiRule> {
        self.rules
            .iter()
            .filter(|rule| rule.matches(path, method))
            .collect()
    }

    fn host_id(&self, auth: Option<&AuthPrincipal>) -> Result<Uuid, HandlerRejection> {
        let auth = auth.ok_or_else(|| {
            HandlerRejection::unauthorized("PII tokenization requires authenticated principal")
        })?;
        let claim = self.config.host_id_claim.trim();
        let host_id = claim_value(&auth.claims, claim)
            .or_else(|| {
                if claim.eq_ignore_ascii_case("host_id") {
                    auth.host.as_deref()
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                HandlerRejection::forbidden(format!("PII tokenization requires `{claim}` claim"))
            })?;
        Uuid::parse_str(host_id).map_err(|_| {
            HandlerRejection::forbidden(format!("PII tokenization `{claim}` claim is not a UUID"))
        })
    }

    async fn apply_tokenize_fields(
        &self,
        host_id: Uuid,
        payload: &mut JsonValue,
        fields: &[CompiledPiiFieldRule],
    ) -> Result<(), HandlerRejection> {
        for field in fields {
            let matches = collect_string_matches(payload, &field.path);
            validate_field_matches(&matches, field.required, "tokenize")?;
            for matched in matches.values {
                let token = self
                    .tokenize_value(host_id, field.scheme, matched.value.as_str())
                    .await?;
                set_json_string(payload, &matched.location, token)?;
            }
        }
        Ok(())
    }

    async fn apply_detokenize_fields(
        &self,
        host_id: Uuid,
        payload: &mut JsonValue,
        fields: &[CompiledPiiFieldRule],
    ) -> Result<(), HandlerRejection> {
        for field in fields {
            let matches = collect_string_matches(payload, &field.path);
            validate_field_matches(&matches, field.required, "detokenize")?;
            for matched in matches.values {
                let cleartext = self
                    .detokenize_value(host_id, matched.value.as_str())
                    .await?;
                set_json_string(payload, &matched.location, cleartext)?;
            }
        }
        Ok(())
    }

    async fn select_token_by_value_hash(
        &self,
        host_id: Uuid,
        scheme_id: i16,
        value_hash: &[u8],
    ) -> Result<Option<String>, HandlerRejection> {
        let row = sqlx::query(
            r#"
            SELECT token
            FROM pii_token_vault_t
            WHERE host_id = $1
              AND scheme_id = $2
              AND value_hash = $3
              AND active = TRUE
              AND (expires_ts IS NULL OR expires_ts > CURRENT_TIMESTAMP)
            LIMIT 1
            "#,
        )
        .bind(host_id)
        .bind(scheme_id)
        .bind(value_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_rejection)?;
        row.map(|row| {
            row.try_get::<String, _>("token")
                .map_err(database_rejection)
        })
        .transpose()
    }

    async fn insert_token_row(
        &self,
        host_id: Uuid,
        token: &str,
        scheme_id: i16,
        value_hash: &[u8],
        ciphertext: &[u8],
        nonce: &[u8],
    ) -> Result<bool, HandlerRejection> {
        let result = sqlx::query(
            r#"
            INSERT INTO pii_token_vault_t (
                host_id, token, scheme_id, value_hash, value_ciphertext, value_nonce, key_id
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(host_id)
        .bind(token)
        .bind(scheme_id)
        .bind(value_hash)
        .bind(ciphertext)
        .bind(nonce)
        .bind(self.keyring.key_id.as_str())
        .execute(&self.pool)
        .await
        .map_err(database_rejection)?;
        Ok(result.rows_affected() == 1)
    }

    async fn select_token_row(
        &self,
        host_id: Uuid,
        token: &str,
    ) -> Result<Option<PiiTokenRow>, HandlerRejection> {
        let row = sqlx::query(
            r#"
            SELECT value_ciphertext, value_nonce
            FROM pii_token_vault_t
            WHERE host_id = $1
              AND token = $2
              AND active = TRUE
              AND (expires_ts IS NULL OR expires_ts > CURRENT_TIMESTAMP)
            LIMIT 1
            "#,
        )
        .bind(host_id)
        .bind(token)
        .fetch_optional(&self.pool)
        .await
        .map_err(database_rejection)?;

        row.map(|row| {
            Ok(PiiTokenRow {
                value_ciphertext: row
                    .try_get::<Vec<u8>, _>("value_ciphertext")
                    .map_err(database_rejection)?,
                value_nonce: row
                    .try_get::<Vec<u8>, _>("value_nonce")
                    .map_err(database_rejection)?,
            })
        })
        .transpose()
    }
}

pub fn load_pii_tokenization_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<PiiTokenizationRuntime>, RuntimeError> {
    if !active {
        if let Some(registry) = runtime_config.cache_registry.as_ref() {
            registry.unregister(PII_TOKENIZATION_CACHE_NAME);
        }
        return Ok(None);
    }

    let config = load_pii_tokenization_config(runtime_config)?;
    runtime_config.module_registry.register_loaded_config(
        PII_TOKENIZATION_MODULE_ID,
        PII_TOKENIZATION_CONFIG_NAME,
        ModuleKind::Framework,
        &config,
        [
            MaskSpec::key("valueEncryptionKey"),
            MaskSpec::key("valueHashKey"),
            MaskSpec::key("url"),
        ],
        true,
        None,
        true,
    )?;

    let compiled_rules = compile_rules(&config)?;
    let keyring = Arc::new(PiiKeyring::from_config(&config.crypto)?);
    let pool = connect_pool(&config.database)?;
    let cache = Arc::new(PiiTokenCache::new(&config.cache));
    if let Some(registry) = runtime_config.cache_registry.as_ref() {
        registry.register_arc(
            PII_TOKENIZATION_CACHE_NAME,
            Arc::clone(&cache) as Arc<dyn RuntimeCache>,
        );
    }

    Ok(Some(PiiTokenizationRuntime {
        config: Arc::new(config),
        pool,
        keyring,
        cache,
        rules: Arc::new(compiled_rules),
    }))
}

fn load_pii_tokenization_config(
    runtime_config: &RuntimeConfig,
) -> Result<PiiTokenizationConfig, RuntimeError> {
    for file in [PII_TOKENIZATION_FILE, PII_TOKENIZATION_LEGACY_FILE] {
        match runtime_config
            .module_registry
            .load_config::<PiiTokenizationConfig>(runtime_config, file)
        {
            Ok(config) => return Ok(config),
            Err(RuntimeError::MissingConfig(missing)) if missing == file => continue,
            Err(error) => return Err(error),
        }
    }
    Err(RuntimeError::MissingConfig(
        PII_TOKENIZATION_FILE.to_string(),
    ))
}

fn connect_pool(config: &PiiDatabaseConfig) -> Result<PgPool, RuntimeError> {
    let url = database_url(config)?;
    let options = PgConnectOptions::from_str(url.as_str()).map_err(|error| {
        RuntimeError::Config(format!("invalid pii-tokenization database.url: {error}"))
    })?;
    let future = PgPoolOptions::new()
        .max_connections(config.max_connections.max(1))
        .min_connections(config.min_connections.min(config.max_connections.max(1)))
        .acquire_timeout(Duration::from_millis(config.connect_timeout_ms))
        .connect_with(options);
    block_on_runtime(future).map_err(|error| {
        RuntimeError::Config(format!(
            "failed to connect pii-tokenization database: {error}"
        ))
    })
}

fn block_on_runtime<F, T, E>(future: F) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build temporary tokio runtime")
            .block_on(future)
    }
}

fn database_url(config: &PiiDatabaseConfig) -> Result<String, RuntimeError> {
    std::env::var(DEFAULT_DATABASE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("DATABASE_URL").ok())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            let url = config.url.trim();
            (!url.is_empty()).then(|| url.to_string())
        })
        .ok_or_else(|| {
            RuntimeError::Config("pii-tokenization database.url is required".to_string())
        })
}

#[derive(Debug, Clone)]
struct CompiledPiiRule {
    path_prefix: String,
    methods: BTreeSet<String>,
    request: Vec<CompiledPiiFieldRule>,
    response: Vec<CompiledPiiFieldRule>,
}

impl CompiledPiiRule {
    fn matches(&self, path: &str, method: &str) -> bool {
        path_matches_prefix(path, self.path_prefix.as_str())
            && (self.methods.is_empty() || self.methods.contains(&method.to_ascii_uppercase()))
    }
}

#[derive(Debug, Clone)]
struct CompiledPiiFieldRule {
    path: CompiledFieldPath,
    scheme: TokenScheme,
    required: bool,
}

fn compile_rules(config: &PiiTokenizationConfig) -> Result<Vec<CompiledPiiRule>, RuntimeError> {
    if config.max_body_size == 0 {
        return Err(RuntimeError::Config(
            "pii-tokenization maxBodySize must be greater than zero".to_string(),
        ));
    }
    config
        .rules
        .iter()
        .map(|rule| {
            let path_prefix = normalize_path_prefix(rule.path_prefix.as_str())?;
            let methods = rule
                .methods
                .iter()
                .map(|method| method.trim().to_ascii_uppercase())
                .filter(|method| !method.is_empty())
                .collect::<BTreeSet<_>>();
            let request = compile_fields(&rule.request)?;
            let response = compile_fields(&rule.response)?;
            Ok(CompiledPiiRule {
                path_prefix,
                methods,
                request,
                response,
            })
        })
        .collect()
}

fn compile_fields(fields: &[PiiFieldRule]) -> Result<Vec<CompiledPiiFieldRule>, RuntimeError> {
    fields
        .iter()
        .map(|field| {
            Ok(CompiledPiiFieldRule {
                path: CompiledFieldPath::parse(field.path.as_str())?,
                scheme: TokenScheme::parse(field.scheme.as_str())?,
                required: field.required,
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompiledFieldPath {
    segments: Vec<PathSegment>,
}

impl CompiledFieldPath {
    fn parse(path: &str) -> Result<Self, RuntimeError> {
        let path = path.trim();
        if !path.starts_with('$') {
            return Err(RuntimeError::Config(format!(
                "pii-tokenization field path `{path}` must start with `$`"
            )));
        }
        let mut chars = path.char_indices().peekable();
        chars.next();
        let mut segments = Vec::new();
        while let Some((index, ch)) = chars.next() {
            match ch {
                '.' => {
                    let start = index + 1;
                    let mut end = start;
                    while let Some((next_index, next_ch)) = chars.peek().copied() {
                        if next_ch == '.' || next_ch == '[' {
                            break;
                        }
                        end = next_index + next_ch.len_utf8();
                        chars.next();
                    }
                    if end <= start {
                        return Err(RuntimeError::Config(format!(
                            "pii-tokenization field path `{path}` contains an empty field"
                        )));
                    }
                    let field = &path[start..end];
                    if !field
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
                    {
                        return Err(RuntimeError::Config(format!(
                            "pii-tokenization field path `{path}` contains unsupported field `{field}`"
                        )));
                    }
                    segments.push(PathSegment::Field(field.to_string()));
                }
                '[' => {
                    let Some((_, '*')) = chars.next() else {
                        return Err(RuntimeError::Config(format!(
                            "pii-tokenization field path `{path}` only supports `[*]` arrays"
                        )));
                    };
                    let Some((_, ']')) = chars.next() else {
                        return Err(RuntimeError::Config(format!(
                            "pii-tokenization field path `{path}` has an unterminated `[*]` segment"
                        )));
                    };
                    segments.push(PathSegment::ArrayWildcard);
                }
                _ => {
                    return Err(RuntimeError::Config(format!(
                        "pii-tokenization field path `{path}` has unexpected character `{ch}`"
                    )));
                }
            }
        }
        if segments.is_empty() {
            return Err(RuntimeError::Config(
                "pii-tokenization field path `$` is too broad".to_string(),
            ));
        }
        Ok(Self { segments })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PathSegment {
    Field(String),
    ArrayWildcard,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldMatches {
    values: Vec<StringMatch>,
    non_string_matches: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StringMatch {
    location: Vec<JsonLocation>,
    value: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum JsonLocation {
    Field(String),
    Index(usize),
}

fn collect_string_matches(payload: &JsonValue, path: &CompiledFieldPath) -> FieldMatches {
    let mut matches = FieldMatches {
        values: Vec::new(),
        non_string_matches: 0,
    };
    let mut location = Vec::new();
    collect_string_matches_inner(
        payload,
        path.segments.as_slice(),
        &mut location,
        &mut matches,
    );
    matches
}

fn collect_string_matches_inner(
    value: &JsonValue,
    segments: &[PathSegment],
    location: &mut Vec<JsonLocation>,
    matches: &mut FieldMatches,
) {
    let Some((segment, remaining)) = segments.split_first() else {
        if let Some(value) = value.as_str() {
            matches.values.push(StringMatch {
                location: location.clone(),
                value: value.to_string(),
            });
        } else {
            matches.non_string_matches += 1;
        }
        return;
    };

    match segment {
        PathSegment::Field(name) => {
            let Some(object) = value.as_object() else {
                return;
            };
            let Some(next) = object.get(name) else {
                return;
            };
            location.push(JsonLocation::Field(name.clone()));
            collect_string_matches_inner(next, remaining, location, matches);
            location.pop();
        }
        PathSegment::ArrayWildcard => {
            let Some(values) = value.as_array() else {
                return;
            };
            for (index, next) in values.iter().enumerate() {
                location.push(JsonLocation::Index(index));
                collect_string_matches_inner(next, remaining, location, matches);
                location.pop();
            }
        }
    }
}

fn validate_field_matches(
    matches: &FieldMatches,
    required: bool,
    operation: &str,
) -> Result<(), HandlerRejection> {
    if required && matches.values.is_empty() {
        return Err(HandlerRejection::new(
            400,
            "ERR13006",
            format!("required PII field missing for {operation}"),
        ));
    }
    if required && matches.non_string_matches > 0 {
        return Err(HandlerRejection::new(
            400,
            "ERR13007",
            format!("required PII field must be a string for {operation}"),
        ));
    }
    Ok(())
}

fn set_json_string(
    payload: &mut JsonValue,
    location: &[JsonLocation],
    replacement: String,
) -> Result<(), HandlerRejection> {
    let mut current = payload;
    for segment in location {
        match segment {
            JsonLocation::Field(name) => {
                current = current
                    .as_object_mut()
                    .and_then(|object| object.get_mut(name))
                    .ok_or_else(|| {
                        HandlerRejection::new(500, "ERR13008", "failed to update PII field")
                    })?;
            }
            JsonLocation::Index(index) => {
                current = current
                    .as_array_mut()
                    .and_then(|values| values.get_mut(*index))
                    .ok_or_else(|| {
                        HandlerRejection::new(500, "ERR13008", "failed to update PII field")
                    })?;
            }
        }
    }
    *current = JsonValue::String(replacement);
    Ok(())
}

fn parse_json_body(body: &[u8], label: &str) -> Result<JsonValue, HandlerRejection> {
    serde_json::from_slice::<JsonValue>(body).map_err(|_| {
        HandlerRejection::new(400, "ERR13009", format!("invalid JSON body for {label}"))
    })
}

fn claim_value<'a>(claims: &'a JsonValue, claim: &str) -> Option<&'a str> {
    let mut current = claims;
    for part in claim.split('.') {
        current = current.get(part)?;
    }
    current.as_str()
}

#[derive(Debug)]
struct PiiKeyring {
    key_id: String,
    encryption_key: LessSafeKey,
    hash_key: Vec<u8>,
}

impl PiiKeyring {
    fn from_config(config: &PiiTokenCryptoConfig) -> Result<Self, RuntimeError> {
        if !config.algorithm.eq_ignore_ascii_case("AES-256-GCM") {
            return Err(RuntimeError::Config(format!(
                "pii-tokenization crypto.algorithm `{}` is not supported",
                config.algorithm
            )));
        }
        let key_id = non_empty(config.key_id.as_str()).unwrap_or_else(default_key_id);
        let encryption_key = resolve_secret(
            config.value_encryption_key_env.as_str(),
            config.value_encryption_key.as_str(),
            Some(32),
            "valueEncryptionKey",
        )?;
        let hash_key = resolve_secret(
            config.value_hash_key_env.as_str(),
            config.value_hash_key.as_str(),
            None,
            "valueHashKey",
        )?;
        let unbound = UnboundKey::new(&AES_256_GCM, encryption_key.as_slice()).map_err(|_| {
            RuntimeError::Config("pii-tokenization valueEncryptionKey must be 32 bytes".to_string())
        })?;
        Ok(Self {
            key_id,
            encryption_key: LessSafeKey::new(unbound),
            hash_key,
        })
    }

    fn value_hash(&self, host_id: Uuid, scheme_id: i16, value: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(self.hash_key.as_slice())
            .expect("HMAC accepts keys of any size");
        mac.update(host_id.as_bytes());
        mac.update(&scheme_id.to_be_bytes());
        mac.update(value.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    fn encrypt(&self, value: &str) -> Result<(Vec<u8>, Vec<u8>), HandlerRejection> {
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let mut in_out = value.as_bytes().to_vec();
        let tag = self
            .encryption_key
            .seal_in_place_separate_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| HandlerRejection::new(500, "ERR13010", "PII encryption failed"))?;
        in_out.extend_from_slice(tag.as_ref());
        Ok((in_out, nonce.to_vec()))
    }

    fn decrypt(&self, ciphertext: &[u8], nonce: &[u8]) -> Result<String, HandlerRejection> {
        let nonce = nonce.try_into().map_err(|_| {
            HandlerRejection::new(500, "ERR13011", "PII ciphertext nonce is invalid")
        })?;
        let mut in_out = ciphertext.to_vec();
        let cleartext = self
            .encryption_key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| HandlerRejection::new(500, "ERR13012", "PII decryption failed"))?;
        String::from_utf8(cleartext.to_vec())
            .map_err(|_| HandlerRejection::new(500, "ERR13013", "PII cleartext is not valid UTF-8"))
    }
}

fn resolve_secret(
    env_name: &str,
    config_value: &str,
    expected_len: Option<usize>,
    field: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let value = non_empty(env_name)
        .and_then(|env_name| std::env::var(env_name).ok())
        .and_then(|value| non_empty(value.as_str()))
        .or_else(|| non_empty(config_value))
        .ok_or_else(|| {
            RuntimeError::Config(format!("pii-tokenization crypto.{field} is required"))
        })?;
    decode_secret(value.as_str(), expected_len).map_err(|message| {
        RuntimeError::Config(format!(
            "pii-tokenization crypto.{field} is invalid: {message}"
        ))
    })
}

fn decode_secret(value: &str, expected_len: Option<usize>) -> Result<Vec<u8>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("empty secret".to_string());
    }
    if expected_len.is_some_and(|len| value.as_bytes().len() == len) {
        return Ok(value.as_bytes().to_vec());
    }
    if value.len() % 2 == 0 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        if let Ok(decoded) = hex::decode(value)
            && expected_len.is_none_or(|len| decoded.len() == len)
        {
            return Ok(decoded);
        }
    }
    for engine in [&STANDARD, &URL_SAFE_NO_PAD] {
        if let Ok(decoded) = engine.decode(value)
            && expected_len.is_none_or(|len| decoded.len() == len)
        {
            return Ok(decoded);
        }
    }
    if expected_len.is_none() {
        return Ok(value.as_bytes().to_vec());
    }
    Err(format!(
        "expected {} bytes as raw text, hex, or base64",
        expected_len.unwrap_or_default()
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScheme {
    Uuid,
    Guid,
    Luhn,
    Numeric,
    LuhnLast4,
    AlphaNumeric,
    AlphaNumericLast4,
    CreditCard,
    CreditCardLast4,
}

impl TokenScheme {
    fn parse(value: &str) -> Result<Self, RuntimeError> {
        match value.trim().to_ascii_uppercase().as_str() {
            "0" | "UUID" => Ok(Self::Uuid),
            "1" | "GUID" => Ok(Self::Guid),
            "2" | "LN" => Ok(Self::Luhn),
            "3" | "N" => Ok(Self::Numeric),
            "4" | "LN4" => Ok(Self::LuhnLast4),
            "5" | "AN" => Ok(Self::AlphaNumeric),
            "6" | "AN4" => Ok(Self::AlphaNumericLast4),
            "7" | "CC" => Ok(Self::CreditCard),
            "8" | "CC4" => Ok(Self::CreditCardLast4),
            scheme => Err(RuntimeError::Config(format!(
                "unknown pii-tokenization scheme `{scheme}`"
            ))),
        }
    }

    fn id(self) -> i16 {
        match self {
            Self::Uuid => 0,
            Self::Guid => 1,
            Self::Luhn => 2,
            Self::Numeric => 3,
            Self::LuhnLast4 => 4,
            Self::AlphaNumeric => 5,
            Self::AlphaNumericLast4 => 6,
            Self::CreditCard => 7,
            Self::CreditCardLast4 => 8,
        }
    }

    fn generate(self, value: &str) -> Result<String, HandlerRejection> {
        match self {
            Self::Uuid => Ok(Uuid::new_v4().to_string()),
            Self::Guid => Ok(URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes())),
            Self::Luhn => generate_luhn_token("", value.len()),
            Self::Numeric => generate_numeric(value.len()),
            Self::LuhnLast4 => generate_luhn_last4(value),
            Self::AlphaNumeric => generate_alpha_numeric(value.len()),
            Self::AlphaNumericLast4 => generate_alpha_numeric_last4(value),
            Self::CreditCard => generate_credit_card(value),
            Self::CreditCardLast4 => generate_credit_card_last4(value),
        }
    }
}

fn generate_luhn_token(prefix: &str, length: usize) -> Result<String, HandlerRejection> {
    if length <= prefix.len() {
        return Err(invalid_value_for_scheme());
    }
    let random_len = length - prefix.len() - 1;
    let mut value = String::with_capacity(length);
    value.push_str(prefix);
    for _ in 0..random_len {
        value.push(char::from(b'0' + random_below(10)));
    }
    value.push(char::from(b'0' + luhn_check_digit(value.as_str())));
    Ok(value)
}

fn generate_numeric(length: usize) -> Result<String, HandlerRejection> {
    if length == 0 {
        return Err(invalid_value_for_scheme());
    }
    let mut token = String::with_capacity(length);
    token.push(char::from(b'1' + random_below(9)));
    for _ in 1..length {
        token.push(char::from(b'0' + random_below(10)));
    }
    Ok(token)
}

fn generate_luhn_last4(value: &str) -> Result<String, HandlerRejection> {
    if value.len() <= 4 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(invalid_value_for_scheme());
    }
    let suffix = &value[value.len() - 4..];
    for _ in 0..1024 {
        let prefix = generate_numeric(value.len() - 4)?;
        let candidate = format!("{prefix}{suffix}");
        if luhn_valid(candidate.as_str()) {
            return Ok(candidate);
        }
    }
    Err(HandlerRejection::new(
        500,
        "ERR13014",
        "failed to generate Luhn token",
    ))
}

fn generate_alpha_numeric(length: usize) -> Result<String, HandlerRejection> {
    if length == 0 {
        return Err(invalid_value_for_scheme());
    }
    let mut token = String::with_capacity(length);
    for _ in 0..length {
        token.push(ALPHA_NUMERIC[random_below(ALPHA_NUMERIC.len() as u8) as usize] as char);
    }
    Ok(token)
}

fn generate_alpha_numeric_last4(value: &str) -> Result<String, HandlerRejection> {
    if value.len() <= 4 {
        return Err(invalid_value_for_scheme());
    }
    Ok(format!(
        "{}{}",
        generate_alpha_numeric(value.len() - 4)?,
        &value[value.len() - 4..]
    ))
}

fn generate_credit_card(value: &str) -> Result<String, HandlerRejection> {
    if value.len() < 2 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(invalid_value_for_scheme());
    }
    generate_luhn_token(&value[..1], value.len())
}

fn generate_credit_card_last4(value: &str) -> Result<String, HandlerRejection> {
    if value.len() <= 5 || !value.chars().all(|ch| ch.is_ascii_digit()) {
        return Err(invalid_value_for_scheme());
    }
    let first = &value[..1];
    let suffix = &value[value.len() - 4..];
    for _ in 0..1024 {
        let middle = generate_numeric(value.len() - 5)?;
        let candidate = format!("{first}{middle}{suffix}");
        if luhn_valid(candidate.as_str()) {
            return Ok(candidate);
        }
    }
    Err(HandlerRejection::new(
        500,
        "ERR13014",
        "failed to generate Luhn token",
    ))
}

fn luhn_check_digit(number_without_check: &str) -> u8 {
    let mut sum = 0u32;
    let remainder = (number_without_check.len() + 1) % 2;
    for (index, digit) in number_without_check.bytes().enumerate() {
        let mut digit = u32::from(digit - b'0');
        if index % 2 == remainder {
            digit *= 2;
            if digit > 9 {
                digit = (digit / 10) + (digit % 10);
            }
        }
        sum += digit;
    }
    let modulo = sum % 10;
    if modulo == 0 { 0 } else { (10 - modulo) as u8 }
}

fn luhn_valid(value: &str) -> bool {
    let mut sum = 0u32;
    let mut alternate = false;
    for digit in value.bytes().rev() {
        if !digit.is_ascii_digit() {
            return false;
        }
        let mut digit = u32::from(digit - b'0');
        if alternate {
            digit *= 2;
            if digit > 9 {
                digit = (digit % 10) + 1;
            }
        }
        sum += digit;
        alternate = !alternate;
    }
    sum % 10 == 0
}

fn random_below(max: u8) -> u8 {
    let mut byte = [0u8; 1];
    OsRng.fill_bytes(&mut byte);
    byte[0] % max
}

fn invalid_value_for_scheme() -> HandlerRejection {
    HandlerRejection::new(
        400,
        "ERR13015",
        "PII value is invalid for configured scheme",
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ValueCacheKey {
    host_id: Uuid,
    scheme_id: i16,
    value_hash: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct TokenCacheKey {
    host_id: Uuid,
    token: String,
}

#[derive(Debug, Clone)]
struct CachedValue {
    value: String,
    expires_at: Instant,
}

pub struct PiiTokenCache {
    enabled: bool,
    cache_cleartext: bool,
    max_entries: usize,
    ttl: Duration,
    value_to_token: Mutex<BTreeMap<ValueCacheKey, CachedValue>>,
    token_to_value: Mutex<BTreeMap<TokenCacheKey, CachedValue>>,
}

impl PiiTokenCache {
    fn new(config: &PiiTokenCacheConfig) -> Self {
        Self {
            enabled: config.enabled && config.max_entries > 0,
            cache_cleartext: config.cache_cleartext,
            max_entries: config.max_entries,
            ttl: Duration::from_secs(config.ttl_seconds),
            value_to_token: Mutex::new(BTreeMap::new()),
            token_to_value: Mutex::new(BTreeMap::new()),
        }
    }

    async fn get_token(&self, key: &ValueCacheKey) -> Option<String> {
        self.get_from_map(&self.value_to_token, key).await
    }

    async fn insert_token(&self, key: ValueCacheKey, token: String) {
        self.insert_into_map(&self.value_to_token, key, token).await;
    }

    async fn get_cleartext(&self, key: &TokenCacheKey) -> Option<String> {
        if !self.cache_cleartext {
            return None;
        }
        self.get_from_map(&self.token_to_value, key).await
    }

    async fn insert_cleartext(&self, key: TokenCacheKey, cleartext: String) {
        if !self.cache_cleartext {
            return;
        }
        self.insert_into_map(&self.token_to_value, key, cleartext)
            .await;
    }

    async fn get_from_map<K>(
        &self,
        map: &Mutex<BTreeMap<K, CachedValue>>,
        key: &K,
    ) -> Option<String>
    where
        K: Ord + Clone,
    {
        if !self.enabled {
            return None;
        }
        let mut entries = map.lock().await;
        let Some(entry) = entries.get(key) else {
            return None;
        };
        if Instant::now() >= entry.expires_at {
            entries.remove(key);
            return None;
        }
        Some(entry.value.clone())
    }

    async fn insert_into_map<K>(&self, map: &Mutex<BTreeMap<K, CachedValue>>, key: K, value: String)
    where
        K: Ord + Clone,
    {
        if !self.enabled {
            return;
        }
        let mut entries = map.lock().await;
        if !entries.contains_key(&key)
            && entries.len() >= self.max_entries
            && let Some(evict_key) = entries
                .iter()
                .min_by_key(|(_, cached)| cached.expires_at)
                .map(|(key, _)| key.clone())
        {
            entries.remove(&evict_key);
        }
        entries.insert(
            key,
            CachedValue {
                value,
                expires_at: Instant::now() + self.ttl,
            },
        );
    }
}

#[async_trait::async_trait]
impl RuntimeCache for PiiTokenCache {
    async fn len(&self) -> usize {
        self.value_to_token.lock().await.len() + self.token_to_value.lock().await.len()
    }

    async fn entries_summary(&self) -> JsonValue {
        json!({
            "valueToTokenEntries": self.value_to_token.lock().await.len(),
            "tokenToValueEntries": self.token_to_value.lock().await.len(),
            "cleartextCacheEnabled": self.cache_cleartext,
            "maxEntries": self.max_entries
        })
    }

    async fn clear(&self) {
        self.value_to_token.lock().await.clear();
        self.token_to_value.lock().await.clear();
    }
}

struct PiiTokenRow {
    value_ciphertext: Vec<u8>,
    value_nonce: Vec<u8>,
}

fn database_rejection(error: sqlx::Error) -> HandlerRejection {
    tracing::warn!(target: "light_pingora::pii_tokenization", error = %error, "PII tokenization database operation failed");
    HandlerRejection::new(
        500,
        "ERR13016",
        "PII tokenization database operation failed",
    )
}

fn canonical_value(value: &str) -> &str {
    value
}

fn normalize_path_prefix(prefix: &str) -> Result<String, RuntimeError> {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return Ok("/".to_string());
    }
    if !prefix.starts_with('/') {
        return Err(RuntimeError::Config(format!(
            "pii-tokenization pathPrefix `{prefix}` must start with `/`"
        )));
    }
    Ok(if prefix.len() > 1 {
        prefix.trim_end_matches('/').to_string()
    } else {
        prefix.to_string()
    })
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    if prefix == "/" {
        return true;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn default_true() -> bool {
    true
}

fn default_host_id_claim() -> String {
    "host_id".to_string()
}

fn default_max_body_size() -> usize {
    DEFAULT_MAX_BODY_SIZE
}

fn default_max_connections() -> u32 {
    8
}

fn default_min_connections() -> u32 {
    1
}

fn default_connect_timeout_ms() -> u64 {
    2_000
}

fn default_crypto_algorithm() -> String {
    "AES-256-GCM".to_string()
}

fn default_key_id() -> String {
    "default".to_string()
}

fn default_value_encryption_key_env() -> String {
    DEFAULT_VALUE_ENCRYPTION_KEY_ENV.to_string()
}

fn default_value_hash_key_env() -> String {
    DEFAULT_VALUE_HASH_KEY_ENV.to_string()
}

fn default_cache_max_entries() -> usize {
    10_000
}

fn default_cache_ttl_seconds() -> u64 {
    86_400
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_schemes_generate_expected_shapes() {
        assert_eq!(TokenScheme::Uuid.generate("123").unwrap().len(), 36);
        assert_eq!(TokenScheme::Guid.generate("123").unwrap().len(), 22);
        assert_eq!(TokenScheme::Numeric.generate("123456").unwrap().len(), 6);

        let luhn = TokenScheme::Luhn.generate("123456789").unwrap();
        assert_eq!(luhn.len(), 9);
        assert!(luhn_valid(&luhn));

        let ln4 = TokenScheme::LuhnLast4.generate("123456789").unwrap();
        assert!(ln4.ends_with("6789"));
        assert!(luhn_valid(&ln4));

        let an4 = TokenScheme::AlphaNumericLast4
            .generate("abcdefghi")
            .unwrap();
        assert!(an4.ends_with("fghi"));

        let cc = TokenScheme::CreditCard
            .generate("4111111111111111")
            .unwrap();
        assert!(cc.starts_with('4'));
        assert!(luhn_valid(&cc));

        let cc4 = TokenScheme::CreditCardLast4
            .generate("4111111111111111")
            .unwrap();
        assert!(cc4.starts_with('4'));
        assert!(cc4.ends_with("1111"));
        assert!(luhn_valid(&cc4));
    }

    #[test]
    fn compiles_json_path_subset_and_rejects_broad_paths() {
        let path = CompiledFieldPath::parse("$.claims[*].ssn").unwrap();
        assert_eq!(
            path.segments,
            vec![
                PathSegment::Field("claims".to_string()),
                PathSegment::ArrayWildcard,
                PathSegment::Field("ssn".to_string())
            ]
        );
        assert!(CompiledFieldPath::parse("$").is_err());
        assert!(CompiledFieldPath::parse("$.claims[0].ssn").is_err());
        assert!(CompiledFieldPath::parse("$.claims..ssn").is_err());
    }

    #[test]
    fn json_path_collects_and_updates_array_values() {
        let path = CompiledFieldPath::parse("$.claims[*].ssn").unwrap();
        let mut payload = json!({
            "claims": [
                { "ssn": "123456789" },
                { "ssn": "987654321" },
                { "name": "missing" }
            ]
        });
        let matches = collect_string_matches(&payload, &path);
        assert_eq!(matches.values.len(), 2);
        set_json_string(
            &mut payload,
            &matches.values[0].location,
            "token-1".to_string(),
        )
        .unwrap();
        assert_eq!(payload["claims"][0]["ssn"], json!("token-1"));
    }

    #[test]
    fn config_supports_injected_rules_string() {
        let config = serde_yaml::from_str::<PiiTokenizationConfig>(
            r#"
database:
  url: postgres://user:pass@localhost/portal
crypto:
  valueEncryptionKey: "12345678901234567890123456789012"
  valueHashKey: "hash-key"
rules: '[{"pathPrefix":"/claims","methods":["POST"],"request":[{"path":"$.claimant.ssn","scheme":"LN"}],"response":[{"path":"$.claimant.ssn","scheme":"LN"}]}]'
"#,
        )
        .unwrap();
        let compiled = compile_rules(&config).unwrap();
        assert_eq!(compiled.len(), 1);
        assert!(compiled[0].matches("/claims/123", "POST"));
        assert!(!compiled[0].matches("/other", "POST"));
    }

    #[test]
    fn keyring_encrypts_and_decrypts_with_raw_dev_keys() {
        let keyring = PiiKeyring::from_config(&PiiTokenCryptoConfig {
            value_encryption_key: "12345678901234567890123456789012".to_string(),
            value_hash_key: "hash-key".to_string(),
            ..PiiTokenCryptoConfig::default()
        })
        .unwrap();
        let (ciphertext, nonce) = keyring.encrypt("123-45-6789").unwrap();
        assert_ne!(ciphertext, b"123-45-6789");
        assert_eq!(keyring.decrypt(&ciphertext, &nonce).unwrap(), "123-45-6789");
    }
}
