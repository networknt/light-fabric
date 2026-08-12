use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use knowledge_connectors::{
    ConnectorKind, ConnectorPage, ConnectorSyncMode, ValidatedConnectorPage, normalize_permission,
    permission_digest, stable_objects,
};
use knowledge_core::{
    BaseManifest, ChangeKind, CorpusDocumentState, DocumentInput, FullBaseGeneration,
    KnowledgeError, ProcessingContract, SourceLimits, build_full_base,
    build_full_base_with_context, classify_corpus_changes, compact_resolved_generation,
    ingest_markdown_repository, sha256_hex,
};
use light_client::load_ca_cert_bundle;
use light_runtime::{
    BoundTransport, LightRuntimeBuilder, RuntimeConfig, RuntimeError, TransportRuntime,
};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerConfig {
    version: u16,
    #[serde(default)]
    worker_database_url_file: PathBuf,
    #[serde(default)]
    projector_database_url_file: PathBuf,
    heartbeat_secret_file: PathBuf,
    #[serde(default)]
    portal_command_url: Option<String>,
    #[serde(default)]
    portal_authorization_file: Option<PathBuf>,
    #[serde(default)]
    checkout_root: PathBuf,
    #[serde(default)]
    approved_repository_uri: String,
    #[serde(default)]
    immutable_commit: String,
    #[serde(default = "default_checkout_seconds")]
    maximum_checkout_seconds: u64,
    #[serde(default)]
    object_store_root: PathBuf,
    projector_id: String,
    #[serde(default)]
    knowledge_base_id: Uuid,
    #[serde(default)]
    source_id: Uuid,
    #[serde(default)]
    environment: String,
    #[serde(default)]
    embedding_profile_id: Uuid,
    #[serde(default)]
    embedding_profile_revision: i64,
    #[serde(default = "default_true")]
    deterministic_pilot: bool,
    #[serde(default)]
    migration_deterministic_pilot: bool,
    #[serde(default)]
    embedding_gateway_url: Option<String>,
    #[serde(default)]
    embedding_authorization_file: Option<PathBuf>,
    #[serde(default)]
    embedding_gateway_ca_cert_file: Option<PathBuf>,
    #[serde(default = "default_true")]
    embedding_gateway_verify_hostname: bool,
    #[serde(default = "default_embedding_alias")]
    embedding_alias: String,
    #[serde(default = "default_embedding_batch_size")]
    embedding_batch_size: usize,
    #[serde(default = "default_embedding_space_id")]
    embedding_space_id: String,
    #[serde(default = "default_embedding_space_revision")]
    embedding_space_revision: u64,
    #[serde(default = "default_embedding_dimension")]
    embedding_dimension: usize,
    #[serde(default = "default_snapshot_watermark")]
    snapshot_watermark: u64,
    #[serde(default)]
    ingestion_policy_id: Uuid,
    #[serde(default)]
    ingestion_policy_version: i64,
    #[serde(default)]
    maximum_stored_bytes: u64,
    #[serde(default)]
    maximum_spend_micros: u64,
    #[serde(default)]
    maximum_concurrency: u32,
    #[serde(default = "default_maximum_provider_calls")]
    maximum_provider_calls: usize,
    #[serde(default)]
    platform_caps: PlatformCaps,
    #[serde(skip)]
    resolved_sources: Vec<ResolvedSourceConfig>,
    #[serde(default)]
    source_snapshot: serde_json::Value,
    #[serde(default)]
    source_include_prefixes: Vec<String>,
    #[serde(default)]
    source_exclude_prefixes: Vec<String>,
    #[serde(default)]
    current_job_id: Option<Uuid>,
    #[serde(default)]
    sync_run_id: Option<Uuid>,
    #[serde(default)]
    coalesce_queued_syncs: bool,
    #[serde(skip)]
    coalesce_created_before: Option<DateTime<Utc>>,
    #[serde(default)]
    limits: SourceLimits,
    #[serde(default)]
    enterprise_connector_fixture_file: Option<PathBuf>,
    #[serde(default)]
    enterprise_connector_approved_origin: Option<String>,
    #[serde(default)]
    enterprise_connector_page_url: Option<String>,
    #[serde(default)]
    enterprise_connector_authorization_file: Option<PathBuf>,
    #[serde(default)]
    embedding_migration_enabled: bool,
    #[serde(default)]
    production_operations_enabled: bool,
    #[serde(default)]
    graph_assisted_enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlatformCaps {
    maximum_documents: Option<usize>,
    maximum_chunks: Option<usize>,
    maximum_source_bytes: Option<u64>,
    maximum_stored_bytes: Option<u64>,
    maximum_embedding_tokens: Option<usize>,
    maximum_spend_micros: Option<u64>,
    maximum_wall_time_seconds: Option<u64>,
    maximum_concurrency: Option<u32>,
    maximum_provider_calls: Option<usize>,
}

#[derive(Debug, Clone)]
struct ResolvedSourceConfig {
    source_id: Uuid,
    source_type: String,
    approved_repository_uri: String,
    immutable_commit: String,
    source_include_prefixes: Vec<String>,
    source_exclude_prefixes: Vec<String>,
    ingestion_policy_id: Uuid,
    ingestion_policy_version: i64,
    limits: SourceLimits,
    maximum_stored_bytes: u64,
    maximum_spend_micros: u64,
    maximum_wall_time_seconds: u64,
    maximum_concurrency: u32,
    maximum_provider_calls: usize,
}

const DEFAULT_MAXIMUM_PROVIDER_CALLS: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourcePathPolicy {
    include_prefixes: Vec<String>,
    exclude_prefixes: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
struct HeadlessTransport;

#[async_trait]
impl TransportRuntime for HeadlessTransport {
    type Handle = ();

    async fn bind(
        &self,
        _config: &RuntimeConfig,
    ) -> std::result::Result<BoundTransport<Self::Handle>, RuntimeError> {
        Err(RuntimeError::Unsupported(
            "headless Knowledge worker does not bind a listener".into(),
        ))
    }

    async fn stop(&self, _handle: &mut Self::Handle) -> std::result::Result<(), RuntimeError> {
        Ok(())
    }
}

impl WorkerConfig {
    async fn load(command: &str) -> Result<Self> {
        let (config_dir, config_file) = if let Ok(path) =
            env::var("LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE")
        {
            let path = PathBuf::from(path);
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .context("LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE must name a UTF-8 YAML file")?;
            (parent.to_path_buf(), file_name.to_string())
        } else {
            (
                PathBuf::from(
                    env::var("LIGHT_KNOWLEDGE_CONFIG_DIR").unwrap_or_else(|_| "config".to_string()),
                ),
                "worker.yml".to_string(),
            )
        };
        let runtime = LightRuntimeBuilder::new(HeadlessTransport)
            .with_config_dir(&config_dir)
            .build()
            .prepare_config()
            .await
            .context("bootstrap Knowledge worker configuration")?;
        let config = runtime
            .module_registry
            .load_config::<Self>(&runtime, &config_file)
            .with_context(|| {
                format!(
                    "load effective Knowledge worker configuration {}",
                    config_dir.join(config_file).display()
                )
            })?;
        config.validate(command)
    }

    fn validate(self, command: &str) -> Result<Self> {
        let projector_mode = matches!(command, "project-once" | "project-loop" | "heartbeat");
        let enterprise_connector_configured = self.enterprise_connector_fixture_file.is_some()
            || self.enterprise_connector_page_url.is_some();
        if self.version != 1
            || self.projector_id.trim().is_empty()
            || !self.heartbeat_secret_file.is_file()
            || (projector_mode && !self.projector_database_url_file.is_file())
            || (!projector_mode
                && (!self.worker_database_url_file.is_file()
                    || !self.checkout_root.is_dir()
                    || self.object_store_root.as_os_str().is_empty()
                    || self.embedding_batch_size == 0
                    || self.embedding_batch_size > 128
                    || self.embedding_dimension == 0
                    || self.embedding_space_revision == 0
                    || self.embedding_space_id.trim().is_empty()
                    || self.embedding_alias.trim().is_empty()
                    || (self.deterministic_pilot && self.embedding_gateway_url.is_some())
                    || self
                        .embedding_gateway_ca_cert_file
                        .as_ref()
                        .is_some_and(|path| !path.is_file())
                    || (self.migration_deterministic_pilot && !self.deterministic_pilot)))
            || (!projector_mode
                && !self.deterministic_pilot
                && (self
                    .embedding_gateway_url
                    .as_deref()
                    .is_none_or(|url| !url.starts_with("https://"))
                    || self
                        .embedding_authorization_file
                        .as_ref()
                        .is_none_or(|path| !path.is_file())))
            || (enterprise_connector_configured
                != self.enterprise_connector_approved_origin.is_some())
            || (self.enterprise_connector_page_url.is_some()
                != self.enterprise_connector_authorization_file.is_some())
            || self
                .enterprise_connector_page_url
                .as_deref()
                .is_some_and(|url| !url.starts_with("https://"))
            || (self.enterprise_connector_fixture_file.is_some()
                && self.enterprise_connector_page_url.is_some())
            || self
                .enterprise_connector_fixture_file
                .as_ref()
                .is_some_and(|path| !path.is_file())
            || self
                .enterprise_connector_authorization_file
                .as_ref()
                .is_some_and(|path| !path.is_file())
        {
            bail!("invalid Phase 1a worker configuration");
        }
        self.platform_caps.validate()?;
        Ok(self)
    }
}

impl PlatformCaps {
    fn validate(&self) -> Result<()> {
        if self.maximum_documents == Some(0)
            || self.maximum_chunks == Some(0)
            || self.maximum_source_bytes == Some(0)
            || self.maximum_stored_bytes == Some(0)
            || self.maximum_embedding_tokens == Some(0)
            || self.maximum_spend_micros == Some(0)
            || self.maximum_wall_time_seconds == Some(0)
            || self.maximum_concurrency == Some(0)
            || self.maximum_provider_calls == Some(0)
        {
            bail!("Knowledge platform caps must be positive when configured");
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

fn default_checkout_seconds() -> u64 {
    120
}

fn default_maximum_provider_calls() -> usize {
    DEFAULT_MAXIMUM_PROVIDER_CALLS
}

fn default_snapshot_watermark() -> u64 {
    1
}

fn initial_sync_start_watermark() -> i64 {
    0
}

fn default_embedding_alias() -> String {
    "kb-index".into()
}
fn default_embedding_batch_size() -> usize {
    32
}
fn default_embedding_space_id() -> String {
    knowledge_core::FAKE_SPACE_ID.into()
}
fn default_embedding_space_revision() -> u64 {
    knowledge_core::FAKE_SPACE_REVISION
}
fn default_embedding_dimension() -> usize {
    knowledge_core::FAKE_DIMENSION
}

fn embedding_http_client(config: &WorkerConfig) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30));
    if let Some(path) = &config.embedding_gateway_ca_cert_file {
        for certificate in load_ca_cert_bundle(path)
            .with_context(|| format!("load embedding gateway CA bundle {}", path.display()))?
        {
            builder = builder.add_root_certificate(certificate);
        }
    }
    if !config.embedding_gateway_verify_hostname {
        tracing::warn!(
            "embedding gateway hostname verification is disabled by local runtime configuration"
        );
        builder = builder.danger_accept_invalid_hostnames(true);
    }
    builder
        .build()
        .context("build embedding gateway HTTP client")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProjectionEnvelope {
    event_id: Uuid,
    aggregate_type: String,
    aggregate_id: String,
    aggregate_sequence: i64,
    event_type: String,
    payload_digest: String,
    payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectionApplyOutcome {
    AppliedOrDuplicate,
    ParkedGap,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let command = env::args().nth(1).unwrap_or_else(|| "build-loop".into());
    let config = WorkerConfig::load(&command).await?;
    if !matches!(
        command.as_str(),
        "project-once" | "project-loop" | "heartbeat"
    ) {
        fs::create_dir_all(&config.object_store_root)?;
    }
    let database_url_file = if matches!(
        command.as_str(),
        "project-once" | "project-loop" | "heartbeat"
    ) {
        &config.projector_database_url_file
    } else {
        &config.worker_database_url_file
    };
    let database_url = fs::read_to_string(database_url_file)?;
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(database_url.trim())
        .await
        .context("connect to Knowledge database")?;
    match command.as_str() {
        "build-loop" => build_loop(&pool, &config).await,
        "project-once" => {
            let projection = project_once(&pool, &config).await;
            let heartbeat_result = heartbeat(&pool, &config).await;
            projection?;
            heartbeat_result
        }
        "project-loop" => project_loop(&pool, &config).await,
        "heartbeat" => heartbeat(&pool, &config).await,
        other => bail!("unknown worker command {other}"),
    }
}

async fn build_loop(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    tokio::try_join!(
        job_loop(pool, config, WorkerLane::Priority),
        job_loop(pool, config, WorkerLane::Bulk),
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerLane {
    Priority,
    Bulk,
}

const PRIORITY_JOB_TYPES: &[&str] = &["ACL_RECONCILE", "PROVIDER_NOTIFICATION"];
const BULK_JOB_TYPES: &[&str] = &[
    "SYNC",
    "DELTA_SYNC",
    "FULL_REINDEX",
    "PROMOTE",
    "CONNECTIVITY_TEST",
    "UPLOAD",
    "COMPACTION",
    "ANTI_ENTROPY",
    "CONNECTOR_SYNC",
    "PURGE",
    "RETRIEVAL_TEST",
    "MIGRATION_PREFLIGHT",
    "MIGRATION_BACKFILL",
    "MIGRATION_CATCHUP",
    "MIGRATION_VALIDATE",
    "MIGRATION_PAUSE",
    "MIGRATION_CANCEL",
    "MIGRATION_PROMOTE",
    "MIGRATION_ROLLBACK",
    "MIGRATION_RETIRE",
    "BACKUP_CHECKPOINT",
    "RESTORE_VERIFY",
    "SEGMENT_PURGE",
    "GRAPH_BUILD",
];

fn job_fetches_full_base_sources(job_type: &str) -> bool {
    matches!(job_type, "SYNC" | "FULL_REINDEX")
}

fn job_coalesces_queued_syncs(job_type: &str) -> bool {
    job_type == "SYNC"
}

fn projected_job_type(event_type: &str, enterprise_source: bool) -> Option<&'static str> {
    Some(match event_type {
        "KnowledgeSourceSyncRequestedEvent" if enterprise_source => "CONNECTOR_SYNC",
        "KnowledgeSourceSyncRequestedEvent" => "SYNC",
        "KnowledgeSourceAclReconciliationRequestedEvent" => "ACL_RECONCILE",
        "KnowledgeSourceProviderNotificationReceivedEvent" => "PROVIDER_NOTIFICATION",
        "KnowledgeSourceConnectivityTestRequestedEvent" => "CONNECTIVITY_TEST",
        "KnowledgeBaseReindexRequestedEvent" => "FULL_REINDEX",
        "KnowledgeBaseCompactionRequestedEvent" => "COMPACTION",
        "KnowledgeBaseIndexGenerationPromotionRequestedEvent" => "PROMOTE",
        "KnowledgeBaseRetrievalTestRequestedEvent" => "RETRIEVAL_TEST",
        "KnowledgeBasePurgeRequestedEvent" => "PURGE",
        "KnowledgeBaseEmbeddingMigrationRequestedEvent" => "MIGRATION_PREFLIGHT",
        "KnowledgeBaseEmbeddingMigrationPausedEvent" => "MIGRATION_PAUSE",
        "KnowledgeBaseEmbeddingMigrationResumedEvent" => "MIGRATION_BACKFILL",
        "KnowledgeBaseEmbeddingMigrationCancelledEvent" => "MIGRATION_CANCEL",
        "KnowledgeBaseIndexGenerationRollbackRequestedEvent" => "MIGRATION_ROLLBACK",
        "KnowledgeBaseIndexGenerationRetirementRequestedEvent" => "MIGRATION_RETIRE",
        "KnowledgeBaseBackupCheckpointRequestedEvent" => "BACKUP_CHECKPOINT",
        "KnowledgeBasePhysicalRestoreVerificationRequestedEvent" => "RESTORE_VERIFY",
        _ => return None,
    })
}

async fn resolve_job_config(
    pool: &PgPool,
    infrastructure: &WorkerConfig,
    knowledge_base_id: Uuid,
    source_id: Option<Uuid>,
) -> Result<WorkerConfig> {
    let base = sqlx::query(
        "SELECT base.environment,base.version,
                base.desired_embedding_profile_id,
                base.desired_embedding_profile_revision,
                profile.expected_space_id,profile.expected_space_revision,
                profile.dimension,profile.alias_name
           FROM knowledge_base_t base
           JOIN knowledge_embedding_profile_runtime_v profile
             ON profile.profile_id=base.desired_embedding_profile_id
            AND profile.profile_revision=base.desired_embedding_profile_revision
          WHERE base.knowledge_base_id=$1
            AND base.status IN ('DRAFT','ACTIVE','DEPRECATED')",
    )
    .bind(knowledge_base_id)
    .fetch_optional(pool)
    .await?
    .context("KNOWLEDGE_JOB_EMBEDDING_PROFILE_UNAVAILABLE")?;

    let mut resolved = infrastructure.clone();
    resolved.knowledge_base_id = knowledge_base_id;
    resolved.environment = base.get("environment");
    resolved.snapshot_watermark = u64::try_from(base.get::<i64, _>("version"))
        .context("Knowledge Base version is outside the worker range")?;
    resolved.embedding_profile_id = base.get("desired_embedding_profile_id");
    resolved.embedding_profile_revision = base.get("desired_embedding_profile_revision");
    resolved.embedding_space_id = base.get("expected_space_id");
    resolved.embedding_space_revision =
        u64::try_from(base.get::<i64, _>("expected_space_revision"))
            .context("Embedding space revision is outside the worker range")?;
    resolved.embedding_dimension = usize::try_from(base.get::<i32, _>("dimension"))
        .context("Embedding dimension is outside the worker range")?;
    resolved.embedding_alias = base.get("alias_name");

    if let Some(source_id) = source_id {
        let source = sqlx::query(
            "SELECT source.source_id,source.source_type,source.config_json,source.ingestion_policy_id,
                    base.host_id AS base_host_id,policy.host_id AS policy_host_id,
                    policy.active AS policy_active,
                    policy.version AS policy_version,policy.max_documents,
                    policy.max_chunks,policy.max_source_bytes,
                    policy.max_stored_bytes,policy.max_embedding_tokens,
                    policy.max_spend_micros,policy.max_wall_time_seconds,
                    policy.max_concurrency
               FROM knowledge_source_t source
               JOIN knowledge_base_t base
                 ON base.knowledge_base_id=source.knowledge_base_id
               LEFT JOIN knowledge_ingestion_policy_t policy
                 ON policy.ingestion_policy_id=source.ingestion_policy_id
              WHERE source.source_id=$1 AND source.knowledge_base_id=$2
                AND source.status IN ('DRAFT','ACTIVE')",
        )
        .bind(source_id)
        .bind(knowledge_base_id)
        .fetch_optional(pool)
        .await?
        .context("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE")?;
        let source = resolved_job_source_from_row(&source, &resolved.platform_caps)?;
        apply_resolved_source(&mut resolved, &source);
        resolved.resolved_sources = vec![source];
    }
    Ok(resolved)
}

fn cap<T: Ord + Copy>(selected: T, platform: Option<T>) -> T {
    platform.map_or(selected, |platform| selected.min(platform))
}

fn aggregate_wall_time_seconds(
    sources: &[ResolvedSourceConfig],
    platform_cap: Option<u64>,
) -> Result<u64> {
    let selected = sources.iter().try_fold(0_u64, |total, source| {
        total
            .checked_add(source.maximum_wall_time_seconds)
            .context("aggregate maximumWallTimeSeconds overflow")
    })?;
    Ok(cap(selected, platform_cap))
}

fn policy_owner_allowed(
    active: bool,
    policy_host_id: Option<Uuid>,
    knowledge_base_host_id: Option<Uuid>,
) -> bool {
    active
        && policy_host_id
            .map(|policy_host_id| Some(policy_host_id) == knowledge_base_host_id)
            .unwrap_or(true)
}

fn resolved_source_from_row(row: &PgRow, caps: &PlatformCaps) -> Result<ResolvedSourceConfig> {
    let mut resolved = resolved_policy_source_from_row(row, caps)?;
    let source_config: serde_json::Value = row.get("config_json");
    resolved.approved_repository_uri = text_value(&source_config, "repositoryUri")
        .context("KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID")?
        .to_string();
    resolved.immutable_commit = text_value(&source_config, "commit")
        .context("KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID")?
        .to_string();
    if !valid_repository_uri(&resolved.approved_repository_uri)
        || !valid_commit(&resolved.immutable_commit)
    {
        bail!("KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID");
    }
    let path_policy = source_path_policy(&source_config)?;
    resolved.source_include_prefixes = path_policy.include_prefixes;
    resolved.source_exclude_prefixes = path_policy.exclude_prefixes;
    Ok(resolved)
}

fn resolved_job_source_from_row(row: &PgRow, caps: &PlatformCaps) -> Result<ResolvedSourceConfig> {
    if row.get::<String, _>("source_type") == "GIT_MARKDOWN" {
        resolved_source_from_row(row, caps)
    } else {
        resolved_policy_source_from_row(row, caps)
    }
}

fn resolved_policy_source_from_row(
    row: &PgRow,
    caps: &PlatformCaps,
) -> Result<ResolvedSourceConfig> {
    if !policy_owner_allowed(
        row.get::<Option<bool>, _>("policy_active").unwrap_or(false),
        row.get("policy_host_id"),
        row.get("base_host_id"),
    ) {
        bail!("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE");
    }
    let ingestion_policy_version = row
        .get::<Option<i64>, _>("policy_version")
        .context("KNOWLEDGE_JOB_POLICY_VERSION_UNAVAILABLE")?;
    if ingestion_policy_version < 1 {
        bail!("KNOWLEDGE_JOB_POLICY_VERSION_UNAVAILABLE");
    }
    Ok(ResolvedSourceConfig {
        source_id: row.get("source_id"),
        source_type: row.get("source_type"),
        approved_repository_uri: String::new(),
        immutable_commit: String::new(),
        source_include_prefixes: Vec::new(),
        source_exclude_prefixes: Vec::new(),
        ingestion_policy_id: row.get("ingestion_policy_id"),
        ingestion_policy_version,
        limits: SourceLimits {
            maximum_documents: cap(
                usize::try_from(row.get::<i64, _>("max_documents"))?,
                caps.maximum_documents,
            ),
            maximum_source_bytes: cap(
                u64::try_from(row.get::<i64, _>("max_source_bytes"))?,
                caps.maximum_source_bytes,
            ),
            maximum_chunks: cap(
                usize::try_from(row.get::<i64, _>("max_chunks"))?,
                caps.maximum_chunks,
            ),
            maximum_embedding_tokens: cap(
                usize::try_from(row.get::<i64, _>("max_embedding_tokens"))?,
                caps.maximum_embedding_tokens,
            ),
        },
        maximum_stored_bytes: cap(
            u64::try_from(row.get::<i64, _>("max_stored_bytes"))?,
            caps.maximum_stored_bytes,
        ),
        maximum_spend_micros: cap(
            u64::try_from(row.get::<i64, _>("max_spend_micros"))?,
            caps.maximum_spend_micros,
        ),
        maximum_wall_time_seconds: cap(
            u64::try_from(row.get::<i64, _>("max_wall_time_seconds"))?,
            caps.maximum_wall_time_seconds,
        ),
        maximum_concurrency: cap(
            u32::try_from(row.get::<i32, _>("max_concurrency"))?,
            caps.maximum_concurrency,
        ),
        maximum_provider_calls: caps
            .maximum_provider_calls
            .unwrap_or(DEFAULT_MAXIMUM_PROVIDER_CALLS),
    })
}

fn apply_resolved_source(config: &mut WorkerConfig, source: &ResolvedSourceConfig) {
    config.source_id = source.source_id;
    config.approved_repository_uri = source.approved_repository_uri.clone();
    config.immutable_commit = source.immutable_commit.clone();
    config.source_include_prefixes = source.source_include_prefixes.clone();
    config.source_exclude_prefixes = source.source_exclude_prefixes.clone();
    config.ingestion_policy_id = source.ingestion_policy_id;
    config.ingestion_policy_version = source.ingestion_policy_version;
    config.limits = source.limits.clone();
    config.maximum_stored_bytes = source.maximum_stored_bytes;
    config.maximum_spend_micros = source.maximum_spend_micros;
    config.maximum_checkout_seconds = source.maximum_wall_time_seconds;
    config.maximum_concurrency = source.maximum_concurrency;
    config.maximum_provider_calls = source.maximum_provider_calls;
}

async fn resolve_build_sources(pool: &PgPool, config: &mut WorkerConfig) -> Result<()> {
    let rows = sqlx::query(
        "SELECT source.source_id,source.source_type,source.config_json,source.ingestion_policy_id,
                base.host_id AS base_host_id,policy.host_id AS policy_host_id,
                policy.active AS policy_active,
                policy.version AS policy_version,policy.max_documents,
                policy.max_chunks,policy.max_source_bytes,policy.max_stored_bytes,
                policy.max_embedding_tokens,policy.max_spend_micros,
                policy.max_wall_time_seconds,policy.max_concurrency
           FROM knowledge_source_t source
           JOIN knowledge_base_t base ON base.knowledge_base_id=source.knowledge_base_id
           LEFT JOIN knowledge_ingestion_policy_t policy
             ON policy.ingestion_policy_id=source.ingestion_policy_id
          WHERE source.knowledge_base_id=$1
            AND source.status IN ('DRAFT','ACTIVE')
          ORDER BY source.source_id",
    )
    .bind(config.knowledge_base_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        bail!("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE");
    }
    let sources = rows
        .iter()
        .map(|row| resolved_job_source_from_row(row, &config.platform_caps))
        .collect::<Result<Vec<_>>>()?;
    if !config.source_id.is_nil()
        && !sources
            .iter()
            .any(|source| source.source_id == config.source_id)
    {
        bail!("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE");
    }
    apply_aggregate_source_limits(config, sources)
}

async fn resolve_compaction_sources(pool: &PgPool, config: &mut WorkerConfig) -> Result<()> {
    let rows = sqlx::query(
        "SELECT source.source_id,source.source_type,source.ingestion_policy_id,
                base.host_id AS base_host_id,policy.host_id AS policy_host_id,
                policy.active AS policy_active,policy.version AS policy_version,
                policy.max_documents,policy.max_chunks,policy.max_source_bytes,
                policy.max_stored_bytes,policy.max_embedding_tokens,
                policy.max_spend_micros,policy.max_wall_time_seconds,
                policy.max_concurrency
           FROM knowledge_source_t source
           JOIN knowledge_base_t base ON base.knowledge_base_id=source.knowledge_base_id
           LEFT JOIN knowledge_ingestion_policy_t policy
             ON policy.ingestion_policy_id=source.ingestion_policy_id
          WHERE source.knowledge_base_id=$1
            AND source.status IN ('DRAFT','ACTIVE')
          ORDER BY source.source_id",
    )
    .bind(config.knowledge_base_id)
    .fetch_all(pool)
    .await?;
    if rows.is_empty() {
        bail!("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE");
    }
    let sources = rows
        .iter()
        .map(|row| resolved_policy_source_from_row(row, &config.platform_caps))
        .collect::<Result<Vec<_>>>()?;
    apply_aggregate_source_limits(config, sources)
}

fn apply_aggregate_source_limits(
    config: &mut WorkerConfig,
    sources: Vec<ResolvedSourceConfig>,
) -> Result<()> {
    let mut aggregate_limits = SourceLimits {
        maximum_documents: 0,
        maximum_source_bytes: 0,
        maximum_chunks: 0,
        maximum_embedding_tokens: 0,
    };
    let mut aggregate_stored_bytes = 0_u64;
    let mut aggregate_spend_micros = 0_u64;
    for source in &sources {
        aggregate_limits.maximum_documents = aggregate_limits
            .maximum_documents
            .checked_add(source.limits.maximum_documents)
            .context("aggregate maximumDocuments overflow")?;
        aggregate_limits.maximum_source_bytes = aggregate_limits
            .maximum_source_bytes
            .checked_add(source.limits.maximum_source_bytes)
            .context("aggregate maximumSourceBytes overflow")?;
        aggregate_limits.maximum_chunks = aggregate_limits
            .maximum_chunks
            .checked_add(source.limits.maximum_chunks)
            .context("aggregate maximumChunks overflow")?;
        aggregate_limits.maximum_embedding_tokens = aggregate_limits
            .maximum_embedding_tokens
            .checked_add(source.limits.maximum_embedding_tokens)
            .context("aggregate maximumEmbeddingTokens overflow")?;
        aggregate_stored_bytes = aggregate_stored_bytes
            .checked_add(source.maximum_stored_bytes)
            .context("aggregate maximumStoredBytes overflow")?;
        aggregate_spend_micros = aggregate_spend_micros
            .checked_add(source.maximum_spend_micros)
            .context("aggregate maximumSpendMicros overflow")?;
    }
    config.limits = aggregate_limits;
    config.maximum_stored_bytes = aggregate_stored_bytes;
    config.maximum_spend_micros = aggregate_spend_micros;
    config.maximum_checkout_seconds =
        aggregate_wall_time_seconds(&sources, config.platform_caps.maximum_wall_time_seconds)?;
    config.maximum_concurrency = sources
        .iter()
        .map(|source| source.maximum_concurrency)
        .min()
        .context("KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE")?;
    config.resolved_sources = sources;
    Ok(())
}

async fn record_job_config_snapshot(
    pool: &PgPool,
    job_id: Uuid,
    config: &WorkerConfig,
) -> Result<()> {
    sqlx::query(
        "UPDATE knowledge_job_t SET payload=payload || jsonb_build_object(
             'resolvedConfig',jsonb_build_object(
               'knowledgeBaseId',$2,'sourceId',NULLIF($3,$4),
               'embeddingProfileId',$5,'embeddingProfileRevision',$6,
               'embeddingSpaceId',$7,'embeddingSpaceRevision',$8,
               'ingestionPolicyId',NULLIF($9,$4),'ingestionPolicyVersion',$10,
               'maxDocuments',$11,'maxChunks',$12,'maxSourceBytes',$13,
               'maxStoredBytes',$14,'maxEmbeddingTokens',$15,
               'maxSpendMicros',$16,'maxWallTimeSeconds',$17,
               'maxConcurrency',$18,'snapshotWatermark',$19,
               'sources',$20)),update_ts=now()
          WHERE job_id=$1",
    )
    .bind(job_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .bind(Uuid::nil())
    .bind(config.embedding_profile_id)
    .bind(config.embedding_profile_revision)
    .bind(&config.embedding_space_id)
    .bind(as_i64(config.embedding_space_revision as usize))
    .bind(config.ingestion_policy_id)
    .bind(config.ingestion_policy_version)
    .bind(as_i64(config.limits.maximum_documents))
    .bind(as_i64(config.limits.maximum_chunks))
    .bind(i64::try_from(config.limits.maximum_source_bytes)?)
    .bind(i64::try_from(config.maximum_stored_bytes)?)
    .bind(as_i64(config.limits.maximum_embedding_tokens))
    .bind(i64::try_from(config.maximum_spend_micros)?)
    .bind(i64::try_from(config.maximum_checkout_seconds)?)
    .bind(i32::try_from(config.maximum_concurrency)?)
    .bind(i64::try_from(config.snapshot_watermark)?)
    .bind(resolved_sources_snapshot(config))
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE knowledge_sync_run_t SET ingestion_policy_id=NULLIF($2,$3),
           ingestion_policy_version=NULLIF($4,0),snapshot_watermark=$5,
           phase='CONFIG_RESOLVED',progress=jsonb_build_object(
             'maxDocuments',$6,'maxChunks',$7,'maxSourceBytes',$8,
             'maxStoredBytes',$9,'maxEmbeddingTokens',$10,
             'maxSpendMicros',$11,'maxWallTimeSeconds',$12,
             'maxConcurrency',$13,'sources',$14),update_ts=now()
         WHERE job_id=$1",
    )
    .bind(job_id)
    .bind(config.ingestion_policy_id)
    .bind(Uuid::nil())
    .bind(config.ingestion_policy_version)
    .bind(i64::try_from(config.snapshot_watermark)?)
    .bind(as_i64(config.limits.maximum_documents))
    .bind(as_i64(config.limits.maximum_chunks))
    .bind(i64::try_from(config.limits.maximum_source_bytes)?)
    .bind(i64::try_from(config.maximum_stored_bytes)?)
    .bind(as_i64(config.limits.maximum_embedding_tokens))
    .bind(i64::try_from(config.maximum_spend_micros)?)
    .bind(i64::try_from(config.maximum_checkout_seconds)?)
    .bind(i32::try_from(config.maximum_concurrency)?)
    .bind(resolved_sources_snapshot(config))
    .execute(pool)
    .await?;
    Ok(())
}

fn resolved_sources_snapshot(config: &WorkerConfig) -> serde_json::Value {
    source_snapshots(&config.resolved_sources)
}

fn source_snapshots(sources: &[ResolvedSourceConfig]) -> serde_json::Value {
    serde_json::Value::Array(
        sources
            .iter()
            .map(|source| {
                json!({
                    "sourceId": source.source_id,
                    "sourceType": source.source_type,
                    "repositoryUri": (!source.approved_repository_uri.is_empty())
                        .then_some(source.approved_repository_uri.as_str()),
                    "immutableCommit": (!source.immutable_commit.is_empty())
                        .then_some(source.immutable_commit.as_str()),
                    "ingestionPolicyId": source.ingestion_policy_id,
                    "ingestionPolicyVersion": source.ingestion_policy_version,
                    "effectiveCeilings": {
                        "maxDocuments": source.limits.maximum_documents,
                        "maxProviderCalls": source.maximum_provider_calls,
                        "maxChunks": source.limits.maximum_chunks,
                        "maxSourceBytes": source.limits.maximum_source_bytes,
                        "maxStoredBytes": source.maximum_stored_bytes,
                        "maxEmbeddingTokens": source.limits.maximum_embedding_tokens,
                        "maxSpendMicros": source.maximum_spend_micros,
                        "maxWallTimeSeconds": source.maximum_wall_time_seconds,
                        "maxConcurrency": source.maximum_concurrency
                    }
                })
            })
            .collect(),
    )
}

fn full_base_source_snapshot(sources: Vec<serde_json::Value>) -> serde_json::Value {
    json!({
        "contract": "knowledge-full-base-source-snapshot-v1",
        "sources": sources
    })
}

async fn job_loop(pool: &PgPool, config: &WorkerConfig, lane: WorkerLane) -> Result<()> {
    loop {
        reclaim_expired_jobs(pool, config).await?;
        let mut tx = pool.begin().await?;
        let lane_types = match lane {
            WorkerLane::Priority => PRIORITY_JOB_TYPES,
            WorkerLane::Bulk => BULK_JOB_TYPES,
        };
        let lane_predicate = format!(
            "job_type IN ({})",
            lane_types
                .iter()
                .map(|job_type| format!("'{job_type}'"))
                .collect::<Vec<_>>()
                .join(",")
        );
        let job = sqlx::query(&format!(
            "SELECT job_id,knowledge_base_id,source_id,job_type,payload
               FROM knowledge_job_t
              WHERE state='QUEUED' AND {lane_predicate}
                AND (next_attempt_ts IS NULL OR next_attempt_ts<=now())
              ORDER BY CASE
                WHEN job_type IN ('SYNC','DELTA_SYNC','CONNECTOR_SYNC','ACL_RECONCILE',
                                  'MIGRATION_PREFLIGHT') THEN 0
                WHEN job_type IN ('MIGRATION_PAUSE','MIGRATION_CANCEL') THEN 1
                WHEN job_type IN ('MIGRATION_CATCHUP','MIGRATION_VALIDATE',
                                  'MIGRATION_PROMOTE','MIGRATION_ROLLBACK') THEN 2
                WHEN job_type='MIGRATION_BACKFILL' THEN 3
                ELSE 4 END,
                created_ts FOR UPDATE SKIP LOCKED LIMIT 1"
        ))
        .fetch_optional(&mut *tx)
        .await?;
        let Some(job) = job else {
            tx.rollback().await?;
            if lane == WorkerLane::Priority {
                schedule_due_acl_reconciliation(pool, config).await?;
                tokio::time::sleep(Duration::from_secs(1)).await;
            } else {
                publish_promotion_acknowledgements(pool, config).await?;
                schedule_production_maintenance(pool, config).await?;
                schedule_graph_build(pool, config).await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            continue;
        };
        let job_id: Uuid = job.get("job_id");
        let knowledge_base_id: Uuid = job.get("knowledge_base_id");
        let source_id: Option<Uuid> = job.get("source_id");
        let job_type: String = job.get("job_type");
        let payload: serde_json::Value = job.get("payload");
        if matches!(job_type.as_str(), "SYNC" | "DELTA_SYNC" | "FULL_REINDEX") {
            sqlx::query("SELECT 1 FROM knowledge_base_t WHERE knowledge_base_id=$1 FOR UPDATE")
                .bind(knowledge_base_id)
                .fetch_one(&mut *tx)
                .await?;
            let build_running: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM knowledge_job_t
                  WHERE knowledge_base_id=$1 AND state='RUNNING'
                    AND job_type IN ('SYNC','DELTA_SYNC','FULL_REINDEX'))",
            )
            .bind(knowledge_base_id)
            .fetch_one(&mut *tx)
            .await?;
            if build_running {
                defer_queued_job(&mut tx, job_id).await?;
                tx.commit().await?;
                continue;
            }
        }
        if !job_policy_concurrency_available(
            &mut tx,
            config,
            knowledge_base_id,
            source_id,
            &job_type,
        )
        .await?
        {
            defer_queued_job(&mut tx, job_id).await?;
            tx.commit().await?;
            continue;
        }
        let claim_token = Uuid::now_v7();
        sqlx::query("UPDATE knowledge_job_t SET state='RUNNING',claim_token=$2,lease_expires_ts=now()+interval '5 minutes',attempt_count=attempt_count+1,update_ts=now() WHERE job_id=$1 AND state='QUEUED'")
            .bind(job_id).bind(claim_token).execute(&mut *tx).await?;
        sqlx::query("UPDATE knowledge_sync_run_t SET state='RUNNING',phase='CLAIMED',attempt_count=attempt_count+1,next_attempt_ts=NULL,update_ts=now() WHERE job_id=$1 AND state IN ('ACCEPTED','QUEUED','FAILED','PAUSED_BUDGET')")
            .bind(job_id).execute(&mut *tx).await?;
        tx.commit().await?;
        let config_resolution_started_at =
            sqlx::query_scalar::<_, DateTime<Utc>>("SELECT CURRENT_TIMESTAMP")
                .fetch_one(pool)
                .await?;
        let lease_done = Arc::new(AtomicBool::new(false));
        let lease_task =
            spawn_job_lease_renewal(pool.clone(), job_id, claim_token, Arc::clone(&lease_done));
        let mut resolved_config =
            resolve_job_config(pool, config, knowledge_base_id, source_id).await;
        if job_fetches_full_base_sources(&job_type)
            && let Ok(job_config) = &mut resolved_config
            && let Err(error) = resolve_build_sources(pool, job_config).await
        {
            resolved_config = Err(error);
        } else if job_type == "COMPACTION"
            && let Ok(job_config) = &mut resolved_config
            && let Err(error) = resolve_compaction_sources(pool, job_config).await
        {
            resolved_config = Err(error);
        }
        if let Ok(job_config) = &mut resolved_config {
            if job_type == "SYNC"
                && !job_config.source_id.is_nil()
                && let Some(trigger_source) = job_config
                    .resolved_sources
                    .iter()
                    .find(|source| source.source_id == job_config.source_id)
                    .cloned()
            {
                apply_resolved_source(job_config, &trigger_source);
            }
            job_config.current_job_id = Some(job_id);
            job_config.sync_run_id = sqlx::query_scalar::<_, Uuid>(
                "SELECT sync_run_id FROM knowledge_sync_run_t WHERE job_id=$1",
            )
            .bind(job_id)
            .fetch_optional(pool)
            .await?;
            job_config.coalesce_queued_syncs = job_coalesces_queued_syncs(&job_type);
            job_config.coalesce_created_before = Some(config_resolution_started_at);
        }
        let result = match &resolved_config {
            Ok(job_config) => {
                if let Err(error) = record_job_config_snapshot(pool, job_id, job_config).await {
                    Err(error)
                } else if job_config.resolved_sources.is_empty() {
                    execute_job(pool, job_config, &job_type, &payload).await
                } else {
                    match tokio::time::timeout(
                        Duration::from_secs(job_config.maximum_checkout_seconds),
                        execute_job(pool, job_config, &job_type, &payload),
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!(
                            "KNOWLEDGE_INGESTION_MAX_WALL_TIME_EXCEEDED"
                        )),
                    }
                }
            }
            Err(error) => Err(anyhow::anyhow!(error.to_string())),
        };
        lease_done.store(true, Ordering::Release);
        lease_task.abort();
        let _ = lease_task.await;
        match result {
            Ok(()) => {
                let updated = sqlx::query("UPDATE knowledge_job_t SET state='SUCCEEDED',result=jsonb_build_object('completed',true),claim_token=NULL,lease_expires_ts=NULL,update_ts=now() WHERE job_id=$1 AND state='RUNNING' AND claim_token=$2")
                    .bind(job_id).bind(claim_token).execute(pool).await?;
                if updated.rows_affected() != 1 {
                    tracing::warn!(%job_id, %claim_token,
                        "Knowledge job completed after its claim was lost; terminal update skipped");
                    continue;
                }
                sqlx::query(
                    "UPDATE knowledge_sync_run_t SET state='SUCCEEDED',phase='COMPLETE',
                       error_summary=NULL,finished_ts=now(),update_ts=now()
                     WHERE job_id=$1 AND state='RUNNING'",
                )
                .bind(job_id)
                .execute(pool)
                .await?;
                publish_promotion_acknowledgements(pool, config).await?;
            }
            Err(error) => {
                tracing::error!(job_id=%job_id, %error, "bounded Knowledge build failed");
                let error_code = worker_error_code(&error);
                let updated = sqlx::query("UPDATE knowledge_job_t SET state='FAILED',result=jsonb_build_object('code',$2),claim_token=NULL,lease_expires_ts=NULL,update_ts=now() WHERE job_id=$1 AND state='RUNNING' AND claim_token=$3")
                    .bind(job_id)
                    .bind(error_code)
                    .bind(claim_token)
                    .execute(pool).await?;
                if updated.rows_affected() != 1 {
                    tracing::warn!(%job_id, %claim_token,
                        "Knowledge job failed after its claim was lost; terminal update skipped");
                    continue;
                }
                let sync_state = budget_terminal_state(error_code);
                sqlx::query("UPDATE knowledge_sync_run_t SET state=$2,phase='TERMINAL',error_summary=jsonb_build_object('code',$3),finished_ts=now(),update_ts=now() WHERE job_id=$1 AND state='RUNNING'")
                    .bind(job_id).bind(sync_state).bind(error_code).execute(pool).await?;
                if matches!(
                    job_type.as_str(),
                    "MIGRATION_PREFLIGHT"
                        | "MIGRATION_BACKFILL"
                        | "MIGRATION_CATCHUP"
                        | "MIGRATION_VALIDATE"
                ) {
                    record_migration_job_failure(pool, &payload, &error).await?;
                }
            }
        }
    }
}

async fn defer_queued_job(tx: &mut Transaction<'_, Postgres>, job_id: Uuid) -> Result<()> {
    sqlx::query(
        "UPDATE knowledge_job_t SET next_attempt_ts=now()+interval '1 second',update_ts=now()
          WHERE job_id=$1 AND state='QUEUED'",
    )
    .bind(job_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_sync_run_t SET next_attempt_ts=now()+interval '1 second',update_ts=now()
          WHERE job_id=$1 AND state IN ('ACCEPTED','QUEUED')",
    )
    .bind(job_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn job_policy_concurrency_available(
    tx: &mut Transaction<'_, Postgres>,
    config: &WorkerConfig,
    knowledge_base_id: Uuid,
    source_id: Option<Uuid>,
    job_type: &str,
) -> Result<bool> {
    let policy_wide = matches!(
        job_type,
        "SYNC" | "DELTA_SYNC" | "FULL_REINDEX" | "COMPACTION"
    );
    let policies = if policy_wide {
        sqlx::query(
            "SELECT policy.ingestion_policy_id,policy.max_concurrency
               FROM knowledge_ingestion_policy_t policy
              WHERE policy.active AND EXISTS(
                    SELECT 1 FROM knowledge_source_t source
                     WHERE source.knowledge_base_id=$1
                       AND source.ingestion_policy_id=policy.ingestion_policy_id
                       AND source.status IN ('DRAFT','ACTIVE'))
              ORDER BY policy.ingestion_policy_id FOR UPDATE",
        )
        .bind(knowledge_base_id)
        .fetch_all(&mut **tx)
        .await?
    } else if let Some(source_id) = source_id {
        sqlx::query(
            "SELECT policy.ingestion_policy_id,policy.max_concurrency
               FROM knowledge_ingestion_policy_t policy
               JOIN knowledge_source_t source
                 ON source.ingestion_policy_id=policy.ingestion_policy_id
              WHERE source.source_id=$1 AND policy.active
              ORDER BY policy.ingestion_policy_id FOR UPDATE OF policy",
        )
        .bind(source_id)
        .fetch_all(&mut **tx)
        .await?
    } else {
        Vec::new()
    };
    for policy in policies {
        let ingestion_policy_id: Uuid = policy.get("ingestion_policy_id");
        let maximum_concurrency = cap(
            u32::try_from(policy.get::<i32, _>("max_concurrency"))?,
            config.platform_caps.maximum_concurrency,
        );
        let running: i64 = sqlx::query_scalar(
            "SELECT count(DISTINCT job.job_id)
               FROM knowledge_job_t job
              WHERE job.state='RUNNING' AND (
                    EXISTS(SELECT 1 FROM knowledge_source_t trigger_source
                            WHERE trigger_source.source_id=job.source_id
                              AND trigger_source.ingestion_policy_id=$1)
                 OR (job.job_type IN ('SYNC','DELTA_SYNC','FULL_REINDEX','COMPACTION')
                     AND EXISTS(SELECT 1 FROM knowledge_source_t build_source
                                 WHERE build_source.knowledge_base_id=job.knowledge_base_id
                                   AND build_source.ingestion_policy_id=$1
                                   AND build_source.status IN ('DRAFT','ACTIVE'))))",
        )
        .bind(ingestion_policy_id)
        .fetch_one(&mut **tx)
        .await?;
        if running >= i64::from(maximum_concurrency) {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn execute_job(
    pool: &PgPool,
    config: &WorkerConfig,
    job_type: &str,
    payload: &serde_json::Value,
) -> Result<()> {
    if job_type == "PROMOTE" {
        promote_generation(pool, config, payload).await
    } else if job_type == "PROVIDER_NOTIFICATION" {
        match record_connector_notification(pool, config, payload).await {
            Err(error) => Err(error),
            Ok(()) => match enqueue_connector_job(
                pool,
                config,
                "ACL_RECONCILE",
                "provider-notification-acl",
            )
            .await
            {
                Err(error) => Err(error),
                Ok(()) => {
                    enqueue_connector_job(
                        pool,
                        config,
                        "CONNECTOR_SYNC",
                        "provider-notification-content",
                    )
                    .await
                }
            },
        }
    } else if job_type == "ACL_RECONCILE" {
        connector_build(pool, config, false).await
    } else if job_type == "CONNECTOR_SYNC"
        || (job_type == "SYNC"
            && (config.enterprise_connector_fixture_file.is_some()
                || config.enterprise_connector_page_url.is_some()))
    {
        connector_build(pool, config, true).await
    } else if job_type == "CONNECTIVITY_TEST" {
        if config.enterprise_connector_fixture_file.is_some()
            || config.enterprise_connector_page_url.is_some()
        {
            test_connector_connection(config).await
        } else {
            prepare_checkout(config).await.map(|_| ())
        }
    } else if job_type == "UPLOAD" {
        process_upload(pool, config, payload).await
    } else if job_type == "DELTA_SYNC" {
        incremental_build(pool, config).await
    } else if job_type == "SYNC" && phase1b_schema_ready(pool).await? {
        let mut incremental_config = config.clone();
        incremental_config.coalesce_queued_syncs = false;
        incremental_build(pool, &incremental_config).await
    } else if matches!(job_type, "SYNC" | "FULL_REINDEX") {
        build(pool, config).await
    } else if job_type == "COMPACTION" {
        compact_generation(pool, config).await
    } else if job_type == "ANTI_ENTROPY" {
        run_anti_entropy(pool, config, payload).await
    } else if job_type == "MIGRATION_PREFLIGHT" {
        migration_preflight(pool, config, payload).await
    } else if job_type == "MIGRATION_BACKFILL" {
        migration_backfill(pool, config, payload).await
    } else if job_type == "MIGRATION_CATCHUP" {
        migration_catchup(pool, config, payload).await
    } else if job_type == "MIGRATION_VALIDATE" {
        migration_validate(pool, config, payload).await
    } else if job_type == "MIGRATION_PAUSE" {
        migration_pause(pool, config, payload).await
    } else if job_type == "MIGRATION_CANCEL" {
        migration_cancel(pool, config, payload).await
    } else if job_type == "MIGRATION_PROMOTE" {
        migration_promote(pool, config, payload).await
    } else if job_type == "MIGRATION_ROLLBACK" {
        migration_rollback(pool, config, payload).await
    } else if job_type == "MIGRATION_RETIRE" {
        migration_retire(pool, config, payload).await
    } else if job_type == "BACKUP_CHECKPOINT" {
        create_backup_checkpoint(pool, config, payload).await
    } else if job_type == "RESTORE_VERIFY" {
        verify_restore_checkpoint(pool, config, payload).await
    } else if job_type == "SEGMENT_PURGE" {
        purge_retired_generation(pool, config, payload).await
    } else if job_type == "GRAPH_BUILD" {
        build_graph_artifact(pool, config, payload).await
    } else {
        Err(anyhow::anyhow!(
            "KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED:{job_type}"
        ))
    }
}

async fn reclaim_expired_jobs(pool: &PgPool, _config: &WorkerConfig) -> Result<()> {
    sqlx::query(
        "UPDATE knowledge_job_t
            SET state='FAILED',claim_token=NULL,lease_expires_ts=NULL,
                result=COALESCE(result,'{}'::jsonb) || jsonb_build_object(
                  'code','KNOWLEDGE_JOB_LEASE_RETRY_EXHAUSTED',
                  'leaseExpiryCode','KNOWLEDGE_JOB_LEASE_EXPIRED',
                  'previousFailureCode',result->>'code'),
                update_ts=now()
          WHERE state='RUNNING'
            AND (lease_expires_ts IS NULL OR lease_expires_ts<=now())
            AND attempt_count>=5",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE knowledge_sync_run_t run SET state='FAILED',phase='TERMINAL',
           error_summary=jsonb_build_object('code','KNOWLEDGE_JOB_LEASE_RETRY_EXHAUSTED'),
           finished_ts=now(),update_ts=now()
         FROM knowledge_job_t job WHERE run.job_id=job.job_id
           AND job.state='FAILED' AND job.result->>'code'='KNOWLEDGE_JOB_LEASE_RETRY_EXHAUSTED'
           AND run.state='RUNNING'",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE knowledge_job_t
            SET state='QUEUED',claim_token=NULL,lease_expires_ts=NULL,
                next_attempt_ts=now()+make_interval(secs =>
                  LEAST(3600,60*power(2,LEAST(attempt_count,6)))::int),
                result=COALESCE(result,'{}'::jsonb) || jsonb_build_object(
                  'leaseExpiryCode','KNOWLEDGE_JOB_LEASE_EXPIRED'),
                update_ts=now()
          WHERE state='RUNNING'
            AND (lease_expires_ts IS NULL OR lease_expires_ts<=now())
            AND attempt_count<5",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "UPDATE knowledge_sync_run_t run SET state='QUEUED',phase='RETRY_WAIT',
           next_attempt_ts=job.next_attempt_ts,attempt_count=job.attempt_count,
           progress=run.progress || jsonb_build_object(
             'lastFailureCode','KNOWLEDGE_JOB_LEASE_EXPIRED'),update_ts=now()
         FROM knowledge_job_t job WHERE run.job_id=job.job_id
           AND job.state='QUEUED' AND run.state='RUNNING'",
    )
    .execute(pool)
    .await?;
    Ok(())
}

fn spawn_job_lease_renewal(
    pool: PgPool,
    job_id: Uuid,
    claim_token: Uuid,
    done: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while !done.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if done.load(Ordering::Acquire) {
                break;
            }
            let renewed = sqlx::query(
                "UPDATE knowledge_job_t
                    SET lease_expires_ts=now()+interval '5 minutes',update_ts=now()
                  WHERE job_id=$1 AND state='RUNNING' AND claim_token=$2",
            )
            .bind(job_id)
            .bind(claim_token)
            .execute(&pool)
            .await;
            match renewed {
                Ok(result) if result.rows_affected() == 0 => break,
                Ok(_) => {}
                Err(error) => tracing::warn!(%job_id, %error,
                    "Knowledge job lease renewal failed transiently; retrying"),
            }
        }
    })
}

async fn record_migration_job_failure(
    pool: &PgPool,
    payload: &serde_json::Value,
    error: &anyhow::Error,
) -> Result<()> {
    let Some(migration_id) = payload
        .get("migrationId")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
    else {
        return Ok(());
    };
    let code = migration_failure_code(error);
    if code == "KNOWLEDGE_MIGRATION_EXTERNAL_EVALUATION_REQUIRED" {
        return Ok(());
    }
    sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='PAUSED',pause_reason='WORKER_FAILURE',failure_code=$2,
                version=version+1,update_ts=now()
          WHERE migration_id=$1 AND state IN (
            'PREFLIGHTED','BACKFILLING','CATCHING_UP','VALIDATING')",
    )
    .bind(migration_id)
    .bind(code)
    .execute(pool)
    .await?;
    Ok(())
}

fn migration_failure_code(error: &anyhow::Error) -> String {
    error
        .chain()
        .flat_map(|cause| {
            cause
                .to_string()
                .split(|character: char| {
                    !(character.is_ascii_uppercase()
                        || character.is_ascii_digit()
                        || character == '_')
                })
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .find(|token| token.starts_with("KNOWLEDGE_") && token.len() <= 96)
        .unwrap_or_else(|| "KNOWLEDGE_MIGRATION_DEPENDENCY_FAILURE".to_string())
}

fn worker_error_code(error: &anyhow::Error) -> &'static str {
    let detail = error.to_string();
    if error
        .to_string()
        .starts_with("KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED:")
    {
        "KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED"
    } else if detail.contains("SOURCE_SPEND_BUDGET_UNAVAILABLE") {
        "KNOWLEDGE_INGESTION_SOURCE_SPEND_BUDGET_UNAVAILABLE"
    } else if detail.contains("SPEND_BUDGET_REQUIRED") {
        "KNOWLEDGE_INGESTION_SPEND_BUDGET_REQUIRED"
    } else if detail.contains("spend budget")
        || detail.contains("billed-cost")
        || detail.contains("SPEND_BUDGET_EXCEEDED")
    {
        "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED"
    } else if detail.contains("maximum_documents") {
        "KNOWLEDGE_INGESTION_MAX_DOCUMENTS_EXCEEDED"
    } else if detail.contains("maximum_chunks") {
        "KNOWLEDGE_INGESTION_MAX_CHUNKS_EXCEEDED"
    } else if detail.contains("maximum_source_bytes") {
        "KNOWLEDGE_INGESTION_MAX_SOURCE_BYTES_EXCEEDED"
    } else if detail.contains("maximum_embedding_tokens") {
        "KNOWLEDGE_INGESTION_MAX_EMBEDDING_TOKENS_EXCEEDED"
    } else if detail.contains("PROVIDER_CALL_LIMIT") {
        "KNOWLEDGE_INGESTION_MAX_PROVIDER_CALLS_EXCEEDED"
    } else if detail.contains("MAX_STORED_BYTES") {
        "KNOWLEDGE_INGESTION_MAX_STORED_BYTES_EXCEEDED"
    } else if detail.contains("MAX_WALL_TIME") {
        "KNOWLEDGE_INGESTION_MAX_WALL_TIME_EXCEEDED"
    } else if detail.contains("SOURCE_OR_POLICY_UNAVAILABLE") {
        "KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE"
    } else if detail.contains("EMBEDDING_PROFILE_UNAVAILABLE") {
        "KNOWLEDGE_JOB_EMBEDDING_PROFILE_UNAVAILABLE"
    } else if detail.contains("IMMUTABLE_GIT_CONFIG_INVALID") {
        "KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID"
    } else if detail.contains("SOURCE_INCLUDE_POLICY_UNSUPPORTED") {
        "KNOWLEDGE_SOURCE_INCLUDE_POLICY_UNSUPPORTED"
    } else if detail.contains("SOURCE_EXCLUDE_POLICY_INVALID") {
        "KNOWLEDGE_SOURCE_EXCLUDE_POLICY_INVALID"
    } else {
        "KNOWLEDGE_BUILD_FAILED"
    }
}

fn is_budget_error_code(error_code: &str) -> bool {
    error_code.starts_with("KNOWLEDGE_INGESTION_MAX_")
        || error_code == "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED"
        || error_code == "KNOWLEDGE_INGESTION_SPEND_BUDGET_REQUIRED"
}

fn budget_terminal_state(error_code: &str) -> &'static str {
    if error_code == "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED" {
        "PAUSED_BUDGET"
    } else if is_budget_error_code(error_code) {
        "FAILED_BUDGET"
    } else {
        "FAILED"
    }
}

async fn enqueue_migration_job(
    pool: &PgPool,
    config: &WorkerConfig,
    migration_id: Uuid,
    job_type: &str,
    progress_identity: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,job_type,idempotency_key,requested_by,payload)
         VALUES($1,$2,$3,$4,'light-knowledge-migration',$5)
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(config.knowledge_base_id)
    .bind(job_type)
    .bind(format!(
        "migration:{migration_id}:{job_type}:{progress_identity}"
    ))
    .bind(json!({"migrationId": migration_id}))
    .execute(pool)
    .await?;
    Ok(())
}

async fn migration_preflight(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    if !config.embedding_migration_enabled {
        bail!("KNOWLEDGE_EMBEDDING_MIGRATION_DISABLED");
    }
    let migration_id = uuid_value(payload, "migrationId")?;
    let candidate_generation_id = uuid_value(payload, "candidateGenerationId")?;
    let target_profile_id = uuid_value(payload, "targetEmbeddingProfileId")?;
    let target_profile_revision = payload
        .get("targetEmbeddingProfileRevision")
        .and_then(serde_json::Value::as_i64)
        .context("migration request requires targetEmbeddingProfileRevision")?;
    let expected_active_generation_id = uuid_value(payload, "expectedActiveGenerationId")?;
    let accepted_cost_ceiling = payload
        .get("acceptedCostCeilingMicros")
        .and_then(serde_json::Value::as_i64)
        .context("migration request requires acceptedCostCeilingMicros")?;
    let estimate_version = payload
        .get("estimateVersion")
        .and_then(serde_json::Value::as_i64)
        .context("migration request requires estimateVersion")?;
    if estimate_version != 1 {
        bail!("KNOWLEDGE_MIGRATION_ESTIMATE_VERSION_UNSUPPORTED");
    }
    let rollback_window_seconds = payload
        .get("rollbackWindowSeconds")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(86_400);
    if !(300..=2_592_000).contains(&rollback_window_seconds) {
        bail!("KNOWLEDGE_MIGRATION_ROLLBACK_WINDOW_INVALID");
    }
    let requested_by = payload
        .get("requestedBy")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("portal-operator");
    let mut tx = pool.begin().await?;
    let active = sqlx::query(
        "SELECT pointer.index_generation_id,generation.final_watermark,
                generation.space_id,generation.space_revision,generation.dimension
           FROM knowledge_index_pointer_t pointer
           JOIN knowledge_index_generation_t generation
             ON generation.index_generation_id=pointer.index_generation_id
          WHERE pointer.knowledge_base_id=$1 AND pointer.environment=$2
          FOR UPDATE OF pointer",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .fetch_one(&mut *tx)
    .await?;
    if active.get::<Uuid, _>("index_generation_id") != expected_active_generation_id {
        bail!("KNOWLEDGE_MIGRATION_ACTIVE_GENERATION_CONFLICT");
    }
    let target = sqlx::query(
        "SELECT expected_space_id,expected_space_revision,dimension,
                document_input_transform_version,query_input_transform_version,
                alias_name
           FROM knowledge_embedding_profile_runtime_v
          WHERE profile_id=$1 AND profile_revision=$2",
    )
    .bind(target_profile_id)
    .bind(target_profile_revision)
    .fetch_one(&mut *tx)
    .await?;
    let target_space_id: String = target.get("expected_space_id");
    let target_space_revision: i64 = target.get("expected_space_revision");
    let target_dimension: i32 = target.get("dimension");
    if target_space_id == active.get::<String, _>("space_id")
        && target_space_revision == active.get::<i64, _>("space_revision")
        && target_dimension == active.get::<i32, _>("dimension")
    {
        bail!("KNOWLEDGE_MIGRATION_TARGET_SPACE_UNCHANGED");
    }
    if config.migration_deterministic_pilot
        && usize::try_from(target_dimension).ok() != Some(knowledge_core::FAKE_DIMENSION)
    {
        bail!("KNOWLEDGE_MIGRATION_PILOT_DIMENSION_UNSUPPORTED");
    }
    let estimate = sqlx::query(
        "SELECT count(*)::bigint AS chunk_count,
                COALESCE(sum(chunk.token_count),0)::bigint AS token_count,
                COALESCE(sum(length(chunk.chunk_text)),0)::bigint AS source_bytes
           FROM knowledge_resolved_generation_chunk($2) resolved
           JOIN knowledge_chunk_t chunk ON chunk.chunk_id=resolved.chunk_id",
    )
    .bind(config.knowledge_base_id)
    .bind(expected_active_generation_id)
    .fetch_one(&mut *tx)
    .await?;
    let chunk_count: i64 = estimate.get("chunk_count");
    let token_count: i64 = estimate.get("token_count");
    let source_bytes: i64 = estimate.get("source_bytes");
    let policy = sqlx::query(
        "SELECT maximum_migration_cost_micros,
                migration_cost_per_token_micros::float8 AS cost_per_token
           FROM knowledge_operational_policy_t WHERE knowledge_base_id=$1",
    )
    .bind(config.knowledge_base_id)
    .fetch_optional(&mut *tx)
    .await?;
    let (policy_ceiling, cost_per_token) = policy
        .map(|row| {
            (
                row.get::<i64, _>("maximum_migration_cost_micros"),
                row.get::<f64, _>("cost_per_token"),
            )
        })
        .unwrap_or((100_000_000, 1.0));
    let estimated_cost = ((token_count as f64) * cost_per_token).ceil() as i64;
    if accepted_cost_ceiling < estimated_cost || accepted_cost_ceiling > policy_ceiling {
        bail!("KNOWLEDGE_MIGRATION_COST_APPROVAL_INVALID");
    }
    let watermark = active.get::<Option<i64>, _>("final_watermark").unwrap_or(0);
    sqlx::query(
        "INSERT INTO knowledge_embedding_migration_t(
           migration_id,knowledge_base_id,environment,source_generation_id,
           candidate_generation_id,target_profile_id,target_profile_revision,
           target_space_id,target_space_revision,target_dimension,estimate_version,
           estimated_chunk_count,estimated_token_count,estimated_cost_micros,
           estimated_duration_seconds,estimated_temporary_bytes,
           accepted_cost_ceiling_micros,rollback_window_seconds,
           start_watermark,snapshot_watermark,
           predecessor_reconciled_watermark,state,requested_by)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,
                GREATEST(1,ceil($12::numeric/32)::bigint),$15,$16,$17,$18,$18,$18,
                'PREFLIGHTED',$19)
         ON CONFLICT(migration_id) DO NOTHING",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .bind(expected_active_generation_id)
    .bind(candidate_generation_id)
    .bind(target_profile_id)
    .bind(target_profile_revision)
    .bind(&target_space_id)
    .bind(target_space_revision)
    .bind(target_dimension)
    .bind(estimate_version)
    .bind(chunk_count)
    .bind(token_count)
    .bind(estimated_cost)
    .bind(source_bytes.saturating_add(chunk_count.saturating_mul(i64::from(target_dimension) * 4)))
    .bind(accepted_cost_ceiling)
    .bind(rollback_window_seconds)
    .bind(watermark)
    .bind(requested_by)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    enqueue_migration_job(pool, config, migration_id, "MIGRATION_BACKFILL", "initial").await
}

async fn migration_pause(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='PAUSED',pause_reason=COALESCE($3,'OPERATOR_REQUEST'),
                version=version+1,update_ts=now()
          WHERE migration_id=$1 AND knowledge_base_id=$2
            AND state IN ('PREFLIGHTED','BACKFILLING','CATCHING_UP','VALIDATING')
            AND version=$4",
    )
    .bind(uuid_value(payload, "migrationId")?)
    .bind(config.knowledge_base_id)
    .bind(payload.get("reason").and_then(serde_json::Value::as_str))
    .bind(
        payload
            .get("expectedMigrationVersion")
            .and_then(serde_json::Value::as_i64)
            .context("pause requires expectedMigrationVersion")?,
    )
    .execute(pool)
    .await?;
    if affected.rows_affected() != 1 {
        bail!("KNOWLEDGE_MIGRATION_VERSION_OR_STATE_CONFLICT");
    }
    Ok(())
}

async fn migration_cancel(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let affected = sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='CANCELLED',version=version+1,finished_ts=now(),update_ts=now()
          WHERE migration_id=$1 AND knowledge_base_id=$2
            AND state IN ('REQUESTED','PREFLIGHTED','BACKFILLING','PAUSED',
                          'CATCHING_UP','VALIDATING','READY')
            AND version=$3",
    )
    .bind(uuid_value(payload, "migrationId")?)
    .bind(config.knowledge_base_id)
    .bind(
        payload
            .get("expectedMigrationVersion")
            .and_then(serde_json::Value::as_i64)
            .context("cancel requires expectedMigrationVersion")?,
    )
    .execute(pool)
    .await?;
    if affected.rows_affected() != 1 {
        bail!("KNOWLEDGE_MIGRATION_VERSION_OR_STATE_CONFLICT");
    }
    Ok(())
}

