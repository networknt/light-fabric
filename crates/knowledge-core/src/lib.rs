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
    #[error("KNOWLEDGE_MIGRATION_STATE_CONFLICT")]
    MigrationStateConflict,
    #[error("KNOWLEDGE_MIGRATION_COST_CEILING_EXCEEDED")]
    MigrationCostCeilingExceeded,
    #[error("KNOWLEDGE_MIGRATION_BACKFILL_INCOMPLETE")]
    MigrationBackfillIncomplete,
    #[error("KNOWLEDGE_MIGRATION_FINAL_FENCE_FAILED")]
    MigrationFinalFenceFailed,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AclMode {
    UniformScope,
    MirrorSourceAcl,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AclEffect {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum AclSubjectType {
    User,
    Group,
    Organization,
    Everyone,
    Unresolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AclSubject {
    pub provider_subject_id: String,
    pub subject_type: AclSubjectType,
    pub subject_id: String,
    pub effect: AclEffect,
    pub mapping_complete: bool,
    pub provider_evidence_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NormalizedAclRevision {
    pub mode: AclMode,
    pub complete: bool,
    pub observed_at: DateTime<Utc>,
    pub fresh_until: DateTime<Utc>,
    pub provider_effective_decision: bool,
    pub subjects: Vec<AclSubject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrincipalContext {
    pub subject_id: String,
    pub subject_type: String,
    pub groups: BTreeSet<String>,
    pub organizations: BTreeSet<String>,
}

pub fn authorize_document_acl(
    acl: &NormalizedAclRevision,
    principal: &PrincipalContext,
    now: DateTime<Utc>,
) -> bool {
    if acl.mode == AclMode::UniformScope {
        return true;
    }
    if !acl.complete
        || !acl.provider_effective_decision
        || acl.observed_at > now
        || acl.fresh_until <= now
        || acl.subjects.iter().any(|subject| !subject.mapping_complete)
    {
        return false;
    }
    let matches = |subject: &AclSubject| match subject.subject_type {
        AclSubjectType::User => {
            principal.subject_type.eq_ignore_ascii_case("user")
                && subject.subject_id == principal.subject_id
        }
        AclSubjectType::Group => principal.groups.contains(&subject.subject_id),
        AclSubjectType::Organization => principal.organizations.contains(&subject.subject_id),
        AclSubjectType::Everyone => subject.subject_id == "*",
        AclSubjectType::Unresolved => false,
    };
    if acl
        .subjects
        .iter()
        .any(|subject| subject.effect == AclEffect::Deny && matches(subject))
    {
        return false;
    }
    acl.subjects
        .iter()
        .any(|subject| subject.effect == AclEffect::Allow && matches(subject))
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
        if !is_indexable_markdown(&markdown) {
            continue;
        }
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
    let document_count = chunks
        .iter()
        .map(|chunk| chunk.document_id)
        .collect::<BTreeSet<_>>()
        .len();
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
            document_count,
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
    retrieve_with_lexical_gate(generation, authorization, request, now, true)
}

pub fn retrieve_with_lexical_gate(
    generation: &FullBaseGeneration,
    authorization: &AuthorizationSnapshot,
    request: &RetrieveRequest,
    now: DateTime<Utc>,
    lexical_evidence_required: bool,
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
    if lexical_evidence_required && lexical_ranks.is_empty() {
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChangeKind {
    Add,
    Modify,
    Delete,
    AclOnly,
    MetadataOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CorpusDocumentState {
    pub source_object_id: String,
    pub canonical_uri: String,
    pub source_version: String,
    pub content_digest: String,
    pub metadata_digest: String,
    pub acl_digest: String,
    pub markdown: String,
}

impl From<DocumentInput> for CorpusDocumentState {
    fn from(input: DocumentInput) -> Self {
        Self {
            source_object_id: input.source_object_id,
            canonical_uri: input.canonical_uri,
            source_version: input.source_version,
            content_digest: sha256_hex(input.markdown.as_bytes()),
            metadata_digest: sha256_hex(b"{}"),
            acl_digest: sha256_hex(b"UNIFORM_SCOPE"),
            markdown: input.markdown,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClassifiedChange {
    pub operation_id: Uuid,
    pub source_object_id: String,
    pub kind: ChangeKind,
    pub previous_source_version: Option<String>,
    pub selected_source_version: Option<String>,
    pub change_digest: String,
}

pub fn classify_corpus_changes(
    knowledge_base_id: Uuid,
    previous: &[CorpusDocumentState],
    current: &[CorpusDocumentState],
) -> Vec<ClassifiedChange> {
    let before = previous
        .iter()
        .filter(|document| is_indexable_markdown(&document.markdown))
        .map(|document| (document.source_object_id.as_str(), document))
        .collect::<BTreeMap<_, _>>();
    let after = current
        .iter()
        .filter(|document| is_indexable_markdown(&document.markdown))
        .map(|document| (document.source_object_id.as_str(), document))
        .collect::<BTreeMap<_, _>>();
    let identities = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    identities
        .into_iter()
        .filter_map(|source_object_id| {
            let previous = before.get(source_object_id).copied();
            let selected = after.get(source_object_id).copied();
            let kind = match (previous, selected) {
                (None, Some(_)) => ChangeKind::Add,
                (Some(_), None) => ChangeKind::Delete,
                (Some(left), Some(right)) if left.content_digest != right.content_digest => {
                    ChangeKind::Modify
                }
                (Some(left), Some(right)) if left.acl_digest != right.acl_digest => {
                    ChangeKind::AclOnly
                }
                (Some(left), Some(right)) if left.metadata_digest != right.metadata_digest => {
                    ChangeKind::MetadataOnly
                }
                _ => return None,
            };
            let seed = format!(
                "{}\n{:?}\n{}\n{}\n{}\n{}",
                source_object_id,
                kind,
                previous.map_or("", |document| document.source_version.as_str()),
                selected.map_or("", |document| document.source_version.as_str()),
                selected.map_or("", |document| document.metadata_digest.as_str()),
                selected.map_or("", |document| document.acl_digest.as_str()),
            );
            let change_digest = sha256_hex(seed.as_bytes());
            Some(ClassifiedChange {
                operation_id: stable_uuid(&[
                    b"knowledge-change-v1",
                    knowledge_base_id.as_bytes(),
                    change_digest.as_bytes(),
                ]),
                source_object_id: source_object_id.to_string(),
                kind,
                previous_source_version: previous.map(|value| value.source_version.clone()),
                selected_source_version: selected.map(|value| value.source_version.clone()),
                change_digest,
            })
        })
        .collect()
}

pub fn is_indexable_markdown(markdown: &str) -> bool {
    !markdown.trim().is_empty()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaDocumentOperation {
    pub operation_id: Uuid,
    pub kind: ChangeKind,
    pub document_id: Uuid,
    pub source_object_id: String,
    pub chunks: Vec<Chunk>,
    pub acl_digest: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeltaSegment {
    pub segment_id: Uuid,
    pub knowledge_base_id: Uuid,
    pub snapshot_watermark: u64,
    pub predecessor_manifest_digest: String,
    pub manifest_digest: String,
    pub operations: Vec<DeltaDocumentOperation>,
}

pub fn stable_passage_anchor_id(chunk: &Chunk, citation_contract_digest: &str) -> Uuid {
    let section = chunk.section_path.join("\u{1f}");
    stable_uuid(&[
        b"knowledge-passage-anchor-v1",
        chunk.document_id.as_bytes(),
        citation_contract_digest.as_bytes(),
        section.as_bytes(),
        chunk.content_digest.as_bytes(),
    ])
}

pub fn build_delta_segment(
    knowledge_base_id: Uuid,
    snapshot_watermark: u64,
    predecessor_manifest_digest: &str,
    previous: &[CorpusDocumentState],
    current: &[CorpusDocumentState],
    contract: &ProcessingContract,
    limits: &SourceLimits,
) -> Result<DeltaSegment, KnowledgeError> {
    let changes = classify_corpus_changes(knowledge_base_id, previous, current);
    let selected = current
        .iter()
        .map(|document| (document.source_object_id.as_str(), document))
        .collect::<BTreeMap<_, _>>();
    let mut operations = Vec::with_capacity(changes.len());
    for change in changes {
        let document_id = stable_uuid(&[
            knowledge_base_id.as_bytes(),
            change.source_object_id.as_bytes(),
        ]);
        let (chunks, acl_digest) = match change.kind {
            ChangeKind::Add | ChangeKind::Modify | ChangeKind::MetadataOnly => {
                let document = selected[change.source_object_id.as_str()];
                let generation = build_full_base(
                    knowledge_base_id,
                    snapshot_watermark,
                    &[DocumentInput {
                        source_object_id: document.source_object_id.clone(),
                        canonical_uri: document.canonical_uri.clone(),
                        source_version: document.source_version.clone(),
                        markdown: document.markdown.clone(),
                    }],
                    contract,
                    limits,
                )?;
                (generation.chunks, Some(document.acl_digest.clone()))
            }
            ChangeKind::AclOnly => (
                Vec::new(),
                Some(
                    selected[change.source_object_id.as_str()]
                        .acl_digest
                        .clone(),
                ),
            ),
            ChangeKind::Delete => (Vec::new(), None),
        };
        operations.push(DeltaDocumentOperation {
            operation_id: stable_uuid(&[
                b"knowledge-delta-operation-v1",
                knowledge_base_id.as_bytes(),
                &snapshot_watermark.to_be_bytes(),
                change.source_object_id.as_bytes(),
                change.change_digest.as_bytes(),
            ]),
            kind: change.kind,
            document_id,
            source_object_id: change.source_object_id,
            chunks,
            acl_digest,
        });
    }
    operations.sort_by(|left, right| {
        left.source_object_id
            .cmp(&right.source_object_id)
            .then_with(|| left.operation_id.cmp(&right.operation_id))
    });
    let operation_seed = operations
        .iter()
        .map(|operation| {
            format!(
                "{}:{:?}:{}",
                operation.operation_id,
                operation.kind,
                operation
                    .chunks
                    .iter()
                    .map(|chunk| chunk.content_digest.as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let manifest_seed = format!(
        "{knowledge_base_id}\n{snapshot_watermark}\n{predecessor_manifest_digest}\n{operation_seed}"
    );
    let manifest_digest = sha256_hex(manifest_seed.as_bytes());
    Ok(DeltaSegment {
        segment_id: stable_uuid(&[
            b"knowledge-delta-v1",
            knowledge_base_id.as_bytes(),
            manifest_digest.as_bytes(),
        ]),
        knowledge_base_id,
        snapshot_watermark,
        predecessor_manifest_digest: predecessor_manifest_digest.to_string(),
        manifest_digest,
        operations,
    })
}

pub fn resolve_base_plus_deltas(
    base: &FullBaseGeneration,
    deltas: &[DeltaSegment],
) -> Result<FullBaseGeneration, KnowledgeError> {
    if base.manifest.segment_kind != "BASE" {
        return Err(KnowledgeError::NotFullBase);
    }
    let mut chunks_by_document = base.chunks.iter().cloned().fold(
        BTreeMap::<Uuid, Vec<Chunk>>::new(),
        |mut output, chunk| {
            output.entry(chunk.document_id).or_default().push(chunk);
            output
        },
    );
    let mut ordered = deltas.to_vec();
    ordered.sort_by_key(|delta| (delta.snapshot_watermark, delta.segment_id));
    let mut predecessor = base.manifest.manifest_digest.clone();
    for delta in &ordered {
        if delta.knowledge_base_id != base.manifest.knowledge_base_id
            || delta.predecessor_manifest_digest != predecessor
        {
            return Err(KnowledgeError::InvalidSource(
                "DELTA predecessor or Knowledge Base mismatch".into(),
            ));
        }
        for operation in &delta.operations {
            match operation.kind {
                ChangeKind::Delete => {
                    chunks_by_document.remove(&operation.document_id);
                }
                ChangeKind::Add | ChangeKind::Modify | ChangeKind::MetadataOnly => {
                    chunks_by_document.insert(operation.document_id, operation.chunks.clone());
                }
                ChangeKind::AclOnly => {}
            }
        }
        predecessor = delta.manifest_digest.clone();
    }
    let mut chunks = chunks_by_document
        .into_values()
        .flatten()
        .collect::<Vec<_>>();
    chunks.sort_by_key(|chunk| (chunk.document_id, chunk.ordinal, chunk.chunk_id));
    let snapshot_watermark = ordered
        .last()
        .map_or(base.manifest.snapshot_watermark, |delta| {
            delta.snapshot_watermark
        });
    let manifest_digest = sha256_hex(
        format!(
            "{}\n{}\n{}",
            base.manifest.manifest_digest,
            ordered
                .iter()
                .map(|delta| delta.manifest_digest.as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            chunks
                .iter()
                .map(|chunk| chunk.chunk_id.to_string())
                .collect::<Vec<_>>()
                .join("\n")
        )
        .as_bytes(),
    );
    let generation_id = stable_uuid(&[
        b"knowledge-logical-generation-v1",
        base.manifest.knowledge_base_id.as_bytes(),
        manifest_digest.as_bytes(),
    ]);
    Ok(FullBaseGeneration {
        manifest: BaseManifest {
            generation_id,
            segment_id: ordered
                .last()
                .map_or(base.manifest.segment_id, |d| d.segment_id),
            knowledge_base_id: base.manifest.knowledge_base_id,
            snapshot_watermark,
            document_count: chunks
                .iter()
                .map(|chunk| chunk.document_id)
                .collect::<BTreeSet<_>>()
                .len(),
            chunk_count: chunks.len(),
            vector_count: chunks.len(),
            parser_digest: base.manifest.parser_digest.clone(),
            chunker_digest: base.manifest.chunker_digest.clone(),
            lexical_digest: base.manifest.lexical_digest.clone(),
            citation_digest: base.manifest.citation_digest.clone(),
            space_id: base.manifest.space_id.clone(),
            space_revision: base.manifest.space_revision,
            dimension: base.manifest.dimension,
            manifest_digest,
            segment_kind: "BASE+DELTA".into(),
        },
        chunks,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ScopedEmbeddingReuseKey {
    pub knowledge_base_id: Uuid,
    pub input_digest: String,
    pub space_id: String,
    pub space_revision: u64,
    pub dimension: usize,
    pub transform_digest: String,
}

#[derive(Debug, Default)]
pub struct EmbeddingReuseLedger {
    entries: BTreeMap<ScopedEmbeddingReuseKey, (Uuid, usize)>,
}

impl EmbeddingReuseLedger {
    pub fn acquire(&mut self, key: ScopedEmbeddingReuseKey) -> (Uuid, bool) {
        if let Some((artifact_id, references)) = self.entries.get_mut(&key) {
            *references += 1;
            return (*artifact_id, true);
        }
        let artifact_id = stable_uuid(&[
            b"knowledge-embedding-artifact-v1",
            key.knowledge_base_id.as_bytes(),
            key.input_digest.as_bytes(),
            key.space_id.as_bytes(),
            &key.space_revision.to_be_bytes(),
            key.transform_digest.as_bytes(),
        ]);
        self.entries.insert(key, (artifact_id, 1));
        (artifact_id, false)
    }

    pub fn release(&mut self, key: &ScopedEmbeddingReuseKey) -> bool {
        let Some((_, references)) = self.entries.get_mut(key) else {
            return false;
        };
        *references = references.saturating_sub(1);
        if *references == 0 {
            self.entries.remove(key);
            return true;
        }
        false
    }
}

#[derive(Debug, Clone)]
pub struct KnowledgeBaseRankedResponse {
    pub response: RetrievalResponse,
    pub priority: i32,
    pub embedding_group_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MultiKnowledgeBaseHit {
    pub knowledge_base_id: Uuid,
    pub generation_id: Uuid,
    pub local_rank: usize,
    pub cross_knowledge_base_score: f64,
    pub hit: RetrievalHit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MultiKnowledgeBaseResponse {
    pub status: String,
    pub disposition: String,
    pub knowledge_base_ids: Vec<Uuid>,
    pub embedding_group_count: usize,
    pub warnings: Vec<String>,
    pub exclusions: Vec<String>,
    pub results: Vec<MultiKnowledgeBaseHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum KnowledgeSearchResponse {
    Single(RetrievalResponse),
    Multi(MultiKnowledgeBaseResponse),
}

pub fn fuse_knowledge_base_results(
    mut ranked: Vec<KnowledgeBaseRankedResponse>,
    maximum_knowledge_bases: usize,
    top_k: usize,
    token_budget: usize,
) -> Result<MultiKnowledgeBaseResponse, KnowledgeError> {
    if ranked.is_empty() || ranked.len() > maximum_knowledge_bases.min(4) {
        return Err(KnowledgeError::MultipleKnowledgeBases);
    }
    ranked.sort_by(|left, right| {
        right.priority.cmp(&left.priority).then_with(|| {
            left.response
                .knowledge_base_id
                .cmp(&right.response.knowledge_base_id)
        })
    });
    let knowledge_base_ids = ranked
        .iter()
        .map(|entry| entry.response.knowledge_base_id)
        .collect::<Vec<_>>();
    let embedding_group_count = ranked
        .iter()
        .map(|entry| entry.embedding_group_key.clone())
        .collect::<BTreeSet<_>>()
        .len();
    let mut candidates = ranked
        .iter()
        .flat_map(|entry| {
            entry
                .response
                .results
                .iter()
                .cloned()
                .enumerate()
                .map(move |(index, hit)| MultiKnowledgeBaseHit {
                    knowledge_base_id: entry.response.knowledge_base_id,
                    generation_id: entry.response.generation_id,
                    local_rank: index + 1,
                    cross_knowledge_base_score: 1.0 / (RRF_K + index as f64 + 1.0),
                    hit,
                })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        right
            .cross_knowledge_base_score
            .partial_cmp(&left.cross_knowledge_base_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                let left_priority = ranked
                    .iter()
                    .find(|entry| entry.response.knowledge_base_id == left.knowledge_base_id)
                    .map_or(0, |entry| entry.priority);
                let right_priority = ranked
                    .iter()
                    .find(|entry| entry.response.knowledge_base_id == right.knowledge_base_id)
                    .map_or(0, |entry| entry.priority);
                right_priority.cmp(&left_priority)
            })
            .then_with(|| left.knowledge_base_id.cmp(&right.knowledge_base_id))
            .then_with(|| left.hit.chunk_id.cmp(&right.hit.chunk_id))
    });
    let mut consumed_tokens: usize = 0;
    let mut results = Vec::new();
    let mut selected_chunks = BTreeSet::new();

    // Preserve the Phase 1b fairness contract before filling the remaining
    // budget by cross-KB RRF: every non-empty KB gets one local result when
    // the caller's top-k and token budget permit it.
    for entry in &ranked {
        let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.knowledge_base_id == entry.response.knowledge_base_id)
        else {
            continue;
        };
        if results.len() >= top_k.max(1) {
            break;
        }
        let tokens = candidate.hit.text.split_whitespace().count();
        if consumed_tokens.saturating_add(tokens) <= token_budget {
            consumed_tokens += tokens;
            selected_chunks.insert(candidate.hit.chunk_id);
            results.push(candidate.clone());
        }
    }
    for candidate in candidates {
        if selected_chunks.contains(&candidate.hit.chunk_id) {
            continue;
        }
        let tokens = candidate.hit.text.split_whitespace().count();
        if results.len() >= top_k.max(1) {
            break;
        }
        if consumed_tokens.saturating_add(tokens) > token_budget {
            continue;
        }
        consumed_tokens += tokens;
        results.push(candidate);
    }
    Ok(MultiKnowledgeBaseResponse {
        status: "COMPLETE".into(),
        disposition: if results.is_empty() {
            "NO_QUALIFIED_EVIDENCE".into()
        } else {
            "EVIDENCE_FOUND".into()
        },
        knowledge_base_ids,
        embedding_group_count,
        warnings: Vec::new(),
        exclusions: Vec::new(),
        results,
    })
}

pub fn retrieve_resolved_generation(
    generation: &FullBaseGeneration,
    authorization: &AuthorizationSnapshot,
    request: &RetrieveRequest,
    now: DateTime<Utc>,
) -> Result<RetrievalResponse, KnowledgeError> {
    retrieve_resolved_generation_with_gate(generation, authorization, request, now, true)
}

pub fn retrieve_resolved_generation_with_gate(
    generation: &FullBaseGeneration,
    authorization: &AuthorizationSnapshot,
    request: &RetrieveRequest,
    now: DateTime<Utc>,
    lexical_evidence_required: bool,
) -> Result<RetrievalResponse, KnowledgeError> {
    let mut resolved = generation.clone();
    if resolved.manifest.segment_kind == "BASE+DELTA" {
        resolved.manifest.segment_kind = "BASE".into();
    }
    retrieve_with_lexical_gate(
        &resolved,
        authorization,
        request,
        now,
        lexical_evidence_required,
    )
}

pub fn compact_resolved_generation(
    resolved: &FullBaseGeneration,
) -> Result<FullBaseGeneration, KnowledgeError> {
    if resolved.manifest.segment_kind != "BASE+DELTA" {
        return Err(KnowledgeError::InvalidSource(
            "compaction requires a resolved BASE+DELTA generation".into(),
        ));
    }
    let corpus_digest = sha256_hex(
        resolved
            .chunks
            .iter()
            .map(|chunk| format!("{}:{}", chunk.chunk_id, chunk.content_digest))
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    let generation_id = stable_uuid(&[
        b"knowledge-compaction-v1",
        resolved.manifest.knowledge_base_id.as_bytes(),
        corpus_digest.as_bytes(),
    ]);
    let mut compacted = resolved.clone();
    compacted.manifest.generation_id = generation_id;
    compacted.manifest.segment_id = stable_uuid(&[b"base", generation_id.as_bytes()]);
    compacted.manifest.manifest_digest = corpus_digest;
    compacted.manifest.segment_kind = "BASE".into();
    Ok(compacted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingMigrationState {
    Preflighted,
    Backfilling,
    Paused,
    CatchingUp,
    Validating,
    Ready,
    Promoted,
    RolledBack,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingMigrationLedger {
    pub state: EmbeddingMigrationState,
    pub snapshot_watermark: u64,
    pub final_watermark: Option<u64>,
    pub estimated_chunks: usize,
    pub catchup_chunks: usize,
    pub completed_chunks: usize,
    pub accepted_cost_ceiling_micros: u64,
    pub consumed_cost_micros: u64,
}

impl EmbeddingMigrationLedger {
    pub fn start_backfill(&mut self) -> Result<(), KnowledgeError> {
        if !matches!(
            self.state,
            EmbeddingMigrationState::Preflighted
                | EmbeddingMigrationState::Paused
                | EmbeddingMigrationState::CatchingUp
        ) {
            return Err(KnowledgeError::MigrationStateConflict);
        }
        self.state = EmbeddingMigrationState::Backfilling;
        Ok(())
    }

    pub fn record_batch(&mut self, chunks: usize, cost_micros: u64) -> Result<(), KnowledgeError> {
        if self.state != EmbeddingMigrationState::Backfilling {
            return Err(KnowledgeError::MigrationStateConflict);
        }
        let next_cost = self.consumed_cost_micros.saturating_add(cost_micros);
        if next_cost > self.accepted_cost_ceiling_micros {
            self.state = EmbeddingMigrationState::Paused;
            return Err(KnowledgeError::MigrationCostCeilingExceeded);
        }
        self.consumed_cost_micros = next_cost;
        self.completed_chunks = self.completed_chunks.saturating_add(chunks);
        Ok(())
    }

    pub fn begin_catchup(&mut self, newly_discovered_chunks: usize) -> Result<(), KnowledgeError> {
        if self.state != EmbeddingMigrationState::Backfilling
            || self.completed_chunks < self.estimated_chunks
        {
            return Err(KnowledgeError::MigrationBackfillIncomplete);
        }
        self.catchup_chunks = self.catchup_chunks.saturating_add(newly_discovered_chunks);
        self.state = if newly_discovered_chunks == 0 {
            EmbeddingMigrationState::Validating
        } else {
            EmbeddingMigrationState::Backfilling
        };
        Ok(())
    }

    pub fn final_fence(
        &mut self,
        candidate_watermark: u64,
        active_watermark: u64,
    ) -> Result<(), KnowledgeError> {
        if self.state != EmbeddingMigrationState::Validating {
            return Err(KnowledgeError::MigrationStateConflict);
        }
        if candidate_watermark != active_watermark
            || self.completed_chunks < self.estimated_chunks + self.catchup_chunks
        {
            self.state = EmbeddingMigrationState::CatchingUp;
            return Err(KnowledgeError::MigrationFinalFenceFailed);
        }
        self.final_watermark = Some(active_watermark);
        self.state = EmbeddingMigrationState::Ready;
        Ok(())
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
    fn zero_chunk_documents_are_not_part_of_the_indexed_corpus() {
        let knowledge_base_id = Uuid::from_u128(1);
        let mut inputs = documents();
        inputs.push(DocumentInput {
            source_object_id: "empty.md".into(),
            canonical_uri: "repo://empty.md".into(),
            source_version: "empty-v1".into(),
            markdown: " \n\t".into(),
        });
        let generation = build_full_base(
            knowledge_base_id,
            7,
            &inputs,
            &ProcessingContract::default(),
            &SourceLimits::default(),
        )
        .unwrap();
        assert_eq!(generation.manifest.document_count, 2);
        assert!(
            generation
                .chunks
                .iter()
                .all(|chunk| chunk.source_object_id != "empty.md")
        );
        assert!(
            classify_corpus_changes(
                knowledge_base_id,
                &[],
                &[corpus_state("empty.md", " \n\t", "{}", "scope")]
            )
            .is_empty()
        );
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
        fs::write(directory.path().join("empty.md"), " \n\t").unwrap();
        let docs = ingest_markdown_repository(directory.path(), &SourceLimits::default()).unwrap();
        assert!(
            docs.iter()
                .all(|document| document.source_object_id != "empty.md")
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

    fn corpus_state(id: &str, content: &str, metadata: &str, acl: &str) -> CorpusDocumentState {
        CorpusDocumentState {
            source_object_id: id.into(),
            canonical_uri: format!("repo://{id}"),
            source_version: format!("v-{content}-{metadata}-{acl}"),
            content_digest: sha256_hex(content.as_bytes()),
            metadata_digest: sha256_hex(metadata.as_bytes()),
            acl_digest: sha256_hex(acl.as_bytes()),
            markdown: content.into(),
        }
    }

    #[test]
    fn incremental_classifier_covers_all_five_operations() {
        let before = vec![
            corpus_state("delete.md", "delete", "m", "a"),
            corpus_state("modify.md", "old", "m", "a"),
            corpus_state("acl.md", "same", "m", "old"),
            corpus_state("metadata.md", "same", "old", "a"),
        ];
        let after = vec![
            corpus_state("add.md", "add", "m", "a"),
            corpus_state("modify.md", "new", "m", "a"),
            corpus_state("acl.md", "same", "m", "new"),
            corpus_state("metadata.md", "same", "new", "a"),
        ];
        let changes = classify_corpus_changes(Uuid::from_u128(1), &before, &after);
        assert_eq!(
            changes
                .iter()
                .map(|change| change.kind)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ChangeKind::Add,
                ChangeKind::Modify,
                ChangeKind::Delete,
                ChangeKind::AclOnly,
                ChangeKind::MetadataOnly,
            ])
        );
        assert_eq!(
            changes,
            classify_corpus_changes(Uuid::from_u128(1), &before, &after)
        );
    }

    #[test]
    fn delta_resolution_and_compaction_preserve_effective_corpus() {
        let kb = Uuid::from_u128(1);
        let contract = ProcessingContract::default();
        let limits = SourceLimits::default();
        let original = documents()
            .into_iter()
            .map(CorpusDocumentState::from)
            .collect::<Vec<_>>();
        let base = build_full_base(
            kb,
            1,
            &original
                .iter()
                .map(|document| DocumentInput {
                    source_object_id: document.source_object_id.clone(),
                    canonical_uri: document.canonical_uri.clone(),
                    source_version: document.source_version.clone(),
                    markdown: document.markdown.clone(),
                })
                .collect::<Vec<_>>(),
            &contract,
            &limits,
        )
        .unwrap();
        let current = vec![
            corpus_state(
                "config/server.md",
                "# Server\nSet `serviceId` and `environment` in server.yml.",
                "{}",
                "UNIFORM_SCOPE",
            ),
            corpus_state(
                "new.md",
                "# New\nIncremental content.",
                "{}",
                "UNIFORM_SCOPE",
            ),
        ];
        let delta = build_delta_segment(
            kb,
            2,
            &base.manifest.manifest_digest,
            &original,
            &current,
            &contract,
            &limits,
        )
        .unwrap();
        let resolved = resolve_base_plus_deltas(&base, &[delta]).unwrap();
        assert_eq!(resolved.manifest.segment_kind, "BASE+DELTA");
        assert!(
            resolved
                .chunks
                .iter()
                .any(|chunk| chunk.text.contains("environment"))
        );
        assert!(
            !resolved
                .chunks
                .iter()
                .any(|chunk| chunk.text.contains("Authorization"))
        );
        let compacted = compact_resolved_generation(&resolved).unwrap();
        assert_eq!(compacted.manifest.segment_kind, "BASE");
        assert_eq!(compacted.chunks, resolved.chunks);
        assert_ne!(
            compacted.manifest.generation_id,
            resolved.manifest.generation_id
        );
        assert_eq!(compacted, compact_resolved_generation(&resolved).unwrap());
    }

    #[test]
    fn recurring_semantic_change_has_distinct_occurrence_identity() {
        let kb = Uuid::from_u128(1);
        let contract = ProcessingContract::default();
        let limits = SourceLimits::default();
        let v1 = vec![corpus_state("doc.md", "v1", "{}", "scope")];
        let v2 = vec![corpus_state("doc.md", "v2", "{}", "scope")];
        let first =
            build_delta_segment(kb, 2, &"a".repeat(64), &v1, &v2, &contract, &limits).unwrap();
        let repeated =
            build_delta_segment(kb, 4, &"b".repeat(64), &v1, &v2, &contract, &limits).unwrap();
        let deterministic_retry =
            build_delta_segment(kb, 4, &"b".repeat(64), &v1, &v2, &contract, &limits).unwrap();
        assert_ne!(
            first.operations[0].operation_id,
            repeated.operations[0].operation_id
        );
        assert_eq!(repeated, deterministic_retry);
    }

    #[test]
    fn principal_acl_is_fresh_complete_deny_first_and_fail_closed() {
        let now = Utc::now();
        let principal = PrincipalContext {
            subject_id: "user-1".into(),
            subject_type: "user".into(),
            groups: BTreeSet::from(["group-readers".into()]),
            organizations: BTreeSet::from(["org-1".into()]),
        };
        let allow_group = AclSubject {
            provider_subject_id: "provider-group-readers".into(),
            subject_type: AclSubjectType::Group,
            subject_id: "group-readers".into(),
            effect: AclEffect::Allow,
            mapping_complete: true,
            provider_evidence_digest: "a".repeat(64),
        };
        let mut acl = NormalizedAclRevision {
            mode: AclMode::MirrorSourceAcl,
            complete: true,
            observed_at: now - Duration::minutes(1),
            fresh_until: now + Duration::minutes(14),
            provider_effective_decision: true,
            subjects: vec![allow_group],
        };
        assert!(authorize_document_acl(&acl, &principal, now));

        acl.subjects.push(AclSubject {
            provider_subject_id: "provider-user-1".into(),
            subject_type: AclSubjectType::User,
            subject_id: "user-1".into(),
            effect: AclEffect::Deny,
            mapping_complete: true,
            provider_evidence_digest: "b".repeat(64),
        });
        assert!(!authorize_document_acl(&acl, &principal, now));
        acl.subjects.pop();
        acl.complete = false;
        assert!(!authorize_document_acl(&acl, &principal, now));
        acl.complete = true;
        acl.fresh_until = now;
        assert!(!authorize_document_acl(&acl, &principal, now));
        acl.fresh_until = now + Duration::minutes(1);
        acl.subjects[0].mapping_complete = false;
        assert!(!authorize_document_acl(&acl, &principal, now));
    }

    #[test]
    fn embedding_reuse_is_scoped_and_last_reference_deletes() {
        let mut ledger = EmbeddingReuseLedger::default();
        let key = ScopedEmbeddingReuseKey {
            knowledge_base_id: Uuid::from_u128(1),
            input_digest: "a".repeat(64),
            space_id: "space".into(),
            space_revision: 1,
            dimension: 32,
            transform_digest: "b".repeat(64),
        };
        let (artifact, reused) = ledger.acquire(key.clone());
        assert!(!reused);
        assert_eq!(ledger.acquire(key.clone()), (artifact, true));
        let mut other = key.clone();
        other.knowledge_base_id = Uuid::from_u128(2);
        assert_ne!(ledger.acquire(other).0, artifact);
        assert!(!ledger.release(&key));
        assert!(ledger.release(&key));
    }

    #[test]
    fn multi_kb_fusion_is_deterministic_fair_and_space_grouped() {
        let now = Utc::now();
        let host = Uuid::from_u128(9);
        let responses = [Uuid::from_u128(1), Uuid::from_u128(2)]
            .into_iter()
            .enumerate()
            .map(|(index, kb)| {
                let generation = build_full_base(
                    kb,
                    1,
                    &documents(),
                    &ProcessingContract::default(),
                    &SourceLimits::default(),
                )
                .unwrap();
                let request = RetrieveRequest {
                    knowledge_base_ids: vec![kb],
                    environment: "dev".into(),
                    query: "serviceId".into(),
                    top_k: 2,
                    token_budget: 1000,
                    filters: None,
                };
                KnowledgeBaseRankedResponse {
                    response: retrieve(
                        &generation,
                        &default_authorization(kb, host, "dev"),
                        &request,
                        now,
                    )
                    .unwrap(),
                    priority: 10 - index as i32,
                    embedding_group_key: "fake:v1:32".into(),
                }
            })
            .collect::<Vec<_>>();
        let first = fuse_knowledge_base_results(responses.clone(), 4, 2, 1000).unwrap();
        let second = fuse_knowledge_base_results(responses, 4, 2, 1000).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.embedding_group_count, 1);
        assert_eq!(first.results.len(), 2);
        assert_eq!(
            first
                .results
                .iter()
                .map(|result| result.knowledge_base_id)
                .collect::<BTreeSet<_>>()
                .len(),
            2
        );
    }

    #[test]
    fn embedding_migration_budget_and_final_fence_fail_closed() {
        let mut migration = EmbeddingMigrationLedger {
            state: EmbeddingMigrationState::Preflighted,
            snapshot_watermark: 10,
            final_watermark: None,
            estimated_chunks: 2,
            catchup_chunks: 0,
            completed_chunks: 0,
            accepted_cost_ceiling_micros: 100,
            consumed_cost_micros: 0,
        };
        migration.start_backfill().unwrap();
        migration.record_batch(2, 80).unwrap();
        migration.begin_catchup(0).unwrap();
        assert!(migration.final_fence(10, 11).is_err());
        assert_eq!(migration.state, EmbeddingMigrationState::CatchingUp);

        migration.start_backfill().unwrap();
        assert!(migration.record_batch(1, 21).is_err());
        assert_eq!(migration.state, EmbeddingMigrationState::Paused);
        assert_eq!(migration.consumed_cost_micros, 80);
    }

    #[test]
    fn final_fence_does_not_rewind_an_invalid_state() {
        let mut migration = EmbeddingMigrationLedger {
            state: EmbeddingMigrationState::Ready,
            snapshot_watermark: 10,
            final_watermark: Some(10),
            estimated_chunks: 1,
            catchup_chunks: 0,
            completed_chunks: 1,
            accepted_cost_ceiling_micros: 10,
            consumed_cost_micros: 1,
        };
        assert_eq!(
            migration.final_fence(10, 10),
            Err(KnowledgeError::MigrationStateConflict)
        );
        assert_eq!(migration.state, EmbeddingMigrationState::Ready);
    }

    #[test]
    fn embedding_migration_reaches_ready_only_at_one_current_watermark() {
        let mut migration = EmbeddingMigrationLedger {
            state: EmbeddingMigrationState::Preflighted,
            snapshot_watermark: 20,
            final_watermark: None,
            estimated_chunks: 2,
            catchup_chunks: 0,
            completed_chunks: 0,
            accepted_cost_ceiling_micros: 100,
            consumed_cost_micros: 0,
        };
        migration.start_backfill().unwrap();
        migration.record_batch(2, 50).unwrap();
        migration.begin_catchup(1).unwrap();
        migration.record_batch(1, 10).unwrap();
        migration.begin_catchup(0).unwrap();
        migration.final_fence(24, 24).unwrap();
        assert_eq!(migration.state, EmbeddingMigrationState::Ready);
        assert_eq!(migration.final_watermark, Some(24));
    }
}
