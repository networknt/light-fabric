use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use agent_delegation::{DelegationKind, DelegationVerifier};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use knowledge_core::{
    AuthorizationSnapshot, BaseManifest, Chunk, FullBaseGeneration, KnowledgeError,
    RetrievalResponse, RetrieveRequest, retrieve,
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
    pub delegation_secret_file: PathBuf,
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
    pub features: FeatureFlags,
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
}

impl KnowledgeConfig {
    pub fn load() -> Result<Self, String> {
        let path = env::var("LIGHT_KNOWLEDGE_CONFIG_FILE")
            .unwrap_or_else(|_| "config/knowledge.yml".to_string());
        let content = fs::read_to_string(&path)
            .map_err(|error| format!("failed to read Knowledge config {path}: {error}"))?;
        let config: Self = serde_yaml::from_str(&content)
            .map_err(|error| format!("failed to parse Knowledge config {path}: {error}"))?;
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
                        .is_none_or(|path| !path.is_file())))
        {
            return Err("invalid Phase 1a limits, lease, or embedding-space contract".into());
        }
        if self.features.delta_segments
            || self.features.uploads
            || self.features.context_expansion
            || self.features.multi_knowledge_base
            || self.features.graph_assisted
        {
            return Err("Phase 1a unsupported features must remain disabled".into());
        }
        if !self.database_url_file.is_file() {
            return Err("databaseUrlFile must be a readable regular file".into());
        }
        if !self.delegation_secret_file.is_file()
            || !self.heartbeat_secret_file.is_file()
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

impl KnowledgeState {
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
        let delegation_secret =
            read_secret_file(&config.delegation_secret_file).map_err(RuntimeError::Config)?;
        let delegation_verifier = DelegationVerifier::new(
            delegation_secret.as_bytes(),
            config.delegation_issuer.clone(),
            "light-knowledge",
        )
        .map_err(|error| RuntimeError::Config(format!("Knowledge delegation verifier: {error}")))?;
        let heartbeat_secret = fs::read(&config.heartbeat_secret_file)
            .map_err(|error| RuntimeError::Config(format!("heartbeat secret: {error}")))?;
        if heartbeat_secret.is_empty() {
            return Err(RuntimeError::Config("heartbeat secret is empty".into()));
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
            query_cache_key: delegation_secret.into_bytes(),
            query_cache: Mutex::new(QueryEmbeddingCache::default()),
            embedding_client,
            config,
        })
    }
}

fn read_secret_file(path: &PathBuf) -> Result<String, String> {
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

pub fn knowledge_router(state: Arc<KnowledgeState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/knowledge/retrieve", post(retrieve_handler))
        .route(
            "/v1/knowledge/documents/{document_id}/versions/{document_version_id}",
            get(document_version_handler),
        )
        .layer(DefaultBodyLimit::max(state.config.maximum_request_bytes))
        .with_state(state)
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
        "SELECT EXISTS(SELECT 1 FROM knowledge_projection_heartbeat_t WHERE lease_expires_ts > now())",
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
    let pending = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM knowledge_projection_inbox_t WHERE state IN ('RECEIVED','GAP')",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(-1);
    let body = format!(
        "# TYPE light_knowledge_projection_pending gauge\nlight_knowledge_projection_pending {pending}\n"
    );
    (
        [(
            &axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        body,
    )
        .into_response()
}

struct AuthenticatedKnowledgeRequest {
    host_id: Uuid,
    agent_def_id: Uuid,
    environment: String,
    policy_digest: String,
    data_boundary_digest: String,
}

async fn authenticated_context(
    headers: &HeaderMap,
    state: &KnowledgeState,
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
    if !normalized_claims_present && !legacy_window_open {
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
    if principal.kind != DelegationKind::KnowledgeRetrieve
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
    })
}

async fn retrieve_handler(
    State(state): State<Arc<KnowledgeState>>,
    headers: HeaderMap,
    Json(mut request): Json<RetrieveRequest>,
) -> Result<Json<RetrievalResponse>, ApiError> {
    let authenticated = authenticated_context(&headers, &state).await?;
    if request.query.trim().is_empty()
        || request.query.len() > state.config.maximum_query_bytes.min(8192)
        || request.top_k > 20
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "KNOWLEDGE_INVALID_REQUEST",
        ));
    }
    request.environment = authenticated.environment;
    let request_id = required_text_header(&headers, "x-request-id")?;
    let response = tokio::time::timeout(
        StdDuration::from_millis(state.config.request_timeout_ms),
        retrieve_transaction(
            &state,
            &request_id,
            authenticated.host_id,
            authenticated.agent_def_id,
            &authenticated.policy_digest,
            &authenticated.data_boundary_digest,
            &request,
        ),
    )
    .await
    .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "KNOWLEDGE_DEADLINE_EXCEEDED"))??;
    Ok(Json(response))
}

