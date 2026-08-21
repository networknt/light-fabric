use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use agent_delegation::{DelegationKind, DelegationVerifier};
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use knowledge_core::{
    AuthorizationSnapshot, BaseManifest, Chunk, FullBaseGeneration, KnowledgeBaseRankedResponse,
    KnowledgeError, KnowledgeSearchResponse, MultiKnowledgeBaseResponse, RetrievalResponse,
    RetrieveRequest, fuse_knowledge_base_results, retrieve_resolved_generation_with_gate,
};
use light_runtime::{RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KnowledgeConfig {
    pub version: u16,
    pub database_url_file: PathBuf,
    #[serde(default)]
    pub expected_database: Option<String>,
    pub delegation_secret_file: PathBuf,
    pub query_cache_key_file: PathBuf,
    pub heartbeat_secret_file: PathBuf,
    pub delegation_issuer: String,
    pub object_store_root: PathBuf,
    pub maximum_request_bytes: usize,
    pub maximum_query_bytes: usize,
    pub request_timeout_ms: u64,
    pub maximum_database_connections: u32,
    pub projection_lease_seconds: u64,
    #[serde(default)]
    pub legacy_delegation_acceptance_deadline: Option<DateTime<Utc>>,
    #[serde(default = "default_true")]
    pub deterministic_pilot: bool,
    #[serde(default)]
    pub embedding_gateway_url: Option<String>,
    #[serde(default)]
    pub embedding_authorization_file: Option<PathBuf>,
    #[serde(default = "default_query_embedding_alias")]
    pub embedding_alias: String,
    pub embedding_space_id: String,
    pub embedding_space_revision: u64,
    pub embedding_dimension: usize,
    #[serde(default = "default_query_cache_entries")]
    pub query_cache_maximum_entries: usize,
    #[serde(default = "default_query_cache_bytes")]
    pub query_cache_maximum_bytes: usize,
    #[serde(default = "default_query_cache_ttl_seconds")]
    pub query_cache_ttl_seconds: u64,
    #[serde(default)]
    pub graph_limits: GraphLimits,
    pub features: FeatureFlags,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GraphLimits {
    pub maximum_seeds: usize,
    pub maximum_pairs: usize,
    pub maximum_fan_out: usize,
    pub maximum_hops: usize,
    pub maximum_visited_nodes: usize,
    pub maximum_visited_edges: usize,
    pub maximum_paths: usize,
    pub maximum_evidence_chunks: usize,
    pub maximum_token_budget: usize,
    pub maximum_memory_bytes: usize,
    pub timeout_ms: u64,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            maximum_seeds: 8,
            maximum_pairs: 64,
            maximum_fan_out: 16,
            maximum_hops: 3,
            maximum_visited_nodes: 128,
            maximum_visited_edges: 256,
            maximum_paths: 32,
            maximum_evidence_chunks: 20,
            maximum_token_budget: 4_096,
            maximum_memory_bytes: 1_048_576,
            timeout_ms: 100,
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_query_embedding_alias() -> String {
    "kb-query".into()
}

fn default_query_cache_entries() -> usize {
    2_048
}

fn default_query_cache_bytes() -> usize {
    64 * 1_024 * 1_024
}

fn default_query_cache_ttl_seconds() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeatureFlags {
    pub delta_segments: bool,
    pub uploads: bool,
    pub context_expansion: bool,
    pub multi_knowledge_base: bool,
    pub graph_assisted: bool,
    #[serde(default)]
    pub enterprise_source_acls: bool,
    #[serde(default)]
    pub embedding_migration: bool,
    #[serde(default)]
    pub production_operations: bool,
}

impl KnowledgeConfig {
    pub fn load_from_runtime(runtime: &RuntimeConfig) -> Result<Self, String> {
        Self::load_from_runtime_file(runtime, "knowledge.yml")
    }

    pub fn load_from_runtime_file(
        runtime: &RuntimeConfig,
        file_name: &str,
    ) -> Result<Self, String> {
        let mut config = runtime
            .module_registry
            .load_config::<Self>(runtime, file_name)
            .map_err(|error| format!("load effective Knowledge configuration: {error}"))?;
        if let Ok(value) = env::var("LIGHT_KNOWLEDGE_EXPECTED_DATABASE") {
            config.expected_database = Some(value);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn load() -> Result<Self, String> {
        let path = env::var("LIGHT_KNOWLEDGE_CONFIG_FILE")
            .unwrap_or_else(|_| "config/knowledge.yml".to_string());
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read Knowledge config {path}: {error}"))?;
        let mut config: Self = serde_yaml::from_str(&content)
            .map_err(|error| format!("failed to parse Knowledge config {path}: {error}"))?;
        if let Ok(value) = env::var("LIGHT_KNOWLEDGE_EXPECTED_DATABASE") {
            config.expected_database = Some(value);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!(
                "unsupported Knowledge config version {}",
                self.version
            ));
        }
        if self.maximum_request_bytes == 0
            || self.maximum_query_bytes == 0
            || self.maximum_query_bytes > self.maximum_request_bytes
            || self.request_timeout_ms == 0
            || self.maximum_database_connections == 0
            || !secret_available(&self.query_cache_key_file)
            || self.projection_lease_seconds != 30
            || self
                .legacy_delegation_acceptance_deadline
                .is_some_and(|deadline| deadline > Utc::now() + chrono::Duration::minutes(10))
            || self.embedding_dimension == 0
            || self.embedding_space_id.trim().is_empty()
            || self.embedding_space_revision == 0
            || self.embedding_alias.trim().is_empty()
            || self.query_cache_maximum_entries == 0
            || self.query_cache_maximum_entries > 2_048
            || self.query_cache_maximum_bytes == 0
            || self.query_cache_maximum_bytes > 64 * 1_024 * 1_024
            || self.query_cache_ttl_seconds == 0
            || self.query_cache_ttl_seconds > 300
            || (self.deterministic_pilot
                && (self.embedding_dimension != knowledge_core::FAKE_DIMENSION
                    || self.embedding_space_id != knowledge_core::FAKE_SPACE_ID
                    || self.embedding_space_revision != knowledge_core::FAKE_SPACE_REVISION
                    || self.embedding_gateway_url.is_some()))
            || (!self.deterministic_pilot
                && (self
                    .embedding_gateway_url
                    .as_deref()
                    .is_none_or(|url| !url.starts_with("https://"))
                    || self
                        .embedding_authorization_file
                        .as_ref()
                        .is_none_or(|path| !secret_available(path))))
        {
            return Err("invalid Phase 1a limits, lease, or embedding-space contract".into());
        }
        if self.features.graph_assisted
            && (self.graph_limits.maximum_seeds == 0
                || self.graph_limits.maximum_seeds > 16
                || self.graph_limits.maximum_pairs == 0
                || self.graph_limits.maximum_pairs > 256
                || self.graph_limits.maximum_fan_out == 0
                || self.graph_limits.maximum_fan_out > 32
                || self.graph_limits.maximum_hops == 0
                || self.graph_limits.maximum_hops > 4
                || self.graph_limits.maximum_visited_nodes == 0
                || self.graph_limits.maximum_visited_nodes > 512
                || self.graph_limits.maximum_visited_edges == 0
                || self.graph_limits.maximum_visited_edges > 1_024
                || self.graph_limits.maximum_paths == 0
                || self.graph_limits.maximum_paths > 128
                || self.graph_limits.maximum_evidence_chunks == 0
                || self.graph_limits.maximum_evidence_chunks > 20
                || self.graph_limits.maximum_token_budget == 0
                || self.graph_limits.maximum_memory_bytes == 0
                || self.graph_limits.maximum_memory_bytes > 4 * 1_024 * 1_024
                || self.graph_limits.timeout_ms == 0
                || self.graph_limits.timeout_ms > 500)
        {
            return Err("invalid Phase 4 server-owned graph limits".into());
        }
        if (self.features.uploads || self.features.context_expansion)
            && !self.features.delta_segments
        {
            return Err("Phase 1b uploads and context expansion require delta segments".into());
        }
        if self.features.enterprise_source_acls && !self.features.delta_segments {
            return Err("Phase 2 enterprise source ACLs require delta segments".into());
        }
        if !secret_available(&self.database_url_file) {
            return Err("databaseUrlFile must be a readable regular file".into());
        }
        if !secret_available(&self.delegation_secret_file)
            || !secret_available(&self.heartbeat_secret_file)
            || self.delegation_issuer.trim().is_empty()
        {
            return Err("delegation secret and issuer are required".into());
        }
        fs::create_dir_all(&self.object_store_root)
            .map_err(|error| format!("failed to create objectStoreRoot: {error}"))?;
        Ok(())
    }
}

pub struct KnowledgeState {
    pool: PgPool,
    delegation_verifier: DelegationVerifier,
    heartbeat_secret: Vec<u8>,
    query_cache_key: Vec<u8>,
    query_cache: Mutex<QueryEmbeddingCache>,
    metrics_cache: Mutex<Option<MetricsCacheEntry>>,
    embedding_client: reqwest::Client,
    config: KnowledgeConfig,
}

struct QueryEmbeddingCacheEntry {
    vector: Vec<f32>,
    expires_at: Instant,
    stored_at: Instant,
}

#[derive(Default)]
struct QueryEmbeddingCache {
    entries: HashMap<String, QueryEmbeddingCacheEntry>,
    stored_bytes: usize,
}

struct MetricsCacheEntry {
    body: String,
    expires_at: Instant,
}

impl KnowledgeState {
    pub fn database_pool(&self) -> PgPool {
        self.pool.clone()
    }

    pub async fn build(
        runtime_config: &RuntimeConfig,
        config: KnowledgeConfig,
    ) -> Result<Self, RuntimeError> {
        let database_url =
            read_secret_file(&config.database_url_file).map_err(RuntimeError::Config)?;
        let pool = PgPoolOptions::new()
            .max_connections(config.maximum_database_connections)
            .acquire_timeout(StdDuration::from_millis(config.request_timeout_ms))
            .connect(&database_url)
            .await
            .map_err(|error| RuntimeError::Config(format!("Knowledge database: {error}")))?;
        if let Some(expected_database) = config.expected_database.as_deref() {
            let actual_database = sqlx::query_scalar::<_, String>("SELECT current_database()")
                .fetch_one(&pool)
                .await
                .map_err(|error| {
                    RuntimeError::Config(format!("Knowledge database identity: {error}"))
                })?;
            if actual_database != expected_database {
                return Err(RuntimeError::Config(format!(
                    "Knowledge database identity mismatch: expected {expected_database}, got {actual_database}"
                )));
            }
        }
        let api_contract_available: bool = sqlx::query_scalar(
            "SELECT to_regclass('knowledge_runtime_authorization_t') IS NOT NULL
                 AND to_regclass('knowledge_consumer_quota_t') IS NOT NULL
                 AND to_regclass('knowledge_query_admission_t') IS NOT NULL
                 AND has_table_privilege(
                       current_user,'knowledge_runtime_authorization_t','SELECT')
                 AND has_table_privilege(
                       current_user,'knowledge_consumer_quota_t','SELECT')
                 AND has_table_privilege(
                       current_user,'knowledge_query_admission_t','SELECT')
                 AND has_table_privilege(
                       current_user,'knowledge_query_admission_t','INSERT')
                 AND has_table_privilege(
                       current_user,'knowledge_query_admission_t','UPDATE')",
        )
        .fetch_one(&pool)
        .await
        .map_err(|error| RuntimeError::Config(format!("Knowledge API schema contract: {error}")))?;
        if !api_contract_available {
            return Err(RuntimeError::Config(
                "Knowledge API schema or privilege contract is unavailable".into(),
            ));
        }
        let delegation_secret =
            read_secret_file(&config.delegation_secret_file).map_err(RuntimeError::Config)?;
        let delegation_verifier = DelegationVerifier::new(
            delegation_secret.as_bytes(),
            config.delegation_issuer.clone(),
            "light-knowledge",
        )
        .map_err(|error| RuntimeError::Config(format!("Knowledge delegation verifier: {error}")))?;
        let heartbeat_secret =
            read_secret_bytes(&config.heartbeat_secret_file).map_err(RuntimeError::Config)?;
        if heartbeat_secret.is_empty() {
            return Err(RuntimeError::Config("heartbeat secret is empty".into()));
        }
        let query_cache_key =
            read_secret_bytes(&config.query_cache_key_file).map_err(RuntimeError::Config)?;
        if query_cache_key.len() < 32 {
            return Err(RuntimeError::Config(
                "query cache key must contain at least 32 bytes".into(),
            ));
        }
        let embedding_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(StdDuration::from_millis(config.request_timeout_ms))
            .timeout(StdDuration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|error| {
                RuntimeError::Config(format!("Knowledge embedding client: {error}"))
            })?;
        let _ = runtime_config;
        Ok(Self {
            pool,
            delegation_verifier,
            heartbeat_secret,
            query_cache_key,
            query_cache: Mutex::new(QueryEmbeddingCache::default()),
            metrics_cache: Mutex::new(None),
            embedding_client,
            config,
        })
    }
}

fn read_secret_file(path: &std::path::Path) -> Result<String, String> {
    for environment_name in secret_environment_names(path) {
        if let Ok(value) = env::var(environment_name) {
            let value = value.trim();
            if value.is_empty() {
                return Err(format!(
                    "secret environment variable {environment_name} is empty"
                ));
            }
            return Ok(value.to_string());
        }
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("failed to inspect secret file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "secret path is not a regular file: {}",
            path.display()
        ));
    }
    let value = fs::read_to_string(path)
        .map_err(|error| format!("failed to read secret file {}: {error}", path.display()))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("secret file is empty: {}", path.display()));
    }
    Ok(value.to_string())
}

fn read_secret_bytes(path: &std::path::Path) -> Result<Vec<u8>, String> {
    for environment_name in secret_environment_names(path) {
        if let Ok(value) = env::var(environment_name) {
            let value = value.into_bytes();
            if value.is_empty() {
                return Err(format!(
                    "secret environment variable {environment_name} is empty"
                ));
            }
            return Ok(value);
        }
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("failed to inspect secret file {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!(
            "secret path is not a regular file: {}",
            path.display()
        ));
    }
    let value = fs::read(path)
        .map_err(|error| format!("failed to read secret file {}: {error}", path.display()))?;
    if value.is_empty() {
        return Err(format!("secret file is empty: {}", path.display()));
    }
    Ok(value)
}