async fn initialize_migration_candidate(
    pool: &PgPool,
    config: &WorkerConfig,
    migration_id: Uuid,
) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let migration = sqlx::query(
        "SELECT migration.*,profile.document_input_transform_version,
                profile.query_input_transform_version,
                source.parser_contract_digest,source.chunker_contract_digest,
                source.metadata_contract_digest,source.citation_contract_digest,
                source.acl_normalization_contract_digest,source.lexical_contract_digest,
                source.contract_set_digest
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_embedding_profile_t profile
             ON profile.profile_id=migration.target_profile_id
            AND profile.profile_revision=migration.target_profile_revision
           JOIN knowledge_index_generation_t source
             ON source.index_generation_id=migration.source_generation_id
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
          FOR UPDATE OF migration",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    let state: String = migration.get("state");
    if matches!(state.as_str(), "CANCELLED" | "FAILED" | "RETIRED") {
        tx.rollback().await?;
        bail!("KNOWLEDGE_MIGRATION_TERMINAL");
    }
    if state == "PAUSED" {
        tx.rollback().await?;
        return Ok(migration.get("candidate_generation_id"));
    }
    let candidate_generation_id: Uuid = migration.get("candidate_generation_id");
    let segment_id = derived_uuid("migration-base", candidate_generation_id);
    let manifest_digest = sha256_hex(
        format!(
            "migration:{migration_id}:{candidate_generation_id}:{}:{}",
            migration.get::<String, _>("target_space_id"),
            migration.get::<i64, _>("target_space_revision")
        )
        .as_bytes(),
    );
    let embedding_contract_digest = sha256_hex(
        format!(
            "embedding:{}:{}:{}:{}",
            migration.get::<String, _>("target_space_id"),
            migration.get::<i64, _>("target_space_revision"),
            migration.get::<i32, _>("target_dimension"),
            migration.get::<String, _>("document_input_transform_version")
        )
        .as_bytes(),
    );
    let contract_set_digest = sha256_hex(
        format!(
            "{}:{embedding_contract_digest}",
            migration.get::<String, _>("contract_set_digest")
        )
        .as_bytes(),
    );
    sqlx::query(
        "INSERT INTO knowledge_index_generation_t(
           index_generation_id,knowledge_base_id,embedding_profile_id,
           embedding_profile_revision,space_id,space_revision,dimension,
           parser_contract_digest,chunker_contract_digest,metadata_contract_digest,
           citation_contract_digest,acl_normalization_contract_digest,
           lexical_contract_digest,contract_set_digest,query_input_transform_version,
           snapshot_watermark,ordered_segment_manifest_digest,state,evidence)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,
                'BUILDING',jsonb_build_object('migrationId',$18,'canonicalChunksReused',true))
         ON CONFLICT(index_generation_id) DO NOTHING",
    )
    .bind(candidate_generation_id)
    .bind(config.knowledge_base_id)
    .bind(migration.get::<Uuid, _>("target_profile_id"))
    .bind(migration.get::<i64, _>("target_profile_revision"))
    .bind(migration.get::<String, _>("target_space_id"))
    .bind(migration.get::<i64, _>("target_space_revision"))
    .bind(migration.get::<i32, _>("target_dimension"))
    .bind(migration.get::<String, _>("parser_contract_digest"))
    .bind(migration.get::<String, _>("chunker_contract_digest"))
    .bind(migration.get::<String, _>("metadata_contract_digest"))
    .bind(migration.get::<String, _>("citation_contract_digest"))
    .bind(migration.get::<String, _>("acl_normalization_contract_digest"))
    .bind(migration.get::<String, _>("lexical_contract_digest"))
    .bind(contract_set_digest)
    .bind(migration.get::<String, _>("query_input_transform_version"))
    .bind(migration.get::<i64, _>("snapshot_watermark"))
    .bind(&manifest_digest)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_index_segment_t(
           index_segment_id,knowledge_base_id,index_generation_id,segment_kind,state,
           snapshot_watermark,parser_contract_digest,chunker_contract_digest,
           lexical_contract_digest,embedding_contract_digest,acl_contract_digest,
           physical_locator,manifest_digest,document_count,chunk_count,vector_count,acl_count)
         SELECT $1,$2,$3,'BASE','BUILDING',$4,$5,$6,$7,$8,$9,$10,$11,
                count(DISTINCT resolved.document_id),count(resolved.chunk_id),0,
                count(DISTINCT resolved.document_id)
           FROM knowledge_resolved_generation_chunk(
             (SELECT source_generation_id FROM knowledge_embedding_migration_t
               WHERE migration_id=$12)) resolved
         ON CONFLICT(index_segment_id) DO NOTHING",
    )
    .bind(segment_id)
    .bind(config.knowledge_base_id)
    .bind(candidate_generation_id)
    .bind(migration.get::<i64, _>("snapshot_watermark"))
    .bind(migration.get::<String, _>("parser_contract_digest"))
    .bind(migration.get::<String, _>("chunker_contract_digest"))
    .bind(migration.get::<String, _>("lexical_contract_digest"))
    .bind(embedding_contract_digest)
    .bind(migration.get::<String, _>("acl_normalization_contract_digest"))
    .bind(format!(
        "object://light-knowledge/migrations/{migration_id}/manifest.json"
    ))
    .bind(&manifest_digest)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_generation_segment_t(
           index_generation_id,ordinal,index_segment_id)
         VALUES($1,0,$2) ON CONFLICT DO NOTHING",
    )
    .bind(candidate_generation_id)
    .bind(segment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_embedding_migration_chunk_t(
           migration_id,chunk_id,knowledge_base_id,transformed_input_digest,token_count)
         SELECT $1,chunk.chunk_id,$2,
                encode(digest(chunk.chunk_text || ':' || $3,'sha256'),'hex'),
                chunk.token_count
           FROM knowledge_resolved_generation_chunk(
             (SELECT source_generation_id FROM knowledge_embedding_migration_t
               WHERE migration_id=$1)) resolved
           JOIN knowledge_chunk_t chunk ON chunk.chunk_id=resolved.chunk_id
         ON CONFLICT DO NOTHING",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .bind(migration.get::<String, _>("document_input_transform_version"))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_segment_document_t(
           index_segment_id,document_id,knowledge_base_id,document_version_id,acl_revision_id)
         SELECT DISTINCT ON(resolved.document_id) $1,resolved.document_id,$2,
                resolved.document_version_id,resolved.acl_revision_id
           FROM knowledge_resolved_generation_chunk(
             (SELECT source_generation_id FROM knowledge_embedding_migration_t
               WHERE migration_id=$3)) resolved
         ON CONFLICT(index_segment_id,document_id) DO UPDATE SET
           document_version_id=EXCLUDED.document_version_id,
           acl_revision_id=EXCLUDED.acl_revision_id",
    )
    .bind(segment_id)
    .bind(config.knowledge_base_id)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_segment_chunk_t(
           index_segment_id,chunk_id,knowledge_base_id,acl_revision_id)
         SELECT $1,resolved.chunk_id,$2,resolved.acl_revision_id
           FROM knowledge_resolved_generation_chunk(
             (SELECT source_generation_id FROM knowledge_embedding_migration_t
               WHERE migration_id=$3)) resolved
         ON CONFLICT(index_segment_id,chunk_id) DO UPDATE SET
           acl_revision_id=EXCLUDED.acl_revision_id",
    )
    .bind(segment_id)
    .bind(config.knowledge_base_id)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    if state != "BACKFILLING" {
        sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='BACKFILLING',version=version+1,pause_reason=NULL,update_ts=now()
              WHERE migration_id=$1",
        )
        .bind(migration_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(segment_id)
}

struct MigrationEmbeddingTarget {
    alias: String,
    space_id: String,
    space_revision: i64,
    dimension: i32,
}

struct MigrationEmbeddingBatch {
    vectors: Vec<Vec<f32>>,
    billed_cost_micros: i64,
}

fn allocate_exact(total: i64, weights: &[i64]) -> Vec<i64> {
    if weights.is_empty() {
        return Vec::new();
    }
    let weight_total = weights.iter().copied().sum::<i64>().max(1);
    let mut prefix = 0i64;
    let mut allocated = 0i64;
    weights
        .iter()
        .map(|weight| {
            prefix = prefix.saturating_add((*weight).max(0));
            let next = i64::try_from(
                i128::from(total).saturating_mul(i128::from(prefix)) / i128::from(weight_total),
            )
            .unwrap_or(total);
            let value = next.saturating_sub(allocated);
            allocated = next;
            value
        })
        .collect()
}

