use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use hmac::{Hmac, Mac};
use knowledge_connectors::{
    ConnectorKind, ConnectorPage, ConnectorSyncMode, ValidatedConnectorPage, normalize_permission,
    permission_digest, stable_objects,
};
use knowledge_core::{
    BaseManifest, ChangeKind, CorpusDocumentState, DocumentInput, FullBaseGeneration,
    ProcessingContract, SourceLimits, build_full_base, classify_corpus_changes,
    compact_resolved_generation, ingest_markdown_repository, sha256_hex,
};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkerConfig {
    version: u16,
    worker_database_url_file: PathBuf,
    projector_database_url_file: PathBuf,
    heartbeat_secret_file: PathBuf,
    #[serde(default)]
    portal_command_url: Option<String>,
    #[serde(default)]
    portal_authorization_file: Option<PathBuf>,
    checkout_root: PathBuf,
    approved_repository_uri: String,
    immutable_commit: String,
    maximum_checkout_seconds: u64,
    object_store_root: PathBuf,
    projector_id: String,
    knowledge_base_id: Uuid,
    source_id: Uuid,
    environment: String,
    embedding_profile_id: Uuid,
    embedding_profile_revision: i64,
    #[serde(default = "default_true")]
    deterministic_pilot: bool,
    #[serde(default)]
    embedding_gateway_url: Option<String>,
    #[serde(default)]
    embedding_authorization_file: Option<PathBuf>,
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
    snapshot_watermark: u64,
    limits: SourceLimits,
    #[serde(default)]
    enterprise_connector_fixture_file: Option<PathBuf>,
    #[serde(default)]
    enterprise_connector_approved_origin: Option<String>,
    #[serde(default)]
    enterprise_connector_page_url: Option<String>,
    #[serde(default)]
    enterprise_connector_authorization_file: Option<PathBuf>,
}

impl WorkerConfig {
    fn load() -> Result<Self> {
        let path = env::var("LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE")
            .unwrap_or_else(|_| "config/worker.yml".to_string());
        let content =
            fs::read_to_string(&path).with_context(|| format!("read worker config {path}"))?;
        let config: Self = serde_yaml::from_str(&content)
            .with_context(|| format!("parse worker config {path}"))?;
        let enterprise_connector_configured = config.enterprise_connector_fixture_file.is_some()
            || config.enterprise_connector_page_url.is_some();
        if config.version != 1
            || config.embedding_profile_revision < 1
            || config.snapshot_watermark == 0
            || config.environment.trim().is_empty()
            || config.projector_id.trim().is_empty()
            || !config.worker_database_url_file.is_file()
            || !config.projector_database_url_file.is_file()
            || !config.heartbeat_secret_file.is_file()
            || config.maximum_checkout_seconds == 0
            || !config.checkout_root.is_dir()
            || !valid_repository_uri(&config.approved_repository_uri)
            || !valid_commit(&config.immutable_commit)
            || config.embedding_batch_size == 0
            || config.embedding_batch_size > 128
            || config.embedding_dimension == 0
            || config.embedding_space_revision == 0
            || config.embedding_space_id.trim().is_empty()
            || config.embedding_alias.trim().is_empty()
            || (config.deterministic_pilot && config.embedding_gateway_url.is_some())
            || (!config.deterministic_pilot
                && (config
                    .embedding_gateway_url
                    .as_deref()
                    .is_none_or(|url| !url.starts_with("https://"))
                    || config
                        .embedding_authorization_file
                        .as_ref()
                        .is_none_or(|path| !path.is_file())))
            || (enterprise_connector_configured
                != config.enterprise_connector_approved_origin.is_some())
            || (config.enterprise_connector_page_url.is_some()
                != config.enterprise_connector_authorization_file.is_some())
            || config
                .enterprise_connector_page_url
                .as_deref()
                .is_some_and(|url| !url.starts_with("https://"))
            || (config.enterprise_connector_fixture_file.is_some()
                && config.enterprise_connector_page_url.is_some())
            || config
                .enterprise_connector_fixture_file
                .as_ref()
                .is_some_and(|path| !path.is_file())
            || config
                .enterprise_connector_authorization_file
                .as_ref()
                .is_some_and(|path| !path.is_file())
        {
            bail!("invalid Phase 1a worker configuration");
        }
        Ok(config)
    }
}

