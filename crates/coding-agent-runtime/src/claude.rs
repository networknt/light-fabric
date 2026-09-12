//! Shared Claude local-worker admission policy and pinned technical contract.
use crate::{
    CodingAdapterContract, CodingAdapterQualification, CodingAdapterQualificationDimension,
    CodingAdapterQualificationStatus,
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionSource {
    AgentPolicy,
    ClaudeCli,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Inherit,
    DontAsk,
    BypassPermissions,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchPolicy {
    pub permission_source: PermissionSource,
    pub permission_mode: PermissionMode,
    /// Explicit server-owned default; omission on resume resolves identically.
    pub default_model: String,
    /// Native alias -> expected reported model. No silent fallback.
    pub models: BTreeMap<String, String>,
    pub tools: Vec<String>,
    /// CLI permission rules, distinct from the available tool surface.
    pub allowed_tools: Vec<String>,
}
impl LaunchPolicy {
    pub fn resolve(&self, requested: Option<&str>) -> Result<&str> {
        ensure!(
            self.permission_source != PermissionSource::AgentPolicy
                || self.permission_mode != PermissionMode::Inherit,
            "managed permission policy cannot inherit"
        );
        ensure!(
            self.permission_source != PermissionSource::ClaudeCli
                || self.tools.is_empty() && self.allowed_tools.is_empty(),
            "native permissions cannot include Light tool overrides"
        );
        ensure!(
            self.tools.len() <= 64 && self.tools.iter().all(|v| identifier(v)),
            "invalid tool policy"
        );
        ensure!(
            !self.models.is_empty()
                && self.models.len() <= 32
                && self
                    .models
                    .iter()
                    .all(|(a, b)| identifier(a) && identifier(b)),
            "invalid model policy"
        );
        ensure!(
            self.models.contains_key(&self.default_model),
            "default native model not admitted"
        );
        ensure!(
            self.allowed_tools.len() <= 64
                && self.allowed_tools.iter().all(|rule| !rule.is_empty()
                    && rule.len() <= 512
                    && !rule.starts_with('-')
                    && !rule.chars().any(char::is_control)),
            "invalid native tool permission rule"
        );
        let key = requested.unwrap_or(&self.default_model);
        self.models
            .get_key_value(key)
            .map(|(key, _)| key.as_str())
            .context("native model not admitted")
    }
}
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

pub const ADAPTER_ID: &str = "claude-code-v1";
pub const VERSION: &str = "2.1.269";
pub const PROTOCOL: &str = "claude-cli-stream-json-v1";
pub const ACTION: &str = "coding.claude-code-v1";
pub const TEMPLATE: &str = "coding-claude-code-v1";
pub const BINARY_DIGEST: &str =
    "sha256:25e44883f54419569a3d739f38cbbdaebe83b09895da0f343e1b003710a4775b";
pub const LAUNCH: &str = include_str!("../../../contracts/claude-code/v2.1.269/phase2-launch.json");
pub const EVIDENCE: &str =
    include_str!("../../../contracts/claude-code/v2.1.269/phase2-qualification.json");
fn digest(bytes: &[u8]) -> String {
    use sha2::Digest;
    format!("sha256:{:x}", sha2::Sha256::digest(bytes))
}
pub fn schema_digest() -> String {
    digest(LAUNCH.as_bytes())
}
pub fn evidence_digest() -> String {
    digest(EVIDENCE.as_bytes())
}
pub fn capabilities() -> agent_runtime_protocol::RuntimeCapabilities {
    agent_runtime_protocol::RuntimeCapabilities {
        adapter_id: ADAPTER_ID.into(),
        adapter_version: VERSION.into(),
        adapter_protocol_version: PROTOCOL.into(),
        protocol_version: agent_runtime_protocol::PROTOCOL_VERSION.into(),
        actions: std::collections::BTreeSet::from([ACTION.into()]),
        supports_approvals: false,
        supports_checkpoint: false,
        supports_session_reuse: true,
        supports_streaming: true,
        supports_thread_turn_identity: false,
        supports_usage: false,
        maximum_event_bytes: 1024 * 1024,
    }
}
pub fn local_dimensions() -> std::collections::BTreeSet<CodingAdapterQualificationDimension> {
    let mut dimensions = CodingAdapterQualificationDimension::required();
    dimensions.remove(&CodingAdapterQualificationDimension::LicenseCompatibility);
    dimensions
}
/// Does not satisfy require_selectable(): local technical qualification never
/// silently promotes an adapter to the production/distribution qualification.
pub fn require_local_contract(
    contract: &CodingAdapterContract,
    evidence: &CodingAdapterQualification,
) -> Result<()> {
    contract.validate()?;
    evidence.validate()?;
    ensure!(
        contract.adapter_id == ADAPTER_ID
            && contract.adapter_version == VERSION
            && contract.adapter_protocol_version == PROTOCOL
            && contract.action_kind == ACTION
            && contract.template_id == TEMPLATE
            && contract.template_version == 1
            && contract.executable == "/usr/local/bin/claude"
            && contract.binary_digest == BINARY_DIGEST
            && contract.schema_digest == schema_digest()
            && contract.capability_digest
                == agent_runtime_protocol::canonical_digest(&capabilities())?
            && [
                ADAPTER_ID,
                "canonical-patch-output",
                "workflow-coding-threads-v1",
                "claude-review-namespace-v1"
            ]
            .iter()
            .all(|f| contract.required_features.contains(*f)),
        "Claude local contract does not match the pinned worker"
    );
    ensure!(
        evidence.status == CodingAdapterQualificationStatus::LocalQualified
            && evidence.adapter_id == ADAPTER_ID
            && evidence.adapter_version == VERSION
            && evidence.contract_digest.as_deref() == Some(contract.digest()?.as_str())
            && evidence.evidence_digest == evidence_digest(),
        "Claude local qualification does not match the admitted contract"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_qualification_never_satisfies_production_promotion() {
        let d = digest(b"test");
        let contract:CodingAdapterContract=serde_json::from_value(serde_json::json!({"schemaVersion":1,
            "adapterId":ADAPTER_ID,"adapterVersion":VERSION,"adapterProtocolVersion":PROTOCOL,"actionKind":ACTION,
            "compatibilityDigest":d,"imageDigest":d,"capabilityDigest":agent_runtime_protocol::canonical_digest(&capabilities()).unwrap(),
            "templateId":TEMPLATE,"templateVersion":1,"templateDigest":d,"executable":"/usr/local/bin/claude","binaryDigest":BINARY_DIGEST,
            "schemaDigest":schema_digest(),"requiredFeatures":[ADAPTER_ID,"canonical-patch-output","workflow-coding-threads-v1","claude-review-namespace-v1"]})).unwrap();
        let mut qualification = CodingAdapterQualification {
            schema_version: 1,
            adapter_id: ADAPTER_ID.into(),
            adapter_version: VERSION.into(),
            status: CodingAdapterQualificationStatus::LocalQualified,
            evaluated_dimensions: local_dimensions(),
            contract_digest: Some(contract.digest().unwrap()),
            evidence_digest: evidence_digest(),
        };
        require_local_contract(&contract, &qualification).unwrap();
        assert!(qualification.require_selectable(&contract).is_err());
        qualification
            .evaluated_dimensions
            .remove(&CodingAdapterQualificationDimension::ReviewIsolation);
        assert!(require_local_contract(&contract, &qualification).is_err());
        assert!(!capabilities().supports_approvals && !capabilities().supports_usage);
    }
}