async fn embed_migration_texts(
    config: &WorkerConfig,
    migration_id: Uuid,
    target: &MigrationEmbeddingTarget,
    texts: &[String],
    cost_ceilings: &[i64],
    maximum_billed_cost_micros: i64,
) -> Result<MigrationEmbeddingBatch> {
    if texts.len() != cost_ceilings.len()
        || cost_ceilings.iter().any(|cost| *cost < 0)
        || cost_ceilings.iter().copied().sum::<i64>() != maximum_billed_cost_micros
    {
        bail!("KNOWLEDGE_MIGRATION_COST_RESERVATION_INVALID");
    }
    if config.migration_deterministic_pilot {
        let vectors = texts
            .iter()
            .map(|text| knowledge_core::fake_embedding(text))
            .collect::<Vec<_>>();
        if vectors
            .iter()
            .any(|vector| vector.len() != usize::try_from(target.dimension).unwrap_or_default())
        {
            bail!("KNOWLEDGE_MIGRATION_EMBEDDING_DIMENSION_MISMATCH");
        }
        return Ok(MigrationEmbeddingBatch {
            vectors,
            billed_cost_micros: maximum_billed_cost_micros,
        });
    }
    let endpoint = config
        .embedding_gateway_url
        .as_deref()
        .context("migration embedding requires embeddingGatewayUrl")?;
    let token = fs::read_to_string(
        config
            .embedding_authorization_file
            .as_ref()
            .context("migration embedding requires embeddingAuthorizationFile")?,
    )?;
    let client = embedding_http_client(config)?;
    let mut pending = vec![(0usize, texts.len())];
    let mut vectors = vec![None; texts.len()];
    let mut billed_cost_micros = 0i64;
    while let Some((start, end)) = pending.pop() {
        let slice_cost_ceiling = cost_ceilings[start..end].iter().copied().sum::<i64>();
        let input_digest = sha256_hex(texts[start..end].join("\n").as_bytes());
        let response = client
            .post(endpoint)
            .bearer_auth(token.trim())
            .header(
                "x-request-id",
                format!("kb-migration:{migration_id}:{input_digest}"),
            )
            .header("x-light-expected-embedding-space-id", &target.space_id)
            .header(
                "x-light-expected-embedding-space-revision",
                target.space_revision.to_string(),
            )
            .header(
                "x-light-maximum-billed-cost-micros",
                slice_cost_ceiling.to_string(),
            )
            .json(&json!({
                "model": target.alias,
                "input": &texts[start..end],
                "dimensions": target.dimension
            }))
            .send()
            .await;
        let parsed = match response {
            Ok(response)
                if response.status().is_success()
                    && response
                        .headers()
                        .get("x-light-embedding-space-id")
                        .and_then(|value| value.to_str().ok())
                        == Some(target.space_id.as_str())
                    && response
                        .headers()
                        .get("x-light-embedding-space-revision")
                        .and_then(|value| value.to_str().ok())
                        == Some(target.space_revision.to_string().as_str()) =>
            {
                let billed = response
                    .headers()
                    .get("x-light-billed-cost-micros")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<i64>().ok())
                    .filter(|value| *value >= 0 && *value <= slice_cost_ceiling)
                    .context("embedding gateway omitted bounded billed-cost evidence")?;
                Some((response.json::<serde_json::Value>().await?, billed))
            }
            _ => None,
        };
        let batch = parsed
            .as_ref()
            .and_then(|(body, _)| body.get("data"))
            .and_then(serde_json::Value::as_array)
            .filter(|data| data.len() == end - start)
            .and_then(|data| {
                data.iter()
                    .enumerate()
                    .map(|(expected, item)| {
                        let index = item.get("index")?.as_u64()? as usize;
                        let values = item
                            .get("embedding")?
                            .as_array()?
                            .iter()
                            .map(|value| value.as_f64().map(|value| value as f32))
                            .collect::<Option<Vec<_>>>()?;
                        (index == expected
                            && values.len()
                                == usize::try_from(target.dimension).unwrap_or_default()
                            && values.iter().all(|value| value.is_finite()))
                        .then_some(values)
                    })
                    .collect::<Option<Vec<_>>>()
            });
        if let Some(batch) = batch {
            billed_cost_micros = billed_cost_micros.saturating_add(
                parsed
                    .as_ref()
                    .map(|(_, billed)| *billed)
                    .unwrap_or_default(),
            );
            for (offset, vector) in batch.into_iter().enumerate() {
                vectors[start + offset] = Some(vector);
            }
        } else if end - start > 1 {
            let middle = start + (end - start) / 2;
            pending.push((middle, end));
            pending.push((start, middle));
        } else {
            bail!("KNOWLEDGE_MIGRATION_EMBEDDING_FAILED");
        }
    }
    let vectors = vectors
        .into_iter()
        .map(|vector| vector.context("migration embedding response omitted a chunk"))
        .collect::<Result<Vec<_>>>()?;
    if billed_cost_micros > maximum_billed_cost_micros {
        bail!("KNOWLEDGE_MIGRATION_PROVIDER_COST_CEILING_EXCEEDED");
    }
    Ok(MigrationEmbeddingBatch {
        vectors,
        billed_cost_micros,
    })
}

async fn migration_backfill(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    if let Some(expected_version) = payload
        .get("expectedMigrationVersion")
        .and_then(serde_json::Value::as_i64)
    {
        let resumed = sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='BACKFILLING',pause_reason=NULL,version=version+1,update_ts=now()
              WHERE migration_id=$1 AND knowledge_base_id=$2
                AND state='PAUSED' AND version=$3",
        )
        .bind(migration_id)
        .bind(config.knowledge_base_id)
        .bind(expected_version)
        .execute(pool)
        .await?;
        if resumed.rows_affected() != 1 {
            bail!("KNOWLEDGE_MIGRATION_VERSION_OR_STATE_CONFLICT");
        }
    }
    let segment_id = initialize_migration_candidate(pool, config, migration_id).await?;
    let mut claim_tx = pool.begin().await?;
    let migration = sqlx::query(
        "SELECT migration.state,migration.target_profile_id,
                migration.target_profile_revision,migration.target_space_id,
                target_space_revision,target_dimension,estimated_token_count,
                estimated_cost_micros,completed_chunk_count,consumed_cost_micros,
                reserved_cost_micros,accepted_cost_ceiling_micros,
                profile.document_input_transform_version,profile.alias_name
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_embedding_profile_runtime_v profile
             ON profile.profile_id=migration.target_profile_id
            AND profile.profile_revision=migration.target_profile_revision
          WHERE migration_id=$1 FOR UPDATE OF migration",
    )
    .bind(migration_id)
    .fetch_one(&mut *claim_tx)
    .await?;
    if migration.get::<String, _>("state") == "PAUSED" {
        claim_tx.rollback().await?;
        return Ok(());
    }
    let rows = sqlx::query(
        "SELECT item.chunk_id,item.transformed_input_digest,item.token_count,
                item.state,item.reserved_cost_micros,chunk.chunk_text
           FROM knowledge_embedding_migration_chunk_t item
           JOIN knowledge_chunk_t chunk ON chunk.chunk_id=item.chunk_id
          WHERE item.migration_id=$1
            AND (item.state='PENDING'
                 OR (item.state='CLAIMED' AND item.claim_expires_ts<=now()))
          ORDER BY item.chunk_id LIMIT $2 FOR UPDATE OF item SKIP LOCKED",
    )
    .bind(migration_id)
    .bind(i64::try_from(config.embedding_batch_size).unwrap_or(32))
    .fetch_all(&mut *claim_tx)
    .await?;
    if rows.is_empty() {
        let next_claim_expiry: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT min(claim_expires_ts) FROM knowledge_embedding_migration_chunk_t
              WHERE migration_id=$1 AND state='CLAIMED'",
        )
        .bind(migration_id)
        .fetch_one(&mut *claim_tx)
        .await?;
        if let Some(next_claim_expiry) = next_claim_expiry {
            claim_tx.rollback().await?;
            sqlx::query(
                "INSERT INTO knowledge_job_t(
                   job_id,knowledge_base_id,job_type,idempotency_key,requested_by,
                   payload,next_attempt_ts)
                 VALUES($1,$2,'MIGRATION_BACKFILL',$3,'light-knowledge-migration',$4,$5)
                 ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
            )
            .bind(Uuid::now_v7())
            .bind(config.knowledge_base_id)
            .bind(format!(
                "migration:{migration_id}:claim-expiry:{}",
                next_claim_expiry.timestamp()
            ))
            .bind(json!({"migrationId": migration_id}))
            .bind(next_claim_expiry)
            .execute(pool)
            .await?;
            return Ok(());
        }
        sqlx::query(
            "UPDATE knowledge_index_generation_t SET state='CATCHING_UP'
              WHERE index_generation_id=(SELECT candidate_generation_id
                FROM knowledge_embedding_migration_t WHERE migration_id=$1)
                AND state='BUILDING'",
        )
        .bind(migration_id)
        .execute(&mut *claim_tx)
        .await?;
        let changed = sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='CATCHING_UP',version=version+1,update_ts=now()
              WHERE migration_id=$1 AND state='BACKFILLING'",
        )
        .bind(migration_id)
        .execute(&mut *claim_tx)
        .await?;
        claim_tx.commit().await?;
        if changed.rows_affected() == 1 {
            enqueue_migration_job(pool, config, migration_id, "MIGRATION_CATCHUP", "initial")
                .await?;
        }
        return Ok(());
    }
    let claim_token = Uuid::now_v7();
    let pending_tokens = rows
        .iter()
        .filter(|row| row.get::<String, _>("state") == "PENDING")
        .map(|row| i64::from(row.get::<i32, _>("token_count")))
        .sum::<i64>();
    let estimated_tokens = migration.get::<i64, _>("estimated_token_count").max(1);
    let estimated_cost = migration.get::<i64, _>("estimated_cost_micros");
    let new_reservation = i64::try_from(
        i128::from(pending_tokens)
            .saturating_mul(i128::from(estimated_cost))
            .saturating_add(i128::from(estimated_tokens - 1))
            / i128::from(estimated_tokens),
    )
    .unwrap_or(i64::MAX);
    let pending_weights = rows
        .iter()
        .filter(|row| row.get::<String, _>("state") == "PENDING")
        .map(|row| i64::from(row.get::<i32, _>("token_count")))
        .collect::<Vec<_>>();
    let pending_allocations = allocate_exact(new_reservation, &pending_weights);
    let mut pending_index = 0usize;
    let mut claim_reservation = 0i64;
    let mut row_reservations = Vec::with_capacity(rows.len());
    for row in &rows {
        let chunk_id: Uuid = row.get("chunk_id");
        let reservation = if row.get::<String, _>("state") == "PENDING" {
            let value = pending_allocations[pending_index];
            pending_index += 1;
            value
        } else {
            row.get("reserved_cost_micros")
        };
        row_reservations.push(reservation);
        claim_reservation = claim_reservation.saturating_add(reservation);
        let affected = sqlx::query(
            "UPDATE knowledge_embedding_migration_chunk_t
                SET state='CLAIMED',claim_token=$3,
                    claim_expires_ts=now()+interval '2 minutes',
                    reserved_cost_micros=$4,attempt_count=attempt_count+1,update_ts=now()
              WHERE migration_id=$1 AND chunk_id=$2
                AND (state='PENDING' OR (state='CLAIMED' AND claim_expires_ts<=now()))",
        )
        .bind(migration_id)
        .bind(chunk_id)
        .bind(claim_token)
        .bind(reservation)
        .execute(&mut *claim_tx)
        .await?;
        if affected.rows_affected() != 1 {
            bail!("KNOWLEDGE_MIGRATION_CHUNK_CLAIM_CONFLICT");
        }
    }
    let reserved = sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET reserved_cost_micros=reserved_cost_micros+$2,update_ts=now()
          WHERE migration_id=$1 AND state='BACKFILLING'
            AND consumed_cost_micros+reserved_cost_micros+$2
                <=accepted_cost_ceiling_micros",
    )
    .bind(migration_id)
    .bind(new_reservation)
    .execute(&mut *claim_tx)
    .await?;
    if reserved.rows_affected() != 1 {
        claim_tx.rollback().await?;
        bail!("KNOWLEDGE_MIGRATION_COST_CEILING_EXCEEDED");
    }
    claim_tx.commit().await?;
    let texts = rows
        .iter()
        .map(|row| row.get::<String, _>("chunk_text"))
        .collect::<Vec<_>>();
    let embedding_target = MigrationEmbeddingTarget {
        alias: migration.get("alias_name"),
        space_id: migration.get("target_space_id"),
        space_revision: migration.get("target_space_revision"),
        dimension: migration.get("target_dimension"),
    };
    let embedded = embed_migration_texts(
        config,
        migration_id,
        &embedding_target,
        &texts,
        &row_reservations,
        claim_reservation,
    )
    .await?;
    let total_tokens = rows
        .iter()
        .map(|row| i64::from(row.get::<i32, _>("token_count")))
        .sum::<i64>();
    let per_chunk_cost = allocate_exact(
        embedded.billed_cost_micros,
        &rows
            .iter()
            .map(|row| i64::from(row.get::<i32, _>("token_count")))
            .collect::<Vec<_>>(),
    );
    let mut tx = pool.begin().await?;
    for ((row, vector), chunk_cost) in rows.iter().zip(embedded.vectors).zip(per_chunk_cost) {
        let chunk_id: Uuid = row.get("chunk_id");
        let input_digest: String = row.get("transformed_input_digest");
        let artifact_id = derived_uuid_text(&format!(
            "migration-artifact:{}:{}:{}:{}",
            config.knowledge_base_id,
            migration.get::<String, _>("target_space_id"),
            migration.get::<i64, _>("target_space_revision"),
            input_digest
        ));
        sqlx::query(
            "INSERT INTO knowledge_embedding_artifact_t(
               embedding_artifact_id,knowledge_base_id,transformed_input_digest,
               space_id,space_revision,dimension,document_input_transform_version,embedding)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8::vector)
             ON CONFLICT(embedding_artifact_id) DO NOTHING",
        )
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(&input_digest)
        .bind(migration.get::<String, _>("target_space_id"))
        .bind(migration.get::<i64, _>("target_space_revision"))
        .bind(migration.get::<i32, _>("target_dimension"))
        .bind(migration.get::<String, _>("document_input_transform_version"))
        .bind(vector_literal(&vector))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_chunk_embedding_t(
               chunk_id,embedding_artifact_id,knowledge_base_id,
               embedding_profile_id,embedding_profile_revision,request_id,reused)
             VALUES($1,$2,$3,$4,$5,$6,FALSE) ON CONFLICT DO NOTHING",
        )
        .bind(chunk_id)
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(migration.get::<Uuid, _>("target_profile_id"))
        .bind(migration.get::<i64, _>("target_profile_revision"))
        .bind(format!("migration:{migration_id}:{chunk_id}"))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_segment_vector_t(
               index_segment_id,chunk_id,embedding_artifact_id,
               knowledge_base_id,projection,dimension)
             VALUES($1,$2,$3,$4,$5::vector,$6)
             ON CONFLICT(index_segment_id,chunk_id) DO UPDATE SET
               embedding_artifact_id=EXCLUDED.embedding_artifact_id,
               projection=EXCLUDED.projection,dimension=EXCLUDED.dimension",
        )
        .bind(segment_id)
        .bind(chunk_id)
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(vector_literal(&vector))
        .bind(migration.get::<i32, _>("target_dimension"))
        .execute(&mut *tx)
        .await?;
        let affected = sqlx::query(
            "UPDATE knowledge_embedding_migration_chunk_t
                SET embedding_artifact_id=$3,state='EMBEDDED',
                    cost_micros=$4,reserved_cost_micros=0,
                    claim_token=NULL,claim_expires_ts=NULL,update_ts=now()
              WHERE migration_id=$1 AND chunk_id=$2 AND state='CLAIMED'
                AND claim_token=$5",
        )
        .bind(migration_id)
        .bind(chunk_id)
        .bind(artifact_id)
        .bind(chunk_cost)
        .bind(claim_token)
        .execute(&mut *tx)
        .await?;
        if affected.rows_affected() != 1 {
            tx.rollback().await?;
            bail!("KNOWLEDGE_MIGRATION_CHUNK_CLAIM_LOST");
        }
    }
    let updated = sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET completed_chunk_count=completed_chunk_count+$2,
                reused_canonical_chunk_count=reused_canonical_chunk_count+$2,
                consumed_token_count=consumed_token_count+$3,
                consumed_cost_micros=consumed_cost_micros+$4,
                reserved_cost_micros=reserved_cost_micros-$5,
                version=version+1,update_ts=now()
          WHERE migration_id=$1 AND state='BACKFILLING'
            AND reserved_cost_micros>=$5
            AND consumed_cost_micros+reserved_cost_micros-$5+$4
                <=accepted_cost_ceiling_micros",
    )
    .bind(migration_id)
    .bind(i64::try_from(rows.len()).unwrap_or(i64::MAX))
    .bind(total_tokens)
    .bind(embedded.billed_cost_micros)
    .bind(claim_reservation)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        tx.rollback().await?;
        bail!("KNOWLEDGE_MIGRATION_COST_CEILING_EXCEEDED");
    }
    sqlx::query(
        "UPDATE knowledge_index_segment_t SET vector_count=(
           SELECT count(*) FROM knowledge_segment_vector_t WHERE index_segment_id=$1)
          WHERE index_segment_id=$1",
    )
    .bind(segment_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let completed =
        migration.get::<i64, _>("completed_chunk_count") + i64::try_from(rows.len()).unwrap_or(0);
    enqueue_migration_job(
        pool,
        config,
        migration_id,
        "MIGRATION_BACKFILL",
        &completed.to_string(),
    )
    .await
}

async fn migration_catchup(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    let mut tx = pool.begin().await?;
    let migration = sqlx::query(
        "SELECT migration.state,migration.candidate_generation_id,
                migration.completed_chunk_count,
                pointer.index_generation_id AS active_generation_id,
                active.final_watermark AS active_watermark
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=migration.knowledge_base_id
            AND pointer.environment=migration.environment
           JOIN knowledge_index_generation_t active
             ON active.index_generation_id=pointer.index_generation_id
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
          FOR UPDATE OF migration,pointer",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    if migration.get::<String, _>("state") != "CATCHING_UP" {
        tx.rollback().await?;
        return Ok(());
    }
    let candidate_generation_id: Uuid = migration.get("candidate_generation_id");
    let active_generation_id: Uuid = migration.get("active_generation_id");
    let segment_id = derived_uuid("migration-base", candidate_generation_id);
    let removed: i64 = sqlx::query_scalar(
        "WITH removed AS (
           DELETE FROM knowledge_embedding_migration_chunk_t item
            WHERE item.migration_id=$1 AND NOT EXISTS (
              SELECT 1 FROM knowledge_resolved_generation_chunk($2) resolved
               WHERE resolved.chunk_id=item.chunk_id)
            RETURNING chunk_id)
         SELECT count(*)::bigint FROM removed",
    )
    .bind(migration_id)
    .bind(active_generation_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM knowledge_segment_vector_t vector
          WHERE vector.index_segment_id=$1 AND NOT EXISTS (
            SELECT 1 FROM knowledge_embedding_migration_chunk_t item
             WHERE item.migration_id=$2 AND item.chunk_id=vector.chunk_id)",
    )
    .bind(segment_id)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM knowledge_segment_chunk_t member
          WHERE member.index_segment_id=$1 AND NOT EXISTS (
            SELECT 1 FROM knowledge_embedding_migration_chunk_t item
             WHERE item.migration_id=$2 AND item.chunk_id=member.chunk_id)",
    )
    .bind(segment_id)
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM knowledge_segment_document_t member
          WHERE member.index_segment_id=$1 AND NOT EXISTS (
            SELECT 1 FROM knowledge_resolved_generation_chunk($2) resolved
             WHERE resolved.document_id=member.document_id)",
    )
    .bind(segment_id)
    .bind(active_generation_id)
    .execute(&mut *tx)
    .await?;
    let added = sqlx::query(
        "WITH inserted AS (
           INSERT INTO knowledge_embedding_migration_chunk_t(
             migration_id,chunk_id,knowledge_base_id,transformed_input_digest,token_count)
           SELECT $1,chunk.chunk_id,$2,
                  encode(digest(chunk.chunk_text || ':' || profile.document_input_transform_version,
                                'sha256'),'hex'),chunk.token_count
             FROM knowledge_resolved_generation_chunk($3) resolved
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=resolved.chunk_id
             JOIN knowledge_embedding_migration_t migration ON migration.migration_id=$1
             JOIN knowledge_embedding_profile_t profile
               ON profile.profile_id=migration.target_profile_id
              AND profile.profile_revision=migration.target_profile_revision
           ON CONFLICT DO NOTHING RETURNING chunk_id)
         SELECT count(*)::bigint AS count FROM inserted",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .bind(active_generation_id)
    .fetch_one(&mut *tx)
    .await?
    .get::<i64, _>("count");
    sqlx::query(
        "INSERT INTO knowledge_segment_document_t(
           index_segment_id,document_id,knowledge_base_id,document_version_id,acl_revision_id)
         SELECT DISTINCT ON(resolved.document_id) $1,resolved.document_id,$2,
                resolved.document_version_id,resolved.acl_revision_id
           FROM knowledge_resolved_generation_chunk($3) resolved
         ON CONFLICT(index_segment_id,document_id) DO UPDATE SET
           document_version_id=EXCLUDED.document_version_id,
           acl_revision_id=EXCLUDED.acl_revision_id",
    )
    .bind(segment_id)
    .bind(config.knowledge_base_id)
    .bind(active_generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_segment_chunk_t(
           index_segment_id,chunk_id,knowledge_base_id,acl_revision_id)
         SELECT $1,resolved.chunk_id,$2,resolved.acl_revision_id
           FROM knowledge_resolved_generation_chunk($3) resolved
         ON CONFLICT(index_segment_id,chunk_id) DO UPDATE SET
           acl_revision_id=EXCLUDED.acl_revision_id",
    )
    .bind(segment_id)
    .bind(config.knowledge_base_id)
    .bind(active_generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_segment_t SET
           document_count=(SELECT count(*) FROM knowledge_segment_document_t
                            WHERE index_segment_id=$1),
           chunk_count=(SELECT count(*) FROM knowledge_segment_chunk_t
                         WHERE index_segment_id=$1),
           vector_count=(SELECT count(*) FROM knowledge_segment_vector_t
                          WHERE index_segment_id=$1),
           acl_count=(SELECT count(DISTINCT acl_revision_id)
                       FROM knowledge_segment_document_t
                       WHERE index_segment_id=$1)
          WHERE index_segment_id=$1",
    )
    .bind(segment_id)
    .execute(&mut *tx)
    .await?;
    let active_watermark = migration
        .get::<Option<i64>, _>("active_watermark")
        .unwrap_or(0);
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM knowledge_embedding_migration_chunk_t
          WHERE migration_id=$1 AND state='PENDING'",
    )
    .bind(migration_id)
    .fetch_one(&mut *tx)
    .await?;
    let completed = migration
        .get::<i64, _>("completed_chunk_count")
        .saturating_sub(removed);
    if remaining > 0 {
        sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='BACKFILLING',completed_chunk_count=$2,
                    reused_canonical_chunk_count=LEAST(reused_canonical_chunk_count,$2),
                    catchup_chunk_count=catchup_chunk_count+$3,
                    version=version+1,update_ts=now()
              WHERE migration_id=$1",
        )
        .bind(migration_id)
        .bind(completed)
        .bind(added)
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='VALIDATING',final_watermark=$2,
                    completed_chunk_count=$3,
                    reused_canonical_chunk_count=LEAST(reused_canonical_chunk_count,$3),
                    version=version+1,update_ts=now()
              WHERE migration_id=$1",
        )
        .bind(migration_id)
        .bind(active_watermark)
        .bind(completed)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE knowledge_index_generation_t
                SET final_watermark=$2,state='VALIDATING'
              WHERE index_generation_id=$1 AND state='CATCHING_UP'",
        )
        .bind(candidate_generation_id)
        .bind(active_watermark)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    if remaining > 0 {
        enqueue_migration_job(
            pool,
            config,
            migration_id,
            "MIGRATION_BACKFILL",
            &format!("catchup-{active_watermark}"),
        )
        .await
    } else {
        enqueue_migration_job(
            pool,
            config,
            migration_id,
            "MIGRATION_VALIDATE",
            &active_watermark.to_string(),
        )
        .await
    }
}