fn secret_available(path: &std::path::Path) -> bool {
    secret_environment_names(path)
        .iter()
        .any(|name| env::var(name).is_ok_and(|value| !value.trim().is_empty()))
        || path.is_file()
}

fn secret_environment_names(path: &std::path::Path) -> &'static [&'static str] {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("knowledge-database-url") => &["LIGHT_KNOWLEDGE_DATABASE_URL"],
        Some("agent-delegation-secret") => &["LIGHT_AGENT_DELEGATION_SECRET"],
        Some("knowledge-query-cache-key") => &["LIGHT_KNOWLEDGE_QUERY_CACHE_KEY"],
        Some("knowledge-heartbeat-secret") => &["LIGHT_KNOWLEDGE_HEARTBEAT_SECRET"],
        Some("knowledge-query-embedding-authorization") => &[
            "KNOWLEDGE_QUERY_EMBEDDING_AUTHORIZATION",
            "LIGHT_PORTAL_AUTHORIZATION",
            "LIGHT_KNOWLEDGE_AUTHORIZATION",
        ],
        _ => &[],
    }
}

pub fn knowledge_router(state: Arc<KnowledgeState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/knowledge/retrieve", post(retrieve_handler))
        .route("/v1/knowledge/uploads", post(upload_handler))
        .route("/mcp", post(mcp_handler))
        .route(
            "/v1/knowledge/documents/{document_id}/versions/{document_version_id}",
            get(document_version_handler),
        )
        .route(
            "/v1/knowledge/documents/{document_id}/passages/{passage_anchor_id}",
            get(passage_anchor_handler),
        )
        .layer(DefaultBodyLimit::max(state.config.maximum_request_bytes))
        .with_state(state)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadAcceptedResponse {
    upload_id: Uuid,
    lifecycle_state: String,
    staged_digest: String,
}

async fn upload_handler(
    State(state): State<Arc<KnowledgeState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<(StatusCode, Json<UploadAcceptedResponse>), ApiError> {
    if !state.config.features.uploads || body.is_empty() || body.len() > 100 * 1024 * 1024 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    let authenticated =
        authenticated_context(&headers, &state, DelegationKind::KnowledgeUpload).await?;
    let knowledge_base_id = required_uuid_header(&headers, "x-knowledge-base-id")?;
    let source_id = required_uuid_header(&headers, "x-knowledge-source-id")?;
    preauthorize_request(
        &state,
        knowledge_base_id,
        &authenticated,
        &authenticated.environment,
    )
    .await?;
    let filename = required_text_header(&headers, "x-upload-filename")?;
    let media_type = required_text_header(&headers, "content-type")?
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        media_type.as_str(),
        "text/plain" | "text/markdown" | "text/html"
    ) || filename.len() > 512
        || filename.contains(['\n', '\r', '\0'])
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "KNOWLEDGE_UNSUPPORTED_CONTRACT",
        ));
    }
    let authorized_source = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
           SELECT 1 FROM knowledge_source_t source
           JOIN knowledge_base_t kb ON kb.knowledge_base_id=source.knowledge_base_id
          WHERE source.source_id=$1 AND source.knowledge_base_id=$2
            AND upper(source.source_type)='UPLOAD'
            AND source.status='ACTIVE')",
    )
    .bind(source_id)
    .bind(knowledge_base_id)
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if !authorized_source {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "KNOWLEDGE_FORBIDDEN"));
    }
    let upload_id = Uuid::now_v7();
    let digest = sha256_hex(&body);
    let upload_root = state.config.object_store_root.join("uploads");
    fs::create_dir_all(&upload_root).map_err(ApiError::database)?;
    let staged_path = upload_root.join(format!("{upload_id}.staged"));
    const EICAR_MARKER: &[u8] = b"EICAR-STANDARD-ANTIVIRUS-TEST-FILE";
    let decoded = std::str::from_utf8(&body);
    let rejected = body
        .windows(EICAR_MARKER.len())
        .any(|window| window == EICAR_MARKER)
        || decoded.is_err()
        || decoded.is_ok_and(|markdown| !knowledge_core::is_indexable_markdown(markdown));
    let (scan_state, lifecycle_state, rejection_code) = if rejected {
        ("REJECTED", "REJECTED", Some("UPLOAD_CONTENT_REJECTED"))
    } else {
        ("PENDING", "STAGED", None)
    };
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query(
        "INSERT INTO knowledge_upload_t(
           upload_id,knowledge_base_id,source_id,source_object_id,
           original_filename,media_type,content_length,staged_locator,
           staged_digest,scan_state,lifecycle_state,rejection_code,requested_by,
           verified_ts,purge_after_ts)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,
           NULL,now()+interval '24 hours')",
    )
    .bind(upload_id)
    .bind(knowledge_base_id)
    .bind(source_id)
    .bind(format!("upload:{upload_id}"))
    .bind(&filename)
    .bind(&media_type)
    .bind(i64::try_from(body.len()).unwrap_or(i64::MAX))
    .bind(staged_path.to_string_lossy().as_ref())
    .bind(&digest)
    .bind(scan_state)
    .bind(lifecycle_state)
    .bind(rejection_code)
    .bind(authenticated.agent_def_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(ApiError::database)?;
    tx.commit().await.map_err(ApiError::database)?;
    if rejected {
        return Ok((
            StatusCode::ACCEPTED,
            Json(UploadAcceptedResponse {
                upload_id,
                lifecycle_state: lifecycle_state.into(),
                staged_digest: digest,
            }),
        ));
    }
    if let Err(error) = fs::write(&staged_path, &body) {
        let _ = mark_upload_orphaned(&state.pool, upload_id, "UPLOAD_STAGING_FAILED").await;
        let _ = fs::remove_file(&staged_path);
        return Err(ApiError::database(error));
    }
    let mut tx = state.pool.begin().await.map_err(ApiError::database)?;
    let finalize_result = async {
        sqlx::query(
            "UPDATE knowledge_upload_t
                SET scan_state='CLEAN',lifecycle_state='VERIFIED',verified_ts=now()
              WHERE upload_id=$1 AND lifecycle_state='STAGED'",
        )
        .bind(upload_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO knowledge_job_t(
               job_id,knowledge_base_id,source_id,job_type,idempotency_key,
               requested_by,payload)
             VALUES($1,$2,$3,'UPLOAD',$4,$5,$6)
             ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(knowledge_base_id)
        .bind(source_id)
        .bind(format!("upload:{upload_id}:{digest}"))
        .bind(authenticated.agent_def_id.to_string())
        .bind(json!({"uploadId": upload_id}))
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    if let Err(error) = finalize_result {
        let _ = fs::remove_file(&staged_path);
        let _ = mark_upload_orphaned(&state.pool, upload_id, "UPLOAD_FINALIZE_FAILED").await;
        return Err(ApiError::database(error));
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(UploadAcceptedResponse {
            upload_id,
            lifecycle_state: "VERIFIED".into(),
            staged_digest: digest,
        }),
    ))
}

async fn mark_upload_orphaned(
    pool: &PgPool,
    upload_id: Uuid,
    rejection_code: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE knowledge_upload_t
            SET scan_state='ERROR',lifecycle_state='ORPHANED',rejection_code=$2
          WHERE upload_id=$1 AND lifecycle_state='STAGED'",
    )
    .bind(upload_id)
    .bind(rejection_code)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct McpRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

async fn mcp_handler(
    State(state): State<Arc<KnowledgeState>>,
    headers: HeaderMap,
    Json(message): Json<McpRequest>,
) -> Result<Json<Value>, ApiError> {
    if message.jsonrpc != "2.0" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    let result = match message.method.as_str() {
        "initialize" => json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "light-knowledge", "version": env!("CARGO_PKG_VERSION")}
        }),
        "tools/list" => json!({"tools": [
            {
                "name": "knowledge.search",
                "description": "Search up to four authorized Knowledge Bases with cited results.",
                "inputSchema": {
                    "type": "object",
                    "required": ["query"],
                    "additionalProperties": false,
                    "properties": {
                        "query": {"type": "string", "maxLength": 8192},
                        "knowledgeBaseIds": {"type": "array", "maxItems": 4, "items": {"type": "string", "format": "uuid"}},
                        "topK": {"type": "integer", "minimum": 0, "maximum": 20},
                        "filters": {"type": "object"}
                    }
                }
            },
            {
                "name": "knowledge.get_document",
                "description": "Resolve one exact authorized Knowledge citation.",
                "inputSchema": {
                    "type": "object",
                    "required": ["knowledgeBaseId", "documentId", "documentVersionId"],
                    "additionalProperties": false,
                    "properties": {
                        "knowledgeBaseId": {"type": "string", "format": "uuid"},
                        "documentId": {"type": "string", "format": "uuid"},
                        "documentVersionId": {"type": "string", "format": "uuid"}
                    }
                }
            }
        ]}),
        "tools/call" => {
            let authenticated =
                authenticated_context(&headers, &state, DelegationKind::KnowledgeRetrieve).await?;
            let name = message
                .params
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ApiError::new(StatusCode::BAD_REQUEST, "KNOWLEDGE_INVALID_REQUEST")
                })?;
            let arguments = message
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let content = match name {
                "knowledge.search" => {
                    let mut request: RetrieveRequest =
                        serde_json::from_value(arguments).map_err(|_| {
                            ApiError::new(StatusCode::BAD_REQUEST, "KNOWLEDGE_INVALID_REQUEST")
                        })?;
                    validate_retrieve_request(&state.config, &request)?;
                    request.environment = authenticated.environment.clone();
                    let request_id = required_text_header(&headers, "x-request-id")?;
                    serde_json::to_value(
                        tokio::time::timeout(
                            StdDuration::from_millis(state.config.request_timeout_ms),
                            search_application(&state, &request_id, &authenticated, &request),
                        )
                        .await
                        .map_err(|_| {
                            ApiError::new(
                                StatusCode::GATEWAY_TIMEOUT,
                                "KNOWLEDGE_DEADLINE_EXCEEDED",
                            )
                        })??,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "KNOWLEDGE_RESPONSE_INVALID",
                        )
                    })?
                }
                "knowledge.get_document" => {
                    let knowledge_base_id = mcp_uuid(&arguments, "knowledgeBaseId")?;
                    let document_id = mcp_uuid(&arguments, "documentId")?;
                    let document_version_id = mcp_uuid(&arguments, "documentVersionId")?;
                    serde_json::to_value(
                        load_document_version(
                            &state,
                            &authenticated,
                            knowledge_base_id,
                            document_id,
                            document_version_id,
                        )
                        .await?,
                    )
                    .map_err(|_| {
                        ApiError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "KNOWLEDGE_RESPONSE_INVALID",
                        )
                    })?
                }
                _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "KNOWLEDGE_NOT_FOUND")),
            };
            json!({"content": [{"type": "text", "text": content.to_string()}], "structuredContent": content, "isError": false})
        }
        _ => return Err(ApiError::new(StatusCode::NOT_FOUND, "KNOWLEDGE_NOT_FOUND")),
    };
    Ok(Json(
        json!({"jsonrpc": "2.0", "id": message.id, "result": result}),
    ))
}

fn mcp_uuid(value: &Value, field: &str) -> Result<Uuid, ApiError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "KNOWLEDGE_INVALID_REQUEST"))
}

async fn health() -> impl IntoResponse {
    Json(json!({"status":"healthy"}))
}

async fn ready(State(state): State<Arc<KnowledgeState>>) -> Response {
    let database = sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .is_ok();
    let projection = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM knowledge_control_snapshot_t
          WHERE state='APPLIED' AND lease_expires_ts > now())",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(false);
    let status = if database && projection {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(
            json!({"ready": status == StatusCode::OK, "database": database,
        "projectionLease": projection}),
        ),
    )
        .into_response()
}

async fn metrics(State(state): State<Arc<KnowledgeState>>) -> Response {
    // Prometheus endpoints are intentionally unauthenticated. Serialize refreshes and cache the
    // result so one scrape cannot amplify into repeated database scans.
    let mut cache = state.metrics_cache.lock().await;
    if let Some(cached) = cache
        .as_ref()
        .filter(|cached| cached.expires_at > Instant::now())
    {
        return prometheus_response(cached.body.clone());
    }
    let stale = cache.as_ref().map(|cached| cached.body.clone());
    let refresh = tokio::time::timeout(StdDuration::from_secs(5), load_metrics(&state.pool)).await;
    let (body, ttl) = match refresh {
        Ok(Ok(body)) => (body, StdDuration::from_secs(15)),
        Ok(Err(error)) => {
            tracing::warn!(%error, "Knowledge metrics refresh failed; serving stale metrics");
            (
                stale.unwrap_or_else(|| render_metrics([-1; 11])),
                StdDuration::from_secs(5),
            )
        }
        Err(_) => {
            tracing::warn!("Knowledge metrics refresh timed out; serving stale metrics");
            (
                stale.unwrap_or_else(|| render_metrics([-1; 11])),
                StdDuration::from_secs(5),
            )
        }
    };
    *cache = Some(MetricsCacheEntry {
        body: body.clone(),
        expires_at: Instant::now() + ttl,
    });
    prometheus_response(body)
}

