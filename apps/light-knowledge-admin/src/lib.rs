use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, MatchedPath, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use light_runtime::{RuntimeConfig, RuntimeError};
use light_security::{
    AuthPrincipal, JwtExpiryMode, SecurityRuntime, load_security_runtime, verify_jwt_token,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{Column, PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const RESPONSE_CONTENT_TYPE: &str = "application/json";
const MAXIMUM_CURSOR_BYTES: usize = 2_048;
const MAXIMUM_ROW_BYTES: usize = 65_536;
const LATENCY_BUCKETS_MS: [u64; 8] = [10, 50, 100, 250, 500, 1_000, 2_000, u64::MAX];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdminConfig {
    pub version: u16,
    pub database_url_file: PathBuf,
    pub expected_database: String,
    pub opaque_actor_key_file: PathBuf,
    pub snapshot_signing_key_file: PathBuf,
    pub maximum_request_bytes: usize,
    pub maximum_response_bytes: usize,
    pub maximum_page_size: u16,
    pub request_timeout_ms: u64,
    pub maximum_database_connections: u32,
    #[serde(default)]
    pub ignore_jwt_expiry: bool,
}

impl AdminConfig {
    pub fn load(runtime: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let config = runtime
            .module_registry
            .load_config::<Self>(runtime, "knowledge-admin.yml")?;
        config.validate().map_err(RuntimeError::Config)?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.expected_database.trim().is_empty()
            || self.maximum_request_bytes == 0
            || self.maximum_request_bytes > 1_048_576
            || self.maximum_response_bytes == 0
            || self.maximum_response_bytes > 1_048_576
            || self.maximum_page_size == 0
            || self.maximum_page_size > 200
            || self.request_timeout_ms == 0
            || self.request_timeout_ms > 2_000
            || self.maximum_database_connections == 0
        {
            return Err("invalid Light Knowledge administration bounds".into());
        }
        Ok(())
    }
}

pub struct AdminState {
    pool: PgPool,
    security: SecurityRuntime,
    config: AdminConfig,
    cursor_key: Vec<u8>,
    opaque_actor_key: Vec<u8>,
    snapshot_signing_key: Vec<u8>,
    metrics: AdminMetrics,
}

impl AdminState {
    pub async fn build(runtime: &RuntimeConfig, config: AdminConfig) -> Result<Self, RuntimeError> {
        let database_url = read_secret(&config.database_url_file, "Knowledge database URL")?;
        let opaque_actor_key = read_secret(&config.opaque_actor_key_file, "opaque actor key")?;
        let snapshot_signing_key = read_secret(
            &config.snapshot_signing_key_file,
            "Knowledge control snapshot signing key",
        )?;
        if opaque_actor_key.len() < 32 || snapshot_signing_key.len() < 32 {
            return Err(RuntimeError::Config(
                "administration HMAC keys must contain at least 32 bytes".into(),
            ));
        }
        let pool = PgPoolOptions::new()
            .max_connections(config.maximum_database_connections)
            .acquire_timeout(Duration::from_millis(config.request_timeout_ms))
            .connect(&database_url)
            .await
            .map_err(|error| {
                RuntimeError::Config(format!("Knowledge administration database: {error}"))
            })?;
        let actual: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .map_err(|error| {
                RuntimeError::Config(format!("Knowledge database identity: {error}"))
            })?;
        if actual != config.expected_database {
            return Err(RuntimeError::Config(format!(
                "Knowledge database identity mismatch: expected {}, got {actual}",
                config.expected_database
            )));
        }
        let security = load_security_runtime(runtime, true)?.ok_or_else(|| {
            RuntimeError::Config("administration JWT verification must be enabled".into())
        })?;
        security.bootstrap().await.map_err(|error| {
            RuntimeError::Config(format!(
                "administration JWKS bootstrap failed: {}",
                error.message
            ))
        })?;
        let cursor_key =
            Sha256::digest([opaque_actor_key.as_bytes(), b":cursor-v1"].concat()).to_vec();
        Ok(Self {
            pool,
            security,
            config,
            cursor_key,
            opaque_actor_key: opaque_actor_key.into_bytes(),
            snapshot_signing_key: snapshot_signing_key.into_bytes(),
            metrics: AdminMetrics::default(),
        })
    }

    pub fn pool(&self) -> PgPool {
        self.pool.clone()
    }
}

#[derive(Default)]
struct AdminMetrics {
    routes: Mutex<BTreeMap<String, RouteMetrics>>,
}

#[derive(Clone, Default)]
struct RouteMetrics {
    requests: u64,
    denials: u64,
    redactions: u64,
    timeouts: u64,
    results: u64,
    latency_sum_micros: u64,
    latency_buckets: [u64; 8],
}

impl AdminMetrics {
    fn record(
        &self,
        route: &str,
        status: StatusCode,
        elapsed: Duration,
        results: u64,
        redactions: u64,
    ) {
        let mut routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let metric = routes.entry(route.to_string()).or_default();
        metric.requests = metric.requests.saturating_add(1);
        metric.results = metric.results.saturating_add(results);
        metric.redactions = metric.redactions.saturating_add(redactions);
        metric.latency_sum_micros = metric
            .latency_sum_micros
            .saturating_add(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX));
        if matches!(
            status,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            metric.denials = metric.denials.saturating_add(1);
        }
        if status == StatusCode::GATEWAY_TIMEOUT {
            metric.timeouts = metric.timeouts.saturating_add(1);
        }
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        for (index, bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
            if elapsed_ms <= *bound {
                metric.latency_buckets[index] = metric.latency_buckets[index].saturating_add(1);
            }
        }
    }

    fn prometheus(&self, pool: &PgPool) -> String {
        let routes = self
            .routes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut body = String::from(
            "# HELP light_knowledge_admin_requests_total Bounded administration requests.\n\
# TYPE light_knowledge_admin_requests_total counter\n",
        );
        for (route, metric) in routes.iter() {
            let label = prometheus_label(route);
            body.push_str(&format!(
                "light_knowledge_admin_requests_total{{route=\"{label}\"}} {}\n\
light_knowledge_admin_denials_total{{route=\"{label}\"}} {}\n\
light_knowledge_admin_redactions_total{{route=\"{label}\"}} {}\n\
light_knowledge_admin_timeouts_total{{route=\"{label}\"}} {}\n\
light_knowledge_admin_results_total{{route=\"{label}\"}} {}\n\
light_knowledge_admin_latency_seconds_sum{{route=\"{label}\"}} {:.6}\n\
light_knowledge_admin_latency_seconds_count{{route=\"{label}\"}} {}\n",
                metric.requests,
                metric.denials,
                metric.redactions,
                metric.timeouts,
                metric.results,
                metric.latency_sum_micros as f64 / 1_000_000.0,
                metric.requests,
            ));
            for (index, bound) in LATENCY_BUCKETS_MS.iter().enumerate() {
                let le = if *bound == u64::MAX {
                    "+Inf".to_string()
                } else {
                    format!("{:.3}", *bound as f64 / 1_000.0)
                };
                body.push_str(&format!(
                    "light_knowledge_admin_latency_seconds_bucket{{route=\"{label}\",le=\"{le}\"}} {}\n",
                    metric.latency_buckets[index]
                ));
            }
        }
        body.push_str(&format!(
            "# TYPE light_knowledge_admin_database_pool_connections gauge\n\
light_knowledge_admin_database_pool_connections {}\n\
# TYPE light_knowledge_admin_database_pool_idle gauge\n\
light_knowledge_admin_database_pool_idle {}\n",
            pool.size(),
            pool.num_idle()
        ));
        body
    }
}

fn prometheus_label(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('"', "\\\"")
}

fn read_secret(path: &PathBuf, label: &str) -> Result<String, RuntimeError> {
    let environment = match path.file_name().and_then(|name| name.to_str()) {
        Some("knowledge-database-url") => Some("LIGHT_KNOWLEDGE_DATABASE_URL"),
        Some("opaque-actor-key") => Some("LIGHT_KNOWLEDGE_ADMIN_OPAQUE_ACTOR_KEY"),
        Some("control-snapshot-signing-key") => {
            Some("LIGHT_KNOWLEDGE_CONTROL_SNAPSHOT_SIGNING_KEY")
        }
        _ => None,
    };
    let value = environment
        .and_then(|name| std::env::var(name).ok())
        .or_else(|| fs::read_to_string(path).ok())
        .ok_or_else(|| RuntimeError::Config(format!("{label} is unavailable")))?;
    let value = value.trim().to_string();
    if value.is_empty() {
        return Err(RuntimeError::Config(format!("{label} is empty")));
    }
    Ok(value)
}

pub fn admin_router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route(
            "/v1/knowledge/admin/knowledge-base-summaries:batch",
            post(summary_batch),
        )
        .route(
            "/v1/knowledge/admin/source-status:batch",
            post(source_status_batch),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/sync-runs",
            get(sync_runs),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/documents",
            get(documents),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/index-generations",
            get(generations),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/index-segments",
            get(all_segments),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/index-generations/{generationId}/segments",
            get(segments),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/incremental-operations",
            get(incremental),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/acl-status",
            get(acl_status),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/production-operations",
            get(production),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/promotion-receipts",
            get(promotion_receipts),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/embedding-migration-estimates",
            post(estimate),
        )
        .route(
            "/v1/knowledge/admin/knowledge-bases/{id}/authorization-simulations",
            post(simulate),
        )
        .route("/v1/knowledge/admin/commands", post(submit_command))
        .route(
            "/v1/knowledge/admin/control-snapshots:apply",
            post(apply_control_snapshot),
        )
        .layer(DefaultBodyLimit::max(state.config.maximum_request_bytes))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            observe_request,
        ))
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({"status":"UP"}))
}

async fn ready(State(state): State<Arc<AdminState>>) -> Result<Json<Value>, ApiError> {
    sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&state.pool)
        .await
        .map_err(|_| ApiError::unavailable("KNOWLEDGE_ADMIN_DATABASE_UNAVAILABLE"))?;
    Ok(Json(json!({"status":"UP"})))
}

async fn metrics(State(state): State<Arc<AdminState>>) -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
        state.metrics.prometheus(&state.pool),
    )
        .into_response()
}

