use crate::error::LlmGatewayError;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use model_provider::inference::{
    ContentBlock, InferenceEvent, InferenceRequest, InferenceResponse,
};
use rand::RngCore;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;
use sqlx::{PgPool, Row};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime};
use zeroize::Zeroizing;

type HmacSha256 = Hmac<Sha256>;
const TOKEN_PREFIX: &str = "[[PII:v1:";
const TOKEN_SUFFIX: &str = "]]";

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b").expect("email regex")
});
static US_SSN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[0-9]{3}-[0-9]{2}-[0-9]{4}\b").expect("SSN regex"));
static PHONE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\b(?:\+?1[-. ]?)?\(?[2-9][0-9]{2}\)?[-. ][0-9]{3}[-. ][0-9]{4}\b")
        .expect("phone regex")
});

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiScope {
    #[default]
    Request,
    Session,
    Host,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnresolvedPiiBehavior {
    #[default]
    LeaveMasked,
    RejectBuffered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiiKind {
    Email,
    UsSsn,
    Phone,
}

impl PiiKind {
    fn label(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::UsSsn => "us_ssn",
            Self::Phone => "phone",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiiProfile {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub scope: PiiScope,
    #[serde(default)]
    pub unresolved: UnresolvedPiiBehavior,
    #[serde(default)]
    pub kinds: BTreeSet<PiiKind>,
    #[serde(default = "default_max_placeholders")]
    pub max_placeholders: usize,
    #[serde(default = "default_max_value_bytes")]
    pub max_value_bytes: usize,
    #[serde(default = "default_detector_version")]
    pub detector_version: String,
    #[serde(default = "default_token_format_version")]
    pub token_format_version: String,
    #[serde(default = "default_preservation_threshold")]
    pub minimum_placeholder_preservation_percent: u8,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PiiPromotionAttestation {
    schema_version: String,
    physical_model: String,
    detector_version: String,
    token_format_version: String,
    scope: PiiScope,
    vault_implementation_version: String,
    placeholder_preservation_percent: u8,
    valid_until: DateTime<Utc>,
    lanes: PiiPromotionLanes,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PiiPromotionLanes {
    functional: PiiPromotionLane,
    security: PiiPromotionLane,
    durability: PiiPromotionLane,
    performance: PiiPromotionLane,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PiiPromotionLane {
    state: PiiPromotionLaneState,
    digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PiiPromotionLaneState {
    Pass,
}

impl Default for PiiProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            scope: PiiScope::Request,
            unresolved: UnresolvedPiiBehavior::LeaveMasked,
            kinds: BTreeSet::new(),
            max_placeholders: default_max_placeholders(),
            max_value_bytes: default_max_value_bytes(),
            detector_version: default_detector_version(),
            token_format_version: default_token_format_version(),
            minimum_placeholder_preservation_percent: default_preservation_threshold(),
        }
    }
}

impl PiiProfile {
    pub fn id(&self) -> String {
        if !self.enabled {
            return "none".to_string();
        }
        format!(
            "{}:{}:{}",
            self.detector_version,
            self.token_format_version,
            match self.scope {
                PiiScope::Request => "request",
                PiiScope::Session => "session",
                PiiScope::Host => "host",
            }
        )
    }

    pub fn validate(&self) -> Result<(), LlmGatewayError> {
        if !self.enabled {
            return Ok(());
        }
        if self.scope != PiiScope::Request {
            return Err(LlmGatewayError::Config(
                "session/host PII requires a separately promoted durable vault".to_string(),
            ));
        }
        if self.kinds.is_empty()
            || self.max_placeholders == 0
            || self.max_value_bytes == 0
            || self.detector_version.is_empty()
            || self.token_format_version != "v1"
            || self.minimum_placeholder_preservation_percent == 0
            || self.minimum_placeholder_preservation_percent > 100
        {
            return Err(LlmGatewayError::Config(
                "invalid request-scoped PII profile".to_string(),
            ));
        }
        Ok(())
    }
}

/// Validates the independent PII promotion lanes carried inside the signed
/// deployment conformance result. The identity tuple must match the exact
/// deployment and alias profile; a bare percentage is never production
/// evidence.
pub(crate) fn validate_pii_promotion(
    profile: &PiiProfile,
    physical_model: &str,
    evidence: Option<&Value>,
    now: DateTime<Utc>,
) -> Result<(), LlmGatewayError> {
    if !profile.enabled {
        return Ok(());
    }
    let evidence = evidence.ok_or_else(|| {
        LlmGatewayError::Config(format!(
            "PII profile `{}` has no promotion attestation for model `{physical_model}`",
            profile.id()
        ))
    })?;
    let attestation: PiiPromotionAttestation = serde_json::from_value(evidence.clone())
        .map_err(|_| LlmGatewayError::Config("invalid PII promotion attestation".to_string()))?;
    let lanes = [
        &attestation.lanes.functional,
        &attestation.lanes.security,
        &attestation.lanes.durability,
        &attestation.lanes.performance,
    ];
    let valid_lane = |lane: &PiiPromotionLane| {
        lane.state == PiiPromotionLaneState::Pass
            && lane.digest.len() == 64
            && lane
                .digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    };
    let expected_vault = match profile.scope {
        PiiScope::Request => "none",
        PiiScope::Session | PiiScope::Host => {
            return Err(LlmGatewayError::Config(
                "session/host PII requires a separately promoted durable vault".to_string(),
            ));
        }
    };
    if attestation.schema_version != "1"
        || attestation.physical_model != physical_model
        || attestation.detector_version != profile.detector_version
        || attestation.token_format_version != profile.token_format_version
        || attestation.scope != profile.scope
        || attestation.vault_implementation_version != expected_vault
        || attestation.placeholder_preservation_percent
            < profile.minimum_placeholder_preservation_percent
        || now >= attestation.valid_until
        || !lanes.into_iter().all(valid_lane)
    {
        return Err(LlmGatewayError::Config(format!(
            "PII profile `{}` is not independently promoted for model `{physical_model}`",
            profile.id()
        )));
    }
    Ok(())
}

fn default_max_placeholders() -> usize {
    128
}
fn default_max_value_bytes() -> usize {
    4096
}
fn default_detector_version() -> String {
    "local-regex-v1".to_string()
}
fn default_token_format_version() -> String {
    "v1".to_string()
}
fn default_preservation_threshold() -> u8 {
    100
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PiiTransformStats {
    pub detected: usize,
    pub unique: usize,
    pub recovered: usize,
    pub unresolved: usize,
}

struct Mapping {
    value: Zeroizing<String>,
}

pub struct RequestPiiSession {
    profile: PiiProfile,
    key: Zeroizing<Vec<u8>>,
    by_token: BTreeMap<String, Mapping>,
    counter: u64,
    stats: PiiTransformStats,
}

impl RequestPiiSession {
    pub fn new(profile: PiiProfile) -> Result<Self, LlmGatewayError> {
        profile.validate()?;
        let mut key = Zeroizing::new(vec![0_u8; 32]);
        rand::rngs::OsRng.fill_bytes(&mut key);
        Ok(Self {
            profile,
            key,
            by_token: BTreeMap::new(),
            counter: 0,
            stats: PiiTransformStats::default(),
        })
    }

    pub fn profile_id(&self) -> String {
        self.profile.id()
    }

    pub fn stats(&self) -> &PiiTransformStats {
        &self.stats
    }

    pub fn tokenize_request(
        &mut self,
        request: &mut InferenceRequest,
    ) -> Result<(), LlmGatewayError> {
        if !self.profile.enabled {
            return Ok(());
        }
        for message in &mut request.messages {
            self.tokenize_blocks(&mut message.content)?;
        }
        Ok(())
    }

    pub fn recover_response(
        &mut self,
        response: &mut InferenceResponse,
    ) -> Result<(), LlmGatewayError> {
        if !self.profile.enabled {
            return Ok(());
        }
        self.recover_blocks(&mut response.content)?;
        Ok(())
    }

    pub fn stream_recoverer(self) -> PiiStreamRecoverer {
        PiiStreamRecoverer {
            session: self,
            text_pending: String::new(),
            tool_pending: BTreeMap::new(),
        }
    }

    fn tokenize_blocks(&mut self, blocks: &mut [ContentBlock]) -> Result<(), LlmGatewayError> {
        for block in blocks {
            match block {
                ContentBlock::Text { text } => *text = self.tokenize_text(text)?,
                ContentBlock::ToolCall { call } => self.tokenize_json(&mut call.arguments)?,
                ContentBlock::ToolResult { result } => self.tokenize_blocks(&mut result.content)?,
                ContentBlock::Image { .. } => {}
            }
        }
        Ok(())
    }

    fn recover_blocks(&mut self, blocks: &mut [ContentBlock]) -> Result<(), LlmGatewayError> {
        for block in blocks {
            match block {
                ContentBlock::Text { text } => *text = self.recover_text(text)?,
                ContentBlock::ToolCall { call } => self.recover_json(&mut call.arguments)?,
                ContentBlock::ToolResult { result } => self.recover_blocks(&mut result.content)?,
                ContentBlock::Image { .. } => {}
            }
        }
        Ok(())
    }

    fn tokenize_json(&mut self, value: &mut Value) -> Result<(), LlmGatewayError> {
        match value {
            Value::String(text) => *text = self.tokenize_text(text)?,
            Value::Array(items) => {
                for item in items {
                    self.tokenize_json(item)?;
                }
            }
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    self.tokenize_json(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn recover_json(&mut self, value: &mut Value) -> Result<(), LlmGatewayError> {
        match value {
            Value::String(text) => *text = self.recover_text(text)?,
            Value::Array(items) => {
                for item in items {
                    self.recover_json(item)?;
                }
            }
            Value::Object(fields) => {
                for value in fields.values_mut() {
                    self.recover_json(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn tokenize_text(&mut self, text: &str) -> Result<String, LlmGatewayError> {
        let mut detections = Vec::<(usize, usize, PiiKind)>::new();
        for kind in &self.profile.kinds {
            let regex = match kind {
                PiiKind::Email => &*EMAIL,
                PiiKind::UsSsn => &*US_SSN,
                PiiKind::Phone => &*PHONE,
            };
            detections.extend(
                regex
                    .find_iter(text)
                    .map(|found| (found.start(), found.end(), *kind)),
            );
        }
        detections.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
        let mut output = String::with_capacity(text.len());
        let mut cursor = 0;
        for (start, end, kind) in detections {
            if start < cursor {
                continue;
            }
            let value = &text[start..end];
            if value.len() > self.profile.max_value_bytes {
                return Err(LlmGatewayError::InvalidRequest(
                    "detected PII value exceeds the configured bound".to_string(),
                ));
            }
            output.push_str(&text[cursor..start]);
            output.push_str(&self.token_for(kind, value)?);
            cursor = end;
            self.stats.detected = self.stats.detected.saturating_add(1);
        }
        output.push_str(&text[cursor..]);
        Ok(output)
    }

    fn token_for(&mut self, kind: PiiKind, value: &str) -> Result<String, LlmGatewayError> {
        // Reusing a token for a repeated value intentionally preserves entity
        // equality for model reasoning while revealing neither value.
        if let Some((token, _)) = self
            .by_token
            .iter()
            .find(|(_, mapping)| mapping.value.as_str() == value)
        {
            return Ok(token.to_string());
        }
        if self.by_token.len() >= self.profile.max_placeholders {
            return Err(LlmGatewayError::InvalidRequest(
                "PII placeholder limit exceeded".to_string(),
            ));
        }
        self.counter = self.counter.saturating_add(1);
        let mut mac = HmacSha256::new_from_slice(&self.key)
            .map_err(|_| LlmGatewayError::InvalidRequest("PII tokenization failed".to_string()))?;
        mac.update(kind.label().as_bytes());
        mac.update(&self.counter.to_be_bytes());
        mac.update(value.as_bytes());
        let digest = mac.finalize().into_bytes();
        let tag = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let token = format!("{TOKEN_PREFIX}{}:{tag}{TOKEN_SUFFIX}", kind.label());
        self.by_token.insert(
            token.clone(),
            Mapping {
                value: Zeroizing::new(value.to_string()),
            },
        );
        self.stats.unique = self.stats.unique.saturating_add(1);
        Ok(token)
    }

    fn recover_text(&mut self, text: &str) -> Result<String, LlmGatewayError> {
        let mut output = text.to_string();
        for (token, mapping) in &self.by_token {
            let count = output.matches(token).count();
            if count > 0 {
                output = output.replace(token, &mapping.value);
                self.stats.recovered = self.stats.recovered.saturating_add(count);
            }
        }
        if output.contains(TOKEN_PREFIX) {
            self.stats.unresolved = self.stats.unresolved.saturating_add(1);
            if self.profile.unresolved == UnresolvedPiiBehavior::RejectBuffered {
                return Err(LlmGatewayError::ProviderUnavailable);
            }
        }
        Ok(output)
    }
}

pub struct PiiStreamRecoverer {
    session: RequestPiiSession,
    text_pending: String,
    tool_pending: BTreeMap<u32, String>,
}

impl PiiStreamRecoverer {
    pub fn recover(
        &mut self,
        event: InferenceEvent,
    ) -> Result<Option<InferenceEvent>, LlmGatewayError> {
        match event {
            InferenceEvent::TextDelta { text } => {
                let recovered =
                    recover_stream_fragment(&mut self.session, &mut self.text_pending, &text)?;
                Ok(
                    (!recovered.is_empty())
                        .then_some(InferenceEvent::TextDelta { text: recovered }),
                )
            }
            InferenceEvent::ToolCallDelta { mut delta } => {
                let pending = self.tool_pending.entry(delta.index).or_default();
                let recovered =
                    recover_stream_fragment(&mut self.session, pending, &delta.arguments_fragment)?;
                delta.arguments_fragment = recovered;
                Ok((delta.id.is_some()
                    || delta.name.is_some()
                    || !delta.arguments_fragment.is_empty())
                .then_some(InferenceEvent::ToolCallDelta { delta }))
            }
            other => Ok(Some(other)),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<InferenceEvent>, LlmGatewayError> {
        let mut events = Vec::new();
        if !self.text_pending.is_empty() {
            let text = self
                .session
                .recover_text(&std::mem::take(&mut self.text_pending))?;
            if !text.is_empty() {
                events.push(InferenceEvent::TextDelta { text });
            }
        }
        for (index, pending) in std::mem::take(&mut self.tool_pending) {
            let arguments_fragment = self.session.recover_text(&pending)?;
            if !arguments_fragment.is_empty() {
                events.push(InferenceEvent::ToolCallDelta {
                    delta: model_provider::inference::ToolCallDelta {
                        index,
                        id: None,
                        name: None,
                        arguments_fragment,
                    },
                });
            }
        }
        Ok(events)
    }

    pub fn stats(&self) -> &PiiTransformStats {
        self.session.stats()
    }
}

fn recover_stream_fragment(
    session: &mut RequestPiiSession,
    pending: &mut String,
    fragment: &str,
) -> Result<String, LlmGatewayError> {
    pending.push_str(fragment);
    let keep = session
        .by_token
        .keys()
        .map(|token| longest_suffix_prefix(pending, token))
        .max()
        .unwrap_or(0);
    let split = pending.len().saturating_sub(keep);
    // Tokens and their prefixes are ASCII; this only protects preceding UTF-8 text.
    if !pending.is_char_boundary(split) {
        return Err(LlmGatewayError::ProviderUnavailable);
    }
    let ready = pending[..split].to_string();
    *pending = pending[split..].to_string();
    session.recover_text(&ready)
}

fn longest_suffix_prefix(value: &str, token: &str) -> usize {
    let max = value.len().min(token.len().saturating_sub(1));
    (1..=max)
        .rev()
        .find(|length| value.ends_with(&token[..*length]))
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiVaultScope {
    pub host_id: String,
    pub scope_id: String,
}

#[derive(Debug)]
pub struct PiiVaultEntry {
    pub token: String,
    pub value: Zeroizing<String>,
    pub expires_at: SystemTime,
    pub key_reference: String,
}

#[async_trait]
pub trait PiiVault: Send + Sync {
    async fn insert_exact(
        &self,
        scope: &PiiVaultScope,
        entry: PiiVaultEntry,
    ) -> Result<(), LlmGatewayError>;
    async fn resolve_exact(
        &self,
        scope: &PiiVaultScope,
        token: &str,
    ) -> Result<Option<Zeroizing<String>>, LlmGatewayError>;
    async fn revoke_exact(&self, scope: &PiiVaultScope, token: &str)
    -> Result<(), LlmGatewayError>;
    async fn expire_before(&self, deadline: SystemTime) -> Result<u64, LlmGatewayError>;
    fn operation_timeout(&self) -> Duration;
}

pub trait PiiVaultCipher: Send + Sync {
    fn encrypt(&self, key_reference: &str, plaintext: &str) -> Result<Vec<u8>, LlmGatewayError>;
    fn decrypt(
        &self,
        key_reference: &str,
        ciphertext: &[u8],
    ) -> Result<Zeroizing<String>, LlmGatewayError>;
}

pub struct PostgresPiiVault {
    pool: PgPool,
    cipher: std::sync::Arc<dyn PiiVaultCipher>,
    index_key: Zeroizing<Vec<u8>>,
    gateway_instance: String,
    timeout: Duration,
}

impl PostgresPiiVault {
    pub fn new(
        pool: PgPool,
        cipher: std::sync::Arc<dyn PiiVaultCipher>,
        index_key: Zeroizing<Vec<u8>>,
        gateway_instance: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, LlmGatewayError> {
        if index_key.len() < 32 || timeout.is_zero() {
            return Err(LlmGatewayError::Config(
                "invalid durable PII vault cryptographic or timeout bounds".to_string(),
            ));
        }
        let gateway_instance = gateway_instance.into();
        if gateway_instance.is_empty() {
            return Err(LlmGatewayError::Config(
                "durable PII vault gateway instance is required".to_string(),
            ));
        }
        Ok(Self {
            pool,
            cipher,
            index_key,
            gateway_instance,
            timeout,
        })
    }

    fn digest(&self, domain: &[u8], value: &str) -> Result<String, LlmGatewayError> {
        let mut mac = HmacSha256::new_from_slice(&self.index_key)
            .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
        mac.update(domain);
        mac.update(&[0]);
        mac.update(value.as_bytes());
        Ok(mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    async fn access_audit(
        transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        gateway_instance: &str,
        operation: &str,
        host_digest: &str,
        scope_digest: &str,
        token_digest: Option<&str>,
        outcome: &str,
    ) -> Result<(), LlmGatewayError> {
        sqlx::query("SELECT llm_pii_vault_record_access($1,$2,$3,$4,$5,$6,$7)")
            .bind(uuid::Uuid::now_v7())
            .bind(gateway_instance)
            .bind(operation)
            .bind(host_digest)
            .bind(scope_digest)
            .bind(token_digest)
            .bind(outcome)
            .execute(&mut **transaction)
            .await
            .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
        Ok(())
    }
}

#[async_trait]
impl PiiVault for PostgresPiiVault {
    async fn insert_exact(
        &self,
        scope: &PiiVaultScope,
        entry: PiiVaultEntry,
    ) -> Result<(), LlmGatewayError> {
        let host = self.digest(b"host", &scope.host_id)?;
        let scope_id = self.digest(b"scope", &scope.scope_id)?;
        let token = self.digest(b"token", &entry.token)?;
        let encrypted = self.cipher.encrypt(&entry.key_reference, &entry.value)?;
        let expires_at: chrono::DateTime<chrono::Utc> = entry.expires_at.into();
        tokio::time::timeout(self.timeout, async {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            sqlx::query("SELECT llm_pii_vault_insert_exact($1,$2,$3,$4,$5,$6)")
                .bind(&host)
                .bind(&scope_id)
                .bind(&token)
                .bind(encrypted)
                .bind(&entry.key_reference)
                .bind(expires_at)
                .execute(&mut *transaction)
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            Self::access_audit(
                &mut transaction,
                &self.gateway_instance,
                "insert",
                &host,
                &scope_id,
                Some(&token),
                "accepted",
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)
        })
        .await
        .map_err(|_| LlmGatewayError::ProviderUnavailable)?
    }

    async fn resolve_exact(
        &self,
        scope: &PiiVaultScope,
        token_value: &str,
    ) -> Result<Option<Zeroizing<String>>, LlmGatewayError> {
        let host = self.digest(b"host", &scope.host_id)?;
        let scope_id = self.digest(b"scope", &scope.scope_id)?;
        let token = self.digest(b"token", token_value)?;
        tokio::time::timeout(self.timeout, async {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            let row = sqlx::query(
                "SELECT encrypted_value,key_reference FROM llm_pii_vault_resolve_exact($1,$2,$3)",
            )
            .bind(&host)
            .bind(&scope_id)
            .bind(&token)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            let outcome = if row.is_some() { "found" } else { "not_found" };
            Self::access_audit(
                &mut transaction,
                &self.gateway_instance,
                "resolve",
                &host,
                &scope_id,
                Some(&token),
                outcome,
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            row.map(|row| {
                let ciphertext: Vec<u8> = row.get("encrypted_value");
                let key_reference: String = row.get("key_reference");
                self.cipher.decrypt(&key_reference, &ciphertext)
            })
            .transpose()
        })
        .await
        .map_err(|_| LlmGatewayError::ProviderUnavailable)?
    }

    async fn revoke_exact(
        &self,
        scope: &PiiVaultScope,
        token_value: &str,
    ) -> Result<(), LlmGatewayError> {
        let host = self.digest(b"host", &scope.host_id)?;
        let scope_id = self.digest(b"scope", &scope.scope_id)?;
        let token = self.digest(b"token", token_value)?;
        tokio::time::timeout(self.timeout, async {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            let deleted: i64 = sqlx::query_scalar("SELECT llm_pii_vault_revoke_exact($1,$2,$3)")
                .bind(&host)
                .bind(&scope_id)
                .bind(&token)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            Self::access_audit(
                &mut transaction,
                &self.gateway_instance,
                "revoke",
                &host,
                &scope_id,
                Some(&token),
                if deleted == 0 { "not_found" } else { "deleted" },
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)
        })
        .await
        .map_err(|_| LlmGatewayError::ProviderUnavailable)?
    }

    async fn expire_before(&self, deadline: SystemTime) -> Result<u64, LlmGatewayError> {
        let deadline: chrono::DateTime<chrono::Utc> = deadline.into();
        tokio::time::timeout(self.timeout, async {
            let mut transaction = self
                .pool
                .begin()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            let deleted: i64 = sqlx::query_scalar("SELECT llm_pii_vault_expire_before($1)")
                .bind(deadline)
                .fetch_one(&mut *transaction)
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            Self::access_audit(
                &mut transaction,
                &self.gateway_instance,
                "expire",
                "batch",
                "batch",
                None,
                "deleted",
            )
            .await?;
            transaction
                .commit()
                .await
                .map_err(|_| LlmGatewayError::ProviderUnavailable)?;
            u64::try_from(deleted).map_err(|_| LlmGatewayError::ProviderUnavailable)
        })
        .await
        .map_err(|_| LlmGatewayError::ProviderUnavailable)?
    }

    fn operation_timeout(&self) -> Duration {
        self.timeout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use model_provider::inference::{FinishReason, Message, Role, TerminalState, ToolCall};

    fn profile(unresolved: UnresolvedPiiBehavior) -> PiiProfile {
        PiiProfile {
            enabled: true,
            unresolved,
            kinds: BTreeSet::from([PiiKind::Email, PiiKind::UsSsn]),
            ..PiiProfile::default()
        }
    }

    fn token_for_value(session: &RequestPiiSession, value: &str) -> String {
        session
            .by_token
            .iter()
            .find(|(_, mapping)| mapping.value.as_str() == value)
            .map(|(token, _)| token.clone())
            .unwrap()
    }

    fn promotion_evidence() -> Value {
        let lane = serde_json::json!({"state":"pass","digest":"a".repeat(64)});
        serde_json::json!({
            "schemaVersion":"1",
            "physicalModel":"gpt-qualified",
            "detectorVersion":"local-regex-v1",
            "tokenFormatVersion":"v1",
            "scope":"request",
            "vaultImplementationVersion":"none",
            "placeholderPreservationPercent":100,
            "validUntil":"2999-01-01T00:00:00Z",
            "lanes":{
                "functional":lane.clone(),
                "security":lane.clone(),
                "durability":lane.clone(),
                "performance":lane
            }
        })
    }

    #[test]
    fn pii_promotion_requires_every_lane_and_the_exact_identity_tuple() {
        let profile = profile(UnresolvedPiiBehavior::LeaveMasked);
        assert!(
            validate_pii_promotion(
                &profile,
                "gpt-qualified",
                Some(&promotion_evidence()),
                Utc::now(),
            )
            .is_ok()
        );

        let mut wrong_model = promotion_evidence();
        wrong_model["physicalModel"] = Value::String("other-model".to_string());
        assert!(
            validate_pii_promotion(&profile, "gpt-qualified", Some(&wrong_model), Utc::now(),)
                .is_err()
        );

        let mut missing_performance = promotion_evidence();
        missing_performance["lanes"]
            .as_object_mut()
            .unwrap()
            .remove("performance");
        assert!(
            validate_pii_promotion(
                &profile,
                "gpt-qualified",
                Some(&missing_performance),
                Utc::now(),
            )
            .is_err()
        );
    }

    #[test]
    fn typed_request_and_response_recover_only_exact_authenticated_tokens() {
        let mut session =
            RequestPiiSession::new(profile(UnresolvedPiiBehavior::LeaveMasked)).unwrap();
        let mut request = InferenceRequest::text("alias", "email a@example.com ssn 123-45-6789");
        request.messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolCall {
                call: ToolCall {
                    id: "call-1".to_string(),
                    name: "lookup".to_string(),
                    arguments: serde_json::json!({"contact":"a@example.com"}),
                },
            }],
        });
        session.tokenize_request(&mut request).unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(!encoded.contains("a@example.com"));
        assert!(!encoded.contains("123-45-6789"));
        assert_eq!(session.stats().unique, 2);
        let email_token = token_for_value(&session, "a@example.com");
        let altered = email_token.replacen("email", "phone", 1);
        let mut response = InferenceResponse {
            content: vec![ContentBlock::Text {
                text: format!("known {email_token}; altered {altered}"),
            }],
            finish_reason: FinishReason::Stop,
            usage: None,
            evidence: Default::default(),
            terminal_state: TerminalState::Complete,
        };
        session.recover_response(&mut response).unwrap();
        let ContentBlock::Text { text } = &response.content[0] else {
            panic!()
        };
        assert!(text.contains("known a@example.com"));
        assert!(text.contains(&altered));
        assert!(!text.contains("known [[PII:"));
    }

    #[test]
    fn fragmented_stream_token_recovers_at_every_split_position() {
        let expected = "hello a@example.com world";
        let token_length = {
            let mut session =
                RequestPiiSession::new(profile(UnresolvedPiiBehavior::LeaveMasked)).unwrap();
            let mut request = InferenceRequest::text("alias", "a@example.com");
            session.tokenize_request(&mut request).unwrap();
            token_for_value(&session, "a@example.com").len()
        };

        for split in 0..=("hello ".len() + token_length + " world".len()) {
            let mut session =
                RequestPiiSession::new(profile(UnresolvedPiiBehavior::LeaveMasked)).unwrap();
            let mut request = InferenceRequest::text("alias", "a@example.com");
            session.tokenize_request(&mut request).unwrap();
            let token = token_for_value(&session, "a@example.com");
            let wire = format!("hello {token} world");
            let mut stream = session.stream_recoverer();
            let mut recovered = String::new();
            for fragment in [&wire[..split], &wire[split..]] {
                if let Some(InferenceEvent::TextDelta { text }) = stream
                    .recover(InferenceEvent::TextDelta {
                        text: fragment.to_string(),
                    })
                    .unwrap()
                {
                    recovered.push_str(&text);
                }
            }
            for event in stream.finish().unwrap() {
                if let InferenceEvent::TextDelta { text } = event {
                    recovered.push_str(&text);
                }
            }
            assert_eq!(recovered, expected, "failed at split {split}");
        }
    }

    #[test]
    fn repeated_values_share_one_authenticated_token_and_mapping() {
        let mut session =
            RequestPiiSession::new(profile(UnresolvedPiiBehavior::LeaveMasked)).unwrap();
        let mut request = InferenceRequest::text("alias", "a@example.com then a@example.com again");
        session.tokenize_request(&mut request).unwrap();
        let encoded = serde_json::to_string(&request).unwrap();
        let token = token_for_value(&session, "a@example.com");
        assert_eq!(encoded.matches(&token).count(), 2);
        assert_eq!(session.by_token.len(), 1);
        assert_eq!(session.stats().detected, 2);
        assert_eq!(session.stats().unique, 1);
    }

    #[test]
    fn reject_buffered_fails_on_altered_placeholder() {
        let mut session =
            RequestPiiSession::new(profile(UnresolvedPiiBehavior::RejectBuffered)).unwrap();
        let mut request = InferenceRequest::text("alias", "a@example.com");
        session.tokenize_request(&mut request).unwrap();
        let altered = token_for_value(&session, "a@example.com").replacen("email", "phone", 1);
        let mut response = InferenceResponse {
            content: vec![ContentBlock::Text { text: altered }],
            finish_reason: FinishReason::Stop,
            usage: None,
            evidence: Default::default(),
            terminal_state: TerminalState::Complete,
        };
        assert_eq!(
            session.recover_response(&mut response),
            Err(LlmGatewayError::ProviderUnavailable)
        );
    }

    #[test]
    fn durable_scopes_fail_closed_without_a_promoted_vault() {
        let mut durable = profile(UnresolvedPiiBehavior::LeaveMasked);
        durable.scope = PiiScope::Session;
        assert!(RequestPiiSession::new(durable).is_err());
    }

    #[test]
    fn message_end_is_not_reordered_by_stream_recovery() {
        let session = RequestPiiSession::new(profile(UnresolvedPiiBehavior::LeaveMasked)).unwrap();
        let mut stream = session.stream_recoverer();
        let event = InferenceEvent::MessageEnd {
            finish_reason: FinishReason::Stop,
            terminal_state: TerminalState::Complete,
        };
        assert_eq!(stream.recover(event.clone()).unwrap(), Some(event));
    }
}