async fn migration_validate(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT migration.candidate_generation_id,migration.final_watermark,
                pointer.index_generation_id AS active_generation_id,
                active.final_watermark AS active_watermark
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=migration.knowledge_base_id
            AND pointer.environment=migration.environment
           JOIN knowledge_index_generation_t active
             ON active.index_generation_id=pointer.index_generation_id
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
            AND migration.state='VALIDATING'
          FOR UPDATE OF migration,pointer",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(());
    };
    let active_watermark = row.get::<Option<i64>, _>("active_watermark").unwrap_or(0);
    if row.get::<Option<i64>, _>("final_watermark") != Some(active_watermark) {
        sqlx::query(
            "UPDATE knowledge_embedding_migration_t
                SET state='CATCHING_UP',version=version+1,update_ts=now()
              WHERE migration_id=$1",
        )
        .bind(migration_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return enqueue_migration_job(
            pool,
            config,
            migration_id,
            "MIGRATION_CATCHUP",
            &format!("refence-{active_watermark}"),
        )
        .await;
    }
    let candidate_generation_id: Uuid = row.get("candidate_generation_id");
    let segment_id = derived_uuid("migration-base", candidate_generation_id);
    let counts = sqlx::query(
        "SELECT (SELECT count(*) FROM knowledge_segment_chunk_t
                  WHERE index_segment_id=$1)::bigint AS chunks,
                (SELECT count(*) FROM knowledge_segment_vector_t
                  WHERE index_segment_id=$1)::bigint AS vectors,
                (SELECT count(*) FROM knowledge_embedding_migration_chunk_t
                  WHERE migration_id=$2 AND state='EMBEDDED')::bigint AS embedded",
    )
    .bind(segment_id)
    .bind(migration_id)
    .fetch_one(&mut *tx)
    .await?;
    let chunks: i64 = counts.get("chunks");
    let vectors: i64 = counts.get("vectors");
    let embedded: i64 = counts.get("embedded");
    if chunks != vectors || chunks != embedded {
        bail!("KNOWLEDGE_MIGRATION_CANDIDATE_INCOMPLETE");
    }
    let (
        evidence_id,
        evaluation_contract_version,
        metrics,
        evidence_digest,
        expires_ts,
        authorized_by,
    ) = if config.migration_deterministic_pilot {
        let metrics = json!({
            "candidateOnly": true,
            "rawCrossSpaceScoresCompared": false,
            "canonicalChunkCount": chunks,
            "vectorCount": vectors,
            "watermark": active_watermark,
            "deterministicPilot": true
        });
        (
            derived_uuid("migration-evaluation", migration_id),
            "phase3-deterministic-v1".to_string(),
            metrics.clone(),
            sha256_hex(serde_json::to_string(&metrics)?.as_bytes()),
            Utc::now() + chrono::Duration::hours(1),
            "light-knowledge-deterministic-pilot".to_string(),
        )
    } else {
        let evidence_id = uuid_value(payload, "evaluationEvidenceId")
            .context("authorized candidate evaluation evidence is required")?;
        if uuid_value(payload, "candidateGenerationId")? != candidate_generation_id {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_GENERATION_MISMATCH");
        }
        if payload
            .get("corpusWatermark")
            .and_then(serde_json::Value::as_i64)
            != Some(active_watermark)
        {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_WATERMARK_MISMATCH");
        }
        let metrics = payload
            .get("metrics")
            .filter(|value| value.is_object())
            .cloned()
            .context("candidate evaluation metrics are required")?;
        if metrics
            .get("rawCrossSpaceScoresCompared")
            .and_then(serde_json::Value::as_bool)
            != Some(false)
            || metrics
                .get("candidateOnly")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_CONTRACT_INVALID");
        }
        if payload.get("passed").and_then(serde_json::Value::as_bool) != Some(true) {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_FAILED");
        }
        let computed_digest = sha256_hex(serde_json::to_string(&metrics)?.as_bytes());
        if payload
            .get("evidenceDigest")
            .and_then(serde_json::Value::as_str)
            != Some(computed_digest.as_str())
        {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_DIGEST_MISMATCH");
        }
        let expires_ts = chrono::DateTime::parse_from_rfc3339(text_value(payload, "expiresAt")?)
            .context("candidate evaluation expiresAt must be RFC3339")?
            .with_timezone(&Utc);
        if expires_ts <= Utc::now() || expires_ts > Utc::now() + chrono::Duration::hours(24) {
            bail!("KNOWLEDGE_MIGRATION_EVALUATION_EXPIRY_INVALID");
        }
        (
            evidence_id,
            "phase3-candidate-evaluation-v1".to_string(),
            metrics,
            computed_digest,
            expires_ts,
            payload
                .get("authorizedBy")
                .and_then(serde_json::Value::as_str)
                .context("authorized candidate evaluation requires authorizedBy")?
                .to_string(),
        )
    };
    let manifest_row = sqlx::query(
        "SELECT generation.snapshot_watermark,generation.parser_contract_digest,
                generation.chunker_contract_digest,generation.lexical_contract_digest,
                generation.citation_contract_digest,generation.space_id,
                generation.space_revision,generation.dimension,
                segment.physical_locator,segment.manifest_digest,
                segment.document_count,segment.chunk_count,segment.vector_count
           FROM knowledge_index_generation_t generation
           JOIN knowledge_index_segment_t segment
             ON segment.index_generation_id=generation.index_generation_id
          WHERE generation.index_generation_id=$1 AND segment.index_segment_id=$2",
    )
    .bind(candidate_generation_id)
    .bind(segment_id)
    .fetch_one(&mut *tx)
    .await?;
    let manifest = BaseManifest {
        generation_id: candidate_generation_id,
        segment_id,
        knowledge_base_id: config.knowledge_base_id,
        snapshot_watermark: u64::try_from(manifest_row.get::<i64, _>("snapshot_watermark"))
            .unwrap_or_default(),
        document_count: usize::try_from(manifest_row.get::<i64, _>("document_count"))
            .unwrap_or_default(),
        chunk_count: usize::try_from(manifest_row.get::<i64, _>("chunk_count")).unwrap_or_default(),
        vector_count: usize::try_from(manifest_row.get::<i64, _>("vector_count"))
            .unwrap_or_default(),
        parser_digest: manifest_row.get("parser_contract_digest"),
        chunker_digest: manifest_row.get("chunker_contract_digest"),
        lexical_digest: manifest_row.get("lexical_contract_digest"),
        citation_digest: manifest_row.get("citation_contract_digest"),
        space_id: manifest_row.get("space_id"),
        space_revision: u64::try_from(manifest_row.get::<i64, _>("space_revision"))
            .unwrap_or_default(),
        dimension: usize::try_from(manifest_row.get::<i32, _>("dimension")).unwrap_or_default(),
        manifest_digest: manifest_row
            .get::<String, _>("manifest_digest")
            .trim()
            .into(),
        segment_kind: "BASE".into(),
    };
    let manifest_path = object_locator_path(
        &config.object_store_root,
        &manifest_row.get::<String, _>("physical_locator"),
    )?;
    if let Some(parent) = manifest_path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_immutable(&manifest_path, &serde_json::to_vec_pretty(&manifest)?)?;
    sqlx::query(
        "INSERT INTO knowledge_migration_evaluation_t(
           evaluation_evidence_id,migration_id,knowledge_base_id,
           candidate_generation_id,evaluation_contract_version,corpus_watermark,
           metrics,evidence_digest,passed,expires_ts,authorized_by)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,TRUE,$9,$10)",
    )
    .bind(evidence_id)
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .bind(candidate_generation_id)
    .bind(&evaluation_contract_version)
    .bind(active_watermark)
    .bind(&metrics)
    .bind(&evidence_digest)
    .bind(expires_ts)
    .bind(authorized_by)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_segment_t SET state='READY'
          WHERE index_segment_id=$1 AND state='BUILDING'",
    )
    .bind(segment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_generation_t
            SET state='READY',evidence=evidence || $2
          WHERE index_generation_id=$1 AND state='VALIDATING'",
    )
    .bind(candidate_generation_id)
    .bind(json!({"migrationEvaluationEvidenceId": evidence_id,
                 "migrationEvaluationDigest": evidence_digest}))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='READY',evaluation_evidence_id=$2,
                evaluation_evidence_digest=$3,version=version+1,update_ts=now()
          WHERE migration_id=$1 AND state='VALIDATING'",
    )
    .bind(migration_id)
    .bind(evidence_id)
    .bind(&evidence_digest)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn migration_promote(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    let expected_pointer_version = payload
        .get("expectedPointerVersion")
        .and_then(serde_json::Value::as_i64)
        .context("migration promotion requires expectedPointerVersion")?;
    let expected_active_generation_id = uuid_value(payload, "expectedActiveGenerationId")?;
    let mut tx = pool.begin().await?;
    let migration = sqlx::query(
        "SELECT migration.*,evaluation.passed,evaluation.expires_ts,
                evaluation.evidence_digest,evaluation.evaluation_contract_version,
                pointer.index_generation_id,
                pointer.pointer_version,active.final_watermark AS active_watermark
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_migration_evaluation_t evaluation
             ON evaluation.evaluation_evidence_id=migration.evaluation_evidence_id
            AND evaluation.migration_id=migration.migration_id
            AND evaluation.candidate_generation_id=migration.candidate_generation_id
            AND evaluation.corpus_watermark=migration.final_watermark
            AND evaluation.evidence_digest=migration.evaluation_evidence_digest
           JOIN knowledge_embedding_profile_runtime_v profile
             ON profile.profile_id=migration.target_profile_id
            AND profile.profile_revision=migration.target_profile_revision
           JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=migration.knowledge_base_id
            AND pointer.environment=migration.environment
           JOIN knowledge_index_generation_t active
             ON active.index_generation_id=pointer.index_generation_id
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
          FOR UPDATE OF migration,pointer",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    if migration.get::<String, _>("state") != "READY"
        || !migration.get::<bool, _>("passed")
        || migration.get::<chrono::DateTime<Utc>, _>("expires_ts") <= Utc::now()
        || migration.get::<Uuid, _>("index_generation_id") != expected_active_generation_id
        || migration.get::<i64, _>("pointer_version") != expected_pointer_version
        || migration.get::<Option<i64>, _>("final_watermark")
            != migration.get::<Option<i64>, _>("active_watermark")
        || (migration.get::<String, _>("evaluation_contract_version") == "phase3-deterministic-v1"
            && !config.migration_deterministic_pilot)
    {
        bail!("KNOWLEDGE_MIGRATION_PROMOTION_FENCE_FAILED");
    }
    let candidate_generation_id: Uuid = migration.get("candidate_generation_id");
    let rollback_window_seconds: i64 = migration.get("rollback_window_seconds");
    let rollback_deadline = Utc::now() + chrono::Duration::seconds(rollback_window_seconds);
    let evidence_digest: String = migration.get::<String, _>("evidence_digest").trim().into();
    let promotion_payload = json!({
        "promotionId": derived_uuid("migration-promotion", migration_id),
        "indexGenerationId": candidate_generation_id,
        "expectedPointerVersion": expected_pointer_version,
        "evidence": {
            "migrationId": migration_id,
            "evaluationEvidenceId": migration.get::<Uuid, _>("evaluation_evidence_id"),
            "finalWatermark": migration.get::<Option<i64>, _>("final_watermark"),
            "candidateOnly": true,
            "rawCrossSpaceScoresCompared": false
        },
        "evidenceDigest": evidence_digest,
        "reason": payload.get("reason").and_then(serde_json::Value::as_str)
            .unwrap_or("Portal-authorized embedding migration"),
        "rollbackDeadline": rollback_deadline.to_rfc3339()
    });
    promote_generation_transaction(&mut tx, config, &promotion_payload).await?;
    sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='SOAKING',promotion_watermark=final_watermark,
                rollback_deadline=$2,authorized_by=$3,version=version+1,update_ts=now()
          WHERE migration_id=$1 AND state='READY'",
    )
    .bind(migration_id)
    .bind(rollback_deadline)
    .bind(
        payload
            .get("authorizedBy")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("portal-operator"),
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_generation_retention_t(
           index_generation_id,knowledge_base_id,retention_state,retain_until_ts,
           migration_reference_count,last_reference_check_ts)
         VALUES($1,$3,'ROLLBACK_ELIGIBLE',$4,1,now()),
               ($2,$3,'ACTIVE',NULL,1,now())
         ON CONFLICT(index_generation_id) DO UPDATE SET
           retention_state=EXCLUDED.retention_state,
           retain_until_ts=EXCLUDED.retain_until_ts,
           migration_reference_count=EXCLUDED.migration_reference_count,
           last_reference_check_ts=now(),update_ts=now()",
    )
    .bind(expected_active_generation_id)
    .bind(candidate_generation_id)
    .bind(config.knowledge_base_id)
    .bind(rollback_deadline)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn migration_rollback(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    let expected_pointer_version = payload
        .get("expectedPointerVersion")
        .and_then(serde_json::Value::as_i64)
        .context("migration rollback requires expectedPointerVersion")?;
    let mut tx = pool.begin().await?;
    let migration = sqlx::query(
        "SELECT migration.*,pointer.index_generation_id,pointer.pointer_version,
                active.final_watermark AS current_source_watermark
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=migration.knowledge_base_id
            AND pointer.environment=migration.environment
           JOIN knowledge_index_generation_t active
             ON active.index_generation_id=pointer.index_generation_id
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
          FOR UPDATE OF migration,pointer",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    if migration.get::<String, _>("state") != "SOAKING"
        || migration
            .get::<Option<chrono::DateTime<Utc>>, _>("rollback_deadline")
            .is_none_or(|deadline| deadline <= Utc::now())
        || migration.get::<Uuid, _>("index_generation_id")
            != migration.get::<Uuid, _>("candidate_generation_id")
        || migration.get::<i64, _>("pointer_version") != expected_pointer_version
        || migration
            .get::<Option<i64>, _>("current_source_watermark")
            .unwrap_or(0)
            > migration.get::<i64, _>("predecessor_reconciled_watermark")
    {
        bail!("KNOWLEDGE_MIGRATION_ROLLBACK_FENCE_FAILED");
    }
    let predecessor: Uuid = migration.get("source_generation_id");
    sqlx::query(
        "UPDATE knowledge_index_generation_t SET state='READY'
          WHERE index_generation_id=$1 AND state='SUPERSEDED'",
    )
    .bind(predecessor)
    .execute(&mut *tx)
    .await?;
    let evidence = json!({
        "migrationId": migration_id,
        "rollback": true,
        "predecessorReconciledWatermark": migration
            .get::<i64, _>("predecessor_reconciled_watermark")
    });
    let rollback_payload = json!({
        "promotionId": derived_uuid("migration-rollback", migration_id),
        "indexGenerationId": predecessor,
        "expectedPointerVersion": expected_pointer_version,
        "evidence": evidence,
        "evidenceDigest": sha256_hex(serde_json::to_string(&evidence)?.as_bytes()),
        "reason": payload.get("reason").and_then(serde_json::Value::as_str)
            .unwrap_or("Portal-authorized embedding migration rollback"),
        "rollbackDeadline": Utc::now().to_rfc3339()
    });
    promote_generation_transaction(&mut tx, config, &rollback_payload).await?;
    sqlx::query(
        "UPDATE knowledge_embedding_migration_t
            SET state='ROLLED_BACK',version=version+1,finished_ts=now(),update_ts=now()
          WHERE migration_id=$1 AND state='SOAKING'",
    )
    .bind(migration_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_generation_retention_t SET
           retention_state=CASE WHEN index_generation_id=$1 THEN 'ACTIVE' ELSE 'RETAINED' END,
           retain_until_ts=CASE WHEN index_generation_id=$2 THEN now()
                                ELSE retain_until_ts END,
           migration_reference_count=0,last_reference_check_ts=now(),update_ts=now()
          WHERE index_generation_id IN ($1,$2)",
    )
    .bind(predecessor)
    .bind(migration.get::<Uuid, _>("candidate_generation_id"))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn migration_retire(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    expire_backup_references(pool, config.knowledge_base_id).await?;
    let mut tx = pool.begin().await?;
    let updated = sqlx::query(
        "UPDATE knowledge_embedding_migration_t migration
            SET state='RETIRED',version=version+1,finished_ts=now(),update_ts=now()
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
            AND ((migration.state='SOAKING' AND migration.rollback_deadline<=now())
                 OR migration.state='ROLLED_BACK')
            AND NOT EXISTS (SELECT 1 FROM knowledge_index_pointer_t pointer
                             WHERE pointer.index_generation_id=CASE
                               WHEN EXISTS (SELECT 1 FROM knowledge_index_pointer_t current
                                             WHERE current.index_generation_id=
                                               migration.source_generation_id)
                                 THEN migration.candidate_generation_id
                               ELSE migration.source_generation_id END)
            AND EXISTS (SELECT 1 FROM knowledge_generation_retention_t retention
                         WHERE retention.index_generation_id=CASE
                           WHEN EXISTS (SELECT 1 FROM knowledge_index_pointer_t current
                                         WHERE current.index_generation_id=
                                           migration.source_generation_id)
                             THEN migration.candidate_generation_id
                           ELSE migration.source_generation_id END
                           AND retention.legal_hold=FALSE
                           AND retention.backup_reference_count=0
                           AND NOT EXISTS (
                             SELECT 1 FROM knowledge_backup_checkpoint_t checkpoint
                              WHERE checkpoint.index_generation_id=retention.index_generation_id
                                AND checkpoint.state IN ('VERIFIED','RESTORED')
                                AND (checkpoint.retain_until_ts IS NULL
                                     OR checkpoint.retain_until_ts>now()))
                           AND (retention.retain_until_ts IS NULL
                                OR retention.retain_until_ts<=now()))
          RETURNING CASE
            WHEN EXISTS (SELECT 1 FROM knowledge_index_pointer_t current
                          WHERE current.index_generation_id=migration.source_generation_id)
              THEN migration.candidate_generation_id
            ELSE migration.source_generation_id END AS retired_generation_id",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(updated) = updated else {
        bail!("KNOWLEDGE_MIGRATION_RETIREMENT_BLOCKED");
    };
    let retired_generation: Uuid = updated.get("retired_generation_id");
    let approved = sqlx::query(
        "UPDATE knowledge_generation_retention_t
            SET retention_state='PURGE_APPROVED',migration_reference_count=0,
                last_reference_check_ts=now(),update_ts=now()
          WHERE index_generation_id=$1 AND legal_hold=FALSE
            AND backup_reference_count=0
            AND (retain_until_ts IS NULL OR retain_until_ts<=now())",
    )
    .bind(retired_generation)
    .execute(&mut *tx)
    .await?;
    if approved.rows_affected() != 1 {
        tx.rollback().await?;
        bail!("KNOWLEDGE_MIGRATION_RETENTION_TRANSITION_CONFLICT");
    }
    tx.commit().await?;
    enqueue_migration_job(
        pool,
        config,
        migration_id,
        "SEGMENT_PURGE",
        &retired_generation.to_string(),
    )
    .await
}

async fn expire_backup_references(pool: &PgPool, knowledge_base_id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE knowledge_backup_checkpoint_t
            SET state='EXPIRED'
          WHERE knowledge_base_id=$1 AND state IN ('VERIFIED','RESTORED')
            AND retain_until_ts IS NOT NULL AND retain_until_ts<=now()",
    )
    .bind(knowledge_base_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_generation_retention_t retention
            SET backup_reference_count=(
                  SELECT count(*) FROM knowledge_backup_checkpoint_t checkpoint
                   WHERE checkpoint.index_generation_id=retention.index_generation_id
                     AND checkpoint.state IN ('VERIFIED','RESTORED')
                     AND (checkpoint.retain_until_ts IS NULL
                          OR checkpoint.retain_until_ts>now())),
                last_reference_check_ts=now(),update_ts=now()
          WHERE retention.knowledge_base_id=$1",
    )
    .bind(knowledge_base_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn create_backup_checkpoint(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let checkpoint_id = payload
        .get("checkpointId")
        .and_then(serde_json::Value::as_str)
        .map(Uuid::parse_str)
        .transpose()?
        .unwrap_or_else(Uuid::now_v7);
    let pointer = sqlx::query(
        "SELECT pointer.index_generation_id,pointer.pointer_version,
                generation.ordered_segment_manifest_digest
           FROM knowledge_index_pointer_t pointer
           JOIN knowledge_index_generation_t generation
             ON generation.index_generation_id=pointer.index_generation_id
          WHERE pointer.knowledge_base_id=$1 AND pointer.environment=$2",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .fetch_one(pool)
    .await?;
    let generation_id: Uuid = pointer.get("index_generation_id");
    let manifest_digest = pointer
        .get::<Option<String>, _>("ordered_segment_manifest_digest")
        .context("active generation has no ordered segment manifest")?;
    let directory = config.object_store_root.join("checkpoints");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{checkpoint_id}.json"));
    let checkpoint = json!({
        "checkpointId": checkpoint_id,
        "knowledgeBaseId": config.knowledge_base_id,
        "environment": config.environment,
        "indexGenerationId": generation_id,
        "pointerVersion": pointer.get::<i64, _>("pointer_version"),
        "objectManifestDigest": manifest_digest
    });
    write_immutable(&path, serde_json::to_vec_pretty(&checkpoint)?.as_slice())?;
    sqlx::query(
        "INSERT INTO knowledge_backup_checkpoint_t(
           checkpoint_id,knowledge_base_id,index_generation_id,environment,
           pointer_version,object_manifest_digest,database_checkpoint_reference,
           encrypted_object_checkpoint_reference,state,retain_until_ts)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,'REQUESTED',now()+interval '30 days')
         ON CONFLICT(checkpoint_id) DO NOTHING",
    )
    .bind(checkpoint_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(&config.environment)
    .bind(pointer.get::<i64, _>("pointer_version"))
    .bind(manifest_digest.trim())
    .bind(format!("external-checkpoint://postgres/{checkpoint_id}"))
    .bind(format!(
        "object://light-knowledge/checkpoints/{checkpoint_id}.json"
    ))
    .execute(pool)
    .await?;
    Ok(())
}

async fn verify_restore_checkpoint(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let checkpoint_id = uuid_value(payload, "checkpointId")?;
    let physical_restore_evidence_digest = text_value(payload, "physicalRestoreEvidenceDigest")?;
    if physical_restore_evidence_digest.len() != 64
        || !physical_restore_evidence_digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("KNOWLEDGE_PHYSICAL_RESTORE_EVIDENCE_INVALID");
    }
    let isolated_environment = text_value(payload, "isolatedEnvironment")?;
    if isolated_environment == config.environment {
        bail!("KNOWLEDGE_PHYSICAL_RESTORE_NOT_ISOLATED");
    }
    let restored_database_reference = text_value(payload, "restoredDatabaseReference")?;
    let restored_object_reference = text_value(payload, "restoredObjectReference")?;
    let row = sqlx::query(
        "SELECT checkpoint.*,generation.ordered_segment_manifest_digest
           FROM knowledge_backup_checkpoint_t checkpoint
           JOIN knowledge_index_generation_t generation
             ON generation.index_generation_id=checkpoint.index_generation_id
          WHERE checkpoint.checkpoint_id=$1 AND checkpoint.knowledge_base_id=$2",
    )
    .bind(checkpoint_id)
    .bind(config.knowledge_base_id)
    .fetch_one(pool)
    .await?;
    let path = config
        .object_store_root
        .join("checkpoints")
        .join(format!("{checkpoint_id}.json"));
    let bytes = fs::read(&path)?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    let expected = row.get::<String, _>("object_manifest_digest");
    let actual = manifest
        .get("objectManifestDigest")
        .and_then(serde_json::Value::as_str)
        .context("checkpoint manifest omits objectManifestDigest")?;
    if actual != expected.trim()
        || row
            .get::<Option<String>, _>("ordered_segment_manifest_digest")
            .is_none_or(|digest| digest.trim() != actual)
    {
        bail!("KNOWLEDGE_RESTORE_CHECKPOINT_MISMATCH");
    }
    if row.get::<String, _>("state") == "RESTORED" {
        return Ok(());
    }
    let updated = sqlx::query(
        "UPDATE knowledge_backup_checkpoint_t
            SET state='RESTORED',verified_ts=now(),
                verification_evidence=jsonb_build_object(
                  'manifestRoundTrip',true,'physicalRestoreExecuted',true,
                  'physicalRestoreEvidenceDigest',$2,
                  'isolatedEnvironment',$3,
                  'restoredDatabaseReference',$4,
                  'restoredObjectReference',$5)
          WHERE checkpoint_id=$1 AND state='REQUESTED'",
    )
    .bind(checkpoint_id)
    .bind(physical_restore_evidence_digest)
    .bind(isolated_environment)
    .bind(restored_database_reference)
    .bind(restored_object_reference)
    .execute(pool)
    .await?;
    if updated.rows_affected() != 1 {
        bail!("KNOWLEDGE_PHYSICAL_RESTORE_STATE_CONFLICT");
    }
    let generation_id = row.get::<Uuid, _>("index_generation_id");
    sqlx::query(
        "INSERT INTO knowledge_generation_retention_t(
           index_generation_id,knowledge_base_id,retention_state,
           backup_reference_count,last_reference_check_ts)
         SELECT $1,$2,'RETAINED',count(*),now()
           FROM knowledge_backup_checkpoint_t
          WHERE index_generation_id=$1 AND state IN ('VERIFIED','RESTORED')
            AND (retain_until_ts IS NULL OR retain_until_ts>now())
         ON CONFLICT(index_generation_id) DO UPDATE SET
           backup_reference_count=EXCLUDED.backup_reference_count,
           last_reference_check_ts=now(),update_ts=now()",
    )
    .bind(generation_id)
    .bind(config.knowledge_base_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn purge_retired_generation(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let migration_id = uuid_value(payload, "migrationId")?;
    expire_backup_references(pool, config.knowledge_base_id).await?;
    let mut tx = pool.begin().await?;
    let generation_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT retention.index_generation_id
           FROM knowledge_embedding_migration_t migration
           JOIN knowledge_generation_retention_t retention
             ON retention.index_generation_id=CASE
               WHEN EXISTS (SELECT 1 FROM knowledge_index_pointer_t current
                             WHERE current.index_generation_id=
                               migration.source_generation_id)
                 THEN migration.candidate_generation_id
               ELSE migration.source_generation_id END
          WHERE migration.migration_id=$1 AND migration.knowledge_base_id=$2
            AND migration.state='RETIRED'
            AND retention.retention_state='PURGE_APPROVED'
            AND retention.legal_hold=FALSE
            AND retention.backup_reference_count=0
            AND retention.migration_reference_count=0
            AND NOT EXISTS (SELECT 1 FROM knowledge_index_pointer_t pointer
                             WHERE pointer.index_generation_id=
                               retention.index_generation_id)
          FOR UPDATE OF retention",
    )
    .bind(migration_id)
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    let locator_rows = sqlx::query(
        "SELECT DISTINCT segment.physical_locator
           FROM knowledge_generation_segment_t member
           JOIN knowledge_index_segment_t segment
             ON segment.index_segment_id=member.index_segment_id
          WHERE member.index_generation_id=$1",
    )
    .bind(generation_id)
    .fetch_all(&mut *tx)
    .await?;
    let manifest_paths = locator_rows
        .iter()
        .map(|row| {
            object_locator_path(
                &config.object_store_root,
                &row.get::<String, _>("physical_locator"),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    sqlx::query(
        "CREATE TEMP TABLE phase3_purge_artifact_candidate
           ON COMMIT DROP AS
         SELECT DISTINCT vector.embedding_artifact_id
           FROM knowledge_generation_segment_t member
           JOIN knowledge_segment_vector_t vector
             ON vector.index_segment_id=member.index_segment_id
          WHERE member.index_generation_id=$1",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    let counts = sqlx::query(
        "SELECT (SELECT count(*) FROM knowledge_generation_segment_t
                  WHERE index_generation_id=$1)::bigint AS segments,
                (SELECT count(*) FROM phase3_purge_artifact_candidate)::bigint
                  AS artifacts",
    )
    .bind(generation_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "WITH segments AS (
           SELECT index_segment_id FROM knowledge_generation_segment_t
            WHERE index_generation_id=$1)
         DELETE FROM knowledge_segment_vector_t
          WHERE index_segment_id IN (SELECT index_segment_id FROM segments)",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    let deleted_migration_chunks = sqlx::query(
        "DELETE FROM knowledge_embedding_migration_chunk_t item
          USING phase3_purge_artifact_candidate candidate
          WHERE item.migration_id=$1
            AND item.embedding_artifact_id=candidate.embedding_artifact_id
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_segment_vector_t remaining
               WHERE remaining.embedding_artifact_id=item.embedding_artifact_id)",
    )
    .bind(migration_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let deleted_mappings = sqlx::query(
        "DELETE FROM knowledge_chunk_embedding_t mapping
          USING phase3_purge_artifact_candidate candidate
          WHERE mapping.embedding_artifact_id=candidate.embedding_artifact_id
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_segment_vector_t remaining
               WHERE remaining.embedding_artifact_id=mapping.embedding_artifact_id)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let deleted_references = sqlx::query(
        "DELETE FROM knowledge_embedding_reference_t reference
          USING phase3_purge_artifact_candidate candidate
          WHERE reference.embedding_artifact_id=candidate.embedding_artifact_id
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_segment_vector_t remaining
               WHERE remaining.embedding_artifact_id=reference.embedding_artifact_id)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    let deleted_artifacts = sqlx::query(
        "DELETE FROM knowledge_embedding_artifact_t artifact
          USING phase3_purge_artifact_candidate candidate
          WHERE artifact.embedding_artifact_id=candidate.embedding_artifact_id
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_segment_vector_t remaining
               WHERE remaining.embedding_artifact_id=artifact.embedding_artifact_id)
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_chunk_embedding_t mapping
               WHERE mapping.embedding_artifact_id=artifact.embedding_artifact_id)",
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "WITH segments AS (
           SELECT index_segment_id FROM knowledge_generation_segment_t
            WHERE index_generation_id=$1)
         DELETE FROM knowledge_segment_operation_t
          WHERE index_segment_id IN (SELECT index_segment_id FROM segments)",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH segments AS (
           SELECT index_segment_id FROM knowledge_generation_segment_t
            WHERE index_generation_id=$1)
         DELETE FROM knowledge_segment_chunk_t
          WHERE index_segment_id IN (SELECT index_segment_id FROM segments)",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH segments AS (
           SELECT index_segment_id FROM knowledge_generation_segment_t
            WHERE index_generation_id=$1)
         DELETE FROM knowledge_segment_document_t
          WHERE index_segment_id IN (SELECT index_segment_id FROM segments)",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_segment_t SET state='PURGED'
          WHERE index_generation_id=$1",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_generation_t SET state='PURGED'
          WHERE index_generation_id=$1 AND state='SUPERSEDED'",
    )
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    let purge_evidence_id = derived_uuid("purge-evidence", migration_id);
    let requested_evidence = json!({
        "generationId": generation_id,
        "segmentsPurged": counts.get::<i64, _>("segments"),
        "candidateArtifactsConsidered": counts.get::<i64, _>("artifacts"),
        "migrationChunkLedgerRowsDeleted": deleted_migration_chunks,
        "embeddingMappingsDeleted": deleted_mappings,
        "embeddingReferenceRowsDeleted": deleted_references,
        "embeddingArtifactsDeleted": deleted_artifacts,
        "manifestObjectsPending": manifest_paths.len(),
        "lastReferenceFencePassed": true
    });
    sqlx::query(
        "INSERT INTO knowledge_purge_evidence_t(
           purge_evidence_id,knowledge_base_id,index_generation_id,purge_scope,
           state,reference_counts,deletion_counts,evidence_digest,authorized_by,finished_ts)
         VALUES($1,$2,$3,'GENERATION','REQUESTED',$4,$5,$6,
                'light-knowledge-worker',NULL)
         ON CONFLICT(purge_evidence_id) DO UPDATE SET
           state='REQUESTED',deletion_counts=EXCLUDED.deletion_counts,
           evidence_digest=EXCLUDED.evidence_digest,finished_ts=NULL",
    )
    .bind(purge_evidence_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(json!({"activePointer": 0, "backup": 0, "migration": 0, "legalHold": 0}))
    .bind(&requested_evidence)
    .bind(sha256_hex(
        serde_json::to_string(&requested_evidence)?.as_bytes(),
    ))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut manifest_objects_deleted = 0usize;
    for path in &manifest_paths {
        match fs::remove_file(path) {
            Ok(()) => manifest_objects_deleted += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                sqlx::query(
                    "UPDATE knowledge_purge_evidence_t
                        SET state='FAILED',finished_ts=now()
                      WHERE purge_evidence_id=$1",
                )
                .bind(purge_evidence_id)
                .execute(pool)
                .await?;
                return Err(error.into());
            }
        }
    }
    let verified_evidence = json!({
        "generationId": generation_id,
        "segmentsPurged": counts.get::<i64, _>("segments"),
        "candidateArtifactsConsidered": counts.get::<i64, _>("artifacts"),
        "migrationChunkLedgerRowsDeleted": deleted_migration_chunks,
        "embeddingMappingsDeleted": deleted_mappings,
        "embeddingReferenceRowsDeleted": deleted_references,
        "embeddingArtifactsDeleted": deleted_artifacts,
        "manifestObjectsDeleted": manifest_objects_deleted,
        "manifestObjectsVerifiedAbsent": manifest_paths.len(),
        "lastReferenceFencePassed": true
    });
    let mut finish = pool.begin().await?;
    sqlx::query(
        "UPDATE knowledge_generation_retention_t
            SET retention_state='PURGED',update_ts=now()
          WHERE index_generation_id=$1 AND retention_state='PURGE_APPROVED'",
    )
    .bind(generation_id)
    .execute(&mut *finish)
    .await?;
    sqlx::query(
        "UPDATE knowledge_purge_evidence_t
            SET state='VERIFIED',deletion_counts=$2,evidence_digest=$3,
                finished_ts=now()
          WHERE purge_evidence_id=$1 AND state='REQUESTED'",
    )
    .bind(purge_evidence_id)
    .bind(&verified_evidence)
    .bind(sha256_hex(
        serde_json::to_string(&verified_evidence)?.as_bytes(),
    ))
    .execute(&mut *finish)
    .await?;
    finish.commit().await?;
    Ok(())
}

async fn schedule_due_acl_reconciliation(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,source_id,job_type,idempotency_key,
           requested_by,payload)
         SELECT gen_random_uuid(),$1,$2,'ACL_RECONCILE',
                'scheduled-acl:'||$2::text||':'||floor(extract(epoch FROM now())/300)::bigint,
                'light-knowledge-scheduler','{}'::jsonb
          FROM knowledge_source_t source
          LEFT JOIN knowledge_source_acl_state_t acl ON acl.source_id=source.source_id
         WHERE source.source_id=$2 AND source.knowledge_base_id=$1
           AND source.status='ACTIVE' AND source.acl_mode='MIRROR_SOURCE_ACL'
           AND (acl.source_id IS NULL OR acl.state<>'COMPLETE'
                OR acl.fresh_until_ts<=now()+interval '5 minutes')
           AND NOT EXISTS (
             SELECT 1 FROM knowledge_job_t active
              WHERE active.knowledge_base_id=$1 AND active.source_id=$2
                AND active.job_type IN ('ACL_RECONCILE','PROVIDER_NOTIFICATION')
                AND active.state IN ('QUEUED','RUNNING'))
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .execute(pool)
    .await?;
    Ok(())
}

const GRAPH_CONTRACT_VERSION: &str = "phase4-structural-v1";

async fn schedule_graph_build(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    if !config.graph_assisted_enabled {
        return Ok(());
    }
    let ready = sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('knowledge_graph_generation_t')::text",
    )
    .fetch_one(pool)
    .await?
    .is_some();
    if !ready {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,job_type,idempotency_key,requested_by,payload)
         SELECT gen_random_uuid(),pointer.knowledge_base_id,'GRAPH_BUILD',
                'graph-build:'||pointer.index_generation_id::text||':phase4-structural-v1',
                'light-knowledge-scheduler',
                jsonb_build_object('indexGenerationId',pointer.index_generation_id)
           FROM knowledge_index_pointer_t pointer
          WHERE pointer.knowledge_base_id=$1 AND pointer.environment=$2
            AND NOT EXISTS(
              SELECT 1 FROM knowledge_source_t source
               WHERE source.knowledge_base_id=pointer.knowledge_base_id
                 AND source.acl_mode<>'UNIFORM_SCOPE')
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_graph_generation_t graph
               WHERE graph.index_generation_id=pointer.index_generation_id
                 AND graph.contract_version='phase4-structural-v1'
                 AND graph.state='READY')
         ON CONFLICT(knowledge_base_id,idempotency_key) DO UPDATE
           SET state='QUEUED',
               next_attempt_ts=now()+make_interval(secs=>LEAST(
                 3600,(60*power(2,LEAST(knowledge_job_t.attempt_count,6)))::int)),
               lease_expires_ts=NULL,update_ts=now()
         WHERE knowledge_job_t.state='FAILED'
           AND knowledge_job_t.attempt_count<5",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .execute(pool)
    .await?;
    Ok(())
}

async fn build_graph_artifact(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    if !config.graph_assisted_enabled {
        bail!("KNOWLEDGE_GRAPH_ASSISTED_DISABLED");
    }
    let generation_id = uuid_value(payload, "indexGenerationId")?;
    let contract_digest = sha256_hex(GRAPH_CONTRACT_VERSION.as_bytes());
    let graph_generation_id = Uuid::now_v7();
    let mut tx = pool.begin().await?;
    let uniform: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM knowledge_index_pointer_t
                        WHERE knowledge_base_id=$1 AND index_generation_id=$2
                          AND environment=$3)
             AND NOT EXISTS(SELECT 1 FROM knowledge_source_t
                             WHERE knowledge_base_id=$1
                               AND acl_mode<>'UNIFORM_SCOPE')",
    )
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(&config.environment)
    .fetch_one(&mut *tx)
    .await?;
    if !uniform {
        bail!("KNOWLEDGE_GRAPH_UNIFORM_SCOPE_REQUIRED");
    }
    sqlx::query(
        "INSERT INTO knowledge_graph_generation_t(
           graph_generation_id,knowledge_base_id,index_generation_id,state,
           contract_version,contract_digest)
         VALUES($1,$2,$3,'BUILDING',$4,$5)
         ON CONFLICT(index_generation_id,contract_digest) DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .bind(&contract_digest)
    .execute(&mut *tx)
    .await?;
    let graph_generation_id: Uuid = sqlx::query_scalar(
        "SELECT graph_generation_id FROM knowledge_graph_generation_t
          WHERE index_generation_id=$1 AND contract_digest=$2 FOR UPDATE",
    )
    .bind(generation_id)
    .bind(&contract_digest)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "WITH resolved AS (
           SELECT member.chunk_id,member.document_id,member.document_version_id,
                  chunk.section_path,chunk.chunk_text
             FROM knowledge_resolved_generation_chunk($3) member
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
         )
         INSERT INTO knowledge_graph_entity_t(
           graph_entity_id,graph_generation_id,knowledge_base_id,entity_type,
           normalized_key,display_name,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,'DOCUMENT','document:'||document_id::text,
                'document:'||document_id::text,'STRUCTURAL',$4
           FROM resolved GROUP BY document_id
         ON CONFLICT(graph_generation_id,entity_type,normalized_key) DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH resolved AS (
           SELECT member.chunk_id,member.document_id,member.document_version_id,
                  chunk.section_path
             FROM knowledge_resolved_generation_chunk($3) member
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
            WHERE jsonb_array_length(chunk.section_path)>0
         ), headings AS (
           SELECT DISTINCT document_id,document_version_id,chunk_id,
                  section_path::text AS path_key,
                  section_path->>-1 AS display_name FROM resolved
         )
         INSERT INTO knowledge_graph_entity_t(
           graph_entity_id,graph_generation_id,knowledge_base_id,entity_type,
           normalized_key,display_name,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,'HEADING',
                'heading:'||document_id::text||':'||path_key,display_name,
                'STRUCTURAL',$4 FROM headings
         ON CONFLICT(graph_generation_id,entity_type,normalized_key) DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH resolved AS (
           SELECT member.chunk_id,member.document_id,member.document_version_id,
                  chunk.chunk_text
             FROM knowledge_resolved_generation_chunk($3) member
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
         ), facts AS (
           SELECT chunk_id,document_id,document_version_id,'REPOSITORY'::text AS entity_type,
                  'repository:'||$2::text AS normalized_key,
                  'Knowledge Base '||$2::text AS display_name,'STRUCTURAL'::text AS origin
             FROM resolved
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'LINK_TARGET',
                  'link:'||lower(match[1]),match[1],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\[[^]]+\\]\\(([^)[:space:]]+)\\)','g') match
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'API_OPERATION',
                  'api:'||upper(match[1])||' '||match[2],
                  upper(match[1])||' '||match[2],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(GET|POST|PUT|PATCH|DELETE)[[:space:]]+(/[A-Za-z0-9_./:{}-]+)','g') match
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'CONFIGURATION_KEY',
                  'config:'||lower(match[1]),match[1],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'`([A-Za-z][A-Za-z0-9_.-]{2,})`','g') match
            WHERE match[1] LIKE '%.%' OR match[1] ~ '[A-Z]'
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'SERVICE',
                  'service:'||lower(match[1]),match[1],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(light-[a-z0-9-]+)\\M','g') match
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'COMPONENT',
                  'component:'||lower(match[1]),match[1],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m([A-Za-z][A-Za-z0-9_-]*(Component|component))\\M','g') match
           UNION ALL
           SELECT chunk_id,document_id,document_version_id,'DESIGN_REFERENCE',
                  'design:'||lower(regexp_replace(match[1],'[[:space:]]+',' ','g')),
                  match[1],'EXPLICIT'
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(Phase[[:space:]]+[0-9]+[a-z]?)\\M','gi') match
         )
         INSERT INTO knowledge_graph_entity_t(
           graph_entity_id,graph_generation_id,knowledge_base_id,entity_type,
           normalized_key,display_name,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,entity_type,normalized_key,
                min(display_name),min(origin),$4 FROM facts
          GROUP BY entity_type,normalized_key
         ON CONFLICT(graph_generation_id,entity_type,normalized_key) DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH resolved AS (
           SELECT member.chunk_id,member.document_id,member.document_version_id,
                  chunk.section_path
             FROM knowledge_resolved_generation_chunk($3) member
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
         ), structural AS (
           SELECT chunk_id,document_version_id,'DOCUMENT'::text AS entity_type,
                  'document:'||document_id::text AS normalized_key FROM resolved
           UNION ALL
           SELECT chunk_id,document_version_id,'HEADING',
                  'heading:'||document_id::text||':'||section_path::text
             FROM resolved WHERE jsonb_array_length(section_path)>0
         )
         INSERT INTO knowledge_graph_entity_contribution_t(
           graph_entity_id,graph_generation_id,knowledge_base_id,chunk_id,document_version_id)
         SELECT entity.graph_entity_id,$1,$2,structural.chunk_id,
                structural.document_version_id
           FROM structural JOIN knowledge_graph_entity_t entity
             ON entity.graph_generation_id=$1
            AND entity.entity_type=structural.entity_type
            AND entity.normalized_key=structural.normalized_key
         ON CONFLICT DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH resolved AS (
           SELECT member.chunk_id,member.document_version_id,chunk.chunk_text
             FROM knowledge_resolved_generation_chunk($3) member
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
         ), facts AS (
           SELECT chunk_id,document_version_id,'REPOSITORY'::text AS entity_type,
                  'repository:'||$2::text AS normalized_key FROM resolved
           UNION ALL
           SELECT chunk_id,document_version_id,'LINK_TARGET','link:'||lower(match[1])
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\[[^]]+\\]\\(([^)[:space:]]+)\\)','g') match
           UNION ALL
           SELECT chunk_id,document_version_id,'API_OPERATION',
                  'api:'||upper(match[1])||' '||match[2]
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(GET|POST|PUT|PATCH|DELETE)[[:space:]]+(/[A-Za-z0-9_./:{}-]+)','g') match
           UNION ALL
           SELECT chunk_id,document_version_id,'CONFIGURATION_KEY',
                  'config:'||lower(match[1])
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'`([A-Za-z][A-Za-z0-9_.-]{2,})`','g') match
            WHERE match[1] LIKE '%.%' OR match[1] ~ '[A-Z]'
           UNION ALL
           SELECT chunk_id,document_version_id,'SERVICE','service:'||lower(match[1])
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(light-[a-z0-9-]+)\\M','g') match
           UNION ALL
           SELECT chunk_id,document_version_id,'COMPONENT','component:'||lower(match[1])
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m([A-Za-z][A-Za-z0-9_-]*(Component|component))\\M','g') match
           UNION ALL
           SELECT chunk_id,document_version_id,'DESIGN_REFERENCE',
                  'design:'||lower(regexp_replace(match[1],'[[:space:]]+',' ','g'))
             FROM resolved CROSS JOIN LATERAL
                  regexp_matches(chunk_text,'\\m(Phase[[:space:]]+[0-9]+[a-z]?)\\M','gi') match
         )
         INSERT INTO knowledge_graph_entity_contribution_t(
           graph_entity_id,graph_generation_id,knowledge_base_id,chunk_id,document_version_id)
         SELECT entity.graph_entity_id,$1,$2,fact.chunk_id,fact.document_version_id
           FROM facts fact JOIN knowledge_graph_entity_t entity
             ON entity.graph_generation_id=$1
            AND entity.entity_type=fact.entity_type
            AND entity.normalized_key=fact.normalized_key
         ON CONFLICT DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH pairs AS (
           SELECT DISTINCT document.graph_entity_id AS subject_id,
                  heading.graph_entity_id AS object_id,
                  contribution.chunk_id,contribution.document_version_id
             FROM knowledge_graph_entity_t heading
             JOIN knowledge_graph_entity_contribution_t contribution
               ON contribution.graph_entity_id=heading.graph_entity_id
             JOIN knowledge_document_version_t version
               ON version.document_version_id=contribution.document_version_id
             JOIN knowledge_graph_entity_t document
               ON document.graph_generation_id=heading.graph_generation_id
              AND document.entity_type='DOCUMENT'
              AND document.normalized_key='document:'||version.document_id::text
            WHERE heading.graph_generation_id=$1 AND heading.entity_type='HEADING'
         )
         INSERT INTO knowledge_graph_relation_t(
           graph_relation_id,graph_generation_id,knowledge_base_id,
           subject_entity_id,object_entity_id,relation_type,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,subject_id,object_id,
                'CONTAINS_HEADING','STRUCTURAL',$3 FROM pairs
         ON CONFLICT(graph_generation_id,subject_entity_id,relation_type,object_entity_id)
         DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH fact AS (
           SELECT entity.graph_entity_id AS object_id,
                  contribution.chunk_id,contribution.document_version_id,
                  CASE entity.entity_type
                    WHEN 'LINK_TARGET' THEN 'LINKS_TO'
                    WHEN 'API_OPERATION' THEN 'REFERENCES_API_OPERATION'
                    WHEN 'CONFIGURATION_KEY' THEN 'REFERENCES_CONFIGURATION_KEY'
                    WHEN 'SERVICE' THEN 'REFERENCES_SERVICE'
                    WHEN 'COMPONENT' THEN 'REFERENCES_COMPONENT'
                    WHEN 'DESIGN_REFERENCE' THEN 'REFERENCES_DESIGN'
                    ELSE 'REFERENCES' END AS relation_type
             FROM knowledge_graph_entity_t entity
             JOIN knowledge_graph_entity_contribution_t contribution
               ON contribution.graph_entity_id=entity.graph_entity_id
            WHERE entity.graph_generation_id=$1
              AND entity.entity_type IN ('LINK_TARGET','API_OPERATION',
                  'CONFIGURATION_KEY','SERVICE','COMPONENT','DESIGN_REFERENCE')
         ), pairs AS (
           SELECT document.graph_entity_id AS subject_id,fact.*
             FROM fact
             JOIN knowledge_document_version_t version
               ON version.document_version_id=fact.document_version_id
             JOIN knowledge_graph_entity_t document
               ON document.graph_generation_id=$1
              AND document.entity_type='DOCUMENT'
              AND document.normalized_key='document:'||version.document_id::text
         )
         INSERT INTO knowledge_graph_relation_t(
           graph_relation_id,graph_generation_id,knowledge_base_id,
           subject_entity_id,object_entity_id,relation_type,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,subject_id,object_id,relation_type,'EXPLICIT',$3
           FROM pairs GROUP BY subject_id,object_id,relation_type
         ON CONFLICT(graph_generation_id,subject_entity_id,relation_type,object_entity_id)
         DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "WITH pairs AS (
           SELECT repository.graph_entity_id AS subject_id,
                  document.graph_entity_id AS object_id,
                  contribution.chunk_id,contribution.document_version_id
             FROM knowledge_graph_entity_t repository
             JOIN knowledge_graph_entity_t document
               ON document.graph_generation_id=repository.graph_generation_id
              AND document.entity_type='DOCUMENT'
             JOIN knowledge_graph_entity_contribution_t contribution
               ON contribution.graph_entity_id=document.graph_entity_id
            WHERE repository.graph_generation_id=$1
              AND repository.entity_type='REPOSITORY'
         )
         INSERT INTO knowledge_graph_relation_t(
           graph_relation_id,graph_generation_id,knowledge_base_id,
           subject_entity_id,object_entity_id,relation_type,origin,contract_version)
         SELECT gen_random_uuid(),$1,$2,subject_id,object_id,
                'CONTAINS_DOCUMENT','STRUCTURAL',$3 FROM pairs
          GROUP BY subject_id,object_id
         ON CONFLICT(graph_generation_id,subject_entity_id,relation_type,object_entity_id)
         DO NOTHING",
    )
    .bind(graph_generation_id)
    .bind(config.knowledge_base_id)
    .bind(GRAPH_CONTRACT_VERSION)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_graph_relation_contribution_t(
           graph_relation_id,graph_generation_id,knowledge_base_id,chunk_id,document_version_id)
         SELECT relation.graph_relation_id,relation.graph_generation_id,
                relation.knowledge_base_id,contribution.chunk_id,
                contribution.document_version_id
           FROM knowledge_graph_relation_t relation
           JOIN knowledge_graph_entity_contribution_t contribution
             ON contribution.graph_entity_id=relation.object_entity_id
            AND contribution.graph_generation_id=relation.graph_generation_id
            AND contribution.knowledge_base_id=relation.knowledge_base_id
          WHERE relation.graph_generation_id=$1
         ON CONFLICT DO NOTHING",
    )
    .bind(graph_generation_id)
    .execute(&mut *tx)
    .await?;
    let (entity_count, relation_count): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM knowledge_graph_entity_t WHERE graph_generation_id=$1),
                (SELECT count(*) FROM knowledge_graph_relation_t WHERE graph_generation_id=$1)",
    )
    .bind(graph_generation_id)
    .fetch_one(&mut *tx)
    .await?;
    let manifest_digest = sha256_hex(
        format!("{GRAPH_CONTRACT_VERSION}\0{generation_id}\0{entity_count}\0{relation_count}")
            .as_bytes(),
    );
    sqlx::query(
        "UPDATE knowledge_graph_generation_t SET state='READY',manifest_digest=$2,
                entity_count=$3,relation_count=$4,completed_ts=now()
          WHERE graph_generation_id=$1",
    )
    .bind(graph_generation_id)
    .bind(manifest_digest)
    .bind(entity_count)
    .bind(relation_count)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn schedule_production_maintenance(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    if !config.production_operations_enabled {
        return Ok(());
    }
    let phase3_ready = sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('knowledge_operational_policy_t')::text",
    )
    .fetch_one(pool)
    .await?
    .is_some();
    if !phase3_ready {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO knowledge_operational_policy_t(knowledge_base_id)
         VALUES($1) ON CONFLICT(knowledge_base_id) DO NOTHING",
    )
    .bind(config.knowledge_base_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,job_type,idempotency_key,requested_by,payload)
         SELECT gen_random_uuid(),pointer.knowledge_base_id,'ANTI_ENTROPY',
                'scheduled-anti-entropy:'||floor(extract(epoch FROM now())/
                  policy.anti_entropy_interval_seconds)::bigint,
                'light-knowledge-scheduler',
                jsonb_build_object('indexGenerationId',pointer.index_generation_id)
           FROM knowledge_index_pointer_t pointer
           JOIN knowledge_operational_policy_t policy
             ON policy.knowledge_base_id=pointer.knowledge_base_id
          WHERE pointer.knowledge_base_id=$1
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_anti_entropy_run_t run
               WHERE run.knowledge_base_id=pointer.knowledge_base_id
                 AND run.started_ts>now()-
                     make_interval(secs=>policy.anti_entropy_interval_seconds::int))
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(config.knowledge_base_id)
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,job_type,idempotency_key,requested_by,payload)
         SELECT gen_random_uuid(),pointer.knowledge_base_id,'BACKUP_CHECKPOINT',
                'scheduled-backup:'||floor(extract(epoch FROM now())/
                  policy.backup_interval_seconds)::bigint,
                'light-knowledge-scheduler',
                jsonb_build_object('checkpointId',gen_random_uuid())
           FROM knowledge_index_pointer_t pointer
           JOIN knowledge_operational_policy_t policy
             ON policy.knowledge_base_id=pointer.knowledge_base_id
          WHERE pointer.knowledge_base_id=$1
            AND NOT EXISTS (
              SELECT 1 FROM knowledge_backup_checkpoint_t checkpoint
               WHERE checkpoint.knowledge_base_id=pointer.knowledge_base_id
                 AND checkpoint.created_ts>now()-
                     make_interval(secs=>policy.backup_interval_seconds::int))
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(config.knowledge_base_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn enqueue_connector_job(
    pool: &PgPool,
    config: &WorkerConfig,
    job_type: &str,
    reason: &str,
) -> Result<()> {
    let bucket = Utc::now().timestamp() / 300;
    sqlx::query(
        "INSERT INTO knowledge_job_t(
           job_id,knowledge_base_id,source_id,job_type,idempotency_key,
           requested_by,payload)
         VALUES($1,$2,$3,$4,$5,'light-knowledge-worker',$6)
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .bind(job_type)
    .bind(format!("{reason}:{}:{bucket}", config.source_id))
    .bind(json!({"reason": reason}))
    .execute(pool)
    .await?;
    Ok(())
}

async fn publish_promotion_acknowledgements(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    sqlx::query(
        "UPDATE knowledge_promotion_outbox_t o SET
           state='ACKNOWLEDGED',acknowledged_ts=a.acknowledged_ts
          FROM knowledge_promotion_ack_t a
         WHERE a.promotion_id=o.promotion_id AND o.state<>'ACKNOWLEDGED'",
    )
    .execute(pool)
    .await?;
    let (Some(endpoint), Some(token_file)) = (
        config.portal_command_url.as_deref(),
        config.portal_authorization_file.as_ref(),
    ) else {
        return Ok(());
    };
    let row = sqlx::query(
        "SELECT o.promotion_id,o.knowledge_base_id,o.environment,
                o.index_generation_id,o.pointer_version,o.evidence_digest,
                b.host_id,b.version AS knowledge_base_version
           FROM knowledge_promotion_outbox_t o
           JOIN knowledge_base_t b ON b.knowledge_base_id=o.knowledge_base_id
          WHERE o.state IN ('PENDING','FAILED')
            AND (o.next_attempt_ts IS NULL OR o.next_attempt_ts<=now())
          ORDER BY o.created_ts LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else { return Ok(()) };
    let promotion_id: Uuid = row.get("promotion_id");
    let evidence_digest: String = row.get::<String, _>("evidence_digest").trim().into();
    let owner_host: Option<Uuid> = row.get("host_id");
    let data = json!({
        "promotionId": promotion_id,
        "knowledgeBaseId": row.get::<Uuid, _>("knowledge_base_id"),
        "environment": row.get::<String, _>("environment"),
        "indexGenerationId": row.get::<Uuid, _>("index_generation_id"),
        "pointerVersion": row.get::<i64, _>("pointer_version"),
        "evidenceDigest": evidence_digest,
        "aggregateVersion": row.get::<i64, _>("knowledge_base_version"),
        "scope": if owner_host.is_some() { "TENANT" } else { "GLOBAL" }
    });
    let token = fs::read_to_string(token_file)?;
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .build()?
        .post(endpoint)
        .bearer_auth(token.trim())
        .json(&json!({
            "host": "lightapi.net",
            "service": "genai",
            "action": "acknowledgeKnowledgeBaseIndexGenerationPromotion",
            "version": "0.1.0",
            "data": data
        }))
        .send()
        .await;
    let (state, next_attempt) = match response {
        Ok(response) if response.status().is_success() => ("SENT", None),
        Ok(response) => {
            tracing::warn!(promotion_id=%promotion_id, status=%response.status(),
                "Portal rejected Knowledge promotion acknowledgement");
            ("FAILED", Some("now()+interval '10 seconds'"))
        }
        Err(error) => {
            tracing::warn!(promotion_id=%promotion_id, %error,
                "Knowledge promotion acknowledgement remains durable");
            ("FAILED", Some("now()+interval '10 seconds'"))
        }
    };
    let next_attempt_sql = next_attempt.unwrap_or("NULL");
    let statement = format!(
        "UPDATE knowledge_promotion_outbox_t SET state=$2,
         attempt_count=attempt_count+1,next_attempt_ts={next_attempt_sql}
         WHERE promotion_id=$1"
    );
    sqlx::query(&statement)
        .bind(promotion_id)
        .bind(state)
        .execute(pool)
        .await?;
    Ok(())
}

async fn promote_generation(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    promote_generation_transaction(&mut tx, config, payload).await?;
    tx.commit().await?;
    Ok(())
}

async fn promote_generation_transaction(
    tx: &mut Transaction<'_, Postgres>,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let promotion_id = uuid_value(payload, "promotionId")?;
    let generation_id = uuid_value(payload, "indexGenerationId")?;
    let expected_pointer_version = payload
        .get("expectedPointerVersion")
        .and_then(serde_json::Value::as_i64)
        .context("promotion payload requires expectedPointerVersion")?;
    let evidence = payload
        .get("evidence")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let evidence_digest = payload
        .get("evidenceDigest")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() == 64)
        .context("promotion payload requires a SHA-256 evidenceDigest")?;
    sqlx::query_scalar::<_, i64>(
        "SELECT promote_knowledge_base_generation(
           $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,
           COALESCE($11::timestamptz,now()+interval '24 hours'))",
    )
    .bind(promotion_id)
    .bind(Uuid::now_v7())
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .bind(generation_id)
    .bind(expected_pointer_version)
    .bind("light-knowledge-worker")
    .bind(
        payload
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Portal-authorized Phase 1a promotion"),
    )
    .bind(evidence)
    .bind(evidence_digest)
    .bind(
        payload
            .get("rollbackDeadline")
            .and_then(serde_json::Value::as_str),
    )
    .fetch_one(&mut **tx)
    .await?;
    Ok(())
}