async fn observe_request(
    State(state): State<Arc<AdminState>>,
    request: Request,
    next: Next,
) -> Response {
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("unmatched")
        .to_string();
    let started = Instant::now();
    let response = match tokio::time::timeout(
        Duration::from_millis(state.config.request_timeout_ms),
        next.run(request),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => ApiError::timeout("KNOWLEDGE_ADMIN_REQUEST_TIMEOUT").into_response(),
    };
    let response_is_json = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let response = if !response_is_json && response.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::too_large("KNOWLEDGE_ADMIN_REQUEST_TOO_LARGE").into_response()
    } else if !response_is_json
        && matches!(
            response.status(),
            StatusCode::BAD_REQUEST
                | StatusCode::UNPROCESSABLE_ENTITY
                | StatusCode::UNSUPPORTED_MEDIA_TYPE
        )
    {
        ApiError::bad_request("KNOWLEDGE_ADMIN_REQUEST_INVALID").into_response()
    } else {
        response
    };
    let results = response
        .headers()
        .get("x-knowledge-result-count")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let redactions = response
        .headers()
        .get("x-knowledge-redaction-count")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    state.metrics.record(
        &route,
        response.status(),
        started.elapsed(),
        results,
        redactions,
    );
    response
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BatchRequest {
    knowledge_base_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SourceBatchRequest {
    knowledge_base_id: Uuid,
    source_ids: Vec<Uuid>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageQuery {
    page_size: Option<u16>,
    cursor: Option<String>,
    uploads_cursor: Option<String>,
    changes_cursor: Option<String>,
    anchors_cursor: Option<String>,
    compactions_cursor: Option<String>,
    anti_entropy_cursor: Option<String>,
    acl_freshness_cursor: Option<String>,
    acl_reconciliations_cursor: Option<String>,
    acl_transitions_cursor: Option<String>,
    connector_objects_cursor: Option<String>,
    embedding_migrations_cursor: Option<String>,
    migration_evaluations_cursor: Option<String>,
    generation_retention_cursor: Option<String>,
    backup_checkpoints_cursor: Option<String>,
    purge_evidence_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EstimateRequest {
    target_profile_id: Uuid,
    target_profile_revision: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SimulationRequest {
    subject_type: String,
    subject_id: String,
}

struct Scope {
    host_id: Uuid,
    environment: String,
    global_read: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigurationMetadata {
    snapshot_id: Uuid,
    publication_sequence: i64,
    payload_digest: String,
    applied_ts: DateTime<Utc>,
    lease_expires_ts: DateTime<Utc>,
}

#[derive(Clone)]
struct KnowledgeBaseContext {
    owner_scope: &'static str,
    configuration: ConfigurationMetadata,
}

async fn require_fresh_configuration(
    state: &AdminState,
    scope: &Scope,
) -> Result<ConfigurationMetadata, ApiError> {
    let row = sqlx::query(
        "SELECT snapshot_id,publication_sequence,payload_digest,applied_ts,lease_expires_ts
           FROM knowledge_control_snapshot_t
          WHERE host_id=$1 AND environment=$2 AND state='APPLIED'
          ORDER BY publication_sequence DESC LIMIT 1",
    )
    .bind(scope.host_id)
    .bind(&scope.environment)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| ApiError::unavailable("KNOWLEDGE_CONFIGURATION_UNAVAILABLE"))?;
    let lease_expires_ts: DateTime<Utc> = row.get("lease_expires_ts");
    if lease_expires_ts <= Utc::now() {
        return Err(ApiError::unavailable("KNOWLEDGE_CONFIGURATION_STALE"));
    }
    Ok(ConfigurationMetadata {
        snapshot_id: row.get("snapshot_id"),
        publication_sequence: row.get("publication_sequence"),
        payload_digest: row.get("payload_digest"),
        applied_ts: row.get("applied_ts"),
        lease_expires_ts,
    })
}

async fn knowledge_base_context(
    state: &AdminState,
    scope: &Scope,
    knowledge_base_id: Uuid,
) -> Result<KnowledgeBaseContext, ApiError> {
    let configuration = require_fresh_configuration(state, scope).await?;
    let host_id: Option<Uuid> = sqlx::query_scalar(
        "SELECT host_id FROM knowledge_base_t
          WHERE knowledge_base_id=$1 AND environment=$2
            AND (host_id=$3 OR (host_id IS NULL AND $4)) AND status<>'DELETED'",
    )
    .bind(knowledge_base_id)
    .bind(&scope.environment)
    .bind(scope.host_id)
    .bind(scope.global_read)
    .fetch_optional(&state.pool)
    .await
    .map_err(ApiError::database)?
    .ok_or_else(|| ApiError::not_found("KNOWLEDGE_BASE_NOT_FOUND"))?;
    Ok(KnowledgeBaseContext {
        owner_scope: if host_id.is_some() {
            "TENANT"
        } else {
            "GLOBAL"
        },
        configuration,
    })
}

async fn authorize(
    state: &AdminState,
    headers: &HeaderMap,
    capability: &str,
) -> Result<Scope, ApiError> {
    let token = bearer(headers)?;
    let expiry = if state.config.ignore_jwt_expiry {
        JwtExpiryMode::Ignore
    } else {
        JwtExpiryMode::Enforce
    };
    let principal = verify_jwt_token(&state.security, token, expiry)
        .await
        .map_err(|_| ApiError::unauthorized("KNOWLEDGE_ADMIN_TOKEN_INVALID"))?;
    let required_scope = required_scope(capability)
        .ok_or_else(|| ApiError::forbidden("KNOWLEDGE_ADMIN_CAPABILITY_DENIED"))?;
    validate_delegated_user_claims(headers, &principal, required_scope)
}

fn required_scope(capability: &str) -> Option<&'static str> {
    match capability {
        "knowledge.admin.summary.read"
        | "knowledge.admin.source-status.read"
        | "knowledge.admin.operational.read"
        | "knowledge.admin.migration-estimate.read"
        | "knowledge.admin.authorization-simulation.read" => Some("portal.r"),
        "knowledge.admin.command.write" | "knowledge.admin.snapshot.write" => Some("portal.w"),
        _ => None,
    }
}

fn bearer(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::unauthorized("KNOWLEDGE_ADMIN_TOKEN_REQUIRED"))
}

fn validate_delegated_user_claims(
    headers: &HeaderMap,
    principal: &AuthPrincipal,
    required_scope: &str,
) -> Result<Scope, ApiError> {
    let claims = &principal.claims;
    let host_id = principal
        .host
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| ApiError::forbidden("KNOWLEDGE_ADMIN_SCOPE_INVALID"))?;
    let environment = headers
        .get("x-knowledge-environment")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::forbidden("KNOWLEDGE_ADMIN_ENVIRONMENT_REQUIRED"))?;
    if environment.is_empty()
        || environment.len() > 16
        || !environment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(ApiError::forbidden("KNOWLEDGE_ADMIN_ENVIRONMENT_INVALID"));
    }
    let environment = environment.to_string();
    let scopes = delegated_scopes(claims);
    if !scopes.contains(required_scope) {
        return Err(ApiError::forbidden("KNOWLEDGE_ADMIN_SCOPE_DENIED"));
    }
    let roles = delegated_roles(principal);
    if !roles.iter().any(|role| {
        matches!(
            role.as_str(),
            "admin" | "host-admin" | "org-admin" | "platformKnowledgeBaseAdmin"
        )
    }) {
        return Err(ApiError::forbidden("KNOWLEDGE_ADMIN_ROLE_DENIED"));
    }
    Ok(Scope {
        host_id,
        environment,
        global_read: roles.contains("admin") || roles.contains("platformKnowledgeBaseAdmin"),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OperationalCommandRequest {
    action: String,
    data: Value,
}

const SNAPSHOT_TABLES: &[&str] = &[
    "knowledge_embedding_profile_t",
    "knowledge_ingestion_policy_t",
    "knowledge_retrieval_profile_t",
    "knowledge_base_t",
    "knowledge_source_t",
    "agent_knowledge_base_t",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlSnapshotEnvelope {
    payload: String,
    payload_digest: String,
    signature: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ControlSnapshotPayload {
    contract_version: u16,
    compatibility_generation: u32,
    snapshot_id: Uuid,
    publication_sequence: i64,
    source_event_watermark: Value,
    host_id: Uuid,
    environment: String,
    complete: bool,
    replica_inventory: BTreeMap<String, usize>,
    tables: BTreeMap<String, Vec<Value>>,
    tombstones: BTreeMap<String, Vec<Value>>,
}

async fn apply_control_snapshot(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(envelope): Json<ControlSnapshotEnvelope>,
) -> Result<Response, ApiError> {
    let scope = authorize(&state, &headers, "knowledge.admin.snapshot.write").await?;
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(&envelope.payload)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_SNAPSHOT_PAYLOAD_INVALID"))?;
    let payload_digest = hex(&Sha256::digest(&payload_bytes));
    if payload_digest != envelope.payload_digest {
        return Err(ApiError::bad_request("KNOWLEDGE_SNAPSHOT_DIGEST_INVALID"));
    }
    let signature = decode_hex(&envelope.signature)
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_SNAPSHOT_SIGNATURE_INVALID"))?;
    let mut mac = HmacSha256::new_from_slice(&state.snapshot_signing_key)
        .map_err(|_| ApiError::internal("KNOWLEDGE_SNAPSHOT_VERIFIER_UNAVAILABLE"))?;
    mac.update(&payload_bytes);
    mac.verify_slice(&signature)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_SNAPSHOT_SIGNATURE_INVALID"))?;
    let payload: ControlSnapshotPayload = serde_json::from_slice(&payload_bytes)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_SNAPSHOT_PAYLOAD_INVALID"))?;
    let expected_tables = SNAPSHOT_TABLES
        .iter()
        .map(|value| value.to_string())
        .collect::<BTreeSet<_>>();
    if payload.contract_version != 1
        || payload.compatibility_generation != 1
        || !payload.complete
        || payload.publication_sequence < 0
        || payload.host_id != scope.host_id
        || payload.environment != scope.environment
        || payload.tables.keys().cloned().collect::<BTreeSet<_>>() != expected_tables
        || payload
            .replica_inventory
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>()
            != expected_tables
        || payload.tombstones.keys().cloned().collect::<BTreeSet<_>>() != expected_tables
        || payload
            .replica_inventory
            .iter()
            .any(|(table, count)| payload.tables.get(table).map(Vec::len) != Some(*count))
        || !payload
            .tombstones
            .values()
            .all(|entries| entries.iter().all(Value::is_object))
        || !snapshot_tombstones_match_rows(&payload)
        || !payload.source_event_watermark.is_object()
    {
        return Err(ApiError::bad_request("KNOWLEDGE_SNAPSHOT_CONTRACT_INVALID"));
    }

    let mut transaction = state.pool.begin().await.map_err(ApiError::database)?;
    let current = sqlx::query(
        "SELECT publication_sequence,payload_digest FROM knowledge_control_snapshot_t
          WHERE host_id=$1 AND environment=$2 AND state='APPLIED'
          ORDER BY publication_sequence DESC LIMIT 1 FOR UPDATE",
    )
    .bind(payload.host_id)
    .bind(&payload.environment)
    .fetch_optional(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    if let Some(current) = current {
        let sequence: i64 = current.get("publication_sequence");
        let digest: String = current.get("payload_digest");
        if sequence > payload.publication_sequence {
            return Err(ApiError::conflict("KNOWLEDGE_SNAPSHOT_DOWNGRADE_REJECTED"));
        }
        if sequence == payload.publication_sequence {
            if digest != envelope.payload_digest {
                return Err(ApiError::conflict("KNOWLEDGE_SNAPSHOT_SEQUENCE_CONFLICT"));
            }
            sqlx::query(
                "UPDATE knowledge_control_snapshot_t SET applied_ts=now(),
                   lease_expires_ts=now()+interval '5 minutes'
                 WHERE host_id=$1 AND environment=$2 AND publication_sequence=$3",
            )
            .bind(payload.host_id)
            .bind(&payload.environment)
            .bind(sequence)
            .execute(&mut *transaction)
            .await
            .map_err(ApiError::database)?;
            sqlx::query(
                "UPDATE knowledge_runtime_authorization_t
                    SET lease_expires_ts=now()+interval '5 minutes',update_ts=now()
                  WHERE consumer_host_id=$1 AND environment=$2 AND projector_id=$3",
            )
            .bind(payload.host_id)
            .bind(&payload.environment)
            .bind(payload.snapshot_id.to_string())
            .execute(&mut *transaction)
            .await
            .map_err(ApiError::database)?;
            transaction.commit().await.map_err(ApiError::database)?;
            return bounded_response(
                &state,
                json!({"snapshotId":payload.snapshot_id,"publicationSequence":sequence,
                    "state":"APPLIED","idempotentReplay":true}),
            );
        }
    }
    for table in SNAPSHOT_TABLES {
        materialize_snapshot_table(
            &mut transaction,
            table,
            payload
                .tables
                .get(*table)
                .expect("validated table inventory"),
        )
        .await?;
    }
    sqlx::query(
        "INSERT INTO knowledge_control_snapshot_t(
           snapshot_id,host_id,environment,publication_sequence,
           source_event_watermark,compatibility_generation,payload_digest,
           signature_digest,state)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,'APPLIED')",
    )
    .bind(payload.snapshot_id)
    .bind(payload.host_id)
    .bind(&payload.environment)
    .bind(payload.publication_sequence)
    .bind(&payload.source_event_watermark)
    .bind(i32::try_from(payload.compatibility_generation).unwrap_or(1))
    .bind(&envelope.payload_digest)
    .bind(hex(&Sha256::digest(signature)))
    .execute(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    sqlx::query(
        "UPDATE knowledge_control_snapshot_t SET state='SUPERSEDED'
          WHERE host_id=$1 AND environment=$2 AND snapshot_id<>$3 AND state='APPLIED'",
    )
    .bind(payload.host_id)
    .bind(&payload.environment)
    .bind(payload.snapshot_id)
    .execute(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    sqlx::query(
        "INSERT INTO knowledge_runtime_authorization_t(
           knowledge_base_id,consumer_host_id,environment,agent_id,
           retrieval_profile_id,active,desired_event_sequence,
           applied_event_sequence,projector_id,lease_expires_ts,
           authorization_digest)
         SELECT binding.knowledge_base_id,binding.host_id,binding.environment,
           binding.agent_id,binding.retrieval_profile_id,binding.active,
           binding.version,binding.version,$1::text,now()+interval '5 minutes',
           encode(digest(concat_ws('|',binding.knowledge_base_id::text,
             binding.host_id::text,binding.environment,binding.agent_id::text,
             binding.retrieval_profile_id::text,binding.version::text,
             binding.active::text,$2),'sha256'),'hex')
         FROM agent_knowledge_base_t binding
         WHERE binding.host_id=$3 AND binding.environment=$4
         ON CONFLICT(knowledge_base_id,consumer_host_id,environment,agent_id)
         DO UPDATE SET retrieval_profile_id=EXCLUDED.retrieval_profile_id,
           active=EXCLUDED.active,
           desired_event_sequence=EXCLUDED.desired_event_sequence,
           applied_event_sequence=EXCLUDED.applied_event_sequence,
           projector_id=EXCLUDED.projector_id,
           lease_expires_ts=EXCLUDED.lease_expires_ts,
           authorization_digest=EXCLUDED.authorization_digest,update_ts=now()",
    )
    .bind(payload.snapshot_id)
    .bind(&envelope.payload_digest)
    .bind(payload.host_id)
    .bind(&payload.environment)
    .execute(&mut *transaction)
    .await
    .map_err(ApiError::database)?;
    transaction.commit().await.map_err(ApiError::database)?;
    bounded_response(
        &state,
        json!({"snapshotId":payload.snapshot_id,
            "publicationSequence":payload.publication_sequence,
            "state":"APPLIED","idempotentReplay":false}),
    )
}

fn snapshot_tombstones_match_rows(payload: &ControlSnapshotPayload) -> bool {
    payload.tombstones.iter().all(|(table, tombstones)| {
        let Some(rows) = payload.tables.get(table) else {
            return false;
        };
        tombstones.iter().all(|tombstone| {
            let Some(tombstone) = tombstone.as_object() else {
                return false;
            };
            let Some(version) = tombstone.get("version") else {
                return false;
            };
            rows.iter().any(|row| {
                let Some(row) = row.as_object() else {
                    return false;
                };
                let identity_matches = snapshot_primary_keys(table).iter().all(|key| {
                    tombstone
                        .get(*key)
                        .is_some_and(|value| row.get(*key) == Some(value))
                });
                let terminal = match table.as_str() {
                    "knowledge_base_t" | "knowledge_source_t" => {
                        row.get("status").and_then(Value::as_str) == Some("DELETED")
                    }
                    _ => row.get("active").and_then(Value::as_bool) == Some(false),
                };
                identity_matches && row.get("version") == Some(version) && terminal
            })
        })
    })
}

fn snapshot_primary_keys(table: &str) -> &'static [&'static str] {
    match table {
        "knowledge_embedding_profile_t" => &["profile_id", "profile_revision"],
        "knowledge_ingestion_policy_t" => &["ingestion_policy_id"],
        "knowledge_retrieval_profile_t" => &["profile_id"],
        "knowledge_base_t" => &["knowledge_base_id"],
        "knowledge_source_t" => &["source_id"],
        "agent_knowledge_base_t" => &["host_id", "environment", "agent_id", "knowledge_base_id"],
        _ => &[],
    }
}

async fn materialize_snapshot_table(
    transaction: &mut sqlx::Transaction<'_, Postgres>,
    table: &str,
    rows: &[Value],
) -> Result<(), ApiError> {
    if rows.is_empty() {
        return Ok(());
    }
    if !rows.iter().all(Value::is_object) {
        return Err(ApiError::bad_request("KNOWLEDGE_SNAPSHOT_ROW_INVALID"));
    }
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns
          WHERE table_schema='public' AND table_name=$1 AND is_generated='NEVER'
          ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut **transaction)
    .await
    .map_err(ApiError::database)?;
    if columns.is_empty() {
        return Err(ApiError::unavailable(
            "KNOWLEDGE_SNAPSHOT_SCHEMA_UNAVAILABLE",
        ));
    }
    let keys = snapshot_primary_keys(table);
    let updates = columns
        .iter()
        .filter(|column| !keys.contains(&column.as_str()))
        .map(|column| format!("{column}=EXCLUDED.{column}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO {table} SELECT * FROM jsonb_populate_recordset(NULL::{table},$1::jsonb)
         ON CONFLICT({}) DO UPDATE SET {updates}",
        keys.join(",")
    );
    sqlx::query(&sql)
        .bind(Value::Array(rows.to_vec()))
        .execute(&mut **transaction)
        .await
        .map_err(ApiError::database)?;
    Ok(())
}

fn operational_job_type(action: &str) -> Option<&'static str> {
    Some(match action {
        "testKnowledgeSource" => "CONNECTIVITY_TEST",
        "requestKnowledgeSourceSync" => "SYNC",
        "requestKnowledgeSourceAclReconciliation" => "ACL_RECONCILE",
        "receiveKnowledgeSourceProviderNotification" => "PROVIDER_NOTIFICATION",
        "requestKnowledgeBaseReindex" => "FULL_REINDEX",
        "requestKnowledgeBaseCompaction" => "COMPACTION",
        "promoteKnowledgeBaseIndexGeneration" => "PROMOTE",
        "requestKnowledgeBasePurge" => "PURGE",
        "testKnowledgeRetrieval" => "RETRIEVAL_TEST",
        "requestKnowledgeBaseEmbeddingMigration" => "MIGRATION_PREFLIGHT",
        "pauseKnowledgeBaseEmbeddingMigration" => "MIGRATION_PAUSE",
        "resumeKnowledgeBaseEmbeddingMigration" => "MIGRATION_BACKFILL",
        "cancelKnowledgeBaseEmbeddingMigration" => "MIGRATION_CANCEL",
        "rollbackKnowledgeBaseIndexGeneration" => "MIGRATION_ROLLBACK",
        "retireKnowledgeBaseIndexGeneration" => "MIGRATION_RETIRE",
        "requestKnowledgeBaseBackupCheckpoint" => "BACKUP_CHECKPOINT",
        "verifyKnowledgeBasePhysicalRestore" => "RESTORE_VERIFY",
        _ => return None,
    })
}

async fn submit_command(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(request): Json<OperationalCommandRequest>,
) -> Result<Response, ApiError> {
    let scope = authorize(&state, &headers, "knowledge.admin.command.write").await?;
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_IDEMPOTENCY_KEY_REQUIRED"))?;
    let job_type = operational_job_type(&request.action)
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_OPERATIONAL_ACTION_INVALID"))?;
    let data = request
        .data
        .as_object()
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_COMMAND_DATA_INVALID"))?;
    let knowledge_base_id = data
        .get("knowledgeBaseId")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_BASE_ID_REQUIRED"))?;
    let source_id = data
        .get("sourceId")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let environment = data
        .get("environment")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_ENVIRONMENT_REQUIRED"))?;
    if environment != scope.environment {
        return Err(ApiError::not_found("KNOWLEDGE_BASE_NOT_FOUND"));
    }
    knowledge_base_context(&state, &scope, knowledge_base_id).await?;
    if let Some(source_id) = source_id {
        let source_visible: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM knowledge_source_t
              WHERE source_id=$1 AND knowledge_base_id=$2 AND status<>'DELETED')",
        )
        .bind(source_id)
        .bind(knowledge_base_id)
        .fetch_one(&state.pool)
        .await
        .map_err(ApiError::database)?;
        if !source_visible {
            return Err(ApiError::not_found("KNOWLEDGE_SOURCE_NOT_FOUND"));
        }
    }
    let subject = bearer(&headers)?;
    let actor = opaque_actor(&state, subject)?;
    if data.keys().any(|key| {
        matches!(
            key.as_str(),
            "authorizedBy" | "requestedBy" | "authenticatedPrincipal" | "principalClaims"
        )
    }) {
        return Err(ApiError::bad_request(
            "KNOWLEDGE_COMMAND_PRINCIPAL_FIELD_FORBIDDEN",
        ));
    }
    let mut payload = request.data.clone();
    payload
        .as_object_mut()
        .expect("validated command data")
        .insert("authorizedBy".into(), Value::String(actor.clone()));
    let job_id = Uuid::now_v7();
    let inserted = sqlx::query(
        "INSERT INTO knowledge_job_t(job_id,knowledge_base_id,source_id,job_type,
           idempotency_key,requested_by,payload)
         VALUES($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING",
    )
    .bind(job_id)
    .bind(knowledge_base_id)
    .bind(source_id)
    .bind(job_type)
    .bind(idempotency_key)
    .bind(actor)
    .bind(payload)
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let row = sqlx::query(
        "SELECT job_id,job_type,state,created_ts,payload FROM knowledge_job_t
          WHERE knowledge_base_id=$1 AND idempotency_key=$2",
    )
    .bind(knowledge_base_id)
    .bind(idempotency_key)
    .fetch_one(&state.pool)
    .await
    .map_err(ApiError::database)?;
    if inserted.rows_affected() == 0
        && !same_operational_command(
            job_type,
            &request.data,
            row.get("job_type"),
            row.get("payload"),
        )
    {
        return Err(ApiError::conflict("KNOWLEDGE_COMMAND_IDEMPOTENCY_CONFLICT"));
    }
    bounded_response(
        &state,
        json!({"jobId":row.get::<Uuid,_>("job_id"),
            "jobType":row.get::<String,_>("job_type"),
            "state":row.get::<String,_>("state"),
            "createdTs":row.get::<DateTime<Utc>,_>("created_ts"),
            "idempotentReplay":inserted.rows_affected()==0}),
    )
}

