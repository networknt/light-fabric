use super::fixtures::{ConformanceCapability, FixtureProvenance};
use super::runner::{
    CONFORMANCE_RESULT_SCHEMA_VERSION, CapabilityEvidence, CaseResult, ConformanceResult,
    ConformanceState, EvidenceKind, LiveEvidenceBinding,
};
use crate::inference::capabilities::{
    ContentCapabilities, EmbeddingCapabilities, GenerationCapabilities, ProviderCapabilities,
};
use crate::inference::provider::{Operation, ProviderProtocol};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveQualificationSpec {
    pub provider: ProviderProtocol,
    pub provider_codec_version: String,
    pub physical_model: String,
    pub api_version: String,
    pub operations: BTreeSet<Operation>,
    pub live_evidence: LiveEvidenceBinding,
    pub valid_for_seconds: i64,
    pub refresh_before_seconds: u64,
    pub max_parallel_requests: usize,
    #[serde(default = "default_max_queued_requests")]
    pub max_queued_requests: usize,
    #[serde(default = "default_cold_start_timeout_ms")]
    pub cold_start_timeout_ms: u64,
    #[serde(default = "default_stream_setup_timeout_ms")]
    pub stream_setup_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    #[serde(default)]
    pub embedding_capabilities: Option<EmbeddingCapabilities>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LiveProbeReport {
    #[serde(default)]
    pub successful_operations: BTreeSet<Operation>,
    #[serde(default)]
    pub streaming_succeeded: bool,
    #[serde(default)]
    pub disconnect_succeeded: bool,
    #[serde(default)]
    pub warmup_succeeded: bool,
    #[serde(default)]
    pub cold_start_millis: u64,
    #[serde(default)]
    pub parallelism_observed: usize,
    #[serde(default)]
    pub queue_saturation_observed: usize,
    #[serde(default)]
    pub timeout_bounds_succeeded: bool,
    #[serde(default)]
    pub embedding_format_succeeded: bool,
    #[serde(default)]
    pub cases: Vec<CaseResult>,
}

pub fn build_live_conformance_result(
    spec: &LiveQualificationSpec,
    report: LiveProbeReport,
) -> Result<ConformanceResult, String> {
    validate_live_spec(spec)?;
    let passing = spec
        .operations
        .iter()
        .all(|operation| report.successful_operations.contains(operation))
        && report.warmup_succeeded
        && report.cold_start_millis <= spec.cold_start_timeout_ms
        && report.parallelism_observed >= spec.max_parallel_requests
        && report.queue_saturation_observed
            >= spec
                .max_parallel_requests
                .saturating_add(spec.max_queued_requests)
        && report.timeout_bounds_succeeded
        && (!spec.operations.contains(&Operation::Embed) || report.embedding_format_succeeded)
        && report.cases.iter().all(|case| case.passed);
    let generation =
        spec.operations
            .contains(&Operation::Generate)
            .then(|| GenerationCapabilities {
                content: ContentCapabilities {
                    text: true,
                    ..ContentCapabilities::default()
                },
                streaming: report.streaming_succeeded,
            });
    let embedding = spec
        .operations
        .contains(&Operation::Embed)
        .then(|| spec.embedding_capabilities.clone().unwrap_or_default());
    let capabilities = ProviderCapabilities {
        operations: report.successful_operations.clone(),
        generation,
        embedding,
    };
    let mut evidence = BTreeMap::new();
    for operation in &report.successful_operations {
        let capability = match operation {
            Operation::Generate => ConformanceCapability::Generate,
            Operation::Embed => ConformanceCapability::Embed,
        };
        evidence.insert(capability, live_capability_evidence(capability));
    }
    if report.successful_operations.contains(&Operation::Generate) {
        evidence.insert(
            ConformanceCapability::Text,
            live_capability_evidence(ConformanceCapability::Text),
        );
        if report.streaming_succeeded {
            evidence.insert(
                ConformanceCapability::Streaming,
                live_capability_evidence(ConformanceCapability::Streaming),
            );
        }
    }
    let now = Utc::now();
    let report_digest = live_report_digest(&report);
    let mut result = ConformanceResult {
        schema_version: CONFORMANCE_RESULT_SCHEMA_VERSION.to_string(),
        evidence_kind: EvidenceKind::LiveEndpoint,
        live_evidence: Some(spec.live_evidence.clone()),
        provider: spec.provider,
        provider_codec_version: spec.provider_codec_version.clone(),
        physical_model: spec.physical_model.clone(),
        api_version: spec.api_version.clone(),
        tested_operations: report.successful_operations,
        capabilities,
        capability_evidence: evidence,
        state: if passing {
            ConformanceState::Pass
        } else {
            ConformanceState::Fail
        },
        corpus_version: "live-endpoint-v1".to_string(),
        corpus_digest: report_digest,
        tested_at: now,
        valid_until: now + Duration::seconds(spec.valid_for_seconds),
        refresh_before_seconds: spec.refresh_before_seconds,
        pii_preservation: None,
        cases: report.cases,
        quarantine_reason: None,
        signer_key_id: None,
        signature: None,
        digest: String::new(),
    };
    result.refresh_digest();
    Ok(result)
}

fn validate_live_spec(spec: &LiveQualificationSpec) -> Result<(), String> {
    if spec.operations.is_empty()
        || spec.valid_for_seconds <= 0
        || spec.max_parallel_requests == 0
        || spec.max_queued_requests == 0
        || spec.cold_start_timeout_ms == 0
        || spec.stream_setup_timeout_ms == 0
        || spec.request_timeout_ms == 0
        || spec.stream_setup_timeout_ms > spec.request_timeout_ms
        || spec.cold_start_timeout_ms > spec.request_timeout_ms
        || spec.live_evidence.deployment_revision_id.trim().is_empty()
        || spec.live_evidence.physical_runtime_id.trim().is_empty()
        || !is_digest(&spec.live_evidence.provider_endpoint_sha256)
        || !is_digest(&spec.live_evidence.network_profile_sha256)
        || !is_digest(&spec.live_evidence.capacity_declaration_sha256)
    {
        return Err("live qualification identity/capacity contract is incomplete".to_string());
    }
    if let Some(sidecar) = &spec.live_evidence.sidecar {
        if sidecar.raw_port_reachable
            || !is_digest(&sidecar.config_sha256)
            || !is_digest(&sidecar.certificate_identity_sha256)
            || !is_digest(&sidecar.isolation_evidence_sha256)
        {
            return Err("sidecar isolation or identity evidence is invalid".to_string());
        }
        let vantage = spec.live_evidence.runner_vantage.as_ref().ok_or_else(|| {
            "sidecar qualification requires external runner vantage evidence".to_string()
        })?;
        if vantage.source_network_namespace_id.trim().is_empty()
            || vantage.target_network_namespace_id.trim().is_empty()
            || vantage.source_network_namespace_id == vantage.target_network_namespace_id
            || !is_digest(&vantage.raw_probe_target_sha256)
        {
            return Err(
                "runner and sidecar target must have distinct recorded namespaces".to_string(),
            );
        }
    }
    Ok(())
}

const fn default_max_queued_requests() -> usize {
    1
}

const fn default_cold_start_timeout_ms() -> u64 {
    30_000
}

const fn default_stream_setup_timeout_ms() -> u64 {
    10_000
}

const fn default_request_timeout_ms() -> u64 {
    30_000
}

fn live_capability_evidence(capability: ConformanceCapability) -> CapabilityEvidence {
    CapabilityEvidence {
        fixture_ids: BTreeSet::from([format!("live:{capability:?}").to_ascii_lowercase()]),
        provenances: BTreeSet::from([FixtureProvenance::CapturedSanitized]),
    }
}

fn live_report_digest(report: &LiveProbeReport) -> String {
    let bytes = super::signing::canonical_json_bytes(report).unwrap_or_default();
    super::signing::sha256_hex(&bytes)
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::super::runner::{RunnerVantage, SidecarEvidence};
    use super::*;

    fn spec() -> LiveQualificationSpec {
        LiveQualificationSpec {
            provider: ProviderProtocol::OpenAiEmbeddings,
            provider_codec_version: "openai-v1".to_string(),
            physical_model: "nomic-embed-text".to_string(),
            api_version: "v1".to_string(),
            operations: BTreeSet::from([Operation::Embed]),
            live_evidence: LiveEvidenceBinding {
                deployment_revision_id: "deployment/r7".to_string(),
                provider_endpoint_sha256: "a".repeat(64),
                network_profile_sha256: "b".repeat(64),
                physical_runtime_id: "gpu-a/ollama".to_string(),
                capacity_declaration_sha256: "c".repeat(64),
                sidecar: Some(SidecarEvidence {
                    profile_version: "ollama-embedding-v1".to_string(),
                    config_sha256: "d".repeat(64),
                    certificate_identity_sha256: "e".repeat(64),
                    isolation_evidence_sha256: "f".repeat(64),
                    raw_port_reachable: false,
                }),
                runner_vantage: Some(RunnerVantage {
                    kind: "kubernetes_external_namespace".to_string(),
                    environment_id: "prod".to_string(),
                    source_workload_id: "runner-a".to_string(),
                    source_network_zone_id: "runner-zone".to_string(),
                    source_network_namespace_id: "qualification".to_string(),
                    target_network_namespace_id: "models".to_string(),
                    raw_probe_target_sha256: "1".repeat(64),
                }),
            },
            valid_for_seconds: 3600,
            refresh_before_seconds: 300,
            max_parallel_requests: 1,
            max_queued_requests: 1,
            cold_start_timeout_ms: 10_000,
            stream_setup_timeout_ms: 5_000,
            request_timeout_ms: 10_000,
            embedding_capabilities: Some(EmbeddingCapabilities {
                max_batch_items: 1,
                ..EmbeddingCapabilities::default()
            }),
        }
    }

    #[test]
    fn live_result_is_type_level_distinct_and_endpoint_bound() {
        let report = LiveProbeReport {
            successful_operations: BTreeSet::from([Operation::Embed]),
            warmup_succeeded: true,
            cold_start_millis: 1,
            parallelism_observed: 1,
            queue_saturation_observed: 2,
            timeout_bounds_succeeded: true,
            embedding_format_succeeded: true,
            cases: vec![CaseResult {
                id: "embedding".to_string(),
                passed: true,
                detail: None,
            }],
            ..LiveProbeReport::default()
        };
        let result = build_live_conformance_result(&spec(), report).unwrap();
        assert_eq!(result.evidence_kind, EvidenceKind::LiveEndpoint);
        assert_eq!(result.state, ConformanceState::Pass);
        assert!(result.verify_digest());
    }

    #[test]
    fn same_namespace_or_missing_isolation_fails_closed() {
        let mut value = spec();
        value
            .live_evidence
            .runner_vantage
            .as_mut()
            .unwrap()
            .target_network_namespace_id = "qualification".to_string();
        assert!(build_live_conformance_result(&value, LiveProbeReport::default()).is_err());
        let mut value = spec();
        value
            .live_evidence
            .sidecar
            .as_mut()
            .unwrap()
            .isolation_evidence_sha256
            .clear();
        assert!(build_live_conformance_result(&value, LiveProbeReport::default()).is_err());
    }
}