async fn build(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    if let Some(job_id) = config.current_job_id {
        sqlx::query("UPDATE knowledge_sync_run_t SET phase='FETCHING_SOURCES',update_ts=now() WHERE job_id=$1 AND state='RUNNING'")
            .bind(job_id).execute(pool).await?;
    }
    if config.resolved_sources.is_empty() {
        bail!("KNOWLEDGE_JOB_RESOLVED_SOURCE_SNAPSHOT_REQUIRED");
    }
    let mut build_config = config.clone();
    apply_aggregate_source_limits(&mut build_config, config.resolved_sources.clone())?;
    let mut documents = Vec::new();
    let mut snapshots = Vec::new();
    let (preserved_generation_id, preserved_corpus) = if config
        .resolved_sources
        .iter()
        .any(|source| source.source_type != "GIT_MARKDOWN")
    {
        let mut corpus_config = config.clone();
        corpus_config.source_id = Uuid::nil();
        let generation_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT index_generation_id FROM knowledge_index_pointer_t
              WHERE knowledge_base_id=$1 AND environment=$2",
        )
        .bind(config.knowledge_base_id)
        .bind(&config.environment)
        .fetch_optional(pool)
        .await?;
        let corpus = if let Some(generation_id) = generation_id {
            load_generation_corpus_state(pool, &corpus_config, generation_id).await?
        } else {
            Vec::new()
        };
        (generation_id, corpus)
    } else {
        (None, Vec::new())
    };
    for source in &config.resolved_sources {
        let source_documents = if source.source_type == "GIT_MARKDOWN" {
            let mut source_config = config.clone();
            apply_resolved_source(&mut source_config, source);
            source_config.resolved_sources = vec![source.clone()];
            let checkout = prepare_checkout(&source_config).await?;
            let mut source_documents = ingest_markdown_repository(checkout.path(), &source.limits)?;
            normalize_source_documents(
                &mut source_documents,
                source.source_id,
                &source.approved_repository_uri,
                &source.immutable_commit,
                &source.source_include_prefixes,
                &source.source_exclude_prefixes,
            );
            source_documents
        } else {
            preserved_corpus
                .iter()
                .filter(|document| {
                    source_id_from_object_id(&document.source_object_id) == Some(source.source_id)
                })
                .map(|document| DocumentInput {
                    source_object_id: document.source_object_id.clone(),
                    canonical_uri: document.canonical_uri.clone(),
                    source_version: document.source_version.clone(),
                    markdown: document.markdown.clone(),
                })
                .collect()
        };
        let source_stored_bytes = source_documents.iter().try_fold(0_u64, |total, document| {
            total
                .checked_add(u64::try_from(document.markdown.len()).unwrap_or(u64::MAX))
                .context("source stored byte count overflow")
        })?;
        if source_documents.len() > source.limits.maximum_documents {
            return Err(KnowledgeError::SourceLimit("maximum_documents").into());
        }
        if source_stored_bytes > source.limits.maximum_source_bytes {
            return Err(KnowledgeError::SourceLimit("maximum_source_bytes").into());
        }
        if source_stored_bytes > source.maximum_stored_bytes {
            bail!("KNOWLEDGE_INGESTION_MAX_STORED_BYTES_EXCEEDED");
        }
        snapshots.push(json!({
            "sourceId": source.source_id,
            "sourceType": source.source_type,
            "repositoryUri": (!source.approved_repository_uri.is_empty())
                .then_some(source.approved_repository_uri.as_str()),
            "immutableCommit": (!source.immutable_commit.is_empty())
                .then_some(source.immutable_commit.as_str()),
            "ingestionPolicyId": source.ingestion_policy_id,
            "ingestionPolicyVersion": source.ingestion_policy_version,
            "preservedFromGenerationId": (source.source_type != "GIT_MARKDOWN")
                .then_some(preserved_generation_id).flatten(),
            "documentCount": source_documents.len()
        }));
        documents.extend(source_documents);
    }
    let normalized_bytes = documents.iter().try_fold(0_u64, |total, document| {
        total
            .checked_add(u64::try_from(document.markdown.len()).unwrap_or(u64::MAX))
            .context("normalized source byte count overflow")
    })?;
    if normalized_bytes > build_config.maximum_stored_bytes {
        bail!("KNOWLEDGE_INGESTION_MAX_STORED_BYTES_EXCEEDED");
    }
    build_config.source_snapshot = full_base_source_snapshot(snapshots.clone());
    let source_snapshot_digest = sha256_hex(&serde_json::to_vec(&build_config.source_snapshot)?);
    let mut generation = build_full_base_with_context(
        build_config.knowledge_base_id,
        build_config.snapshot_watermark,
        &documents,
        &ProcessingContract::default(),
        &build_config.limits,
        &source_snapshot_digest,
    )?;
    enforce_source_chunk_limits(&generation, &build_config.resolved_sources)?;
    let observed_embedding_tokens = generation
        .chunks
        .iter()
        .map(|chunk| chunk.token_count)
        .sum::<usize>();
    if let Some(job_id) = config.current_job_id {
        sqlx::query("UPDATE knowledge_sync_run_t SET phase='CHUNKING',document_count=$3,chunk_count=$4,source_bytes=$5,embedding_tokens=$6,progress=progress || jsonb_build_object('sourceCount',$2,'documentCount',$3,'chunkCount',$4,'sourceBytes',$5,'embeddingTokens',$6),update_ts=now() WHERE job_id=$1 AND state='RUNNING'")
            .bind(job_id)
            .bind(snapshots.len() as i64)
            .bind(as_i64(documents.len()))
            .bind(as_i64(generation.chunks.len()))
            .bind(i64::try_from(normalized_bytes)?)
            .bind(as_i64(observed_embedding_tokens))
            .execute(pool).await?;
    }
    if let Some(job_id) = config.current_job_id {
        sqlx::query("UPDATE knowledge_sync_run_t SET phase='EMBEDDING',progress=progress || jsonb_build_object('chunkCount',$2),update_ts=now() WHERE job_id=$1 AND state='RUNNING'")
            .bind(job_id).bind(generation.manifest.chunk_count as i64)
            .execute(pool).await?;
    }
    apply_configured_embeddings(&build_config, &mut generation).await?;
    let objects = write_objects(&build_config.object_store_root, &generation, &documents)?;
    let source_manifest_path = build_config
        .object_store_root
        .join("generations")
        .join(generation.manifest.generation_id.to_string())
        .join("sources.json");
    write_immutable(
        &source_manifest_path,
        &serde_json::to_vec(&build_config.source_snapshot)?,
    )?;
    persist_full_base(pool, &build_config, &generation, &objects).await?;
    println!("{}", serde_json::to_string_pretty(&generation.manifest)?);
    Ok(())
}

fn enforce_source_chunk_limits(
    generation: &FullBaseGeneration,
    sources: &[ResolvedSourceConfig],
) -> Result<()> {
    let mut observed = HashMap::<Uuid, (usize, usize)>::new();
    for chunk in &generation.chunks {
        let source_id = source_id_from_object_id(&chunk.source_object_id)
            .context("KNOWLEDGE_DOCUMENT_SOURCE_ID_UNRESOLVED")?;
        let entry = observed.entry(source_id).or_default();
        entry.0 = entry.0.saturating_add(1);
        entry.1 = entry.1.saturating_add(chunk.token_count);
    }
    for source in sources {
        let (chunks, embedding_tokens) =
            observed.get(&source.source_id).copied().unwrap_or_default();
        if chunks > source.limits.maximum_chunks {
            return Err(KnowledgeError::SourceLimit("maximum_chunks").into());
        }
        if embedding_tokens > source.limits.maximum_embedding_tokens {
            return Err(KnowledgeError::SourceLimit("maximum_embedding_tokens").into());
        }
    }
    Ok(())
}

async fn compact_generation(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    let source = sqlx::query(
        "SELECT p.index_generation_id,p.pointer_version,g.final_watermark,
                g.ordered_segment_manifest_digest,
                count(gs.index_segment_id) AS segment_count
           FROM knowledge_index_pointer_t p
           JOIN knowledge_index_generation_t g
             ON g.index_generation_id=p.index_generation_id
           JOIN knowledge_generation_segment_t gs
             ON gs.index_generation_id=g.index_generation_id
          WHERE p.knowledge_base_id=$1 AND p.environment=$2
          GROUP BY p.index_generation_id,p.pointer_version,g.final_watermark,
                   g.ordered_segment_manifest_digest",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .fetch_one(pool)
    .await?;
    let segment_count: i64 = source.get("segment_count");
    if segment_count <= 1 {
        return Ok(());
    }
    let source_generation_id: Uuid = source.get("index_generation_id");
    let pointer_version: i64 = source.get("pointer_version");
    let watermark: i64 = source.get("final_watermark");
    let source_manifest_digest = source
        .get::<Option<String>, _>("ordered_segment_manifest_digest")
        .unwrap_or_else(|| "0".repeat(64));
    let states = load_generation_corpus_state(pool, config, source_generation_id).await?;
    let documents = states
        .iter()
        .map(|document| DocumentInput {
            source_object_id: document.source_object_id.clone(),
            canonical_uri: document.canonical_uri.clone(),
            source_version: document.source_version.clone(),
            markdown: document.markdown.clone(),
        })
        .collect::<Vec<_>>();
    let source_corpus_digest = corpus_digest(&states);
    let mut generation = build_full_base(
        config.knowledge_base_id,
        u64::try_from(watermark).unwrap_or_default(),
        &documents,
        &ProcessingContract::default(),
        &config.limits,
    )?;
    apply_configured_embeddings(config, &mut generation).await?;
    generation.manifest.segment_kind = "BASE+DELTA".into();
    generation = compact_resolved_generation(&generation)?;
    let objects = write_objects(&config.object_store_root, &generation, &documents)?;
    let compaction_run_id = derived_uuid_text(&format!(
        "compaction:{source_generation_id}:{}",
        generation.manifest.generation_id
    ));
    sqlx::query(
        "INSERT INTO knowledge_compaction_run_t(
           compaction_run_id,knowledge_base_id,source_generation_id,
           canonical_watermark,state,source_manifest_digest)
         VALUES($1,$2,$3,$4,'RUNNING',$5)
         ON CONFLICT(compaction_run_id) DO UPDATE SET state='RUNNING',finished_ts=NULL",
    )
    .bind(compaction_run_id)
    .bind(config.knowledge_base_id)
    .bind(source_generation_id)
    .bind(watermark)
    .bind(&source_manifest_digest)
    .execute(pool)
    .await?;
    if let Err(error) = persist_full_base(pool, config, &generation, &objects).await {
        sqlx::query(
            "UPDATE knowledge_compaction_run_t SET state='FAILED',finished_ts=now(),
                    verification_evidence=jsonb_build_object('error',$2)
              WHERE compaction_run_id=$1",
        )
        .bind(compaction_run_id)
        .bind(error.to_string())
        .execute(pool)
        .await?;
        return Err(error);
    }
    let candidate_states =
        load_generation_corpus_state(pool, config, generation.manifest.generation_id).await?;
    let candidate_corpus_digest = corpus_digest(&candidate_states);
    if candidate_corpus_digest != source_corpus_digest {
        sqlx::query(
            "UPDATE knowledge_compaction_run_t SET candidate_generation_id=$2,
                    resolved_corpus_digest=$3,state='FAILED',finished_ts=now(),
                    verification_evidence=jsonb_build_object(
                      'sourceCorpusDigest',$3,'candidateCorpusDigest',$4,
                      'sourceSegmentCount',$5,'candidateSegmentCount',1,
                      'corpusEquivalent',false)
              WHERE compaction_run_id=$1",
        )
        .bind(compaction_run_id)
        .bind(generation.manifest.generation_id)
        .bind(&source_corpus_digest)
        .bind(&candidate_corpus_digest)
        .bind(segment_count)
        .execute(pool)
        .await?;
        bail!("KNOWLEDGE_COMPACTION_CORPUS_MISMATCH");
    }
    sqlx::query(
        "UPDATE knowledge_compaction_run_t SET candidate_generation_id=$2,
                resolved_corpus_digest=$3,state='VERIFIED',
                verification_evidence=jsonb_build_object(
                  'sourceCorpusDigest',$3,'candidateCorpusDigest',$3,
                  'sourceSegmentCount',$4,'candidateSegmentCount',1,
                  'corpusEquivalent',true)
          WHERE compaction_run_id=$1",
    )
    .bind(compaction_run_id)
    .bind(generation.manifest.generation_id)
    .bind(&source_corpus_digest)
    .bind(segment_count)
    .execute(pool)
    .await?;
    let evidence = json!({
        "phase": "1b",
        "compactionRunId": compaction_run_id,
        "sourceGenerationId": source_generation_id,
        "resolvedCorpusDigest": source_corpus_digest,
        "equivalent": true
    });
    promote_generation(
        pool,
        config,
        &json!({
            "promotionId": derived_uuid("compaction-promotion", generation.manifest.generation_id),
            "indexGenerationId": generation.manifest.generation_id,
            "expectedPointerVersion": pointer_version,
            "evidence": evidence,
            "evidenceDigest": sha256_hex(serde_json::to_string(&evidence)?.as_bytes()),
            "reason": "verified Phase 1b BASE compaction"
        }),
    )
    .await?;
    sqlx::query(
        "UPDATE knowledge_compaction_run_t SET state='PROMOTED',finished_ts=now()
          WHERE compaction_run_id=$1 AND state='VERIFIED'",
    )
    .bind(compaction_run_id)
    .execute(pool)
    .await?;
    Ok(())
}

fn corpus_digest(states: &[CorpusDocumentState]) -> String {
    let mut identities = states
        .iter()
        .map(|document| {
            format!(
                "{}:{}:{}:{}:{}",
                document.source_object_id,
                document.content_digest,
                document.metadata_digest,
                document.acl_digest,
                document.source_version
            )
        })
        .collect::<Vec<_>>();
    identities.sort();
    sha256_hex((identities.join("\n") + "\n").as_bytes())
}

async fn process_upload(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let upload_id = uuid_value(payload, "uploadId")?;
    let row = sqlx::query(
        "SELECT source_object_id,original_filename,media_type,staged_locator,
                staged_digest,lifecycle_state
           FROM knowledge_upload_t
          WHERE upload_id=$1 AND knowledge_base_id=$2 AND source_id=$3",
    )
    .bind(upload_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .fetch_one(pool)
    .await?;
    let state: String = row.get("lifecycle_state");
    if state == "PROMOTED" {
        return Ok(());
    }
    if state != "VERIFIED" {
        bail!("upload is not verified for indexing");
    }
    let locator: String = row.get("staged_locator");
    if fs::metadata(&locator)?.len() > config.limits.maximum_source_bytes {
        return Err(KnowledgeError::SourceLimit("maximum_source_bytes").into());
    }
    let bytes = fs::read(&locator)?;
    let expected_digest: String = row.get::<String, _>("staged_digest").trim().into();
    if sha256_hex(&bytes) != expected_digest {
        bail!("verified upload digest changed before indexing");
    }
    let markdown = String::from_utf8(bytes).context("verified upload is not UTF-8 text")?;
    let source_object_id: String = row.get("source_object_id");
    let input = DocumentInput {
        source_object_id: format!("{}/{source_object_id}", config.source_id),
        canonical_uri: format!(
            "upload://{upload_id}/{}",
            row.get::<String, _>("original_filename")
        ),
        source_version: expected_digest.clone(),
        markdown,
    };
    let previous = load_corpus_state(pool, config).await?;
    let mut current = previous.clone();
    current.retain(|document| document.source_object_id != input.source_object_id);
    current.push(CorpusDocumentState::from(input));
    incremental_from_states(pool, config, &previous, &current, Some(upload_id), false).await?;
    sqlx::query(
        "UPDATE knowledge_upload_t SET lifecycle_state='PROMOTED',promoted_ts=now()
          WHERE upload_id=$1 AND lifecycle_state='VERIFIED'",
    )
    .bind(upload_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn incremental_build(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    let checkout = prepare_checkout(config).await?;
    let mut documents = ingest_markdown_repository(checkout.path(), &config.limits)?;
    normalize_source_documents(
        &mut documents,
        config.source_id,
        &config.approved_repository_uri,
        &config.immutable_commit,
        &config.source_include_prefixes,
        &config.source_exclude_prefixes,
    );
    let current = documents
        .into_iter()
        .map(CorpusDocumentState::from)
        .collect::<Vec<_>>();
    let previous = load_corpus_state(pool, config).await?;
    incremental_from_states(pool, config, &previous, &current, None, false)
        .await
        .map(|_| ())
}

async fn connector_build(pool: &PgPool, config: &WorkerConfig, apply_content: bool) -> Result<()> {
    let approved_origin = config
        .enterprise_connector_approved_origin
        .as_deref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_ORIGIN_REQUIRED")?;
    let page = load_connector_page(pool, config, approved_origin).await?;
    let page = page.validate(approved_origin)?;
    let schema_ready: bool = sqlx::query_scalar(
        "SELECT to_regclass('knowledge_acl_reconciliation_t') IS NOT NULL
             AND to_regclass('knowledge_source_acl_state_t') IS NOT NULL
             AND to_regclass('knowledge_acl_subject_t') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?;
    if !schema_ready {
        bail!("KNOWLEDGE_PHASE2_SCHEMA_REQUIRED");
    }

    let reconciliation_id = Uuid::now_v7();
    let provider = connector_kind_name(page.page().provider);
    let reconciliation_mode = connector_sync_mode_name(page.page().sync_mode);
    let input_cursor_digest = page
        .page()
        .requested_cursor
        .as_deref()
        .map(|cursor| sha256_hex(cursor.as_bytes()));
    let output_cursor_digest = sha256_hex(page.committed_cursor().as_bytes());
    let initial_evidence =
        sha256_hex(format!("{provider}:{input_cursor_digest:?}:{output_cursor_digest}").as_bytes());
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO knowledge_acl_reconciliation_t(
           reconciliation_id,knowledge_base_id,source_id,provider,
           reconciliation_mode,state,input_cursor_digest,output_cursor_digest,
           evidence_digest) VALUES($1,$2,$3,$4,$5,'RUNNING',$6,$7,$8)",
    )
    .bind(reconciliation_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .bind(provider)
    .bind(reconciliation_mode)
    .bind(&input_cursor_digest)
    .bind(&output_cursor_digest)
    .bind(&initial_evidence)
    .execute(&mut *tx)
    .await?;
    if page.page().sync_mode == ConnectorSyncMode::Full {
        sqlx::query(
            "INSERT INTO knowledge_source_acl_state_t(
           source_id,knowledge_base_id,reconciliation_id,state)
         VALUES($1,$2,$3,'RECONCILING')
         ON CONFLICT(source_id) DO UPDATE SET reconciliation_id=EXCLUDED.reconciliation_id,
           state='RECONCILING',observed_ts=NULL,fresh_until_ts=NULL,update_ts=now()",
        )
        .bind(config.source_id)
        .bind(config.knowledge_base_id)
        .bind(reconciliation_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    if !page.page().reconciliation_complete {
        sqlx::query(
            "UPDATE knowledge_acl_reconciliation_t SET state='INCOMPLETE',
               finished_ts=now(),error_code='KNOWLEDGE_CONNECTOR_PAGE_INCOMPLETE'
             WHERE reconciliation_id=$1",
        )
        .bind(reconciliation_id)
        .execute(pool)
        .await?;
        if page.page().sync_mode == ConnectorSyncMode::Full {
            sqlx::query(
                "UPDATE knowledge_source_acl_state_t SET state='INCOMPLETE',
                   update_ts=now() WHERE source_id=$1",
            )
            .bind(config.source_id)
            .execute(pool)
            .await?;
        }
        bail!("KNOWLEDGE_CONNECTOR_PAGE_INCOMPLETE");
    }

    let objects = stable_objects(&page);
    let mut previous = load_corpus_state(pool, config).await?;
    let previous_acl = load_current_acl_digests(pool, config).await?;
    let page_documents = objects
        .values()
        .filter(|object| !object.deleted && !object.markdown.trim().is_empty())
        .map(|object| {
            let acl = normalize_permission(&object.permission, page.page().observed_at);
            let mut document = CorpusDocumentState::from(DocumentInput {
                source_object_id: object.external_id.clone(),
                canonical_uri: object.canonical_uri.clone(),
                source_version: object.provider_version.clone(),
                markdown: object.markdown.clone(),
            });
            document.acl_digest = permission_digest(&acl);
            document
        })
        .collect::<Vec<_>>();
    let current =
        resolve_connector_corpus(page.page().sync_mode, &previous, &objects, page_documents);
    // ACL revisions advance independently from content generations. Suppress
    // ACL-only segment creation; the immutable revision is persisted below.
    for document in &mut previous {
        if let Some(current_document) = current
            .iter()
            .find(|candidate| candidate.source_object_id == document.source_object_id)
        {
            document.acl_digest = current_document.acl_digest.clone();
        }
    }
    let content_changes = classify_corpus_changes(config.knowledge_base_id, &previous, &current);
    let content_consistent = apply_content || content_changes.is_empty();
    let pending_promotion = if apply_content {
        incremental_from_states(pool, config, &previous, &current, None, true).await
    } else {
        Ok(None)
    };
    let pending_promotion = match pending_promotion {
        Ok(pending) => pending,
        Err(error) => {
            sqlx::query(
                "UPDATE knowledge_acl_reconciliation_t SET state='FAILED',
               finished_ts=now(),error_code='KNOWLEDGE_CONNECTOR_CONTENT_APPLY_FAILED'
             WHERE reconciliation_id=$1",
            )
            .bind(reconciliation_id)
            .execute(pool)
            .await?;
            if page.page().sync_mode == ConnectorSyncMode::Full {
                sqlx::query(
                    "UPDATE knowledge_source_acl_state_t SET state='INCOMPLETE',
                   update_ts=now() WHERE source_id=$1",
                )
                .bind(config.source_id)
                .execute(pool)
                .await?;
            }
            return Err(error);
        }
    };
    persist_connector_reconciliation(
        pool,
        config,
        reconciliation_id,
        provider,
        &page,
        &objects,
        previous_acl,
        input_cursor_digest.as_deref(),
        &output_cursor_digest,
        content_consistent,
        pending_promotion.as_ref(),
    )
    .await?;
    if !content_consistent {
        enqueue_connector_job(pool, config, "CONNECTOR_SYNC", "acl-found-content-change").await?;
        bail!("KNOWLEDGE_ACL_RECONCILIATION_REQUIRES_CONTENT_SYNC");
    }
    sqlx::query(
        "UPDATE knowledge_connector_notification_t
            SET state='APPLIED',applied_ts=now()
          WHERE source_id=$1 AND state='RECEIVED'",
    )
    .bind(config.source_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn test_connector_connection(config: &WorkerConfig) -> Result<()> {
    let approved_origin = config
        .enterprise_connector_approved_origin
        .as_deref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_ORIGIN_REQUIRED")?;
    if let Some(fixture) = &config.enterprise_connector_fixture_file {
        let page: ConnectorPage = serde_json::from_slice(&fs::read(fixture)?)?;
        page.validate(approved_origin)?;
        return Ok(());
    }
    let endpoint = config
        .enterprise_connector_page_url
        .as_deref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_NOT_CONFIGURED")?;
    let token_file = config
        .enterprise_connector_authorization_file
        .as_ref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_AUTHORIZATION_REQUIRED")?;
    let token = fs::read_to_string(token_file)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut response = client
        .post(endpoint)
        .bearer_auth(token.trim())
        .header("x-knowledge-base-id", config.knowledge_base_id.to_string())
        .header("x-knowledge-source-id", config.source_id.to_string())
        .json(&json!({"cursor": null, "fullPermissionReconciliation": false}))
        .send()
        .await?;
    if !response.status().is_success() {
        bail!(
            "KNOWLEDGE_CONNECTOR_HTTP_STATUS:{}",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if u64::try_from(bytes.len().saturating_add(chunk.len()))?
            > config.limits.maximum_source_bytes
        {
            return Err(KnowledgeError::SourceLimit("maximum_source_bytes").into());
        }
        bytes.extend_from_slice(&chunk);
    }
    let page: ConnectorPage = serde_json::from_slice(&bytes)?;
    page.validate(approved_origin)?;
    Ok(())
}

async fn load_current_acl_digests(
    pool: &PgPool,
    config: &WorkerConfig,
) -> Result<std::collections::BTreeMap<String, String>> {
    let rows = sqlx::query(
        "SELECT document.source_object_id,latest.evidence_digest
           FROM knowledge_document_t document
           JOIN LATERAL (
             SELECT revision.evidence_digest
               FROM knowledge_document_acl_t revision
              WHERE revision.document_id=document.document_id
              ORDER BY revision.acl_sequence DESC LIMIT 1
           ) latest ON TRUE
          WHERE document.knowledge_base_id=$1 AND document.source_id=$2",
    )
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("source_object_id"),
                row.get::<String, _>("evidence_digest").trim().to_string(),
            )
        })
        .collect())
}

fn resolve_connector_corpus(
    mode: ConnectorSyncMode,
    previous: &[CorpusDocumentState],
    objects: &std::collections::BTreeMap<String, knowledge_connectors::ConnectorObject>,
    page_documents: Vec<CorpusDocumentState>,
) -> Vec<CorpusDocumentState> {
    if mode == ConnectorSyncMode::Full {
        return page_documents;
    }
    let mut resolved = previous.to_vec();
    for object in objects.values() {
        resolved.retain(|document| document.source_object_id != object.external_id);
    }
    resolved.extend(page_documents);
    resolved.sort_by(|left, right| left.source_object_id.cmp(&right.source_object_id));
    resolved
}

async fn load_connector_page(
    pool: &PgPool,
    config: &WorkerConfig,
    approved_origin: &str,
) -> Result<ConnectorPage> {
    if let Some(fixture) = &config.enterprise_connector_fixture_file {
        let bytes = fs::read(fixture)?;
        if u64::try_from(bytes.len())? > config.limits.maximum_source_bytes {
            return Err(KnowledgeError::SourceLimit("maximum_source_bytes").into());
        }
        let page: ConnectorPage = serde_json::from_slice(&bytes)?;
        if page.objects.len() > config.limits.maximum_documents {
            return Err(KnowledgeError::SourceLimit("maximum_documents").into());
        }
        return Ok(page);
    }
    let endpoint = config
        .enterprise_connector_page_url
        .as_deref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_NOT_CONFIGURED")?;
    let token_file = config
        .enterprise_connector_authorization_file
        .as_ref()
        .context("KNOWLEDGE_ENTERPRISE_CONNECTOR_AUTHORIZATION_REQUIRED")?;
    let mut cursor = sqlx::query_scalar::<_, Option<String>>(
        "SELECT opaque_cursor FROM knowledge_source_cursor_t WHERE source_id=$1",
    )
    .bind(config.source_id)
    .fetch_optional(pool)
    .await?
    .flatten();
    let token = fs::read_to_string(token_file)?;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    let mut combined: Option<ConnectorPage> = None;
    let mut identities = std::collections::BTreeSet::new();
    let mut total_bytes = 0_u64;
    for _ in 0..config.maximum_provider_calls {
        let response = client
            .post(endpoint)
            .bearer_auth(token.trim())
            .header("x-knowledge-base-id", config.knowledge_base_id.to_string())
            .header("x-knowledge-source-id", config.source_id.to_string())
            .json(&json!({"cursor": cursor, "fullPermissionReconciliation": true}))
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "KNOWLEDGE_CONNECTOR_HTTP_STATUS:{}",
                response.status().as_u16()
            );
        }
        let bytes = response.bytes().await?;
        total_bytes = total_bytes.saturating_add(u64::try_from(bytes.len())?);
        if total_bytes > config.limits.maximum_source_bytes {
            return Err(KnowledgeError::SourceLimit("maximum_source_bytes").into());
        }
        let page: ConnectorPage = serde_json::from_slice(&bytes)?;
        page.clone().validate(approved_origin)?;
        if page.requested_cursor != cursor {
            bail!("KNOWLEDGE_CONNECTOR_CURSOR_CHAIN_MISMATCH");
        }
        if let Some(first) = &combined
            && (first.provider != page.provider || first.sync_mode != page.sync_mode)
        {
            bail!("KNOWLEDGE_CONNECTOR_PAGE_CONTRACT_CHANGED");
        }
        for object in &page.objects {
            if !identities.insert(object.external_id.clone()) {
                bail!("KNOWLEDGE_CONNECTOR_DUPLICATE_OBJECT_ACROSS_PAGES");
            }
            if identities.len() > config.limits.maximum_documents {
                return Err(KnowledgeError::SourceLimit("maximum_documents").into());
            }
        }
        let complete = page.reconciliation_complete;
        let next_cursor = page.next_cursor.clone();
        if let Some(accumulated) = &mut combined {
            accumulated.objects.extend(page.objects);
            accumulated.next_cursor = next_cursor.clone();
            accumulated.observed_at = accumulated.observed_at.max(page.observed_at);
            accumulated.reconciliation_complete = complete;
        } else {
            combined = Some(page);
        }
        if complete {
            return combined.context("KNOWLEDGE_CONNECTOR_EMPTY_PAGE_SEQUENCE");
        }
        cursor = Some(next_cursor);
    }
    bail!("KNOWLEDGE_CONNECTOR_PROVIDER_CALL_LIMIT_EXCEEDED")
}

async fn record_connector_notification(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let notification_id = payload
        .get("providerNotificationId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .context("KNOWLEDGE_PROVIDER_NOTIFICATION_ID_REQUIRED")?;
    let provider = payload
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .filter(|value| matches!(*value, "SHAREPOINT" | "CONFLUENCE"))
        .context("KNOWLEDGE_PROVIDER_NOTIFICATION_INVALID")?;
    sqlx::query(
        "INSERT INTO knowledge_connector_notification_t(
           connector_notification_id,source_id,provider,
           provider_notification_id,state,evidence_digest)
         VALUES($1,$2,$3,$4,'RECEIVED',$5)
         ON CONFLICT(source_id,provider_notification_id) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(config.source_id)
    .bind(provider)
    .bind(notification_id)
    .bind(sha256_hex(
        format!("{provider}:{notification_id}").as_bytes(),
    ))
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn persist_connector_reconciliation(
    pool: &PgPool,
    config: &WorkerConfig,
    reconciliation_id: Uuid,
    provider: &str,
    page: &ValidatedConnectorPage,
    objects: &std::collections::BTreeMap<String, knowledge_connectors::ConnectorObject>,
    previous_acl: std::collections::BTreeMap<String, String>,
    input_cursor_digest: Option<&str>,
    output_cursor_digest: &str,
    content_consistent: bool,
    pending_promotion: Option<&serde_json::Value>,
) -> Result<()> {
    let effective_observed_at = page.page().observed_at.min(Utc::now());
    let indexed = objects
        .values()
        .filter(|object| !object.deleted && !object.markdown.trim().is_empty())
        .collect::<Vec<_>>();
    let normalized = indexed
        .iter()
        .map(|object| {
            (
                *object,
                normalize_permission(&object.permission, effective_observed_at),
            )
        })
        .collect::<Vec<_>>();
    let unresolved = normalized
        .iter()
        .flat_map(|(_, acl)| &acl.subjects)
        .filter(|subject| !subject.mapping_complete)
        .count();
    let page_complete = page.page().reconciliation_complete
        && normalized.iter().all(|(_, acl)| acl.complete)
        && unresolved == 0
        && content_consistent;
    let evidence_digest = sha256_hex(
        serde_json::to_vec(&normalized.iter().map(|(_, acl)| acl).collect::<Vec<_>>())?.as_slice(),
    );
    let mut tx = pool.begin().await?;
    let owner_host_id = sqlx::query_scalar::<_, Option<Uuid>>(
        "SELECT host_id FROM knowledge_base_t WHERE knowledge_base_id=$1",
    )
    .bind(config.knowledge_base_id)
    .fetch_one(&mut *tx)
    .await?;
    for object in objects.values() {
        let document_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT document_id FROM knowledge_document_t
              WHERE knowledge_base_id=$1 AND source_id=$2 AND source_object_id=$3",
        )
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(&object.external_id)
        .fetch_optional(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_connector_object_t(
               connector_object_id,knowledge_base_id,source_id,provider,external_id,
               provider_version,canonical_uri,document_id,parent_external_id,
               relationship_kind,deleted,last_reconciliation_id,observed_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,
               CASE WHEN $9::text IS NULL THEN 'NONE' ELSE 'CONTAINMENT' END,
               $10,$11,$12)
             ON CONFLICT(source_id,external_id) DO UPDATE SET
               provider_version=EXCLUDED.provider_version,
               canonical_uri=EXCLUDED.canonical_uri,document_id=EXCLUDED.document_id,
               parent_external_id=EXCLUDED.parent_external_id,
               relationship_kind=EXCLUDED.relationship_kind,deleted=EXCLUDED.deleted,
               last_reconciliation_id=EXCLUDED.last_reconciliation_id,
               observed_ts=EXCLUDED.observed_ts",
        )
        .bind(derived_uuid_text(&format!(
            "connector:{}:{}",
            config.source_id, object.external_id
        )))
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(provider)
        .bind(&object.external_id)
        .bind(&object.provider_version)
        .bind(&object.canonical_uri)
        .bind(document_id)
        .bind(&object.parent_external_id)
        .bind(object.deleted)
        .bind(reconciliation_id)
        .bind(effective_observed_at)
        .execute(&mut *tx)
        .await?;
    }
    if page.page().sync_mode == ConnectorSyncMode::Full {
        sqlx::query(
            "UPDATE knowledge_connector_object_t SET deleted=TRUE,
               last_reconciliation_id=$2,observed_ts=$3
             WHERE source_id=$1 AND last_reconciliation_id<>$2",
        )
        .bind(config.source_id)
        .bind(reconciliation_id)
        .bind(effective_observed_at)
        .execute(&mut *tx)
        .await?;
    }
    for (object, acl) in &normalized {
        let document_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT document_id FROM knowledge_document_t
              WHERE knowledge_base_id=$1 AND source_id=$2 AND source_object_id=$3
              FOR UPDATE",
        )
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(&object.external_id)
        .fetch_one(&mut *tx)
        .await?;
        let acl_digest = permission_digest(acl);
        let acl_revision_id =
            derived_uuid_text(&format!("provider-acl:{document_id}:{acl_digest}"));
        let acl_sequence = sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(max(acl_sequence),0)+1 FROM knowledge_document_acl_t
              WHERE document_id=$1",
        )
        .bind(document_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_document_acl_t(
               acl_revision_id,document_id,knowledge_base_id,acl_sequence,
               visibility_mode,normalized_acl,normalization_contract_digest,
               completeness_state,observed_ts,fresh_until_ts,evidence_digest,
               reconciliation_id,provider_effective_decision)
             VALUES($1,$2,$3,$4,'MIRROR_SOURCE_ACL',$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT(acl_revision_id) DO NOTHING",
        )
        .bind(acl_revision_id)
        .bind(document_id)
        .bind(config.knowledge_base_id)
        .bind(acl_sequence)
        .bind(serde_json::to_value(acl)?)
        .bind(sha256_hex(b"principal-acl-v1"))
        .bind(if acl.complete {
            "COMPLETE"
        } else {
            "INCOMPLETE"
        })
        .bind(acl.observed_at)
        .bind(acl.fresh_until)
        .bind(&acl_digest)
        .bind(reconciliation_id)
        .bind(acl.provider_effective_decision)
        .execute(&mut *tx)
        .await?;
        if previous_acl
            .get(&object.external_id)
            .is_some_and(|previous| previous != &acl_digest)
        {
            sqlx::query(
                "INSERT INTO knowledge_acl_transition_t(
                   acl_transition_id,reconciliation_id,knowledge_base_id,
                   source_id,document_id,previous_acl_digest,current_acl_digest,
                   transition_kind,observed_ts)
                 VALUES($1,$2,$3,$4,$5,$6,$7,'PERMISSION_CHANGED',$8)
                 ON CONFLICT(reconciliation_id,document_id) DO NOTHING",
            )
            .bind(derived_uuid_text(&format!(
                "acl-transition:{reconciliation_id}:{document_id}"
            )))
            .bind(reconciliation_id)
            .bind(config.knowledge_base_id)
            .bind(config.source_id)
            .bind(document_id)
            .bind(previous_acl.get(&object.external_id))
            .bind(&acl_digest)
            .bind(effective_observed_at)
            .execute(&mut *tx)
            .await?;
        }
        for (ordinal, subject) in acl.subjects.iter().enumerate() {
            let subject_type = acl_subject_type_name(subject.subject_type);
            let mapping_id = derived_uuid_text(&format!(
                "subject-mapping:{:?}:{}:{}:{}",
                owner_host_id, config.source_id, subject_type, subject.provider_subject_id
            ));
            sqlx::query(
                "INSERT INTO knowledge_subject_mapping_t(
                   subject_mapping_id,host_id,source_id,provider_subject_type,
                   provider_subject_id,normalized_subject_type,
                   normalized_subject_id,mapping_state,evidence_digest,update_user)
                 VALUES($1,$2,$3,$4,$5,$4,$6,$7,$8,'light-knowledge-worker')
                 ON CONFLICT(subject_mapping_id) DO UPDATE SET
                   normalized_subject_type=EXCLUDED.normalized_subject_type,
                   normalized_subject_id=EXCLUDED.normalized_subject_id,
                   mapping_state=EXCLUDED.mapping_state,
                   evidence_digest=EXCLUDED.evidence_digest,
                   valid_from_ts=now(),valid_until_ts=NULL,
                   update_user=EXCLUDED.update_user",
            )
            .bind(mapping_id)
            .bind(owner_host_id)
            .bind(config.source_id)
            .bind(subject_type)
            .bind(&subject.provider_subject_id)
            .bind(if subject.mapping_complete {
                Some(subject.subject_id.as_str())
            } else {
                None
            })
            .bind(if subject.mapping_complete {
                "APPROVED"
            } else {
                "UNRESOLVED"
            })
            .bind(&subject.provider_evidence_digest)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "INSERT INTO knowledge_acl_subject_t(
                   acl_revision_id,subject_ordinal,knowledge_base_id,document_id,
                   provider_subject_type,provider_subject_id,normalized_subject_type,
                   normalized_subject_id,effect,mapping_complete,evidence_digest)
                 VALUES($1,$2,$3,$4,$5,$6,$5,$7,$8,$9,$10)
                 ON CONFLICT(acl_revision_id,subject_ordinal) DO NOTHING",
            )
            .bind(acl_revision_id)
            .bind(i32::try_from(ordinal).unwrap_or(i32::MAX))
            .bind(config.knowledge_base_id)
            .bind(document_id)
            .bind(subject_type)
            .bind(&subject.provider_subject_id)
            .bind(if subject.mapping_complete {
                Some(subject.subject_id.as_str())
            } else {
                None
            })
            .bind(acl_effect_name(subject.effect))
            .bind(subject.mapping_complete)
            .bind(&subject.provider_evidence_digest)
            .execute(&mut *tx)
            .await?;
        }
    }
    let page_discovered = i64::try_from(indexed.len()).unwrap_or(i64::MAX);
    let page_unresolved = i64::try_from(unresolved).unwrap_or(i64::MAX);
    let coverage = sqlx::query(
        "SELECT count(DISTINCT object.connector_object_id)
                  FILTER (WHERE NOT object.deleted) AS discovered,
                count(DISTINCT object.connector_object_id)
                  FILTER (WHERE NOT object.deleted
                  AND object.document_id IS NOT NULL
                  AND latest.visibility_mode='MIRROR_SOURCE_ACL'
                  AND latest.completeness_state='COMPLETE'
                  AND latest.provider_effective_decision
                  AND NOT EXISTS (
                    SELECT 1 FROM knowledge_acl_subject_t subject
                     WHERE subject.acl_revision_id=latest.acl_revision_id
                       AND (NOT subject.mapping_complete
                            OR subject.normalized_subject_type='UNRESOLVED')
                  )) AS covered,
                count(subject.acl_revision_id) FILTER (WHERE NOT subject.mapping_complete
                  OR subject.normalized_subject_type='UNRESOLVED') AS unresolved
           FROM knowledge_connector_object_t object
           LEFT JOIN LATERAL (
             SELECT revision.* FROM knowledge_document_acl_t revision
              WHERE revision.document_id=object.document_id
              ORDER BY revision.acl_sequence DESC LIMIT 1
           ) latest ON TRUE
           LEFT JOIN knowledge_acl_subject_t subject
             ON subject.acl_revision_id=latest.acl_revision_id
          WHERE object.source_id=$1",
    )
    .bind(config.source_id)
    .fetch_one(&mut *tx)
    .await?;
    let discovered: i64 = coverage.get("discovered");
    let covered: i64 = coverage.get("covered");
    let unresolved_total: i64 = coverage.get("unresolved");
    let complete = page_complete && covered == discovered && unresolved_total == 0;
    let state = if complete { "COMPLETE" } else { "INCOMPLETE" };
    sqlx::query(
        "UPDATE knowledge_acl_reconciliation_t SET state=$2,
           discovered_object_count=$3,applied_acl_count=$4,
           denied_object_count=$5,unresolved_subject_count=$6,
           provider_evidence=jsonb_build_object('cursorCommitted',$7),
           evidence_digest=$8,finished_ts=statement_timestamp(),
           fresh_until_ts=statement_timestamp()+interval '15 minutes'
         WHERE reconciliation_id=$1",
    )
    .bind(reconciliation_id)
    .bind(state)
    .bind(page_discovered)
    .bind(if page_complete { page_discovered } else { 0 })
    .bind(if page_complete {
        0_i64
    } else {
        page_discovered
    })
    .bind(page_unresolved)
    .bind(complete)
    .bind(&evidence_digest)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_source_acl_state_t(
           source_id,knowledge_base_id,reconciliation_id,state,
           discovered_object_count,covered_object_count,denied_object_count,
           unresolved_subject_count,observed_ts,fresh_until_ts,evidence_digest)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,statement_timestamp(),
                statement_timestamp()+interval '15 minutes',$9)
         ON CONFLICT(source_id) DO UPDATE SET
           reconciliation_id=EXCLUDED.reconciliation_id,state=EXCLUDED.state,
           discovered_object_count=EXCLUDED.discovered_object_count,
           covered_object_count=EXCLUDED.covered_object_count,
           denied_object_count=EXCLUDED.denied_object_count,
           unresolved_subject_count=EXCLUDED.unresolved_subject_count,
           observed_ts=EXCLUDED.observed_ts,
           fresh_until_ts=EXCLUDED.fresh_until_ts,
           evidence_digest=EXCLUDED.evidence_digest,update_ts=now()",
    )
    .bind(config.source_id)
    .bind(config.knowledge_base_id)
    .bind(reconciliation_id)
    .bind(state)
    .bind(discovered)
    .bind(if complete { covered } else { 0 })
    .bind(if complete { 0 } else { discovered })
    .bind(unresolved_total)
    .bind(&evidence_digest)
    .execute(&mut *tx)
    .await?;
    if complete {
        let cursor_result = sqlx::query(
            "INSERT INTO knowledge_source_cursor_t(
               source_id,knowledge_base_id,opaque_cursor,last_full_reconciliation_ts,
               cursor_digest,update_ts)
             VALUES($1,$2,$3,CASE WHEN $6 THEN now() ELSE NULL END,$4,now())
             ON CONFLICT(source_id) DO UPDATE SET opaque_cursor=EXCLUDED.opaque_cursor,
               last_full_reconciliation_ts=CASE WHEN $6
                 THEN now() ELSE knowledge_source_cursor_t.last_full_reconciliation_ts END,
               cursor_digest=EXCLUDED.cursor_digest,update_ts=now()
             WHERE knowledge_source_cursor_t.cursor_digest IS NOT DISTINCT FROM $5",
        )
        .bind(config.source_id)
        .bind(config.knowledge_base_id)
        .bind(page.committed_cursor())
        .bind(output_cursor_digest)
        .bind(input_cursor_digest)
        .bind(page.page().sync_mode == ConnectorSyncMode::Full)
        .execute(&mut *tx)
        .await?;
        if cursor_result.rows_affected() != 1 {
            bail!("KNOWLEDGE_CONNECTOR_CURSOR_CONFLICT");
        }
        if let Some(promotion) = pending_promotion {
            promote_generation_transaction(&mut tx, config, promotion).await?;
        }
    }
    tx.commit().await?;
    if !complete {
        bail!("KNOWLEDGE_CONNECTOR_PERMISSION_INCOMPLETE");
    }
    Ok(())
}