fn same_operational_command(
    requested_job_type: &str,
    requested_data: &Value,
    existing_job_type: String,
    mut existing_payload: Value,
) -> bool {
    if let Some(object) = existing_payload.as_object_mut() {
        object.remove("authorizedBy");
    }
    requested_job_type == existing_job_type && requested_data == &existing_payload
}

fn delegated_scopes(claims: &Value) -> BTreeSet<String> {
    let mut scopes = BTreeSet::new();
    if let Some(values) = claims.get("scp").and_then(Value::as_array) {
        scopes.extend(values.iter().filter_map(Value::as_str).map(str::to_string));
    }
    if let Some(value) = claims.get("scope").and_then(Value::as_str) {
        scopes.extend(value.split_whitespace().map(str::to_string));
    }
    scopes
}

fn delegated_roles(principal: &AuthPrincipal) -> BTreeSet<String> {
    principal
        .role
        .as_deref()
        .unwrap_or_default()
        .split(|character: char| character.is_whitespace() || character == ',')
        .filter(|role| !role.is_empty())
        .map(str::to_string)
        .collect()
}

fn require_ids(_scope: &Scope, ids: &[Uuid]) -> Result<(), ApiError> {
    if ids.is_empty()
        || ids.len() > 200
        || ids.iter().copied().collect::<BTreeSet<_>>().len() != ids.len()
    {
        return Err(ApiError::bad_request(
            "KNOWLEDGE_ADMIN_KNOWLEDGE_BASE_SCOPE_INVALID",
        ));
    }
    Ok(())
}

