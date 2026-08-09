use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hmac::{Hmac, Mac};
use knowledge_core::{
    FullBaseGeneration, ProcessingContract, SourceLimits, build_full_base,
    ingest_markdown_repository, sha256_hex,
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
}

impl WorkerConfig {
    fn load() -> Result<Self> {
        let path = env::var("LIGHT_KNOWLEDGE_WORKER_CONFIG_FILE")
            .unwrap_or_else(|_| "config/worker.yml".to_string());
        let content =
            fs::read_to_string(&path).with_context(|| format!("read worker config {path}"))?;
        let config: Self = serde_yaml::from_str(&content)
            .with_context(|| format!("parse worker config {path}"))?;
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
    loop {
        let mut tx = pool.begin().await?;
        let job = sqlx::query(
            "SELECT job_id,knowledge_base_id,source_id,job_type,payload
               FROM knowledge_job_t
              WHERE state='QUEUED' AND job_type IN (
                'SYNC','FULL_REINDEX','PROMOTE','CONNECTIVITY_TEST')
                AND (next_attempt_ts IS NULL OR next_attempt_ts<=now())
              ORDER BY created_ts FOR UPDATE SKIP LOCKED LIMIT 1",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some(job) = job else {
            tx.rollback().await?;
            publish_promotion_acknowledgements(pool, config).await?;
            tokio::time::sleep(Duration::from_secs(2)).await;
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
        } else if job_type == "CONNECTIVITY_TEST" {
            prepare_checkout(config).await.map(|_| ())
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
                sqlx::query("UPDATE knowledge_job_t SET state='FAILED',result=jsonb_build_object('code','KNOWLEDGE_BUILD_FAILED'),lease_expires_ts=NULL,update_ts=now() WHERE job_id=$1")
                    .bind(job_id).execute(pool).await?;
            }
        }
    }
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
    .fetch_one(pool)
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
        let artifact_id = derived_uuid("embedding", chunk.chunk_id);
        sqlx::query(
            "INSERT INTO knowledge_document_t(
               document_id,knowledge_base_id,source_id,source_object_id,
               canonical_uri,lifecycle_state,observed_ts
             ) VALUES($1,$2,$3,$4,$5,'ACTIVE',now())
             ON CONFLICT(document_id) DO NOTHING",
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
             ) VALUES($1,$2,$3,$4,$5,$6,FALSE) ON CONFLICT DO NOTHING",
        )
        .bind(chunk.chunk_id)
        .bind(artifact_id)
        .bind(config.knowledge_base_id)
        .bind(config.embedding_profile_id)
        .bind(config.embedding_profile_revision)
        .bind(format!("sync:{sync_run_id}:{}", chunk.chunk_id))
        .execute(&mut *tx)
        .await?;
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

fn as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn as_i32(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

fn derived_uuid(namespace: &str, id: Uuid) -> Uuid {
    let digest = sha256_hex(format!("{namespace}:{id}").as_bytes());
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[index * 2..index * 2 + 2], 16).unwrap_or_default();
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
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
        | "KnowledgeSourceConnectivityTestRequestedEvent"
        | "KnowledgeBaseReindexRequestedEvent"
        | "KnowledgeBaseIndexGenerationPromotionRequestedEvent"
        | "KnowledgeBasePurgeRequestedEvent"
        | "KnowledgeBaseRetrievalTestRequestedEvent" => {
            let job_type = match event.event_type.as_str() {
                "KnowledgeSourceSyncRequestedEvent" => "SYNC",
                "KnowledgeSourceConnectivityTestRequestedEvent" => "CONNECTIVITY_TEST",
                "KnowledgeBaseReindexRequestedEvent" => "FULL_REINDEX",
                "KnowledgeBaseIndexGenerationPromotionRequestedEvent" => "PROMOTE",
                "KnowledgeBaseRetrievalTestRequestedEvent" => "RETRIEVAL_TEST",
                _ => "PURGE",
            };
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
}