fn default_true() -> bool {
    true
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
    let config = WorkerConfig::load()?;
    fs::create_dir_all(&config.object_store_root)?;
    let command = env::args().nth(1).unwrap_or_else(|| "build".into());
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
        "build" => build(&pool, &config).await,
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
];

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
        _ => return None,
    })
}

async fn job_loop(pool: &PgPool, config: &WorkerConfig, lane: WorkerLane) -> Result<()> {
    loop {
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
              ORDER BY created_ts FOR UPDATE SKIP LOCKED LIMIT 1"
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
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            continue;
        };
        let job_id: Uuid = job.get("job_id");
        let knowledge_base_id: Uuid = job.get("knowledge_base_id");
        let source_id: Option<Uuid> = job.get("source_id");
        let job_type: String = job.get("job_type");
        let payload: serde_json::Value = job.get("payload");
        sqlx::query("UPDATE knowledge_job_t SET state='RUNNING',claim_token=$2,lease_expires_ts=now()+interval '5 minutes',attempt_count=attempt_count+1,update_ts=now() WHERE job_id=$1")
            .bind(job_id).bind(Uuid::now_v7()).execute(&mut *tx).await?;
        tx.commit().await?;
        let result = if knowledge_base_id != config.knowledge_base_id
            || source_id.is_some_and(|value| value != config.source_id)
        {
            Err(anyhow::anyhow!(
                "job does not match this bounded pilot worker identity"
            ))
        } else if job_type == "PROMOTE" {
            promote_generation(pool, config, &payload).await
        } else if job_type == "PROVIDER_NOTIFICATION" {
            match record_connector_notification(pool, config, &payload).await {
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
            prepare_checkout(config).await.map(|_| ())
        } else if job_type == "UPLOAD" {
            process_upload(pool, config, &payload).await
        } else if job_type == "DELTA_SYNC" {
            incremental_build(pool, config).await
        } else if job_type == "SYNC" {
            match phase1b_schema_ready(pool).await {
                Ok(true) => incremental_build(pool, config).await,
                Ok(false) => build(pool, config).await,
                Err(error) => Err(error),
            }
        } else if job_type == "COMPACTION" {
            compact_generation(pool, config).await
        } else if job_type == "ANTI_ENTROPY" {
            run_anti_entropy(pool, config, &payload).await
        } else if matches!(job_type.as_str(), "PURGE" | "RETRIEVAL_TEST") {
            Err(anyhow::anyhow!(
                "KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED:{job_type}"
            ))
        } else {
            build(pool, config).await
        };
        match result {
            Ok(()) => {
                sqlx::query("UPDATE knowledge_job_t SET state='SUCCEEDED',result=jsonb_build_object('completed',true),lease_expires_ts=NULL,update_ts=now() WHERE job_id=$1")
                    .bind(job_id).execute(pool).await?;
                publish_promotion_acknowledgements(pool, config).await?;
            }
            Err(error) => {
                tracing::error!(job_id=%job_id, %error, "bounded Knowledge build failed");
                sqlx::query("UPDATE knowledge_job_t SET state='FAILED',result=jsonb_build_object('code',$2),lease_expires_ts=NULL,update_ts=now() WHERE job_id=$1")
                    .bind(job_id)
                    .bind(worker_error_code(&error))
                    .execute(pool).await?;
            }
        }
    }
}

