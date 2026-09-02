use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use a2a_backend::{
    BackendAuthorizedInvocation, BackendCapabilities, BackendClient, BackendEndpoint,
    BackendOperation, BusinessRequest, BusinessResponse, BusinessState, InvocationBudget,
};
use a2a_client::{A2aClient, ValidatedEndpoint};
use a2a_core::{
    AuthorizedInvocation, Direction, InvocationAuthority, RuntimeIdentity,
    canonical_projection_digest, verify_authorized_invocation,
};
use a2a_protocol::{
    A2aOperation, EXTENSIONS_HEADER, ProtocolError, ProtocolProfile, ProtocolVersion,
    TrustedCardSigningProfile, VERSION_HEADER, agent_card_etag, rewrite_agent_card_url,
    verify_signed_agent_card,
};
use a2a_store::{ExpectedBinding, Repository, TaskAccess, TaskAdmission, TaskScope};
use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, Response};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use light_runtime::{ModuleKind, RuntimeConfig};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use std::time::Duration;
use uuid::Uuid;

pub const A2A_CONFIG_FILE: &str = "a2a.yml";
pub const A2A_MODULE_ID: &str = "light-a2a/a2a";
pub const A2A_CONFIG_NAME: &str = "a2a";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aConfig {
    pub runtime_policy: RuntimePolicy,
    pub operational_store: OperationalStore,
    pub managed_artifact_store: ManagedArtifactStore,
    pub authorization_context_key_file: PathBuf,
    pub maximum_database_connections: u32,
    pub maximum_request_bytes: usize,
    pub maximum_response_bytes: usize,
    pub request_timeout_ms: u64,
    #[serde(default)]
    pub allow_unsigned_agent_cards: bool,
    pub bindings: Vec<A2aBinding>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedArtifactStore {
    pub binding_id: Uuid,
    pub binding_digest: String,
    pub minimum_schema_generation: i64,
    pub database_url_file: PathBuf,
    pub root_directory: PathBuf,
    pub scan_profile_id: String,
    pub allowed_media_types: BTreeSet<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicy {
    pub publication_id: Uuid,
    pub release_version: u64,
    pub policy_snapshot_id: Uuid,
    pub policy_version: u64,
    pub policy_digest: String,
    pub audience: String,
    pub host: String,
    pub service_id: String,
    pub env_tag: String,
    pub content_digest: String,
    pub source_event_sequence: i64,
    pub schema_version: u64,
    pub created_at: String,
    pub valid_from: String,
    pub refresh_after: String,
    pub expires_at: String,
    pub revocation_epoch: u64,
    pub compatibility_generation: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OperationalStore {
    pub contract_version: u16,
    pub binding_id: Uuid,
    pub binding_digest: String,
    pub host_id: Uuid,
    pub environment: String,
    pub server_host: String,
    pub port: u16,
    pub tls_mode: String,
    pub service_owner: String,
    pub schema: String,
    pub expected_database: String,
    pub minimum_schema_generation: i64,
    pub database_url_file: PathBuf,
    pub credential_generation: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aBinding {
    pub agent_ref: String,
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: String,
    pub directions: Vec<Direction>,
    pub backend_kind: String,
    pub backend_binding_id: Uuid,
    #[serde(default)]
    pub backend_transport: Option<BackendTransportProfile>,
    pub protocol_profile: ProtocolProfile,
    pub allowed_operations: BTreeSet<A2aOperation>,
    #[serde(default)]
    pub allowed_skill_ids: BTreeSet<String>,
    #[serde(default)]
    pub allowed_principal_prefixes: Vec<String>,
    pub public_url: String,
    pub agent_card: Value,
    pub artifact_retention: A2aArtifactRetentionPolicy,
    #[serde(default)]
    pub trusted_signing_profile: Option<TrustedCardSigningProfile>,
    #[serde(default)]
    pub remote_url: Option<String>,
    #[serde(default)]
    pub outbound_policy: Option<OutboundPolicy>,
    #[serde(default)]
    pub phase6_profile: Option<Phase6Profile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Phase6Profile {
    pub profile_id: String,
    #[serde(default)]
    pub extended_card: Option<ExtendedCardProfile>,
    #[serde(default)]
    pub data_extensions: Vec<DataExtensionProfile>,
    #[serde(default)]
    pub push_notifications: Option<PushNotificationProfile>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtendedCardProfile {
    pub authorization_policy_digest: String,
    pub allowed_principal_prefixes: Vec<String>,
    pub card: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DataExtensionProfile {
    pub extension_uri: String,
    pub schema_digest: String,
    pub schema_document: Value,
    pub handler_identity: String,
    #[serde(default)]
    pub dependency_ids: Vec<String>,
    pub allowed_operations: BTreeSet<A2aOperation>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushNotificationProfile {
    pub profile_id: String,
    pub maximum_attempts: i64,
    pub initial_backoff_seconds: i64,
    pub maximum_backoff_seconds: i64,
    pub lease_seconds: i64,
    pub request_timeout_ms: u64,
    pub registrations: Vec<PushCallbackRegistration>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PushCallbackRegistration {
    pub registration_id: Uuid,
    pub url: String,
    pub owner_principal_prefixes: Vec<String>,
    pub hmac_key_file: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboundPolicy {
    pub environment: String,
    pub approved_card_digest: String,
    pub review_state: String,
    pub signature_verified: bool,
    #[serde(default)]
    pub revoked: bool,
    pub review_expires_at: String,
    pub maximum_delegation_depth: u16,
    pub maximum_budget_units: u64,
    pub allowed_calling_agent_refs: BTreeSet<String>,
    pub allowed_principal_prefixes: Vec<String>,
    pub allowed_data_boundary_digests: BTreeSet<String>,
    pub artifact_handling: String,
    #[serde(default)]
    pub credential_file: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendTransportProfile {
    pub contract_version: String,
    pub contract_digest: String,
    pub origin: String,
    pub audience: String,
    pub context_key_file: PathBuf,
    pub data_boundary_digest: String,
    pub request_timeout_ms: u64,
    pub maximum_request_bytes: u64,
    pub maximum_response_bytes: usize,
    pub capabilities: BackendCapabilities,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct A2aArtifactRetentionPolicy {
    pub profile_id: String,
    pub task_retention_days: u32,
    pub artifact_retention_days: u32,
    pub maximum_artifact_bytes: u64,
    pub access_policy_ref: String,
}

impl A2aConfig {
    pub fn load(runtime: &RuntimeConfig) -> Result<Self, String> {
        let config = runtime
            .module_registry
            .load_config::<Self>(runtime, A2A_CONFIG_FILE)
            .map_err(|error| format!("load effective A2A configuration: {error}"))?;
        let env_tag = runtime
            .service_identity
            .env_tag
            .as_deref()
            .ok_or_else(|| "startup envTag is required".to_string())?;
        config.validate(
            &runtime.bootstrap.host,
            &runtime.service_identity.service_id,
            env_tag,
        )?;
        runtime
            .module_registry
            .register_loaded_config(
                A2A_MODULE_ID,
                A2A_CONFIG_NAME,
                ModuleKind::Application,
                &config,
                [],
                true,
                Some(true),
                true,
            )
            .map_err(|error| format!("register A2A runtime projection: {error}"))?;
        Ok(config)
    }

    pub fn validate(&self, host: &str, service_id: &str, env_tag: &str) -> Result<(), String> {
        if self.runtime_policy.audience != "light-a2a"
            || self.operational_store.contract_version != 2
            || self.operational_store.credential_generation < 1
            || self.operational_store.environment != self.runtime_policy.env_tag
            || self.operational_store.service_owner != "light-a2a"
            || self.operational_store.schema != "a2a_ops"
            || !operational_store::runtime::postgres_identifier(
                &self.operational_store.expected_database,
            )
            || self.operational_store.binding_digest.len() < 8
            || self.maximum_database_connections == 0
            || self.maximum_request_bytes == 0
            || self.maximum_response_bytes == 0
            || self.request_timeout_ms == 0
            || self.bindings.is_empty()
            || self.runtime_policy.schema_version != 1
            || self.managed_artifact_store.binding_digest.len() != 71
            || self.managed_artifact_store.minimum_schema_generation < 1
            || !self.managed_artifact_store.root_directory.is_absolute()
            || self
                .managed_artifact_store
                .scan_profile_id
                .trim()
                .is_empty()
            || self.managed_artifact_store.allowed_media_types.is_empty()
        {
            return Err("invalid immutable light-a2a runtime/store projection".into());
        }
        RuntimeIdentity {
            host: self.runtime_policy.host.clone(),
            service_id: self.runtime_policy.service_id.clone(),
            env_tag: self.runtime_policy.env_tag.clone(),
        }
        .validate_against(host, service_id, env_tag)
        .map_err(|_| {
            "runtimePolicy host, serviceId, and envTag do not match startup".to_string()
        })?;
        if !self.runtime_policy.content_digest.starts_with("sha256:")
            || self.runtime_policy.content_digest.len() != 71
            || !self.runtime_policy.policy_digest.starts_with("sha256:")
            || self.runtime_policy.policy_digest.len() != 71
            || self.runtime_policy.release_version == 0
            || self.runtime_policy.policy_version == 0
            || self.runtime_policy.source_event_sequence < 0
        {
            return Err("runtimePolicy immutable publication metadata is invalid".into());
        }
        let canonical_bindings = serde_json::json!({"bindings": self.bindings});
        let computed_content_digest = canonical_projection_digest(&canonical_bindings)
            .map_err(|_| "runtimePolicy content cannot be canonicalized".to_string())?;
        if computed_content_digest != self.runtime_policy.content_digest {
            return Err("runtimePolicy contentDigest does not match canonical bindings".into());
        }
        let created_at = parse_time("runtimePolicy.createdAt", &self.runtime_policy.created_at)?;
        let valid_from = parse_time("runtimePolicy.validFrom", &self.runtime_policy.valid_from)?;
        let refresh_after = parse_time(
            "runtimePolicy.refreshAfter",
            &self.runtime_policy.refresh_after,
        )?;
        let expires_at = parse_time("runtimePolicy.expiresAt", &self.runtime_policy.expires_at)?;
        let now = Utc::now();
        if created_at > valid_from
            || valid_from > now
            || expires_at <= now
            || !(valid_from < refresh_after && refresh_after < expires_at)
        {
            return Err("runtimePolicy validity window is invalid or expired".into());
        }
        let mut aliases = BTreeMap::new();
        for binding in &self.bindings {
            if binding.agent_ref.trim().is_empty()
                || !binding.policy_digest.starts_with("sha256:")
                || binding.directions.is_empty()
                || binding.allowed_operations.is_empty()
                || binding.allowed_principal_prefixes.is_empty()
                || binding
                    .allowed_principal_prefixes
                    .iter()
                    .any(|value| value.trim().is_empty())
                || binding.artifact_retention.profile_id.trim().is_empty()
                || binding.artifact_retention.task_retention_days == 0
                || binding.artifact_retention.task_retention_days > 3650
                || binding.artifact_retention.artifact_retention_days == 0
                || binding.artifact_retention.artifact_retention_days > 3650
                || binding.artifact_retention.maximum_artifact_bytes == 0
                || binding.artifact_retention.maximum_artifact_bytes > 1_099_511_627_776
                || binding
                    .artifact_retention
                    .access_policy_ref
                    .trim()
                    .is_empty()
                || !matches!(
                    binding.backend_kind.as_str(),
                    "EXTERNAL_SIDECAR" | "REMOTE_A2A"
                )
                || aliases
                    .insert(binding.agent_ref.clone(), binding.binding_id)
                    .is_some()
            {
                return Err("invalid or duplicate A2A binding".into());
            }
            binding
                .protocol_profile
                .validate()
                .map_err(|error| format!("invalid A2A protocol profile: {error}"))?;
            validate_phase6_profile(binding, self.allow_unsigned_agent_cards)?;
            rewrite_agent_card_url(&binding.agent_card, &binding.public_url)
                .map_err(|error| format!("invalid Agent Card publication: {error}"))?;
            match (
                binding.agent_card.get("signatures"),
                binding.trusted_signing_profile.as_ref(),
            ) {
                (Some(_), Some(profile)) => verify_signed_agent_card(&binding.agent_card, profile)
                    .map_err(|error| format!("invalid Agent Card signature: {error}"))?,
                (None, None) if self.allow_unsigned_agent_cards && cfg!(debug_assertions) => {}
                _ => {
                    return Err(
                        "Agent Card and trusted signing profile must be projected together".into(),
                    );
                }
            }
            match binding.backend_kind.as_str() {
                "REMOTE_A2A" => {
                    ValidatedEndpoint::parse(
                        binding
                            .remote_url
                            .as_deref()
                            .ok_or_else(|| "REMOTE_A2A binding requires remoteUrl".to_string())?,
                    )
                    .map_err(|error| error.to_string())?;
                    let policy = binding.outbound_policy.as_ref().ok_or_else(|| {
                        "REMOTE_A2A binding requires an approved outboundPolicy".to_string()
                    })?;
                    if !binding.directions.contains(&Direction::Outbound)
                        || policy.environment != self.runtime_policy.env_tag
                        || !policy.approved_card_digest.starts_with("sha256:")
                        || policy.approved_card_digest.len() != 71
                        || canonical_projection_digest(&binding.agent_card)
                            .map_err(|error| error.to_string())?
                            != policy.approved_card_digest
                        || policy.review_state != "APPROVED"
                        || !policy.signature_verified
                        || policy.revoked
                        || parse_time("outboundPolicy.reviewExpiresAt", &policy.review_expires_at)?
                            <= Utc::now()
                        || policy.maximum_delegation_depth == 0
                        || policy.maximum_budget_units == 0
                        || policy.allowed_calling_agent_refs.is_empty()
                        || policy.allowed_principal_prefixes.is_empty()
                        || policy
                            .allowed_principal_prefixes
                            .iter()
                            .any(|value| value.trim().is_empty())
                        || policy.allowed_data_boundary_digests.is_empty()
                        || !policy
                            .allowed_data_boundary_digests
                            .iter()
                            .all(|value| value.starts_with("sha256:") && value.len() == 71)
                        || !matches!(policy.artifact_handling.as_str(), "MANAGED" | "EPHEMERAL")
                    {
                        return Err("REMOTE_A2A outbound trust policy is not executable".into());
                    }
                }
                "EXTERNAL_SIDECAR" if binding.remote_url.is_some() => {
                    return Err("EXTERNAL_SIDECAR binding must not carry remoteUrl".into());
                }
                "EXTERNAL_SIDECAR" => {
                    let transport = binding.backend_transport.as_ref().ok_or_else(|| {
                        "EXTERNAL_SIDECAR binding requires backendTransport".to_string()
                    })?;
                    if transport.contract_version != a2a_backend::CONTRACT_VERSION
                        || transport.contract_digest != a2a_backend::contract_digest_value()
                        || transport.audience.trim().is_empty()
                        || !transport.data_boundary_digest.starts_with("sha256:")
                        || transport.data_boundary_digest.len() != 71
                        || transport.request_timeout_ms == 0
                        || transport.maximum_request_bytes == 0
                        || transport.maximum_response_bytes == 0
                        || transport.capabilities.contract_version != a2a_backend::CONTRACT_VERSION
                        || transport.capabilities.accepted_content_modes.is_empty()
                        || transport.capabilities.maximum_artifact_bytes == 0
                        || transport.capabilities.maximum_artifact_bytes
                            > binding.artifact_retention.maximum_artifact_bytes
                    {
                        return Err("invalid immutable backend transport profile".into());
                    }
                    BackendEndpoint::parse(&transport.origin).map_err(|error| error.to_string())?;
                }
                _ => {}
            }
            if binding.backend_kind != "EXTERNAL_SIDECAR" && binding.backend_transport.is_some() {
                return Err("only EXTERNAL_SIDECAR may carry backendTransport".into());
            }
            if binding.backend_kind != "REMOTE_A2A" && binding.outbound_policy.is_some() {
                return Err("only REMOTE_A2A may carry outboundPolicy".into());
            }
        }
        Ok(())
    }
}

fn validate_phase6_profile(
    binding: &A2aBinding,
    allow_unsigned_agent_cards: bool,
) -> Result<(), String> {
    let extension_sets_nonempty = !binding.protocol_profile.advertised_extensions.is_empty()
        || !binding
            .protocol_profile
            .allowed_inbound_extensions
            .is_empty()
        || !binding.protocol_profile.required_extensions.is_empty();
    let Some(profile) = binding.phase6_profile.as_ref() else {
        if extension_sets_nonempty {
            return Err("extensions require an explicit Phase 6 profile".into());
        }
        return Ok(());
    };
    if binding.backend_kind != "EXTERNAL_SIDECAR"
        || binding.protocol_profile.version != ProtocolVersion::V10
        || profile.profile_id.trim().is_empty()
        || (profile.extended_card.is_none()
            && profile.data_extensions.is_empty()
            && profile.push_notifications.is_none())
    {
        return Err("Phase 6 capabilities require one non-empty A2A 1.0 profile".into());
    }
    if !binding.protocol_profile.required_extensions.is_empty() {
        return Err("required extensions need a later independently qualified profile".into());
    }
    let extension_uris = profile
        .data_extensions
        .iter()
        .map(|value| value.extension_uri.clone())
        .collect::<BTreeSet<_>>();
    if extension_uris.len() != profile.data_extensions.len()
        || extension_uris != binding.protocol_profile.advertised_extensions
        || extension_uris != binding.protocol_profile.allowed_inbound_extensions
    {
        return Err(
            "Phase 6 data extension handlers must exactly match the protocol profile".into(),
        );
    }
    for extension in &profile.data_extensions {
        if !extension.extension_uri.starts_with("https://")
            || !extension.schema_digest.starts_with("sha256:")
            || extension.schema_digest.len() != 71
            || extension.handler_identity != "light-a2a-data-json-schema-v1"
            || !extension.dependency_ids.is_empty()
            || canonical_projection_digest(&extension.schema_document)
                .map_or(true, |digest| digest != extension.schema_digest)
            || jsonschema::draft202012::new(&extension.schema_document).is_err()
            || extension.allowed_operations.is_empty()
            || !extension
                .allowed_operations
                .is_subset(&binding.allowed_operations)
            || extension.allowed_operations.iter().any(|operation| {
                matches!(
                    operation,
                    A2aOperation::GetAgentCard
                        | A2aOperation::GetExtendedAgentCard
                        | A2aOperation::CreateTaskPushNotificationConfig
                        | A2aOperation::GetTaskPushNotificationConfig
                        | A2aOperation::ListTaskPushNotificationConfigs
                        | A2aOperation::DeleteTaskPushNotificationConfig
                )
            })
        {
            return Err("Phase 6 extension is not an optional data-only handler".into());
        }
    }
    if let Some(extended) = profile.extended_card.as_ref() {
        if !binding
            .allowed_operations
            .contains(&A2aOperation::GetExtendedAgentCard)
            || !extended.authorization_policy_digest.starts_with("sha256:")
            || extended.authorization_policy_digest.len() != 71
            || extended.allowed_principal_prefixes.is_empty()
            || extended
                .allowed_principal_prefixes
                .iter()
                .any(|value| value.trim().is_empty())
        {
            return Err("extended Agent Card profile is not independently authorized".into());
        }
        rewrite_agent_card_url(&extended.card, &binding.public_url)
            .map_err(|error| format!("invalid extended Agent Card: {error}"))?;
        match (
            extended.card.get("signatures"),
            binding.trusted_signing_profile.as_ref(),
        ) {
            (Some(_), Some(signing)) => verify_signed_agent_card(&extended.card, signing)
                .map_err(|error| format!("invalid extended Agent Card signature: {error}"))?,
            (None, None) if allow_unsigned_agent_cards && cfg!(debug_assertions) => {}
            _ => return Err("extended Agent Card requires the published signing profile".into()),
        }
    }
    if let Some(push) = profile.push_notifications.as_ref() {
        let required = [
            A2aOperation::CreateTaskPushNotificationConfig,
            A2aOperation::GetTaskPushNotificationConfig,
            A2aOperation::ListTaskPushNotificationConfigs,
            A2aOperation::DeleteTaskPushNotificationConfig,
        ];
        if push.profile_id.trim().is_empty()
            || !(1..=100).contains(&push.maximum_attempts)
            || !(1..=86400).contains(&push.initial_backoff_seconds)
            || push.maximum_backoff_seconds < push.initial_backoff_seconds
            || push.maximum_backoff_seconds > 86400
            || !(1..=300).contains(&push.lease_seconds)
            || push.request_timeout_ms == 0
            || push.request_timeout_ms.saturating_add(5_000)
                > (push.lease_seconds as u64).saturating_mul(1_000)
            || push.registrations.is_empty()
            || required
                .iter()
                .any(|operation| !binding.allowed_operations.contains(operation))
        {
            return Err("push notification profile is incomplete".into());
        }
        let mut registration_ids = BTreeSet::new();
        let mut urls = BTreeSet::new();
        for registration in &push.registrations {
            ValidatedEndpoint::parse(&registration.url).map_err(|error| error.to_string())?;
            if !registration_ids.insert(registration.registration_id)
                || !urls.insert(registration.url.clone())
                || registration.owner_principal_prefixes.is_empty()
                || registration
                    .owner_principal_prefixes
                    .iter()
                    .any(|value| value.trim().is_empty())
                || !registration.hmac_key_file.is_absolute()
            {
                return Err("push callback registration is invalid or duplicated".into());
            }
        }
    }
    Ok(())
}

fn parse_time(name: &str, value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| format!("{name} is invalid: {error}"))
}

#[derive(Clone)]
pub struct A2aState {
    repository: Repository,
    artifact_repository: artifact_store::Repository,
    artifact_root: PathBuf,
    artifact_scan_profile_id: String,
    artifact_media_types: BTreeSet<String>,
    projection: Arc<ArcSwap<A2aRuntimeProjection>>,
    authorization_key: Arc<Vec<u8>>,
    maximum_request_bytes: usize,
    federation_client: A2aClient,
    operational_binding_id: Uuid,
    operational_binding_digest: String,
    authorization_context_key_file: PathBuf,
    host_id: Uuid,
    environment: String,
    push_worker_started: Arc<AtomicBool>,
    push_last_success_epoch: Arc<AtomicI64>,
}

struct A2aRuntimeProjection {
    bindings: BTreeMap<String, A2aBinding>,
    backend_clients: BTreeMap<Uuid, BackendRuntime>,
    remote_credentials: BTreeMap<Uuid, Arc<String>>,
    push_profiles: BTreeMap<Uuid, PushRuntime>,
    maximum_response_bytes: usize,
    expires_at: DateTime<Utc>,
    revocation_epoch: u64,
}

#[derive(Clone)]
struct PushRuntime {
    client: A2aClient,
    maximum_attempts: i64,
    initial_backoff_seconds: i64,
    maximum_backoff_seconds: i64,
    lease_seconds: i64,
    registrations: BTreeMap<Uuid, PushRegistrationRuntime>,
}

#[derive(Clone)]
struct PushRegistrationRuntime {
    endpoint: ValidatedEndpoint,
    hmac_key: Arc<Vec<u8>>,
}

#[derive(Clone)]
struct BackendRuntime {
    client: BackendClient,
    audience: String,
    data_boundary_digest: String,
    maximum_request_bytes: u64,
    expected_capabilities: BackendCapabilities,
}

impl A2aState {
    pub async fn build(config: A2aConfig) -> Result<Self, String> {
        let database_url = a2a_store::read_database_url(
            &config.operational_store.database_url_file,
            &config.operational_store.server_host,
            config.operational_store.port,
            &config.operational_store.tls_mode,
            &config.operational_store.expected_database,
        )
        .map_err(|error| error.to_string())?;
        let pool = PgPoolOptions::new()
            .max_connections(config.maximum_database_connections)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO a2a_ops, operational_meta")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&database_url)
            .await
            .map_err(|error| format!("connect A2A operational store: {error}"))?;
        a2a_store::validate(
            &pool,
            &ExpectedBinding {
                binding_id: config.operational_store.binding_id,
                binding_digest: &config.operational_store.binding_digest,
                host_id: config.operational_store.host_id,
                environment: &config.operational_store.environment,
                server_host: &config.operational_store.server_host,
                port: config.operational_store.port,
                tls_mode: &config.operational_store.tls_mode,
                expected_database: &config.operational_store.expected_database,
                minimum_schema_generation: config.operational_store.minimum_schema_generation,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        let artifact_database_url =
            artifact_store::read_database_url(&config.managed_artifact_store.database_url_file)
                .map_err(|error| error.to_string())?;
        let artifact_pool = PgPoolOptions::new()
            .max_connections(config.maximum_database_connections)
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO artifact_ops, operational_meta")
                        .execute(connection)
                        .await?;
                    Ok(())
                })
            })
            .connect(&artifact_database_url)
            .await
            .map_err(|error| format!("connect managed artifact store: {error}"))?;
        artifact_store::validate(
            &artifact_pool,
            &artifact_store::ExpectedBinding {
                binding_id: config.managed_artifact_store.binding_id,
                binding_digest: &config.managed_artifact_store.binding_digest,
                host_id: config.operational_store.host_id,
                environment: &config.operational_store.environment,
                minimum_schema_generation: config.managed_artifact_store.minimum_schema_generation,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&config.managed_artifact_store.root_directory)
            .map_err(|error| format!("create managed artifact root: {error}"))?;
        let authorization_key = std::fs::read(&config.authorization_context_key_file)
            .map_err(|error| format!("read A2A authorized-context key: {error}"))?;
        if authorization_key.len() < 32 {
            return Err("A2A authorized-context key must contain at least 32 bytes".into());
        }
        let federation_client = A2aClient::new(Duration::from_millis(config.request_timeout_ms))
            .map_err(|error| format!("build A2A federation client: {error}"))?;
        let projection = runtime_projection(&config)?;
        verify_backend_capabilities(&projection).await?;
        Ok(Self {
            repository: Repository::new(pool),
            artifact_repository: artifact_store::Repository::new(artifact_pool),
            artifact_root: config.managed_artifact_store.root_directory,
            artifact_scan_profile_id: config.managed_artifact_store.scan_profile_id,
            artifact_media_types: config.managed_artifact_store.allowed_media_types,
            projection: Arc::new(ArcSwap::from_pointee(projection)),
            authorization_key: Arc::new(authorization_key),
            maximum_request_bytes: config.maximum_request_bytes,
            federation_client,
            operational_binding_id: config.operational_store.binding_id,
            operational_binding_digest: config.operational_store.binding_digest,
            authorization_context_key_file: config.authorization_context_key_file,
            host_id: config.operational_store.host_id,
            environment: config.runtime_policy.env_tag,
            push_worker_started: Arc::new(AtomicBool::new(false)),
            push_last_success_epoch: Arc::new(AtomicI64::new(0)),
        })
    }

    pub fn pool(&self) -> sqlx::PgPool {
        self.repository.pool().clone()
    }

    pub fn artifact_pool(&self) -> sqlx::PgPool {
        self.artifact_repository.pool().clone()
    }

    pub fn spawn_push_worker(self: &Arc<Self>) {
        self.push_worker_started.store(true, Ordering::Release);
        let state = Arc::downgrade(self);
        let worker_id = format!("light-a2a:{}", Uuid::now_v7());
        tokio::spawn(async move {
            loop {
                let Some(state) = state.upgrade() else { break };
                match state.deliver_push_batch(&worker_id).await {
                    Ok(()) => state
                        .push_last_success_epoch
                        .store(Utc::now().timestamp(), Ordering::Release),
                    Err(error) => tracing::warn!(%error, "A2A push delivery batch failed"),
                }
                drop(state);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    pub fn spawn_artifact_retention_worker(self: &Arc<Self>) {
        let state = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let now = Utc::now();
                match state
                    .repository
                    .expired_artifacts(state.host_id, now, 25)
                    .await
                {
                    Ok(candidates) => {
                        for candidate in candidates {
                            let access = TaskAccess {
                                host_id: state.host_id,
                                task_id: candidate.task_id,
                                principal_subject: &candidate.principal_subject,
                                caller_agent_ref: &candidate.caller_agent_ref,
                                target_agent_ref: &candidate.target_agent_ref,
                                binding_id: candidate.binding_id,
                            };
                            if let Err(error) = state
                                .delete_expired_artifact(&access, candidate.artifact_id, now)
                                .await
                            {
                                tracing::warn!(
                                    artifact_id = %candidate.artifact_id,
                                    task_id = %candidate.task_id,
                                    %error,
                                    "A2A artifact retention deletion failed"
                                );
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "A2A artifact retention scan failed"),
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    async fn deliver_push_batch(&self, worker_id: &str) -> Result<(), String> {
        let projection = self.projection.load_full();
        let lease_seconds = projection
            .push_profiles
            .values()
            .map(|profile| profile.lease_seconds)
            // Claiming happens before the delivery's binding profile is loaded.
            // Use the longest qualified lease so no profile can be reclaimed
            // while its own permitted callback is still in flight.
            .max()
            .unwrap_or(30);
        let deliveries = self
            .repository
            .claim_push_deliveries(self.host_id, worker_id, 1, lease_seconds)
            .await
            .map_err(|error| error.to_string())?;
        for delivery in deliveries {
            let Some(profile) = projection.push_profiles.get(&delivery.binding_id) else {
                self.repository
                    .retry_push_delivery(
                        self.host_id,
                        delivery.delivery_id,
                        worker_id,
                        "PUSH_PROFILE_UNAVAILABLE",
                        60,
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                continue;
            };
            let Some(registration) = profile
                .registrations
                .get(&delivery.callback_registration_id)
            else {
                self.repository
                    .retry_push_delivery(
                        self.host_id,
                        delivery.delivery_id,
                        worker_id,
                        "CALLBACK_REGISTRATION_REVOKED",
                        profile.maximum_backoff_seconds,
                        None,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                continue;
            };
            let body = serde_json::to_vec(&delivery.payload).map_err(|error| error.to_string())?;
            let timestamp = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let signed = format!(
                "{}\n{}\n{}\n{}",
                delivery.delivery_id, delivery.delivery_nonce, timestamp, delivery.payload_digest
            );
            let mut mac = Hmac::<Sha256>::new_from_slice(&registration.hmac_key)
                .map_err(|_| "push HMAC key is invalid".to_string())?;
            mac.update(signed.as_bytes());
            let signature = BASE64_STANDARD.encode(mac.finalize().into_bytes());
            let outcome = profile
                .client
                .post_signed_callback(
                    &registration.endpoint,
                    body,
                    &delivery.delivery_id.to_string(),
                    &delivery.delivery_nonce.to_string(),
                    &timestamp,
                    &format!("hmac-sha256={signature}"),
                )
                .await;
            match outcome {
                Ok(response) if response.status().is_success() => {
                    self.repository
                        .complete_push_delivery(
                            self.host_id,
                            delivery.delivery_id,
                            worker_id,
                            response.status().as_u16(),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Ok(response) => {
                    let delay = push_retry_delay(profile, delivery.attempt);
                    self.repository
                        .retry_push_delivery(
                            self.host_id,
                            delivery.delivery_id,
                            worker_id,
                            "CALLBACK_HTTP_STATUS",
                            delay,
                            Some(response.status().as_u16()),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
                Err(_) => {
                    let delay = push_retry_delay(profile, delivery.attempt);
                    self.repository
                        .retry_push_delivery(
                            self.host_id,
                            delivery.delivery_id,
                            worker_id,
                            "CALLBACK_TRANSPORT_ERROR",
                            delay,
                            None,
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
        }
        Ok(())
    }

    /// Apply a policy-authorized legal hold to an artifact already proven to
    /// belong to `access`. Callers must not construct `TaskAccess` from an
    /// untrusted artifact or task identifier alone.
    pub async fn place_artifact_hold(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        hold_id: Uuid,
        reason_code: &str,
    ) -> Result<(), String> {
        self.repository
            .owned_artifact(access, artifact_id)
            .await
            .map_err(|error| error.to_string())?;
        self.artifact_repository
            .place_hold(self.host_id, artifact_id, hold_id, reason_code)
            .await
            .map_err(|error| error.to_string())?;
        self.repository
            .set_artifact_hold(access, artifact_id, true)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn release_artifact_hold(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        hold_id: Uuid,
    ) -> Result<(), String> {
        self.repository
            .set_artifact_hold(access, artifact_id, false)
            .await
            .map_err(|error| error.to_string())?;
        self.artifact_repository
            .release_hold(self.host_id, artifact_id, hold_id)
            .await
            .map_err(|error| error.to_string())
    }

    /// Delete expired managed bytes, verify absence, and persist the same
    /// tombstone evidence in both operational authorities. The operation is
    /// restart-safe: an absent file is accepted only after ownership,
    /// retention, and legal-hold checks pass again.
    pub async fn delete_expired_artifact(
        &self,
        access: &TaskAccess<'_>,
        artifact_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<String, String> {
        let artifact = self
            .repository
            .begin_artifact_deletion(access, artifact_id, now)
            .await
            .map_err(|error| error.to_string())?;
        self.artifact_repository
            .begin_deletion(self.host_id, artifact_id, now)
            .await
            .map_err(|error| error.to_string())?;
        let path = managed_object_path(&self.artifact_root, &artifact.object_reference)?;
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("delete managed artifact: {error}")),
        }
        if path.exists() {
            return Err("managed artifact deletion could not be verified".into());
        }
        let evidence = format!(
            "sha256:{:x}",
            Sha256::digest(format!(
                "{}|{}|{}|verified-absent",
                artifact.artifact_id, artifact.content_digest, artifact.object_reference
            ))
        );
        self.artifact_repository
            .tombstone(self.host_id, artifact_id, &evidence, now)
            .await
            .map_err(|error| error.to_string())?;
        self.repository
            .complete_artifact_deletion(access, artifact_id, &evidence)
            .await
            .map_err(|error| error.to_string())?;
        Ok(evidence)
    }

    pub async fn reload_projection(&self, config: A2aConfig) -> Result<(), String> {
        if config.operational_store.binding_id != self.operational_binding_id
            || config.operational_store.binding_digest != self.operational_binding_digest
            || config.authorization_context_key_file != self.authorization_context_key_file
            || config.maximum_request_bytes != self.maximum_request_bytes
            || config.managed_artifact_store.root_directory != self.artifact_root
            || config.managed_artifact_store.scan_profile_id != self.artifact_scan_profile_id
            || config.managed_artifact_store.allowed_media_types != self.artifact_media_types
        {
            return Err(
                "reload cannot change the operational store, key file, or listener body limit"
                    .into(),
            );
        }
        let candidate = runtime_projection(&config)?;
        verify_backend_capabilities(&candidate).await?;
        if candidate.revocation_epoch < self.projection.load().revocation_epoch {
            return Err("reload cannot decrease runtimePolicy.revocationEpoch".into());
        }
        self.projection.store(Arc::new(candidate));
        Ok(())
    }
}

fn push_retry_delay(profile: &PushRuntime, attempt: i64) -> i64 {
    let exponent = u32::try_from(attempt.saturating_sub(1).min(20)).unwrap_or(20);
    profile
        .initial_backoff_seconds
        .saturating_mul(2_i64.saturating_pow(exponent))
        .min(profile.maximum_backoff_seconds)
}

fn managed_object_path(root: &FsPath, object_reference: &str) -> Result<PathBuf, String> {
    let reference = FsPath::new(object_reference);
    if reference.is_absolute()
        || reference.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err("managed artifact reference escaped its tenant root".into());
    }
    Ok(root.join(reference))
}

fn runtime_projection(config: &A2aConfig) -> Result<A2aRuntimeProjection, String> {
    let mut backend_clients = BTreeMap::new();
    let mut backend_profiles = BTreeMap::new();
    let mut remote_credentials = BTreeMap::new();
    let mut push_profiles = BTreeMap::new();
    for binding in &config.bindings {
        if let Some(path) = binding
            .outbound_policy
            .as_ref()
            .and_then(|policy| policy.credential_file.as_ref())
        {
            remote_credentials.insert(binding.binding_id, Arc::new(read_secret_file(path)?));
        }
        if let Some(push) = binding
            .phase6_profile
            .as_ref()
            .and_then(|profile| profile.push_notifications.as_ref())
        {
            let mut registrations = BTreeMap::new();
            for registration in &push.registrations {
                let key = std::fs::read(&registration.hmac_key_file).map_err(|error| {
                    format!(
                        "read push HMAC key for {}: {error}",
                        registration.registration_id
                    )
                })?;
                if key.len() < 32 {
                    return Err("push HMAC key must contain at least 32 bytes".into());
                }
                registrations.insert(
                    registration.registration_id,
                    PushRegistrationRuntime {
                        endpoint: ValidatedEndpoint::parse(&registration.url)
                            .map_err(|error| error.to_string())?,
                        hmac_key: Arc::new(key),
                    },
                );
            }
            push_profiles.insert(
                binding.binding_id,
                PushRuntime {
                    client: A2aClient::new(Duration::from_millis(push.request_timeout_ms))
                        .map_err(|error| error.to_string())?,
                    maximum_attempts: push.maximum_attempts,
                    initial_backoff_seconds: push.initial_backoff_seconds,
                    maximum_backoff_seconds: push.maximum_backoff_seconds,
                    lease_seconds: push.lease_seconds,
                    registrations,
                },
            );
        }
        let Some(transport) = binding.backend_transport.as_ref() else {
            continue;
        };
        if let Some(existing) = backend_profiles.get(&binding.backend_binding_id) {
            if existing != transport {
                return Err(
                    "one backend transport profile ID resolved to different content".into(),
                );
            }
            continue;
        }
        let key = std::fs::read(&transport.context_key_file).map_err(|error| {
            format!(
                "read backend context key for {}: {error}",
                binding.agent_ref
            )
        })?;
        if key.len() < 32 {
            return Err(format!(
                "backend context key for {} must contain at least 32 bytes",
                binding.agent_ref
            ));
        }
        let runtime = BackendRuntime {
            client: BackendClient::new(
                BackendEndpoint::parse(&transport.origin).map_err(|error| error.to_string())?,
                Arc::new(key),
                Duration::from_millis(transport.request_timeout_ms),
                transport.maximum_response_bytes,
            )
            .map_err(|error| error.to_string())?,
            audience: transport.audience.clone(),
            data_boundary_digest: transport.data_boundary_digest.clone(),
            maximum_request_bytes: transport.maximum_request_bytes,
            expected_capabilities: transport.capabilities.clone(),
        };
        backend_clients.insert(binding.backend_binding_id, runtime);
        backend_profiles.insert(binding.backend_binding_id, transport.clone());
    }
    Ok(A2aRuntimeProjection {
        bindings: config
            .bindings
            .iter()
            .cloned()
            .map(|value| (value.agent_ref.clone(), value))
            .collect(),
        backend_clients,
        remote_credentials,
        push_profiles,
        maximum_response_bytes: config.maximum_response_bytes,
        expires_at: parse_time("runtimePolicy.expiresAt", &config.runtime_policy.expires_at)?,
        revocation_epoch: config.runtime_policy.revocation_epoch,
    })
}

fn read_secret_file(path: &FsPath) -> Result<String, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("inspect server-owned A2A credential: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("server-owned A2A credential must be a regular non-symlink file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err("server-owned A2A credential permissions are too broad".into());
        }
    }
    let value = std::fs::read_to_string(path)
        .map_err(|error| format!("read server-owned A2A credential: {error}"))?;
    let value = value.trim_end_matches(['\r', '\n']).to_string();
    if value.is_empty() || value.len() > 8192 || value.contains(['\r', '\n']) {
        return Err("server-owned A2A credential is invalid".into());
    }
    Ok(value)
}

async fn verify_backend_capabilities(projection: &A2aRuntimeProjection) -> Result<(), String> {
    for runtime in projection.backend_clients.values() {
        let actual = runtime
            .client
            .capabilities()
            .await
            .map_err(|error| format!("read external backend capabilities: {error}"))?;
        if actual != runtime.expected_capabilities {
            return Err("external backend capabilities differ from the published profile".into());
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct JsonRpcRequest {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: String,
    id: Value,
    #[serde(rename = "method")]
    _method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SendParams {
    #[serde(default)]
    task_id: Option<Uuid>,
    #[serde(default)]
    context_id: Option<Uuid>,
    message: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskParams {
    id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListTasksParams {
    #[serde(default)]
    context_id: Option<Uuid>,
    #[serde(default = "default_task_list_size")]
    page_size: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreatePushConfigParams {
    #[serde(default)]
    id: Option<Uuid>,
    task_id: Uuid,
    url: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    authentication: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PushConfigParams {
    task_id: Uuid,
    id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ListPushConfigsParams {
    task_id: Uuid,
    #[serde(default)]
    page_token: Option<String>,
    #[serde(default = "default_task_list_size")]
    page_size: i64,
}

const fn default_task_list_size() -> i64 {
    20
}

pub fn router(state: Arc<A2aState>) -> Router {
    let limit = state.maximum_request_bytes;
    Router::new()
        .route("/a2a/{agent_ref}", post(a2a_request))
        .route(
            "/a2a/{agent_ref}/.well-known/agent-card.json",
            get(agent_card),
        )
        .route("/a2a/{agent_ref}/.well-known/agent.json", get(agent_card))
        .route("/internal/a2a/outbound/{agent_ref}", post(outbound_request))
        .route("/_a2a/ready", get(phase7_readiness))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

async fn phase7_readiness(State(state): State<Arc<A2aState>>) -> Response<Body> {
    let projection = state.projection.load();
    let now = Utc::now();
    let maximum_lease = projection
        .push_profiles
        .values()
        .map(|profile| profile.lease_seconds)
        .max()
        .unwrap_or(30);
    if let Some(reason) = phase7_not_ready_reason(
        projection.expires_at,
        !projection.push_profiles.is_empty(),
        state.push_worker_started.load(Ordering::Acquire),
        state.push_last_success_epoch.load(Ordering::Acquire),
        maximum_lease,
        now,
    ) {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"not-ready","reason":reason})),
        )
            .into_response();
    }
    (axum::http::StatusCode::OK, Json(json!({"status":"ready"}))).into_response()
}

fn phase7_not_ready_reason(
    expires_at: DateTime<Utc>,
    push_enabled: bool,
    worker_started: bool,
    last_success_epoch: i64,
    maximum_lease_seconds: i64,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if expires_at <= now {
        return Some("projection-expired");
    }
    if push_enabled
        && (!worker_started
            || last_success_epoch == 0
            || now.timestamp() - last_success_epoch > maximum_lease_seconds + 5)
    {
        return Some("push-worker-stale");
    }
    None
}

async fn a2a_request(
    State(state): State<Arc<A2aState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    handle(state, agent_ref, headers, body, Direction::Inbound).await
}

async fn outbound_request(
    State(state): State<Arc<A2aState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    handle(state, agent_ref, headers, body, Direction::Outbound).await
}

async fn agent_card(
    State(state): State<Arc<A2aState>>,
    Path(agent_ref): Path<String>,
    headers: HeaderMap,
) -> Response<Body> {
    let projection = state.projection.load();
    if projection.expires_at <= Utc::now() {
        return rpc_error(Value::Null, -32003, "A2A publication is expired");
    }
    let Some(binding) = projection.bindings.get(&agent_ref) else {
        return rpc_error(Value::Null, -32004, "Agent binding not found");
    };
    tracing::info!(
        a2a_agent_ref = agent_ref,
        a2a_operation = "GET_AGENT_CARD",
        a2a_version = binding.protocol_profile.version.as_str(),
        revocation_epoch = projection.revocation_epoch,
        "serving governed Agent Card"
    );
    if !binding.directions.contains(&Direction::Inbound) {
        return rpc_error(Value::Null, -32003, "Agent Card disclosure denied");
    }
    let path = format!("/a2a/{agent_ref}/.well-known/agent-card.json");
    if let Err(error) = binding.protocol_profile.classify(
        &Method::GET,
        &path,
        header_value(&headers, VERSION_HEADER),
        header_value(&headers, EXTENSIONS_HEADER),
        &[],
    ) {
        return protocol_error(Value::Null, error);
    }
    match rewrite_agent_card_url(&binding.agent_card, &binding.public_url) {
        Ok(card) => {
            let etag = agent_card_etag(&card, &binding.policy_digest, projection.revocation_epoch);
            if headers
                .get("if-none-match")
                .and_then(|value| value.to_str().ok())
                == Some(etag.as_str())
            {
                return Response::builder()
                    .status(304)
                    .header("etag", etag)
                    .body(Body::empty())
                    .expect("valid conditional Agent Card response");
            }
            let mut response = Json(card).into_response();
            response.headers_mut().insert(
                VERSION_HEADER,
                HeaderValue::from_static(binding.protocol_profile.version.as_str()),
            );
            response.headers_mut().insert(
                "etag",
                HeaderValue::from_str(&etag).expect("SHA-256 ETag is a valid header"),
            );
            response.headers_mut().insert(
                "cache-control",
                HeaderValue::from_static("public, max-age=60, must-revalidate"),
            );
            response
        }
        Err(error) => protocol_error(Value::Null, error),
    }
}

async fn handle(
    state: Arc<A2aState>,
    agent_ref: String,
    headers: HeaderMap,
    body: Bytes,
    direction: Direction,
) -> Response<Body> {
    let projection = state.projection.load();
    if projection.expires_at <= Utc::now() {
        return rpc_error(Value::Null, -32003, "A2A publication is expired");
    }
    let Some(binding) = projection.bindings.get(&agent_ref) else {
        return rpc_error(Value::Null, -32004, "Agent binding not found");
    };
    let version = header_value(&headers, VERSION_HEADER);
    let extensions = header_value(&headers, EXTENSIONS_HEADER);
    let classified =
        match binding
            .protocol_profile
            .classify(&Method::POST, "/", version, extensions, &body)
        {
            Ok(value) => value,
            Err(error) => return protocol_error(Value::Null, error),
        };
    tracing::info!(
        a2a_agent_ref = agent_ref,
        a2a_operation = ?classified.operation,
        a2a_version = classified.version.as_str(),
        a2a_direction = direction.as_str(),
        revocation_epoch = projection.revocation_epoch,
        "processing governed A2A request"
    );
    let request = match serde_json::from_slice::<JsonRpcRequest>(&body) {
        Ok(value) => value,
        Err(_) => return rpc_error(Value::Null, -32600, "Invalid Request"),
    };
    let invocation = match verify_context(&headers, &body, &state.authorization_key) {
        Ok(value) => value,
        Err(message) => return rpc_error(request.id, -32001, message),
    };
    let authority = InvocationAuthority {
        binding_id: binding.binding_id,
        publication_id: binding.publication_id,
        policy_digest: binding.policy_digest.clone(),
        directions: binding.directions.iter().copied().collect(),
        operations: binding.allowed_operations.clone(),
        principal_prefixes: binding.allowed_principal_prefixes.clone(),
    };
    if !invocation_matches_binding(
        state.host_id,
        &agent_ref,
        direction,
        &invocation,
        &authority,
        classified.operation,
    ) {
        return rpc_error(request.id, -32003, "A2A binding denied");
    }
    if let Err(error) = authorize_activated_extensions(
        binding,
        classified.operation,
        &classified.activated_extensions,
        &request.params,
    ) {
        return rpc_error(request.id, -32003, &error);
    }
    if classified.operation == A2aOperation::GetExtendedAgentCard {
        return extended_agent_card_response(
            binding,
            &invocation,
            request.id,
            projection.revocation_epoch,
        );
    }
    if matches!(
        classified.operation,
        A2aOperation::CreateTaskPushNotificationConfig
            | A2aOperation::GetTaskPushNotificationConfig
            | A2aOperation::ListTaskPushNotificationConfigs
            | A2aOperation::DeleteTaskPushNotificationConfig
    ) {
        return handle_push_configuration(
            &state,
            binding,
            &invocation,
            classified.operation,
            request.id,
            request.params,
        )
        .await;
    }
    if binding.backend_kind == "REMOTE_A2A" {
        if let Err(error) = authorize_outbound(
            &state,
            binding,
            &invocation,
            classified.operation,
            body.len(),
        )
        .await
        {
            return rpc_error(request.id, -32003, &error);
        }
        return federate(
            Arc::clone(&state),
            binding.clone(),
            &headers,
            body,
            request,
            &invocation,
            classified.operation,
            projection.maximum_response_bytes,
        )
        .await;
    }

    let response = match classified.operation {
        A2aOperation::SendMessage | A2aOperation::SendStreamingMessage => {
            let params = match serde_json::from_value::<SendParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(request.id, -32602, "Invalid params");
                }
            };
            let selected_skill_id = params
                .message
                .pointer("/metadata/skillId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if selected_skill_id
                .as_ref()
                .is_some_and(|skill| !binding.allowed_skill_ids.contains(skill))
            {
                return rpc_error(
                    request.id,
                    -32003,
                    "A2A skill is not published for this agent",
                );
            }
            let admission = TaskAdmission {
                task_id: params.task_id.unwrap_or_else(Uuid::now_v7),
                context_id: params.context_id.unwrap_or_else(Uuid::now_v7),
                invocation: invocation.clone(),
            };
            match state.repository.admit(&admission).await {
                Ok(snapshot) => {
                    let backend = match projection.backend_clients.get(&binding.backend_binding_id)
                    {
                        Some(value) => value,
                        None => {
                            return rpc_error(
                                request.id,
                                -32050,
                                "published backend transport is unavailable",
                            );
                        }
                    };
                    let business_request = BusinessRequest {
                        task_id: snapshot.task_id,
                        context_id: snapshot.context_id,
                        idempotency_key: invocation.idempotency_key.clone(),
                        skill_id: selected_skill_id.clone(),
                        message: params.message,
                        metadata: json!({}),
                    };
                    let operation = if classified.operation == A2aOperation::SendStreamingMessage {
                        BackendOperation::InvokeStream
                    } else {
                        BackendOperation::Invoke
                    };
                    if operation == BackendOperation::InvokeStream
                        && !backend.expected_capabilities.streaming
                    {
                        return rpc_error(request.id, -32050, "backend does not declare streaming");
                    }
                    let backend_context = match backend_context(
                        &state,
                        binding,
                        backend,
                        &invocation,
                        &business_request,
                        operation,
                        None,
                        selected_skill_id.clone(),
                    ) {
                        Ok(value) => value,
                        Err(error) => return rpc_error(request.id, -32050, &error),
                    };
                    if operation == BackendOperation::InvokeStream {
                        let receiver = match backend
                            .client
                            .start_stream(&backend_context, &business_request)
                            .await
                        {
                            Ok(value) => value,
                            Err(error) => {
                                return rpc_error(request.id, -32050, &error.to_string());
                            }
                        };
                        let operation_id = format!("pending:{}", snapshot.task_id);
                        let task_access = access(&invocation, snapshot.task_id);
                        if let Err(error) = state
                            .repository
                            .bind_backend(
                                &task_access,
                                &binding.backend_kind,
                                binding.backend_binding_id,
                                &operation_id,
                                selected_skill_id.as_deref(),
                            )
                            .await
                        {
                            return rpc_error(request.id, -32050, &error.to_string());
                        }
                        return activated_extensions_response(
                            stream_backend_response(
                                Arc::clone(&state),
                                binding.clone(),
                                invocation,
                                snapshot.task_id,
                                request.id,
                                receiver,
                            ),
                            &classified.activated_extensions,
                        );
                    }
                    let response = match backend
                        .client
                        .call(&backend_context, &business_request)
                        .await
                    {
                        Ok(value) => value,
                        Err(error) => return rpc_error(request.id, -32050, &error.to_string()),
                    };
                    let operation_id = response
                        .backend_operation_id
                        .clone()
                        .unwrap_or_else(|| format!("synchronous:{}", snapshot.task_id));
                    let task_access = access(&invocation, snapshot.task_id);
                    if let Err(error) = state
                        .repository
                        .bind_backend(
                            &task_access,
                            &binding.backend_kind,
                            binding.backend_binding_id,
                            &operation_id,
                            selected_skill_id.as_deref(),
                        )
                        .await
                    {
                        return rpc_error(request.id, -32050, &error.to_string());
                    }
                    let snapshot =
                        match reconcile_business_response(&state, binding, &task_access, response)
                            .await
                        {
                            Ok(value) => value,
                            Err(error) => return rpc_error(request.id, -32050, &error),
                        };
                    let result = serde_json::to_value(snapshot).unwrap_or(Value::Null);
                    if classified.operation == A2aOperation::SendStreamingMessage {
                        sse_result(request.id, result)
                    } else {
                        rpc_result(request.id, result)
                    }
                }
                Err(error) => rpc_error(request.id, -32010, &error.to_string()),
            }
        }
        A2aOperation::GetTask => {
            let params = match serde_json::from_value::<TaskParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(request.id, -32602, "Invalid params");
                }
            };
            let task_access = access(&invocation, params.id);
            match refresh_task(&state, binding, &projection, &invocation, &task_access).await {
                Ok(snapshot) => rpc_result(
                    request.id,
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                ),
                Err(error) => rpc_error(request.id, -32004, &error),
            }
        }
        A2aOperation::CancelTask => {
            let params = match serde_json::from_value::<TaskParams>(request.params) {
                Ok(value) => value,
                Err(_) => {
                    return rpc_error(request.id, -32602, "Invalid params");
                }
            };
            let task_access = access(&invocation, params.id);
            let correlation = match state.repository.backend_task_binding(&task_access).await {
                Ok(value) => value,
                Err(error) => return rpc_error(request.id, -32011, &error.to_string()),
            };
            if correlation.backend_binding_id != binding.backend_binding_id
                || correlation.backend_kind != "EXTERNAL_SIDECAR"
            {
                return rpc_error(request.id, -32003, "backend task binding mismatch");
            }
            let Some(backend) = projection.backend_clients.get(&binding.backend_binding_id) else {
                return rpc_error(
                    request.id,
                    -32050,
                    "published backend transport is unavailable",
                );
            };
            if !backend.expected_capabilities.cancellation {
                return rpc_error(request.id, -32011, "backend does not declare cancellation");
            }
            let business_request = BusinessRequest {
                task_id: params.id,
                context_id: correlation.context_id,
                idempotency_key: correlation.idempotency_key,
                skill_id: correlation.selected_skill_id.clone(),
                message: Value::Null,
                metadata: Value::Null,
            };
            let context = match backend_context(
                &state,
                binding,
                backend,
                &invocation,
                &business_request,
                BackendOperation::Cancel,
                Some(correlation.backend_operation_id),
                correlation.selected_skill_id,
            ) {
                Ok(value) => value,
                Err(error) => return rpc_error(request.id, -32050, &error),
            };
            let response = match backend.client.call(&context, &business_request).await {
                Ok(value) => value,
                Err(error) => return rpc_error(request.id, -32050, &error.to_string()),
            };
            match reconcile_business_response(&state, binding, &task_access, response).await {
                Ok(snapshot) => rpc_result(
                    request.id,
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                ),
                Err(error) => rpc_error(request.id, -32011, &error),
            }
        }
        A2aOperation::ListTasks => {
            let params = match serde_json::from_value::<ListTasksParams>(request.params) {
                Ok(value) => value,
                Err(_) => return rpc_error(request.id, -32602, "Invalid params"),
            };
            let scope = TaskScope {
                host_id: invocation.host_id,
                principal_subject: &invocation.principal_subject,
                caller_agent_ref: &invocation.caller_agent_ref,
                target_agent_ref: &invocation.target_agent_ref,
                binding_id: invocation.binding_id,
                context_id: params.context_id,
                maximum_results: params.page_size,
            };
            match state.repository.list(&scope).await {
                Ok(tasks) => rpc_result(
                    request.id,
                    json!({"tasks": tasks, "nextPageToken": Value::Null}),
                ),
                Err(error) => rpc_error(request.id, -32004, &error.to_string()),
            }
        }
        A2aOperation::SubscribeToTask => {
            let params = match serde_json::from_value::<TaskParams>(request.params) {
                Ok(value) => value,
                Err(_) => return rpc_error(request.id, -32602, "Invalid params"),
            };
            let task_access = access(&invocation, params.id);
            match refresh_task(&state, binding, &projection, &invocation, &task_access).await {
                Ok(snapshot) if snapshot.state.terminal() => sse_result(
                    request.id,
                    serde_json::to_value(snapshot).unwrap_or(Value::Null),
                ),
                Ok(_) => subscribe_task_response(
                    Arc::clone(&state),
                    binding.clone(),
                    invocation,
                    params.id,
                    request.id,
                ),
                Err(error) => rpc_error(request.id, -32004, &error),
            }
        }
        A2aOperation::GetAgentCard
        | A2aOperation::GetExtendedAgentCard
        | A2aOperation::CreateTaskPushNotificationConfig
        | A2aOperation::GetTaskPushNotificationConfig
        | A2aOperation::ListTaskPushNotificationConfigs
        | A2aOperation::DeleteTaskPushNotificationConfig => {
            rpc_error(request.id, -32601, "Method not implemented")
        }
    };
    activated_extensions_response(response, &classified.activated_extensions)
}

fn authorize_activated_extensions(
    binding: &A2aBinding,
    operation: A2aOperation,
    activated: &BTreeSet<String>,
    params: &Value,
) -> Result<(), String> {
    if activated.is_empty() {
        return Ok(());
    }
    let profile = binding
        .phase6_profile
        .as_ref()
        .ok_or_else(|| "activated extensions have no qualified profile".to_string())?;
    for extension in activated {
        let handler = profile
            .data_extensions
            .iter()
            .find(|handler| &handler.extension_uri == extension)
            .ok_or_else(|| "activated extension has no runtime handler".to_string())?;
        if !handler.allowed_operations.contains(&operation) {
            return Err("activated extension is not allowed for this operation".into());
        }
        let data = params
            .get("metadata")
            .and_then(Value::as_object)
            .and_then(|metadata| metadata.get(extension))
            .or_else(|| {
                params
                    .pointer("/message/metadata")
                    .and_then(Value::as_object)
                    .and_then(|metadata| metadata.get(extension))
            })
            .ok_or_else(|| "activated extension data is missing".to_string())?;
        let validator = jsonschema::draft202012::new(&handler.schema_document)
            .map_err(|_| "activated extension schema is invalid".to_string())?;
        if !validator.is_valid(data) {
            return Err("activated extension data does not match its published schema".into());
        }
    }
    Ok(())
}

fn activated_extensions_response(
    mut response: Response<Body>,
    activated: &BTreeSet<String>,
) -> Response<Body> {
    if !activated.is_empty() {
        let value = activated.iter().cloned().collect::<Vec<_>>().join(",");
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert(EXTENSIONS_HEADER, value);
        }
    }
    response
}

fn extended_agent_card_response(
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    id: Value,
    revocation_epoch: u64,
) -> Response<Body> {
    let Some(profile) = binding
        .phase6_profile
        .as_ref()
        .and_then(|profile| profile.extended_card.as_ref())
    else {
        return rpc_error(id, -32004, "Extended Agent Card is not configured");
    };
    if !profile
        .allowed_principal_prefixes
        .iter()
        .any(|prefix| invocation.principal_subject.starts_with(prefix))
    {
        return rpc_error(id, -32003, "Extended Agent Card disclosure denied");
    }
    let card = match rewrite_agent_card_url(&profile.card, &binding.public_url) {
        Ok(card) => card,
        Err(error) => return protocol_error(id, error),
    };
    let etag = agent_card_etag(
        &card,
        &profile.authorization_policy_digest,
        revocation_epoch,
    );
    let mut response = rpc_result(id, card);
    response.headers_mut().insert(
        "etag",
        HeaderValue::from_str(&etag).expect("SHA-256 ETag is a valid header"),
    );
    response.headers_mut().insert(
        "cache-control",
        HeaderValue::from_static("private, no-store"),
    );
    response
}

async fn handle_push_configuration(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    operation: A2aOperation,
    id: Value,
    params: Value,
) -> Response<Body> {
    let Some(profile) = binding
        .phase6_profile
        .as_ref()
        .and_then(|profile| profile.push_notifications.as_ref())
    else {
        return rpc_error(id, -32010, "PushNotificationNotSupportedError");
    };
    match operation {
        A2aOperation::CreateTaskPushNotificationConfig => {
            let request = match serde_json::from_value::<CreatePushConfigParams>(params) {
                Ok(request)
                    if request.token.as_deref().is_none_or(str::is_empty)
                        && request.authentication.is_none() =>
                {
                    request
                }
                _ => return rpc_error(id, -32602, "Invalid params"),
            };
            let Some(registration) = profile.registrations.iter().find(|registration| {
                registration.url == request.url
                    && registration
                        .owner_principal_prefixes
                        .iter()
                        .any(|prefix| invocation.principal_subject.starts_with(prefix))
            }) else {
                return rpc_error(id, -32003, "Callback registration or ownership denied");
            };
            let access = access(invocation, request.task_id);
            let config_id = request.id.unwrap_or_else(Uuid::now_v7);
            let url_digest = format!("sha256:{:x}", Sha256::digest(request.url.as_bytes()));
            match state
                .repository
                .create_push_config(
                    &access,
                    config_id,
                    registration.registration_id,
                    &url_digest,
                )
                .await
            {
                Ok(config) => rpc_result(id, push_config_value(&config, &registration.url)),
                Err(error) => rpc_error(id, -32004, &error.to_string()),
            }
        }
        A2aOperation::GetTaskPushNotificationConfig => {
            let request = match serde_json::from_value::<PushConfigParams>(params) {
                Ok(request) => request,
                Err(_) => return rpc_error(id, -32602, "Invalid params"),
            };
            let access = access(invocation, request.task_id);
            match state.repository.get_push_config(&access, request.id).await {
                Ok(config) => match callback_url(profile, config.callback_registration_id) {
                    Some(url) => rpc_result(id, push_config_value(&config, url)),
                    None => rpc_error(id, -32004, "Callback registration is unavailable"),
                },
                Err(error) => rpc_error(id, -32004, &error.to_string()),
            }
        }
        A2aOperation::ListTaskPushNotificationConfigs => {
            let request = match serde_json::from_value::<ListPushConfigsParams>(params) {
                Ok(request)
                    if request.page_token.as_deref().is_none_or(str::is_empty)
                        && (1..=100).contains(&request.page_size) =>
                {
                    request
                }
                _ => return rpc_error(id, -32602, "Invalid params"),
            };
            let access = access(invocation, request.task_id);
            match state.repository.list_push_configs(&access).await {
                Ok(configs) => {
                    let values = configs
                        .iter()
                        .filter_map(|config| {
                            callback_url(profile, config.callback_registration_id)
                                .map(|url| push_config_value(config, url))
                        })
                        .collect::<Vec<_>>();
                    rpc_result(id, json!({"configs": values, "nextPageToken": Value::Null}))
                }
                Err(error) => rpc_error(id, -32004, &error.to_string()),
            }
        }
        A2aOperation::DeleteTaskPushNotificationConfig => {
            let request = match serde_json::from_value::<PushConfigParams>(params) {
                Ok(request) => request,
                Err(_) => return rpc_error(id, -32602, "Invalid params"),
            };
            let access = access(invocation, request.task_id);
            match state
                .repository
                .delete_push_config(&access, request.id)
                .await
            {
                Ok(()) => rpc_result(id, json!({})),
                Err(error) => rpc_error(id, -32004, &error.to_string()),
            }
        }
        _ => rpc_error(id, -32601, "Method not implemented"),
    }
}

fn callback_url(profile: &PushNotificationProfile, registration_id: Uuid) -> Option<&str> {
    profile
        .registrations
        .iter()
        .find(|registration| registration.registration_id == registration_id)
        .map(|registration| registration.url.as_str())
}

fn push_config_value(config: &a2a_store::PushConfig, url: &str) -> Value {
    json!({
        "id": config.config_id,
        "taskId": config.task_id,
        "url": url,
        "createdAt": config.created_at,
        "authentication": {"schemes": ["HMAC-SHA256"]}
    })
}

async fn authorize_outbound(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    operation: A2aOperation,
    request_bytes: usize,
) -> Result<(), String> {
    let policy = binding
        .outbound_policy
        .as_ref()
        .ok_or_else(|| "outbound trust policy is unavailable".to_string())?;
    let constraints = invocation
        .outbound
        .as_ref()
        .ok_or_else(|| "outbound delegation constraints are required".to_string())?;
    validate_outbound_constraints(
        binding,
        invocation,
        policy,
        &state.environment,
        operation,
        request_bytes,
    )?;
    state
        .repository
        .consume_delegation(
            invocation.host_id,
            constraints.delegation_id,
            &invocation.request_digest,
            invocation.expires_at,
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn validate_outbound_constraints(
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    policy: &OutboundPolicy,
    environment: &str,
    operation: A2aOperation,
    request_bytes: usize,
) -> Result<(), String> {
    let constraints = invocation
        .outbound
        .as_ref()
        .ok_or_else(|| "outbound delegation constraints are required".to_string())?;
    if invocation.direction != Direction::Outbound
        || policy.environment != environment
        || constraints.environment != environment
        || constraints.delegation_depth > policy.maximum_delegation_depth
        || constraints.remaining_budget_units > policy.maximum_budget_units
        || request_bytes as u64 > constraints.remaining_budget_units
        || !policy
            .allowed_calling_agent_refs
            .contains(&invocation.caller_agent_ref)
        || !policy
            .allowed_principal_prefixes
            .iter()
            .any(|prefix| invocation.principal_subject.starts_with(prefix))
        || !policy
            .allowed_data_boundary_digests
            .contains(&constraints.data_boundary_digest)
        || constraints
            .skill_id
            .as_ref()
            .is_some_and(|skill| !binding.allowed_skill_ids.contains(skill))
        || !binding.allowed_operations.contains(&operation)
    {
        return Err("outbound A2A policy denied the invocation".into());
    }
    Ok(())
}

async fn refresh_task(
    state: &A2aState,
    binding: &A2aBinding,
    projection: &A2aRuntimeProjection,
    invocation: &AuthorizedInvocation,
    task_access: &TaskAccess<'_>,
) -> Result<a2a_core::TaskSnapshot, String> {
    let mut snapshot = state
        .repository
        .get(task_access)
        .await
        .map_err(|error| error.to_string())?;
    if snapshot.state.terminal() {
        return Ok(snapshot);
    }
    let correlation = match state.repository.backend_task_binding(task_access).await {
        Ok(value) => value,
        Err(_) => return Ok(snapshot),
    };
    if correlation.backend_binding_id != binding.backend_binding_id {
        return Err("backend task binding mismatch".into());
    }
    let Some(backend) = projection.backend_clients.get(&binding.backend_binding_id) else {
        return Ok(snapshot);
    };
    if !backend.expected_capabilities.status_reconciliation {
        return Ok(snapshot);
    }
    let business_request = BusinessRequest {
        task_id: snapshot.task_id,
        context_id: correlation.context_id,
        idempotency_key: correlation.idempotency_key,
        skill_id: correlation.selected_skill_id.clone(),
        message: Value::Null,
        metadata: Value::Null,
    };
    let context = backend_context(
        state,
        binding,
        backend,
        invocation,
        &business_request,
        BackendOperation::Status,
        Some(correlation.backend_operation_id),
        correlation.selected_skill_id,
    )?;
    // A detached backend is not evidence of a terminal task. Preserve the durable
    // nonterminal snapshot and reconcile on a later lookup/subscription tick.
    if let Ok(response) = backend.client.call(&context, &business_request).await {
        if let Ok(reconciled) =
            reconcile_business_response(state, binding, task_access, response).await
        {
            snapshot = reconciled;
        }
    }
    Ok(snapshot)
}

fn subscribe_task_response(
    state: Arc<A2aState>,
    binding: A2aBinding,
    invocation: AuthorizedInvocation,
    task_id: Uuid,
    id: Value,
) -> Response<Body> {
    let interval = tokio::time::interval(Duration::from_secs(1));
    let stream = futures_util::stream::unfold(
        (state, binding, invocation, id, interval, false),
        move |(state, binding, invocation, id, mut interval, ended)| async move {
            if ended {
                return None;
            }
            interval.tick().await;
            if invocation.expires_at <= Utc::now() {
                let payload = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32001,"message":"Authorized context expired"}});
                return Some((
                    Ok::<_, std::convert::Infallible>(format!(
                        "event: message\ndata: {payload}\n\n"
                    )),
                    (state, binding, invocation, id, interval, true),
                ));
            }
            let projection = state.projection.load();
            let Some(current) = projection.bindings.get(&binding.agent_ref) else {
                let payload = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32003,"message":"A2A binding revoked"}});
                return Some((
                    Ok(format!("event: message\ndata: {payload}\n\n")),
                    (state, binding, invocation, id, interval, true),
                ));
            };
            if current.binding_id != binding.binding_id
                || current.policy_digest != binding.policy_digest
                || !current
                    .allowed_operations
                    .contains(&A2aOperation::SubscribeToTask)
            {
                let payload = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32003,"message":"A2A subscription denied"}});
                return Some((
                    Ok(format!("event: message\ndata: {payload}\n\n")),
                    (state, binding, invocation, id, interval, true),
                ));
            }
            let task_access = access(&invocation, task_id);
            let (payload, terminal) =
                match refresh_task(&state, current, &projection, &invocation, &task_access).await {
                    Ok(snapshot) => {
                        let terminal = snapshot.state.terminal();
                        (json!({"jsonrpc":"2.0","id":id,"result":snapshot}), terminal)
                    }
                    Err(error) => (
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32004,"message":error}}),
                        true,
                    ),
                };
            drop(projection);
            Some((
                Ok(format!("event: message\ndata: {payload}\n\n")),
                (state, binding, invocation, id, interval, terminal),
            ))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

fn backend_context(
    state: &A2aState,
    binding: &A2aBinding,
    backend: &BackendRuntime,
    invocation: &AuthorizedInvocation,
    request: &BusinessRequest,
    operation: BackendOperation,
    backend_operation_id: Option<String>,
    selected_skill_id: Option<String>,
) -> Result<BackendAuthorizedInvocation, String> {
    let body = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    if body.len() as u64 > backend.maximum_request_bytes {
        return Err("business request exceeded backend transport limit".into());
    }
    let now = Utc::now();
    let deadline = std::cmp::min(invocation.expires_at, now + chrono::Duration::seconds(30));
    Ok(BackendAuthorizedInvocation {
        contract_version: a2a_backend::CONTRACT_VERSION.into(),
        invocation_id: Uuid::now_v7(),
        issuer: "light-a2a".into(),
        audience: backend.audience.clone(),
        host_id: state.host_id,
        environment: state.environment.clone(),
        principal_subject: invocation.principal_subject.clone(),
        caller_agent_ref: invocation.caller_agent_ref.clone(),
        target_agent_ref: invocation.target_agent_ref.clone(),
        binding_id: binding.binding_id,
        publication_id: binding.publication_id,
        selected_skill_id,
        operation,
        task_id: request.task_id,
        context_id: request.context_id,
        idempotency_key: request.idempotency_key.clone(),
        backend_operation_id,
        policy_digest: invocation.policy_digest.clone(),
        data_boundary_digest: backend.data_boundary_digest.clone(),
        request_digest: a2a_backend::request_digest(&body),
        budget: InvocationBudget {
            maximum_input_bytes: backend.maximum_request_bytes,
            maximum_output_bytes: binding.artifact_retention.maximum_artifact_bytes,
            maximum_artifact_bytes: binding.artifact_retention.maximum_artifact_bytes,
        },
        traceparent: None,
        issued_at: now,
        deadline,
        expires_at: deadline,
    })
}

async fn reconcile_business_response(
    state: &A2aState,
    binding: &A2aBinding,
    access: &TaskAccess<'_>,
    response: BusinessResponse,
) -> Result<a2a_core::TaskSnapshot, String> {
    for artifact in &response.artifacts {
        persist_artifact(state, binding, access, artifact).await?;
    }
    let task_state = match response.state {
        BusinessState::Submitted => a2a_core::TaskState::Submitted,
        BusinessState::Working => a2a_core::TaskState::Working,
        BusinessState::InputRequired => a2a_core::TaskState::InputRequired,
        BusinessState::AuthRequired => a2a_core::TaskState::AuthRequired,
        BusinessState::Completed => a2a_core::TaskState::Completed,
        BusinessState::Failed => a2a_core::TaskState::Failed,
        BusinessState::Canceled => a2a_core::TaskState::Canceled,
        BusinessState::Rejected => a2a_core::TaskState::Rejected,
    };
    let snapshot = state
        .repository
        .reconcile(
            access,
            task_state,
            response.result,
            response
                .error
                .and_then(|value| serde_json::to_value(value).ok()),
        )
        .await
        .map_err(|error| error.to_string())?;
    if let Some(push) = state
        .projection
        .load()
        .push_profiles
        .get(&binding.binding_id)
    {
        let payload = json!({
            "statusUpdate": {
                "taskId": snapshot.task_id,
                "contextId": snapshot.context_id,
                "status": {"state": snapshot.state}
            }
        });
        state
            .repository
            .enqueue_push_deliveries(access, &payload, push.maximum_attempts)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(snapshot)
}

async fn persist_artifact(
    state: &A2aState,
    binding: &A2aBinding,
    access: &TaskAccess<'_>,
    artifact: &a2a_backend::InlineArtifact,
) -> Result<a2a_core::ArtifactDescriptor, String> {
    if artifact.logical_name.is_empty()
        || artifact.logical_name.len() > 256
        || artifact.logical_name.contains('/')
        || artifact.logical_name.contains("..")
        || !state.artifact_media_types.contains(&artifact.media_type)
    {
        return Err("backend artifact name or media type is not allowed".into());
    }
    let bytes = BASE64_STANDARD
        .decode(&artifact.content_base64)
        .map_err(|_| "backend artifact is not valid base64".to_string())?;
    if bytes.len() as u64 > binding.artifact_retention.maximum_artifact_bytes {
        return Err("backend artifact exceeded the published retention limit".into());
    }
    let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
    if digest != artifact.content_digest {
        return Err("backend artifact digest mismatch".into());
    }
    let relative = format!(
        "{}/{}/{}",
        state.host_id, access.task_id, artifact.artifact_id
    );
    let destination = state.artifact_root.join(&relative);
    let parent = destination
        .parent()
        .ok_or_else(|| "managed artifact path has no parent".to_string())?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| format!("create managed artifact directory: {error}"))?;
    let temporary = destination.with_extension(format!("{}.tmp", Uuid::now_v7()));
    tokio::fs::write(&temporary, &bytes)
        .await
        .map_err(|error| format!("write managed artifact: {error}"))?;
    tokio::fs::rename(&temporary, &destination)
        .await
        .map_err(|error| format!("activate managed artifact: {error}"))?;
    let retain_until = Utc::now()
        + chrono::Duration::days(binding.artifact_retention.artifact_retention_days.into());
    let visibility = match artifact.visibility {
        a2a_backend::ArtifactVisibility::Owner => "OWNER",
        a2a_backend::ArtifactVisibility::AuthorizedCaller => "AUTHORIZED_CALLER",
        a2a_backend::ArtifactVisibility::TenantPolicy => "TENANT_POLICY",
    };
    let owner_id = access.task_id.to_string();
    let registration = artifact_store::ArtifactRegistration {
        artifact_id: artifact.artifact_id,
        owner_service: "light-a2a",
        owner_kind: "TASK",
        owner_id: &owner_id,
        logical_name: &artifact.logical_name,
        media_type: &artifact.media_type,
        size_bytes: bytes.len() as i64,
        content_digest: &digest,
        object_reference: &relative,
        visibility,
        retain_until,
        relationship_kind: "TASK",
        related_service: "light-a2a",
        related_id: &owner_id,
    };
    state
        .artifact_repository
        .register(state.host_id, &registration)
        .await
        .map_err(|error| error.to_string())?;
    let scan_evidence = format!(
        "sha256:{:x}",
        Sha256::digest(format!(
            "{}|{}|{}",
            digest,
            artifact.media_type,
            bytes.len()
        ))
    );
    state
        .artifact_repository
        .record_scan(
            state.host_id,
            artifact.artifact_id,
            "CLEAN",
            &state.artifact_scan_profile_id,
            &scan_evidence,
        )
        .await
        .map_err(|error| error.to_string())?;
    state
        .repository
        .add_artifact(
            access,
            &a2a_store::ArtifactMetadata {
                artifact_id: artifact.artifact_id,
                logical_name: &artifact.logical_name,
                media_type: &artifact.media_type,
                size_bytes: bytes.len() as i64,
                content_digest: &digest,
                object_reference: &relative,
                visibility,
                retain_until,
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok(a2a_core::ArtifactDescriptor {
        artifact_id: artifact.artifact_id,
        logical_name: artifact.logical_name.clone(),
        media_type: artifact.media_type.clone(),
        size_bytes: bytes.len() as u64,
        content_digest: digest.clone(),
        visibility: a2a_core::ArtifactVisibility::TaskOwner,
        retention_deadline: retain_until,
        provenance_digest: format!(
            "sha256:{:x}",
            Sha256::digest(format!(
                "{}|{}|{}",
                access.task_id, artifact.artifact_id, digest
            ))
        ),
    })
}

fn access(invocation: &AuthorizedInvocation, task_id: Uuid) -> TaskAccess<'_> {
    TaskAccess {
        host_id: invocation.host_id,
        task_id,
        principal_subject: &invocation.principal_subject,
        caller_agent_ref: &invocation.caller_agent_ref,
        target_agent_ref: &invocation.target_agent_ref,
        binding_id: invocation.binding_id,
    }
}

fn invocation_matches_binding(
    runtime_host_id: Uuid,
    agent_ref: &str,
    direction: Direction,
    invocation: &AuthorizedInvocation,
    authority: &InvocationAuthority,
    operation: A2aOperation,
) -> bool {
    invocation.host_id == runtime_host_id
        && invocation.target_agent_ref == agent_ref
        && invocation.direction == direction
        && authority.authorize(invocation, operation).is_ok()
}

fn verify_context(
    headers: &HeaderMap,
    body: &[u8],
    key: &[u8],
) -> Result<AuthorizedInvocation, &'static str> {
    let encoded = headers
        .get("x-light-a2a-context")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context")?;
    let signature = headers
        .get("x-light-a2a-signature")
        .and_then(|value| value.to_str().ok())
        .ok_or("Missing authorized context signature")?;
    verify_authorized_invocation(
        encoded,
        signature,
        body,
        key,
        "light-a2a",
        chrono::Utc::now(),
    )
    .map_err(|_| "Authorized context rejected")
}

async fn federate(
    state: Arc<A2aState>,
    binding: A2aBinding,
    headers: &HeaderMap,
    body: Bytes,
    request: JsonRpcRequest,
    invocation: &AuthorizedInvocation,
    operation: A2aOperation,
    maximum_response_bytes: usize,
) -> Response<Body> {
    let Some(remote_url) = binding.remote_url.as_deref() else {
        return rpc_error(Value::Null, -32050, "remote A2A destination is unavailable");
    };
    let endpoint = match ValidatedEndpoint::parse(remote_url) {
        Ok(endpoint) => endpoint,
        Err(error) => return rpc_error(Value::Null, -32050, &error.to_string()),
    };
    let (outbound_body, local_task) = match prepare_remote_request(
        &state, &binding, invocation, operation, request, body,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return rpc_error(Value::Null, -32003, &error),
    };
    let response = match state
        .federation_client
        .post(
            &endpoint,
            headers,
            outbound_body,
            state
                .projection
                .load()
                .remote_credentials
                .get(&binding.binding_id)
                .map(|value| value.as_str()),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            return rpc_error(Value::Null, -32050, &format!("remote A2A failed: {error}"));
        }
    };
    let status = response.status();
    let content_type = response.headers().get("content-type").cloned();
    let is_sse = content_type
        .as_ref()
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if is_sse {
        let upstream = response.bytes_stream();
        let stream = futures_util::stream::unfold(
            (
                upstream,
                Vec::<u8>::new(),
                0usize,
                state,
                binding,
                invocation.clone(),
                local_task,
                false,
            ),
            move |(
                mut upstream,
                mut pending,
                mut total,
                state,
                binding,
                invocation,
                local_task,
                done,
            )| async move {
                if done {
                    return None;
                }
                loop {
                    if let Some(boundary) = pending.windows(2).position(|window| window == b"\n\n")
                    {
                        let event = pending.drain(..boundary + 2).collect::<Vec<_>>();
                        let governed = govern_remote_sse_event(
                            &state,
                            &binding,
                            &invocation,
                            local_task,
                            &event,
                        )
                        .await
                        .map(Bytes::from)
                        .map_err(std::io::Error::other);
                        return Some((
                            governed,
                            (
                                upstream, pending, total, state, binding, invocation, local_task,
                                false,
                            ),
                        ));
                    }
                    match upstream.next().await {
                        Some(Ok(chunk)) => {
                            total = total.saturating_add(chunk.len());
                            if total > maximum_response_bytes {
                                return Some((
                                    Err(std::io::Error::other(
                                        "remote A2A response exceeded limit",
                                    )),
                                    (
                                        upstream, pending, total, state, binding, invocation,
                                        local_task, true,
                                    ),
                                ));
                            }
                            pending.extend_from_slice(&chunk);
                        }
                        Some(Err(error)) => {
                            return Some((
                                Err(std::io::Error::other(error)),
                                (
                                    upstream, pending, total, state, binding, invocation,
                                    local_task, true,
                                ),
                            ));
                        }
                        None if pending.is_empty() => return None,
                        None => {
                            let event = std::mem::take(&mut pending);
                            let governed = govern_remote_sse_event(
                                &state,
                                &binding,
                                &invocation,
                                local_task,
                                &event,
                            )
                            .await
                            .map(Bytes::from)
                            .map_err(std::io::Error::other);
                            return Some((
                                governed,
                                (
                                    upstream, pending, total, state, binding, invocation,
                                    local_task, true,
                                ),
                            ));
                        }
                    }
                }
            },
        );
        let mut result = Response::new(Body::from_stream(stream));
        *result.status_mut() = status;
        if let Some(content_type) = content_type {
            result.headers_mut().insert("content-type", content_type);
        }
        result
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
        return result;
    }
    let response_bytes = match response.bytes().await {
        Ok(value) if value.len() <= maximum_response_bytes => value,
        Ok(_) => return rpc_error(Value::Null, -32050, "remote A2A response exceeded limit"),
        Err(error) => {
            return rpc_error(
                Value::Null,
                -32050,
                &format!("read remote A2A response: {error}"),
            );
        }
    };
    let governed = match govern_remote_response(
        &state,
        &binding,
        invocation,
        local_task,
        content_type.as_ref(),
        &response_bytes,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => return rpc_error(Value::Null, -32050, &error),
    };
    let mut result = Response::new(Body::from(governed));
    *result.status_mut() = status;
    if let Some(content_type) = content_type {
        result.headers_mut().insert("content-type", content_type);
    }
    result
}

async fn prepare_remote_request(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    operation: A2aOperation,
    request: JsonRpcRequest,
    original_body: Bytes,
) -> Result<(Vec<u8>, Option<(Uuid, Uuid)>), String> {
    match operation {
        A2aOperation::SendMessage | A2aOperation::SendStreamingMessage => {
            let params: SendParams = serde_json::from_value(request.params.clone())
                .map_err(|_| "invalid outbound A2A message params".to_string())?;
            let task_id = params.task_id.unwrap_or_else(Uuid::now_v7);
            let context_id = params.context_id.unwrap_or_else(Uuid::now_v7);
            let selected_skill_id = params
                .message
                .pointer("/metadata/skillId")
                .and_then(Value::as_str);
            if selected_skill_id.is_some_and(|skill| !binding.allowed_skill_ids.contains(skill)) {
                return Err("outbound A2A skill is not published for this binding".into());
            }
            state
                .repository
                .admit(&TaskAdmission {
                    task_id,
                    context_id,
                    invocation: invocation.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
            Ok((original_body.to_vec(), Some((task_id, context_id))))
        }
        A2aOperation::GetTask | A2aOperation::CancelTask | A2aOperation::SubscribeToTask => {
            let params: TaskParams = serde_json::from_value(request.params.clone())
                .map_err(|_| "invalid outbound A2A task params".to_string())?;
            let task_access = access(invocation, params.id);
            let correlation = state
                .repository
                .backend_task_binding(&task_access)
                .await
                .map_err(|error| error.to_string())?;
            if correlation.backend_kind != "REMOTE_A2A"
                || correlation.backend_binding_id != binding.backend_binding_id
            {
                return Err("remote task binding mismatch".into());
            }
            let remote_task_id = correlation
                .remote_task_id
                .ok_or_else(|| "remote task identity is unavailable".to_string())?;
            let mut envelope: Value = serde_json::from_slice(&original_body)
                .map_err(|_| "invalid outbound A2A request".to_string())?;
            envelope["params"]["id"] = Value::String(remote_task_id);
            let rewritten = serde_json::to_vec(&envelope).map_err(|error| error.to_string())?;
            Ok((rewritten, Some((params.id, correlation.context_id))))
        }
        _ => Ok((original_body.to_vec(), None)),
    }
}

async fn govern_remote_sse_event(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    local_task: Option<(Uuid, Uuid)>,
    event: &[u8],
) -> Result<Vec<u8>, String> {
    let text =
        std::str::from_utf8(event).map_err(|_| "remote A2A stream is not UTF-8".to_string())?;
    let mut rendered = String::with_capacity(text.len());
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let mut value: Value = serde_json::from_str(data.trim())
                .map_err(|_| "remote A2A stream contained invalid JSON".to_string())?;
            govern_remote_envelope(state, binding, invocation, local_task, &mut value).await?;
            rendered.push_str("data: ");
            rendered.push_str(&value.to_string());
            rendered.push('\n');
        } else {
            rendered.push_str(line);
            rendered.push('\n');
        }
    }
    rendered.push('\n');
    Ok(rendered.into_bytes())
}

async fn govern_remote_response(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    local_task: Option<(Uuid, Uuid)>,
    content_type: Option<&HeaderValue>,
    response: &[u8],
) -> Result<Vec<u8>, String> {
    let is_sse = content_type
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if is_sse {
        let text = std::str::from_utf8(response)
            .map_err(|_| "remote A2A stream is not UTF-8".to_string())?;
        let mut rendered = String::with_capacity(text.len());
        for line in text.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let mut value: Value = serde_json::from_str(data.trim())
                    .map_err(|_| "remote A2A stream contained invalid JSON".to_string())?;
                govern_remote_envelope(state, binding, invocation, local_task, &mut value).await?;
                rendered.push_str("data: ");
                rendered.push_str(&value.to_string());
                rendered.push('\n');
            } else {
                rendered.push_str(line);
                rendered.push('\n');
            }
        }
        return Ok(rendered.into_bytes());
    }
    let mut value: Value = serde_json::from_slice(response)
        .map_err(|_| "remote A2A response is not valid JSON".to_string())?;
    govern_remote_envelope(state, binding, invocation, local_task, &mut value).await?;
    serde_json::to_vec(&value).map_err(|error| error.to_string())
}

async fn govern_remote_envelope(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    local_task: Option<(Uuid, Uuid)>,
    envelope: &mut Value,
) -> Result<(), String> {
    let Some((task_id, context_id)) = local_task else {
        return Ok(());
    };
    if envelope.get("error").is_some() {
        return Ok(());
    }
    let Some(result) = envelope.get_mut("result") else {
        return Ok(());
    };
    let remote_task_id = result
        .get("id")
        .or_else(|| result.pointer("/task/id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let remote_context_id = result
        .get("contextId")
        .or_else(|| result.pointer("/task/contextId"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(remote_task_id) = remote_task_id.as_deref() {
        let task_access = access(invocation, task_id);
        let selected_skill_id = invocation
            .outbound
            .as_ref()
            .and_then(|constraints| constraints.skill_id.as_deref());
        state
            .repository
            .bind_remote_task(
                &task_access,
                binding.backend_binding_id,
                remote_task_id,
                remote_context_id.as_deref(),
                selected_skill_id,
            )
            .await
            .map_err(|error| error.to_string())?;
    }
    rewrite_remote_identity(result, task_id, context_id);
    govern_remote_artifacts(state, binding, invocation, task_id, result).await?;
    if let Some(task_state) = remote_task_state(result) {
        let task_access = access(invocation, task_id);
        state
            .repository
            .reconcile(&task_access, task_state, Some(result.clone()), None)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn rewrite_remote_identity(result: &mut Value, task_id: Uuid, context_id: Uuid) {
    if let Some(object) = result.as_object_mut() {
        if object.contains_key("id") {
            object.insert("id".into(), Value::String(task_id.to_string()));
        }
        if object.contains_key("contextId") {
            object.insert("contextId".into(), Value::String(context_id.to_string()));
        }
        if let Some(task) = object.get_mut("task").and_then(Value::as_object_mut) {
            task.insert("id".into(), Value::String(task_id.to_string()));
            task.insert("contextId".into(), Value::String(context_id.to_string()));
        }
    }
}

fn remote_task_state(result: &Value) -> Option<a2a_core::TaskState> {
    let value = result
        .pointer("/status/state")
        .or_else(|| result.get("state"))
        .or_else(|| result.pointer("/task/status/state"))?
        .as_str()?;
    match value.to_ascii_lowercase().replace('_', "-").as_str() {
        "submitted" => Some(a2a_core::TaskState::Submitted),
        "working" => Some(a2a_core::TaskState::Working),
        "input-required" => Some(a2a_core::TaskState::InputRequired),
        "auth-required" => Some(a2a_core::TaskState::AuthRequired),
        "completed" => Some(a2a_core::TaskState::Completed),
        "failed" => Some(a2a_core::TaskState::Failed),
        "canceled" | "cancelled" => Some(a2a_core::TaskState::Canceled),
        "rejected" => Some(a2a_core::TaskState::Rejected),
        _ => None,
    }
}

async fn govern_remote_artifacts(
    state: &A2aState,
    binding: &A2aBinding,
    invocation: &AuthorizedInvocation,
    task_id: Uuid,
    result: &mut Value,
) -> Result<(), String> {
    let Some(policy) = binding.outbound_policy.as_ref() else {
        return Err("remote A2A artifact policy is unavailable".into());
    };
    if policy.artifact_handling == "EPHEMERAL" {
        return Ok(());
    }
    let Some(artifacts) = result.get_mut("artifacts").and_then(Value::as_array_mut) else {
        return Ok(());
    };
    let task_access = access(invocation, task_id);
    let mut managed = Vec::with_capacity(artifacts.len());
    for artifact in artifacts.iter() {
        let object = artifact
            .as_object()
            .ok_or_else(|| "remote A2A artifact is not an object".to_string())?;
        let logical_name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("remote-artifact");
        let parts = object
            .get("parts")
            .and_then(Value::as_array)
            .ok_or_else(|| "managed remote artifact must contain inline parts".to_string())?;
        for (index, part) in parts.iter().enumerate() {
            if part.get("uri").is_some() || part.pointer("/file/uri").is_some() {
                return Err(
                    "managed remote artifacts must be inline; upstream URIs are never durable"
                        .into(),
                );
            }
            let encoded = part
                .get("bytes")
                .or_else(|| part.get("data"))
                .or_else(|| part.pointer("/file/bytes"))
                .and_then(Value::as_str)
                .ok_or_else(|| "managed remote artifact part has no inline bytes".to_string())?;
            let bytes = BASE64_STANDARD
                .decode(encoded)
                .map_err(|_| "managed remote artifact bytes are invalid base64".to_string())?;
            let media_type = part
                .get("mediaType")
                .or_else(|| part.pointer("/file/mimeType"))
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream");
            let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
            let inline = a2a_backend::InlineArtifact {
                artifact_id: Uuid::now_v7(),
                logical_name: format!("{logical_name}-{index}"),
                media_type: media_type.to_string(),
                content_base64: encoded.to_string(),
                content_digest: digest,
                visibility: a2a_backend::ArtifactVisibility::AuthorizedCaller,
            };
            managed.push(
                serde_json::to_value(
                    persist_artifact(state, binding, &task_access, &inline).await?,
                )
                .map_err(|error| error.to_string())?,
            );
        }
    }
    *artifacts = managed;
    Ok(())
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn rpc_result(id: Value, result: Value) -> Response<Body> {
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}

fn sse_result(id: Value, result: Value) -> Response<Body> {
    let event = format!(
        "event: message\ndata: {}\n\n",
        json!({"jsonrpc":"2.0","id":id,"result":result})
    );
    let mut response = Response::new(Body::from(event));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

fn stream_backend_response(
    state: Arc<A2aState>,
    binding: A2aBinding,
    invocation: AuthorizedInvocation,
    task_id: Uuid,
    id: Value,
    receiver: tokio::sync::mpsc::Receiver<
        Result<a2a_backend::BusinessEvent, a2a_backend::BackendError>,
    >,
) -> Response<Body> {
    let stream = futures_util::stream::unfold(
        (receiver, state, binding, invocation, id, false),
        move |(mut receiver, state, binding, invocation, id, terminated)| async move {
            if terminated {
                return None;
            }
            let event = receiver.recv().await?;
            let (payload, next_terminated) = match event {
                Ok(event) => {
                    let terminal = event.terminal;
                    let task_access = access(&invocation, task_id);
                    let payload = if let Some(operation_id) = event.backend_operation_id.as_deref()
                    {
                        if let Err(error) = state
                            .repository
                            .bind_backend(
                                &task_access,
                                &binding.backend_kind,
                                binding.backend_binding_id,
                                operation_id,
                                None,
                            )
                            .await
                        {
                            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32050,"message":error.to_string()}})
                        } else {
                            stream_event_snapshot(&state, &binding, &task_access, event, &id).await
                        }
                    } else {
                        stream_event_snapshot(&state, &binding, &task_access, event, &id).await
                    };
                    (payload, terminal)
                }
                Err(error) => (
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32050,"message":error.to_string()}}),
                    true,
                ),
            };
            Some((
                Ok::<_, std::convert::Infallible>(format!("event: message\ndata: {payload}\n\n")),
                (receiver, state, binding, invocation, id, next_terminated),
            ))
        },
    );
    let mut response = Response::new(Body::from_stream(stream));
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response
}

async fn stream_event_snapshot(
    state: &A2aState,
    binding: &A2aBinding,
    access: &TaskAccess<'_>,
    event: a2a_backend::BusinessEvent,
    id: &Value,
) -> Value {
    let response = BusinessResponse {
        state: event.state,
        backend_operation_id: event.backend_operation_id,
        result: event.result,
        error: event.error,
        artifacts: event.artifact.into_iter().collect(),
    };
    match reconcile_business_response(state, binding, access, response).await {
        Ok(snapshot) => json!({"jsonrpc":"2.0","id":id,"result":snapshot}),
        Err(error) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32050,"message":error}})
        }
    }
}

fn protocol_error(id: Value, error: ProtocolError) -> Response<Body> {
    Json(error.jsonrpc_response(id)).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Response<Body> {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config(revocation_epoch: u64) -> A2aConfig {
        let now = Utc::now();
        let backend_key_file =
            std::env::temp_dir().join(format!("light-a2a-backend-test-key-{}", std::process::id()));
        std::fs::write(&backend_key_file, vec![b'b'; 32]).unwrap();
        let mut config = A2aConfig {
            runtime_policy: RuntimePolicy {
                publication_id: Uuid::now_v7(),
                release_version: 1,
                policy_snapshot_id: Uuid::now_v7(),
                policy_version: 1,
                policy_digest: format!("sha256:{}", "b".repeat(64)),
                audience: "light-a2a".into(),
                host: "a2a.dev.lightapi.net".into(),
                service_id: "com.networknt.light-a2a-1.0.0".into(),
                env_tag: "dev".into(),
                content_digest: format!("sha256:{}", "c".repeat(64)),
                source_event_sequence: 1,
                schema_version: 1,
                created_at: (now - chrono::Duration::minutes(2)).to_rfc3339(),
                valid_from: (now - chrono::Duration::minutes(1)).to_rfc3339(),
                refresh_after: (now + chrono::Duration::minutes(30)).to_rfc3339(),
                expires_at: (now + chrono::Duration::hours(1)).to_rfc3339(),
                revocation_epoch,
                compatibility_generation: 1,
            },
            operational_store: OperationalStore {
                contract_version: 2,
                binding_id: Uuid::now_v7(),
                binding_digest: format!("sha256:{}", "d".repeat(64)),
                host_id: Uuid::now_v7(),
                environment: "dev".into(),
                server_host: "postgres".into(),
                port: 5432,
                tls_mode: "DISABLE".into(),
                service_owner: "light-a2a".into(),
                schema: "a2a_ops".into(),
                expected_database: "operations".into(),
                minimum_schema_generation: 2,
                database_url_file: "/test/operations-url".into(),
                credential_generation: 1,
            },
            managed_artifact_store: ManagedArtifactStore {
                binding_id: Uuid::now_v7(),
                binding_digest: format!("sha256:{}", "1".repeat(64)),
                minimum_schema_generation: 1,
                database_url_file: "/test/artifact-url".into(),
                root_directory: std::env::temp_dir().join("light-a2a-artifacts-test"),
                scan_profile_id: "light-a2a-static-v1".into(),
                allowed_media_types: ["text/plain".into()].into_iter().collect(),
            },
            authorization_context_key_file: "/test/a2a-key".into(),
            maximum_database_connections: 4,
            maximum_request_bytes: 1024,
            maximum_response_bytes: 4096,
            request_timeout_ms: 1000,
            allow_unsigned_agent_cards: true,
            bindings: vec![A2aBinding {
                agent_ref: "account.agent".into(),
                binding_id: Uuid::now_v7(),
                publication_id: Uuid::now_v7(),
                policy_digest: format!("sha256:{}", "e".repeat(64)),
                directions: vec![Direction::Inbound],
                backend_kind: "EXTERNAL_SIDECAR".into(),
                backend_binding_id: Uuid::now_v7(),
                backend_transport: Some(BackendTransportProfile {
                    contract_version: a2a_backend::CONTRACT_VERSION.into(),
                    contract_digest: a2a_backend::contract_digest_value().into(),
                    origin: "http://127.0.0.1:19010/".into(),
                    audience: "account-agent-backend".into(),
                    context_key_file: backend_key_file,
                    data_boundary_digest: format!("sha256:{}", "f".repeat(64)),
                    request_timeout_ms: 1_000,
                    maximum_request_bytes: 1_024,
                    maximum_response_bytes: 4_096,
                    capabilities: BackendCapabilities {
                        contract_version: a2a_backend::CONTRACT_VERSION.into(),
                        streaming: true,
                        cancellation: true,
                        status_reconciliation: true,
                        accepted_content_modes: ["application/json".into()].into_iter().collect(),
                        maximum_artifact_bytes: 1_048_576,
                    },
                }),
                protocol_profile: ProtocolProfile {
                    version: a2a_protocol::ProtocolVersion::V10,
                    advertised_extensions: BTreeSet::new(),
                    allowed_inbound_extensions: BTreeSet::new(),
                    required_extensions: BTreeSet::new(),
                    maximum_extension_count: 8,
                    maximum_extension_bytes: 2048,
                },
                allowed_operations: [A2aOperation::SendMessage].into_iter().collect(),
                allowed_skill_ids: ["account.lookup".into()].into_iter().collect(),
                allowed_principal_prefixes: vec!["user:".into()],
                public_url: "https://agents.example/a2a/account".into(),
                agent_card: json!({"name":"account","url":"https://agents.example/a2a/account"}),
                artifact_retention: A2aArtifactRetentionPolicy {
                    profile_id: Uuid::now_v7().to_string(),
                    task_retention_days: 30,
                    artifact_retention_days: 30,
                    maximum_artifact_bytes: 1_048_576,
                    access_policy_ref: "account-agent-artifacts".into(),
                },
                trusted_signing_profile: None,
                remote_url: None,
                outbound_policy: None,
                phase6_profile: None,
            }],
        };
        config.runtime_policy.content_digest =
            canonical_projection_digest(&serde_json::json!({"bindings": config.bindings})).unwrap();
        config
    }

    fn refresh_content_digest(config: &mut A2aConfig) {
        config.runtime_policy.content_digest =
            canonical_projection_digest(&serde_json::json!({"bindings": config.bindings})).unwrap();
    }

    #[test]
    fn raw_destination_is_not_representable_in_send_params() {
        let value = json!({"message":{},"url":"https://forbidden.example"});
        assert!(serde_json::from_value::<SendParams>(value).is_err());
    }

    #[test]
    fn portal_artifact_retention_projection_deserializes_with_profile_authority() {
        let policy: A2aArtifactRetentionPolicy = serde_json::from_value(json!({
            "profileId":"01964b05-552a-7c4b-9184-6857e7f3dc5f",
            "taskRetentionDays":30,
            "artifactRetentionDays":60,
            "maximumArtifactBytes":1048576,
            "accessPolicyRef":"account-agent-artifacts"
        }))
        .expect("Portal artifact retention projection");
        assert_eq!(policy.profile_id, "01964b05-552a-7c4b-9184-6857e7f3dc5f");
    }

    #[test]
    fn runtime_rejects_an_empty_binding_principal_boundary() {
        let mut config = valid_config(1);
        config.bindings[0].allowed_principal_prefixes.clear();
        refresh_content_digest(&mut config);
        assert!(
            config
                .validate(
                    "a2a.dev.lightapi.net",
                    "com.networknt.light-a2a-1.0.0",
                    "dev"
                )
                .unwrap_err()
                .contains("invalid or duplicate A2A binding")
        );
    }

    #[test]
    fn invocation_host_must_match_the_runtime_host() {
        let runtime_host_id = Uuid::now_v7();
        let binding_id = Uuid::now_v7();
        let publication_id = Uuid::now_v7();
        let now = Utc::now();
        let invocation = AuthorizedInvocation {
            host_id: Uuid::now_v7(),
            audience: "light-a2a".into(),
            principal_subject: "user:1".into(),
            caller_agent_ref: "caller".into(),
            target_agent_ref: "account.agent".into(),
            binding_id,
            policy_digest: format!("sha256:{}", "a".repeat(64)),
            publication_id,
            direction: Direction::Inbound,
            idempotency_key: "request-1".into(),
            request_digest: format!("sha256:{}", "b".repeat(64)),
            outbound: None,
            issued_at: now,
            expires_at: now + chrono::Duration::minutes(1),
        };
        let authority = InvocationAuthority {
            binding_id,
            publication_id,
            policy_digest: invocation.policy_digest.clone(),
            directions: [Direction::Inbound].into_iter().collect(),
            operations: [A2aOperation::SendMessage].into_iter().collect(),
            principal_prefixes: vec!["user:".into()],
        };
        assert!(!invocation_matches_binding(
            runtime_host_id,
            "account.agent",
            Direction::Inbound,
            &invocation,
            &authority,
            A2aOperation::SendMessage
        ));
    }

    #[test]
    fn runtime_rejects_a_well_formed_but_stale_content_digest() {
        let mut config = valid_config(1);
        config.bindings[0].public_url = "https://tampered.example/a2a".into();
        assert!(
            config
                .validate(
                    "a2a.dev.lightapi.net",
                    "com.networknt.light-a2a-1.0.0",
                    "dev"
                )
                .unwrap_err()
                .contains("contentDigest")
        );
    }

    #[test]
    fn activated_data_extension_validates_its_published_schema() {
        let mut config = valid_config(1);
        let extension = "https://extensions.lightapi.net/a2a/evidence/v1".to_string();
        let schema = json!({"type":"object","required":["evidenceId"],"properties":{"evidenceId":{"type":"string"}},"additionalProperties":false});
        config.bindings[0].phase6_profile = Some(Phase6Profile {
            profile_id: "optional-data-v1".into(),
            extended_card: None,
            data_extensions: vec![DataExtensionProfile {
                extension_uri: extension.clone(),
                schema_digest: canonical_projection_digest(&schema).unwrap(),
                schema_document: schema,
                handler_identity: "light-a2a-data-json-schema-v1".into(),
                dependency_ids: Vec::new(),
                allowed_operations: [A2aOperation::SendMessage].into_iter().collect(),
            }],
            push_notifications: None,
        });
        let activated = [extension.clone()].into_iter().collect();
        assert!(
            authorize_activated_extensions(
                &config.bindings[0],
                A2aOperation::SendMessage,
                &activated,
                &json!({"metadata":{extension.clone():{"evidenceId":"e-1"}}}),
            )
            .is_ok()
        );
        assert!(
            authorize_activated_extensions(
                &config.bindings[0],
                A2aOperation::SendMessage,
                &activated,
                &json!({"metadata":{extension:{"evidenceId":7}}}),
            )
            .is_err()
        );
    }

    #[test]
    fn managed_artifact_references_cannot_escape_the_tenant_root() {
        let root = FsPath::new("/srv/light-a2a/artifacts");
        assert_eq!(
            managed_object_path(root, "host/task/artifact").unwrap(),
            root.join("host/task/artifact")
        );
        for reference in ["/etc/passwd", "../secret", "host/../../secret"] {
            assert!(managed_object_path(root, reference).is_err(), "{reference}");
        }
    }

    #[test]
    fn jsonrpc_application_errors_use_http_200() {
        assert_eq!(
            rpc_error(Value::Null, -32003, "denied").status(),
            axum::http::StatusCode::OK
        );
        assert_eq!(
            protocol_error(Value::Null, ProtocolError::VersionNotSupported).status(),
            axum::http::StatusCode::OK
        );
    }

    #[test]
    fn streaming_result_uses_sse() {
        let response = sse_result(json!(1), json!({"state":"working"}));
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[test]
    fn phase6_data_extension_requires_an_exact_v1_handler_profile() {
        let mut config = valid_config(5);
        let extension = "https://extensions.lightapi.net/a2a/redacted-evidence/v1".to_string();
        config.bindings[0]
            .protocol_profile
            .advertised_extensions
            .insert(extension.clone());
        config.bindings[0]
            .protocol_profile
            .allowed_inbound_extensions
            .insert(extension.clone());
        let schema = json!({"type":"object","required":["evidenceId"],"properties":{"evidenceId":{"type":"string"}},"additionalProperties":false});
        let schema_digest = canonical_projection_digest(&schema).unwrap();
        config.bindings[0].phase6_profile = Some(Phase6Profile {
            profile_id: "optional-data-v1".into(),
            extended_card: None,
            data_extensions: vec![DataExtensionProfile {
                extension_uri: extension,
                schema_digest,
                schema_document: schema,
                handler_identity: "light-a2a-data-json-schema-v1".into(),
                dependency_ids: Vec::new(),
                allowed_operations: [A2aOperation::SendMessage].into_iter().collect(),
            }],
            push_notifications: None,
        });
        refresh_content_digest(&mut config);
        config
            .validate(
                "a2a.dev.lightapi.net",
                "com.networknt.light-a2a-1.0.0",
                "dev",
            )
            .unwrap();
        config.bindings[0]
            .phase6_profile
            .as_mut()
            .unwrap()
            .data_extensions
            .clear();
        assert!(
            config
                .validate(
                    "a2a.dev.lightapi.net",
                    "com.networknt.light-a2a-1.0.0",
                    "dev"
                )
                .is_err()
        );
    }

    #[tokio::test]
    async fn invalid_reload_retains_last_known_good_projection() {
        let mut current = valid_config(5);
        // Projection reload semantics do not need a live business backend. Use the
        // remote profile so capability conformance remains isolated in adapter tests.
        current.bindings[0].backend_kind = "REMOTE_A2A".into();
        current.bindings[0].backend_transport = None;
        current.bindings[0].remote_url = Some("https://remote.example/a2a".into());
        current.bindings[0].directions = vec![Direction::Outbound];
        current.bindings[0].outbound_policy = Some(OutboundPolicy {
            environment: "dev".into(),
            approved_card_digest: canonical_projection_digest(&current.bindings[0].agent_card)
                .unwrap(),
            review_state: "APPROVED".into(),
            signature_verified: true,
            revoked: false,
            review_expires_at: (Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
            maximum_delegation_depth: 4,
            maximum_budget_units: 4096,
            allowed_calling_agent_refs: ["caller.agent".into()].into_iter().collect(),
            allowed_principal_prefixes: vec!["user:".into()],
            allowed_data_boundary_digests: [format!("sha256:{}", "f".repeat(64))]
                .into_iter()
                .collect(),
            artifact_handling: "EPHEMERAL".into(),
            credential_file: None,
        });
        refresh_content_digest(&mut current);
        current
            .validate(
                "a2a.dev.lightapi.net",
                "com.networknt.light-a2a-1.0.0",
                "dev",
            )
            .unwrap();
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://test:test@localhost/operations")
            .unwrap();
        let state = A2aState {
            repository: Repository::new(pool),
            artifact_repository: artifact_store::Repository::new(
                PgPoolOptions::new()
                    .connect_lazy("postgres://test:test@localhost/operations")
                    .unwrap(),
            ),
            artifact_root: current.managed_artifact_store.root_directory.clone(),
            artifact_scan_profile_id: current.managed_artifact_store.scan_profile_id.clone(),
            artifact_media_types: current.managed_artifact_store.allowed_media_types.clone(),
            projection: Arc::new(ArcSwap::from_pointee(runtime_projection(&current).unwrap())),
            authorization_key: Arc::new(vec![b'k'; 32]),
            maximum_request_bytes: current.maximum_request_bytes,
            federation_client: A2aClient::new(Duration::from_secs(1)).unwrap(),
            operational_binding_id: current.operational_store.binding_id,
            operational_binding_digest: current.operational_store.binding_digest.clone(),
            authorization_context_key_file: current.authorization_context_key_file.clone(),
            host_id: current.operational_store.host_id,
            environment: current.runtime_policy.env_tag.clone(),
            push_worker_started: Arc::new(AtomicBool::new(false)),
            push_last_success_epoch: Arc::new(AtomicI64::new(0)),
        };
        let mut stale = current.clone();
        stale.runtime_policy.revocation_epoch = 4;
        assert!(state.reload_projection(stale).await.is_err());
        assert_eq!(state.projection.load().revocation_epoch, 5);

        let mut changed_store = current.clone();
        changed_store.operational_store.binding_digest = format!("sha256:{}", "f".repeat(64));
        assert!(state.reload_projection(changed_store).await.is_err());
        assert_eq!(state.projection.load().revocation_epoch, 5);

        let mut newer = current;
        newer.runtime_policy.revocation_epoch = 6;
        state.reload_projection(newer).await.unwrap();
        assert_eq!(state.projection.load().revocation_epoch, 6);
    }

    #[test]
    fn runtime_policy_uses_only_the_workload_identity_triple() {
        let value = json!({
            "publicationId": Uuid::now_v7(),
            "releaseVersion": 1,
            "policySnapshotId": Uuid::now_v7(),
            "policyVersion": 1,
            "policyDigest": format!("sha256:{}", "b".repeat(64)),
            "audience": "light-a2a",
            "host": "a2a.dev.lightapi.net",
            "serviceId": "com.networknt.light-a2a-1.0.0",
            "envTag": "dev",
            "contentDigest": format!("sha256:{}", "a".repeat(64)),
            "sourceEventSequence": 1,
            "schemaVersion": 1,
            "createdAt": "2026-08-30T00:00:00Z",
            "validFrom": "2026-08-30T00:00:00Z",
            "refreshAfter": "2026-08-30T00:30:00Z",
            "expiresAt": "2026-08-30T01:00:00Z",
            "revocationEpoch": 0,
            "compatibilityGeneration": 1
        });
        let policy = serde_json::from_value::<RuntimePolicy>(value.clone()).unwrap();
        RuntimeIdentity {
            host: policy.host,
            service_id: policy.service_id,
            env_tag: policy.env_tag,
        }
        .validate_against(
            "a2a.dev.lightapi.net",
            "com.networknt.light-a2a-1.0.0",
            "dev",
        )
        .unwrap();

        let mut obsolete = value.as_object().unwrap().clone();
        obsolete.insert("instanceId".into(), json!(Uuid::now_v7()));
        assert!(serde_json::from_value::<RuntimePolicy>(Value::Object(obsolete)).is_err());
    }

    #[test]
    fn phase7_readiness_fails_for_expired_authority_or_stale_push_worker() {
        let now = Utc::now();
        assert_eq!(
            phase7_not_ready_reason(now, false, false, 0, 30, now),
            Some("projection-expired")
        );
        assert_eq!(
            phase7_not_ready_reason(
                now + chrono::Duration::minutes(5),
                true,
                true,
                now.timestamp() - 36,
                30,
                now,
            ),
            Some("push-worker-stale")
        );
        assert_eq!(
            phase7_not_ready_reason(
                now + chrono::Duration::minutes(5),
                true,
                true,
                now.timestamp(),
                30,
                now,
            ),
            None
        );
    }

    #[test]
    fn federation_destinations_reject_ssrf_shapes() {
        for value in [
            "http://remote.example/a2a",
            "https://localhost/a2a",
            "https://127.0.0.1/a2a",
            "https://10.0.0.1/a2a",
            "https://user:password@remote.example/a2a",
            "https://remote.example/a2a#fragment",
        ] {
            assert!(ValidatedEndpoint::parse(value).is_err(), "{value}");
        }
        assert!(ValidatedEndpoint::parse("https://remote.example/a2a").is_ok());
    }

    #[test]
    fn remote_task_state_and_identity_are_normalized_to_local_authority() {
        let task_id = Uuid::now_v7();
        let context_id = Uuid::now_v7();
        let mut result = json!({
            "id": "remote-task",
            "contextId": "remote-context",
            "status": {"state": "input-required"}
        });
        assert_eq!(
            remote_task_state(&result),
            Some(a2a_core::TaskState::InputRequired)
        );
        rewrite_remote_identity(&mut result, task_id, context_id);
        assert_eq!(result["id"], task_id.to_string());
        assert_eq!(result["contextId"], context_id.to_string());
    }
}
