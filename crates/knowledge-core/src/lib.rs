//! Deterministic Phase 1a Knowledge Base ingestion and retrieval contracts.
//!
//! Phase 1a intentionally builds one complete immutable BASE generation. It
//! does not implement deltas, uploads, context expansion, or multi-KB fusion.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

pub const FAKE_SPACE_ID: &str = "light-knowledge-fake-v1";
pub const FAKE_SPACE_REVISION: u64 = 1;
pub const FAKE_DIMENSION: usize = 32;
pub const RRF_K: f64 = 60.0;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum KnowledgeError {
    #[error("KNOWLEDGE_BASE_SELECTION_LIMIT_EXCEEDED")]
    MultipleKnowledgeBases,
    #[error("KNOWLEDGE_RUNTIME_AUTHORIZATION_STALE")]
    StaleAuthorization,
    #[error("KNOWLEDGE_RUNTIME_AUTHORIZATION_DENIED")]
    AuthorizationDenied,
    #[error("KNOWLEDGE_PROJECTION_SEQUENCE_GAP: expected {expected}, received {received}")]
    ProjectionGap { expected: u64, received: u64 },
    #[error("KNOWLEDGE_PROJECTION_EVENT_CONFLICT")]
    ProjectionConflict,
    #[error("KNOWLEDGE_SOURCE_LIMIT_EXCEEDED: {0}")]
    SourceLimit(&'static str),
    #[error("KNOWLEDGE_SOURCE_INVALID: {0}")]
    InvalidSource(String),
    #[error("KNOWLEDGE_QUOTA_EXHAUSTED")]
    QuotaExhausted,
    #[error("KNOWLEDGE_GENERATION_NOT_FULL_BASE")]
    NotFullBase,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessingContract {
    pub parser_digest: String,
    pub chunker_digest: String,
    pub lexical_digest: String,
    pub citation_digest: String,
    pub target_tokens: usize,
    pub overlap_tokens: usize,
}

impl Default for ProcessingContract {
    fn default() -> Self {
        Self {
            parser_digest: sha256_hex(b"light-knowledge-markdown-parser-v1"),
            chunker_digest: sha256_hex(b"light-knowledge-heading-chunker-v1:450:50"),
            lexical_digest: sha256_hex(b"light-knowledge-lexical-v1:english+identifier"),
            citation_digest: sha256_hex(b"light-knowledge-citation-v1"),
            target_tokens: 450,
            overlap_tokens: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceLimits {
    pub maximum_documents: usize,
    pub maximum_source_bytes: u64,
    pub maximum_chunks: usize,
    pub maximum_embedding_tokens: usize,
}

impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            maximum_documents: 10_000,
            maximum_source_bytes: 256 * 1024 * 1024,
            maximum_chunks: 100_000,
            maximum_embedding_tokens: 20_000_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DocumentInput {
    pub source_object_id: String,
    pub canonical_uri: String,
    pub source_version: String,
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Chunk {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub document_version_id: Uuid,
    pub source_object_id: String,
    pub canonical_uri: String,
    pub source_version: String,
    pub ordinal: usize,
    pub section_path: Vec<String>,
    pub start_offset: usize,
    pub end_offset: usize,
    pub text: String,
    pub token_count: usize,
    pub content_digest: String,
    pub vector: Vec<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lexical_rank: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_rank: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BaseManifest {
    pub generation_id: Uuid,
    pub segment_id: Uuid,
    pub knowledge_base_id: Uuid,
    pub snapshot_watermark: u64,
    pub document_count: usize,
    pub chunk_count: usize,
    pub vector_count: usize,
    pub parser_digest: String,
    pub chunker_digest: String,
    pub lexical_digest: String,
    pub citation_digest: String,
    pub space_id: String,
    pub space_revision: u64,
    pub dimension: usize,
    pub manifest_digest: String,
    pub segment_kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FullBaseGeneration {
    pub manifest: BaseManifest,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Citation {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    pub document_version_id: Uuid,
    pub canonical_uri: String,
    pub source_version: String,
    pub section_path: Vec<String>,
    pub start_offset: usize,
    pub end_offset: usize,
    pub content_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrievalHit {
    pub chunk_id: Uuid,
    pub text: String,
    pub fused_score: f64,
    pub lexical_rank: Option<usize>,
    pub vector_rank: Option<usize>,
    pub citation: Citation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrievalResponse {
    pub knowledge_base_id: Uuid,
    pub generation_id: Uuid,
    pub strategy: String,
    pub no_answer: bool,
    pub results: Vec<RetrievalHit>,
}

#[derive(Debug, Clone)]
pub struct AuthorizationSnapshot {
    pub knowledge_base_id: Uuid,
    pub consumer_host_id: Uuid,
    pub environment: String,
    pub active: bool,
    pub desired_event_sequence: u64,
    pub applied_event_sequence: u64,
    pub authorization_lease_expires_at: DateTime<Utc>,
    pub projector_lease_expires_at: DateTime<Utc>,
}

impl AuthorizationSnapshot {
    pub fn validate_fresh_active(&self, now: DateTime<Utc>) -> Result<(), KnowledgeError> {
        if self.desired_event_sequence != self.applied_event_sequence
            || self.authorization_lease_expires_at <= now
            || self.projector_lease_expires_at <= now
        {
            return Err(KnowledgeError::StaleAuthorization);
        }
        if !self.active {
            return Err(KnowledgeError::AuthorizationDenied);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrieveRequest {
    #[serde(default)]
    pub knowledge_base_ids: Vec<Uuid>,
    #[serde(skip)]
    pub environment: String,
    pub query: String,
    #[serde(default)]
    pub top_k: usize,
    #[serde(skip)]
    pub token_budget: usize,
    #[serde(default)]
    pub filters: Option<RetrievalFilters>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetrievalFilters {
    #[serde(default)]
    pub source_ids: Vec<Uuid>,
    #[serde(default)]
    pub languages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectionEvent {
    pub event_id: Uuid,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub aggregate_sequence: u64,
    pub payload_digest: String,
}

#[derive(Debug, Default)]
pub struct OrderedProjection {
    applied: BTreeMap<(String, String), (u64, BTreeMap<u64, String>)>,
}

impl OrderedProjection {
    pub fn apply(&mut self, event: &ProjectionEvent) -> Result<bool, KnowledgeError> {
        let key = (event.aggregate_type.clone(), event.aggregate_id.clone());
        let entry = self
            .applied
            .entry(key)
            .or_insert_with(|| (0, BTreeMap::new()));
        if let Some(digest) = entry.1.get(&event.aggregate_sequence) {
            return if digest == &event.payload_digest {
                Ok(false)
            } else {
                Err(KnowledgeError::ProjectionConflict)
            };
        }
        let expected = entry.0 + 1;
        if event.aggregate_sequence != expected {
            return Err(KnowledgeError::ProjectionGap {
                expected,
                received: event.aggregate_sequence,
            });
        }
        entry
            .1
            .insert(event.aggregate_sequence, event.payload_digest.clone());
        entry.0 = event.aggregate_sequence;
        Ok(true)
    }

    pub fn applied_sequence(&self, aggregate_type: &str, aggregate_id: &str) -> u64 {
        self.applied
            .get(&(aggregate_type.to_string(), aggregate_id.to_string()))
            .map_or(0, |entry| entry.0)
    }
}

#[derive(Debug, Clone)]
pub struct QuotaPolicy {
    pub maximum_concurrency: usize,
    pub requests_per_minute: usize,
}

#[derive(Debug, Default)]
pub struct QuotaLedger {
    active: HashMap<(Uuid, Uuid), BTreeSet<String>>,
    minute: HashMap<(Uuid, Uuid, i64), BTreeSet<String>>,
}

impl QuotaLedger {
    pub fn admit(
        &mut self,
        knowledge_base_id: Uuid,
        consumer_host_id: Uuid,
        request_id: &str,
        now: DateTime<Utc>,
        policy: &QuotaPolicy,
    ) -> Result<bool, KnowledgeError> {
        let key = (knowledge_base_id, consumer_host_id);
        if self
            .active
            .get(&key)
            .is_some_and(|requests| requests.contains(request_id))
        {
            return Ok(false);
        }
        let minute_key = (knowledge_base_id, consumer_host_id, now.timestamp() / 60);
        let minute_count = self.minute.get(&minute_key).map_or(0, BTreeSet::len);
        let active_count = self.active.get(&key).map_or(0, BTreeSet::len);
        if active_count >= policy.maximum_concurrency || minute_count >= policy.requests_per_minute
        {
            return Err(KnowledgeError::QuotaExhausted);
        }
        self.active
            .entry(key)
            .or_default()
            .insert(request_id.to_string());
        self.minute
            .entry(minute_key)
            .or_default()
            .insert(request_id.to_string());
        Ok(true)
    }

    pub fn complete(&mut self, knowledge_base_id: Uuid, consumer_host_id: Uuid, request_id: &str) {
        if let Some(active) = self.active.get_mut(&(knowledge_base_id, consumer_host_id)) {
            active.remove(request_id);
        }
    }
}

pub fn ingest_markdown_repository(
    root: &Path,
    limits: &SourceLimits,
) -> Result<Vec<DocumentInput>, KnowledgeError> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
    let mut paths = Vec::new();
    collect_markdown_paths(&canonical_root, &canonical_root, &mut paths)?;
    paths.sort();
    if paths.len() > limits.maximum_documents {
        return Err(KnowledgeError::SourceLimit("maximum_documents"));
    }

    let mut total_bytes = 0_u64;
    let mut documents = Vec::with_capacity(paths.len());
    for path in paths {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(KnowledgeError::InvalidSource(format!(
                "source path is not a regular file: {}",
                path.display()
            )));
        }
        total_bytes = total_bytes.saturating_add(metadata.len());
        if total_bytes > limits.maximum_source_bytes {
            return Err(KnowledgeError::SourceLimit("maximum_source_bytes"));
        }
        let bytes =
            fs::read(&path).map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
        let markdown = String::from_utf8(bytes)
            .map_err(|_| KnowledgeError::InvalidSource("markdown must be UTF-8".into()))?;
        let relative = path
            .strip_prefix(&canonical_root)
            .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
        let source_object_id = normalized_path(relative)?;
        documents.push(DocumentInput {
            canonical_uri: format!("repo://{source_object_id}"),
            source_version: sha256_hex(markdown.as_bytes()),
            source_object_id,
            markdown,
        });
    }
    Ok(documents)
}

fn collect_markdown_paths(
    root: &Path,
    directory: &Path,
    output: &mut Vec<PathBuf>,
) -> Result<(), KnowledgeError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| KnowledgeError::InvalidSource(error.to_string()))?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_markdown_paths(root, &path, output)?;
        } else if metadata.is_file()
            && path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.eq_ignore_ascii_case("md"))
        {
            if !path.starts_with(root) {
                return Err(KnowledgeError::InvalidSource(
                    "path escaped repository root".into(),
                ));
            }
            output.push(path);
        }
    }
    Ok(())
}

fn normalized_path(path: &Path) -> Result<String, KnowledgeError> {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| KnowledgeError::InvalidSource("source path must be UTF-8".into()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

pub fn build_full_base(
    knowledge_base_id: Uuid,
    snapshot_watermark: u64,
    documents: &[DocumentInput],
    contract: &ProcessingContract,
    limits: &SourceLimits,
) -> Result<FullBaseGeneration, KnowledgeError> {
    if documents.len() > limits.maximum_documents {
        return Err(KnowledgeError::SourceLimit("maximum_documents"));
    }
    let mut chunks = Vec::new();
    let mut embedding_tokens = 0_usize;
    let mut sorted = documents.to_vec();
    sorted.sort_by(|left, right| left.source_object_id.cmp(&right.source_object_id));
    for document in sorted {
        let document_id = stable_uuid(&[
            knowledge_base_id.as_bytes(),
            document.source_object_id.as_bytes(),
        ]);
        let document_version_id = stable_uuid(&[
            document_id.as_bytes(),
            document.source_version.as_bytes(),
            contract.parser_digest.as_bytes(),
        ]);
        for draft in chunk_markdown(&document.markdown, contract) {
            embedding_tokens = embedding_tokens.saturating_add(draft.token_count);
            if chunks.len() >= limits.maximum_chunks {
                return Err(KnowledgeError::SourceLimit("maximum_chunks"));
            }
            if embedding_tokens > limits.maximum_embedding_tokens {
                return Err(KnowledgeError::SourceLimit("maximum_embedding_tokens"));
            }
            let content_digest = sha256_hex(draft.text.as_bytes());
            let ordinal = draft.ordinal.to_string();
            let chunk_id = stable_uuid(&[
                document_version_id.as_bytes(),
                ordinal.as_bytes(),
                contract.chunker_digest.as_bytes(),
                content_digest.as_bytes(),
            ]);
            chunks.push(Chunk {
                chunk_id,
                document_id,
                document_version_id,
                source_object_id: document.source_object_id.clone(),
                canonical_uri: document.canonical_uri.clone(),
                source_version: document.source_version.clone(),
                ordinal: draft.ordinal,
                section_path: draft.section_path,
                start_offset: draft.start_offset,
                end_offset: draft.end_offset,
                token_count: draft.token_count,
                vector: fake_embedding(&draft.text),
                lexical_rank: None,
                vector_rank: None,
                content_digest,
                text: draft.text,
            });
        }
    }
    let generation_seed =
        canonical_generation_seed(knowledge_base_id, snapshot_watermark, &chunks, contract);
    let generation_id = stable_uuid(&[b"generation", generation_seed.as_bytes()]);
    let segment_id = stable_uuid(&[b"base", generation_id.as_bytes()]);
    let manifest_digest = sha256_hex(generation_seed.as_bytes());
    Ok(FullBaseGeneration {
        manifest: BaseManifest {
            generation_id,
            segment_id,
            knowledge_base_id,
            snapshot_watermark,
            document_count: documents.len(),
            chunk_count: chunks.len(),
            vector_count: chunks.len(),
            parser_digest: contract.parser_digest.clone(),
            chunker_digest: contract.chunker_digest.clone(),
            lexical_digest: contract.lexical_digest.clone(),
            citation_digest: contract.citation_digest.clone(),
            space_id: FAKE_SPACE_ID.to_string(),
            space_revision: FAKE_SPACE_REVISION,
            dimension: FAKE_DIMENSION,
            manifest_digest,
            segment_kind: "BASE".to_string(),
        },
        chunks,
    })
}

fn canonical_generation_seed(
    knowledge_base_id: Uuid,
    snapshot_watermark: u64,
    chunks: &[Chunk],
    contract: &ProcessingContract,
) -> String {
    let identities = chunks
        .iter()
        .map(|chunk| format!("{}:{}", chunk.chunk_id, chunk.content_digest))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{knowledge_base_id}\n{snapshot_watermark}\n{}\n{}\n{}\n{}\n{identities}",
        contract.parser_digest,
        contract.chunker_digest,
        contract.lexical_digest,
        contract.citation_digest,
    )
}

#[derive(Debug)]
struct ChunkDraft {
    ordinal: usize,
    section_path: Vec<String>,
    start_offset: usize,
    end_offset: usize,
    text: String,
    token_count: usize,
}

fn chunk_markdown(markdown: &str, contract: &ProcessingContract) -> Vec<ChunkDraft> {
    let mut sections: Vec<(Vec<String>, usize, String)> = Vec::new();
    let mut heading_stack = Vec::<String>::new();
    let mut section_start = 0_usize;
    let mut section_text = String::new();
    let mut offset = 0_usize;
    for line in markdown.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let hashes = trimmed.bytes().take_while(|byte| *byte == b'#').count();
        let is_heading = (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ');
        if is_heading {
            if !section_text.trim().is_empty() {
                sections.push((heading_stack.clone(), section_start, section_text.clone()));
                section_text.clear();
            }
            let title = trimmed[hashes + 1..].trim().to_string();
            heading_stack.truncate(hashes.saturating_sub(1));
            heading_stack.push(title);
            section_start = offset;
        }
        section_text.push_str(line);
        offset += line.len();
    }
    if !section_text.trim().is_empty() {
        sections.push((heading_stack, section_start, section_text));
    }
    if sections.is_empty() && !markdown.trim().is_empty() {
        sections.push((Vec::new(), 0, markdown.to_string()));
    }

    let mut output = Vec::new();
    for (section_path, section_start, text) in sections {
        let spans = word_spans(&text);
        if spans.is_empty() {
            continue;
        }
        let step = contract
            .target_tokens
            .saturating_sub(contract.overlap_tokens)
            .max(1);
        let mut start_word = 0_usize;
        while start_word < spans.len() {
            let end_word = (start_word + contract.target_tokens).min(spans.len());
            let local_start = spans[start_word].0;
            let local_end = spans[end_word - 1].1;
            let chunk_text = text[local_start..local_end].trim().to_string();
            output.push(ChunkDraft {
                ordinal: output.len(),
                section_path: section_path.clone(),
                start_offset: section_start + local_start,
                end_offset: section_start + local_end,
                token_count: end_word - start_word,
                text: chunk_text,
            });
            if end_word == spans.len() {
                break;
            }
            start_word += step;
        }
    }
    output
}

fn word_spans(text: &str) -> Vec<(usize, usize)> {
    let mut output = Vec::new();
    let mut start = None;
    for (index, character) in text.char_indices() {
        if character.is_whitespace() {
            if let Some(begin) = start.take() {
                output.push((begin, index));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        output.push((begin, text.len()));
    }
    output
}

pub fn retrieve(
    generation: &FullBaseGeneration,
    authorization: &AuthorizationSnapshot,
    request: &RetrieveRequest,
    now: DateTime<Utc>,
) -> Result<RetrievalResponse, KnowledgeError> {
    if request.knowledge_base_ids.len() != 1 {
        return Err(KnowledgeError::MultipleKnowledgeBases);
    }
    if generation.manifest.segment_kind != "BASE" {
        return Err(KnowledgeError::NotFullBase);
    }
    let knowledge_base_id = request.knowledge_base_ids[0];
    authorization.validate_fresh_active(now)?;
    if knowledge_base_id != generation.manifest.knowledge_base_id {
        return Err(KnowledgeError::AuthorizationDenied);
    }

    let ranked_by_store = generation
        .chunks
        .iter()
        .any(|chunk| chunk.lexical_rank.is_some() || chunk.vector_rank.is_some());
    let (lexical_ranks, vector_ranks) = if ranked_by_store {
        (
            generation
                .chunks
                .iter()
                .filter_map(|chunk| chunk.lexical_rank.map(|rank| (chunk.chunk_id, rank)))
                .collect::<HashMap<_, _>>(),
            generation
                .chunks
                .iter()
                .filter_map(|chunk| chunk.vector_rank.map(|rank| (chunk.chunk_id, rank)))
                .collect::<HashMap<_, _>>(),
        )
    } else {
        let query_terms = lexical_terms(&request.query);
        let query_vector = fake_embedding(&request.query);
        let candidate_limit = request.top_k.max(1).saturating_mul(4);
        let mut lexical = generation
            .chunks
            .iter()
            .map(|chunk| (chunk.chunk_id, lexical_score(&query_terms, &chunk.text)))
            .filter(|(_, score)| *score > 0.0)
            .collect::<Vec<_>>();
        lexical.sort_by(score_then_id);
        lexical.truncate(candidate_limit);
        let mut vector = generation
            .chunks
            .iter()
            .map(|chunk| (chunk.chunk_id, cosine(&query_vector, &chunk.vector)))
            .collect::<Vec<_>>();
        vector.sort_by(score_then_id);
        vector.truncate(candidate_limit);
        (
            lexical
                .iter()
                .enumerate()
                .map(|(index, (id, _))| (*id, index + 1))
                .collect::<HashMap<_, _>>(),
            vector
                .iter()
                .enumerate()
                .map(|(index, (id, _))| (*id, index + 1))
                .collect::<HashMap<_, _>>(),
        )
    };
    let mut fused = generation
        .chunks
        .iter()
        .filter_map(|chunk| {
            let lexical_rank = lexical_ranks.get(&chunk.chunk_id).copied();
            let vector_rank = vector_ranks.get(&chunk.chunk_id).copied();
            if lexical_rank.is_none() && vector_rank.is_none() {
                return None;
            }
            let score = lexical_rank.map_or(0.0, |rank| 1.0 / (RRF_K + rank as f64))
                + vector_rank.map_or(0.0, |rank| 1.0 / (RRF_K + rank as f64));
            Some((chunk, lexical_rank, vector_rank, score))
        })
        .collect::<Vec<_>>();
    fused.sort_by(|left, right| {
        right
            .3
            .partial_cmp(&left.3)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.chunk_id.cmp(&right.0.chunk_id))
    });

    let mut consumed_tokens = 0_usize;
    let mut results = Vec::new();
    for (chunk, lexical_rank, vector_rank, fused_score) in fused {
        if results.len() >= request.top_k.max(1) {
            break;
        }
        if consumed_tokens.saturating_add(chunk.token_count) > request.token_budget {
            continue;
        }
        consumed_tokens += chunk.token_count;
        results.push(RetrievalHit {
            chunk_id: chunk.chunk_id,
            text: chunk.text.clone(),
            fused_score,
            lexical_rank,
            vector_rank,
            citation: Citation {
                chunk_id: chunk.chunk_id,
                document_id: chunk.document_id,
                document_version_id: chunk.document_version_id,
                canonical_uri: chunk.canonical_uri.clone(),
                source_version: chunk.source_version.clone(),
                section_path: chunk.section_path.clone(),
                start_offset: chunk.start_offset,
                end_offset: chunk.end_offset,
                content_digest: chunk.content_digest.clone(),
            },
        });
    }
    // Vector similarity alone is not sufficient evidence for an unrelated query
    // in the deterministic pilot. At least one lexical candidate is required.
    if lexical_ranks.is_empty() {
        results.clear();
    }
    Ok(RetrievalResponse {
        knowledge_base_id,
        generation_id: generation.manifest.generation_id,
        strategy: "HYBRID_RRF".to_string(),
        no_answer: results.is_empty(),
        results,
    })
}

fn lexical_terms(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !(character.is_alphanumeric() || "_-./".contains(character)))
        .filter(|term| !term.is_empty())
        .map(|term| term.to_lowercase())
        .collect()
}

fn lexical_score(query_terms: &BTreeSet<String>, text: &str) -> f64 {
    let lowered = text.to_lowercase();
    query_terms
        .iter()
        .map(|term| {
            if lowered.contains(term) {
                if term.contains(['_', '-', '.', '/']) {
                    3.0
                } else {
                    1.0
                }
            } else {
                0.0
            }
        })
        .sum()
}

fn score_then_id(left: &(Uuid, f64), right: &(Uuid, f64)) -> Ordering {
    right
        .1
        .partial_cmp(&left.1)
        .unwrap_or(Ordering::Equal)
        .then_with(|| left.0.cmp(&right.0))
}

pub fn fake_embedding(text: &str) -> Vec<f32> {
    let mut vector = Vec::with_capacity(FAKE_DIMENSION);
    for index in 0..FAKE_DIMENSION {
        let mut hasher = Sha256::new();
        hasher.update(b"light-knowledge-fake-embedding-v1\0");
        hasher.update((index as u32).to_be_bytes());
        hasher.update(text.as_bytes());
        let digest = hasher.finalize();
        let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
        vector.push((value as f64 / u32::MAX as f64 * 2.0 - 1.0) as f32);
    }
    let norm = vector
        .iter()
        .map(|value| f64::from(*value) * f64::from(*value))
        .sum::<f64>()
        .sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value = (f64::from(*value) / norm) as f32;
        }
    }
    vector
}

fn cosine(left: &[f32], right: &[f32]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum()
}

fn stable_uuid(parts: &[&[u8]]) -> Uuid {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn default_authorization(
    knowledge_base_id: Uuid,
    consumer_host_id: Uuid,
    environment: &str,
) -> AuthorizationSnapshot {
    let now = Utc::now();
    AuthorizationSnapshot {
        knowledge_base_id,
        consumer_host_id,
        environment: environment.to_string(),
        active: true,
        desired_event_sequence: 1,
        applied_event_sequence: 1,
        authorization_lease_expires_at: now + Duration::seconds(30),
        projector_lease_expires_at: now + Duration::seconds(30),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn documents() -> Vec<DocumentInput> {
        vec![
            DocumentInput {
                source_object_id: "config/server.md".into(),
                canonical_uri: "repo://config/server.md".into(),
                source_version: "v1".into(),
                markdown:
                    "# Server\nSet `serviceId` in server.yml. The health endpoint is /health."
                        .into(),
            },
            DocumentInput {
                source_object_id: "security/auth.md".into(),
                canonical_uri: "repo://security/auth.md".into(),
                source_version: "v1".into(),
                markdown: "# Authorization\nRuntime authorization expires after thirty seconds."
                    .into(),
            },
        ]
    }

    #[test]
    fn full_base_is_byte_deterministic_and_has_exact_citations() {
        let knowledge_base_id = Uuid::from_u128(1);
        let first = build_full_base(
            knowledge_base_id,
            7,
            &documents(),
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        let second = build_full_base(
            knowledge_base_id,
            7,
            &documents().into_iter().rev().collect::<Vec<_>>(),
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(first.manifest.segment_kind, "BASE");
        assert_eq!(first.manifest.vector_count, first.manifest.chunk_count);
        assert!(first.chunks.iter().all(|chunk| chunk.vector.len() == 32));
    }

    #[test]
    fn ordered_projection_is_idempotent_and_detects_gaps_and_conflicts() {
        let mut projection = OrderedProjection::default();
        let event = ProjectionEvent {
            event_id: Uuid::now_v7(),
            aggregate_type: "KnowledgeBase".into(),
            aggregate_id: "global|dev|1".into(),
            aggregate_sequence: 1,
            payload_digest: "a".repeat(64),
        };
        assert!(projection.apply(&event).unwrap());
        assert!(!projection.apply(&event).unwrap());
        let mut conflict = event.clone();
        conflict.payload_digest = "b".repeat(64);
        assert_eq!(
            projection.apply(&conflict),
            Err(KnowledgeError::ProjectionConflict)
        );
        let mut gap = event;
        gap.aggregate_sequence = 3;
        assert_eq!(
            projection.apply(&gap),
            Err(KnowledgeError::ProjectionGap {
                expected: 2,
                received: 3
            })
        );
    }

    #[test]
    fn retrieval_enforces_one_kb_stale_lease_scope_and_no_answer() {
        let knowledge_base_id = Uuid::from_u128(1);
        let consumer_host_id = Uuid::from_u128(2);
        let generation = build_full_base(
            knowledge_base_id,
            1,
            &documents(),
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        let now = Utc::now();
        let mut authorization = default_authorization(knowledge_base_id, consumer_host_id, "dev");
        authorization.authorization_lease_expires_at = now + Duration::seconds(10);
        authorization.projector_lease_expires_at = now + Duration::seconds(10);
        let request = RetrieveRequest {
            knowledge_base_ids: vec![knowledge_base_id],
            environment: "dev".into(),
            query: "serviceId".into(),
            top_k: 10,
            token_budget: 1000,
            filters: None,
        };
        let response = retrieve(&generation, &authorization, &request, now).unwrap();
        assert!(!response.no_answer);
        assert_eq!(
            response.results[0].citation.canonical_uri,
            "repo://config/server.md"
        );

        let mut multiple = request.clone();
        multiple.knowledge_base_ids.push(Uuid::from_u128(3));
        assert_eq!(
            retrieve(&generation, &authorization, &multiple, now),
            Err(KnowledgeError::MultipleKnowledgeBases)
        );
        authorization.authorization_lease_expires_at = now;
        assert_eq!(
            retrieve(&generation, &authorization, &request, now),
            Err(KnowledgeError::StaleAuthorization)
        );
        authorization.authorization_lease_expires_at = now + Duration::seconds(10);
        let mut unrelated = request;
        unrelated.query = "unfindable_xyzzy_token".into();
        assert!(
            retrieve(&generation, &authorization, &unrelated, now)
                .unwrap()
                .no_answer
        );
    }

    #[test]
    fn store_ranked_stem_match_is_not_discarded_by_substring_scoring() {
        let knowledge_base_id = Uuid::from_u128(1);
        let consumer_host_id = Uuid::from_u128(2);
        let mut generation = build_full_base(
            knowledge_base_id,
            1,
            &documents(),
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        let chunk = generation
            .chunks
            .iter_mut()
            .find(|chunk| chunk.text.contains("Authorization"))
            .unwrap();
        assert!(!chunk.text.to_lowercase().contains("authorize"));
        chunk.lexical_rank = Some(1);
        chunk.vector_rank = Some(1);
        let now = Utc::now();
        let authorization = default_authorization(knowledge_base_id, consumer_host_id, "dev");
        let request = RetrieveRequest {
            knowledge_base_ids: vec![knowledge_base_id],
            environment: "dev".into(),
            query: "authorize".into(),
            top_k: 10,
            token_budget: 1000,
            filters: None,
        };
        let response = retrieve(&generation, &authorization, &request, now).unwrap();
        assert!(!response.no_answer);
        assert_eq!(response.results[0].lexical_rank, Some(1));
    }

    #[test]
    fn repository_reader_is_sorted_bounded_and_ignores_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir(directory.path().join("b")).unwrap();
        fs::write(directory.path().join("b/z.md"), "# Z\nlast").unwrap();
        fs::write(directory.path().join("a.md"), "# A\nfirst").unwrap();
        fs::write(directory.path().join("ignored.txt"), "ignored").unwrap();
        let docs = ingest_markdown_repository(directory.path(), &SourceLimits::default()).unwrap();
        assert_eq!(
            docs.iter()
                .map(|doc| doc.source_object_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a.md", "b/z.md"]
        );
        let mut limits = SourceLimits::default();
        limits.maximum_documents = 1;
        assert_eq!(
            ingest_markdown_repository(directory.path(), &limits),
            Err(KnowledgeError::SourceLimit("maximum_documents"))
        );
    }

    #[test]
    fn quota_admission_is_idempotent_and_bounded() {
        let mut ledger = QuotaLedger::default();
        let kb = Uuid::from_u128(1);
        let host = Uuid::from_u128(2);
        let now = Utc::now();
        let policy = QuotaPolicy {
            maximum_concurrency: 1,
            requests_per_minute: 2,
        };
        assert!(ledger.admit(kb, host, "r1", now, &policy).unwrap());
        assert!(!ledger.admit(kb, host, "r1", now, &policy).unwrap());
        assert_eq!(
            ledger.admit(kb, host, "r2", now, &policy),
            Err(KnowledgeError::QuotaExhausted)
        );
        ledger.complete(kb, host, "r1");
        assert!(ledger.admit(kb, host, "r2", now, &policy).unwrap());
    }
}