fn worker_error_code(error: &anyhow::Error) -> &'static str {
    if error
        .to_string()
        .starts_with("KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED:")
    {
        "KNOWLEDGE_JOB_TYPE_NOT_IMPLEMENTED"
    } else {
        "KNOWLEDGE_BUILD_FAILED"
    }
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
    let checkout = prepare_checkout(config).await?;
    let documents = ingest_markdown_repository(checkout.path(), &config.limits)?;
    let mut generation = build_full_base(
        config.knowledge_base_id,
        config.snapshot_watermark,
        &documents,
        &ProcessingContract::default(),
        &config.limits,
    )?;
    apply_configured_embeddings(config, &mut generation).await?;
    let objects = write_objects(&config.object_store_root, &generation, &documents)?;
    persist_full_base(pool, config, &generation, &objects).await?;
    println!("{}", serde_json::to_string_pretty(&generation.manifest)?);
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
    let bytes = fs::read(&locator)?;
    let expected_digest: String = row.get::<String, _>("staged_digest").trim().into();
    if sha256_hex(&bytes) != expected_digest {
        bail!("verified upload digest changed before indexing");
    }
    let markdown = String::from_utf8(bytes).context("verified upload is not UTF-8 text")?;
    let input = DocumentInput {
        source_object_id: row.get("source_object_id"),
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
    let current = ingest_markdown_repository(checkout.path(), &config.limits)?
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
        return Ok(serde_json::from_slice(&fs::read(fixture)?)?);
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
    let mut total_bytes = 0_usize;
    for _ in 0..1024 {
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
        total_bytes = total_bytes.saturating_add(bytes.len());
        if bytes.len() > 16 * 1024 * 1024 || total_bytes > 256 * 1024 * 1024 {
            bail!("KNOWLEDGE_CONNECTOR_PAGE_TOO_LARGE");
        }
        let page: ConnectorPage = serde_json::from_slice(&bytes)?;
        page.clone().validate(approved_origin)?;
        if page.requested_cursor != cursor {
            bail!("KNOWLEDGE_CONNECTOR_CURSOR_CHAIN_MISMATCH");
        }
        if let Some(first) = &combined {
            if first.provider != page.provider || first.sync_mode != page.sync_mode {
                bail!("KNOWLEDGE_CONNECTOR_PAGE_CONTRACT_CHANGED");
            }
        }
        for object in &page.objects {
            if !identities.insert(object.external_id.clone()) {
                bail!("KNOWLEDGE_CONNECTOR_DUPLICATE_OBJECT_ACROSS_PAGES");
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
    bail!("KNOWLEDGE_CONNECTOR_PAGE_LIMIT_EXCEEDED")
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
                d.source_object_id,d.canonical_uri,v.source_version,
                v.content_digest,v.object_locator
           FROM eligible_documents eligible
           JOIN knowledge_document_t d ON d.document_id=eligible.document_id
           JOIN knowledge_document_version_t v
             ON v.document_version_id=eligible.document_version_id
          WHERE d.knowledge_base_id=$2 AND d.source_id=$3
          ORDER BY d.document_id,eligible.ordinal DESC",
    )
    .bind(generation_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| {
            let locator: String = row.get("object_locator");
            let markdown =
                fs::read_to_string(object_locator_path(&config.object_store_root, &locator)?)?;
            Ok(CorpusDocumentState {
                source_object_id: row.get("source_object_id"),
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
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()?;
    let mut pending = (0..generation.chunks.len())
        .step_by(config.embedding_batch_size)
        .map(|start| {
            (
                start,
                (start + config.embedding_batch_size).min(generation.chunks.len()),
            )
        })
        .collect::<Vec<_>>();
    let mut vectors = vec![None; generation.chunks.len()];
    while let Some((start, end)) = pending.pop() {
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
            .json(&json!({
                "model": config.embedding_alias,
                "input": texts,
                "dimensions": config.embedding_dimension
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
                        == Some(config.embedding_space_id.as_str())
                    && response
                        .headers()
                        .get("x-light-embedding-space-revision")
                        .and_then(|value| value.to_str().ok())
                        == Some(config.embedding_space_revision.to_string().as_str()) =>
            {
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
            pending.push((middle, end));
            pending.push((start, middle));
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

fn valid_repository_uri(uri: &str) -> bool {
    uri.starts_with("https://") && !uri.contains('@') && !uri.contains('\n') && !uri.contains('\r')
}

fn valid_commit(commit: &str) -> bool {
    (commit.len() == 40 || commit.len() == 64)
        && commit.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    let sync_run_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO knowledge_sync_run_t(
           sync_run_id,knowledge_base_id,source_id,requested_by,start_watermark,
           snapshot_watermark,state,document_count,chunk_count,embedding_tokens,
           finished_ts
         ) VALUES($1,$2,$3,'light-knowledge-worker',$4,$4,'SUCCEEDED',
           $5,$6,$7,now())",
    )
    .bind(sync_run_id)
    .bind(config.knowledge_base_id)
    .bind(config.source_id)
    .bind(as_i64(config.snapshot_watermark as usize))
    .bind(as_i64(generation.manifest.document_count))
    .bind(as_i64(generation.manifest.chunk_count))
    .bind(as_i64(
        generation
            .chunks
            .iter()
            .map(|chunk| chunk.token_count)
            .sum(),
    ))
    .execute(&mut *tx)
    .await?;

    let metadata = sha256_hex(b"metadata-v1");
    let acl = sha256_hex(b"uniform-scope-acl-v1");
    let contract_set = sha256_hex(b"phase1a-contract-set-v1");
    let phase1b_references_available = sqlx::query_scalar::<_, Option<String>>(
        "SELECT to_regclass('knowledge_embedding_reference_t')::text",
    )
    .fetch_one(&mut *tx)
    .await?
    .is_some();
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
    .bind(json!({"syncRunId": sync_run_id, "fullBase": true}))
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
        let document_object = objects
            .documents
            .get(&chunk.document_version_id)
            .context("verified document object is missing")?;
        let acl_id = derived_uuid("acl", chunk.document_id);
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
        .bind(config.source_id)
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
        .bind(format!("sync:{sync_run_id}:{}", chunk.chunk_id))
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
        | "KnowledgeBaseRetrievalTestRequestedEvent" => {
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
            let job_type = projected_job_type(&event.event_type, enterprise_source)
                .context("projected Knowledge event has no job route")?;
            sqlx::query("INSERT INTO knowledge_job_t(job_id,knowledge_base_id,source_id,job_type,idempotency_key,requested_by,payload) VALUES($1,$2,$3,$4,$5,'portal-event',$6) ON CONFLICT(knowledge_base_id,idempotency_key) DO NOTHING")
                .bind(event.event_id)
                .bind(uuid_value(payload, "knowledgeBaseId")?)
                .bind(optional_uuid_value(payload, "sourceId")?)
                .bind(job_type).bind(event.event_id.to_string()).bind(payload)
                .execute(&mut **tx).await?;
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
            .bind(host_id).bind(agent_id).bind(knowledge_base_id).bind(&environment)
            .bind(profile_id).bind(payload.get("priority").and_then(|value| value.as_i64()))
            .bind(payload.get("evidenceRequired").and_then(|value| value.as_bool()))
            .bind(payload.get("allowedSourceTrustTiers"))
            .bind(event.aggregate_sequence).execute(&mut **tx).await?;
        sqlx::query("INSERT INTO knowledge_runtime_authorization_t(knowledge_base_id,consumer_host_id,environment,agent_id,retrieval_profile_id,active,desired_event_sequence,applied_event_sequence,projector_id,lease_expires_ts,authorization_digest) VALUES($1,$2,$3,$4,$5,TRUE,$6,$6,$7,now()+interval '30 seconds',$8) ON CONFLICT(knowledge_base_id,consumer_host_id,environment,agent_id) DO UPDATE SET retrieval_profile_id=EXCLUDED.retrieval_profile_id,active=TRUE,desired_event_sequence=EXCLUDED.desired_event_sequence,applied_event_sequence=EXCLUDED.applied_event_sequence,projector_id=EXCLUDED.projector_id,lease_expires_ts=EXCLUDED.lease_expires_ts,authorization_digest=EXCLUDED.authorization_digest,update_ts=now() WHERE knowledge_runtime_authorization_t.applied_event_sequence<EXCLUDED.applied_event_sequence")
            .bind(knowledge_base_id).bind(host_id).bind(&environment).bind(agent_id)
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
            .bind(host_id).bind(agent_id).bind(knowledge_base_id).bind(&environment)
            .bind(event.aggregate_sequence).execute(&mut **tx).await?;
        sqlx::query("UPDATE knowledge_runtime_authorization_t SET active=FALSE,desired_event_sequence=$5,applied_event_sequence=$5,lease_expires_ts=now()+interval '30 seconds',authorization_digest=$6,update_ts=now() WHERE knowledge_base_id=$1 AND consumer_host_id=$2 AND environment=$3 AND agent_id=$4 AND applied_event_sequence<$5")
            .bind(knowledge_base_id).bind(host_id).bind(&environment).bind(agent_id)
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