async fn summary_batch(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(request): Json<BatchRequest>,
) -> Result<Response, ApiError> {
    let scope = authorize(&state, &headers, "knowledge.admin.summary.read").await?;
    require_ids(&scope, &request.knowledge_base_ids)?;
    let configuration = require_fresh_configuration(&state, &scope).await?;
    let rows = sqlx::query(
        "SELECT b.knowledge_base_id,b.host_id,b.version,b.status,
                pointer.index_generation_id AS active_generation_id,pointer.pointer_version,
                generation.state AS generation_state,generation.final_watermark,
                EXISTS(SELECT 1 FROM knowledge_sync_run_t run
                  WHERE run.knowledge_base_id=b.knowledge_base_id
                    AND run.state NOT IN ('SUCCEEDED','FAILED','CANCELLED')) AS has_active_sync,
                (SELECT count(*) FROM knowledge_job_t job
                  WHERE job.knowledge_base_id=b.knowledge_base_id
                    AND job.state IN ('QUEUED','RUNNING')) AS active_job_count,
                (SELECT job.state FROM knowledge_job_t job
                  WHERE job.knowledge_base_id=b.knowledge_base_id
                  ORDER BY job.update_ts DESC,job.job_id DESC LIMIT 1) AS latest_job_state
           FROM knowledge_base_t b
           LEFT JOIN knowledge_index_pointer_t pointer
             ON pointer.knowledge_base_id=b.knowledge_base_id AND pointer.environment=b.environment
           LEFT JOIN knowledge_index_generation_t generation
             ON generation.index_generation_id=pointer.index_generation_id
          WHERE b.knowledge_base_id=ANY($1) AND b.environment=$2
            AND (b.host_id=$3 OR (b.host_id IS NULL AND $4))
          ORDER BY b.knowledge_base_id",
    )
    .bind(&request.knowledge_base_ids)
    .bind(&scope.environment)
    .bind(scope.host_id)
    .bind(scope.global_read)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let found: BTreeSet<Uuid> = rows
        .iter()
        .map(|row| row.get("knowledge_base_id"))
        .collect();
    let summaries = request
        .knowledge_base_ids
        .iter()
        .map(|id| {
            rows.iter()
                .find(|row| row.get::<Uuid, _>("knowledge_base_id") == *id)
                .map(summary_value)
                .unwrap_or_else(|| {
                    json!({"knowledgeBaseId":id,"effectiveState":"NOT_YET_APPLIED",
                "ownerScope":null,"activeGenerationId":null,
                "hasActiveSync":false,"activeJobCount":0})
                })
        })
        .collect::<Vec<_>>();
    debug_assert!(found.len() <= request.knowledge_base_ids.len());
    bounded_response(
        &state,
        json!({"knowledgeBaseSummaries":summaries,"environment":scope.environment,
            "configuration":configuration,"asOf":Utc::now()}),
    )
}

fn summary_value(row: &PgRow) -> Value {
    let generation_state: Option<String> = row.try_get("generation_state").ok();
    json!({
        "knowledgeBaseId": row.get::<Uuid, _>("knowledge_base_id"),
        "ownerScope": if row.try_get::<Uuid,_>("host_id").is_ok() { "TENANT" } else { "GLOBAL" },
        "version": row.get::<i64, _>("version"),
        "desiredStatus": row.get::<String, _>("status"),
        "effectiveState": if generation_state.is_some() { "AVAILABLE" } else { "NOT_YET_APPLIED" },
        "activeGenerationId": row.try_get::<Uuid, _>("active_generation_id").ok(),
        "pointerVersion": row.try_get::<i64, _>("pointer_version").ok(),
        "generationState": generation_state,
        "finalWatermark": row.try_get::<i64, _>("final_watermark").ok(),
        "hasActiveSync": row.get::<bool, _>("has_active_sync"),
        "activeJobCount": row.get::<i64, _>("active_job_count").min(1000),
        "latestJobState": row.try_get::<String, _>("latest_job_state").ok(),
    })
}

async fn source_status_batch(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(request): Json<SourceBatchRequest>,
) -> Result<Response, ApiError> {
    let scope = authorize(&state, &headers, "knowledge.admin.source-status.read").await?;
    require_ids(&scope, &[request.knowledge_base_id])?;
    if request.source_ids.is_empty()
        || request.source_ids.len() > 200
        || request
            .source_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len()
            != request.source_ids.len()
    {
        return Err(ApiError::bad_request(
            "KNOWLEDGE_ADMIN_SOURCE_SCOPE_INVALID",
        ));
    }
    let context = knowledge_base_context(&state, &scope, request.knowledge_base_id).await?;
    let rows = sqlx::query(
        "SELECT source.source_id,run.sync_run_id,run.state,
                successful.finished_ts AS last_successful_sync_ts,
                successful.index_generation_id AS last_successful_generation_id,
                run.update_ts
           FROM knowledge_source_t source JOIN knowledge_base_t b USING(knowledge_base_id)
           LEFT JOIN LATERAL (SELECT r.sync_run_id,r.state,r.finished_ts,r.update_ts
             FROM knowledge_sync_run_t r WHERE r.source_id=source.source_id
             ORDER BY r.requested_ts DESC,r.sync_run_id DESC LIMIT 1) run ON TRUE
           LEFT JOIN LATERAL (SELECT r.finished_ts,r.index_generation_id FROM knowledge_sync_run_t r
             WHERE r.source_id=source.source_id AND r.state='SUCCEEDED'
             ORDER BY r.finished_ts DESC,r.sync_run_id DESC LIMIT 1) successful ON TRUE
          WHERE source.knowledge_base_id=$1 AND source.source_id=ANY($2)
            AND b.environment=$3 AND (b.host_id=$4 OR (b.host_id IS NULL AND $5))
          ORDER BY source.source_id",
    )
    .bind(request.knowledge_base_id)
    .bind(&request.source_ids)
    .bind(&scope.environment)
    .bind(scope.host_id)
    .bind(scope.global_read)
    .fetch_all(&state.pool)
    .await
    .map_err(ApiError::database)?;
    let values = rows
        .into_iter()
        .map(|row| {
            json!({
                "sourceId":row.get::<Uuid,_>("source_id"),
                "latestSyncRunId":row.try_get::<Uuid,_>("sync_run_id").ok(),
                "latestSyncState":row.try_get::<String,_>("state").ok(),
            "lastSuccessfulSyncTs":row.try_get::<DateTime<Utc>,_>("last_successful_sync_ts").ok(),
                "lastSuccessfulGenerationId":row.try_get::<Uuid,_>("last_successful_generation_id").ok(),
                "updateTs":row.try_get::<DateTime<Utc>,_>("update_ts").ok(),
            })
        })
        .collect::<Vec<_>>();
    bounded_response(
        &state,
        json!({"knowledgeSourceStatus":values,"knowledgeBaseId":request.knowledge_base_id,
            "environment":scope.environment,"ownerScope":context.owner_scope,
            "configuration":context.configuration,"asOf":Utc::now()}),
    )
}

#[derive(Clone, Copy)]
struct ResourceSpec {
    name: &'static str,
    table: &'static str,
    collection: &'static str,
    timestamp: &'static str,
    primary_keys: &'static [&'static str],
    fields: &'static [&'static str],
}

macro_rules! spec {
    ($name:literal,$table:literal,$collection:literal,$ts:literal,[$($pk:literal),+],[$($field:literal),+]) => {
        ResourceSpec { name:$name, table:$table, collection:$collection, timestamp:$ts,
            primary_keys:&[$($pk),+], fields:&[$($field),+] }
    };
}