fn connector_kind_name(kind: ConnectorKind) -> &'static str {
    match kind {
        ConnectorKind::SharePoint => "SHAREPOINT",
        ConnectorKind::Confluence => "CONFLUENCE",
    }
}

fn connector_sync_mode_name(mode: ConnectorSyncMode) -> &'static str {
    match mode {
        ConnectorSyncMode::Full => "FULL",
        ConnectorSyncMode::Delta => "DELTA",
    }
}

fn acl_subject_type_name(kind: knowledge_core::AclSubjectType) -> &'static str {
    match kind {
        knowledge_core::AclSubjectType::User => "USER",
        knowledge_core::AclSubjectType::Group => "GROUP",
        knowledge_core::AclSubjectType::Organization => "ORGANIZATION",
        knowledge_core::AclSubjectType::Everyone => "EVERYONE",
        knowledge_core::AclSubjectType::Unresolved => "UNRESOLVED",
    }
}

fn acl_effect_name(effect: knowledge_core::AclEffect) -> &'static str {
    match effect {
        knowledge_core::AclEffect::Allow => "ALLOW",
        knowledge_core::AclEffect::Deny => "DENY",
    }
}

async fn load_corpus_state(
    pool: &PgPool,
    config: &WorkerConfig,
) -> Result<Vec<CorpusDocumentState>> {
    let generation_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT index_generation_id FROM knowledge_index_pointer_t
          WHERE knowledge_base_id=$1 AND environment=$2",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .fetch_optional(pool)
    .await?;
    let Some(generation_id) = generation_id else {
        return Ok(Vec::new());
    };
    load_generation_corpus_state(pool, config, generation_id).await
}

async fn load_generation_corpus_state(
    pool: &PgPool,
    config: &WorkerConfig,
    generation_id: Uuid,
) -> Result<Vec<CorpusDocumentState>> {
    let rows = sqlx::query(
        "WITH generation_segments AS (
           SELECT member.index_segment_id,member.ordinal
             FROM knowledge_generation_segment_t member
            WHERE member.index_generation_id=$1
         ), eligible_documents AS (
           SELECT member.document_id,member.document_version_id,segment.ordinal
             FROM generation_segments segment
             JOIN knowledge_segment_document_t member
               ON member.index_segment_id=segment.index_segment_id
            WHERE NOT EXISTS (
              SELECT 1 FROM generation_segments later
              JOIN knowledge_segment_operation_t operation
                ON operation.index_segment_id=later.index_segment_id
               AND operation.document_id=member.document_id
             WHERE later.ordinal>segment.ordinal
               AND operation.operation_kind IN (
                 'SUPERSEDE_DOCUMENT','TOMBSTONE_DOCUMENT'))
         )
         SELECT DISTINCT ON (d.document_id)
                d.source_id,d.source_object_id,d.canonical_uri,v.source_version,
                v.content_digest,v.object_locator
           FROM eligible_documents eligible
           JOIN knowledge_document_t d ON d.document_id=eligible.document_id
           JOIN knowledge_document_version_t v
             ON v.document_version_id=eligible.document_version_id
          WHERE d.knowledge_base_id=$2 AND ($3=$4 OR d.source_id=$3)
          ORDER BY d.document_id,eligible.ordinal DESC",
    )
    .bind(generation_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .bind(Uuid::nil())
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let locator: String = row.get("object_locator");
            let markdown =
                fs::read_to_string(object_locator_path(&config.object_store_root, &locator)?)?;
            let source_id: Uuid = row.get("source_id");
            let mut source_object_id: String = row.get("source_object_id");
            if source_id_from_object_id(&source_object_id).is_none() {
                source_object_id = format!("{source_id}/{source_object_id}");
            }
            Ok(CorpusDocumentState {
                source_object_id,
                canonical_uri: row.get("canonical_uri"),
                source_version: row.get("source_version"),
                content_digest: row.get::<String, _>("content_digest").trim().into(),
                metadata_digest: sha256_hex(b"{}"),
                acl_digest: sha256_hex(b"UNIFORM_SCOPE"),
                markdown,
            })
        })
        .collect()
}

fn object_locator_path(root: &Path, locator: &str) -> Result<PathBuf> {
    let relative = locator
        .strip_prefix("object://light-knowledge/")
        .context("unsupported Knowledge object locator")?;
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("Knowledge object locator escapes the configured object store");
    }
    Ok(root.join(relative_path))
}

async fn incremental_from_states(
    pool: &PgPool,
    config: &WorkerConfig,
    previous: &[CorpusDocumentState],
    current: &[CorpusDocumentState],
    upload_id: Option<Uuid>,
    defer_promotion: bool,
) -> Result<Option<serde_json::Value>> {
    let changes = classify_corpus_changes(config.knowledge_base_id, previous, current);
    if changes.is_empty() {
        return Ok(None);
    }
    let current_by_id = current
        .iter()
        .map(|document| (document.source_object_id.as_str(), document))
        .collect::<std::collections::BTreeMap<_, _>>();
    let changed_inputs = changes
        .iter()
        .filter_map(|change| match change.kind {
            ChangeKind::Add | ChangeKind::Modify | ChangeKind::MetadataOnly => {
                let document = current_by_id.get(change.source_object_id.as_str())?;
                Some(DocumentInput {
                    source_object_id: document.source_object_id.clone(),
                    canonical_uri: document.canonical_uri.clone(),
                    source_version: document.source_version.clone(),
                    markdown: document.markdown.clone(),
                })
            }
            ChangeKind::Delete | ChangeKind::AclOnly => None,
        })
        .collect::<Vec<_>>();
    let stored_bytes = changed_inputs.iter().try_fold(0_u64, |total, document| {
        total
            .checked_add(u64::try_from(document.markdown.len()).unwrap_or(u64::MAX))
            .context("incremental stored byte count overflow")
    })?;
    if stored_bytes > config.maximum_stored_bytes {
        bail!("KNOWLEDGE_INGESTION_MAX_STORED_BYTES_EXCEEDED");
    }
    let watermark = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(max(snapshot_watermark),0)+1
           FROM knowledge_index_generation_t WHERE knowledge_base_id=$1",
    )
    .bind(config.knowledge_base_id)
    .fetch_one(pool)
    .await?;
    let mut generation = build_full_base(
        config.knowledge_base_id,
        u64::try_from(watermark).unwrap_or(config.snapshot_watermark + 1),
        &changed_inputs,
        &ProcessingContract::default(),
        &config.limits,
    )?;
    apply_configured_embeddings(config, &mut generation).await?;
    let objects = write_objects(&config.object_store_root, &generation, &changed_inputs)?;
    persist_full_base(pool, config, &generation, &objects).await?;
    convert_candidate_to_delta(
        pool,
        config,
        &generation,
        &changes,
        upload_id,
        defer_promotion,
    )
    .await
}

async fn phase1b_schema_ready(pool: &PgPool) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT to_regclass('knowledge_source_change_t') IS NOT NULL
             AND to_regclass('knowledge_segment_operation_t') IS NOT NULL
             AND to_regclass('knowledge_passage_anchor_t') IS NOT NULL
             AND to_regclass('knowledge_embedding_reference_t') IS NOT NULL",
    )
    .fetch_one(pool)
    .await?)
}

async fn convert_candidate_to_delta(
    pool: &PgPool,
    config: &WorkerConfig,
    generation: &FullBaseGeneration,
    changes: &[knowledge_core::ClassifiedChange],
    upload_id: Option<Uuid>,
    defer_promotion: bool,
) -> Result<Option<serde_json::Value>> {
    if !phase1b_schema_ready(pool).await? {
        bail!("KNOWLEDGE_PHASE1B_SCHEMA_REQUIRED");
    }
    let pointer = sqlx::query(
        "SELECT p.index_generation_id,p.pointer_version,
                g.ordered_segment_manifest_digest
           FROM knowledge_index_pointer_t p
           JOIN knowledge_index_generation_t g
             ON g.index_generation_id=p.index_generation_id
          WHERE p.knowledge_base_id=$1 AND p.environment=$2",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .fetch_optional(pool)
    .await?;
    let Some(pointer) = pointer else {
        if defer_promotion {
            let evidence = json!({
                "phase": "2",
                "segmentKind": "BASE",
                "connectorReconciliation": true
            });
            return Ok(Some(json!({
                "promotionId": derived_uuid(
                    "connector-base-promotion",
                    generation.manifest.generation_id
                ),
                "indexGenerationId": generation.manifest.generation_id,
                "expectedPointerVersion": 0,
                "evidence": evidence,
                "evidenceDigest": sha256_hex(
                    serde_json::to_string(&evidence)?.as_bytes()
                ),
                "reason": "complete Phase 2 connector BASE reconciliation"
            })));
        }
        return Ok(None);
    };
    let predecessor_generation_id: Uuid = pointer.get("index_generation_id");
    if predecessor_generation_id == generation.manifest.generation_id {
        return Ok(None);
    }
    let pointer_version: i64 = pointer.get("pointer_version");
    let predecessor_digest: Option<String> = pointer.get("ordered_segment_manifest_digest");
    let mut tx = pool.begin().await?;
    let sync_run_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT (evidence->>'syncRunId')::uuid FROM knowledge_index_generation_t
          WHERE index_generation_id=$1",
    )
    .bind(generation.manifest.generation_id)
    .fetch_one(&mut *tx)
    .await?;
    let source_change_sequence = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(max(change_sequence),0) FROM knowledge_source_change_t
          WHERE source_id=$1",
    )
    .bind(config.source_id)
    .fetch_one(&mut *tx)
    .await?;
    let predecessor_segment_id = sqlx::query_scalar::<_, Uuid>(
        "SELECT index_segment_id FROM knowledge_generation_segment_t
          WHERE index_generation_id=$1 ORDER BY ordinal DESC LIMIT 1",
    )
    .bind(predecessor_generation_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM knowledge_generation_segment_t WHERE index_generation_id=$1")
        .bind(generation.manifest.generation_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE knowledge_index_segment_t SET segment_kind='DELTA',
                predecessor_segment_id=$2,operation_count=$3
          WHERE index_segment_id=$1",
    )
    .bind(generation.manifest.segment_id)
    .bind(predecessor_segment_id)
    .bind(i64::try_from(changes.len()).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_generation_segment_t(index_generation_id,ordinal,index_segment_id)
         SELECT $1,ordinal,index_segment_id FROM knowledge_generation_segment_t
          WHERE index_generation_id=$2 ORDER BY ordinal",
    )
    .bind(generation.manifest.generation_id)
    .bind(predecessor_generation_id)
    .execute(&mut *tx)
    .await?;
    let ordinal = sqlx::query_scalar::<_, i32>(
        "SELECT COALESCE(max(ordinal),-1)+1 FROM knowledge_generation_segment_t
          WHERE index_generation_id=$1",
    )
    .bind(generation.manifest.generation_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_generation_segment_t(index_generation_id,ordinal,index_segment_id)
         VALUES($1,$2,$3)",
    )
    .bind(generation.manifest.generation_id)
    .bind(ordinal)
    .bind(generation.manifest.segment_id)
    .execute(&mut *tx)
    .await?;
    for (operation_ordinal, change) in changes.iter().enumerate() {
        let operation_ordinal = i64::try_from(operation_ordinal).unwrap_or(i64::MAX);
        let operation_id = derived_uuid_text(&format!(
            "segment-operation:{}:{}:{}",
            generation.manifest.segment_id, operation_ordinal, change.change_digest
        ));
        let source_change_id = derived_uuid_text(&format!(
            "source-change:{sync_run_id}:{operation_ordinal}:{}",
            change.change_digest
        ));
        let document_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT document_id FROM knowledge_document_t
              WHERE knowledge_base_id=$1 AND source_id=$2 AND source_object_id=$3",
        )
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(&change.source_object_id)
        .fetch_one(&mut *tx)
        .await?;
        let operation_kind = match change.kind {
            ChangeKind::Add => "ACTIVATE_DOCUMENT",
            ChangeKind::Modify | ChangeKind::MetadataOnly => "SUPERSEDE_DOCUMENT",
            ChangeKind::Delete => "TOMBSTONE_DOCUMENT",
            ChangeKind::AclOnly => "SET_ACL_REVISION",
        };
        let change_kind = match change.kind {
            ChangeKind::Add => "ADD",
            ChangeKind::Modify => "MODIFY",
            ChangeKind::Delete => "DELETE",
            ChangeKind::AclOnly => "ACL_ONLY",
            ChangeKind::MetadataOnly => "METADATA_ONLY",
        };
        let selected_document_version_id = sqlx::query_scalar::<_, Option<Uuid>>(
            "SELECT current_document_version_id FROM knowledge_document_t
              WHERE document_id=$1 AND knowledge_base_id=$2",
        )
        .bind(document_id)
        .bind(config.knowledge_base_id)
        .fetch_one(&mut *tx)
        .await?;
        let selected_acl_revision_id = if change.kind == ChangeKind::AclOnly {
            Some(
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT acl_revision_id FROM knowledge_document_acl_t
                      WHERE document_id=$1 AND knowledge_base_id=$2
                      ORDER BY acl_sequence DESC LIMIT 1",
                )
                .bind(document_id)
                .bind(config.knowledge_base_id)
                .fetch_optional(&mut *tx)
                .await?
                .context("ACL-only change has no selected ACL revision")?,
            )
        } else {
            None
        };
        let previous_document_version_id = match change.previous_source_version.as_deref() {
            Some(source_version) => {
                sqlx::query_scalar::<_, Uuid>(
                    "SELECT document_version_id FROM knowledge_document_version_t
                  WHERE document_id=$1 AND knowledge_base_id=$2 AND source_version=$3
                  ORDER BY created_ts DESC LIMIT 1",
                )
                .bind(document_id)
                .bind(config.knowledge_base_id)
                .bind(source_version)
                .fetch_optional(&mut *tx)
                .await?
            }
            None => None,
        };
        sqlx::query(
            "INSERT INTO knowledge_source_change_t(
               source_change_id,sync_run_id,knowledge_base_id,source_id,
               source_object_id,change_sequence,change_kind,
               previous_document_version_id,selected_document_version_id,
               selected_acl_revision_id,input_contract_digest,change_digest,observed_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,now())
             ON CONFLICT(sync_run_id,source_object_id) DO NOTHING",
        )
        .bind(source_change_id)
        .bind(sync_run_id)
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(&change.source_object_id)
        .bind(source_change_sequence + operation_ordinal + 1)
        .bind(change_kind)
        .bind(previous_document_version_id)
        .bind(selected_document_version_id)
        .bind(selected_acl_revision_id)
        .bind(sha256_hex(
            format!(
                "{}:{}:{}:{}",
                generation.manifest.parser_digest,
                generation.manifest.chunker_digest,
                generation.manifest.lexical_digest,
                generation.manifest.citation_digest
            )
            .as_bytes(),
        ))
        .bind(&change.change_digest)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_segment_operation_t(
               index_segment_id,operation_ordinal,operation_id,knowledge_base_id,
               operation_kind,document_id,acl_revision_id,operation_digest)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING",
        )
        .bind(generation.manifest.segment_id)
        .bind(operation_ordinal)
        .bind(operation_id)
        .bind(config.knowledge_base_id)
        .bind(operation_kind)
        .bind(document_id)
        .bind(selected_acl_revision_id)
        .bind(&change.change_digest)
        .execute(&mut *tx)
        .await?;
        if change.kind == ChangeKind::Delete {
            sqlx::query(
                "UPDATE knowledge_document_t SET lifecycle_state='DELETED',update_ts=now()
                  WHERE document_id=$1 AND knowledge_base_id=$2",
            )
            .bind(document_id)
            .bind(config.knowledge_base_id)
            .execute(&mut *tx)
            .await?;
        }
    }
    for chunk in &generation.chunks {
        persist_passage_anchor(
            &mut tx,
            config.knowledge_base_id,
            chunk,
            &generation.manifest.citation_digest,
        )
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_embedding_reference_t(
               embedding_artifact_id,knowledge_base_id,chunk_id,input_digest,
               transform_contract_digest)
             VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
        )
        .bind(embedding_artifact_id(
            config,
            generation,
            &chunk.content_digest,
        ))
        .bind(config.knowledge_base_id)
        .bind(chunk.chunk_id)
        .bind(&chunk.content_digest)
        .bind(sha256_hex(b"document-v1"))
        .execute(&mut *tx)
        .await?;
    }
    let ordered_digest = append_ordered_segment_digest(
        predecessor_digest.as_deref(),
        &generation.manifest.manifest_digest,
        ordinal,
    );
    sqlx::query(
        "UPDATE knowledge_index_generation_t SET ordered_segment_manifest_digest=$2,
                evidence=evidence||$3::jsonb
          WHERE index_generation_id=$1",
    )
    .bind(generation.manifest.generation_id)
    .bind(&ordered_digest)
    .bind(json!({"phase": "1b", "delta": true, "uploadId": upload_id}))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let evidence = json!({
        "phase": "1b",
        "segmentKind": "DELTA",
        "predecessorGenerationId": predecessor_generation_id,
        "orderedManifestDigest": ordered_digest
    });
    let promotion = json!({
        "promotionId": derived_uuid("delta-promotion", generation.manifest.generation_id),
        "indexGenerationId": generation.manifest.generation_id,
        "expectedPointerVersion": pointer_version,
        "evidence": evidence,
        "evidenceDigest": sha256_hex(serde_json::to_string(&evidence)?.as_bytes()),
        "reason": "validated Phase 1b incremental generation"
    });
    if defer_promotion {
        Ok(Some(promotion))
    } else {
        promote_generation(pool, config, &promotion).await?;
        Ok(None)
    }
}

async fn run_anti_entropy(
    pool: &PgPool,
    config: &WorkerConfig,
    payload: &serde_json::Value,
) -> Result<()> {
    let generation_id = payload
        .get("indexGenerationId")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let generation = sqlx::query(
        "SELECT g.index_generation_id,g.ordered_segment_manifest_digest
           FROM knowledge_index_pointer_t p
           JOIN knowledge_index_generation_t g
             ON g.index_generation_id=p.index_generation_id
          WHERE p.knowledge_base_id=$1 AND p.environment=$2
            AND ($3::uuid IS NULL OR g.index_generation_id=$3)
          FOR SHARE",
    )
    .bind(config.knowledge_base_id)
    .bind(&config.environment)
    .bind(generation_id)
    .fetch_one(pool)
    .await?;
    let generation_id: Uuid = generation.get("index_generation_id");
    let expected_digest = generation
        .get::<Option<String>, _>("ordered_segment_manifest_digest")
        .map(|value| value.trim().to_string())
        .context("active generation has no ordered segment manifest digest")?;
    let segments = sqlx::query(
        "SELECT member.ordinal,segment.manifest_digest,segment.physical_locator,
                segment.document_count,segment.chunk_count,
                (SELECT count(*) FROM knowledge_segment_document_t document
                  WHERE document.index_segment_id=segment.index_segment_id)
                  AS observed_document_count,
                (SELECT count(*) FROM knowledge_segment_chunk_t chunk
                  WHERE chunk.index_segment_id=segment.index_segment_id)
                  AS observed_chunk_count
           FROM knowledge_generation_segment_t member
           JOIN knowledge_index_segment_t segment
             ON segment.index_segment_id=member.index_segment_id
          WHERE member.index_generation_id=$1 ORDER BY member.ordinal",
    )
    .bind(generation_id)
    .fetch_all(pool)
    .await?;
    let mut observed_digest = String::new();
    let mut document_count_mismatches = 0_u64;
    let mut chunk_count_mismatches = 0_u64;
    let mut manifest_object_mismatches = 0_u64;
    for segment in &segments {
        let ordinal: i32 = segment.get("ordinal");
        let manifest_digest = segment
            .get::<String, _>("manifest_digest")
            .trim()
            .to_string();
        observed_digest = append_ordered_segment_digest(
            (!observed_digest.is_empty()).then_some(observed_digest.as_str()),
            &manifest_digest,
            ordinal,
        );
        let declared_documents: i64 = segment.get("document_count");
        let declared_chunks: i64 = segment.get("chunk_count");
        let observed_documents: i64 = segment.get("observed_document_count");
        let observed_chunks: i64 = segment.get("observed_chunk_count");
        document_count_mismatches += u64::from(declared_documents != observed_documents);
        chunk_count_mismatches += u64::from(declared_chunks != observed_chunks);
        let locator: String = segment.get("physical_locator");
        let manifest_matches = object_locator_path(&config.object_store_root, &locator)
            .ok()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<BaseManifest>(&bytes).ok())
            .is_some_and(|manifest| {
                manifest.manifest_digest == manifest_digest
                    && i64::try_from(manifest.document_count).ok() == Some(declared_documents)
                    && i64::try_from(manifest.chunk_count).ok() == Some(declared_chunks)
            });
        manifest_object_mismatches += u64::from(!manifest_matches);
    }
    if observed_digest.is_empty() {
        observed_digest = "0".repeat(64);
    }
    let segment_count_mismatches = u64::from(segments.is_empty());
    let consistent = observed_digest == expected_digest
        && segment_count_mismatches == 0
        && document_count_mismatches == 0
        && chunk_count_mismatches == 0
        && manifest_object_mismatches == 0;
    let run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO knowledge_anti_entropy_run_t(
           anti_entropy_run_id,knowledge_base_id,index_generation_id,state,
           expected_manifest_digest,observed_manifest_digest,mismatch_counts,finished_ts)
         VALUES($1,$2,$3,$4,$5,$6,$7,now())",
    )
    .bind(run_id)
    .bind(config.knowledge_base_id)
    .bind(generation_id)
    .bind(if consistent { "CONSISTENT" } else { "DRIFTED" })
    .bind(&expected_digest)
    .bind(&observed_digest)
    .bind(json!({
        "segmentCount": segment_count_mismatches,
        "documentCount": document_count_mismatches,
        "chunkCount": chunk_count_mismatches,
        "manifestObject": manifest_object_mismatches
    }))
    .execute(pool)
    .await?;
    Ok(())
}

fn append_ordered_segment_digest(
    predecessor_digest: Option<&str>,
    manifest_digest: &str,
    ordinal: i32,
) -> String {
    if ordinal == 0 {
        manifest_digest.to_string()
    } else {
        sha256_hex(
            format!(
                "{}:{manifest_digest}:{ordinal}",
                predecessor_digest.unwrap_or_default()
            )
            .as_bytes(),
        )
    }
}

async fn apply_configured_embeddings(
    config: &WorkerConfig,
    generation: &mut FullBaseGeneration,
) -> Result<()> {
    if config.deterministic_pilot {
        return Ok(());
    }
    let endpoint = config
        .embedding_gateway_url
        .as_deref()
        .context("protected embedding mode requires embeddingGatewayUrl")?;
    let token = fs::read_to_string(
        config
            .embedding_authorization_file
            .as_ref()
            .context("protected embedding mode requires embeddingAuthorizationFile")?,
    )?;
    let client = embedding_http_client(config)?;
    let mut pending = embedding_batches(
        &generation.chunks,
        config.embedding_batch_size,
        config.source_id,
    )?;
    let mut vectors = vec![None; generation.chunks.len()];
    let mut remaining_cost_by_source = config
        .resolved_sources
        .iter()
        .map(|source| (source.source_id, source.maximum_spend_micros))
        .collect::<HashMap<_, _>>();
    if remaining_cost_by_source.is_empty() && !config.source_id.is_nil() {
        remaining_cost_by_source.insert(config.source_id, config.maximum_spend_micros);
    }
    while let Some((start, end, source_id)) = pending.pop() {
        let remaining_cost_micros = remaining_cost_by_source
            .get_mut(&source_id)
            .context("KNOWLEDGE_INGESTION_SOURCE_SPEND_BUDGET_UNAVAILABLE")?;
        let texts = generation.chunks[start..end]
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>();
        let input_digest = sha256_hex(
            generation.chunks[start..end]
                .iter()
                .flat_map(|chunk| chunk.content_digest.as_bytes())
                .copied()
                .collect::<Vec<_>>()
                .as_slice(),
        );
        let response = client
            .post(endpoint)
            .bearer_auth(token.trim())
            .header("x-request-id", format!("kb-index:{input_digest}"))
            .header(
                "x-light-expected-embedding-space-id",
                &config.embedding_space_id,
            )
            .header(
                "x-light-expected-embedding-space-revision",
                config.embedding_space_revision.to_string(),
            )
            .header(
                "x-light-maximum-billed-cost-micros",
                remaining_cost_micros.to_string(),
            )
            .json(&json!({
                "model": config.embedding_alias,
                "input": texts,
                "dimensions": config.embedding_dimension
            }))
            .send()
            .await;
        let parsed = match response {
            Ok(response) if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .context("embedding gateway returned an invalid 429 error envelope")?;
                let error = body.get("error");
                let error_code = error
                    .and_then(|error| error.get("code"))
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| {
                        error
                            .and_then(|error| error.get("type"))
                            .and_then(serde_json::Value::as_str)
                    });
                if error_code == Some("budget_exhausted") {
                    bail!(
                        "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED: embedding gateway rejected the request budget"
                    );
                }
                None
            }
            Ok(response)
                if response.status().is_success()
                    && response
                        .headers()
                        .get("x-light-embedding-space-id")
                        .and_then(|value| value.to_str().ok())
                        == Some(config.embedding_space_id.as_str())
                    && response
                        .headers()
                        .get("x-light-embedding-space-revision")
                        .and_then(|value| value.to_str().ok())
                        == Some(config.embedding_space_revision.to_string().as_str()) =>
            {
                let billed_cost_micros = response
                    .headers()
                    .get("x-light-billed-cost-micros")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .context("embedding gateway omitted bounded billed-cost evidence")?;
                *remaining_cost_micros = remaining_cost_micros
                    .checked_sub(billed_cost_micros)
                    .context("embedding gateway exceeded the ingestion spend budget")?;
                response.json::<serde_json::Value>().await.ok()
            }
            _ => None,
        };
        let batch_vectors = parsed
            .as_ref()
            .and_then(|body| body.get("data"))
            .and_then(serde_json::Value::as_array)
            .filter(|data| data.len() == end - start)
            .and_then(|data| {
                data.iter()
                    .enumerate()
                    .map(|(expected_index, item)| {
                        let index = item.get("index")?.as_u64()? as usize;
                        let values = item
                            .get("embedding")?
                            .as_array()?
                            .iter()
                            .map(|value| value.as_f64().map(|value| value as f32))
                            .collect::<Option<Vec<_>>>()?;
                        (index == expected_index
                            && values.len() == config.embedding_dimension
                            && values.iter().all(|value| value.is_finite()))
                        .then_some(values)
                    })
                    .collect::<Option<Vec<_>>>()
            });
        if let Some(batch_vectors) = batch_vectors {
            for (offset, vector) in batch_vectors.into_iter().enumerate() {
                vectors[start + offset] = Some(vector);
            }
        } else if end - start > 1 {
            let middle = start + (end - start) / 2;
            pending.push((middle, end, source_id));
            pending.push((start, middle, source_id));
        } else {
            bail!("protected kb_index embedding failed after bounded batch bisection");
        }
    }
    for (chunk, vector) in generation.chunks.iter_mut().zip(vectors) {
        chunk.vector = vector.context("embedding response omitted a chunk")?;
    }
    let prior_generation_id = generation.manifest.generation_id;
    generation.manifest.space_id = config.embedding_space_id.clone();
    generation.manifest.space_revision = config.embedding_space_revision;
    generation.manifest.dimension = config.embedding_dimension;
    generation.manifest.generation_id = derived_uuid(
        &format!(
            "generation:{}:{}:{}",
            config.embedding_space_id, config.embedding_space_revision, config.embedding_dimension
        ),
        prior_generation_id,
    );
    generation.manifest.segment_id = derived_uuid("base", generation.manifest.generation_id);
    generation.manifest.manifest_digest = sha256_hex(
        format!(
            "{}:{}:{}:{}",
            generation.manifest.generation_id,
            config.embedding_space_id,
            config.embedding_space_revision,
            generation
                .chunks
                .iter()
                .map(|chunk| chunk.content_digest.as_str())
                .collect::<Vec<_>>()
                .join(":")
        )
        .as_bytes(),
    );
    Ok(())
}