fn prometheus_response(body: String) -> Response {
    (
        [(
            &axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

async fn load_metrics(pool: &PgPool) -> Result<String, sqlx::Error> {
    let readiness = sqlx::query(
        "WITH requested(metric_group,table_name,required_columns) AS (VALUES
           ('projection','knowledge_control_snapshot_t',ARRAY['state','lease_expires_ts']::text[]),
           ('job','knowledge_job_t',ARRAY['state','lease_expires_ts']::text[]),
           ('promotion','knowledge_promotion_receipt_t',ARRAY['committed_ts']::text[]),
           ('authorization','knowledge_runtime_authorization_t',ARRAY['active','lease_expires_ts']::text[]),
           ('source_acl','knowledge_source_acl_state_t',ARRAY['state','fresh_until_ts']::text[]),
           ('migration','knowledge_embedding_migration_t',ARRAY['state']::text[]),
           ('audit','knowledge_query_audit_t',ARRAY['fallback_reason','created_ts']::text[])
         )
         SELECT metric_group,to_regclass(table_name) IS NOT NULL AS table_exists,
           COALESCE(
             has_table_privilege(to_regclass(table_name),'SELECT')
             AND NOT EXISTS(
               SELECT 1 FROM unnest(required_columns) required(column_name)
                WHERE NOT EXISTS(
                  SELECT 1 FROM pg_attribute attribute
                   WHERE attribute.attrelid=to_regclass(table_name)
                     AND attribute.attname=required.column_name
                     AND attribute.attnum>0 AND NOT attribute.attisdropped)),
             FALSE) AS ready
           FROM requested",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(|row| {
        (
            row.get::<String, _>("metric_group"),
            (
                row.get::<bool, _>("table_exists"),
                row.get::<bool, _>("ready"),
            ),
        )
    })
    .collect::<HashMap<_, _>>();
    let scalar = |group: &str, query: &str| match readiness.get(group).copied() {
        Some((true, true)) => format!("({query})"),
        Some((true, false)) => "-1::bigint".to_string(),
        _ => "0::bigint".to_string(),
    };
    let query = format!(
        "SELECT {} AS pending, {} AS gaps, {} AS queued, {} AS running,
           {} AS failed, {} AS expired, {} AS promotion_pending,
           {} AS stale_authorizations, {} AS stale_acl_sources,
           {} AS migration_attention, {} AS graph_fallbacks",
        scalar(
            "projection",
            "SELECT count(*) FROM knowledge_control_snapshot_t WHERE state<>'APPLIED'"
        ),
        scalar(
            "projection",
            "SELECT count(*) FROM knowledge_control_snapshot_t WHERE state='APPLIED' AND lease_expires_ts<=now()"
        ),
        scalar(
            "job",
            "SELECT count(*) FROM knowledge_job_t WHERE state='QUEUED'"
        ),
        scalar(
            "job",
            "SELECT count(*) FROM knowledge_job_t WHERE state='RUNNING'"
        ),
        scalar(
            "job",
            "SELECT count(*) FROM knowledge_job_t WHERE state='FAILED'"
        ),
        scalar(
            "job",
            "SELECT count(*) FROM knowledge_job_t WHERE state='RUNNING' AND (lease_expires_ts IS NULL OR lease_expires_ts<=now())"
        ),
        scalar(
            "promotion",
            "SELECT 0::bigint FROM knowledge_promotion_receipt_t LIMIT 1"
        ),
        scalar(
            "authorization",
            "SELECT count(*) FROM knowledge_runtime_authorization_t WHERE active=TRUE AND lease_expires_ts<=now()"
        ),
        scalar(
            "source_acl",
            "SELECT count(*) FROM knowledge_source_acl_state_t WHERE state<>'COMPLETE' OR fresh_until_ts<=now()"
        ),
        scalar(
            "migration",
            "SELECT count(*) FROM knowledge_embedding_migration_t WHERE state IN ('PAUSED','FAILED')"
        ),
        scalar(
            "audit",
            "SELECT count(*) FROM knowledge_query_audit_t WHERE fallback_reason IS NOT NULL AND created_ts>=now()-interval '5 minutes'"
        ),
    );
    let row = sqlx::query(&query).fetch_one(pool).await?;
    Ok(render_metrics([
        row.try_get("pending").unwrap_or(-1),
        row.try_get("gaps").unwrap_or(-1),
        row.try_get("queued").unwrap_or(-1),
        row.try_get("running").unwrap_or(-1),
        row.try_get("failed").unwrap_or(-1),
        row.try_get("expired").unwrap_or(-1),
        row.try_get("promotion_pending").unwrap_or(-1),
        row.try_get("stale_authorizations").unwrap_or(-1),
        row.try_get("stale_acl_sources").unwrap_or(-1),
        row.try_get("migration_attention").unwrap_or(-1),
        row.try_get("graph_fallbacks").unwrap_or(-1),
    ]))
}

fn render_metrics(values: [i64; 11]) -> String {
    let [
        pending,
        gaps,
        queued,
        running,
        failed,
        expired,
        promotion_pending,
        stale_authorizations,
        stale_acl_sources,
        migration_attention,
        graph_fallbacks,
    ] = values;
    let body = format!(
        "# TYPE light_knowledge_snapshot_superseded gauge\n\
         light_knowledge_snapshot_superseded {pending}\n\
         # TYPE light_knowledge_snapshot_stale gauge\n\
         light_knowledge_snapshot_stale {gaps}\n\
         # TYPE light_knowledge_jobs gauge\n\
         light_knowledge_jobs{{state=\"queued\"}} {queued}\n\
         light_knowledge_jobs{{state=\"running\"}} {running}\n\
         light_knowledge_jobs{{state=\"failed\"}} {failed}\n\
         # TYPE light_knowledge_job_lease_expired gauge\n\
         light_knowledge_job_lease_expired {expired}\n\
         # TYPE light_knowledge_promotion_pending gauge\n\
         light_knowledge_promotion_pending {promotion_pending}\n\
         # TYPE light_knowledge_authorization_stale gauge\n\
         light_knowledge_authorization_stale {stale_authorizations}\n\
         # TYPE light_knowledge_acl_source_stale gauge\n\
         light_knowledge_acl_source_stale {stale_acl_sources}\n\
         # TYPE light_knowledge_migration_attention gauge\n\
         light_knowledge_migration_attention {migration_attention}\n\
         # TYPE light_knowledge_graph_fallbacks_5m gauge\n\
         light_knowledge_graph_fallbacks_5m {graph_fallbacks}\n"
    );
    body
}

struct AuthenticatedKnowledgeRequest {
    host_id: Uuid,
    agent_def_id: Uuid,
    environment: String,
    policy_digest: String,
    data_boundary_digest: String,
    subject_id: String,
    subject_type: String,
    groups: Vec<String>,
    organizations: Vec<String>,
    normalized_claims_present: bool,
}

async fn authenticated_context(
    headers: &HeaderMap,
    state: &KnowledgeState,
    expected_kind: DelegationKind,
) -> Result<AuthenticatedKnowledgeRequest, ApiError> {
    let authorization = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::UNAUTHORIZED,
                "KNOWLEDGE_AUTHENTICATION_REQUIRED",
            )
        })?;
    let principal = state
        .delegation_verifier
        .verify_token(authorization)
        .map_err(|_| ApiError::new(StatusCode::UNAUTHORIZED, "KNOWLEDGE_AUTHENTICATION_FAILED"))?;
    let normalized_claims_present = !principal.subject_id.trim().is_empty()
        && !principal.subject_type.trim().is_empty()
        && principal.groups.is_some()
        && principal.organizations.is_some()
        && principal.agent_policy_version > 0;
    let legacy_window_open = state
        .config
        .legacy_delegation_acceptance_deadline
        .is_some_and(|deadline| deadline > Utc::now());
    if !normalized_claims_present
        && (expected_kind == DelegationKind::KnowledgeUpload || !legacy_window_open)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "KNOWLEDGE_DELEGATION_BINDING_INVALID",
        ));
    }
    if !normalized_claims_present {
        tracing::warn!(
            "Accepted legacy Knowledge delegation during bounded rolling-upgrade window"
        );
    }
    if principal.kind != expected_kind
        || principal.action_attempt_id.is_some()
        || principal.tool_ref.is_some()
        || principal.tool_alias.is_some()
        || principal.destination.as_deref() != Some("knowledge")
        || principal.policy_digest.trim().is_empty()
        || principal.data_boundary_digest.trim().is_empty()
        || principal.environment.as_deref().is_none_or(str::is_empty)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "KNOWLEDGE_DELEGATION_BINDING_INVALID",
        ));
    }
    Ok(AuthenticatedKnowledgeRequest {
        host_id: principal.host_id,
        agent_def_id: principal.agent_def_id.ok_or_else(|| {
            ApiError::new(StatusCode::FORBIDDEN, "KNOWLEDGE_AGENT_CLAIM_REQUIRED")
        })?,
        environment: principal.environment.unwrap_or_default(),
        policy_digest: principal.policy_digest,
        data_boundary_digest: principal.data_boundary_digest,
        subject_id: principal.subject_id,
        subject_type: principal.subject_type,
        groups: principal.groups.unwrap_or_default(),
        organizations: principal.organizations.unwrap_or_default(),
        normalized_claims_present,
    })
}

async fn retrieve_handler(
    State(state): State<Arc<KnowledgeState>>,
    headers: HeaderMap,
    Json(mut request): Json<RetrieveRequest>,
) -> Result<Json<KnowledgeSearchResponse>, ApiError> {
    let authenticated =
        authenticated_context(&headers, &state, DelegationKind::KnowledgeRetrieve).await?;
    validate_retrieve_request(&state.config, &request)?;
    request.environment = authenticated.environment.clone();
    let request_id = required_text_header(&headers, "x-request-id")?;
    let response = tokio::time::timeout(
        StdDuration::from_millis(state.config.request_timeout_ms),
        search_application(&state, &request_id, &authenticated, &request),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "KNOWLEDGE_DEADLINE_EXCEEDED"))??;
    Ok(Json(response))
}

fn validate_retrieve_request(
    config: &KnowledgeConfig,
    request: &RetrieveRequest,
) -> Result<(), ApiError> {
    if request.query.trim().is_empty()
        || request.query.len() > config.maximum_query_bytes.min(8192)
        || request.top_k > 20
        || request
            .knowledge_base_ids
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            > 4
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct SelectedKnowledgeBase {
    knowledge_base_id: Uuid,
    priority: i32,
    maximum_knowledge_bases: usize,
    top_k: usize,
    token_budget: usize,
    failure_policy: String,
    embedding_group_key: String,
    requires_normalized_claims: bool,
}

#[derive(Debug, Clone)]
struct PreparedQueryEmbedding {
    generation_id: Uuid,
    pointer_version: i64,
    space_id: String,
    space_revision: u64,
    dimension: usize,
    vector: Vec<f32>,
}

fn enforce_normalized_claims_for_selection(
    authenticated: &AuthenticatedKnowledgeRequest,
    selected: &[SelectedKnowledgeBase],
) -> Result<(), ApiError> {
    if !authenticated.normalized_claims_present
        && selected
            .iter()
            .any(|selection| selection.requires_normalized_claims)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "KNOWLEDGE_DELEGATION_BINDING_INVALID",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn search_application(
    state: &KnowledgeState,
    request_id: &str,
    authenticated: &AuthenticatedKnowledgeRequest,
    request: &RetrieveRequest,
) -> Result<KnowledgeSearchResponse, ApiError> {
    let selected = select_knowledge_bases(
        state,
        authenticated.host_id,
        authenticated.agent_def_id,
        &request.environment,
        &request.knowledge_base_ids,
    )
    .await?;
    enforce_normalized_claims_for_selection(authenticated, &selected)?;
    if selected.len() == 1 {
        let mut single_request = request.clone();
        single_request.knowledge_base_ids = vec![selected[0].knowledge_base_id];
        return retrieve_transaction(state, request_id, authenticated, &single_request)
            .await
            .map(KnowledgeSearchResponse::Single);
    }
    if !state.config.features.multi_knowledge_base {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "KNOWLEDGE_UNSUPPORTED_CONTRACT",
        ));
    }
    let mut ranked = Vec::new();
    let mut warnings = Vec::new();
    let mut exclusions = Vec::new();
    for selection in &selected {
        let mut single_request = request.clone();
        single_request.knowledge_base_ids = vec![selection.knowledge_base_id];
        match retrieve_transaction(
            state,
            &format!("{request_id}:{}", selection.knowledge_base_id),
            authenticated,
            &single_request,
        )
        .await
        {
            Ok(response) => ranked.push(KnowledgeBaseRankedResponse {
                response,
                priority: selection.priority,
                embedding_group_key: selection.embedding_group_key.clone(),
            }),
            Err(error)
                if selection.failure_policy != "RETURN_PARTIAL"
                    || error.status == StatusCode::FORBIDDEN =>
            {
                return Err(error);
            }
            Err(error) => {
                warnings.push(format!(
                    "KB_SKIPPED_GENERATION_UNAVAILABLE:{}:{}",
                    selection.knowledge_base_id, error.code
                ));
                exclusions.push(selection.knowledge_base_id.to_string());
            }
        }
    }
    if ranked.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        ));
    }
    let mut response: MultiKnowledgeBaseResponse = fuse_knowledge_base_results(
        ranked,
        selected
            .iter()
            .map(|selection| selection.maximum_knowledge_bases)
            .min()
            .unwrap_or(1),
        if request.top_k == 0 {
            selected
                .iter()
                .map(|selection| selection.top_k)
                .sum::<usize>()
        } else {
            request.top_k
        }
        .max(1)
        .min(20),
        selected
            .iter()
            .map(|selection| selection.token_budget)
            .sum::<usize>()
            .max(1),
    )?;
    response.status = if warnings.is_empty() {
        "COMPLETE".into()
    } else {
        "PARTIAL".into()
    };
    if !warnings.is_empty() && response.results.is_empty() {
        response.disposition = "UNKNOWN".into();
    }
    response.warnings = warnings;
    response.exclusions = exclusions;
    Ok(KnowledgeSearchResponse::Multi(response))
}

