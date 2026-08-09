use std::env;
use std::fs;
use std::path::Path;

use chrono::{Duration, Utc};
use knowledge_core::{
    AuthorizationSnapshot, ProcessingContract, RetrieveRequest, SourceLimits, build_full_base,
    ingest_markdown_repository, retrieve,
};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Expected {
    queries: Vec<Query>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Query {
    query: String,
    expected_uri: Option<String>,
    no_answer: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().collect::<Vec<_>>();
    if arguments.len() != 3 {
        return Err("usage: phase1a_pilot FIXTURE_DIRECTORY EXPECTED_JSON".into());
    }
    let documents = ingest_markdown_repository(Path::new(&arguments[1]), &SourceLimits::default())?;
    let knowledge_base_id = Uuid::parse_str("13000000-0000-7000-8000-000000000101")?;
    let consumer_host_id = Uuid::parse_str("13000000-0000-7000-8000-000000000102")?;
    let first = build_full_base(
        knowledge_base_id,
        1,
        &documents,
        &ProcessingContract::default(),
        &SourceLimits::default(),
    )?;
    let second = build_full_base(
        knowledge_base_id,
        1,
        &documents,
        &ProcessingContract::default(),
        &SourceLimits::default(),
    )?;
    if first != second {
        return Err("full BASE generation is not deterministic".into());
    }
    let now = Utc::now();
    let authorization = AuthorizationSnapshot {
        knowledge_base_id,
        consumer_host_id,
        environment: "dev".into(),
        active: true,
        desired_event_sequence: 7,
        applied_event_sequence: 7,
        authorization_lease_expires_at: now + Duration::seconds(30),
        projector_lease_expires_at: now + Duration::seconds(30),
    };
    let expected: Expected = serde_json::from_slice(&fs::read(&arguments[2])?)?;
    for fixture in expected.queries {
        let response = retrieve(
            &first,
            &authorization,
            &RetrieveRequest {
                knowledge_base_ids: vec![knowledge_base_id],
                environment: "dev".into(),
                query: fixture.query.clone(),
                top_k: 5,
                token_budget: 2_000,
                filters: None,
            },
            now,
        )?;
        if response.no_answer != fixture.no_answer {
            return Err(format!("no-answer mismatch for {}", fixture.query).into());
        }
        if let Some(expected_uri) = fixture.expected_uri {
            let actual = response
                .results
                .first()
                .map(|hit| hit.citation.canonical_uri.as_str());
            if actual != Some(expected_uri.as_str()) {
                return Err(format!(
                    "citation mismatch for {}: expected {}, got {:?}",
                    fixture.query, expected_uri, actual
                )
                .into());
            }
        }
    }
    println!(
        "{}",
        serde_json::json!({
            "status": "PASS",
            "generationId": first.manifest.generation_id,
            "manifestDigest": first.manifest.manifest_digest,
            "documentCount": first.manifest.document_count,
            "chunkCount": first.manifest.chunk_count,
            "segmentKind": first.manifest.segment_kind,
        })
    );
    Ok(())
}