const SYNC_RUNS: ResourceSpec = spec!(
    "syncRuns",
    "knowledge_sync_run_t",
    "knowledgeSyncRuns",
    "requested_ts",
    ["sync_run_id"],
    [
        "sync_run_id",
        "job_id",
        "request_event_id",
        "knowledge_base_id",
        "source_id",
        "requested_ts",
        "start_watermark",
        "snapshot_watermark",
        "state",
        "phase",
        "progress",
        "index_generation_id",
        "ingestion_policy_id",
        "ingestion_policy_version",
        "document_count",
        "chunk_count",
        "source_bytes",
        "embedding_tokens",
        "stored_bytes",
        "attempt_count",
        "next_attempt_ts",
        "finished_ts",
        "error_summary",
        "update_ts"
    ]
);
const DOCUMENTS: ResourceSpec = spec!(
    "documents",
    "knowledge_document_t",
    "knowledgeDocuments",
    "update_ts",
    ["document_id"],
    [
        "document_id",
        "knowledge_base_id",
        "source_id",
        "lifecycle_state",
        "current_document_version_id",
        "observed_ts",
        "update_ts"
    ]
);
const GENERATIONS: ResourceSpec = spec!(
    "generations",
    "knowledge_index_generation_t",
    "knowledgeIndexGenerations",
    "created_ts",
    ["index_generation_id"],
    [
        "index_generation_id",
        "knowledge_base_id",
        "embedding_profile_id",
        "embedding_profile_revision",
        "space_id",
        "space_revision",
        "dimension",
        "parser_contract_digest",
        "chunker_contract_digest",
        "metadata_contract_digest",
        "citation_contract_digest",
        "acl_normalization_contract_digest",
        "lexical_contract_digest",
        "contract_set_digest",
        "query_input_transform_version",
        "snapshot_watermark",
        "final_watermark",
        "ordered_segment_manifest_digest",
        "strategy_projections",
        "state",
        "evidence",
        "created_ts",
        "promoted_ts"
    ]
);
const SEGMENTS: ResourceSpec = spec!(
    "segments",
    "knowledge_index_segment_t",
    "knowledgeIndexSegments",
    "created_ts",
    ["index_segment_id"],
    [
        "index_segment_id",
        "knowledge_base_id",
        "index_generation_id",
        "segment_kind",
        "state",
        "snapshot_watermark",
        "parser_contract_digest",
        "chunker_contract_digest",
        "lexical_contract_digest",
        "embedding_contract_digest",
        "acl_contract_digest",
        "manifest_digest",
        "document_count",
        "chunk_count",
        "vector_count",
        "acl_count",
        "created_ts",
        "predecessor_segment_id",
        "operation_count"
    ]
);
const UPLOADS: ResourceSpec = spec!(
    "uploads",
    "knowledge_upload_t",
    "knowledgeUploads",
    "staged_ts",
    ["upload_id"],
    [
        "upload_id",
        "knowledge_base_id",
        "source_id",
        "media_type",
        "content_length",
        "staged_digest",
        "scan_state",
        "lifecycle_state",
        "rejection_code",
        "staged_ts",
        "verified_ts",
        "promoted_ts",
        "purge_after_ts"
    ]
);
const CHANGES: ResourceSpec = spec!(
    "changes",
    "knowledge_source_change_t",
    "knowledgeIncrementalChanges",
    "observed_ts",
    ["source_change_id"],
    [
        "source_change_id",
        "sync_run_id",
        "knowledge_base_id",
        "source_id",
        "change_sequence",
        "change_kind",
        "previous_document_version_id",
        "selected_document_version_id",
        "selected_acl_revision_id",
        "input_contract_digest",
        "change_digest",
        "observed_ts"
    ]
);
const ANCHORS: ResourceSpec = spec!(
    "anchors",
    "knowledge_passage_anchor_t",
    "knowledgePassageAnchors",
    "created_ts",
    ["passage_anchor_id", "document_version_id"],
    [
        "passage_anchor_id",
        "knowledge_base_id",
        "document_id",
        "document_version_id",
        "chunk_id",
        "anchor_contract_digest",
        "continuity_state",
        "anchor_sequence",
        "created_ts"
    ]
);
const COMPACTIONS: ResourceSpec = spec!(
    "compactions",
    "knowledge_compaction_run_t",
    "knowledgeCompactionRuns",
    "created_ts",
    ["compaction_run_id"],
    [
        "compaction_run_id",
        "knowledge_base_id",
        "source_generation_id",
        "candidate_generation_id",
        "canonical_watermark",
        "state",
        "source_manifest_digest",
        "resolved_corpus_digest",
        "created_ts",
        "finished_ts"
    ]
);
const ANTI_ENTROPY: ResourceSpec = spec!(
    "antiEntropy",
    "knowledge_anti_entropy_run_t",
    "knowledgeAntiEntropyRuns",
    "started_ts",
    ["anti_entropy_run_id"],
    [
        "anti_entropy_run_id",
        "knowledge_base_id",
        "index_generation_id",
        "state",
        "expected_manifest_digest",
        "observed_manifest_digest",
        "mismatch_counts",
        "started_ts",
        "finished_ts"
    ]
);
const ACL_FRESHNESS: ResourceSpec = spec!(
    "aclFreshness",
    "knowledge_source_acl_state_t",
    "knowledgeAclFreshness",
    "update_ts",
    ["source_id"],
    [
        "source_id",
        "knowledge_base_id",
        "reconciliation_id",
        "state",
        "discovered_object_count",
        "covered_object_count",
        "denied_object_count",
        "unresolved_subject_count",
        "observed_ts",
        "fresh_until_ts",
        "evidence_digest",
        "update_ts"
    ]
);
const ACL_RECONCILIATIONS: ResourceSpec = spec!(
    "aclReconciliations",
    "knowledge_acl_reconciliation_t",
    "knowledgeAclReconciliations",
    "started_ts",
    ["reconciliation_id"],
    [
        "reconciliation_id",
        "knowledge_base_id",
        "source_id",
        "provider",
        "reconciliation_mode",
        "state",
        "input_cursor_digest",
        "output_cursor_digest",
        "discovered_object_count",
        "applied_acl_count",
        "denied_object_count",
        "unresolved_subject_count",
        "evidence_digest",
        "started_ts",
        "finished_ts",
        "fresh_until_ts",
        "error_code"
    ]
);
const ACL_TRANSITIONS: ResourceSpec = spec!(
    "aclTransitions",
    "knowledge_acl_transition_t",
    "knowledgeAclTransitions",
    "recorded_ts",
    ["acl_transition_id"],
    [
        "acl_transition_id",
        "reconciliation_id",
        "knowledge_base_id",
        "source_id",
        "document_id",
        "previous_acl_digest",
        "current_acl_digest",
        "transition_kind",
        "observed_ts",
        "recorded_ts"
    ]
);
const CONNECTOR_OBJECTS: ResourceSpec = spec!(
    "connectorObjects",
    "knowledge_connector_object_t",
    "knowledgeConnectorObjects",
    "observed_ts",
    ["connector_object_id"],
    [
        "connector_object_id",
        "knowledge_base_id",
        "source_id",
        "provider",
        "relationship_kind",
        "deleted",
        "last_reconciliation_id",
        "observed_ts"
    ]
);
const EMBEDDING_MIGRATIONS: ResourceSpec = spec!(
    "embeddingMigrations",
    "knowledge_embedding_migration_t",
    "knowledgeBaseEmbeddingMigrations",
    "created_ts",
    ["migration_id"],
    [
        "migration_id",
        "knowledge_base_id",
        "environment",
        "source_generation_id",
        "candidate_generation_id",
        "target_profile_id",
        "target_profile_revision",
        "target_space_id",
        "target_space_revision",
        "target_dimension",
        "estimated_chunk_count",
        "estimated_token_count",
        "estimated_cost_micros",
        "estimated_duration_seconds",
        "estimated_temporary_bytes",
        "accepted_cost_ceiling_micros",
        "consumed_token_count",
        "consumed_cost_micros",
        "completed_chunk_count",
        "reused_canonical_chunk_count",
        "start_watermark",
        "snapshot_watermark",
        "final_watermark",
        "state",
        "version",
        "evaluation_evidence_id",
        "promotion_watermark",
        "rollback_deadline",
        "pause_reason",
        "failure_code",
        "created_ts",
        "update_ts",
        "finished_ts"
    ]
);
const MIGRATION_EVALUATIONS: ResourceSpec = spec!(
    "migrationEvaluations",
    "knowledge_migration_evaluation_t",
    "knowledgeMigrationEvaluations",
    "created_ts",
    ["evaluation_evidence_id"],
    [
        "evaluation_evidence_id",
        "migration_id",
        "knowledge_base_id",
        "candidate_generation_id",
        "evaluation_contract_version",
        "corpus_watermark",
        "metrics",
        "evidence_digest",
        "passed",
        "expires_ts",
        "authorized_by",
        "created_ts"
    ]
);
const GENERATION_RETENTION: ResourceSpec = spec!(
    "generationRetention",
    "knowledge_generation_retention_t",
    "knowledgeGenerationRetention",
    "update_ts",
    ["index_generation_id"],
    [
        "index_generation_id",
        "knowledge_base_id",
        "retention_state",
        "retain_until_ts",
        "legal_hold",
        "backup_reference_count",
        "migration_reference_count",
        "last_reference_check_ts",
        "update_ts"
    ]
);
const BACKUP_CHECKPOINTS: ResourceSpec = spec!(
    "backupCheckpoints",
    "knowledge_backup_checkpoint_t",
    "knowledgeBackupCheckpoints",
    "created_ts",
    ["checkpoint_id"],
    [
        "checkpoint_id",
        "knowledge_base_id",
        "index_generation_id",
        "environment",
        "pointer_version",
        "object_manifest_digest",
        "state",
        "verification_evidence",
        "retain_until_ts",
        "created_ts",
        "verified_ts"
    ]
);
const PURGE_EVIDENCE: ResourceSpec = spec!(
    "purgeEvidence",
    "knowledge_purge_evidence_t",
    "knowledgePurgeEvidence",
    "created_ts",
    ["purge_evidence_id"],
    [
        "purge_evidence_id",
        "knowledge_base_id",
        "index_generation_id",
        "purge_scope",
        "state",
        "reference_counts",
        "deletion_counts",
        "evidence_digest",
        "authorized_by",
        "created_ts",
        "finished_ts"
    ]
);
const PROMOTION_RECEIPTS: ResourceSpec = spec!(
    "promotionReceipts",
    "knowledge_promotion_receipt_t",
    "knowledgePromotionReceipts",
    "committed_ts",
    ["promotion_id"],
    [
        "promotion_id",
        "knowledge_base_id",
        "environment",
        "index_generation_id",
        "pointer_version",
        "evidence_digest",
        "authorized_by",
        "committed_ts"
    ]
);

async fn sync_runs(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    single(&s, &h, id, SYNC_RUNS, &q).await
}
async fn documents(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    single(&s, &h, id, DOCUMENTS, &q).await
}
async fn generations(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    single(&s, &h, id, GENERATIONS, &q).await
}
async fn all_segments(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    single(&s, &h, id, SEGMENTS, &q).await
}

async fn promotion_receipts(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    single(&s, &h, id, PROMOTION_RECEIPTS, &q).await
}

async fn segments(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Path((id, generation_id)): Path<(Uuid, Uuid)>,
    Query(query): Query<PageQuery>,
) -> Result<Response, ApiError> {
    let scope = authorize(&state, &headers, "knowledge.admin.operational.read").await?;
    require_ids(&scope, &[id])?;
    let context = knowledge_base_context(&state, &scope, id).await?;
    let page = query_resource(
        &state,
        &scope,
        id,
        SEGMENTS,
        query.page_size,
        query.cursor.as_deref(),
        Some(generation_id),
    )
    .await?;
    bounded_response(&state, page_response(&[page], id, &scope, &context))
}

async fn single(
    state: &AdminState,
    headers: &HeaderMap,
    id: Uuid,
    spec: ResourceSpec,
    query: &PageQuery,
) -> Result<Response, ApiError> {
    let scope = authorize(state, headers, "knowledge.admin.operational.read").await?;
    require_ids(&scope, &[id])?;
    let context = knowledge_base_context(state, &scope, id).await?;
    let page = query_resource(
        state,
        &scope,
        id,
        spec,
        query.page_size,
        query.cursor.as_deref(),
        None,
    )
    .await?;
    bounded_response(state, page_response(&[page], id, &scope, &context))
}

async fn incremental(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    grouped(
        &s,
        &h,
        id,
        q.page_size,
        &[
            (UPLOADS, q.uploads_cursor.as_deref()),
            (CHANGES, q.changes_cursor.as_deref()),
            (ANCHORS, q.anchors_cursor.as_deref()),
            (COMPACTIONS, q.compactions_cursor.as_deref()),
            (ANTI_ENTROPY, q.anti_entropy_cursor.as_deref()),
        ],
    )
    .await
}
async fn acl_status(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    grouped(
        &s,
        &h,
        id,
        q.page_size,
        &[
            (ACL_FRESHNESS, q.acl_freshness_cursor.as_deref()),
            (ACL_RECONCILIATIONS, q.acl_reconciliations_cursor.as_deref()),
            (ACL_TRANSITIONS, q.acl_transitions_cursor.as_deref()),
            (CONNECTOR_OBJECTS, q.connector_objects_cursor.as_deref()),
        ],
    )
    .await
}
async fn production(
    State(s): State<Arc<AdminState>>,
    h: HeaderMap,
    Path(id): Path<Uuid>,
    Query(q): Query<PageQuery>,
) -> Result<Response, ApiError> {
    grouped(
        &s,
        &h,
        id,
        q.page_size,
        &[
            (
                EMBEDDING_MIGRATIONS,
                q.embedding_migrations_cursor.as_deref(),
            ),
            (
                MIGRATION_EVALUATIONS,
                q.migration_evaluations_cursor.as_deref(),
            ),
            (
                GENERATION_RETENTION,
                q.generation_retention_cursor.as_deref(),
            ),
            (BACKUP_CHECKPOINTS, q.backup_checkpoints_cursor.as_deref()),
            (PURGE_EVIDENCE, q.purge_evidence_cursor.as_deref()),
        ],
    )
    .await
}

async fn grouped(
    state: &AdminState,
    headers: &HeaderMap,
    id: Uuid,
    page_size: Option<u16>,
    specs: &[(ResourceSpec, Option<&str>)],
) -> Result<Response, ApiError> {
    let scope = authorize(state, headers, "knowledge.admin.operational.read").await?;
    require_ids(&scope, &[id])?;
    let context = knowledge_base_context(state, &scope, id).await?;
    let mut pages = Vec::with_capacity(specs.len());
    for (spec, cursor) in specs {
        pages.push(query_resource(state, &scope, id, *spec, page_size, *cursor, None).await?);
    }
    bounded_response(state, page_response(&pages, id, &scope, &context))
}

struct Page {
    spec: ResourceSpec,
    rows: Vec<Value>,
    has_more: bool,
    next_cursor: Option<String>,
}

fn page_response(
    pages: &[Page],
    knowledge_base_id: Uuid,
    scope: &Scope,
    context: &KnowledgeBaseContext,
) -> Value {
    let mut root = Map::new();
    let mut pagination = Map::new();
    for page in pages {
        root.insert(page.spec.collection.into(), Value::Array(page.rows.clone()));
        pagination.insert(
            page.spec.collection.into(),
            json!({"hasMore":page.has_more,"nextCursor":page.next_cursor}),
        );
    }
    root.insert("pagination".into(), Value::Object(pagination));
    root.insert("knowledgeBaseId".into(), json!(knowledge_base_id));
    root.insert("environment".into(), json!(scope.environment));
    root.insert("ownerScope".into(), json!(context.owner_scope));
    root.insert("configuration".into(), json!(context.configuration));
    root.insert("asOf".into(), json!(Utc::now()));
    Value::Object(root)
}

#[derive(Debug, Serialize, Deserialize)]
struct CursorPayload {
    resource: String,
    knowledge_base_id: Uuid,
    environment: String,
    generation_id: Option<Uuid>,
    timestamp: DateTime<Utc>,
    ids: Vec<Uuid>,
}