async fn select_knowledge_bases(
    state: &KnowledgeState,
    consumer_host_id: Uuid,
    agent_def_id: Uuid,
    environment: &str,
    requested: &[Uuid],
) -> Result<Vec<SelectedKnowledgeBase>, ApiError> {
    let requested = requested.iter().copied().collect::<BTreeSet<_>>();
    if requested.len() > 4 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    let rows = sqlx::query(
        "SELECT a.knowledge_base_id,a.priority,p.maximum_knowledge_bases,
                p.top_k,p.token_budget,
                p.operational_failure_policy,g.space_id,g.space_revision,g.dimension,
                g.query_input_transform_version,
                EXISTS(
                  SELECT 1 FROM knowledge_source_t source
                   WHERE source.knowledge_base_id=a.knowledge_base_id
                     AND source.acl_mode='MIRROR_SOURCE_ACL'
                ) AS requires_normalized_claims
           FROM agent_knowledge_base_t a
           JOIN knowledge_retrieval_profile_t p ON p.profile_id=a.retrieval_profile_id
           LEFT JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=a.knowledge_base_id
            AND pointer.environment=a.environment
           LEFT JOIN knowledge_index_generation_t g
             ON g.index_generation_id=pointer.index_generation_id
          WHERE a.host_id=$1 AND a.agent_id=$2 AND a.environment=$3
            AND a.active=TRUE AND p.active=TRUE
            AND (cardinality($4::uuid[])=0 OR a.knowledge_base_id=ANY($4::uuid[]))
          ORDER BY a.priority DESC,a.knowledge_base_id LIMIT 5",
    )
    .bind(consumer_host_id)
    .bind(agent_def_id)
    .bind(environment)
    .bind(requested.iter().copied().collect::<Vec<_>>())
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if rows.is_empty() || (!requested.is_empty() && rows.len() != requested.len()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "KNOWLEDGE_FORBIDDEN"));
    }
    let selected = rows
        .into_iter()
        .map(|row| -> Result<SelectedKnowledgeBase, ApiError> {
            let knowledge_base_id = row
                .try_get("knowledge_base_id")
                .map_err(ApiError::database)?;
            let space_id: Option<String> = row.try_get("space_id").map_err(ApiError::database)?;
            let revision: Option<i64> =
                row.try_get("space_revision").map_err(ApiError::database)?;
            let dimension: Option<i32> = row.try_get("dimension").map_err(ApiError::database)?;
            let transform: Option<String> = row
                .try_get("query_input_transform_version")
                .map_err(ApiError::database)?;
            Ok(SelectedKnowledgeBase {
                knowledge_base_id,
                priority: row.try_get("priority").map_err(ApiError::database)?,
                maximum_knowledge_bases: usize::try_from(
                    row.try_get::<i32, _>("maximum_knowledge_bases")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(1),
                top_k: usize::try_from(row.try_get::<i32, _>("top_k").map_err(ApiError::database)?)
                    .unwrap_or(1),
                token_budget: usize::try_from(
                    row.try_get::<i32, _>("token_budget")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(1),
                failure_policy: row
                    .try_get("operational_failure_policy")
                    .map_err(ApiError::database)?,
                requires_normalized_claims: row
                    .try_get("requires_normalized_claims")
                    .map_err(ApiError::database)?,
                embedding_group_key: match (space_id, revision, dimension, transform) {
                    (Some(space_id), Some(revision), Some(dimension), Some(transform)) => {
                        format!("{space_id}:{revision}:{dimension}:{transform}")
                    }
                    _ => format!("unavailable:{knowledge_base_id}"),
                },
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cap = selected
        .iter()
        .map(|selection| selection.maximum_knowledge_bases)
        .min()
        .unwrap_or(1)
        .min(4);
    if selected.len() > cap {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    Ok(selected)
}

async fn retrieve_transaction(
    state: &KnowledgeState,
    request_id: &str,
    authenticated: &AuthenticatedKnowledgeRequest,
    request: &RetrieveRequest,
) -> Result<RetrievalResponse, ApiError> {
    if request.knowledge_base_ids.len() > 1 {
        return Err(ApiError::from(KnowledgeError::MultipleKnowledgeBases));
    }
    let knowledge_base_id = resolve_knowledge_base_id(
        &state.pool,
        authenticated.host_id,
        authenticated.agent_def_id,
        request,
    )
    .await?;
    preauthorize_request(
        state,
        knowledge_base_id,
        authenticated,
        &request.environment,
    )
    .await?;
    admit_request(
        &state.pool,
        knowledge_base_id,
        authenticated.host_id,
        request_id,
    )
    .await?;
    let mut pointer_retry_available = true;
    let result = loop {
        let prepared_embedding = match prepare_query_embedding(
            state,
            knowledge_base_id,
            &request.environment,
            &request.query,
            &authenticated.policy_digest,
            &authenticated.data_boundary_digest,
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(error) => break Err(error),
        };
        match retrieve_snapshot(
            state,
            request_id,
            knowledge_base_id,
            authenticated,
            request,
            &prepared_embedding,
        )
        .await
        {
            Err(error)
                if error.code == "KNOWLEDGE_RETRIEVAL_POINTER_CHANGED"
                    && pointer_retry_available =>
            {
                pointer_retry_available = false;
            }
            Err(error) if error.code == "KNOWLEDGE_RETRIEVAL_POINTER_CHANGED" => {
                break Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "KNOWLEDGE_ACTIVE_GENERATION_UNAVAILABLE",
                ));
            }
            result => break result,
        }
    };
    let terminal_state = if result.is_ok() {
        "COMPLETED"
    } else {
        "RELEASED"
    };
    sqlx::query(
        "UPDATE knowledge_query_admission_t SET state=$4
          WHERE knowledge_base_id=$1 AND consumer_host_id=$2
            AND request_id=$3 AND state='ADMITTED'",
    )
    .bind(knowledge_base_id)
    .bind(authenticated.host_id)
    .bind(request_id)
    .bind(terminal_state)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    result
}

async fn preauthorize_request(
    state: &KnowledgeState,
    knowledge_base_id: Uuid,
    authenticated: &AuthenticatedKnowledgeRequest,
    environment: &str,
) -> Result<(), ApiError> {
    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    let authorization = load_authorization(
        &mut transaction,
        knowledge_base_id,
        authenticated.host_id,
        authenticated.agent_def_id,
        environment,
        &state.heartbeat_secret,
    )
    .await?;
    authorization.validate_fresh_active(Utc::now())?;
    transaction.rollback().await.map_err(ApiError::database)
}

async fn resolve_knowledge_base_id(
    pool: &PgPool,
    consumer_host_id: Uuid,
    agent_def_id: Uuid,
    request: &RetrieveRequest,
) -> Result<Uuid, ApiError> {
    if let Some(id) = request.knowledge_base_ids.first() {
        return Ok(*id);
    }
    let ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT knowledge_base_id FROM knowledge_runtime_authorization_t
          WHERE consumer_host_id=$1 AND agent_id=$2 AND environment=$3
            AND active=TRUE ORDER BY knowledge_base_id LIMIT 2",
    )
    .bind(consumer_host_id)
    .bind(agent_def_id)
    .bind(&request.environment)
    .fetch_all(pool)
    .await
    .map_err(ApiError::database)?;
    if ids.len() != 1 {
        return Err(ApiError::from(KnowledgeError::MultipleKnowledgeBases));
    }
    Ok(ids[0])
}

async fn admit_request(
    pool: &PgPool,
    knowledge_base_id: Uuid,
    consumer_host_id: Uuid,
    request_id: &str,
) -> Result<(), ApiError> {
    let mut transaction = pool.begin().await.map_err(ApiError::database)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *transaction)
        .await
        .map_err(ApiError::database)?;
    admit(
        &mut transaction,
        knowledge_base_id,
        consumer_host_id,
        request_id,
    )
    .await?;
    transaction.commit().await.map_err(ApiError::database)
}

#[allow(clippy::too_many_arguments)]
async fn retrieve_snapshot(
    state: &KnowledgeState,
    request_id: &str,
    knowledge_base_id: Uuid,
    authenticated: &AuthenticatedKnowledgeRequest,
    request: &RetrieveRequest,
    prepared_embedding: &PreparedQueryEmbedding,
) -> Result<RetrievalResponse, ApiError> {
    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await
        .map_err(ApiError::database)?;
    let authorization = load_authorization(
        &mut transaction,
        knowledge_base_id,
        authenticated.host_id,
        authenticated.agent_def_id,
        &request.environment,
        &state.heartbeat_secret,
    )
    .await?;
    let (
        retrieval_profile_id,
        profile_top_k,
        profile_token_budget,
        lexical_candidates,
        vector_candidates,
        lexical_evidence_required,
        preferred_strategy,
        graph_failure_policy,
        qualified_strategies,
    ): (Uuid, i32, i32, i32, i32, bool, String, String, Value) = sqlx::query_as(
        "SELECT p.profile_id,p.top_k,p.token_budget,
                p.lexical_candidates,p.vector_candidates,p.lexical_evidence_required,
                p.strategy,
                COALESCE(p.graph_policy->>'failurePolicy','FALLBACK_HYBRID'),
                a.qualified_strategies
           FROM knowledge_retrieval_profile_t p
          JOIN knowledge_runtime_authorization_t a
            ON a.retrieval_profile_id=p.profile_id
         WHERE a.knowledge_base_id=$1 AND a.consumer_host_id=$2
           AND a.environment=$3 AND a.agent_id=$4 AND p.active=TRUE",
    )
    .bind(knowledge_base_id)
    .bind(authenticated.host_id)
    .bind(&request.environment)
    .bind(authenticated.agent_def_id)
    .fetch_one(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    let mut effective_request = request.clone();
    effective_request.knowledge_base_ids = vec![knowledge_base_id];
    effective_request.top_k = if request.top_k == 0 {
        usize::try_from(profile_top_k).unwrap_or(1)
    } else {
        request
            .top_k
            .min(usize::try_from(profile_top_k).unwrap_or(1))
    }
    .min(20);
    effective_request.token_budget = usize::try_from(profile_token_budget).unwrap_or(1);
    let generation = load_generation(
        &mut transaction,
        knowledge_base_id,
        &request.environment,
        &effective_request,
        lexical_candidates,
        vector_candidates,
        authenticated,
        prepared_embedding,
        state.config.features.delta_segments,
    )
    .await?;
    let started = std::time::Instant::now();
    let mut response = retrieve_resolved_generation_with_gate(
        &generation,
        &authorization,
        &effective_request,
        Utc::now(),
        lexical_evidence_required,
    )?;
    let graph_requested = state.config.features.graph_assisted
        && preferred_strategy == "GRAPH_ASSISTED"
        && qualified_strategies
            .as_array()
            .is_some_and(|values| values.iter().any(|value| value == "GRAPH_ASSISTED"));
    let mut graph_generation_id = None;
    let mut fallback_reason: Option<String> = None;
    if graph_requested {
        let fallback_to_hybrid = match graph_failure_policy.as_str() {
            "FALLBACK_HYBRID" => true,
            "FAIL_CLOSED" => false,
            _ => {
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "KNOWLEDGE_GRAPH_POLICY_INVALID",
                ));
            }
        };
        match apply_graph_assist_scoped(
            &mut transaction,
            authenticated,
            &generation,
            &mut response,
            &state.config.graph_limits,
        )
        .await
        {
            Ok(id) => graph_generation_id = Some(id),
            Err(error) if fallback_to_hybrid => {
                response.strategy = "HYBRID_FALLBACK".into();
                fallback_reason = Some(error.code.to_string());
            }
            Err(error) => return Err(error),
        }
    }
    let result_identities = response
        .results
        .iter()
        .map(|hit| json!({"chunkId": hit.chunk_id, "documentVersionId": hit.citation.document_version_id}))
        .collect::<Vec<_>>();
    let query_digest = sha256_hex(request.query.as_bytes());
    sqlx::query(
        "INSERT INTO knowledge_query_audit_t(query_audit_id,request_id,knowledge_base_id,consumer_host_id,index_generation_id,retrieval_profile_id,strategy,segment_manifest_digest,query_digest,result_identities,fallback_reason,latency_ms,graph_generation_id,planner_diagnostics) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14) ON CONFLICT(knowledge_base_id,consumer_host_id,request_id) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(request_id)
    .bind(knowledge_base_id)
    .bind(authenticated.host_id)
    .bind(generation.manifest.generation_id)
    .bind(retrieval_profile_id)
    .bind(match response.strategy.as_str() {
        "GRAPH_ASSISTED_PATH_V1" => "GRAPH_ASSISTED",
        "HYBRID_FALLBACK" => "HYBRID_FALLBACK",
        _ => "HYBRID",
    })
    .bind(&generation.manifest.manifest_digest)
    .bind(query_digest)
    .bind(Value::Array(result_identities))
    .bind(&fallback_reason)
    .bind(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX))
    .bind(graph_generation_id)
    .bind(json!({
        "contractVersion": "phase4-path-v1",
        "serverOwnedLimits": graph_requested,
        "fallback": fallback_reason.is_some()
    }))
    .execute(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    transaction.commit().await.map_err(ApiError::database)?;
    Ok(response)
}

async fn apply_graph_assist_scoped(
    transaction: &mut Transaction<'_, Postgres>,
    principal: &AuthenticatedKnowledgeRequest,
    generation: &FullBaseGeneration,
    response: &mut RetrievalResponse,
    limits: &GraphLimits,
) -> Result<Uuid, ApiError> {
    sqlx::query("SAVEPOINT knowledge_graph_assist")
        .execute(&mut **transaction)
        .await
        .map_err(ApiError::database)?;
    let result = apply_graph_assist(transaction, principal, generation, response, limits).await;
    match result {
        Ok(graph_generation_id) => {
            sqlx::query("SET LOCAL statement_timeout = 0")
                .execute(&mut **transaction)
                .await
                .map_err(ApiError::database)?;
            sqlx::query("RELEASE SAVEPOINT knowledge_graph_assist")
                .execute(&mut **transaction)
                .await
                .map_err(ApiError::database)?;
            Ok(graph_generation_id)
        }
        Err(error) => {
            sqlx::query("ROLLBACK TO SAVEPOINT knowledge_graph_assist")
                .execute(&mut **transaction)
                .await
                .map_err(ApiError::database)?;
            sqlx::query("SET LOCAL statement_timeout = 0")
                .execute(&mut **transaction)
                .await
                .map_err(ApiError::database)?;
            sqlx::query("RELEASE SAVEPOINT knowledge_graph_assist")
                .execute(&mut **transaction)
                .await
                .map_err(ApiError::database)?;
            Err(error)
        }
    }
}

async fn apply_graph_assist(
    transaction: &mut Transaction<'_, Postgres>,
    principal: &AuthenticatedKnowledgeRequest,
    generation: &FullBaseGeneration,
    response: &mut RetrievalResponse,
    limits: &GraphLimits,
) -> Result<Uuid, ApiError> {
    let estimated_bytes = response
        .results
        .iter()
        .map(|hit| hit.text.len() + hit.citation.canonical_uri.len())
        .sum::<usize>();
    let estimated_tokens = response
        .results
        .iter()
        .map(|hit| hit.text.split_whitespace().count())
        .sum::<usize>();
    if estimated_bytes > limits.maximum_memory_bytes
        || estimated_tokens > limits.maximum_token_budget
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_GRAPH_MEMORY_LIMIT_EXCEEDED",
        ));
    }
    sqlx::query(&format!(
        "SET LOCAL statement_timeout = '{}'",
        limits.timeout_ms
    ))
    .execute(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    let graph_generation_id: Uuid = sqlx::query_scalar(
        "SELECT graph.graph_generation_id
           FROM knowledge_graph_generation_t graph
          WHERE graph.index_generation_id=$1 AND graph.state='READY'
            AND graph.visibility_mode='UNIFORM_SCOPE'
            AND NOT EXISTS(SELECT 1 FROM knowledge_source_t source
                            WHERE source.knowledge_base_id=graph.knowledge_base_id
                              AND source.acl_mode<>'UNIFORM_SCOPE')
          ORDER BY graph.completed_ts DESC LIMIT 1",
    )
    .bind(generation.manifest.generation_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_GRAPH_ARTIFACT_UNAVAILABLE",
        )
    })?;
    let seed_ids = response
        .results
        .iter()
        .take(limits.maximum_seeds)
        .map(|hit| hit.chunk_id)
        .collect::<Vec<_>>();
    if seed_ids.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_GRAPH_SEED_UNAVAILABLE",
        ));
    }
    let result_ids = response
        .results
        .iter()
        .take(limits.maximum_evidence_chunks)
        .map(|hit| hit.chunk_id)
        .collect::<Vec<_>>();
    let rows = sqlx::query(
        "WITH eligible AS (
           SELECT member.chunk_id
             FROM knowledge_resolved_generation_chunk($2) member
             JOIN knowledge_document_t document ON document.document_id=member.document_id
            WHERE knowledge_document_acl_authorized(
                    document.document_id,$3,$6,$4::text[],$5::text[])
         ), entity_status AS (
           SELECT entity.graph_entity_id,entity.entity_type,entity.normalized_key,
                  bool_and(EXISTS(SELECT 1 FROM eligible
                                   WHERE eligible.chunk_id=contribution.chunk_id)) AS authorized,
                  COALESCE(array_agg(DISTINCT contribution.chunk_id)
                    FILTER(WHERE contribution.chunk_id=ANY($7::uuid[])),
                    '{}'::uuid[]) AS seed_chunks,
                  COALESCE(array_agg(DISTINCT contribution.chunk_id)
                    FILTER(WHERE contribution.chunk_id=ANY($8::uuid[])),
                    '{}'::uuid[]) AS result_chunks
             FROM knowledge_graph_entity_t entity
             JOIN knowledge_graph_entity_contribution_t contribution
               ON contribution.graph_entity_id=entity.graph_entity_id
              AND contribution.graph_generation_id=$1
            WHERE entity.graph_generation_id=$1
            GROUP BY entity.graph_entity_id,entity.entity_type,entity.normalized_key
         ), authorized_entity AS (
           SELECT * FROM entity_status WHERE authorized
         ), authorized_relation AS (
           SELECT relation.graph_relation_id,relation.relation_type,
                  subject.graph_entity_id AS subject_entity_id,
                  object.graph_entity_id AS object_entity_id,
                  subject.normalized_key AS subject_key,
                  object.normalized_key AS object_key,
                  subject.seed_chunks AS subject_seed_chunks,
                  object.seed_chunks AS object_seed_chunks,
                  subject.result_chunks AS subject_result_chunks,
                  object.result_chunks AS object_result_chunks
             FROM knowledge_graph_relation_t relation
             JOIN knowledge_graph_relation_contribution_t contribution
               ON contribution.graph_relation_id=relation.graph_relation_id
              AND contribution.graph_generation_id=$1
             JOIN authorized_entity subject
               ON subject.graph_entity_id=relation.subject_entity_id
             JOIN authorized_entity object
               ON object.graph_entity_id=relation.object_entity_id
            WHERE relation.graph_generation_id=$1
              AND subject.entity_type<>'REPOSITORY'
              AND object.entity_type<>'REPOSITORY'
            GROUP BY relation.graph_relation_id,relation.relation_type,
                     subject.graph_entity_id,object.graph_entity_id,
                     subject.normalized_key,object.normalized_key,
                     subject.seed_chunks,object.seed_chunks,
                     subject.result_chunks,object.result_chunks
           HAVING bool_and(EXISTS(SELECT 1 FROM eligible
                                   WHERE eligible.chunk_id=contribution.chunk_id))
         )
         SELECT graph_relation_id,relation_type,subject_entity_id,object_entity_id,
                subject_key,object_key,
                subject_seed_chunks,object_seed_chunks,
                subject_result_chunks,object_result_chunks
           FROM authorized_relation
          ORDER BY relation_type,subject_key,object_key,graph_relation_id
          LIMIT $9",
    )
    .bind(graph_generation_id)
    .bind(generation.manifest.generation_id)
    .bind(&principal.subject_id)
    .bind(&principal.groups)
    .bind(&principal.organizations)
    .bind(&principal.subject_type)
    .bind(seed_ids)
    .bind(result_ids)
    .bind(i64::try_from(limits.maximum_pairs).unwrap_or(256))
    .fetch_all(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    let deadline = Instant::now() + StdDuration::from_millis(limits.timeout_ms);
    let mut adjacency = BTreeMap::<Uuid, Vec<Uuid>>::new();
    let mut seed_entities = BTreeMap::<String, Uuid>::new();
    let mut result_chunks = HashMap::<Uuid, BTreeSet<Uuid>>::new();
    for row in rows {
        let subject: Uuid = row.get("subject_entity_id");
        let object: Uuid = row.get("object_entity_id");
        adjacency.entry(subject).or_default().push(object);
        adjacency.entry(object).or_default().push(subject);
        if !row.get::<Vec<Uuid>, _>("subject_seed_chunks").is_empty() {
            seed_entities.insert(row.get("subject_key"), subject);
        }
        if !row.get::<Vec<Uuid>, _>("object_seed_chunks").is_empty() {
            seed_entities.insert(row.get("object_key"), object);
        }
        result_chunks
            .entry(subject)
            .or_default()
            .extend(row.get::<Vec<Uuid>, _>("subject_result_chunks"));
        result_chunks
            .entry(object)
            .or_default()
            .extend(row.get::<Vec<Uuid>, _>("object_result_chunks"));
    }
    let scores = bounded_graph_scores(
        adjacency,
        seed_entities.into_values().collect(),
        result_chunks,
        limits,
        deadline,
    )
    .map_err(|code| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, code))?;
    for hit in &mut response.results {
        hit.path_retrieval_score = scores.get(&hit.chunk_id).copied();
    }
    response.results.sort_by(|left, right| {
        right
            .path_retrieval_score
            .unwrap_or(0.0)
            .partial_cmp(&left.path_retrieval_score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                right
                    .fused_score
                    .partial_cmp(&left.fused_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| left.chunk_id.cmp(&right.chunk_id))
    });
    response.strategy = "GRAPH_ASSISTED_PATH_V1".into();
    Ok(graph_generation_id)
}

