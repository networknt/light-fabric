use agent_runtime_protocol::canonical_digest;
use execution_security::ProtectedPathPolicy;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::BTreeSet;
use thiserror::Error;

pub const PI_RPC_ADAPTER_ID: &str = "pi-rpc";
pub const PI_RPC_ADAPTER_VERSION: &str = "1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingAdapter {
    PiRpc,
    CodexJsonl,
    ClaudeStreamJson,
    GeminiJson,
    KiloJson,
}

impl CodingAdapter {
    pub fn id(self) -> &'static str {
        match self {
            Self::PiRpc => "pi-rpc",
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
            Self::PiRpc => true,
            Self::CodexJsonl => args.iter().any(|a| a == "--json"),
            Self::ClaudeStreamJson => args
                .windows(2)
                .any(|a| a == ["--output-format", "stream-json"]),
            Self::GeminiJson | Self::KiloJson => args.iter().any(|a| a == "--output-format=json"),
        };
        structured.then_some(()).ok_or(CodingError::Unstructured)
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
pub struct CodingTurnSpec {
    pub repository_digest: String,
    pub base_revision: String,
    pub workspace_root: String,
    pub prompt: String,
    pub model_alias: String,
    pub materialization_manifest_digest: String,
    pub writable_roots: BTreeSet<String>,
    pub allowed_tools: BTreeSet<String>,
    pub maximum_patch_bytes: u64,
    pub maximum_changed_files: usize,
}

impl CodingTurnSpec {
    pub fn validate(&self) -> Result<(), CodingError> {
        if !self.workspace_root.starts_with("/workspace/")
            || self.workspace_root.contains("..")
            || self.prompt.is_empty()
            || self.prompt.len() > 64 * 1024
        {
            return Err(CodingError::Spec);
        }
        if self.base_revision.len() != 40
            || !self.base_revision.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(CodingError::Spec);
        }
        if self.maximum_patch_bytes == 0
            || self.maximum_patch_bytes > 16 * 1024 * 1024
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PiRpcCommand {
    Start {
        request_id: String,
        spec: CodingTurnSpec,
    },
    Cancel {
        request_id: String,
    },
    Checkpoint {
        request_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PiRpcEvent {
    Progress {
        request_id: String,
        phase: CodingPhase,
        message: String,
    },
    Patch {
        request_id: String,
        base_revision: String,
        patch: String,
        changed_paths: Vec<String>,
    },
    Terminal {
        request_id: String,
        succeeded: bool,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CodingPhase {
    Inspect,
    Edit,
    Build,
    Test,
    Export,
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
        patch_digest: format!("sha256:{:x}", sha2::Sha256::digest(patch.as_bytes())),
        changed_paths: paths,
    })
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
            model_alias: "approved".into(),
            materialization_manifest_digest: format!("sha256:{:064x}", 2),
            writable_roots: BTreeSet::from(["/workspace/repo".into()]),
            allowed_tools: BTreeSet::from(["fs.read".into(), "fs.write".into()]),
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
    }
    #[test]
    fn adapters_require_machine_protocols_and_forbid_bypass() {
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
}