async fn query_resource(
    state: &AdminState,
    scope: &Scope,
    knowledge_base_id: Uuid,
    spec: ResourceSpec,
    page_size: Option<u16>,
    cursor: Option<&str>,
    generation_id: Option<Uuid>,
) -> Result<Page, ApiError> {
    let size = resolve_page_size(page_size, state.config.maximum_page_size)?;
    let decoded = cursor
        .map(|value| {
            decode_cursor(
                state,
                spec,
                value,
                knowledge_base_id,
                &scope.environment,
                generation_id,
            )
        })
        .transpose()?;
    let mut builder = QueryBuilder::<Postgres>::new("SELECT to_jsonb(o) AS row FROM ");
    builder.push(spec.table).push(" o JOIN knowledge_base_t b ON b.knowledge_base_id=o.knowledge_base_id WHERE o.knowledge_base_id=")
        .push_bind(knowledge_base_id).push(" AND b.environment=").push_bind(&scope.environment)
        .push(" AND (b.host_id=").push_bind(scope.host_id).push(" OR (b.host_id IS NULL AND ")
        .push_bind(scope.global_read).push("))");
    if let Some(generation_id) = generation_id {
        builder
            .push(" AND o.index_generation_id=")
            .push_bind(generation_id);
    }
    if let Some(cursor) = decoded.as_ref() {
        builder
            .push(" AND (o.")
            .push(spec.timestamp)
            .push(" < ")
            .push_bind(cursor.timestamp)
            .push(" OR (o.")
            .push(spec.timestamp)
            .push(" = ")
            .push_bind(cursor.timestamp)
            .push(" AND (");
        for (index, key) in spec.primary_keys.iter().enumerate() {
            if index > 0 {
                builder.push(" OR (");
                for prior in 0..index {
                    builder
                        .push("o.")
                        .push(spec.primary_keys[prior])
                        .push(" = ")
                        .push_bind(cursor.ids[prior]);
                    if prior + 1 < index {
                        builder.push(" AND ");
                    }
                }
                builder.push(" AND ");
            }
            builder
                .push("o.")
                .push(*key)
                .push(" < ")
                .push_bind(cursor.ids[index]);
            if index > 0 {
                builder.push(")");
            }
        }
        builder.push(")))");
    }
    builder
        .push(" ORDER BY o.")
        .push(spec.timestamp)
        .push(" DESC");
    for key in spec.primary_keys {
        builder.push(",o.").push(*key).push(" DESC");
    }
    builder.push(" LIMIT ").push_bind(i64::from(size) + 1);
    let raw = builder
        .build()
        .fetch_all(&state.pool)
        .await
        .map_err(ApiError::database)?;
    let has_more = raw.len() > usize::from(size);
    let mut rows = Vec::with_capacity(raw.len().min(usize::from(size)));
    for row in raw.iter().take(usize::from(size)) {
        rows.push(normalize_row(state, spec, row.get("row"))?);
    }
    let next_cursor = if has_more {
        raw.get(usize::from(size) - 1)
            .map(|row| {
                cursor_from_row(
                    state,
                    spec,
                    row.get("row"),
                    knowledge_base_id,
                    &scope.environment,
                    generation_id,
                )
            })
            .transpose()?
    } else {
        None
    };
    Ok(Page {
        spec,
        rows,
        has_more,
        next_cursor,
    })
}

fn cursor_from_row(
    state: &AdminState,
    spec: ResourceSpec,
    row: Value,
    knowledge_base_id: Uuid,
    environment: &str,
    generation_id: Option<Uuid>,
) -> Result<String, ApiError> {
    let timestamp = DateTime::parse_from_rfc3339(
        row.get(spec.timestamp)
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ROW_INVALID"))?,
    )
    .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ROW_INVALID"))?
    .with_timezone(&Utc);
    let ids = spec
        .primary_keys
        .iter()
        .map(|key| {
            row.get(*key)
                .and_then(Value::as_str)
                .and_then(|value| Uuid::parse_str(value).ok())
                .ok_or_else(|| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ROW_INVALID"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    encode_cursor(
        state,
        &CursorPayload {
            resource: spec.name.into(),
            knowledge_base_id,
            environment: environment.to_string(),
            generation_id,
            timestamp,
            ids,
        },
    )
}

fn encode_cursor(state: &AdminState, payload: &CursorPayload) -> Result<String, ApiError> {
    encode_cursor_with_key(&state.cursor_key, payload)
}

fn encode_cursor_with_key(key: &[u8], payload: &CursorPayload) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(payload)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ENCODING_FAILED"))?;
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ENCODING_FAILED"))?;
    mac.update(&bytes);
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        hex(&mac.finalize().into_bytes())
    ))
}

fn decode_cursor(
    state: &AdminState,
    spec: ResourceSpec,
    value: &str,
    knowledge_base_id: Uuid,
    environment: &str,
    generation_id: Option<Uuid>,
) -> Result<CursorPayload, ApiError> {
    decode_cursor_with_key(
        &state.cursor_key,
        spec,
        value,
        knowledge_base_id,
        environment,
        generation_id,
    )
}