fn bounded_graph_scores(
    mut adjacency: BTreeMap<Uuid, Vec<Uuid>>,
    seed_entities: Vec<Uuid>,
    result_chunks: HashMap<Uuid, BTreeSet<Uuid>>,
    limits: &GraphLimits,
    deadline: Instant,
) -> Result<HashMap<Uuid, f64>, &'static str> {
    if seed_entities.is_empty() {
        return Err("KNOWLEDGE_GRAPH_SEED_UNAVAILABLE");
    }
    for neighbours in adjacency.values_mut() {
        let mut seen = HashSet::new();
        neighbours.retain(|entity_id| seen.insert(*entity_id));
    }
    let mut queue = VecDeque::new();
    let mut visited = BTreeMap::<Uuid, usize>::new();
    for seed in seed_entities.into_iter().take(limits.maximum_seeds) {
        if visited.len() >= limits.maximum_visited_nodes {
            break;
        }
        visited.insert(seed, 0);
        queue.push_back(seed);
    }
    let mut edge_visits = 0_usize;
    let mut path_count = 0_usize;
    let mut traversal_exhausted = false;
    while let Some(entity_id) = queue.pop_front() {
        if Instant::now() >= deadline || edge_visits >= limits.maximum_visited_edges {
            traversal_exhausted = true;
            break;
        }
        let depth = visited[&entity_id];
        if depth >= limits.maximum_hops {
            continue;
        }
        for neighbour in adjacency
            .get(&entity_id)
            .into_iter()
            .flatten()
            .take(limits.maximum_fan_out)
        {
            if edge_visits >= limits.maximum_visited_edges || Instant::now() >= deadline {
                traversal_exhausted = true;
                break;
            }
            edge_visits += 1;
            if !visited.contains_key(neighbour) {
                if visited.len() >= limits.maximum_visited_nodes
                    || path_count >= limits.maximum_paths
                {
                    traversal_exhausted = true;
                    break;
                }
                path_count += 1;
                visited.insert(*neighbour, depth + 1);
                queue.push_back(*neighbour);
            }
        }
    }
    if traversal_exhausted {
        return Err("KNOWLEDGE_GRAPH_TRAVERSAL_LIMIT_EXCEEDED");
    }
    let mut scores = HashMap::<Uuid, f64>::new();
    for (entity_id, depth) in visited {
        let score = 1.0 / (depth as f64 + 1.0);
        for chunk_id in result_chunks.get(&entity_id).into_iter().flatten() {
            scores
                .entry(*chunk_id)
                .and_modify(|current| *current = current.max(score))
                .or_insert(score);
        }
    }
    Ok(scores)
}