fn embedding_batches(
    chunks: &[knowledge_core::Chunk],
    batch_size: usize,
    fallback_source_id: Uuid,
) -> Result<Vec<(usize, usize, Uuid)>> {
    let mut batches = Vec::new();
    let mut start = 0;
    while start < chunks.len() {
        let source_id =
            source_id_from_object_id(&chunks[start].source_object_id).unwrap_or(fallback_source_id);
        if source_id.is_nil() {
            bail!("KNOWLEDGE_DOCUMENT_SOURCE_ID_UNRESOLVED");
        }
        let mut end = (start + batch_size).min(chunks.len());
        while end > start + 1
            && source_id_from_object_id(&chunks[end - 1].source_object_id)
                .unwrap_or(fallback_source_id)
                != source_id
        {
            end -= 1;
        }
        batches.push((start, end, source_id));
        start = end;
    }
    Ok(batches)
}

fn valid_repository_uri(uri: &str) -> bool {
    uri.starts_with("https://") && !uri.contains('@') && !uri.contains('\n') && !uri.contains('\r')
}

fn source_path_policy(config: &serde_json::Value) -> Result<SourcePathPolicy> {
    let includes = config
        .get("include")
        .and_then(serde_json::Value::as_array)
        .context("Knowledge source include policy is required")?;
    if includes.is_empty() {
        bail!("KNOWLEDGE_SOURCE_INCLUDE_POLICY_UNSUPPORTED");
    }
    let include_prefixes = includes
        .iter()
        .map(|value| {
            let pattern = value
                .as_str()
                .context("KNOWLEDGE_SOURCE_INCLUDE_POLICY_UNSUPPORTED")?;
            if pattern == "**/*.md" {
                return Ok(String::new());
            }
            let prefix = pattern
                .strip_suffix("/**/*.md")
                .context("KNOWLEDGE_SOURCE_INCLUDE_POLICY_UNSUPPORTED")?;
            validate_source_prefix(prefix, "KNOWLEDGE_SOURCE_INCLUDE_POLICY_UNSUPPORTED")
        })
        .collect::<Result<Vec<_>>>()?;
    let exclude_prefixes = config
        .get("exclude")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .map(|value| {
            let pattern = value
                .as_str()
                .context("Knowledge source exclude entry must be text")?;
            let prefix = pattern.strip_suffix("/**").unwrap_or(pattern);
            validate_source_prefix(prefix, "KNOWLEDGE_SOURCE_EXCLUDE_POLICY_INVALID")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(SourcePathPolicy {
        include_prefixes,
        exclude_prefixes,
    })
}

fn validate_source_prefix(prefix: &str, error_code: &str) -> Result<String> {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty()
        || prefix.starts_with('/')
        || prefix
            .split('/')
            .any(|part| part == ".." || part.is_empty() || part == ".")
    {
        bail!("{error_code}");
    }
    Ok(prefix.to_string())
}

fn source_path_matches(path: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn normalize_source_documents(
    documents: &mut Vec<DocumentInput>,
    source_id: Uuid,
    repository_uri: &str,
    immutable_commit: &str,
    include_prefixes: &[String],
    exclude_prefixes: &[String],
) {
    documents.retain(|document| {
        include_prefixes
            .iter()
            .any(|prefix| source_path_matches(&document.source_object_id, prefix))
            && !exclude_prefixes
                .iter()
                .any(|prefix| source_path_matches(&document.source_object_id, prefix))
    });
    for document in documents {
        let relative = document.source_object_id.clone();
        document.source_object_id = format!("{source_id}/{relative}");
        document.canonical_uri = format!("git+{repository_uri}@{immutable_commit}#{relative}");
    }
}

fn valid_commit(commit: &str) -> bool {
    (commit.len() == 40 || commit.len() == 64)
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn source_id_from_object_id(source_object_id: &str) -> Option<Uuid> {
    source_object_id
        .split_once('/')
        .and_then(|(source_id, _)| Uuid::parse_str(source_id).ok())
}

async fn prepare_checkout(config: &WorkerConfig) -> Result<tempfile::TempDir> {
    let checkout = tempfile::Builder::new()
        .prefix("light-knowledge-")
        .tempdir_in(&config.checkout_root)?;
    run_git(config, checkout.path(), ["init", "--quiet"]).await?;
    run_git(
        config,
        checkout.path(),
        ["remote", "add", "origin", &config.approved_repository_uri],
    )
    .await?;
    run_git(
        config,
        checkout.path(),
        [
            "fetch",
            "--quiet",
            "--depth=1",
            "--no-tags",
            "origin",
            &config.immutable_commit,
        ],
    )
    .await?;
    run_git(
        config,
        checkout.path(),
        ["checkout", "--quiet", "--detach", "FETCH_HEAD"],
    )
    .await?;
    if checkout.path().join(".gitmodules").exists() {
        bail!("Phase 1a rejects repositories containing submodule configuration");
    }
    let output = tokio::time::timeout(
        Duration::from_secs(config.maximum_checkout_seconds),
        tokio::process::Command::new("git")
            .arg("-C")
            .arg(checkout.path())
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .context("Git verification exceeded the checkout deadline")??;
    let resolved = String::from_utf8(output.stdout)?
        .trim()
        .to_ascii_lowercase();
    if !output.status.success() || resolved != config.immutable_commit.to_ascii_lowercase() {
        bail!("Git checkout did not resolve to the approved immutable commit");
    }
    Ok(checkout)
}

async fn run_git<const N: usize>(
    config: &WorkerConfig,
    checkout: &Path,
    arguments: [&str; N],
) -> Result<()> {
    let status = tokio::time::timeout(
        Duration::from_secs(config.maximum_checkout_seconds),
        tokio::process::Command::new("git")
            .arg("-c")
            .arg("core.hooksPath=/dev/null")
            .arg("-c")
            .arg("protocol.ext.allow=never")
            .arg("-c")
            .arg("protocol.file.allow=never")
            .arg("-C")
            .arg(checkout)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await
    .context("Git operation exceeded the checkout deadline")??;
    if !status.success() {
        bail!("bounded Git operation failed without releasing repository output");
    }
    Ok(())
}

struct GenerationObjects {
    manifest_locator: String,
    documents: HashMap<Uuid, (String, String, usize)>,
}

fn write_objects(
    root: &Path,
    generation: &FullBaseGeneration,
    inputs: &[knowledge_core::DocumentInput],
) -> Result<GenerationObjects> {
    let directory = root
        .join("generations")
        .join(generation.manifest.generation_id.to_string());
    fs::create_dir_all(&directory)?;
    write_immutable(
        &directory.join("base.json"),
        &serde_json::to_vec(generation)?,
    )?;
    write_immutable(
        &directory.join("manifest.json"),
        &serde_json::to_vec(&generation.manifest)?,
    )?;
    let document_directory = directory.join("documents");
    fs::create_dir_all(&document_directory)?;
    let mut documents = HashMap::new();
    for input in inputs {
        let Some(chunk) = generation
            .chunks
            .iter()
            .find(|chunk| chunk.source_object_id == input.source_object_id)
        else {
            continue;
        };
        let bytes = input.markdown.as_bytes();
        let digest = sha256_hex(bytes);
        let path = document_directory.join(format!("{}.md", chunk.document_version_id));
        write_immutable(&path, bytes)?;
        documents.insert(
            chunk.document_version_id,
            (
                format!(
                    "object://light-knowledge/generations/{}/documents/{}.md",
                    generation.manifest.generation_id, chunk.document_version_id
                ),
                digest,
                bytes.len(),
            ),
        );
    }
    Ok(GenerationObjects {
        manifest_locator: format!(
            "object://light-knowledge/generations/{}/manifest.json",
            generation.manifest.generation_id
        ),
        documents,
    })
}

fn write_immutable(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        if sha256_hex(&fs::read(path)?) != sha256_hex(bytes) {
            bail!("immutable object collision at {}", path.display());
        }
        return Ok(());
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::now_v7()));
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

async fn persist_full_base(
    pool: &PgPool,
    config: &WorkerConfig,
    generation: &FullBaseGeneration,
    objects: &GenerationObjects,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    let persistence_id = config
        .sync_run_id
        .or(config.current_job_id)
        .unwrap_or_else(Uuid::now_v7);
    let track_sync_run = config.sync_run_id.is_some() || !config.source_id.is_nil();
    if track_sync_run && config.sync_run_id.is_none() {
        sqlx::query(
            "INSERT INTO knowledge_sync_run_t(
               sync_run_id,job_id,knowledge_base_id,source_id,requested_by,
               start_watermark,snapshot_watermark,state,phase
             ) VALUES($1,$2,$3,$4,'light-knowledge-worker',$5,$5,'RUNNING','PERSISTING')",
        )
        .bind(persistence_id)
        .bind(config.current_job_id)
        .bind(config.knowledge_base_id)
        .bind(config.source_id)
        .bind(as_i64(config.snapshot_watermark as usize))
        .execute(&mut *tx)
        .await?;
    } else if track_sync_run {
        sqlx::query("UPDATE knowledge_sync_run_t SET phase='PERSISTING',progress=progress || jsonb_build_object('documentCount',$2,'chunkCount',$3),update_ts=now() WHERE sync_run_id=$1 AND state='RUNNING'")
            .bind(persistence_id)
            .bind(as_i64(generation.manifest.document_count))
            .bind(as_i64(generation.manifest.chunk_count))
            .execute(&mut *tx).await?;
    }

    let metadata = sha256_hex(b"metadata-v1");
    let acl = sha256_hex(b"uniform-scope-acl-v1");
    let contract_set = sha256_hex(b"phase1a-contract-set-v1");
    let phase1b_references_available = sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('knowledge_embedding_reference_t')::text",
    )
    .fetch_one(&mut *tx)
    .await?
    .is_some();
    let mut generation_evidence = json!({
        "fullBase": true,
        "sourceSnapshot": config.source_snapshot,
        "sourceManifestLocator": format!(
            "object://light-knowledge/generations/{}/sources.json",
            generation.manifest.generation_id
        )
    });
    if track_sync_run {
        generation_evidence["syncRunId"] = json!(persistence_id);
    } else {
        generation_evidence["buildOperationId"] = json!(persistence_id);
    }
    sqlx::query(
        "INSERT INTO knowledge_index_generation_t(
           index_generation_id,knowledge_base_id,embedding_profile_id,
           embedding_profile_revision,space_id,space_revision,dimension,
           parser_contract_digest,chunker_contract_digest,
           metadata_contract_digest,citation_contract_digest,
           acl_normalization_contract_digest,lexical_contract_digest,
           contract_set_digest,query_input_transform_version,
           snapshot_watermark,final_watermark,ordered_segment_manifest_digest,
           state,evidence
         ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,
           'query-v1',$15,$15,$16,'BUILDING',$17)
         ON CONFLICT(index_generation_id) DO NOTHING",
    )
    .bind(generation.manifest.generation_id)
    .bind(config.knowledge_base_id)
    .bind(config.embedding_profile_id)
    .bind(config.embedding_profile_revision)
    .bind(&generation.manifest.space_id)
    .bind(as_i64(generation.manifest.space_revision as usize))
    .bind(i32::try_from(generation.manifest.dimension).unwrap_or(i32::MAX))
    .bind(&generation.manifest.parser_digest)
    .bind(&generation.manifest.chunker_digest)
    .bind(&metadata)
    .bind(&generation.manifest.citation_digest)
    .bind(&acl)
    .bind(&generation.manifest.lexical_digest)
    .bind(&contract_set)
    .bind(as_i64(generation.manifest.snapshot_watermark as usize))
    .bind(&generation.manifest.manifest_digest)
    .bind(generation_evidence)
    .execute(&mut *tx)
    .await?;

    sqlx::query(
        "INSERT INTO knowledge_index_segment_t(
           index_segment_id,knowledge_base_id,index_generation_id,segment_kind,
           state,snapshot_watermark,parser_contract_digest,
           chunker_contract_digest,lexical_contract_digest,
           embedding_contract_digest,acl_contract_digest,physical_locator,
           manifest_digest,document_count,chunk_count,vector_count,acl_count
         ) VALUES($1,$2,$3,'BASE','BUILDING',$4,$5,$6,$7,$8,$9,$10,$11,
           $12,$13,$13,$12)
         ON CONFLICT(index_segment_id) DO NOTHING",
    )
    .bind(generation.manifest.segment_id)
    .bind(config.knowledge_base_id)
    .bind(generation.manifest.generation_id)
    .bind(as_i64(generation.manifest.snapshot_watermark as usize))
    .bind(&generation.manifest.parser_digest)
    .bind(&generation.manifest.chunker_digest)
    .bind(&generation.manifest.lexical_digest)
    .bind(sha256_hex(
        format!(
            "embedding:{}:{}:{}:document-v1",
            generation.manifest.space_id,
            generation.manifest.space_revision,
            generation.manifest.dimension
        )
        .as_bytes(),
    ))
    .bind(&acl)
    .bind(&objects.manifest_locator)
    .bind(&generation.manifest.manifest_digest)
    .bind(as_i64(generation.manifest.document_count))
    .bind(as_i64(generation.manifest.chunk_count))
    .execute(&mut *tx)
    .await?;

    for chunk in &generation.chunks {
        let document_source_id =
            source_id_from_object_id(&chunk.source_object_id).unwrap_or(config.source_id);
        if document_source_id.is_nil() {
            bail!("KNOWLEDGE_DOCUMENT_SOURCE_ID_UNRESOLVED");
        }
        let document_object = objects
            .documents
            .get(&chunk.document_version_id)
            .context("verified document object is missing")?;
        let artifact_id = embedding_artifact_id(config, generation, &chunk.content_digest);
        let artifact_reused = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM knowledge_embedding_artifact_t
              WHERE embedding_artifact_id=$1 AND knowledge_base_id=$2)",
        )
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_document_t(
               document_id,knowledge_base_id,source_id,source_object_id,
               canonical_uri,lifecycle_state,observed_ts
             ) VALUES($1,$2,$3,$4,$5,'ACTIVE',now())
             ON CONFLICT(document_id) DO UPDATE SET
               canonical_uri=EXCLUDED.canonical_uri,
               lifecycle_state='ACTIVE',observed_ts=now(),update_ts=now()
             WHERE knowledge_document_t.knowledge_base_id=EXCLUDED.knowledge_base_id
               AND knowledge_document_t.source_id=EXCLUDED.source_id",
        )
        .bind(chunk.document_id)
        .bind(config.knowledge_base_id)
        .bind(document_source_id)
        .bind(&chunk.source_object_id)
        .bind(&chunk.canonical_uri)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_document_version_t(
               document_version_id,document_id,knowledge_base_id,source_version,
               content_digest,parser_contract_digest,metadata_schema_version,
               object_locator,object_digest,normalized_bytes
             ) VALUES($1,$2,$3,$4,$5,$6,'v1',$7,$5,$8)
             ON CONFLICT(document_version_id) DO NOTHING",
        )
        .bind(chunk.document_version_id)
        .bind(chunk.document_id)
        .bind(config.knowledge_base_id)
        .bind(&chunk.source_version)
        .bind(&document_object.1)
        .bind(&generation.manifest.parser_digest)
        .bind(&document_object.0)
        .bind(as_i64(document_object.2))
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE knowledge_document_t SET current_document_version_id=$1
              WHERE document_id=$2",
        )
        .bind(chunk.document_version_id)
        .bind(chunk.document_id)
        .execute(&mut *tx)
        .await?;
        let existing_acl_id = sqlx::query_scalar::<_, Uuid>(
            "SELECT acl_revision_id FROM knowledge_document_acl_t
              WHERE document_id=$1 ORDER BY acl_sequence DESC LIMIT 1",
        )
        .bind(chunk.document_id)
        .fetch_optional(&mut *tx)
        .await?;
        let acl_id = existing_acl_id.unwrap_or_else(|| derived_uuid("acl", chunk.document_id));
        if existing_acl_id.is_none() {
            sqlx::query(
                "INSERT INTO knowledge_document_acl_t(
               acl_revision_id,document_id,knowledge_base_id,acl_sequence,
               visibility_mode,normalized_acl,normalization_contract_digest,
               completeness_state,observed_ts,fresh_until_ts,evidence_digest
             ) VALUES($1,$2,$3,1,'UNIFORM_SCOPE',$4,$5,'COMPLETE',
               now(),now()+interval '365 days',$6)
             ON CONFLICT(acl_revision_id) DO NOTHING",
            )
            .bind(acl_id)
            .bind(chunk.document_id)
            .bind(config.knowledge_base_id)
            .bind(json!({"uniform": true}))
            .bind(&acl)
            .bind(sha256_hex(chunk.source_object_id.as_bytes()))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO knowledge_chunk_t(
               chunk_id,knowledge_base_id,document_version_id,ordinal,
               section_path,start_offset,end_offset,chunk_text,token_count,
               content_digest,parser_output_digest,chunker_contract_digest,
               lexical_input,lexical_input_digest,metadata_schema_version
             ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,
               to_tsvector('english',$8),$13,'v1')
             ON CONFLICT(chunk_id) DO NOTHING",
        )
        .bind(chunk.chunk_id)
        .bind(config.knowledge_base_id)
        .bind(chunk.document_version_id)
        .bind(i32::try_from(chunk.ordinal).unwrap_or(i32::MAX))
        .bind(serde_json::to_value(&chunk.section_path)?)
        .bind(as_i64(chunk.start_offset))
        .bind(as_i64(chunk.end_offset))
        .bind(&chunk.text)
        .bind(i32::try_from(chunk.token_count).unwrap_or(i32::MAX))
        .bind(&chunk.content_digest)
        .bind(sha256_hex(chunk.text.as_bytes()))
        .bind(&generation.manifest.chunker_digest)
        .bind(sha256_hex(chunk.text.to_lowercase().as_bytes()))
        .execute(&mut *tx)
        .await?;
        let vector = vector_literal(&chunk.vector);
        sqlx::query(
            "INSERT INTO knowledge_embedding_artifact_t(
               embedding_artifact_id,knowledge_base_id,
               transformed_input_digest,space_id,space_revision,dimension,
               document_input_transform_version,embedding
             ) VALUES($1,$2,$3,$4,$5,$6,'document-v1',$7::vector)
             ON CONFLICT(embedding_artifact_id) DO NOTHING",
        )
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(&chunk.content_digest)
        .bind(&generation.manifest.space_id)
        .bind(as_i64(generation.manifest.space_revision as usize))
        .bind(i32::try_from(generation.manifest.dimension).unwrap_or(i32::MAX))
        .bind(&vector)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_chunk_embedding_t(
               chunk_id,embedding_artifact_id,knowledge_base_id,
               embedding_profile_id,embedding_profile_revision,request_id,reused
             ) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING",
        )
        .bind(chunk.chunk_id)
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(config.embedding_profile_id)
        .bind(config.embedding_profile_revision)
        .bind(format!("build:{persistence_id}:{}", chunk.chunk_id))
        .bind(artifact_reused)
        .execute(&mut *tx)
        .await?;
        if phase1b_references_available {
            persist_passage_anchor(
                &mut tx,
                config.knowledge_base_id,
                chunk,
                &generation.manifest.citation_digest,
            )
            .await?;
            sqlx::query(
                "INSERT INTO knowledge_embedding_reference_t(
                   embedding_artifact_id,knowledge_base_id,chunk_id,input_digest,
                   transform_contract_digest)
                 VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
            )
            .bind(artifact_id)
            .bind(config.knowledge_base_id)
            .bind(chunk.chunk_id)
            .bind(&chunk.content_digest)
            .bind(sha256_hex(b"document-v1"))
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO knowledge_segment_document_t(
               index_segment_id,document_id,knowledge_base_id,
               document_version_id,acl_revision_id
             ) VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
        )
        .bind(generation.manifest.segment_id)
        .bind(chunk.document_id)
        .bind(config.knowledge_base_id)
        .bind(chunk.document_version_id)
        .bind(acl_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_segment_chunk_t(
               index_segment_id,chunk_id,knowledge_base_id,acl_revision_id
             ) VALUES($1,$2,$3,$4) ON CONFLICT DO NOTHING",
        )
        .bind(generation.manifest.segment_id)
        .bind(chunk.chunk_id)
        .bind(config.knowledge_base_id)
        .bind(acl_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_segment_vector_t(
               index_segment_id,chunk_id,embedding_artifact_id,
               knowledge_base_id,projection,dimension
             ) VALUES($1,$2,$3,$4,$5::vector,$6) ON CONFLICT DO NOTHING",
        )
        .bind(generation.manifest.segment_id)
        .bind(chunk.chunk_id)
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(&vector)
        .bind(as_i32(generation.manifest.dimension))
        .execute(&mut *tx)
        .await?;
    }

    sqlx::query(
        "UPDATE knowledge_index_segment_t SET state='READY'
          WHERE index_segment_id=$1 AND state='BUILDING'",
    )
    .bind(generation.manifest.segment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO knowledge_generation_segment_t(
           index_generation_id,ordinal,index_segment_id
         ) VALUES($1,0,$2) ON CONFLICT DO NOTHING",
    )
    .bind(generation.manifest.generation_id)
    .bind(generation.manifest.segment_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_index_generation_t SET state='READY'
          WHERE index_generation_id=$1 AND state='BUILDING'",
    )
    .bind(generation.manifest.generation_id)
    .execute(&mut *tx)
    .await?;
    let stored_bytes = objects
        .documents
        .values()
        .try_fold(0_i64, |total, document| {
            total.checked_add(i64::try_from(document.2).ok()?)
        })
        .context("stored byte count overflow")?;
    let embedding_tokens = as_i64(
        generation
            .chunks
            .iter()
            .map(|chunk| chunk.token_count)
            .sum(),
    );
    if track_sync_run {
        sqlx::query(
            "UPDATE knowledge_sync_run_t SET phase='PERSISTED',
           index_generation_id=$2,document_count=$3,chunk_count=$4,
           source_bytes=$5,embedding_tokens=$6,stored_bytes=$5,
           progress=progress || jsonb_build_object(
             'generationState','READY','indexGenerationId',$2),
           error_summary=NULL,update_ts=now()
         WHERE sync_run_id=$1 AND state='RUNNING'",
        )
        .bind(persistence_id)
        .bind(generation.manifest.generation_id)
        .bind(as_i64(generation.manifest.document_count))
        .bind(as_i64(generation.manifest.chunk_count))
        .bind(stored_bytes)
        .bind(embedding_tokens)
        .execute(&mut *tx)
        .await?;
    }
    if config.coalesce_queued_syncs
        && let Some(current_job_id) = config.current_job_id
        && let Some(created_before) = config.coalesce_created_before
    {
        // A source sync builds a complete BASE. Only requests already queued when
        // configuration resolution began can share its immutable source snapshot;
        // later requests must remain queued for a fresh resolution and build.
        sqlx::query(
            "UPDATE knowledge_job_t SET state='SUCCEEDED',claim_token=NULL,
               lease_expires_ts=NULL,result=jsonb_build_object(
                 'coalescedIntoJobId',$2,'indexGenerationId',$3),update_ts=now()
             WHERE knowledge_base_id=$1 AND job_id<>$2 AND job_type='SYNC'
               AND state='QUEUED' AND created_ts<=$4
               AND EXISTS (
                 SELECT 1 FROM knowledge_sync_run_t run
                  WHERE run.job_id=knowledge_job_t.job_id AND run.state='QUEUED')",
        )
        .bind(config.knowledge_base_id)
        .bind(current_job_id)
        .bind(generation.manifest.generation_id)
        .bind(created_before)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE knowledge_sync_run_t run SET state='SUCCEEDED',phase='COALESCED',
               index_generation_id=$3,document_count=$4,chunk_count=$5,
               source_bytes=$6,embedding_tokens=$7,stored_bytes=$6,
               progress=run.progress || jsonb_build_object(
                 'coalescedIntoJobId',$2,'generationState','READY',
                 'indexGenerationId',$3),
               error_summary=NULL,finished_ts=now(),update_ts=now()
              FROM knowledge_job_t job
             WHERE run.job_id=job.job_id AND job.knowledge_base_id=$1
               AND job.job_id<>$2 AND job.job_type='SYNC'
               AND job.state='SUCCEEDED' AND job.created_ts<=$8
               AND job.result->>'coalescedIntoJobId'=$2::text
               AND run.state='QUEUED'",
        )
        .bind(config.knowledge_base_id)
        .bind(current_job_id)
        .bind(generation.manifest.generation_id)
        .bind(as_i64(generation.manifest.document_count))
        .bind(as_i64(generation.manifest.chunk_count))
        .bind(stored_bytes)
        .bind(embedding_tokens)
        .bind(created_before)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn persist_passage_anchor(
    tx: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    chunk: &knowledge_core::Chunk,
    citation_contract_digest: &str,
) -> Result<()> {
    let anchor_id = knowledge_core::stable_passage_anchor_id(chunk, citation_contract_digest);
    sqlx::query(
        "WITH chosen_sequence AS (
           SELECT COALESCE(
             (SELECT anchor_sequence FROM knowledge_passage_anchor_t
               WHERE passage_anchor_id=$1 AND document_version_id=$4),
             (SELECT COALESCE(max(anchor_sequence),0)+1
                FROM knowledge_passage_anchor_t WHERE document_id=$3)
           ) AS anchor_sequence
         )
         INSERT INTO knowledge_passage_anchor_t(
           passage_anchor_id,knowledge_base_id,document_id,document_version_id,
           chunk_id,anchor_contract_digest,continuity_state,anchor_sequence)
         SELECT $1,$2,$3,$4,$5,$6,'STABLE',anchor_sequence FROM chosen_sequence
         ON CONFLICT(passage_anchor_id,document_version_id) DO UPDATE SET
           chunk_id=EXCLUDED.chunk_id,continuity_state='STABLE'",
    )
    .bind(anchor_id)
    .bind(knowledge_base_id)
    .bind(chunk.document_id)
    .bind(chunk.document_version_id)
    .bind(chunk.chunk_id)
    .bind(citation_contract_digest)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn as_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn derived_uuid(namespace: &str, id: Uuid) -> Uuid {
    derived_uuid_text(&format!("{namespace}:{id}"))
}

fn derived_uuid_text(identity: &str) -> Uuid {
    let digest = sha256_hex(identity.as_bytes());
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16).unwrap_or_default();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn embedding_artifact_id(
    config: &WorkerConfig,
    generation: &FullBaseGeneration,
    transformed_input_digest: &str,
) -> Uuid {
    derived_uuid_text(&format!(
        "embedding:{}:{}:{}:{}:{}:{}",
        config.knowledge_base_id,
        transformed_input_digest,
        generation.manifest.space_id,
        generation.manifest.space_revision,
        generation.manifest.dimension,
        "document-v1"
    ))
}

