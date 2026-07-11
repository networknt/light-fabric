use execution_security::ProtectedPathPolicy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HighValueActionKind {
    CreateBranch,
    OpenPr,
    PushCommit,
    Publish,
    Sign,
    Deploy,
    SendEmail,
    CreateEvent,
    Payment,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HighValueActionRequest {
    pub action_id: Uuid,
    pub kind: HighValueActionKind,
    pub immutable_input_digest: String,
    pub target_digest: String,
    pub policy_digest: String,
    pub provenance_digest: String,
    pub approval_id: Uuid,
    pub approval_input_digest: String,
    pub approval_target_digest: String,
    pub approval_policy_digest: String,
    pub approval_nonce_digest: String,
    pub approval_expires_at: chrono::DateTime<chrono::Utc>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredActionPlan {
    pub operation: String,
    pub immutable_input_digest: String,
    pub target_digest: String,
    pub credential_scope: String,
}
pub fn authorize_high_value_action(
    r: &HighValueActionRequest,
    allowed: &[HighValueActionKind],
) -> Result<StructuredActionPlan, FixedActionError> {
    if !allowed.contains(&r.kind) || r.approval_expires_at <= chrono::Utc::now() {
        return Err(FixedActionError::Approval);
    }
    if r.immutable_input_digest != r.approval_input_digest
        || r.target_digest != r.approval_target_digest
        || r.policy_digest != r.approval_policy_digest
        || r.approval_nonce_digest.len() < 32
    {
        return Err(FixedActionError::Approval);
    }
    Ok(StructuredActionPlan {
        operation: format!("{:?}", r.kind),
        immutable_input_digest: r.immutable_input_digest.clone(),
        target_digest: r.target_digest.clone(),
        credential_scope: format!("fixed:{}:{}", r.action_id, r.target_digest),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FixedPatchRequest {
    pub request_id: Uuid,
    pub repository: String,
    pub base_commit: String,
    pub target_branch: String,
    pub patch_artifact_ref: String,
    pub patch_digest: String,
    pub policy_digest: String,
    pub approval_id: Uuid,
    pub changed_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedPatchPlan {
    pub isolated_home: PathBuf,
    pub checkout: Vec<String>,
    pub check: Vec<String>,
    pub apply: Vec<String>,
    pub environment: BTreeMap<String, String>,
}

pub fn validate_and_plan(
    request: &FixedPatchRequest,
    patch_bytes: &[u8],
    allowed_repository: &str,
    allowed_branch_prefix: &str,
    workspace: PathBuf,
    protected: &ProtectedPathPolicy,
) -> Result<FixedPatchPlan, FixedActionError> {
    if request.repository != allowed_repository
        || !request.target_branch.starts_with(allowed_branch_prefix)
        || request.target_branch.contains("..")
        || request.target_branch.starts_with('-')
    {
        return Err(FixedActionError::Target);
    }
    if request.base_commit.len() != 40
        || !request.base_commit.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(FixedActionError::Commit);
    }
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(patch_bytes)));
    if digest != request.patch_digest {
        return Err(FixedActionError::Digest);
    }
    protected
        .validate_changes(request.changed_paths.iter().map(String::as_str))
        .map_err(|e| FixedActionError::Protected(e.to_string()))?;
    let home = workspace.join("home");
    let hooks = workspace.join("empty-hooks");
    let repo = workspace.join("checkout");
    let environment = BTreeMap::from([
        ("HOME".into(), home.display().to_string()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
    ]);
    let safe = vec![
        "-c".into(),
        format!("core.hooksPath={}", hooks.display()),
        "-c".into(),
        "filter.lfs.smudge=".into(),
        "-c".into(),
        "filter.lfs.required=false".into(),
        "-c".into(),
        "submodule.recurse=false".into(),
    ];
    let mut checkout = vec!["git".into()];
    checkout.extend(safe.clone());
    checkout.extend([
        "clone".into(),
        "--no-checkout".into(),
        "--filter=blob:none".into(),
        request.repository.clone(),
        repo.display().to_string(),
    ]);
    let mut check = vec!["git".into()];
    check.extend(safe.clone());
    check.extend([
        "-C".into(),
        repo.display().to_string(),
        "apply".into(),
        "--check".into(),
        "--index".into(),
        request.patch_artifact_ref.clone(),
    ]);
    let mut apply = vec!["git".into()];
    apply.extend(safe);
    apply.extend([
        "-C".into(),
        repo.display().to_string(),
        "apply".into(),
        "--index".into(),
        request.patch_artifact_ref.clone(),
    ]);
    Ok(FixedPatchPlan {
        isolated_home: home,
        checkout,
        check,
        apply,
        environment,
    })
}

pub fn verify_post_apply(
    request: &FixedPatchRequest,
    actual_patch: &[u8],
    actual_paths: &[String],
    protected: &ProtectedPathPolicy,
) -> Result<(), FixedActionError> {
    let digest = format!("sha256:{}", hex::encode(Sha256::digest(actual_patch)));
    if digest != request.patch_digest {
        return Err(FixedActionError::Digest);
    }
    let mut expected = request.changed_paths.clone();
    expected.sort();
    let mut actual = actual_paths.to_vec();
    actual.sort();
    if expected != actual {
        return Err(FixedActionError::ChangedPaths);
    }
    protected
        .validate_changes(actual.iter().map(String::as_str))
        .map_err(|e| FixedActionError::Protected(e.to_string()))
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FixedActionError {
    #[error("repository or target branch is not authorized")]
    Target,
    #[error("base commit must be an immutable full SHA-1")]
    Commit,
    #[error("patch digest mismatch")]
    Digest,
    #[error("post-apply changed paths differ from approved paths")]
    ChangedPaths,
    #[error("protected path denied: {0}")]
    Protected(String),
    #[error("fixed action approval is expired, mismatched, or unauthorized")]
    Approval,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_patch_rejects_tampering_and_protected_paths() {
        let bytes = b"patch";
        let mut request = FixedPatchRequest {
            request_id: Uuid::now_v7(),
            repository: "https://example/repo.git".into(),
            base_commit: "a".repeat(40),
            target_branch: "agent/fix".into(),
            patch_artifact_ref: "/inputs/change.patch".into(),
            patch_digest: format!("sha256:{}", hex::encode(Sha256::digest(bytes))),
            policy_digest: "sha256:p".into(),
            approval_id: Uuid::now_v7(),
            changed_paths: vec!["src/lib.rs".into()],
        };
        let policy = ProtectedPathPolicy::default_deny();
        assert!(
            validate_and_plan(
                &request,
                bytes,
                &request.repository,
                "agent/",
                PathBuf::from("/tmp/fixed"),
                &policy
            )
            .is_ok()
        );
        assert!(verify_post_apply(&request, b"tampered", &request.changed_paths, &policy).is_err());
        request.changed_paths = vec![".github/workflows/release.yml".into()];
        assert!(
            validate_and_plan(
                &request,
                bytes,
                &request.repository,
                "agent/",
                PathBuf::from("/tmp/fixed"),
                &policy
            )
            .is_err()
        );
    }
    #[test]
    fn high_value_action_binds_exact_input_target_policy_and_expiry() {
        let now = chrono::Utc::now() + chrono::Duration::minutes(1);
        let mut r = HighValueActionRequest {
            action_id: Uuid::new_v4(),
            kind: HighValueActionKind::OpenPr,
            immutable_input_digest: "input".into(),
            target_digest: "target".into(),
            policy_digest: "policy".into(),
            provenance_digest: "provenance".into(),
            approval_id: Uuid::new_v4(),
            approval_input_digest: "input".into(),
            approval_target_digest: "target".into(),
            approval_policy_digest: "policy".into(),
            approval_nonce_digest: "n".repeat(32),
            approval_expires_at: now,
        };
        assert!(authorize_high_value_action(&r, &[HighValueActionKind::OpenPr]).is_ok());
        r.target_digest = "other".into();
        assert_eq!(
            authorize_high_value_action(&r, &[HighValueActionKind::OpenPr]),
            Err(FixedActionError::Approval)
        );
    }
}