async fn retrieve_transaction(
    state: &KnowledgeState,
    request_id: &str,
    consumer_host_id: Uuid,
    agent_def_id: Uuid,
    policy_digest: &str,
    data_boundary_digest: &str,
    request: &RetrieveRequest,
) -> Result<RetrievalResponse, ApiError> {
    if request.knowledge_base_ids.len() > 1 {
        return Err(ApiError::from(KnowledgeError::MultipleKnowledgeBases));
    }
    let knowledge_base_id =
        resolve_knowledge_base_id(&state.pool, consumer_host_id, agent_def_id, request).await?;
    admit_request(&state.pool, knowledge_base_id, consumer_host_id, request_id).await?;
    let result = retrieve_snapshot(
        state,
        request_id,
        knowledge_base_id,
        consumer_host_id,
        agent_def_id,
        policy_digest,
        data_boundary_digest,
        request,
    )
    .await;
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
    .bind(consumer_host_id)
    .bind(request_id)
    .bind(terminal_state)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    result
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
    consumer_host_id: Uuid,
    agent_def_id: Uuid,
    policy_digest: &str,
    data_boundary_digest: &str,
    request: &RetrieveRequest,
) -> Result<RetrievalResponse, ApiError> {
    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *transaction)
        .await
        .map_err(ApiError::database)?;
    let authorization = load_authorization(
        &mut transaction,
        knowledge_base_id,
        consumer_host_id,
        agent_def_id,
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
    ): (Uuid, i32, i32, i32, i32) = sqlx::query_as(
        "SELECT p.profile_id,p.top_k,p.token_budget,
                p.lexical_candidates,p.vector_candidates
           FROM knowledge_retrieval_profile_t p
          JOIN knowledge_runtime_authorization_t a
            ON a.retrieval_profile_id=p.profile_id
         WHERE a.knowledge_base_id=$1 AND a.consumer_host_id=$2
           AND a.environment=$3 AND a.agent_id=$4 AND p.active=TRUE",
    )
    .bind(knowledge_base_id)
    .bind(consumer_host_id)
    .bind(&request.environment)
    .bind(agent_def_id)
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
        state,
        &mut transaction,
        knowledge_base_id,
        &request.environment,
        &effective_request,
        lexical_candidates,
        vector_candidates,
        policy_digest,
        data_boundary_digest,
    )
    .await?;
    let started = std::time::Instant::now();
    let response = retrieve(&generation, &authorization, &effective_request, Utc::now())?;
    let result_identities = response
        .results
        .iter()
        .map(|hit| json!({"chunkId": hit.chunk_id, "documentVersionId": hit.citation.document_version_id}))
        .collect::<Vec<_>>();
    let query_digest = sha256_hex(request.query.as_bytes());
    sqlx::query(
        "INSERT INTO knowledge_query_audit_t(query_audit_id,request_id,knowledge_base_id,consumer_host_id,index_generation_id,retrieval_profile_id,strategy,segment_manifest_digest,query_digest,result_identities,latency_ms) VALUES($1,$2,$3,$4,$5,$6,'HYBRID',$7,$8,$9,$10) ON CONFLICT(knowledge_base_id,consumer_host_id,request_id) DO NOTHING",
    )
    .bind(Uuid::now_v7())
    .bind(request_id)
    .bind(knowledge_base_id)
    .bind(consumer_host_id)
    .bind(generation.manifest.generation_id)
    .bind(retrieval_profile_id)
    .bind(&generation.manifest.manifest_digest)
    .bind(query_digest)
    .bind(Value::Array(result_identities))
    .bind(i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX))
    .execute(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    transaction.commit().await.map_err(ApiError::database)?;
    Ok(response)
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
    heartbeat_secret: &[u8],
) -> Result<AuthorizationSnapshot, ApiError> {
    let row = sqlx::query("SELECT a.active,a.desired_event_sequence,a.applied_event_sequence,a.lease_expires_ts AS authorization_lease,h.lease_expires_ts AS projector_lease,h.effective_config_digest,h.signature_digest FROM knowledge_runtime_authorization_t a JOIN knowledge_projection_heartbeat_t h ON h.projector_id=a.projector_id WHERE a.knowledge_base_id=$1 AND a.consumer_host_id=$2 AND a.environment=$3 AND a.agent_id=$4")
        .bind(knowledge_base_id).bind(consumer_host_id).bind(environment).bind(agent_def_id)
        .fetch_optional(&mut **transaction).await.map_err(ApiError::database)?
        .ok_or_else(|| ApiError::from(KnowledgeError::AuthorizationDenied))?;
    let digest: String = row
        .try_get("effective_config_digest")
        .map_err(ApiError::database)?;
    let signature: String = row
        .try_get("signature_digest")
        .map_err(ApiError::database)?;
    let signature = decode_hex(signature.trim())
        .ok_or_else(|| ApiError::from(KnowledgeError::StaleAuthorization))?;
    let mut verifier = Hmac::<Sha256>::new_from_slice(heartbeat_secret)
        .map_err(|_| ApiError::from(KnowledgeError::StaleAuthorization))?;
    verifier.update(digest.trim().as_bytes());
    verifier
        .verify_slice(&signature)
        .map_err(|_| ApiError::from(KnowledgeError::StaleAuthorization))?;
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

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
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
    state: &KnowledgeState,
    transaction: &mut Transaction<'_, Postgres>,
    knowledge_base_id: Uuid,
    environment: &str,
    request: &RetrieveRequest,
    lexical_candidates: i32,
    vector_candidates: i32,
    policy_digest: &str,
    data_boundary_digest: &str,
) -> Result<FullBaseGeneration, ApiError> {
    let row = sqlx::query("SELECT g.index_generation_id,s.index_segment_id,g.snapshot_watermark,g.parser_contract_digest,g.chunker_contract_digest,g.lexical_contract_digest,g.citation_contract_digest,g.space_id,g.space_revision,g.dimension,s.manifest_digest,s.document_count,s.chunk_count,s.vector_count FROM knowledge_index_pointer_t p JOIN knowledge_index_generation_t g ON g.index_generation_id=p.index_generation_id JOIN knowledge_generation_segment_t m ON m.index_generation_id=g.index_generation_id AND m.ordinal=0 JOIN knowledge_index_segment_t s ON s.index_segment_id=m.index_segment_id AND s.segment_kind='BASE' AND s.state='READY' WHERE p.knowledge_base_id=$1 AND p.environment=$2 AND g.state='PROMOTED'")
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
    let query_embedding = query_embedding(
        state,
        &request.query,
        policy_digest,
        data_boundary_digest,
        &space_id,
        space_revision,
        dimension,
    )
    .await?;
    let query_vector = format!(
        "[{}]",
        query_embedding
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
        "WITH lexical_ranked AS (
           SELECT c.chunk_id,
                  row_number() OVER (ORDER BY
                    ts_rank_cd(c.lexical_input,
                      plainto_tsquery('english',$2)) DESC,c.chunk_id) AS lexical_rank
             FROM knowledge_segment_chunk_t member
             JOIN knowledge_chunk_t c ON c.chunk_id=member.chunk_id
             JOIN knowledge_document_version_t dv
               ON dv.document_version_id=c.document_version_id
             JOIN knowledge_document_t d ON d.document_id=dv.document_id
            WHERE member.index_segment_id=$1
              AND c.lexical_input @@ plainto_tsquery('english',$2)
              AND (cardinality($6::uuid[])=0 OR d.source_id=ANY($6::uuid[]))
            ORDER BY lexical_rank
            LIMIT $4
         ), vector_ranked AS (
           SELECT v.chunk_id,
                  row_number() OVER (ORDER BY
                    v.projection <=> $3::vector,v.chunk_id) AS vector_rank
             FROM knowledge_segment_vector_t v
             JOIN knowledge_chunk_t c ON c.chunk_id=v.chunk_id
             JOIN knowledge_document_version_t dv
               ON dv.document_version_id=c.document_version_id
             JOIN knowledge_document_t d ON d.document_id=dv.document_id
            WHERE v.index_segment_id=$1
              AND (cardinality($6::uuid[])=0 OR d.source_id=ANY($6::uuid[]))
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
           JOIN knowledge_segment_chunk_t member
             ON member.index_segment_id=$1 AND member.chunk_id=candidate.chunk_id
           JOIN knowledge_chunk_t c ON c.chunk_id=member.chunk_id
           JOIN knowledge_document_version_t dv
             ON dv.document_version_id=c.document_version_id
           JOIN knowledge_document_t d ON d.document_id=dv.document_id
           JOIN knowledge_segment_vector_t v
             ON v.index_segment_id=member.index_segment_id
            AND v.chunk_id=member.chunk_id
          ORDER BY c.chunk_id",
    )
    .bind(segment_id)
    .bind(&request.query)
    .bind(query_vector)
    .bind(lexical_candidates.max(1))
    .bind(vector_candidates.max(1))
    .bind(source_ids)
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
            document_count: usize::try_from(
                row.try_get::<i64, _>("document_count")
                    .map_err(ApiError::database)?,
            )
            .unwrap_or(0),
            chunk_count: usize::try_from(
                row.try_get::<i64, _>("chunk_count")
                    .map_err(ApiError::database)?,
            )
            .unwrap_or(0),
            vector_count: usize::try_from(
                row.try_get::<i64, _>("vector_count")
                    .map_err(ApiError::database)?,
            )
            .unwrap_or(0),
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
            segment_kind: "BASE".into(),
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

async fn document_version_handler(
    State(state): State<Arc<KnowledgeState>>,
    Path((document_id, document_version_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Result<Json<DocumentVersionResponse>, ApiError> {
    let authenticated = authenticated_context(&headers, &state).await?;
    let knowledge_base_id = required_uuid_header(&headers, "x-knowledge-base-id")?;
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
    let chunks = sqlx::query("SELECT c.chunk_id,c.ordinal,c.section_path,c.start_offset,c.end_offset,c.chunk_text,c.content_digest FROM knowledge_chunk_t c JOIN knowledge_document_version_t v ON v.document_version_id=c.document_version_id WHERE c.knowledge_base_id=$1 AND v.document_id=$2 AND c.document_version_id=$3 ORDER BY c.ordinal")
        .bind(knowledge_base_id).bind(document_id).bind(document_version_id)
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
    transaction.commit().await.map_err(ApiError::database)?;
    Ok(Json(DocumentVersionResponse {
        document_version_id,
        chunks,
    }))
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
    fn config_rejects_later_phase_features() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("database-url");
        let delegation = directory.path().join("delegation-secret");
        let heartbeat = directory.path().join("heartbeat-secret");
        fs::write(&database, "postgresql://localhost/test").unwrap();
        fs::write(&delegation, "01234567890123456789012345678901").unwrap();
        fs::write(&heartbeat, "01234567890123456789012345678901").unwrap();
        let mut config = KnowledgeConfig {
            version: 1,
            database_url_file: database,
            delegation_secret_file: delegation,
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
            features: FeatureFlags {
                delta_segments: false,
                uploads: false,
                context_expansion: false,
                multi_knowledge_base: false,
                graph_assisted: false,
            },
        };
        assert!(config.validate().is_ok());
        config.legacy_delegation_acceptance_deadline =
            Some(Utc::now() + chrono::Duration::minutes(11));
        assert!(config.validate().is_err());
        config.legacy_delegation_acceptance_deadline = None;
        config.features.delta_segments = true;
        assert!(
            config
                .validate()
                .unwrap_err()
                .contains("unsupported features")
        );
    }

    #[test]
    fn vector_parser_requires_qualified_dimension() {
        let good = format!("[{}]", vec!["0"; 32].join(","));
        assert_eq!(parse_vector(&good, 32).unwrap().len(), 32);
        assert!(parse_vector("[0,1]", 32).is_err());
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
            delegation_secret_file: directory.path().join("unused-delegation"),
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
            features: FeatureFlags {
                delta_segments: false,
                uploads: false,
                context_expansion: false,
                multi_knowledge_base: false,
                graph_assisted: false,
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