fn decode_cursor_with_key(
    key: &[u8],
    spec: ResourceSpec,
    value: &str,
    knowledge_base_id: Uuid,
    environment: &str,
    generation_id: Option<Uuid>,
) -> Result<CursorPayload, ApiError> {
    if value.len() > MAXIMUM_CURSOR_BYTES {
        return Err(ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"));
    }
    let (body, signature) = value
        .split_once('.')
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"))?;
    let bytes = URL_SAFE_NO_PAD
        .decode(body)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"))?;
    let mut mac = HmacSha256::new_from_slice(key)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_CURSOR_ENCODING_FAILED"))?;
    mac.update(&bytes);
    let signature = decode_hex(signature)
        .ok_or_else(|| ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"))?;
    mac.verify_slice(&signature)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"))?;
    let payload: CursorPayload = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"))?;
    if payload.resource != spec.name
        || payload.knowledge_base_id != knowledge_base_id
        || payload.environment != environment
        || payload.generation_id != generation_id
        || payload.ids.len() != spec.primary_keys.len()
    {
        return Err(ApiError::bad_request("KNOWLEDGE_ADMIN_CURSOR_INVALID"));
    }
    Ok(payload)
}

fn resolve_page_size(requested: Option<u16>, maximum: u16) -> Result<u16, ApiError> {
    match requested {
        Some(value) if value > 0 && value <= maximum => Ok(value),
        Some(_) => Err(ApiError::bad_request("KNOWLEDGE_ADMIN_PAGE_SIZE_INVALID")),
        None => Ok(maximum),
    }
}

fn normalize_row(state: &AdminState, spec: ResourceSpec, row: Value) -> Result<Value, ApiError> {
    let source = row
        .as_object()
        .ok_or_else(|| ApiError::internal("KNOWLEDGE_ADMIN_ROW_INVALID"))?;
    let mut target = Map::new();
    let mut redacted = Vec::new();
    for field in spec.fields {
        let Some(mut value) = source.get(*field).cloned() else {
            continue;
        };
        let camel = snake_to_camel(field);
        if *field == "authorized_by" {
            if let Some(raw) = value.as_str() {
                if raw.starts_with("actor:v1:") && raw.len() <= 128 {
                    value = Value::String(raw.to_string());
                } else {
                    value = Value::String(opaque_actor(state, raw)?);
                    redacted.push(format!("{camel}.rawPrincipal"));
                }
            }
        }
        if *field == "error_summary" {
            value = safe_error_summary(value, &mut redacted);
        }
        let maximum = match *field {
            "error_summary" => 1_024,
            "metrics"
            | "evidence"
            | "strategy_projections"
            | "verification_evidence"
            | "progress" => 32_768,
            "mismatch_counts" | "reference_counts" | "deletion_counts" => 16_384,
            _ => 0,
        };
        if maximum > 0 {
            value = safe_json(value, maximum, &camel, &mut redacted)?;
        }
        target.insert(camel, value);
    }
    if !redacted.is_empty() {
        target.insert("redactedFields".into(), json!(redacted));
    }
    let normalized = Value::Object(target);
    if serde_json::to_vec(&normalized).map_or(true, |bytes| bytes.len() > MAXIMUM_ROW_BYTES) {
        return Err(ApiError::field_too_large(spec.name, MAXIMUM_ROW_BYTES));
    }
    Ok(normalized)
}

fn safe_error_summary(value: Value, redacted: &mut Vec<String>) -> Value {
    let Some(object) = value.as_object() else {
        redacted.push("errorSummary".into());
        return Value::Null;
    };
    let raw_code = object
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN");
    let code = raw_code.chars().take(64).collect::<String>();
    if raw_code.chars().count() > 64 {
        redacted.push("errorSummary.code".into());
    }
    let raw_message = object.get("message").and_then(Value::as_str);
    let message = raw_message.map(|value| value.chars().take(512).collect::<String>());
    if raw_message.is_some_and(|value| value.chars().count() > 512) {
        redacted.push("errorSummary.message".into());
    }
    if object.len() > 2 {
        redacted.push("errorSummary.*".into());
    }
    let mut safe = Map::from_iter([("code".into(), json!(code))]);
    if let Some(message) = message {
        safe.insert("message".into(), json!(message));
    }
    Value::Object(safe)
}

fn safe_json(
    value: Value,
    maximum: usize,
    field: &str,
    redacted: &mut Vec<String>,
) -> Result<Value, ApiError> {
    let scrubbed = scrub_json(value, 0, redacted, field);
    if serde_json::to_vec(&scrubbed).map_or(true, |bytes| bytes.len() > maximum) {
        return Err(ApiError::field_too_large(field, maximum));
    }
    Ok(scrubbed)
}

fn scrub_json(value: Value, depth: usize, redacted: &mut Vec<String>, path: &str) -> Value {
    if depth > 2 {
        redacted.push(format!("{path}.*"));
        return Value::Null;
    }
    match value {
        Value::Object(object) => {
            if object.len() > 64 {
                redacted.push(format!("{path}.*"));
            }
            Value::Object(
                object
                    .into_iter()
                    .take(64)
                    .filter_map(|(key, value)| {
                        let lowered = key.to_ascii_lowercase();
                        if [
                            "provider",
                            "principal",
                            "content",
                            "locator",
                            "filename",
                            "secret",
                            "token",
                        ]
                        .iter()
                        .any(|part| lowered.contains(part))
                        {
                            redacted.push(format!("{path}.{key}"));
                            None
                        } else {
                            Some((
                                key.clone(),
                                scrub_json(value, depth + 1, redacted, &format!("{path}.{key}")),
                            ))
                        }
                    })
                    .collect(),
            )
        }
        Value::Array(values) => {
            if values.len() > 64 {
                redacted.push(format!("{path}.*"));
            }
            Value::Array(
                values
                    .into_iter()
                    .take(64)
                    .enumerate()
                    .map(|(index, value)| {
                        scrub_json(value, depth + 1, redacted, &format!("{path}[{index}]"))
                    })
                    .collect(),
            )
        }
        Value::String(value) => {
            if value.chars().count() > 512 {
                redacted.push(path.to_string());
            }
            Value::String(value.chars().take(512).collect())
        }
        scalar => scalar,
    }
}

fn opaque_actor(state: &AdminState, raw: &str) -> Result<String, ApiError> {
    let mut mac = HmacSha256::new_from_slice(&state.opaque_actor_key)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_ACTOR_ENCODING_FAILED"))?;
    mac.update(raw.as_bytes());
    Ok(format!(
        "actor:v1:{}",
        &hex(&mac.finalize().into_bytes())[..24]
    ))
}

fn snake_to_camel(value: &str) -> String {
    let mut result = String::new();
    let mut uppercase = false;
    for character in value.chars() {
        if character == '_' {
            uppercase = true
        } else if uppercase {
            result.extend(character.to_uppercase());
            uppercase = false
        } else {
            result.push(character)
        }
    }
    result
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
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

async fn estimate(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<EstimateRequest>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let scope = authorize(&state, &headers, "knowledge.admin.migration-estimate.read").await?;
    require_ids(&scope, &[id])?;
    let context = knowledge_base_context(&state, &scope, id).await?;
    let row=sqlx::query("SELECT pointer.index_generation_id AS source_generation_id,target.profile_id AS target_profile_id,target.profile_revision AS target_profile_revision,target.expected_space_id AS target_space_id,target.expected_space_revision AS target_space_revision,target.dimension AS target_dimension,count(chunk.chunk_id)::bigint AS estimated_chunk_count,COALESCE(sum(chunk.token_count),0)::bigint AS estimated_token_count,ceil(COALESCE(sum(chunk.token_count),0)*COALESCE(policy.migration_cost_per_token_micros,0))::bigint AS estimated_cost_micros,GREATEST(1,ceil(count(chunk.chunk_id)::numeric/32))::bigint AS estimated_duration_seconds,(COALESCE(sum(length(chunk.chunk_text)),0)+count(chunk.chunk_id)*target.dimension*4)::bigint AS estimated_temporary_bytes,CASE WHEN source.space_id=target.expected_space_id AND source.space_revision=target.expected_space_revision AND source.dimension=target.dimension THEN jsonb_build_array('TARGET_SPACE_UNCHANGED') WHEN EXISTS(SELECT 1 FROM knowledge_embedding_migration_t active WHERE active.knowledge_base_id=b.knowledge_base_id AND active.state IN ('REQUESTED','PREFLIGHTED','BACKFILLING','PAUSED','CATCHING_UP','VALIDATING','READY','PROMOTED','SOAKING')) THEN jsonb_build_array('ACTIVE_MIGRATION_EXISTS') ELSE '[]'::jsonb END AS blocking_conditions FROM knowledge_base_t b JOIN knowledge_index_pointer_t pointer ON pointer.knowledge_base_id=b.knowledge_base_id AND pointer.environment=b.environment JOIN knowledge_index_generation_t source ON source.index_generation_id=pointer.index_generation_id JOIN knowledge_embedding_profile_runtime_v target ON target.profile_id=$1 AND target.profile_revision=$2 LEFT JOIN knowledge_operational_policy_t policy ON policy.knowledge_base_id=b.knowledge_base_id LEFT JOIN knowledge_document_t document ON document.knowledge_base_id=b.knowledge_base_id AND document.lifecycle_state='ACTIVE' LEFT JOIN knowledge_chunk_t chunk ON chunk.document_version_id=document.current_document_version_id WHERE b.knowledge_base_id=$3 AND b.environment=$4 AND (b.host_id=$5 OR (b.host_id IS NULL AND $6)) GROUP BY pointer.index_generation_id,target.profile_id,target.profile_revision,target.expected_space_id,target.expected_space_revision,target.dimension,policy.migration_cost_per_token_micros,source.space_id,source.space_revision,source.dimension,b.knowledge_base_id")
        .bind(request.target_profile_id).bind(request.target_profile_revision).bind(id).bind(&scope.environment).bind(scope.host_id).bind(scope.global_read)
        .fetch_optional(&state.pool).await.map_err(ApiError::database)?
        .ok_or_else(||ApiError::not_found("KNOWLEDGE_BASE_NOT_FOUND"))?;
    write_admin_audit(
        &state,
        &headers,
        &scope,
        id,
        "EMBEDDING_MIGRATION_ESTIMATE",
        &serde_json::to_value(&request)
            .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_AUDIT_ENCODING_FAILED"))?,
        None,
        1,
        started.elapsed(),
    )
    .await?;
    bounded_response(
        &state,
        json!({"knowledgeBaseEmbeddingMigrationEstimate":[row_to_camel_json(&row)?],
            "knowledgeBaseId":id,"environment":scope.environment,
            "ownerScope":context.owner_scope,"configuration":context.configuration,
            "asOf":Utc::now()}),
    )
}

async fn simulate(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(request): Json<SimulationRequest>,
) -> Result<Response, ApiError> {
    let started = Instant::now();
    let scope = authorize(
        &state,
        &headers,
        "knowledge.admin.authorization-simulation.read",
    )
    .await?;
    require_ids(&scope, &[id])?;
    let context = knowledge_base_context(&state, &scope, id).await?;
    if !["USER", "GROUP", "ORGANIZATION"].contains(&request.subject_type.as_str())
        || request.subject_id.is_empty()
        || request.subject_id.len() > 255
    {
        return Err(ApiError::bad_request("KNOWLEDGE_ADMIN_SUBJECT_INVALID"));
    }
    let row=sqlx::query("WITH documents AS (SELECT d.document_id,s.acl_mode,acl.acl_revision_id,acl.completeness_state,acl.observed_ts,acl.fresh_until_ts,acl.provider_effective_decision,state.state AS source_acl_state,state.fresh_until_ts AS source_fresh_until,state.unresolved_subject_count FROM knowledge_document_t d JOIN knowledge_source_t s ON s.source_id=d.source_id JOIN knowledge_base_t b ON b.knowledge_base_id=d.knowledge_base_id JOIN LATERAL (SELECT revision.* FROM knowledge_document_acl_t revision WHERE revision.document_id=d.document_id ORDER BY revision.acl_sequence DESC LIMIT 1) acl ON TRUE LEFT JOIN knowledge_source_acl_state_t state ON state.source_id=s.source_id WHERE d.knowledge_base_id=$1 AND b.environment=$2 AND (b.host_id=$3 OR (b.host_id IS NULL AND $4)) AND d.lifecycle_state='ACTIVE'),decisions AS (SELECT document.*,EXISTS(SELECT 1 FROM knowledge_acl_subject_t subject WHERE subject.acl_revision_id=document.acl_revision_id AND subject.effect='ALLOW' AND subject.mapping_complete AND ((subject.normalized_subject_type=$5 AND subject.normalized_subject_id=$6) OR (subject.normalized_subject_type='EVERYONE' AND subject.normalized_subject_id='*'))) AS has_allow,EXISTS(SELECT 1 FROM knowledge_acl_subject_t subject WHERE subject.acl_revision_id=document.acl_revision_id AND subject.effect='DENY' AND subject.mapping_complete AND ((subject.normalized_subject_type=$5 AND subject.normalized_subject_id=$6) OR (subject.normalized_subject_type='EVERYONE' AND subject.normalized_subject_id='*'))) AS has_deny,EXISTS(SELECT 1 FROM knowledge_acl_subject_t subject WHERE subject.acl_revision_id=document.acl_revision_id AND NOT subject.mapping_complete) AS has_unresolved FROM documents document) SELECT count(*) AS evaluated_document_count,count(*) FILTER (WHERE acl_mode='UNIFORM_SCOPE' OR (completeness_state='COMPLETE' AND provider_effective_decision AND fresh_until_ts>now() AND source_acl_state='COMPLETE' AND source_fresh_until>now() AND unresolved_subject_count=0 AND NOT has_unresolved AND NOT has_deny AND has_allow)) AS allowed_document_count,count(*) FILTER (WHERE acl_mode='MIRROR_SOURCE_ACL' AND (completeness_state<>'COMPLETE' OR NOT provider_effective_decision OR fresh_until_ts<=now() OR source_acl_state IS DISTINCT FROM 'COMPLETE' OR source_fresh_until<=now() OR unresolved_subject_count<>0 OR has_unresolved OR has_deny OR NOT has_allow)) AS excluded_document_count FROM decisions")
        .bind(id).bind(&scope.environment).bind(scope.host_id).bind(scope.global_read).bind(&request.subject_type).bind(&request.subject_id)
        .fetch_one(&state.pool).await.map_err(ApiError::database)?;
    let evaluated_document_count: i64 = row.get("evaluated_document_count");
    write_admin_audit(
        &state,
        &headers,
        &scope,
        id,
        "AUTHORIZATION_SIMULATION",
        &serde_json::to_value(&request)
            .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_AUDIT_ENCODING_FAILED"))?,
        Some(&format!("{}:{}", request.subject_type, request.subject_id)),
        u64::try_from(evaluated_document_count).unwrap_or(0),
        started.elapsed(),
    )
    .await?;
    bounded_response(
        &state,
        json!({"knowledgeAuthorizationSimulation":[row_to_camel_json(&row)?],
            "knowledgeBaseId":id,"environment":scope.environment,
            "ownerScope":context.owner_scope,"configuration":context.configuration,
            "asOf":Utc::now()}),
    )
}

#[allow(clippy::too_many_arguments)]
async fn write_admin_audit(
    state: &AdminState,
    headers: &HeaderMap,
    scope: &Scope,
    knowledge_base_id: Uuid,
    operation: &str,
    input: &Value,
    subject: Option<&str>,
    result_count: u64,
    elapsed: Duration,
) -> Result<(), ApiError> {
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
                })
        })
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let input_bytes = serde_json::to_vec(input)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_AUDIT_ENCODING_FAILED"))?;
    let subject_ref = subject
        .map(|value| opaque_actor(state, value))
        .transpose()?;
    sqlx::query(
        "INSERT INTO knowledge_admin_audit_t(
           admin_audit_id,request_id,knowledge_base_id,consumer_host_id,
           environment,operation,input_digest,subject_ref,result_count,latency_ms)
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
    .bind(Uuid::now_v7())
    .bind(request_id)
    .bind(knowledge_base_id)
    .bind(scope.host_id)
    .bind(&scope.environment)
    .bind(operation)
    .bind(hex(&Sha256::digest(input_bytes)))
    .bind(subject_ref)
    .bind(i64::try_from(result_count).unwrap_or(i64::MAX))
    .bind(
        i64::try_from(elapsed.as_millis())
            .unwrap_or(2_000)
            .min(2_000),
    )
    .execute(&state.pool)
    .await
    .map_err(ApiError::database)?;
    Ok(())
}

fn row_to_camel_json(row: &PgRow) -> Result<Value, ApiError> {
    let mut object = Map::new();
    for column in row.columns() {
        let name = column.name();
        let value = if let Ok(value) = row.try_get::<i64, _>(name) {
            json!(value)
        } else if let Ok(value) = row.try_get::<i32, _>(name) {
            json!(value)
        } else if let Ok(value) = row.try_get::<String, _>(name) {
            json!(value)
        } else if let Ok(value) = row.try_get::<Uuid, _>(name) {
            json!(value)
        } else if let Ok(value) = row.try_get::<Value, _>(name) {
            value
        } else if let Ok(value) = row.try_get::<bool, _>(name) {
            json!(value)
        } else {
            Value::Null
        };
        object.insert(snake_to_camel(name), value);
    }
    Ok(Value::Object(object))
}

fn bounded_response(state: &AdminState, value: Value) -> Result<Response, ApiError> {
    let result_count = top_level_result_count(&value);
    let redaction_count = redaction_count(&value);
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_RESPONSE_ENCODING_FAILED"))?;
    if bytes.len() > state.config.maximum_response_bytes {
        return Err(ApiError::too_large("KNOWLEDGE_ADMIN_RESPONSE_TOO_LARGE"));
    }
    let mut response = (
        StatusCode::OK,
        [("content-type", RESPONSE_CONTENT_TYPE)],
        Bytes::from(bytes),
    )
        .into_response();
    response.headers_mut().insert(
        "x-knowledge-result-count",
        HeaderValue::from_str(&result_count.to_string())
            .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_METRICS_INVALID"))?,
    );
    response.headers_mut().insert(
        "x-knowledge-redaction-count",
        HeaderValue::from_str(&redaction_count.to_string())
            .map_err(|_| ApiError::internal("KNOWLEDGE_ADMIN_METRICS_INVALID"))?,
    );
    Ok(response)
}

fn top_level_result_count(value: &Value) -> usize {
    value
        .as_object()
        .map(|object| {
            object
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "redactedFields"))
                .filter_map(|(_, value)| value.as_array())
                .map(Vec::len)
                .sum()
        })
        .unwrap_or(0)
}