fn vector_literal(vector: &[f32]) -> String {
    format!(
        "[{}]",
        vector
            .iter()
            .map(f32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    )
}

async fn project_once(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    let rows = sqlx::query(
        "SELECT e.id,e.aggregate_type,e.aggregate_id,e.aggregate_version,
                e.event_type,e.payload
           FROM event_store_t e
           LEFT JOIN knowledge_projection_inbox_t i ON i.event_id=e.id
          WHERE (e.event_type LIKE 'Knowledge%Event'
              OR e.event_type LIKE 'AgentKnowledgeBase%Event')
            AND (i.event_id IS NULL OR (i.state='GAP'
                 AND (i.next_attempt_ts IS NULL OR i.next_attempt_ts<=now())))
          ORDER BY e.event_ts,e.id LIMIT 100",
    )
    .fetch_all(pool)
    .await?;
    for row in rows {
        let cloud_event: serde_json::Value = row.get("payload");
        let payload = cloud_event
            .get("data")
            .cloned()
            .context("Portal Knowledge event requires CloudEvent data")?;
        let event = ProjectionEnvelope {
            event_id: row.get("id"),
            aggregate_type: row.get("aggregate_type"),
            aggregate_id: row.get("aggregate_id"),
            aggregate_sequence: row.get("aggregate_version"),
            event_type: row.get("event_type"),
            payload_digest: sha256_hex(&serde_json::to_vec(&cloud_event)?),
            payload,
        };
        match apply_projection_event(pool, config, event).await {
            Ok(ProjectionApplyOutcome::ParkedGap) => tracing::warn!(
                "Knowledge projection gap was parked without blocking other aggregates"
            ),
            Ok(ProjectionApplyOutcome::AppliedOrDuplicate) => {}
            Err(error) => tracing::warn!(%error,
                "Knowledge projection event failure was isolated from the remaining batch"),
        }
    }
    Ok(())
}

async fn project_loop(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    let heartbeat_pool = pool.clone();
    let heartbeat_config = config.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = heartbeat(&heartbeat_pool, &heartbeat_config).await {
                tracing::error!(%error, "Knowledge projector heartbeat failed closed");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    loop {
        if let Err(error) = project_once(pool, config).await {
            tracing::error!(%error, "Knowledge projection iteration failed closed");
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}

async fn apply_projection_event(
    pool: &PgPool,
    config: &WorkerConfig,
    event: ProjectionEnvelope,
) -> Result<ProjectionApplyOutcome> {
    let mut tx = pool.begin().await?;
    if let Some(row) = sqlx::query(
        "SELECT payload_digest,state FROM knowledge_projection_inbox_t WHERE event_id=$1",
    )
    .bind(event.event_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<String, _>("payload_digest").trim() != event.payload_digest {
            bail!("KNOWLEDGE_PROJECTION_EVENT_CONFLICT");
        }
        if row.get::<String, _>("state") != "GAP" {
            tx.rollback().await?;
            return Ok(ProjectionApplyOutcome::AppliedOrDuplicate);
        }
    }
    let previous: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(aggregate_sequence),0)
           FROM knowledge_projection_inbox_t
          WHERE aggregate_type=$1 AND aggregate_id=$2 AND state='APPLIED'",
    )
    .bind(&event.aggregate_type)
    .bind(&event.aggregate_id)
    .fetch_one(&mut *tx)
    .await?;
    let state = if event.aggregate_sequence == previous + 1 {
        "APPLIED"
    } else {
        "GAP"
    };
    if state == "APPLIED" {
        apply_desired_state(&mut tx, config, &event).await?;
    }
    sqlx::query(
        "INSERT INTO knowledge_projection_inbox_t(
           event_id,aggregate_type,aggregate_id,aggregate_sequence,event_type,
           event_ts,payload,payload_digest,state,applied_ts,last_error
         ) VALUES($1,$2,$3,$4,$5,now(),$6,$7,$8,
           CASE WHEN $8='APPLIED' THEN now() END,
           CASE WHEN $8='GAP' THEN
             jsonb_build_object('code','KNOWLEDGE_PROJECTION_SEQUENCE_GAP',
                                'expected',$9) END)
         ON CONFLICT(event_id) DO UPDATE SET
           state=EXCLUDED.state,applied_ts=EXCLUDED.applied_ts,
           last_error=EXCLUDED.last_error,attempt_count=
             knowledge_projection_inbox_t.attempt_count+1,
           next_attempt_ts=CASE WHEN EXCLUDED.state='GAP'
             THEN now()+interval '5 seconds' END",
    )
    .bind(event.event_id)
    .bind(&event.aggregate_type)
    .bind(&event.aggregate_id)
    .bind(event.aggregate_sequence)
    .bind(&event.event_type)
    .bind(&event.payload)
    .bind(&event.payload_digest)
    .bind(state)
    .bind(previous + 1)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(if state == "GAP" {
        ProjectionApplyOutcome::ParkedGap
    } else {
        ProjectionApplyOutcome::AppliedOrDuplicate
    })
}

async fn apply_desired_state(
    tx: &mut Transaction<'_, Postgres>,
    config: &WorkerConfig,
    event: &ProjectionEnvelope,
) -> Result<()> {
    let payload = &event.payload;
    match event.event_type.as_str() {
        "KnowledgeIngestionPolicyCreatedEvent" | "KnowledgeIngestionPolicyUpdatedEvent" => {
            sqlx::query(
                "INSERT INTO knowledge_ingestion_policy_t(
                   ingestion_policy_id,host_id,policy_name,max_documents,
                   max_chunks,max_source_bytes,max_stored_bytes,
                   max_embedding_tokens,max_spend_micros,max_wall_time_seconds,
                   max_concurrency,version,active,update_user
                 ) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,TRUE,
                   'light-knowledge-projector')
                 ON CONFLICT(ingestion_policy_id) DO UPDATE SET
                   policy_name=EXCLUDED.policy_name,
                   max_documents=EXCLUDED.max_documents,
                   max_chunks=EXCLUDED.max_chunks,
                   max_source_bytes=EXCLUDED.max_source_bytes,
                   max_stored_bytes=EXCLUDED.max_stored_bytes,
                   max_embedding_tokens=EXCLUDED.max_embedding_tokens,
                   max_spend_micros=EXCLUDED.max_spend_micros,
                   max_wall_time_seconds=EXCLUDED.max_wall_time_seconds,
                   max_concurrency=EXCLUDED.max_concurrency,
                   version=EXCLUDED.version,active=TRUE,update_ts=now(),
                   update_user=EXCLUDED.update_user
                 WHERE knowledge_ingestion_policy_t.host_id IS NOT DISTINCT FROM
                       EXCLUDED.host_id
                   AND knowledge_ingestion_policy_t.version<EXCLUDED.version",
            )
            .bind(uuid_value(payload, "ingestionPolicyId")?)
            .bind(optional_uuid_value(payload, "hostId")?)
            .bind(text_value(payload, "policyName")?)
            .bind(i64_value(payload, "maxDocuments")?)
            .bind(i64_value(payload, "maxChunks")?)
            .bind(i64_value(payload, "maxSourceBytes")?)
            .bind(i64_value(payload, "maxStoredBytes")?)
            .bind(i64_value(payload, "maxEmbeddingTokens")?)
            .bind(i64_value(payload, "maxSpendMicros")?)
            .bind(i64_value(payload, "maxWallTimeSeconds")?)
            .bind(i32::try_from(i64_value(payload, "maxConcurrency")?)?)
            .bind(event.aggregate_sequence)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeIngestionPolicyDeactivatedEvent" => {
            sqlx::query(
                "UPDATE knowledge_ingestion_policy_t SET active=FALSE,version=$2,
                   update_ts=now(),update_user='light-knowledge-projector'
                 WHERE ingestion_policy_id=$1 AND host_id IS NOT DISTINCT FROM $3
                   AND version<$2",
            )
            .bind(uuid_value(payload, "ingestionPolicyId")?)
            .bind(event.aggregate_sequence)
            .bind(optional_uuid_value(payload, "hostId")?)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeEmbeddingProfileCreatedEvent" => {
            sqlx::query(
                "INSERT INTO knowledge_embedding_profile_t(
                   profile_id,profile_revision,host_id,alias_owner_host_id,
                   public_alias_id,expected_space_id,expected_space_revision,
                   dimension,normalization,distance_metric,
                   document_input_transform_version,
                   query_input_transform_version,qualification_digest,
                   active,update_user)
                 VALUES($1,$2,NULL,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,TRUE,
                   'light-knowledge-projector')
                 ON CONFLICT(profile_id,profile_revision) DO NOTHING",
            )
            .bind(uuid_value(payload, "profileId")?)
            .bind(i64_value(payload, "profileRevision")?)
            .bind(uuid_value(payload, "aliasOwnerHostId")?)
            .bind(uuid_value(payload, "publicAliasId")?)
            .bind(text_value(payload, "expectedSpaceId")?)
            .bind(i64_value(payload, "expectedSpaceRevision")?)
            .bind(i32::try_from(i64_value(payload, "dimension")?)?)
            .bind(text_value(payload, "normalization")?)
            .bind(text_value(payload, "distanceMetric")?)
            .bind(text_value(payload, "documentInputTransformVersion")?)
            .bind(text_value(payload, "queryInputTransformVersion")?)
            .bind(text_value(payload, "qualificationDigest")?)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeEmbeddingProfileDeactivatedEvent" => {
            sqlx::query(
                "UPDATE knowledge_embedding_profile_t SET active=FALSE,
                   update_ts=now(),update_user='light-knowledge-projector'
                 WHERE profile_id=$1 AND profile_revision=$2 AND active",
            )
            .bind(uuid_value(payload, "profileId")?)
            .bind(i64_value(payload, "profileRevision")?)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeBaseCreatedEvent" | "KnowledgeBaseUpdatedEvent" => {
            sqlx::query(
                "INSERT INTO knowledge_base_t(
                   knowledge_base_id,host_id,name,description,environment,status,
                   desired_embedding_profile_id,desired_embedding_profile_revision,
                   retention_policy,version,update_user
                 ) VALUES($1,$2,$3,$4,$5,COALESCE($6,'DRAFT'),$7,$8,
                   COALESCE($9,'{}'::jsonb),$10,'light-knowledge-projector')
                 ON CONFLICT(knowledge_base_id) DO UPDATE SET
                   name=EXCLUDED.name,description=EXCLUDED.description,
                   status=EXCLUDED.status,
                   desired_embedding_profile_id=EXCLUDED.desired_embedding_profile_id,
                   desired_embedding_profile_revision=EXCLUDED.desired_embedding_profile_revision,
                   retention_policy=EXCLUDED.retention_policy,
                   version=EXCLUDED.version,update_ts=now(),
                   update_user=EXCLUDED.update_user
                 WHERE knowledge_base_t.host_id IS NOT DISTINCT FROM EXCLUDED.host_id
                   AND knowledge_base_t.environment=EXCLUDED.environment
                   AND knowledge_base_t.version<EXCLUDED.version",
            )
            .bind(uuid_value(payload, "knowledgeBaseId")?)
            .bind(optional_uuid_value(payload, "hostId")?)
            .bind(text_value(payload, "name")?)
            .bind(payload.get("description").and_then(|value| value.as_str()))
            .bind(text_value(payload, "environment")?)
            .bind(payload.get("status").and_then(|value| value.as_str()))
            .bind(optional_uuid_value(payload, "desiredEmbeddingProfileId")?)
            .bind(
                payload
                    .get("desiredEmbeddingProfileRevision")
                    .and_then(|value| value.as_i64()),
            )
            .bind(payload.get("retentionPolicy"))
            .bind(event.aggregate_sequence)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeBaseDeactivatedEvent" | "KnowledgeBaseDeletedEvent" => {
            let status = if event.event_type == "KnowledgeBaseDeletedEvent" {
                "DELETED"
            } else {
                "INACTIVE"
            };
            sqlx::query("UPDATE knowledge_base_t SET status=$2,version=$3,update_ts=now(),update_user='light-knowledge-projector' WHERE knowledge_base_id=$1 AND environment=$4 AND host_id IS NOT DISTINCT FROM $5 AND version<$3")
                .bind(uuid_value(payload, "knowledgeBaseId")?)
                .bind(status)
                .bind(event.aggregate_sequence)
                .bind(text_value(payload, "environment")?)
                .bind(optional_uuid_value(payload, "hostId")?)
                .execute(&mut **tx).await?;
        }
        "KnowledgeSourceCreatedEvent" | "KnowledgeSourceUpdatedEvent" => {
            sqlx::query(
                "INSERT INTO knowledge_source_t(
                   source_id,knowledge_base_id,source_type,display_name,
                   config_json,secret_reference,status,acl_mode,
                   source_trust_tier,approval_policy,schedule,
                   acl_reconciliation_policy,ingestion_policy_id,version,update_user
                 ) VALUES($1,$2,$3,$4,COALESCE($5,'{}'::jsonb),$6,
                   COALESCE($7,'DRAFT'),COALESCE($8,'UNIFORM_SCOPE'),
                   COALESCE($9,'UNREVIEWED'),COALESCE($10,'{}'::jsonb),
                   COALESCE($11,'{}'::jsonb),COALESCE($12,'{}'::jsonb),
                   $13,$14,'light-knowledge-projector')
                 ON CONFLICT(source_id) DO UPDATE SET
                   display_name=EXCLUDED.display_name,config_json=EXCLUDED.config_json,
                   secret_reference=EXCLUDED.secret_reference,status=EXCLUDED.status,
                   acl_mode=EXCLUDED.acl_mode,source_trust_tier=EXCLUDED.source_trust_tier,
                   approval_policy=EXCLUDED.approval_policy,schedule=EXCLUDED.schedule,
                   acl_reconciliation_policy=EXCLUDED.acl_reconciliation_policy,
                   ingestion_policy_id=EXCLUDED.ingestion_policy_id,
                   version=EXCLUDED.version,update_ts=now(),update_user=EXCLUDED.update_user
                 WHERE knowledge_source_t.knowledge_base_id=EXCLUDED.knowledge_base_id
                   AND knowledge_source_t.version<EXCLUDED.version",
            )
            .bind(uuid_value(payload, "sourceId")?)
            .bind(uuid_value(payload, "knowledgeBaseId")?)
            .bind(text_value(payload, "sourceType")?)
            .bind(text_value(payload, "displayName")?)
            .bind(payload.get("configJson"))
            .bind(
                payload
                    .get("secretReference")
                    .and_then(|value| value.as_str()),
            )
            .bind(payload.get("status").and_then(|value| value.as_str()))
            .bind(payload.get("aclMode").and_then(|value| value.as_str()))
            .bind(
                payload
                    .get("sourceTrustTier")
                    .and_then(|value| value.as_str()),
            )
            .bind(payload.get("approvalPolicy"))
            .bind(payload.get("schedule"))
            .bind(payload.get("aclReconciliationPolicy"))
            .bind(uuid_value(payload, "ingestionPolicyId")?)
            .bind(event.aggregate_sequence)
            .execute(&mut **tx)
            .await?;
        }
        "KnowledgeSourceDeactivatedEvent" | "KnowledgeSourceDeletedEvent" => {
            let status = if event.event_type == "KnowledgeSourceDeletedEvent" {
                "DELETED"
            } else {
                "INACTIVE"
            };
            sqlx::query("UPDATE knowledge_source_t SET status=$2,version=$3,update_ts=now(),update_user='light-knowledge-projector' WHERE source_id=$1 AND version<$3 AND knowledge_base_id=$4 AND EXISTS(SELECT 1 FROM knowledge_base_t b WHERE b.knowledge_base_id=knowledge_source_t.knowledge_base_id AND b.environment=$5 AND b.host_id IS NOT DISTINCT FROM $6)")
                .bind(uuid_value(payload, "sourceId")?).bind(status)
                .bind(event.aggregate_sequence)
                .bind(uuid_value(payload, "knowledgeBaseId")?)
                .bind(text_value(payload, "environment")?)
                .bind(optional_uuid_value(payload, "hostId")?)
                .execute(&mut **tx).await?;
        }
        "AgentKnowledgeBaseBoundEvent" | "AgentKnowledgeBaseBindingUpdatedEvent" => {
            apply_binding(tx, config, event, true).await?;
        }
        "AgentKnowledgeBaseUnboundEvent" => {
            apply_binding(tx, config, event, false).await?;
        }
        "KnowledgeSourceSyncRequestedEvent"
        | "KnowledgeSourceAclReconciliationRequestedEvent"
        | "KnowledgeSourceProviderNotificationReceivedEvent"
        | "KnowledgeSourceConnectivityTestRequestedEvent"
        | "KnowledgeBaseReindexRequestedEvent"
        | "KnowledgeBaseCompactionRequestedEvent"
        | "KnowledgeBaseIndexGenerationPromotionRequestedEvent"
        | "KnowledgeBasePurgeRequestedEvent"
        | "KnowledgeBaseRetrievalTestRequestedEvent"
        | "KnowledgeBaseEmbeddingMigrationRequestedEvent"
        | "KnowledgeBaseEmbeddingMigrationPausedEvent"
        | "KnowledgeBaseEmbeddingMigrationResumedEvent"
        | "KnowledgeBaseEmbeddingMigrationCancelledEvent"
        | "KnowledgeBaseIndexGenerationRollbackRequestedEvent"
        | "KnowledgeBaseIndexGenerationRetirementRequestedEvent"
        | "KnowledgeBaseBackupCheckpointRequestedEvent"
        | "KnowledgeBasePhysicalRestoreVerificationRequestedEvent" => {
            let enterprise_source = if event.event_type == "KnowledgeSourceSyncRequestedEvent" {
                let source_type = sqlx::query_scalar::<_, String>(
                    "SELECT source_type FROM knowledge_source_t WHERE source_id=$1",
                )
                .bind(uuid_value(payload, "sourceId")?)
                .fetch_one(&mut **tx)
                .await?;
                matches!(source_type.as_str(), "SHAREPOINT" | "CONFLUENCE")
            } else {
                false
            };
            let job_type = if event.event_type
                == "KnowledgeBaseIndexGenerationPromotionRequestedEvent"
                && payload.get("migrationId").is_some()
            {
                "MIGRATION_PROMOTE"
            } else if event.event_type == "KnowledgeBaseRetrievalTestRequestedEvent"
                && payload.get("migrationId").is_some()
            {
                "MIGRATION_VALIDATE"
            } else {
                projected_job_type(&event.event_type, enterprise_source)
                    .context("projected Knowledge event has no job route")?
            };
            let knowledge_base_id = uuid_value(payload, "knowledgeBaseId")?;
            let source_id = optional_uuid_value(payload, "sourceId")?;
            sqlx::query("INSERT INTO knowledge_job_t(job_id,knowledge_base_id,source_id,job_type,idempotency_key,requested_by,payload,state,result) VALUES($1,$2,$3,$4,$5,'portal-event',$6,'QUEUED',$7) ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING")
                .bind(event.event_id)
                .bind(knowledge_base_id)
                .bind(source_id)
                .bind(job_type).bind(event.event_id.to_string()).bind(payload)
                .bind(Option::<serde_json::Value>::None)
                .execute(&mut **tx).await?;
            if event.event_type == "KnowledgeSourceSyncRequestedEvent" {
                let source_id = source_id.context("source sync event requires sourceId")?;
                sqlx::query(
                    "INSERT INTO knowledge_sync_run_t(
                       sync_run_id,job_id,request_event_id,knowledge_base_id,
                       source_id,requested_by,start_watermark,state,phase,progress)
                     VALUES($1,$1,$1,$2,$3,'portal-event',$4,'QUEUED','QUEUED',
                       jsonb_build_object('requestEventType',$5))
                     ON CONFLICT(sync_run_id) DO NOTHING",
                )
                .bind(event.event_id)
                .bind(knowledge_base_id)
                .bind(source_id)
                // The portal event sequence is not a corpus watermark. The worker
                // resolves the base snapshot independently when it claims the job.
                .bind(initial_sync_start_watermark())
                .bind(&event.event_type)
                .execute(&mut **tx)
                .await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn apply_binding(
    tx: &mut Transaction<'_, Postgres>,
    config: &WorkerConfig,
    event: &ProjectionEnvelope,
    active: bool,
) -> Result<()> {
    let payload = &event.payload;
    let host_id = uuid_value(payload, "hostId")?;
    let agent_id = uuid_value(payload, "agentId")?;
    let knowledge_base_id = uuid_value(payload, "knowledgeBaseId")?;
    let environment = text_value(payload, "environment")?;
    if active {
        let profile_id = uuid_value(payload, "retrievalProfileId")?;
        sqlx::query("INSERT INTO agent_knowledge_base_t(host_id,agent_id,knowledge_base_id,environment,retrieval_profile_id,priority,evidence_required,allowed_source_trust_tiers,version,active,update_user) VALUES($1,$2,$3,$4,$5,COALESCE($6,50),COALESCE($7,FALSE),COALESCE($8,'[]'::jsonb),$9,TRUE,'light-knowledge-projector') ON CONFLICT(host_id,agent_id,knowledge_base_id,environment) DO UPDATE SET retrieval_profile_id=EXCLUDED.retrieval_profile_id,priority=EXCLUDED.priority,evidence_required=EXCLUDED.evidence_required,allowed_source_trust_tiers=EXCLUDED.allowed_source_trust_tiers,version=EXCLUDED.version,active=TRUE,update_ts=now(),update_user=EXCLUDED.update_user WHERE agent_knowledge_base_t.version<EXCLUDED.version")
            .bind(host_id).bind(agent_id).bind(knowledge_base_id).bind(environment)
            .bind(profile_id).bind(payload.get("priority").and_then(|value| value.as_i64()))
            .bind(payload.get("evidenceRequired").and_then(|value| value.as_bool()))
            .bind(payload.get("allowedSourceTrustTiers"))
            .bind(event.aggregate_sequence).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO knowledge_runtime_authorization_t(knowledge_base_id,consumer_host_id,environment,agent_id,retrieval_profile_id,active,desired_event_sequence,applied_event_sequence,projector_id,lease_expires_ts,authorization_digest) VALUES($1,$2,$3,$4,$5,TRUE,$6,$6,$7,now()+interval '30 seconds',$8) ON CONFLICT(knowledge_base_id,consumer_host_id,environment,agent_id) DO UPDATE SET retrieval_profile_id=EXCLUDED.retrieval_profile_id,active=TRUE,desired_event_sequence=EXCLUDED.desired_event_sequence,applied_event_sequence=EXCLUDED.applied_event_sequence,projector_id=EXCLUDED.projector_id,lease_expires_ts=EXCLUDED.lease_expires_ts,authorization_digest=EXCLUDED.authorization_digest,update_ts=now() WHERE knowledge_runtime_authorization_t.applied_event_sequence<EXCLUDED.applied_event_sequence")
            .bind(knowledge_base_id).bind(host_id).bind(environment).bind(agent_id)
            .bind(profile_id).bind(event.aggregate_sequence).bind(&config.projector_id)
            .bind(&event.payload_digest).execute(&mut **tx).await?;
        sqlx::query(
            "INSERT INTO knowledge_consumer_quota_t(
               knowledge_base_id,consumer_host_id,max_concurrency,
               requests_per_minute,max_cost_micros_per_day,active
             ) VALUES($1,$2,4,120,1000000,TRUE)
             ON CONFLICT(knowledge_base_id,consumer_host_id) DO UPDATE SET
               active=TRUE,update_ts=now()",
        )
        .bind(knowledge_base_id)
        .bind(host_id)
        .execute(&mut **tx)
        .await?;
    } else {
        sqlx::query("UPDATE agent_knowledge_base_t SET active=FALSE,version=$5,update_ts=now(),update_user='light-knowledge-projector' WHERE host_id=$1 AND agent_id=$2 AND knowledge_base_id=$3 AND environment=$4 AND version<$5")
            .bind(host_id).bind(agent_id).bind(knowledge_base_id).bind(environment)
            .bind(event.aggregate_sequence).execute(&mut **tx).await?;
        sqlx::query("UPDATE knowledge_runtime_authorization_t SET active=FALSE,desired_event_sequence=$5,applied_event_sequence=$5,lease_expires_ts=now()+interval '30 seconds',authorization_digest=$6,update_ts=now() WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND environment=$3 AND agent_id=$4 AND applied_event_sequence<$5")
            .bind(knowledge_base_id).bind(host_id).bind(environment).bind(agent_id)
            .bind(event.aggregate_sequence).bind(&event.payload_digest)
            .execute(&mut **tx).await?;
    }
    Ok(())
}

fn text_value<'a>(value: &'a serde_json::Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .with_context(|| format!("projection payload requires {field}"))
}

fn i64_value(value: &serde_json::Value, field: &str) -> Result<i64> {
    value
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .with_context(|| format!("{field} is required"))
}

fn uuid_value(value: &serde_json::Value, field: &str) -> Result<Uuid> {
    Uuid::parse_str(text_value(value, field)?)
        .with_context(|| format!("projection payload {field} is not a UUID"))
}

fn optional_uuid_value(value: &serde_json::Value, field: &str) -> Result<Option<Uuid>> {
    value
        .get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| {
            Uuid::parse_str(value)
                .with_context(|| format!("projection payload {field} is not a UUID"))
        })
        .transpose()
}

async fn heartbeat(pool: &PgPool, config: &WorkerConfig) -> Result<()> {
    let mut tx = pool.begin().await?;
    let sequence: i64 = sqlx::query_scalar(
        "SELECT COALESCE(max(aggregate_sequence),0)
           FROM knowledge_projection_inbox_t WHERE state='APPLIED'",
    )
    .fetch_one(&mut *tx)
    .await?;
    let config_digest =
        sha256_hex(format!("{}:{}:{sequence}", config.projector_id, config.environment).as_bytes());
    let secret = fs::read(&config.heartbeat_secret_file)?;
    let mut signer = Hmac::<Sha256>::new_from_slice(&secret)
        .context("heartbeat signing secret must be non-empty")?;
    signer.update(config_digest.as_bytes());
    let signature_digest = signer
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    sqlx::query(
        "INSERT INTO knowledge_projection_heartbeat_t(
           projector_id,applied_event_sequence,effective_config_digest,
           signature_digest,lease_expires_ts
         ) VALUES($1,$2,$3,$4,now()+interval '30 seconds')
         ON CONFLICT(projector_id) DO UPDATE SET
           applied_event_sequence=EXCLUDED.applied_event_sequence,
           effective_config_digest=EXCLUDED.effective_config_digest,
           signature_digest=EXCLUDED.signature_digest,
           lease_expires_ts=EXCLUDED.lease_expires_ts,update_ts=now()",
    )
    .bind(&config.projector_id)
    .bind(sequence)
    .bind(config_digest)
    .bind(signature_digest)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE knowledge_runtime_authorization_t
            SET lease_expires_ts=now()+interval '30 seconds',update_ts=now()
          WHERE projector_id=$1 AND active=TRUE
            AND desired_event_sequence=applied_event_sequence",
    )
    .bind(&config.projector_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn embedding_gateway_response(
        billed_cost_micros: u64,
        vector_count: usize,
        dimension: usize,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response_body = serde_json::to_vec(&json!({
            "data": (0..vector_count)
                .map(|index| json!({
                    "index": index,
                    "embedding": vec![0.0_f32; dimension]
                }))
                .collect::<Vec<_>>()
        }))
        .unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "embedding request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                break (header_end, content_length);
            };
            while request.len() < header_end + 4 + content_length {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "embedding request ended before its body");
                request.extend_from_slice(&buffer[..read]);
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let ceiling = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("x-light-maximum-billed-cost-micros")
                        .then(|| value.trim().to_string())
                })
                .expect("embedding request must carry its cost ceiling");
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-light-embedding-space-id: free-space\r\nx-light-embedding-space-revision: 1\r\nx-light-billed-cost-micros: {billed_cost_micros}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
            ceiling
        });
        (format!("http://{address}/v1/embeddings"), task)
    }

    async fn embedding_gateway_error_response(
        status: u16,
        code: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            let (header_end, content_length) = loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "embedding request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    break (header_end, content_length);
                }
            };
            while request.len() < header_end + 4 + content_length {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "embedding request ended before its body");
                request.extend_from_slice(&buffer[..read]);
            }
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let ceiling = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("x-light-maximum-billed-cost-micros")
                        .then(|| value.trim().to_string())
                })
                .expect("embedding request must carry its cost ceiling");
            let response_body = serde_json::to_vec(&json!({
                "error": {
                    "message": "The request budget is exhausted",
                    "type": code,
                    "code": code
                }
            }))
            .unwrap();
            let response = format!(
                "HTTP/1.1 {status} Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.write_all(&response_body).await.unwrap();
            ceiling
        });
        (format!("http://{address}/v1/embeddings"), task)
    }

    fn free_embedding_config_and_generation(
        endpoint: String,
    ) -> (tempfile::TempDir, WorkerConfig, FullBaseGeneration) {
        let source_id = Uuid::from_u128(1);
        let policy_id = Uuid::from_u128(2);
        let directory = tempfile::tempdir().unwrap();
        let authorization_file = directory.path().join("authorization");
        fs::write(&authorization_file, "free-model-token").unwrap();
        let mut source = resolved_source(source_id, policy_id, 10);
        source.maximum_spend_micros = 0;
        let mut config = serde_json::from_value::<WorkerConfig>(json!({
            "version": 1,
            "heartbeatSecretFile": directory.path().join("heartbeat"),
            "projectorId": "test",
            "deterministicPilot": false
        }))
        .unwrap();
        config.embedding_gateway_url = Some(endpoint);
        config.embedding_authorization_file = Some(authorization_file);
        config.embedding_alias = "kb-index".into();
        config.embedding_batch_size = 128;
        config.embedding_space_id = "free-space".into();
        config.embedding_space_revision = 1;
        config.embedding_dimension = 2;
        config.source_id = source_id;
        config.maximum_spend_micros = 0;
        config.resolved_sources = vec![source];
        let documents = vec![DocumentInput {
            source_object_id: format!("{source_id}/free.md"),
            canonical_uri: "repo://free.md".into(),
            source_version: "1".into(),
            markdown: "# Free model\nA zero-cost embedding.".into(),
        }];
        let generation = build_full_base(
            Uuid::from_u128(3),
            1,
            &documents,
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        (directory, config, generation)
    }

    #[test]
    fn embedding_tls_is_secure_by_default_and_rejects_an_unreadable_ca_bundle() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = serde_json::from_value::<WorkerConfig>(json!({
            "version": 1,
            "heartbeatSecretFile": directory.path().join("heartbeat"),
            "projectorId": "test"
        }))
        .unwrap();
        assert!(config.embedding_gateway_verify_hostname);

        let missing_ca = directory.path().join("missing-ca.pem");
        config.embedding_gateway_ca_cert_file = Some(missing_ca.clone());
        let error = embedding_http_client(&config).unwrap_err();
        assert!(error.to_string().contains(&format!(
            "load embedding gateway CA bundle {}",
            missing_ca.display()
        )));
    }

    fn resolved_source(
        source_id: Uuid,
        policy_id: Uuid,
        maximum_chunks: usize,
    ) -> ResolvedSourceConfig {
        ResolvedSourceConfig {
            source_id,
            source_type: "GIT_MARKDOWN".into(),
            approved_repository_uri: "https://github.com/networknt/light-fabric.git".into(),
            immutable_commit: "a".repeat(40),
            source_include_prefixes: vec![String::new()],
            source_exclude_prefixes: vec!["target".into()],
            ingestion_policy_id: policy_id,
            ingestion_policy_version: 3,
            limits: SourceLimits {
                maximum_documents: 10,
                maximum_source_bytes: 1024,
                maximum_chunks,
                maximum_embedding_tokens: 1000,
            },
            maximum_stored_bytes: 2048,
            maximum_spend_micros: 5000,
            maximum_wall_time_seconds: 60,
            maximum_concurrency: 2,
            maximum_provider_calls: DEFAULT_MAXIMUM_PROVIDER_CALLS,
        }
    }

    #[test]
    fn immutable_object_rejects_collision() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("object");
        write_immutable(&path, b"one").unwrap();
        write_immutable(&path, b"one").unwrap();
        assert!(write_immutable(&path, b"two").is_err());
    }

    #[test]
    fn object_locator_maps_to_configured_root_and_rejects_traversal() {
        let root = Path::new("/var/lib/light-knowledge/objects");
        assert_eq!(
            object_locator_path(
                root,
                "object://light-knowledge/generations/generation/documents/document.md"
            )
            .unwrap(),
            root.join("generations/generation/documents/document.md")
        );
        assert!(object_locator_path(root, "object://light-knowledge/../../etc/passwd").is_err());
        assert!(object_locator_path(root, "/etc/passwd").is_err());
    }

    #[test]
    fn ordered_segment_digest_detects_manifest_drift() {
        let base = "a".repeat(64);
        let delta = "b".repeat(64);
        let observed = append_ordered_segment_digest(Some(&base), &delta, 1);
        assert_eq!(append_ordered_segment_digest(None, &base, 0), base);
        assert_eq!(
            observed,
            sha256_hex(format!("{}:{delta}:1", "a".repeat(64)).as_bytes())
        );
        assert_ne!(
            observed,
            append_ordered_segment_digest(Some(&"a".repeat(64)), &"c".repeat(64), 1)
        );
    }

    #[test]
    fn every_projected_job_type_has_a_claiming_worker_lane() {
        let events = [
            ("KnowledgeSourceSyncRequestedEvent", false),
            ("KnowledgeSourceSyncRequestedEvent", true),
            ("KnowledgeSourceAclReconciliationRequestedEvent", true),
            ("KnowledgeSourceProviderNotificationReceivedEvent", true),
            ("KnowledgeSourceConnectivityTestRequestedEvent", false),
            ("KnowledgeBaseReindexRequestedEvent", false),
            ("KnowledgeBaseCompactionRequestedEvent", false),
            ("KnowledgeBaseIndexGenerationPromotionRequestedEvent", false),
            ("KnowledgeBaseRetrievalTestRequestedEvent", false),
            ("KnowledgeBasePurgeRequestedEvent", false),
            ("KnowledgeBaseEmbeddingMigrationRequestedEvent", false),
            ("KnowledgeBaseEmbeddingMigrationPausedEvent", false),
            ("KnowledgeBaseEmbeddingMigrationResumedEvent", false),
            ("KnowledgeBaseEmbeddingMigrationCancelledEvent", false),
            ("KnowledgeBaseIndexGenerationRollbackRequestedEvent", false),
            (
                "KnowledgeBaseIndexGenerationRetirementRequestedEvent",
                false,
            ),
            ("KnowledgeBaseBackupCheckpointRequestedEvent", false),
            (
                "KnowledgeBasePhysicalRestoreVerificationRequestedEvent",
                false,
            ),
        ];
        let claimed = PRIORITY_JOB_TYPES
            .iter()
            .chain(BULK_JOB_TYPES)
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        for (event, enterprise_source) in events {
            let produced = projected_job_type(event, enterprise_source).unwrap();
            assert!(claimed.contains(produced), "{produced} is never claimed");
        }
    }

    #[test]
    fn full_reindex_fetches_sources_while_compaction_only_resolves_policy_budgets() {
        assert!(job_fetches_full_base_sources("FULL_REINDEX"));
        assert!(job_fetches_full_base_sources("SYNC"));
        assert!(!job_fetches_full_base_sources("COMPACTION"));
        assert!(!job_fetches_full_base_sources("UPLOAD"));
        assert!(job_coalesces_queued_syncs("SYNC"));
        assert!(!job_coalesces_queued_syncs("FULL_REINDEX"));
        assert!(!job_coalesces_queued_syncs("COMPACTION"));
        assert!(!job_coalesces_queued_syncs("UPLOAD"));
    }

    #[test]
    fn sync_start_watermark_is_independent_of_portal_event_sequence() {
        assert_eq!(initial_sync_start_watermark(), 0);
    }

    #[test]
    fn source_path_policy_is_bounded_and_source_identity_is_namespaced() {
        assert_eq!(
            source_path_policy(&json!({
                "include": ["**/*.md"],
                "exclude": ["target/**", "private.md"]
            }))
            .unwrap(),
            SourcePathPolicy {
                include_prefixes: vec![String::new()],
                exclude_prefixes: vec!["target".into(), "private.md".into()]
            }
        );
        assert_eq!(
            source_path_policy(&json!({"include": ["src/**/*.md"]})).unwrap(),
            SourcePathPolicy {
                include_prefixes: vec!["src".into()],
                exclude_prefixes: Vec::new()
            }
        );
        assert!(
            source_path_policy(&json!({
                "include": ["**/*"], "exclude": []
            }))
            .is_err()
        );
        let source_id = Uuid::from_u128(2);
        assert_eq!(
            source_id_from_object_id(&format!("{source_id}/docs/index.md")),
            Some(source_id)
        );

        let mut documents = vec![
            DocumentInput {
                source_object_id: "src/guide.md".into(),
                canonical_uri: "repo://src/guide.md".into(),
                source_version: "1".into(),
                markdown: "# Guide".into(),
            },
            DocumentInput {
                source_object_id: "docs/ignored.md".into(),
                canonical_uri: "repo://docs/ignored.md".into(),
                source_version: "1".into(),
                markdown: "# Ignored".into(),
            },
        ];
        normalize_source_documents(
            &mut documents,
            source_id,
            "https://example.invalid/repo.git",
            &"a".repeat(40),
            &["src".into()],
            &[],
        );
        assert_eq!(documents.len(), 1);
        assert_eq!(
            documents[0].source_object_id,
            format!("{source_id}/src/guide.md")
        );
    }

    #[test]
    fn platform_caps_only_tighten_selected_policy_limits() {
        assert_eq!(cap(100_usize, Some(25)), 25);
        assert_eq!(cap(10_usize, Some(25)), 10);
        assert_eq!(cap(10_u64, None), 10);
        assert!(
            PlatformCaps {
                maximum_concurrency: Some(0),
                ..PlatformCaps::default()
            }
            .validate()
            .is_err()
        );
        assert!(
            PlatformCaps {
                maximum_provider_calls: Some(0),
                ..PlatformCaps::default()
            }
            .validate()
            .is_err()
        );
        let first = resolved_source(Uuid::from_u128(1), Uuid::from_u128(11), 12);
        let second = resolved_source(Uuid::from_u128(2), Uuid::from_u128(22), 34);
        assert_eq!(
            aggregate_wall_time_seconds(&[first.clone(), second.clone()], None).unwrap(),
            120
        );
        assert_eq!(
            aggregate_wall_time_seconds(&[first, second], Some(90)).unwrap(),
            90
        );
    }

    #[tokio::test]
    async fn headless_runtime_values_override_platform_cap_template() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(
            directory.path().join("startup.yml"),
            "serviceId: com.networknt.light-knowledge-worker-1.0.0\nenvTag: test\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("server.yml"),
            "ip: 127.0.0.1\nhttpPort: 0\nenableHttp: false\nhttpsPort: 0\nenableHttps: false\nserviceId: com.networknt.light-knowledge-worker-1.0.0\nenableRegistry: false\nenvironment: test\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("values.yml"),
            "knowledgeWorker.platformCaps.maximumDocuments: 7\n",
        )
        .unwrap();
        fs::write(
            directory.path().join("worker.yml"),
            "version: 1\nheartbeatSecretFile: /tmp/heartbeat\nprojectorId: test\nplatformCaps:\n  maximumDocuments: ${knowledgeWorker.platformCaps.maximumDocuments:100}\n",
        )
        .unwrap();
        let runtime = LightRuntimeBuilder::new(HeadlessTransport)
            .with_config_dir(directory.path())
            .with_external_config_dir(directory.path())
            .build()
            .prepare_local_config()
            .await
            .unwrap();
        let config = runtime
            .module_registry
            .load_config::<WorkerConfig>(&runtime, "worker.yml")
            .unwrap();
        assert_eq!(config.platform_caps.maximum_documents, Some(7));
    }

    #[test]
    fn global_and_same_tenant_policies_are_visible_but_other_owners_fail_closed() {
        let tenant = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        assert!(policy_owner_allowed(true, None, Some(tenant)));
        assert!(policy_owner_allowed(true, Some(tenant), Some(tenant)));
        assert!(!policy_owner_allowed(true, Some(other), Some(tenant)));
        assert!(!policy_owner_allowed(false, None, Some(tenant)));
    }

    #[test]
    fn all_source_policy_versions_and_effective_ceilings_are_snapshotted() {
        let first = resolved_source(Uuid::from_u128(1), Uuid::from_u128(11), 12);
        let second = resolved_source(Uuid::from_u128(2), Uuid::from_u128(22), 34);
        let snapshot = source_snapshots(&[first, second]);
        let sources = snapshot.as_array().unwrap();
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0]["ingestionPolicyVersion"], 3);
        assert_eq!(sources[0]["effectiveCeilings"]["maxChunks"], 12);
        assert_eq!(sources[0]["sourceType"], "GIT_MARKDOWN");
        assert_eq!(
            sources[0]["effectiveCeilings"]["maxProviderCalls"],
            DEFAULT_MAXIMUM_PROVIDER_CALLS
        );
        assert_eq!(sources[1]["effectiveCeilings"]["maxChunks"], 34);

        let mut connector = resolved_source(Uuid::from_u128(3), Uuid::from_u128(33), 56);
        connector.source_type = "SHAREPOINT".into();
        connector.approved_repository_uri.clear();
        connector.immutable_commit.clear();
        let connector_snapshot = source_snapshots(&[connector]);
        assert!(connector_snapshot[0]["repositoryUri"].is_null());
        assert!(connector_snapshot[0]["immutableCommit"].is_null());
        assert_eq!(connector_snapshot[0]["effectiveCeilings"]["maxChunks"], 56);
    }

    #[test]
    fn immutable_full_base_source_snapshot_does_not_include_trigger_identity() {
        let snapshot = full_base_source_snapshot(vec![json!({
            "sourceId": Uuid::from_u128(1),
            "documentCount": 2
        })]);
        assert!(snapshot.get("triggerSourceId").is_none());
        assert_eq!(snapshot["sources"][0]["documentCount"], 2);
    }

    #[test]
    fn embedding_batches_never_mix_source_spend_budgets() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let documents = vec![
            DocumentInput {
                source_object_id: format!("{first}/one.md"),
                canonical_uri: "repo://one.md".into(),
                source_version: "1".into(),
                markdown: "# One\nalpha beta gamma".into(),
            },
            DocumentInput {
                source_object_id: format!("{second}/two.md"),
                canonical_uri: "repo://two.md".into(),
                source_version: "1".into(),
                markdown: "# Two\ndelta epsilon zeta".into(),
            },
        ];
        let generation = build_full_base(
            Uuid::from_u128(9),
            1,
            &documents,
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        let batches = embedding_batches(&generation.chunks, 128, Uuid::nil()).unwrap();
        assert_eq!(batches.len(), 2);
        for (start, end, source_id) in batches {
            assert!(generation.chunks[start..end].iter().all(|chunk| {
                source_id_from_object_id(&chunk.source_object_id) == Some(source_id)
            }));
        }
    }

    #[tokio::test]
    async fn zero_spend_ceiling_is_forwarded_for_a_free_embedding_route() {
        let (endpoint, request) = embedding_gateway_response(0, 1, 2).await;
        let (_directory, config, mut generation) = free_embedding_config_and_generation(endpoint);
        assert_eq!(generation.chunks.len(), 1);

        apply_configured_embeddings(&config, &mut generation)
            .await
            .unwrap();

        assert_eq!(request.await.unwrap(), "0");
        assert_eq!(generation.chunks[0].vector, vec![0.0, 0.0]);
    }

    #[tokio::test]
    async fn zero_spend_ceiling_rejects_positive_billed_cost_evidence() {
        let (endpoint, request) = embedding_gateway_response(1, 1, 2).await;
        let (_directory, config, mut generation) = free_embedding_config_and_generation(endpoint);

        let error = apply_configured_embeddings(&config, &mut generation)
            .await
            .unwrap_err();

        assert_eq!(request.await.unwrap(), "0");
        assert!(
            error
                .to_string()
                .contains("embedding gateway exceeded the ingestion spend budget")
        );
        assert_eq!(
            worker_error_code(&error),
            "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED",
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn gateway_budget_rejection_preserves_spend_budget_classification() {
        let (endpoint, request) = embedding_gateway_error_response(429, "budget_exhausted").await;
        let (_directory, config, mut generation) = free_embedding_config_and_generation(endpoint);

        let error = apply_configured_embeddings(&config, &mut generation)
            .await
            .unwrap_err();

        assert_eq!(request.await.unwrap(), "0");
        assert_eq!(
            worker_error_code(&error),
            "KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED",
            "{error:#}"
        );
        assert_eq!(
            budget_terminal_state(worker_error_code(&error)),
            "PAUSED_BUDGET"
        );
    }

    #[test]
    fn unsupported_claimed_jobs_have_an_operator_visible_terminal_code() {
        let unsupported = anyhow::anyhow!("KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED:RETRIEVAL_TEST");
        assert_eq!(
            worker_error_code(&unsupported),
            "KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!("connection failed")),
            "KNOWLEDGE_BUILD_FAILED"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID"
            )),
            "KNOWLEDGE_SOURCE_IMMUTABLE_GIT_CONFIG_INVALID"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE"
            )),
            "KNOWLEDGE_JOB_SOURCE_OR_POLICY_UNAVAILABLE"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_JOB_EMBEDDING_PROFILE_UNAVAILABLE"
            )),
            "KNOWLEDGE_JOB_EMBEDDING_PROFILE_UNAVAILABLE"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_SOURCE_LIMIT_EXCEEDED: maximum_chunks"
            )),
            "KNOWLEDGE_INGESTION_MAX_CHUNKS_EXCEEDED"
        );
        assert!(is_budget_error_code(
            "KNOWLEDGE_INGESTION_MAX_WALL_TIME_EXCEEDED"
        ));
        assert!(!is_budget_error_code("KNOWLEDGE_BUILD_FAILED"));
        assert_eq!(
            budget_terminal_state("KNOWLEDGE_INGESTION_SPEND_BUDGET_EXCEEDED"),
            "PAUSED_BUDGET"
        );
        assert_eq!(
            budget_terminal_state("KNOWLEDGE_INGESTION_MAX_CHUNKS_EXCEEDED"),
            "FAILED_BUDGET"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_INGESTION_SPEND_BUDGET_REQUIRED"
            )),
            "KNOWLEDGE_INGESTION_SPEND_BUDGET_REQUIRED"
        );
        assert_eq!(
            budget_terminal_state("KNOWLEDGE_INGESTION_SPEND_BUDGET_REQUIRED"),
            "FAILED_BUDGET"
        );
        assert_eq!(
            worker_error_code(&anyhow::anyhow!(
                "KNOWLEDGE_INGESTION_SOURCE_SPEND_BUDGET_UNAVAILABLE"
            )),
            "KNOWLEDGE_INGESTION_SOURCE_SPEND_BUDGET_UNAVAILABLE"
        );
        assert_eq!(
            budget_terminal_state("KNOWLEDGE_INGESTION_SOURCE_SPEND_BUDGET_UNAVAILABLE"),
            "FAILED"
        );
        assert_eq!(budget_terminal_state("KNOWLEDGE_BUILD_FAILED"), "FAILED");
    }

    #[test]
    fn migration_cost_allocation_reconciles_exactly() {
        let allocations = allocate_exact(10, &[1, 1, 1]);
        assert_eq!(allocations.iter().sum::<i64>(), 10);
        assert_eq!(allocations, vec![3, 3, 4]);
    }

    #[test]
    fn migration_failure_code_prefers_structured_chain_code() {
        let error = anyhow::anyhow!("error returned from database: unavailable")
            .context("KNOWLEDGE_MIGRATION_CURSOR_CONFLICT: retry required");
        assert_eq!(
            migration_failure_code(&error),
            "KNOWLEDGE_MIGRATION_CURSOR_CONFLICT"
        );
        assert_eq!(
            migration_failure_code(&anyhow::anyhow!("error returned from database: timeout")),
            "KNOWLEDGE_MIGRATION_DEPENDENCY_FAILURE"
        );
    }

    #[test]
    fn connector_delta_replaces_only_reported_objects_while_full_is_authoritative() {
        fn state(id: &str, version: &str) -> CorpusDocumentState {
            CorpusDocumentState {
                source_object_id: id.into(),
                canonical_uri: format!("https://example.test/{id}"),
                source_version: version.into(),
                content_digest: sha256_hex(format!("{id}:{version}").as_bytes()),
                metadata_digest: sha256_hex(b"{}"),
                acl_digest: sha256_hex(b"acl"),
                markdown: format!("# {id} {version}"),
            }
        }
        fn object(id: &str, deleted: bool) -> knowledge_connectors::ConnectorObject {
            knowledge_connectors::ConnectorObject {
                external_id: id.into(),
                parent_external_id: None,
                canonical_uri: format!("https://example.test/{id}"),
                provider_version: "v2".into(),
                title: id.into(),
                markdown: if deleted {
                    String::new()
                } else {
                    format!("# {id} v2")
                },
                deleted,
                permission: knowledge_connectors::ProviderPermission::Confluence(
                    knowledge_connectors::ConfluencePermission {
                        product_access_complete: true,
                        space_permission_complete: true,
                        inherited_restrictions_complete: true,
                        page_restrictions_complete: true,
                        unsupported_precedence: false,
                        effective_subjects: vec![],
                    },
                ),
            }
        }
        let previous = vec![state("a", "v1"), state("b", "v1"), state("c", "v1")];
        let objects = std::collections::BTreeMap::from([
            ("a".into(), object("a", false)),
            ("b".into(), object("b", true)),
        ]);
        let delta = resolve_connector_corpus(
            ConnectorSyncMode::Delta,
            &previous,
            &objects,
            vec![state("a", "v2")],
        );
        assert_eq!(
            delta
                .iter()
                .map(|entry| entry.source_object_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "c"]
        );
        let full = resolve_connector_corpus(
            ConnectorSyncMode::Full,
            &previous,
            &objects,
            vec![state("a", "v2")],
        );
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].source_version, "v2");
    }
}