async fn admit(
    transaction: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    consumer_host_id: Uuid,
    request_id: &str,
) -> Result<(), ApiError> {
    let quota = sqlx::query("SELECT max_concurrency, requests_per_minute FROM knowledge_consumer_quota_t WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND active=TRUE FOR UPDATE")
        .bind(knowledge_base_id).bind(consumer_host_id)
        .fetch_optional(&mut **transaction).await.map_err(ApiError::database)?
        .ok_or_else(|| ApiError::new(StatusCode::FORBIDDEN, "KNOWLEDGE_QUOTA_NOT_CONFIGURED"))?;
    let existing: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM knowledge_query_admission_t
          WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND request_id=$3)",
    )
    .bind(knowledge_base_id)
    .bind(consumer_host_id)
    .bind(request_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    if existing {
        return Ok(());
    }
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_query_admission_t WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND state='ADMITTED' AND lease_expires_ts>now()")
        .bind(knowledge_base_id).bind(consumer_host_id)
        .fetch_one(&mut **transaction).await.map_err(ApiError::database)?;
    let recent: i64 = sqlx::query_scalar("SELECT count(*) FROM knowledge_query_admission_t WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND admitted_ts>=date_trunc('minute',now())")
        .bind(knowledge_base_id).bind(consumer_host_id)
        .fetch_one(&mut **transaction).await.map_err(ApiError::database)?;
    if active
        >= i64::from(
            quota
                .try_get::<i32, _>("max_concurrency")
                .map_err(ApiError::database)?,
        )
        || recent
            >= i64::from(
                quota
                    .try_get::<i32, _>("requests_per_minute")
                    .map_err(ApiError::database)?,
            )
    {
        return Err(ApiError::from(KnowledgeError::QuotaExhausted));
    }
    sqlx::query("INSERT INTO knowledge_query_admission_t(admission_id,knowledge_base_id,consumer_host_id,request_id,lease_expires_ts,reserved_cost_micros) VALUES($1,$2,$3,$4,now()+interval '30 seconds',0) ON CONFLICT(knowledge_base_id,consumer_host_id,request_id) DO NOTHING")
        .bind(Uuid::now_v7()).bind(knowledge_base_id).bind(consumer_host_id).bind(request_id)
        .execute(&mut **transaction).await.map_err(ApiError::database)?;
    Ok(())
}

async fn load_authorization(
    transaction: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    consumer_host_id: Uuid,
    agent_def_id: Uuid,
    environment: &str,
    _heartbeat_secret: &[u8],
) -> Result<AuthorizationSnapshot, ApiError> {
    let row = sqlx::query(
        "SELECT a.active,a.desired_event_sequence,a.applied_event_sequence,
        a.lease_expires_ts AS authorization_lease,s.lease_expires_ts AS projector_lease
        FROM knowledge_runtime_authorization_t a
        JOIN knowledge_control_snapshot_t s ON s.snapshot_id::text=a.projector_id
        WHERE a.knowledge_base_id=$1 AND a.consumer_host_id=$2 AND a.environment=$3
          AND a.agent_id=$4 AND s.state='APPLIED'",
    )
    .bind(knowledge_base_id)
    .bind(consumer_host_id)
    .bind(environment)
    .bind(agent_def_id)
    .fetch_optional(&mut **transaction)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| ApiError::from(KnowledgeError::AuthorizationDenied))?;
    Ok(AuthorizationSnapshot {
        knowledge_base_id,
        consumer_host_id,
        environment: environment.to_string(),
        active: row.try_get("active").map_err(ApiError::database)?,
        desired_event_sequence: u64::try_from(
            row.try_get::<i64, _>("desired_event_sequence")
                .map_err(ApiError::database)?,
        )
        .unwrap_or(0),
        applied_event_sequence: u64::try_from(
            row.try_get::<i64, _>("applied_event_sequence")
                .map_err(ApiError::database)?,
        )
        .unwrap_or(0),
        authorization_lease_expires_at: row
            .try_get("authorization_lease")
            .map_err(ApiError::database)?,
        projector_lease_expires_at: row.try_get("projector_lease").map_err(ApiError::database)?,
    })
}

fn normalized_query(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn query_cache_key(
    state: &KnowledgeState,
    query: &str,
    policy_digest: &str,
    data_boundary_digest: &str,
    space_id: &str,
    space_revision: u64,
    dimension: usize,
) -> Result<String, ApiError> {
    let mut signer = Hmac::<Sha256>::new_from_slice(&state.query_cache_key).map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        )
    })?;
    let revision = space_revision.to_string();
    let dimension = dimension.to_string();
    for part in [
        policy_digest,
        data_boundary_digest,
        space_id,
        revision.as_str(),
        dimension.as_str(),
        "query-v1",
        query,
    ] {
        signer.update(part.as_bytes());
        signer.update(&[0]);
    }
    Ok(signer
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[allow(clippy::too_many_arguments)]
async fn prepare_query_embedding(
    state: &KnowledgeState,
    knowledge_base_id: Uuid,
    environment: &str,
    query: &str,
    policy_digest: &str,
    data_boundary_digest: &str,
) -> Result<PreparedQueryEmbedding, ApiError> {
    let row = sqlx::query(
        "SELECT pointer.index_generation_id,pointer.pointer_version,
                generation.space_id,generation.space_revision,generation.dimension
           FROM knowledge_index_pointer_t pointer
           JOIN knowledge_index_generation_t generation
             ON generation.index_generation_id=pointer.index_generation_id
          WHERE pointer.knowledge_base_id=$1 AND pointer.environment=$2
            AND generation.state='PROMOTED'",
    )
    .bind(knowledge_base_id)
    .bind(environment)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_ACTIVE_GENERATION_UNAVAILABLE",
        )
    })?;
    let generation_id = row
        .try_get("index_generation_id")
        .map_err(ApiError::database)?;
    let pointer_version = row.try_get("pointer_version").map_err(ApiError::database)?;
    let space_id: String = row.try_get("space_id").map_err(ApiError::database)?;
    let space_revision = u64::try_from(
        row.try_get::<i64, _>("space_revision")
            .map_err(ApiError::database)?,
    )
    .unwrap_or(0);
    let dimension = usize::try_from(
        row.try_get::<i32, _>("dimension")
            .map_err(ApiError::database)?,
    )
    .unwrap_or(0);
    let vector = query_embedding(
        state,
        query,
        policy_digest,
        data_boundary_digest,
        &space_id,
        space_revision,
        dimension,
    )
    .await?;
    Ok(PreparedQueryEmbedding {
        generation_id,
        pointer_version,
        space_id,
        space_revision,
        dimension,
        vector,
    })
}

#[allow(clippy::too_many_arguments)]
async fn query_embedding(
    state: &KnowledgeState,
    query: &str,
    policy_digest: &str,
    data_boundary_digest: &str,
    space_id: &str,
    space_revision: u64,
    dimension: usize,
) -> Result<Vec<f32>, ApiError> {
    if space_id != state.config.embedding_space_id
        || space_revision != state.config.embedding_space_revision
        || dimension != state.config.embedding_dimension
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_EMBEDDING_SPACE_MISMATCH",
        ));
    }
    if state.config.deterministic_pilot {
        return Ok(knowledge_core::fake_embedding(query));
    }
    let normalized = normalized_query(query);
    let key = query_cache_key(
        state,
        &normalized,
        policy_digest,
        data_boundary_digest,
        space_id,
        space_revision,
        dimension,
    )?;
    {
        let mut cache = state.query_cache.lock().await;
        let now = Instant::now();
        cache.entries.retain(|_, entry| entry.expires_at > now);
        cache.stored_bytes = cache
            .entries
            .values()
            .map(|entry| entry.vector.len() * std::mem::size_of::<f32>())
            .sum();
        if let Some(entry) = cache.entries.get(&key) {
            return Ok(entry.vector.clone());
        }
    }
    let endpoint = state
        .config
        .embedding_gateway_url
        .as_deref()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
            )
        })?;
    let token = read_secret_file(
        state
            .config
            .embedding_authorization_file
            .as_ref()
            .ok_or_else(|| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
                )
            })?,
    )
    .map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        )
    })?;
    let expected_revision = space_revision.to_string();
    let response = state
        .embedding_client
        .post(endpoint)
        .bearer_auth(token)
        .header("x-request-id", format!("kb-query:{key}"))
        .header("x-light-expected-embedding-space-id", space_id)
        .header(
            "x-light-expected-embedding-space-revision",
            &expected_revision,
        )
        .json(&json!({
            "model": state.config.embedding_alias,
            "input": [normalized],
            "dimensions": dimension
        }))
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
            )
        })?;
    let response_space = response
        .headers()
        .get("x-light-embedding-space-id")
        .and_then(|value| value.to_str().ok());
    let response_revision = response
        .headers()
        .get("x-light-embedding-space-revision")
        .and_then(|value| value.to_str().ok());
    if !response.status().is_success()
        || response_space != Some(space_id)
        || response_revision != Some(expected_revision.as_str())
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        ));
    }
    let body: Value = response.json().await.map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        )
    })?;
    let data = body
        .get("data")
        .and_then(Value::as_array)
        .filter(|data| data.len() == 1)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
            )
        })?;
    let item = &data[0];
    if item.get("index").and_then(Value::as_u64) != Some(0) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        ));
    }
    let vector = item
        .get("embedding")
        .and_then(Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(|value| value.as_f64().map(|value| value as f32))
                .collect::<Option<Vec<_>>>()
        })
        .filter(|vector| vector.len() == dimension && vector.iter().all(|value| value.is_finite()))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
            )
        })?;
    let vector_bytes = vector.len() * std::mem::size_of::<f32>();
    let mut cache = state.query_cache.lock().await;
    while cache.entries.len() >= state.config.query_cache_maximum_entries
        || cache.stored_bytes.saturating_add(vector_bytes) > state.config.query_cache_maximum_bytes
    {
        let Some(oldest) = cache
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.stored_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        if let Some(removed) = cache.entries.remove(&oldest) {
            cache.stored_bytes = cache
                .stored_bytes
                .saturating_sub(removed.vector.len() * std::mem::size_of::<f32>());
        }
    }
    cache.stored_bytes = cache.stored_bytes.saturating_add(vector_bytes);
    cache.entries.insert(
        key,
        QueryEmbeddingCacheEntry {
            vector: vector.clone(),
            expires_at: Instant::now()
                + StdDuration::from_secs(state.config.query_cache_ttl_seconds),
            stored_at: Instant::now(),
        },
    );
    Ok(vector)
}

