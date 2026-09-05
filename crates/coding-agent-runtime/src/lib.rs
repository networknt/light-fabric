use agent_runtime_protocol::canonical_digest;
use base64::Engine;
use execution_security::ProtectedPathPolicy;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeSet;
use thiserror::Error;

pub const CODEX_APP_SERVER_ADAPTER_ID: &str = "codex-app-server-v1";
pub const CODEX_APP_SERVER_VERSION: &str = "0.153.2";
pub const CODEX_APP_SERVER_PROTOCOL_VERSION: &str = "codex-app-server-v2";
pub const CODEX_APP_SERVER_SCHEMA_DIGEST: &str =
    "sha256:d3eace08be5dca386bfd1f1e8df650058b4113f1e10870a284d775d75517576a";
pub const CODEX_APP_SERVER_BINARY_DIGEST: &str =
    "sha256:f8786262ebc0fa1337448a2977332beadec66c8d0cda0ce973c7849766d7943c";
pub const CODEX_EMBEDDED_ADAPTER_ID: &str = "codex-embedded-v1";
pub const CODEX_EMBEDDED_UPSTREAM_REVISION: &str = "657a993cbee87acf52d14b758ce49dbd46d1b8eb";
pub const CODEX_APP_SERVER_QUALIFICATION_EVIDENCE_DIGEST: &str =
    "sha256:268432fcff0f5d90ad58f45be6d8e433baedcb4c6e96e7b16e4c82ee262ebf4c";
pub const CODEX_EMBEDDED_PROTOTYPE_EVIDENCE_DIGEST: &str =
    "sha256:98fc7e79b0680efa86f534dd456fd89f7959ed59b1b3bd421727f5a05dcf9174";
pub const CODING_ADAPTER_CONTRACT_VERSION: u16 = 1;
pub const CODING_ADAPTER_QUALIFICATION_VERSION: u16 = 1;
pub const CODING_ARTIFACT_SCHEMA_VERSION: u16 = 1;
/// Maximum canonical patch carried inline in one 1 MiB runtime event. The
/// remaining envelope budget covers JSON escaping, path metadata and identity.
pub const MAX_INLINE_PATCH_BYTES: u64 = 128 * 1024;
pub const CODING_IMPLEMENT_PROFILE_ID: &str = "coding-implement-v1";
pub const CODING_REVIEW_PROFILE_ID: &str = "coding-review-v1";
pub const CODING_IMPLEMENTER_ALIAS: &str = "coding-implementer";
pub const CODING_REVIEWER_ALIAS: &str = "coding-reviewer";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingAdapterQualificationStatus {
    PrototypeOnly,
    Qualified,
    Withdrawn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingAdapterQualificationDimension {
    ProtocolLifecycle,
    ApprovalMediation,
    StreamingEvents,
    UsageAccounting,
    Cancellation,
    Resumability,
    CanonicalPatch,
    ReviewIsolation,
    AuthenticationProfiles,
    WorkspaceIsolation,
    PanicContainment,
    DependencyCompatibility,
    LicenseCompatibility,
}

impl CodingAdapterQualificationDimension {
    pub fn required() -> BTreeSet<Self> {
        BTreeSet::from([
            Self::ProtocolLifecycle,
            Self::ApprovalMediation,
            Self::StreamingEvents,
            Self::UsageAccounting,
            Self::Cancellation,
            Self::Resumability,
            Self::CanonicalPatch,
            Self::ReviewIsolation,
            Self::AuthenticationProfiles,
            Self::WorkspaceIsolation,
            Self::PanicContainment,
            Self::DependencyCompatibility,
            Self::LicenseCompatibility,
        ])
    }
}

/// Promotion evidence is separate from the launch contract. A worker may only
/// select an adapter when an exact contract digest has passed every dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAdapterQualification {
    pub schema_version: u16,
    pub adapter_id: String,
    pub adapter_version: String,
    pub status: CodingAdapterQualificationStatus,
    pub evaluated_dimensions: BTreeSet<CodingAdapterQualificationDimension>,
    pub contract_digest: Option<String>,
    pub evidence_digest: String,
}

impl CodingAdapterQualification {
    pub fn validate(&self) -> Result<(), CodingError> {
        let dimensions = CodingAdapterQualificationDimension::required();
        if self.schema_version != CODING_ADAPTER_QUALIFICATION_VERSION
            || !safe_identifier(&self.adapter_id)
            || !safe_identifier(&self.adapter_version)
            || self.evaluated_dimensions.is_empty()
            || !self.evaluated_dimensions.is_subset(&dimensions)
            || !canonical_sha256(&self.evidence_digest)
            || self
                .contract_digest
                .as_deref()
                .is_some_and(|digest| !canonical_sha256(digest))
        {
            return Err(CodingError::AdapterQualification);
        }
        if self.status == CodingAdapterQualificationStatus::Qualified
            && (self.evaluated_dimensions != dimensions || self.contract_digest.is_none())
        {
            return Err(CodingError::AdapterQualification);
        }
        if self.status == CodingAdapterQualificationStatus::PrototypeOnly
            && self.contract_digest.is_some()
        {
            return Err(CodingError::AdapterQualification);
        }
        Ok(())
    }

