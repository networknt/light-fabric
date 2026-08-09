use chrono::{Duration, Utc};
use knowledge_core::{
    AuthorizationSnapshot, CorpusDocumentState, DocumentInput, KnowledgeBaseRankedResponse,
    ProcessingContract, RetrieveRequest, SourceLimits, build_delta_segment, build_full_base,
    compact_resolved_generation, fuse_knowledge_base_results, resolve_base_plus_deltas,
    retrieve_resolved_generation,
};
use uuid::Uuid;

fn state(id: &str, version: &str, text: &str) -> CorpusDocumentState {
    CorpusDocumentState::from(DocumentInput {
        source_object_id: id.into(),
        canonical_uri: format!("repo://{id}"),
        source_version: version.into(),
        markdown: text.into(),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let contract = ProcessingContract::default();
    let limits = SourceLimits::default();
    let now = Utc::now();
    let host = Uuid::from_u128(900);
    let mut ranked = Vec::new();
    for (index, kb) in [Uuid::from_u128(101), Uuid::from_u128(102)]
        .into_iter()
        .enumerate()
    {
        let previous = vec![state(
            "guide.md",
            "v1",
            "# Guide\nA BASE passage about deterministic indexing.",
        )];
        let base = build_full_base(
            kb,
            1,
            &[DocumentInput {
                source_object_id: previous[0].source_object_id.clone(),
                canonical_uri: previous[0].canonical_uri.clone(),
                source_version: previous[0].source_version.clone(),
                markdown: previous[0].markdown.clone(),
            }],
            &contract,
            &limits,
        )?;
        let current = vec![state(
            "guide.md",
            "v2",
            &format!(
                "# Guide\nA DELTA passage about deterministic indexing for Knowledge Base {}.",
                index + 1
            ),
        )];
        let delta = build_delta_segment(
            kb,
            2,
            &base.manifest.manifest_digest,
            &previous,
            &current,
            &contract,
            &limits,
        )?;
        let resolved = resolve_base_plus_deltas(&base, &[delta])?;
        let compacted = compact_resolved_generation(&resolved)?;
        if compacted.chunks != resolved.chunks {
            return Err("compaction changed the effective corpus".into());
        }
        let authorization = AuthorizationSnapshot {
            knowledge_base_id: kb,
            consumer_host_id: host,
            environment: "dev".into(),
            active: true,
            desired_event_sequence: 2,
            applied_event_sequence: 2,
            authorization_lease_expires_at: now + Duration::seconds(30),
            projector_lease_expires_at: now + Duration::seconds(30),
        };
        let response = retrieve_resolved_generation(
            &resolved,
            &authorization,
            &RetrieveRequest {
                knowledge_base_ids: vec![kb],
                environment: "dev".into(),
                query: "deterministic indexing".into(),
                top_k: 2,
                token_budget: 500,
                filters: None,
            },
            now,
        )?;
        ranked.push(KnowledgeBaseRankedResponse {
            response,
            priority: 100 - index as i32,
            embedding_group_key: "pilot-space:v1:32:query-v1".into(),
        });
    }
    let response = fuse_knowledge_base_results(ranked, 4, 2, 500)?;
    if response.results.len() != 2 || response.embedding_group_count != 1 {
        return Err("multi-KB fairness or embedding grouping failed".into());
    }
    println!(
        "{}",
        serde_json::json!({
            "status": "PASS",
            "segmentMode": "BASE_PLUS_DELTA",
            "knowledgeBaseCount": response.knowledge_base_ids.len(),
            "embeddingGroupCount": response.embedding_group_count,
            "fairResultCount": response.results.len(),
            "mcpTools": ["knowledge.search", "knowledge.get_document"]
        })
    );
    Ok(())
}