async fn load_generation(
    transaction: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    environment: &str,
    request: &RetrieveRequest,
    lexical_candidates: i32,
    vector_candidates: i32,
    principal: &AuthenticatedKnowledgeRequest,
    prepared_embedding: &PreparedQueryEmbedding,
    delta_segments_enabled: bool,
) -> Result<FullBaseGeneration, ApiError> {
    let row = sqlx::query("SELECT g.index_generation_id,p.pointer_version,s.index_segment_id,g.snapshot_watermark,g.parser_contract_digest,g.chunker_contract_digest,g.lexical_contract_digest,g.citation_contract_digest,g.space_id,g.space_revision,g.dimension,COALESCE(g.ordered_segment_manifest_digest,s.manifest_digest) AS manifest_digest FROM knowledge_index_pointer_t p JOIN knowledge_index_generation_t g ON g.index_generation_id=p.index_generation_id JOIN knowledge_generation_segment_t m ON m.index_generation_id=g.index_generation_id AND m.ordinal=0 JOIN knowledge_index_segment_t s ON s.index_segment_id=m.index_segment_id AND s.segment_kind='BASE' AND s.state='READY' WHERE p.knowledge_base_id=$1 AND p.environment=$2 AND g.state='PROMOTED'")
        .bind(knowledge_base_id).bind(environment)
        .fetch_optional(&mut **transaction).await.map_err(ApiError::database)?
        .ok_or_else(|| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "KNOWLEDGE_ACTIVE_GENERATION_UNAVAILABLE"))?;
    let generation_id: Uuid = row
        .try_get("index_generation_id")
        .map_err(ApiError::database)?;
    let segment_id: Uuid = row
        .try_get("index_segment_id")
        .map_err(ApiError::database)?;
    let space_id: String = row.try_get("space_id").map_err(ApiError::database)?;
    let space_revision = u64::try_from(
        row.try_get::<i64, _>("space_revision")
            .map_err(ApiError::database)?,
    )
    .unwrap_or(0);
    let dimension = usize::try_from(
        row.try_get::<i32, _>("dimension")
            .map_err(ApiError::database)?,
    )
    .unwrap_or(0);
    let pointer_version: i64 = row.try_get("pointer_version").map_err(ApiError::database)?;
    if generation_id != prepared_embedding.generation_id
        || pointer_version != prepared_embedding.pointer_version
        || space_id != prepared_embedding.space_id
        || space_revision != prepared_embedding.space_revision
        || dimension != prepared_embedding.dimension
    {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_RETRIEVAL_POINTER_CHANGED",
        ));
    }
    let query_vector = format!(
        "[{}]",
        prepared_embedding
            .vector
            .iter()
            .map(f32::to_string)
            .collect::<Vec<_>>()
            .join(",")
    );
    let source_ids = request
        .filters
        .as_ref()
        .map(|filters| filters.source_ids.clone())
        .unwrap_or_default();
    if request.filters.as_ref().is_some_and(|filters| {
        filters.languages.iter().any(|language| {
            !language.eq_ignore_ascii_case("en") && !language.eq_ignore_ascii_case("english")
        })
    }) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "KNOWLEDGE_UNSUPPORTED_CONTRACT",
        ));
    }
    let rows = sqlx::query(
        "WITH generation_segments AS (
           SELECT gs.index_segment_id,gs.ordinal
             FROM knowledge_generation_segment_t gs
             JOIN knowledge_index_segment_t s
               ON s.index_segment_id=gs.index_segment_id AND s.state='READY'
            WHERE gs.index_generation_id=$1
         ), eligible_chunks AS (
           SELECT member.index_segment_id,member.chunk_id,gs.ordinal
             FROM generation_segments gs
             JOIN knowledge_segment_chunk_t member
               ON member.index_segment_id=gs.index_segment_id
             JOIN knowledge_chunk_t chunk ON chunk.chunk_id=member.chunk_id
             JOIN knowledge_document_version_t document_version
               ON document_version.document_version_id=chunk.document_version_id
             JOIN knowledge_document_t document
               ON document.document_id=document_version.document_id
            WHERE NOT EXISTS (
              SELECT 1
                FROM generation_segments later
                JOIN knowledge_segment_operation_t operation
                  ON operation.index_segment_id=later.index_segment_id
               WHERE later.ordinal>gs.ordinal
                 AND operation.document_id=chunk.document_id
                 AND (operation.operation_kind IN (
                       'SUPERSEDE_DOCUMENT','TOMBSTONE_DOCUMENT')
                      OR (operation.operation_kind='TOMBSTONE_CHUNK'
                          AND operation.chunk_id=member.chunk_id))
            )
              AND knowledge_document_acl_authorized(
                document.document_id,$7,$10,$8::text[],$9::text[]
              )
         ), lexical_ranked AS (
           SELECT c.chunk_id,
                  row_number() OVER (ORDER BY
                    ts_rank_cd(c.lexical_input,
                      plainto_tsquery('english',$2)) DESC,c.chunk_id) AS lexical_rank
             FROM eligible_chunks member
             JOIN knowledge_chunk_t c ON c.chunk_id=member.chunk_id
             JOIN knowledge_document_version_t dv
               ON dv.document_version_id=c.document_version_id
             JOIN knowledge_document_t d ON d.document_id=dv.document_id
            WHERE c.lexical_input @@ plainto_tsquery('english',$2)
              AND (cardinality($6::uuid[])=0 OR d.source_id=ANY($6::uuid[]))
            ORDER BY lexical_rank
            LIMIT $4
         ), vector_ranked AS (
           SELECT v.chunk_id,
                  row_number() OVER (ORDER BY
                    v.projection <=> $3::vector,v.chunk_id) AS vector_rank
             FROM eligible_chunks member
             JOIN knowledge_segment_vector_t v
               ON v.index_segment_id=member.index_segment_id
              AND v.chunk_id=member.chunk_id
             JOIN knowledge_chunk_t c ON c.chunk_id=v.chunk_id
             JOIN knowledge_document_version_t dv
               ON dv.document_version_id=c.document_version_id
             JOIN knowledge_document_t d ON d.document_id=dv.document_id
            WHERE (cardinality($6::uuid[])=0 OR d.source_id=ANY($6::uuid[]))
            ORDER BY vector_rank
            LIMIT $5
         ), candidates AS (
           SELECT chunk_id,min(lexical_rank)::bigint AS lexical_rank,
                  min(vector_rank)::bigint AS vector_rank
             FROM (
               SELECT chunk_id,lexical_rank,NULL::bigint AS vector_rank
                 FROM lexical_ranked
               UNION ALL
               SELECT chunk_id,NULL::bigint,vector_rank FROM vector_ranked
             ) ranked
            GROUP BY chunk_id
         )
         SELECT c.chunk_id,c.document_version_id,d.document_id,
                dv.source_version,d.canonical_uri,c.ordinal,c.section_path,
                c.start_offset,c.end_offset,c.chunk_text,c.token_count,
                c.content_digest,v.projection::text AS projection,
                candidate.lexical_rank,candidate.vector_rank
           FROM candidates candidate
           JOIN eligible_chunks member ON member.chunk_id=candidate.chunk_id
           JOIN knowledge_chunk_t c ON c.chunk_id=member.chunk_id
           JOIN knowledge_document_version_t dv
             ON dv.document_version_id=c.document_version_id
           JOIN knowledge_document_t d ON d.document_id=dv.document_id
           JOIN knowledge_segment_vector_t v
             ON v.index_segment_id=member.index_segment_id
            AND v.chunk_id=member.chunk_id
          ORDER BY c.chunk_id",
    )
    .bind(generation_id)
    .bind(&request.query)
    .bind(query_vector)
    .bind(lexical_candidates.max(1))
    .bind(vector_candidates.max(1))
    .bind(source_ids)
    .bind(&principal.subject_id)
    .bind(&principal.groups)
    .bind(&principal.organizations)
    .bind(&principal.subject_type)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    let chunks = rows
        .into_iter()
        .map(|chunk| -> Result<Chunk, ApiError> {
            let projection: String = chunk.try_get("projection").map_err(ApiError::database)?;
            Ok(Chunk {
                chunk_id: chunk.try_get("chunk_id").map_err(ApiError::database)?,
                document_id: chunk.try_get("document_id").map_err(ApiError::database)?,
                document_version_id: chunk
                    .try_get("document_version_id")
                    .map_err(ApiError::database)?,
                source_object_id: String::new(),
                canonical_uri: chunk.try_get("canonical_uri").map_err(ApiError::database)?,
                source_version: chunk
                    .try_get("source_version")
                    .map_err(ApiError::database)?,
                ordinal: usize::try_from(
                    chunk
                        .try_get::<i32, _>("ordinal")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(0),
                section_path: chunk
                    .try_get::<Value, _>("section_path")
                    .map_err(ApiError::database)?
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|value| value.as_str().map(str::to_string))
                    .collect(),
                start_offset: usize::try_from(
                    chunk
                        .try_get::<i64, _>("start_offset")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(0),
                end_offset: usize::try_from(
                    chunk
                        .try_get::<i64, _>("end_offset")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(0),
                text: chunk.try_get("chunk_text").map_err(ApiError::database)?,
                token_count: usize::try_from(
                    chunk
                        .try_get::<i32, _>("token_count")
                        .map_err(ApiError::database)?,
                )
                .unwrap_or(0),
                content_digest: chunk
                    .try_get::<String, _>("content_digest")
                    .map_err(ApiError::database)?
                    .trim()
                    .to_string(),
                vector: parse_vector(&projection, dimension)?,
                lexical_rank: chunk
                    .try_get::<Option<i64>, _>("lexical_rank")
                    .map_err(ApiError::database)?
                    .and_then(|rank| usize::try_from(rank).ok()),
                vector_rank: chunk
                    .try_get::<Option<i64>, _>("vector_rank")
                    .map_err(ApiError::database)?
                    .and_then(|rank| usize::try_from(rank).ok()),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(FullBaseGeneration {
        manifest: BaseManifest {
            generation_id,
            segment_id,
            knowledge_base_id,
            snapshot_watermark: u64::try_from(
                row.try_get::<i64, _>("snapshot_watermark")
                    .map_err(ApiError::database)?,
            )
            .unwrap_or(0),
            document_count: chunks
                .iter()
                .map(|chunk| chunk.document_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            chunk_count: chunks.len(),
            vector_count: chunks.len(),
            parser_digest: row
                .try_get::<String, _>("parser_contract_digest")
                .map_err(ApiError::database)?
                .trim()
                .to_string(),
            chunker_digest: row
                .try_get::<String, _>("chunker_contract_digest")
                .map_err(ApiError::database)?
                .trim()
                .to_string(),
            lexical_digest: row
                .try_get::<String, _>("lexical_contract_digest")
                .map_err(ApiError::database)?
                .trim()
                .to_string(),
            citation_digest: row
                .try_get::<String, _>("citation_contract_digest")
                .map_err(ApiError::database)?
                .trim()
                .to_string(),
            space_id,
            space_revision,
            dimension,
            manifest_digest: row
                .try_get::<String, _>("manifest_digest")
                .map_err(ApiError::database)?
                .trim()
                .to_string(),
            segment_kind: if delta_segments_enabled {
                "BASE+DELTA".into()
            } else {
                "BASE".into()
            },
        },
        chunks,
    })
}

fn parse_vector(value: &str, expected_dimension: usize) -> Result<Vec<f32>, ApiError> {
    let vector = value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter(|item| !item.trim().is_empty())
        .map(|item| item.trim().parse::<f32>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "KNOWLEDGE_VECTOR_INVALID",
            )
        })?;
    if vector.len() != expected_dimension {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "KNOWLEDGE_VECTOR_DIMENSION_MISMATCH",
        ));
    }
    Ok(vector)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentVersionResponse {
    document_version_id: Uuid,
    chunks: Vec<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PassageAnchorResponse {
    passage_anchor_id: Uuid,
    document_version_id: Uuid,
    chunk_id: Uuid,
    continuity_state: String,
}

async fn passage_anchor_handler(
    State(state): State<Arc<KnowledgeState>>,
    Path((document_id, passage_anchor_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Json<PassageAnchorResponse>, ApiError> {
    let authenticated =
        authenticated_context(&headers, &state, DelegationKind::KnowledgeRetrieve).await?;
    let knowledge_base_id = required_uuid_header(&headers, "x-knowledge-base-id")?;
    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    load_authorization(
        &mut transaction,
        knowledge_base_id,
        authenticated.host_id,
        authenticated.agent_def_id,
        &authenticated.environment,
        &state.heartbeat_secret,
    )
    .await?
    .validate_fresh_active(Utc::now())?;
    enforce_normalized_claims_for_knowledge_base(
        &mut transaction,
        knowledge_base_id,
        &authenticated,
    )
    .await?;
    let rows = sqlx::query(
        "SELECT anchor.document_version_id,anchor.chunk_id,anchor.continuity_state,
                anchor.anchor_sequence
           FROM knowledge_passage_anchor_t anchor
           JOIN knowledge_document_t document
             ON document.document_id=anchor.document_id
            AND document.knowledge_base_id=anchor.knowledge_base_id
          WHERE anchor.knowledge_base_id=$1 AND anchor.document_id=$2
            AND anchor.passage_anchor_id=$3
            AND anchor.document_version_id=document.current_document_version_id
            AND knowledge_document_acl_authorized(
                  document.document_id,$4,$5,$6::text[],$7::text[]
                )
          ORDER BY anchor.anchor_sequence DESC LIMIT 2",
    )
    .bind(knowledge_base_id)
    .bind(document_id)
    .bind(passage_anchor_id)
    .bind(&authenticated.subject_id)
    .bind(&authenticated.subject_type)
    .bind(&authenticated.groups)
    .bind(&authenticated.organizations)
    .fetch_all(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    let row = rows
        .first()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "KNOWLEDGE_NOT_FOUND"))?;
    let continuity_state: String = row.get("continuity_state");
    if continuity_state == "AMBIGUOUS"
        || rows.get(1).is_some_and(|other| {
            other.get::<i64, _>("anchor_sequence") == row.get::<i64, _>("anchor_sequence")
        })
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "KNOWLEDGE_STATE_CONFLICT",
        ));
    }
    if continuity_state == "RETIRED" {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "KNOWLEDGE_NOT_FOUND"));
    }
    transaction.commit().await.map_err(ApiError::database)?;
    Ok(Json(PassageAnchorResponse {
        passage_anchor_id,
        document_version_id: row.get("document_version_id"),
        chunk_id: row.get("chunk_id"),
        continuity_state,
    }))
}

async fn document_version_handler(
    State(state): State<Arc<KnowledgeState>>,
    Path((document_id, document_version_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Json<DocumentVersionResponse>, ApiError> {
    let authenticated =
        authenticated_context(&headers, &state, DelegationKind::KnowledgeRetrieve).await?;
    let knowledge_base_id = required_uuid_header(&headers, "x-knowledge-base-id")?;
    Ok(Json(
        load_document_version(
            &state,
            &authenticated,
            knowledge_base_id,
            document_id,
            document_version_id,
        )
        .await?,
    ))
}

async fn load_document_version(
    state: &KnowledgeState,
    authenticated: &AuthenticatedKnowledgeRequest,
    knowledge_base_id: Uuid,
    document_id: Uuid,
    document_version_id: Uuid,
) -> Result<DocumentVersionResponse, ApiError> {
    let environment = authenticated.environment.clone();
    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    let authorization = load_authorization(
        &mut transaction,
        knowledge_base_id,
        authenticated.host_id,
        authenticated.agent_def_id,
        &environment,
        &state.heartbeat_secret,
    )
    .await?;
    authorization.validate_fresh_active(Utc::now())?;
    enforce_normalized_claims_for_knowledge_base(
        &mut transaction,
        knowledge_base_id,
        authenticated,
    )
    .await?;
    let chunks: Vec<Value> = sqlx::query("SELECT c.chunk_id,c.ordinal,c.section_path,c.start_offset,c.end_offset,c.chunk_text,c.content_digest FROM knowledge_chunk_t c JOIN knowledge_document_version_t v ON v.document_version_id=c.document_version_id WHERE c.knowledge_base_id=$1 AND v.document_id=$2 AND c.document_version_id=$3 AND knowledge_document_acl_authorized(v.document_id,$4,$5,$6::text[],$7::text[]) ORDER BY c.ordinal")
        .bind(knowledge_base_id).bind(document_id).bind(document_version_id)
        .bind(&authenticated.subject_id).bind(&authenticated.subject_type)
        .bind(&authenticated.groups).bind(&authenticated.organizations)
        .fetch_all(&mut *transaction).await.map_err(ApiError::database)?
        .into_iter().map(|row| json!({
            "chunkId": row.get::<Uuid, _>("chunk_id"),
            "ordinal": row.get::<i32, _>("ordinal"),
            "sectionPath": row.get::<Value, _>("section_path"),
            "startOffset": row.get::<i64, _>("start_offset"),
            "endOffset": row.get::<i64, _>("end_offset"),
        "text": row.get::<String, _>("chunk_text"),
        "contentDigest": row.get::<String, _>("content_digest").trim()
    })).collect();
    if chunks.is_empty() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "KNOWLEDGE_NOT_FOUND"));
    }
    transaction.commit().await.map_err(ApiError::database)?;
    Ok(DocumentVersionResponse {
        document_version_id,
        chunks,
    })
}

async fn enforce_normalized_claims_for_knowledge_base(
    transaction: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    authenticated: &AuthenticatedKnowledgeRequest,
) -> Result<(), ApiError> {
    if authenticated.normalized_claims_present {
        return Ok(());
    }
    let requires_normalized_claims = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
           SELECT 1 FROM knowledge_source_t
            WHERE knowledge_base_id=$1 AND acl_mode='MIRROR_SOURCE_ACL'
         )",
    )
    .bind(knowledge_base_id)
    .fetch_one(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    if requires_normalized_claims {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "KNOWLEDGE_DELEGATION_BINDING_INVALID",
        ));
    }
    Ok(())
}