    pub fn require_selectable(&self, contract: &CodingAdapterContract) -> Result<(), CodingError> {
        self.validate()?;
        let digest = contract.digest()?;
        if self.status != CodingAdapterQualificationStatus::Qualified
            || self.adapter_id != contract.adapter_id
            || self.adapter_version != contract.adapter_version
            || self.contract_digest.as_deref() != Some(digest.as_str())
        {
            return Err(CodingError::AdapterNotQualified);
        }
        Ok(())
    }

    pub fn codex_embedded_prototype(evidence_digest: impl Into<String>) -> Self {
        Self {
            schema_version: CODING_ADAPTER_QUALIFICATION_VERSION,
            adapter_id: CODEX_EMBEDDED_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            status: CodingAdapterQualificationStatus::PrototypeOnly,
            evaluated_dimensions: BTreeSet::from([
                CodingAdapterQualificationDimension::DependencyCompatibility,
                CodingAdapterQualificationDimension::LicenseCompatibility,
            ]),
            contract_digest: None,
            evidence_digest: evidence_digest.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingAuthenticationProfile {
    PersonalSubscription,
    EnterpriseApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingCredentialSource {
    NativeCodexStore,
    AttemptBroker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAuthenticationEvidence {
    pub profile: CodingAuthenticationProfile,
    pub credential_source: CodingCredentialSource,
    pub credential_generation: Option<u64>,
    pub authoritative_usage: bool,
}

impl CodingAuthenticationEvidence {
    pub fn validate(&self) -> Result<(), CodingError> {
        let valid = match (self.profile, self.credential_source) {
            (
                CodingAuthenticationProfile::PersonalSubscription,
                CodingCredentialSource::NativeCodexStore,
            ) => self.credential_generation.is_none() && !self.authoritative_usage,
            (CodingAuthenticationProfile::EnterpriseApi, CodingCredentialSource::AttemptBroker) => {
                self.credential_generation
                    .is_some_and(|generation| generation > 0)
                    && self.authoritative_usage
            }
            _ => false,
        };
        valid
            .then_some(())
            .ok_or(CodingError::AuthenticationProfile)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingRole {
    #[default]
    Implement,
    Review,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingWorkspaceAuthority {
    BoundedWrite,
    ReviewReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingRoleExecutionProfile {
    pub profile_id: String,
    pub model_alias: String,
    pub workspace_authority: CodingWorkspaceAuthority,
}

impl CodingRoleExecutionProfile {
    pub fn pinned(role: CodingRole) -> Self {
        match role {
            CodingRole::Implement => Self {
                profile_id: CODING_IMPLEMENT_PROFILE_ID.into(),
                model_alias: CODING_IMPLEMENTER_ALIAS.into(),
                workspace_authority: CodingWorkspaceAuthority::BoundedWrite,
            },
            CodingRole::Review => Self {
                profile_id: CODING_REVIEW_PROFILE_ID.into(),
                model_alias: CODING_REVIEWER_ALIAS.into(),
                workspace_authority: CodingWorkspaceAuthority::ReviewReadOnly,
            },
        }
    }

    pub fn validate(&self, role: CodingRole) -> Result<(), CodingError> {
        if self != &Self::pinned(role) {
            return Err(CodingError::RoleProfile);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingAdapter {
    CodexJsonl,
    ClaudeStreamJson,
    GeminiJson,
    KiloJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingAdapterDisposition {
    DeprecatedCompatibilityOnly,
}

impl CodingAdapter {
    pub fn id(self) -> &'static str {
        match self {
            Self::CodexJsonl => "codex-jsonl",
            Self::ClaudeStreamJson => "claude-stream-json",
            Self::GeminiJson => "gemini-json",
            Self::KiloJson => "kilo-json",
        }
    }
    pub fn validate_launch(self, args: &[String]) -> Result<(), CodingError> {
        let forbidden = [
            "--dangerously-skip-permissions",
            "--yolo",
            "--approval-mode=full-auto",
            "--trust-all-tools",
            "--auto-approve",
        ];
        if args.iter().any(|a| forbidden.iter().any(|f| a == f)) {
            return Err(CodingError::PermissionBypass);
        }
        let structured = match self {
            Self::CodexJsonl => args.iter().any(|a| a == "--json"),
            Self::ClaudeStreamJson => args
                .windows(2)
                .any(|a| a == ["--output-format", "stream-json"]),
            Self::GeminiJson | Self::KiloJson => args.iter().any(|a| a == "--output-format=json"),
        };
        structured.then_some(()).ok_or(CodingError::Unstructured)
    }

    pub fn disposition(self) -> CodingAdapterDisposition {
        match self {
            Self::CodexJsonl | Self::ClaudeStreamJson | Self::GeminiJson | Self::KiloJson => {
                CodingAdapterDisposition::DeprecatedCompatibilityOnly
            }
        }
    }
}

pub fn parse_adapter_event(
    adapter: CodingAdapter,
    line: &str,
) -> Result<serde_json::Value, CodingError> {
    let value: serde_json::Value =
        serde_json::from_str(line).map_err(|_| CodingError::Unstructured)?;
    if !value.is_object() {
        return Err(CodingError::Unstructured);
    }
    let kind = value
        .get("type")
        .or_else(|| value.get("event"))
        .and_then(|v| v.as_str())
        .ok_or(CodingError::Unstructured)?;
    if kind.len() > 64 || value.to_string().len() > 1024 * 1024 {
        return Err(CodingError::Unstructured);
    }
    Ok(serde_json::json!({"adapter":adapter.id(),"kind":kind,"payload":value}))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingAdapterContract {
    pub schema_version: u16,
    pub adapter_id: String,
    pub adapter_version: String,
    pub adapter_protocol_version: String,
    pub action_kind: String,
    pub compatibility_digest: String,
    pub image_digest: String,
    pub capability_digest: String,
    pub template_id: String,
    pub template_version: u32,
    pub template_digest: String,
    pub executable: String,
    pub binary_digest: String,
    pub schema_digest: String,
    pub required_features: BTreeSet<String>,
}

impl CodingAdapterContract {
    pub fn validate(&self) -> Result<(), CodingError> {
        if self.schema_version != CODING_ADAPTER_CONTRACT_VERSION
            || !safe_identifier(&self.adapter_id)
            || !safe_identifier(&self.adapter_version)
            || !safe_identifier(&self.adapter_protocol_version)
            || !safe_identifier(&self.action_kind)
            || !safe_identifier(&self.template_id)
            || self.template_version == 0
            || !self.executable.starts_with("/usr/local/bin/")
            || self.executable.contains("..")
            || self.required_features.is_empty()
            || self
                .required_features
                .iter()
                .any(|value| !safe_identifier(value))
            || [
                &self.compatibility_digest,
                &self.image_digest,
                &self.capability_digest,
                &self.template_digest,
                &self.binary_digest,
                &self.schema_digest,
            ]
            .into_iter()
            .any(|value| !canonical_sha256(value))
        {
            return Err(CodingError::AdapterContract);
        }
        Ok(())
    }

    pub fn digest(&self) -> Result<String, CodingError> {
        self.validate()?;
        canonical_digest(self).map_err(|_| CodingError::AdapterContract)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ValidationStatus {
    Passed,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingValidationEvidence {
    pub command: Vec<String>,
    pub status: ValidationStatus,
    pub exit_code: Option<i32>,
    pub artifact_digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingImplementationArtifact {
    pub schema_version: u16,
    pub adapter_contract_digest: String,
    pub repository_digest: String,
    pub base_revision: String,
    pub patch_digest: String,
    pub changed_paths: BTreeSet<String>,
    pub validation_evidence: Vec<CodingValidationEvidence>,
    #[serde(default)]
    pub resolved_finding_ids: BTreeSet<String>,
}

impl CodingImplementationArtifact {
    pub fn validate(&self) -> Result<(), CodingError> {
        if self.schema_version != CODING_ARTIFACT_SCHEMA_VERSION
            || !canonical_sha256(&self.adapter_contract_digest)
            || !canonical_sha256(&self.repository_digest)
            || !canonical_sha256(&self.patch_digest)
            || !valid_revision(&self.base_revision)
            || self.changed_paths.is_empty()
            || self
                .changed_paths
                .iter()
                .any(|path| !safe_relative_path(path))
            || self.validation_evidence.iter().any(|evidence| {
                evidence.command.is_empty()
                    || evidence.command.iter().any(|part| part.is_empty())
                    || !valid_validation_outcome(evidence.status, evidence.exit_code)
                    || evidence
                        .artifact_digest
                        .as_ref()
                        .is_some_and(|digest| !canonical_sha256(digest))
            })
            || self
                .resolved_finding_ids
                .iter()
                .any(|finding_id| !safe_identifier(finding_id))
        {
            return Err(CodingError::Artifact);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewSeverity {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewVerdict {
    Approved,
    ChangesRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingReviewFinding {
    pub finding_id: String,
    pub severity: ReviewSeverity,
    pub repository: String,
    pub location: String,
    pub summary: String,
    pub evidence: String,
    pub required_resolution: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingReviewResult {
    pub schema_version: u16,
    pub review_id: String,
    pub artifact_digest: String,
    pub verdict: ReviewVerdict,
    pub findings: Vec<CodingReviewFinding>,
    pub validation_gaps: Vec<String>,
}

impl CodingReviewResult {
    pub fn validate(&self) -> Result<(), CodingError> {
        let mut finding_ids = BTreeSet::new();
        if self.schema_version != CODING_ARTIFACT_SCHEMA_VERSION
            || !safe_identifier(&self.review_id)
            || !canonical_sha256(&self.artifact_digest)
            || self.findings.iter().any(|finding| {
                !safe_identifier(&finding.finding_id)
                    || !finding_ids.insert(finding.finding_id.clone())
                    || !safe_repository(&finding.repository)
                    || !safe_relative_path(&finding.location)
                    || finding.summary.trim().is_empty()
                    || finding.evidence.trim().is_empty()
                    || finding.required_resolution.trim().is_empty()
            })
            || match self.verdict {
                ReviewVerdict::Approved => !self.findings.is_empty(),
                ReviewVerdict::ChangesRequired => self.findings.is_empty(),
            }
        {
            return Err(CodingError::Artifact);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingRemediationInput {
    pub prior_review: CodingReviewResult,
}

impl CodingRemediationInput {
    pub fn validate(&self) -> Result<(), CodingError> {
        self.prior_review.validate()?;
        if self.prior_review.verdict != ReviewVerdict::ChangesRequired {
            return Err(CodingError::ReviewLoop);
        }
        Ok(())
    }

    pub fn finding_ids(&self) -> BTreeSet<String> {
        self.prior_review
            .findings
            .iter()
            .map(|finding| finding.finding_id.clone())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingReviewInput {
    pub review_id: String,
    pub repository: String,
    pub requirements: String,
    pub requirements_digest: String,
    pub candidate_patch: String,
    pub implementation: CodingImplementationArtifact,
    #[serde(default)]
    pub prior_review: Option<CodingReviewResult>,
}

impl CodingReviewInput {
    pub fn validate(&self, spec: &CodingTurnSpec) -> Result<(), CodingError> {
        self.implementation.validate()?;
        if !safe_identifier(&self.review_id)
            || !safe_repository(&self.repository)
            || self.requirements.trim().is_empty()
            || self.requirements.len() > 256 * 1024
            || self.requirements_digest != patch_digest(&self.requirements)
            || self.implementation.repository_digest != spec.repository_digest
            || self.implementation.base_revision != spec.base_revision
            || self.implementation.patch_digest != patch_digest(&self.candidate_patch)
            || self.candidate_patch.len() as u64 > spec.maximum_patch_bytes
            || parse_patch_paths(&self.candidate_patch)? != self.implementation.changed_paths
        {
            return Err(CodingError::ReviewLoop);
        }
        if let Some(prior) = &self.prior_review {
            prior.validate()?;
            if prior.verdict != ReviewVerdict::ChangesRequired
                || prior.artifact_digest == self.implementation.patch_digest
                || self.implementation.resolved_finding_ids
                    != prior
                        .findings
                        .iter()
                        .map(|finding| finding.finding_id.clone())
                        .collect()
            {
                return Err(CodingError::ReviewLoop);
            }
        } else if !self.implementation.resolved_finding_ids.is_empty() {
            return Err(CodingError::ReviewLoop);
        }
        Ok(())
    }
}

pub fn authorize_reviewed_publish(
    implementation: &CodingImplementationArtifact,
    review: &CodingReviewResult,
) -> Result<(), CodingError> {
    implementation.validate()?;
    review.validate()?;
    if review.verdict != ReviewVerdict::Approved
        || review.artifact_digest != implementation.patch_digest
    {
        return Err(CodingError::PublishBlocked);
    }
    Ok(())
}

fn canonical_sha256(value: &str) -> bool {
    value
        .strip_prefix("sha256:")
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_revision(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_validation_outcome(status: ValidationStatus, exit_code: Option<i32>) -> bool {
    match (status, exit_code) {
        (ValidationStatus::Passed, Some(0)) | (ValidationStatus::NotRun, None) => true,
        (ValidationStatus::Failed, Some(code)) => code != 0,
        _ => false,
    }
}

fn safe_repository(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(owner), Some(repository), None)
        if safe_identifier(owner) && safe_identifier(repository))
}

fn safe_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingTurnSpec {
    pub repository_digest: String,
    pub base_revision: String,
    pub workspace_root: String,
    pub prompt: String,
    pub model_alias: String,
    pub authentication_profile: CodingAuthenticationProfile,
    #[serde(default)]
    pub role: CodingRole,
    pub role_profile: CodingRoleExecutionProfile,
    #[serde(default)]
    pub review_input: Option<Box<CodingReviewInput>>,
    #[serde(default)]
    pub remediation: Option<Box<CodingRemediationInput>>,
    pub materialization_manifest_digest: String,
    pub writable_roots: BTreeSet<String>,
    pub allowed_tools: BTreeSet<String>,
    pub maximum_patch_bytes: u64,
    pub maximum_changed_files: usize,
}

impl CodingTurnSpec {
    pub fn supported_tools(role: CodingRole) -> BTreeSet<String> {
        match role {
            CodingRole::Implement => BTreeSet::from([
                "fs.read".to_string(),
                "fs.write".to_string(),
                "process.exec".to_string(),
            ]),
            CodingRole::Review => {
                BTreeSet::from(["fs.read".to_string(), "process.exec".to_string()])
            }
        }
    }

    pub fn validate(&self) -> Result<(), CodingError> {
        if !self.workspace_root.starts_with("/workspace/")
            || self.workspace_root.contains("..")
            || self.prompt.is_empty()
            || self.prompt.len() > 64 * 1024
        {
            return Err(CodingError::Spec);
        }
        self.role_profile.validate(self.role)?;
        if self.model_alias != self.role_profile.model_alias {
            return Err(CodingError::RoleProfile);
        }
        if self.allowed_tools != Self::supported_tools(self.role)
            || self.writable_roots.is_empty()
            || self.writable_roots.iter().any(|root| {
                !root.starts_with('/')
                    || root.contains("..")
                    || (self.role == CodingRole::Implement
                        && root != &self.workspace_root
                        && !root.starts_with(&format!("{}/", self.workspace_root)))
            })
        {
            return Err(CodingError::RoleProfile);
        }
        match self.role {
            CodingRole::Implement => {
                if self.review_input.is_some() {
                    return Err(CodingError::ReviewLoop);
                }
                if let Some(remediation) = &self.remediation {
                    remediation.validate()?;
                }
            }
            CodingRole::Review => {
                if self.remediation.is_some()
                    || self
                        .writable_roots
                        .iter()
                        .any(|root| root == &self.workspace_root)
                    || self.allowed_tools.iter().any(|tool| {
                        matches!(tool.as_str(), "fs.write" | "patch.apply" | "git.commit")
                    })
                {
                    return Err(CodingError::RoleProfile);
                }
                self.review_input
                    .as_deref()
                    .ok_or(CodingError::ReviewLoop)?
                    .validate(self)?;
            }
        }
        if !matches!(self.base_revision.len(), 40 | 64)
            || !self.base_revision.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(CodingError::Spec);
        }
        if self.maximum_patch_bytes == 0
            || self.maximum_patch_bytes > MAX_INLINE_PATCH_BYTES
            || self.maximum_changed_files == 0
            || self.maximum_changed_files > 4096
        {
            return Err(CodingError::Spec);
        }
        Ok(())
    }
    pub fn digest(&self) -> Result<String, serde_json::Error> {
        canonical_digest(self)
    }
    pub fn encode_argument(&self) -> Result<String, CodingError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| CodingError::Spec)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
    }
    pub fn decode_argument(value: &str) -> Result<Self, CodingError> {
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| CodingError::Spec)?;
        let request: Self = serde_json::from_slice(&json).map_err(|_| CodingError::Spec)?;
        request.validate()?;
        Ok(request)
    }

    fn permits_changed_path(&self, path: &str) -> bool {
        let logical_path = format!("{}/{}", self.workspace_root.trim_end_matches('/'), path);
        self.writable_roots.iter().any(|root| {
            logical_path == *root
                || logical_path.starts_with(&format!("{}/", root.trim_end_matches('/')))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImmutableRepositoryInput {
    pub artifact_uri: String,
    pub digest: String,
    pub size: u64,
    pub media_type: String,
}

impl ImmutableRepositoryInput {
    pub fn validate(&self, spec: &CodingTurnSpec) -> Result<(), CodingError> {
        let digest = self.digest.strip_prefix("sha256:").unwrap_or_default();
        if self.digest != spec.repository_digest
            || self.size == 0
            || self.size > i64::MAX as u64
            || self.media_type != "application/x-git-bundle"
            || digest.len() != 64
            || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(CodingError::Repository);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingFixtureRequest {
    pub spec: CodingTurnSpec,
    pub target_path: String,
    pub expected_text: String,
    pub replacement_text: String,
}

impl CodingFixtureRequest {
    pub fn validate(&self) -> Result<(), CodingError> {
        self.spec.validate()?;
        let admitted_workspace = BTreeSet::from([self.spec.workspace_root.clone()]);
        let admitted_tools = CodingTurnSpec::supported_tools(CodingRole::Implement);
        if self.target_path.is_empty()
            || self.target_path.starts_with('/')
            || self
                .target_path
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
            || self.expected_text.is_empty()
            || self.expected_text == self.replacement_text
            || self
                .expected_text
                .len()
                .saturating_add(self.replacement_text.len())
                > 1024 * 1024
            || self.spec.writable_roots != admitted_workspace
            || self.spec.allowed_tools != admitted_tools
        {
            return Err(CodingError::Spec);
        }
        Ok(())
    }

    pub fn encode_argument(&self) -> Result<String, CodingError> {
        self.validate()?;
        let json = serde_json::to_vec(self).map_err(|_| CodingError::Spec)?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
    }

    pub fn decode_argument(value: &str) -> Result<Self, CodingError> {
        let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| CodingError::Spec)?;
        let request: Self = serde_json::from_slice(&json).map_err(|_| CodingError::Spec)?;
        request.validate()?;
        Ok(request)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingFixtureOutput {
    pub adapter_id: String,
    pub adapter_version: String,
    pub repository_digest: String,
    pub base_revision: String,
    pub patch: String,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ValidatedPatch {
    pub base_revision: String,
    pub patch: String,
    pub patch_digest: String,
    pub changed_paths: BTreeSet<String>,
}

pub fn validate_patch(
    spec: &CodingTurnSpec,
    policy: &ProtectedPathPolicy,
    base_revision: &str,
    patch: &str,
    reported_paths: &[String],
) -> Result<ValidatedPatch, CodingError> {
    spec.validate()?;
    if base_revision != spec.base_revision
        || patch.len() as u64 > spec.maximum_patch_bytes
        || reported_paths.len() > spec.maximum_changed_files
    {
        return Err(CodingError::Patch);
    }
    let mut paths = BTreeSet::new();
    for path in reported_paths {
        if spec.role != CodingRole::Implement || !spec.permits_changed_path(path) {
            return Err(CodingError::Protected);
        }
        policy
            .validate_changes([path.as_str()])
            .map_err(|_| CodingError::Protected)?;
        if !paths.insert(path.clone()) {
            return Err(CodingError::Patch);
        }
    }
    let parsed = parse_patch_paths(patch)?;
    if parsed != paths {
        return Err(CodingError::Tampered);
    }
    Ok(ValidatedPatch {
        base_revision: base_revision.into(),
        patch: patch.into(),
        patch_digest: patch_digest(patch),
        changed_paths: paths,
    })
}

pub fn patch_digest(patch: &str) -> String {
    format!("sha256:{:x}", sha2::Sha256::digest(patch.as_bytes()))
}

fn parse_patch_paths(patch: &str) -> Result<BTreeSet<String>, CodingError> {
    let mut paths = BTreeSet::new();
    for line in patch.lines().filter(|l| l.starts_with("+++ b/")) {
        let p = &line[6..];
        if p.is_empty() || p.contains("..") || p.starts_with('/') {
            return Err(CodingError::Patch);
        }
        paths.insert(p.into());
    }
    if paths.is_empty() {
        return Err(CodingError::Patch);
    }
    Ok(paths)
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CodingError {
    #[error("invalid coding turn specification")]
    Spec,
    #[error("invalid patch artifact")]
    Patch,
    #[error("protected path change")]
    Protected,
    #[error("reported paths differ from canonical patch")]
    Tampered,
    #[error("adapter launch requests a permission bypass")]
    PermissionBypass,
    #[error("adapter output is not a pinned structured protocol")]
    Unstructured,
    #[error("invalid immutable repository input")]
    Repository,
    #[error("invalid coding adapter contract")]
    AdapterContract,
    #[error("invalid coding adapter qualification evidence")]
    AdapterQualification,
    #[error("coding adapter is not independently qualified")]
    AdapterNotQualified,
    #[error("invalid coding artifact")]
    Artifact,
    #[error("invalid immutable coding role profile")]
    RoleProfile,
    #[error("invalid immutable coding authentication profile")]
    AuthenticationProfile,
    #[error("invalid coding review or remediation chain")]
    ReviewLoop,
    #[error("coding publication is blocked by review")]
    PublishBlocked,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    fn spec() -> CodingTurnSpec {
        CodingTurnSpec {
            repository_digest: format!("sha256:{:064x}", 1),
            base_revision: "a".repeat(40),
            workspace_root: "/workspace/repo".into(),
            prompt: "fix".into(),
            model_alias: CODING_IMPLEMENTER_ALIAS.into(),
            authentication_profile: CodingAuthenticationProfile::EnterpriseApi,
            role: CodingRole::Implement,
            role_profile: CodingRoleExecutionProfile::pinned(CodingRole::Implement),
            review_input: None,
            remediation: None,
            materialization_manifest_digest: format!("sha256:{:064x}", 2),
            writable_roots: BTreeSet::from(["/workspace/repo".into()]),
            allowed_tools: CodingTurnSpec::supported_tools(CodingRole::Implement),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 10,
        }
    }
    #[test]
    fn canonical_patch_rejects_protected_and_tampered_reports() {
        let p = ProtectedPathPolicy::default_deny();
        let s = spec();
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(validate_patch(&s, &p, &s.base_revision, patch, &["src/lib.rs".into()]).is_ok());
        assert_eq!(
            validate_patch(&s, &p, &s.base_revision, patch, &["other".into()]),
            Err(CodingError::Tampered)
        );
        let ci = patch.replace("src/lib.rs", ".github/workflows/release.yml");
        assert_eq!(
            validate_patch(
                &s,
                &p,
                &s.base_revision,
                &ci,
                &[".github/workflows/release.yml".into()]
            ),
            Err(CodingError::Protected)
        );
        let mut source_only = s.clone();
        source_only.writable_roots = BTreeSet::from(["/workspace/repo/src".into()]);
        let outside = patch.replace("src/lib.rs", "Cargo.toml");
        assert_eq!(
            validate_patch(
                &source_only,
                &p,
                &source_only.base_revision,
                &outside,
                &["Cargo.toml".into()]
            ),
            Err(CodingError::Protected)
        );
        let mut missing_tool = s;
        missing_tool.allowed_tools.remove("process.exec");
        assert_eq!(missing_tool.validate(), Err(CodingError::RoleProfile));
    }

    #[test]
    fn coding_spec_argument_round_trips_through_closed_encoding() {
        let expected = spec();
        assert_eq!(
            CodingTurnSpec::decode_argument(&expected.encode_argument().unwrap()).unwrap(),
            expected
        );
        assert!(CodingTurnSpec::decode_argument("not-base64").is_err());
        let mut oversized = expected;
        oversized.maximum_patch_bytes = MAX_INLINE_PATCH_BYTES + 1;
        assert_eq!(oversized.validate(), Err(CodingError::Spec));
    }
    #[test]
    fn adapters_require_machine_protocols_and_forbid_bypass() {
        assert_eq!(
            CodingAdapter::CodexJsonl.disposition(),
            CodingAdapterDisposition::DeprecatedCompatibilityOnly
        );
        assert!(
            CodingAdapter::CodexJsonl
                .validate_launch(&["--json".into()])
                .is_ok()
        );
        assert_eq!(
            CodingAdapter::ClaudeStreamJson.validate_launch(&[
                "--dangerously-skip-permissions".into(),
                "--output-format".into(),
                "stream-json".into()
            ]),
            Err(CodingError::PermissionBypass)
        );
        assert!(parse_adapter_event(CodingAdapter::GeminiJson, r#"{"type":"progress"}"#).is_ok());
        assert_eq!(
            parse_adapter_event(CodingAdapter::KiloJson, "decorated terminal output"),
            Err(CodingError::Unstructured)
        );
    }

    fn adapter_contract() -> CodingAdapterContract {
        CodingAdapterContract {
            schema_version: CODING_ADAPTER_CONTRACT_VERSION,
            adapter_id: CODEX_APP_SERVER_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            adapter_protocol_version: CODEX_APP_SERVER_PROTOCOL_VERSION.into(),
            action_kind: "coding.codex-app-server-v1".into(),
            compatibility_digest: format!("sha256:{:064x}", 3),
            image_digest: format!("sha256:{:064x}", 4),
            capability_digest: format!("sha256:{:064x}", 5),
            template_id: "coding-codex-app-server-v1".into(),
            template_version: 1,
            template_digest: format!("sha256:{:064x}", 6),
            executable: "/usr/local/bin/codex".into(),
            binary_digest: CODEX_APP_SERVER_BINARY_DIGEST.into(),
            schema_digest: CODEX_APP_SERVER_SCHEMA_DIGEST.into(),
            required_features: BTreeSet::from([
                "canonical-patch-output".into(),
                "deny-all-egress".into(),
            ]),
        }
    }

    #[test]
    fn adapter_contract_is_closed_versioned_and_digest_bound() {
        let contract = adapter_contract();
        assert!(contract.validate().is_ok());
        assert!(contract.digest().unwrap().starts_with("sha256:"));

        let mut wrong = serde_json::to_value(&contract).unwrap();
        wrong
            .as_object_mut()
            .unwrap()
            .insert("untrustedLaunchFlag".into(), serde_json::Value::Bool(true));
        assert!(serde_json::from_value::<CodingAdapterContract>(wrong).is_err());

        let mut wrong = contract;
        wrong.image_digest = "sha256:not-canonical".into();
        assert_eq!(wrong.validate(), Err(CodingError::AdapterContract));
    }

    #[test]
    fn optional_adapters_are_fail_closed_until_independently_qualified() {
        let contract = adapter_contract();
        let evidence_digest = format!("sha256:{:064x}", 9);
        let qualified = CodingAdapterQualification {
            schema_version: CODING_ADAPTER_QUALIFICATION_VERSION,
            adapter_id: CODEX_APP_SERVER_ADAPTER_ID.into(),
            adapter_version: CODEX_APP_SERVER_VERSION.into(),
            status: CodingAdapterQualificationStatus::Qualified,
            evaluated_dimensions: CodingAdapterQualificationDimension::required(),
            contract_digest: Some(contract.digest().unwrap()),
            evidence_digest,
        };
        assert!(qualified.require_selectable(&contract).is_ok());

        let embedded = CodingAdapterQualification::codex_embedded_prototype(
            CODEX_EMBEDDED_PROTOTYPE_EVIDENCE_DIGEST,
        );
        assert!(embedded.validate().is_ok());
        assert_eq!(embedded.adapter_id, CODEX_EMBEDDED_ADAPTER_ID);
        assert_eq!(CODEX_EMBEDDED_UPSTREAM_REVISION.len(), 40);
        assert_eq!(
            embedded.require_selectable(&contract),
            Err(CodingError::AdapterNotQualified)
        );

        let mut incomplete = qualified;
        incomplete
            .evaluated_dimensions
            .remove(&CodingAdapterQualificationDimension::PanicContainment);
        assert_eq!(
            incomplete.validate(),
            Err(CodingError::AdapterQualification)
        );
    }

    #[test]
    fn implementation_and_review_artifacts_enforce_closed_shapes() {
        let implementation = CodingImplementationArtifact {
            schema_version: CODING_ARTIFACT_SCHEMA_VERSION,
            adapter_contract_digest: format!("sha256:{:064x}", 1),
            repository_digest: format!("sha256:{:064x}", 1),
            base_revision: "a".repeat(40),
            patch_digest: format!("sha256:{:064x}", 3),
            changed_paths: BTreeSet::from(["src/lib.rs".into()]),
            validation_evidence: vec![CodingValidationEvidence {
                command: vec!["cargo".into(), "test".into()],
                status: ValidationStatus::Passed,
                exit_code: Some(0),
                artifact_digest: Some(format!("sha256:{:064x}", 4)),
            }],
            resolved_finding_ids: BTreeSet::new(),
        };
        assert!(implementation.validate().is_ok());

        let mut inconsistent_evidence = implementation.clone();
        inconsistent_evidence.validation_evidence[0].exit_code = Some(1);
        assert_eq!(inconsistent_evidence.validate(), Err(CodingError::Artifact));

        let review = CodingReviewResult {
            schema_version: CODING_ARTIFACT_SCHEMA_VERSION,
            review_id: "review-1".into(),
            artifact_digest: implementation.patch_digest.clone(),
            verdict: ReviewVerdict::ChangesRequired,
            findings: vec![CodingReviewFinding {
                finding_id: "CODE-1".into(),
                severity: ReviewSeverity::High,
                repository: "networknt/light-fabric".into(),
                location: "src/lib.rs".into(),
                summary: "missing bound".into(),
                evidence: "input is unchecked".into(),
                required_resolution: "validate input".into(),
            }],
            validation_gaps: vec!["live provider not exercised".into()],
        };
        assert!(review.validate().is_ok());

        let mut approved_with_findings = review;
        approved_with_findings.verdict = ReviewVerdict::Approved;
        assert_eq!(
            approved_with_findings.validate(),
            Err(CodingError::Artifact)
        );
    }

    #[test]
    fn immutable_role_profiles_review_reconstruction_and_publish_gate_fail_closed() {
        let patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let implementation = CodingImplementationArtifact {
            schema_version: CODING_ARTIFACT_SCHEMA_VERSION,
            adapter_contract_digest: format!("sha256:{:064x}", 1),
            repository_digest: format!("sha256:{:064x}", 1),
            base_revision: "a".repeat(40),
            patch_digest: patch_digest(patch),
            changed_paths: BTreeSet::from(["src/lib.rs".into()]),
            validation_evidence: Vec::new(),
            resolved_finding_ids: BTreeSet::new(),
        };
        let mut review_spec = spec();
        review_spec.role = CodingRole::Review;
        review_spec.role_profile = CodingRoleExecutionProfile::pinned(CodingRole::Review);
        review_spec.model_alias = CODING_REVIEWER_ALIAS.into();
        review_spec.writable_roots = BTreeSet::from(["/workspace/review-scratch".into()]);
        review_spec.allowed_tools = BTreeSet::from(["fs.read".into(), "process.exec".into()]);
        review_spec.review_input = Some(Box::new(CodingReviewInput {
            review_id: "review-1".into(),
            repository: "networknt/light-fabric".into(),
            requirements: "require bounded input".into(),
            requirements_digest: patch_digest("require bounded input"),
            candidate_patch: patch.into(),
            implementation: implementation.clone(),
            prior_review: None,
        }));
        assert!(review_spec.validate().is_ok());
        let mut leaked =
            serde_json::to_value(review_spec.review_input.as_deref().unwrap()).unwrap();
        leaked.as_object_mut().unwrap().insert(
            "implementerThreadId".into(),
            serde_json::json!("private-thread"),
        );
        assert!(serde_json::from_value::<CodingReviewInput>(leaked).is_err());

        let mut mutable_review = review_spec.clone();
        mutable_review
            .writable_roots
            .insert(mutable_review.workspace_root.clone());
        assert_eq!(mutable_review.validate(), Err(CodingError::RoleProfile));

        let blocking = CodingReviewResult {
            schema_version: CODING_ARTIFACT_SCHEMA_VERSION,
            review_id: "review-1".into(),
            artifact_digest: implementation.patch_digest.clone(),
            verdict: ReviewVerdict::ChangesRequired,
            findings: vec![CodingReviewFinding {
                finding_id: "CODE-1".into(),
                severity: ReviewSeverity::High,
                repository: "networknt/light-fabric".into(),
                location: "src/lib.rs".into(),
                summary: "missing bound".into(),
                evidence: "input is unchecked".into(),
                required_resolution: "validate input".into(),
            }],
            validation_gaps: Vec::new(),
        };
        assert_eq!(
            authorize_reviewed_publish(&implementation, &blocking),
            Err(CodingError::PublishBlocked)
        );

        let approved = CodingReviewResult {
            verdict: ReviewVerdict::Approved,
            findings: Vec::new(),
            ..blocking.clone()
        };
        assert!(authorize_reviewed_publish(&implementation, &approved).is_ok());

        let mut remediation_spec = spec();
        remediation_spec.remediation = Some(Box::new(CodingRemediationInput {
            prior_review: blocking.clone(),
        }));
        assert!(remediation_spec.validate().is_ok());
        let remediated_patch = "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-a\n+c\n";
        let mut remediated = implementation;
        remediated.patch_digest = patch_digest(remediated_patch);
        remediated.resolved_finding_ids = BTreeSet::from(["CODE-1".into()]);
        let review_input = CodingReviewInput {
            review_id: "review-2".into(),
            repository: "networknt/light-fabric".into(),
            requirements: "require bounded input".into(),
            requirements_digest: patch_digest("require bounded input"),
            candidate_patch: remediated_patch.into(),
            implementation: remediated,
            prior_review: Some(blocking),
        };
        assert!(review_input.validate(&review_spec).is_ok());
    }
}