fn redaction_count(value: &Value) -> usize {
    match value {
        Value::Object(object) => {
            object
                .get("redactedFields")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
                + object.values().map(redaction_count).sum::<usize>()
        }
        Value::Array(values) => values.iter().map(redaction_count).sum(),
        _ => 0,
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    details: Map<String, Value>,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            details: Map::new(),
        }
    }
    fn bad_request(code: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code)
    }
    fn unauthorized(code: &'static str) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code)
    }
    fn forbidden(code: &'static str) -> Self {
        Self::new(StatusCode::FORBIDDEN, code)
    }
    fn not_found(code: &'static str) -> Self {
        Self::new(StatusCode::NOT_FOUND, code)
    }
    fn too_large(code: &'static str) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, code)
    }
    fn conflict(code: &'static str) -> Self {
        Self::new(StatusCode::CONFLICT, code)
    }
    fn timeout(code: &'static str) -> Self {
        Self::new(StatusCode::GATEWAY_TIMEOUT, code)
    }
    fn unavailable(code: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, code)
    }
    fn internal(code: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code)
    }
    fn database(error: sqlx::Error) -> Self {
        tracing::error!(error=%error,"Knowledge administration query failed");
        Self::unavailable("KNOWLEDGE_ADMIN_DATABASE_UNAVAILABLE")
    }
    fn field_too_large(field: &str, maximum: usize) -> Self {
        let mut error = Self::too_large("KNOWLEDGE_ADMIN_FIELD_TOO_LARGE");
        error.details.insert("field".into(), json!(field));
        error.details.insert("maximumBytes".into(), json!(maximum));
        error
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.details;
        body.insert("statusCode".into(), json!(self.status.as_u16()));
        body.insert("code".into(), json!(self.code));
        body.insert("message".into(), json!(self.code));
        (self.status, Json(Value::Object(body))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegated_portal_claims_support_array_and_string_scopes() {
        let claims = json!({"scp":["portal.r"],"scope":"portal.r portal.w"});
        let scopes = delegated_scopes(&claims);
        assert!(scopes.contains("portal.r"));
        assert!(scopes.contains("portal.w"));
    }

    #[test]
    fn delegated_roles_are_split_without_substring_matching() {
        let principal = AuthPrincipal {
            role: Some("user host-admin platformKnowledgeBaseAdmin".into()),
            ..AuthPrincipal::default()
        };
        let roles = delegated_roles(&principal);
        assert!(roles.contains("host-admin"));
        assert!(roles.contains("platformKnowledgeBaseAdmin"));
        assert!(!roles.contains("admin"));
    }

    #[test]
    fn delegated_scope_derives_tenant_and_global_visibility_from_signed_claims() {
        let host_id = Uuid::now_v7();
        let mut headers = HeaderMap::new();
        headers.insert("x-knowledge-environment", HeaderValue::from_static("dev"));
        let tenant = AuthPrincipal {
            host: Some(host_id.to_string()),
            role: Some("host-admin".into()),
            claims: json!({"scp":["portal.r"]}),
            ..AuthPrincipal::default()
        };
        let tenant_scope = validate_delegated_user_claims(&headers, &tenant, "portal.r")
            .expect("tenant administrator");
        assert_eq!(tenant_scope.host_id, host_id);
        assert!(!tenant_scope.global_read);

        let global = AuthPrincipal {
            role: Some("platformKnowledgeBaseAdmin".into()),
            ..tenant
        };
        assert!(
            validate_delegated_user_claims(&headers, &global, "portal.r")
                .expect("global administrator")
                .global_read
        );
    }

    #[test]
    fn delegated_scope_rejects_wrong_scope_role_host_and_environment() {
        let mut headers = HeaderMap::new();
        headers.insert("x-knowledge-environment", HeaderValue::from_static("dev"));
        let valid = AuthPrincipal {
            host: Some(Uuid::now_v7().to_string()),
            role: Some("host-admin".into()),
            claims: json!({"scp":["portal.r"]}),
            ..AuthPrincipal::default()
        };
        assert!(validate_delegated_user_claims(&headers, &valid, "portal.w").is_err());
        assert!(
            validate_delegated_user_claims(
                &headers,
                &AuthPrincipal {
                    role: Some("user".into()),
                    ..valid.clone()
                },
                "portal.r"
            )
            .is_err()
        );
        assert!(
            validate_delegated_user_claims(
                &headers,
                &AuthPrincipal {
                    host: None,
                    ..valid.clone()
                },
                "portal.r"
            )
            .is_err()
        );
        headers.insert(
            "x-knowledge-environment",
            HeaderValue::from_static("dev' OR true--"),
        );
        assert!(validate_delegated_user_claims(&headers, &valid, "portal.r").is_err());
    }

    #[test]
    fn route_capabilities_are_explicit_and_fail_closed() {
        assert_eq!(
            required_scope("knowledge.admin.operational.read"),
            Some("portal.r")
        );
        assert_eq!(
            required_scope("knowledge.admin.command.write"),
            Some("portal.w")
        );
        assert_eq!(required_scope("knowledge.admin.unregistered.read"), None);
    }

    #[test]
    fn public_router_has_no_administration_route() {
        let runtime_openapi = include_str!("../../light-knowledge/openapi.yaml");
        assert!(!runtime_openapi.contains("/v1/knowledge/admin/"));
    }

    #[test]
    fn phase2_snapshot_inventory_and_command_map_are_frozen() {
        assert_eq!(SNAPSHOT_TABLES.len(), 6);
        for table in SNAPSHOT_TABLES {
            assert!(!snapshot_primary_keys(table).is_empty());
        }
        for action in [
            "testKnowledgeSource",
            "requestKnowledgeSourceSync",
            "requestKnowledgeSourceAclReconciliation",
            "receiveKnowledgeSourceProviderNotification",
            "requestKnowledgeBaseReindex",
            "requestKnowledgeBaseCompaction",
            "promoteKnowledgeBaseIndexGeneration",
            "requestKnowledgeBasePurge",
            "testKnowledgeRetrieval",
            "requestKnowledgeBaseEmbeddingMigration",
            "pauseKnowledgeBaseEmbeddingMigration",
            "resumeKnowledgeBaseEmbeddingMigration",
            "cancelKnowledgeBaseEmbeddingMigration",
            "rollbackKnowledgeBaseIndexGeneration",
            "retireKnowledgeBaseIndexGeneration",
            "requestKnowledgeBaseBackupCheckpoint",
            "verifyKnowledgeBasePhysicalRestore",
        ] {
            assert!(operational_job_type(action).is_some(), "{action}");
        }
        let admin_openapi = include_str!("../openapi.yaml");
        assert!(admin_openapi.contains("/v1/knowledge/admin/commands:"));
        assert!(admin_openapi.contains("/v1/knowledge/admin/control-snapshots:apply:"));
        assert!(admin_openapi.contains("OperationalResponse:"));
        assert!(admin_openapi.contains("'504':"));
        assert!(admin_openapi.contains("x-field-contract: operational-field-allowlists-v1"));
    }

    #[test]
    fn idempotency_replay_requires_the_same_action_and_payload() {
        let requested = json!({"knowledgeBaseId":"00000000-0000-0000-0000-000000000001"});
        let stored = json!({
            "knowledgeBaseId":"00000000-0000-0000-0000-000000000001",
            "authorizedBy":"actor:v1:opaque"
        });
        assert!(same_operational_command(
            "SYNC",
            &requested,
            "SYNC".into(),
            stored.clone()
        ));
        assert!(!same_operational_command(
            "PURGE",
            &requested,
            "SYNC".into(),
            stored.clone()
        ));
        assert!(!same_operational_command(
            "SYNC",
            &json!({"knowledgeBaseId":"00000000-0000-0000-0000-000000000002"}),
            "SYNC".into(),
            stored
        ));
    }

    #[test]
    fn frozen_resources_have_unique_fields_and_stable_keys() {
        for spec in [
            SYNC_RUNS,
            DOCUMENTS,
            GENERATIONS,
            SEGMENTS,
            UPLOADS,
            CHANGES,
            ANCHORS,
            COMPACTIONS,
            ANTI_ENTROPY,
            ACL_FRESHNESS,
            ACL_RECONCILIATIONS,
            ACL_TRANSITIONS,
            CONNECTOR_OBJECTS,
            EMBEDDING_MIGRATIONS,
            MIGRATION_EVALUATIONS,
            GENERATION_RETENTION,
            BACKUP_CHECKPOINTS,
            PURGE_EVIDENCE,
            PROMOTION_RECEIPTS,
        ] {
            assert_eq!(
                spec.fields.iter().copied().collect::<BTreeSet<_>>().len(),
                spec.fields.len()
            );
            assert!(!spec.primary_keys.is_empty());
            assert!(spec.fields.contains(&spec.timestamp));
            for key in spec.primary_keys {
                assert!(spec.fields.contains(key));
            }
        }
        assert!(!SEGMENTS.fields.contains(&"physical_locator"));
    }

    #[test]
    fn safe_json_removes_sensitive_keys_and_bounds_strings() {
        let mut redacted = Vec::new();
        let value = safe_json(
            json!({"passed":true,"providerEvidence":"secret","note":"x".repeat(700)}),
            32_768,
            "evidence",
            &mut redacted,
        )
        .expect("safe JSON");
        assert_eq!(value["passed"], true);
        assert!(value.get("providerEvidence").is_none());
        assert_eq!(value["note"].as_str().unwrap().len(), 512);
        assert_eq!(redacted, vec!["evidence.note", "evidence.providerEvidence"]);
    }

    #[test]
    fn safe_json_rejects_a_field_that_remains_oversize() {
        let mut redacted = Vec::new();
        let error = safe_json(
            json!(
                (0..64)
                    .map(|index| (format!("k{index}"), Value::String("x".repeat(512))))
                    .collect::<Map<_, _>>()
            ),
            1_024,
            "metrics",
            &mut redacted,
        )
        .expect_err("oversize field must fail instead of starving the response");
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.code, "KNOWLEDGE_ADMIN_FIELD_TOO_LARGE");
        assert_eq!(error.details.get("maximumBytes"), Some(&json!(1_024)));
    }

    #[test]
    fn page_size_is_strictly_bounded() {
        assert_eq!(resolve_page_size(None, 200).expect("default"), 200);
        assert_eq!(resolve_page_size(Some(1), 200).expect("minimum"), 1);
        assert!(resolve_page_size(Some(0), 200).is_err());
        assert!(resolve_page_size(Some(201), 200).is_err());
    }

    #[test]
    fn cursor_is_signed_and_bound_to_resource_scope_and_generation() {
        let key = b"a fixed test-only cursor key with enough entropy";
        let knowledge_base_id = Uuid::now_v7();
        let generation_id = Uuid::now_v7();
        let payload = CursorPayload {
            resource: SEGMENTS.name.into(),
            knowledge_base_id,
            environment: "dev".into(),
            generation_id: Some(generation_id),
            timestamp: Utc::now(),
            ids: vec![Uuid::now_v7()],
        };
        let cursor = encode_cursor_with_key(key, &payload).expect("cursor");
        let decoded = decode_cursor_with_key(
            key,
            SEGMENTS,
            &cursor,
            knowledge_base_id,
            "dev",
            Some(generation_id),
        )
        .expect("bound cursor");
        assert_eq!(decoded.ids, payload.ids);
        assert!(
            decode_cursor_with_key(
                key,
                SEGMENTS,
                &cursor,
                Uuid::now_v7(),
                "dev",
                Some(generation_id)
            )
            .is_err()
        );
        assert!(
            decode_cursor_with_key(
                key,
                SEGMENTS,
                &format!("{}0", cursor),
                knowledge_base_id,
                "dev",
                Some(generation_id)
            )
            .is_err()
        );
        assert!(
            decode_cursor_with_key(
                key,
                SEGMENTS,
                &"x".repeat(MAXIMUM_CURSOR_BYTES + 1),
                knowledge_base_id,
                "dev",
                Some(generation_id)
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn metrics_use_fixed_route_labels_without_tenant_identifiers() {
        let metrics = AdminMetrics::default();
        metrics.record(
            "/v1/knowledge/admin/knowledge-bases/{id}/documents",
            StatusCode::OK,
            Duration::from_millis(12),
            2,
            1,
        );
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1/unused")
            .expect("lazy pool");
        let output = metrics.prometheus(&pool);
        assert!(output.contains("route=\"/v1/knowledge/admin/knowledge-bases/{id}/documents\""));
        assert!(!output.contains("knowledge_base_id"));
        assert!(!output.contains("host_id"));
    }
}