fn required_uuid_header(headers: &HeaderMap, name: &'static str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(&required_text_header(headers, name)?)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "KNOWLEDGE_INVALID_REQUEST"))
}

fn required_text_header(headers: &HeaderMap, name: &'static str) -> Result<String, ApiError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "KNOWLEDGE_INVALID_REQUEST"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: String,
}

impl ApiError {
    fn new(status: StatusCode, code: impl Into<String>) -> Self {
        Self {
            status,
            code: code.into(),
        }
    }
    fn database(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "Knowledge database operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "KNOWLEDGE_DEPENDENCY_UNAVAILABLE",
        )
    }
}

impl From<KnowledgeError> for ApiError {
    fn from(error: KnowledgeError) -> Self {
        match error {
            KnowledgeError::MultipleKnowledgeBases => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "KNOWLEDGE_UNSUPPORTED_CONTRACT",
            ),
            KnowledgeError::StaleAuthorization => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "KNOWLEDGE_PROJECTION_STALE",
            ),
            KnowledgeError::AuthorizationDenied => {
                Self::new(StatusCode::FORBIDDEN, "KNOWLEDGE_FORBIDDEN")
            }
            KnowledgeError::QuotaExhausted => {
                Self::new(StatusCode::TOO_MANY_REQUESTS, "KNOWLEDGE_QUOTA_EXCEEDED")
            }
            _ => Self::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"code": self.code}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn binary_hmac_secrets_are_not_trimmed_or_utf8_decoded() {
        let directory = tempfile::tempdir().unwrap();
        let secret = directory.path().join("binary-secret");
        let expected = b"\x00 leading and trailing \xff\n";
        fs::write(&secret, expected).unwrap();
        assert_eq!(read_secret_bytes(&secret).unwrap(), expected);
    }

    fn test_authenticated_request(
        normalized_claims_present: bool,
    ) -> AuthenticatedKnowledgeRequest {
        AuthenticatedKnowledgeRequest {
            host_id: Uuid::from_u128(1),
            agent_def_id: Uuid::from_u128(2),
            environment: "dev".into(),
            policy_digest: "policy".into(),
            data_boundary_digest: "boundary".into(),
            subject_id: if normalized_claims_present {
                "user-1".into()
            } else {
                String::new()
            },
            subject_type: if normalized_claims_present {
                "USER".into()
            } else {
                String::new()
            },
            groups: Vec::new(),
            organizations: Vec::new(),
            normalized_claims_present,
        }
    }

    fn test_selection(requires_normalized_claims: bool) -> SelectedKnowledgeBase {
        SelectedKnowledgeBase {
            knowledge_base_id: Uuid::from_u128(3),
            priority: 0,
            maximum_knowledge_bases: 1,
            top_k: 5,
            token_budget: 1024,
            failure_policy: "FAIL_CLOSED".into(),
            embedding_group_key: "test".into(),
            requires_normalized_claims,
        }
    }

    #[test]
    fn legacy_delegation_cannot_select_mirrored_acl_knowledge_base() {
        let legacy = test_authenticated_request(false);
        assert!(enforce_normalized_claims_for_selection(&legacy, &[test_selection(false)]).is_ok());
        let error =
            enforce_normalized_claims_for_selection(&legacy, &[test_selection(true)]).unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.code, "KNOWLEDGE_DELEGATION_BINDING_INVALID");
        assert!(
            enforce_normalized_claims_for_selection(
                &test_authenticated_request(true),
                &[test_selection(true)]
            )
            .is_ok()
        );
    }

    #[test]
    fn metrics_render_all_operational_gauges_from_one_snapshot() {
        let body = render_metrics([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
        assert!(body.contains("light_knowledge_snapshot_superseded 1"));
        assert!(body.contains("light_knowledge_jobs{state=\"failed\"} 5"));
        assert!(body.contains("light_knowledge_graph_fallbacks_5m 11"));
    }

    #[test]
    fn config_accepts_phase4_graph_with_bounded_server_owned_limits() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("database-url");
        let delegation = directory.path().join("delegation-secret");
        let query_cache_key = directory.path().join("query-cache-key");
        let heartbeat = directory.path().join("heartbeat-secret");
        fs::write(&database, "postgresql://localhost/test").unwrap();
        fs::write(&delegation, "01234567890123456789012345678901").unwrap();
        fs::write(&query_cache_key, "abcdefghijklmnopqrstuvwxyz012345").unwrap();
        fs::write(&heartbeat, "01234567890123456789012345678901").unwrap();
        let mut config = KnowledgeConfig {
            version: 1,
            database_url_file: database,
            expected_database: None,
            delegation_secret_file: delegation,
            query_cache_key_file: query_cache_key,
            heartbeat_secret_file: heartbeat,
            delegation_issuer: "light-agent".into(),
            object_store_root: directory.path().join("objects"),
            maximum_request_bytes: 1024,
            maximum_query_bytes: 512,
            request_timeout_ms: 1000,
            maximum_database_connections: 2,
            projection_lease_seconds: 30,
            legacy_delegation_acceptance_deadline: None,
            deterministic_pilot: true,
            embedding_gateway_url: None,
            embedding_authorization_file: None,
            embedding_alias: "kb-query".into(),
            embedding_space_id: knowledge_core::FAKE_SPACE_ID.into(),
            embedding_space_revision: 1,
            embedding_dimension: 32,
            query_cache_maximum_entries: 2048,
            query_cache_maximum_bytes: 64 * 1024 * 1024,
            query_cache_ttl_seconds: 300,
            graph_limits: GraphLimits::default(),
            features: FeatureFlags {
                delta_segments: false,
                uploads: false,
                context_expansion: false,
                multi_knowledge_base: false,
                graph_assisted: false,
                enterprise_source_acls: false,
                embedding_migration: false,
                production_operations: false,
            },
        };
        assert!(config.validate().is_ok());
        let valid_request = RetrieveRequest {
            knowledge_base_ids: vec![Uuid::from_u128(1)],
            environment: String::new(),
            query: "configuration".into(),
            top_k: 20,
            token_budget: 0,
            filters: None,
        };
        assert!(validate_retrieve_request(&config, &valid_request).is_ok());
        let mut invalid_request = valid_request.clone();
        invalid_request.query = " ".into();
        assert!(validate_retrieve_request(&config, &invalid_request).is_err());
        invalid_request = valid_request.clone();
        invalid_request.top_k = 21;
        assert!(validate_retrieve_request(&config, &invalid_request).is_err());
        invalid_request = valid_request.clone();
        invalid_request.knowledge_base_ids = (1..=5).map(Uuid::from_u128).collect();
        assert!(validate_retrieve_request(&config, &invalid_request).is_err());
        invalid_request.knowledge_base_ids = vec![Uuid::from_u128(1); 5];
        assert!(validate_retrieve_request(&config, &invalid_request).is_ok());
        invalid_request = valid_request.clone();
        invalid_request.query = "x".repeat(513);
        assert!(validate_retrieve_request(&config, &invalid_request).is_err());
        config.legacy_delegation_acceptance_deadline =
            Some(Utc::now() + chrono::Duration::minutes(11));
        assert!(config.validate().is_err());
        config.legacy_delegation_acceptance_deadline = None;
        config.features.delta_segments = true;
        config.features.multi_knowledge_base = true;
        assert!(config.validate().is_ok());
        config.features.delta_segments = false;
        config.features.uploads = true;
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("require delta segments")
        );
        config.features.uploads = false;
        config.features.graph_assisted = true;
        assert!(config.validate().is_ok());
        config.graph_limits.maximum_hops = 5;
        assert!(config.validate().unwrap_err().contains("Phase 4"));
    }

    #[test]
    fn vector_parser_requires_qualified_dimension() {
        let good = format!("[{}]", vec!["0"; 32].join(","));
        assert_eq!(parse_vector(&good, 32).unwrap().len(), 32);
        assert!(parse_vector("[0,1]", 32).is_err());
    }

    #[test]
    fn graph_traversal_enforces_node_edge_and_path_work_limits() {
        let first = Uuid::from_u128(1);
        let second = Uuid::from_u128(2);
        let third = Uuid::from_u128(3);
        let chunk = Uuid::from_u128(10);
        let adjacency = BTreeMap::from([
            (first, vec![second]),
            (second, vec![first, third]),
            (third, vec![second]),
        ]);
        let result_chunks = HashMap::from([(second, BTreeSet::from([chunk]))]);
        let scores = bounded_graph_scores(
            adjacency.clone(),
            vec![first],
            result_chunks.clone(),
            &GraphLimits::default(),
            Instant::now() + StdDuration::from_secs(1),
        )
        .unwrap();
        assert_eq!(scores.get(&chunk), Some(&0.5));

        let mut limits = GraphLimits::default();
        limits.maximum_visited_nodes = 2;
        limits.maximum_visited_edges = 10;
        limits.maximum_paths = 10;
        let error = bounded_graph_scores(
            adjacency,
            vec![first],
            result_chunks,
            &limits,
            Instant::now() + StdDuration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error, "KNOWLEDGE_GRAPH_TRAVERSAL_LIMIT_EXCEEDED");

        let mut edge_limits = GraphLimits::default();
        edge_limits.maximum_visited_edges = 1;
        let error = bounded_graph_scores(
            BTreeMap::from([
                (first, vec![second]),
                (second, vec![first, third]),
                (third, vec![second]),
            ]),
            vec![first],
            HashMap::new(),
            &edge_limits,
            Instant::now() + StdDuration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error, "KNOWLEDGE_GRAPH_TRAVERSAL_LIMIT_EXCEEDED");
    }

    #[tokio::test]
    async fn protected_query_embedding_validates_space_and_uses_keyed_cache() {
        let calls = Arc::new(AtomicUsize::new(0));
        let application = Router::new()
            .route(
                "/embeddings",
                post(|State(calls): State<Arc<AtomicUsize>>| async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    (
                        [
                            ("x-light-embedding-space-id", "qualified-space"),
                            ("x-light-embedding-space-revision", "7"),
                        ],
                        Json(json!({
                            "data": [{"index": 0, "embedding": [0.1, 0.2, 0.3]}]
                        })),
                    )
                }),
            )
            .with_state(calls.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, application).await.unwrap() });

        let directory = tempfile::tempdir().unwrap();
        let token = directory.path().join("embedding-token");
        fs::write(&token, "test-token").unwrap();
        let config = KnowledgeConfig {
            version: 1,
            database_url_file: directory.path().join("unused-db"),
            expected_database: None,
            delegation_secret_file: directory.path().join("unused-delegation"),
            query_cache_key_file: directory.path().join("unused-query-cache-key"),
            heartbeat_secret_file: directory.path().join("unused-heartbeat"),
            delegation_issuer: "light-agent".into(),
            object_store_root: directory.path().join("objects"),
            maximum_request_bytes: 1024,
            maximum_query_bytes: 512,
            request_timeout_ms: 1000,
            maximum_database_connections: 2,
            projection_lease_seconds: 30,
            legacy_delegation_acceptance_deadline: None,
            deterministic_pilot: false,
            embedding_gateway_url: Some(format!("http://{address}/embeddings")),
            embedding_authorization_file: Some(token),
            embedding_alias: "kb-query".into(),
            embedding_space_id: "qualified-space".into(),
            embedding_space_revision: 7,
            embedding_dimension: 3,
            query_cache_maximum_entries: 8,
            query_cache_maximum_bytes: 1024,
            query_cache_ttl_seconds: 30,
            graph_limits: GraphLimits::default(),
            features: FeatureFlags {
                delta_segments: false,
                uploads: false,
                context_expansion: false,
                multi_knowledge_base: false,
                graph_assisted: false,
                enterprise_source_acls: false,
                embedding_migration: false,
                production_operations: false,
            },
        };
        let secret = b"01234567890123456789012345678901";
        let state = KnowledgeState {
            pool: PgPoolOptions::new()
                .connect_lazy("postgresql://localhost/unused")
                .unwrap(),
            delegation_verifier: DelegationVerifier::new(secret, "light-agent", "light-knowledge")
                .unwrap(),
            heartbeat_secret: secret.to_vec(),
            query_cache_key: secret.to_vec(),
            query_cache: Mutex::new(QueryEmbeddingCache::default()),
            metrics_cache: Mutex::new(None),
            embedding_client: reqwest::Client::new(),
            config,
        };
        let first = query_embedding(
            &state,
            "  configured   service ",
            "policy-a",
            "boundary-a",
            "qualified-space",
            7,
            3,
        )
        .await
        .unwrap();
        let second = query_embedding(
            &state,
            "configured service",
            "policy-a",
            "boundary-a",
            "qualified-space",
            7,
            3,
        )
        .await
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        query_embedding(
            &state,
            "configured service",
            "policy-b",
            "boundary-a",
            "qualified-space",
            7,
            3,
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
