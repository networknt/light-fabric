use execution_runner_protocol::{ArtifactEvidence, NormalizedExecutionResult, canonical_sha256};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustedBuildProvenance {
    pub predicate_type: String,
    pub execution_id: String,
    pub definition_digest: String,
    pub policy_digest: String,
    pub command_template_digest: String,
    pub compatibility_digest: String,
    pub backend_operation_id: String,
    pub inputs: Vec<String>,
    pub artifacts: Vec<ArtifactEvidence>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
}

pub fn generate_trusted_provenance(
    result: &NormalizedExecutionResult,
    input_digests: Vec<String>,
) -> Result<(TrustedBuildProvenance, String), execution_runner_protocol::CanonicalJsonError> {
    let statement = TrustedBuildProvenance {
        predicate_type: "https://slsa.dev/provenance/v1".into(),
        execution_id: result.execution_id.to_string(),
        definition_digest: result.definition_digest.clone(),
        policy_digest: result.policy_digest.clone(),
        command_template_digest: result.command_template_digest.clone(),
        compatibility_digest: result.compatibility_digest.clone(),
        backend_operation_id: result.backend_operation_id.clone(),
        inputs: input_digests,
        artifacts: result.artifacts.clone(),
        started_at: result.started_at,
        finished_at: result.finished_at,
    };
    let digest = canonical_sha256(&statement)?;
    Ok((statement, digest))
}
